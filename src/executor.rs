//! Play / task execution engine (linear strategy).
use crate::conn::Conn;
use crate::events::{Event, HostStat, Sink};
use crate::inventory::Inventory;
use crate::modules::{self, Ctx};
use crate::playbook::{
    Play, Role, Task, apply_inherit, find_role, load_role, loose_bool, read_yaml,
};
use crate::template::{Templar, Vars};
use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

#[derive(Default, Clone)]
pub struct Opts {
    pub check: bool,
    pub diff: bool,
    #[allow(dead_code)]
    pub verbose: u8,
    pub tags: Vec<String>,
    pub skip_tags: Vec<String>,
    pub extra_vars: Map<String, Value>,
    pub limit: Option<String>,
    pub forks: usize,
    pub list_tasks: bool,
    pub list_hosts: bool,
    pub list_tags: bool,
    pub start_at_task: Option<String>,
}

#[derive(Default, Clone)]
pub struct Stats {
    pub ok: u32,
    pub changed: u32,
    pub unreachable: u32,
    pub failed: u32,
    pub skipped: u32,
    pub rescued: u32,
    pub ignored: u32,
}

#[derive(Clone)]
pub struct Host {
    pub name: String,
    base: Vars,   // role defaults < inventory vars
    facts: Vars,  // gathered facts (setup, package_facts, ...)
    layer: Vars,  // play vars < vars_files < role vars
    params: Vars, // role params / include_role vars (above set_fact)
    dynv: Vars,   // set_fact / register / include_vars
    failed: bool,
    ended: bool,
    notified: BTreeSet<String>,
    last_fail: Option<(String, Value)>,
    pub stats: Stats,
}

pub struct Runner {
    pub inv: std::sync::RwLock<Inventory>,
    pub opts: Opts,
    pub templar: Templar,
    pub sink: std::sync::Arc<dyn Sink>,
    pub need_hostvars: bool,
    pub playbook_dir: PathBuf,
    pub all_stats: std::sync::Mutex<Vec<(String, Stats)>>,
    pub started: std::sync::atomic::AtomicBool,
}

struct PlayState<'p> {
    play: &'p Play,
    hosts: Vec<Host>,
    handlers: Vec<Task>,
    groups: Value,
    play_hosts: Value,
}

struct TaskEnv<'a> {
    play: &'a Play,
    hostvars: Option<&'a Value>,
    groups: &'a Value,
    play_hosts: &'a Value,
    overlay: &'a Map<String, Value>,
}

#[derive(Default)]
struct TaskRun {
    events: Vec<Event>,
    touched: Vec<String>,
}

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    Ok,
    Changed,
    Failed,
    Skipped,
    Unreachable,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Ok => "ok",
            Kind::Changed => "changed",
            Kind::Failed => "failed",
            Kind::Skipped => "skipped",
            Kind::Unreachable => "unreachable",
        }
    }
}

struct ItemOut {
    res: Map<String, Value>,
    kind: Kind,
}

fn tpl(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains("{{") || s.contains("{%"),
        Value::Array(a) => a.iter().any(tpl),
        Value::Object(o) => o.values().any(tpl),
        _ => false,
    }
}

fn to_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| x.to_string())
            })
            .collect(),
        Some(Value::String(s)) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => vec![],
    }
}

fn vstr(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        o => o.to_string(),
    }
}

impl Runner {
    fn emit(&self, e: Event) {
        self.sink.emit(&e);
    }

    fn banner(&self, kind: &str, name: &str) {
        self.emit(Event::Banner {
            kind: kind.to_string(),
            name: name.to_string(),
            role: None,
        });
    }

    fn msg(&self, text: impl Into<String>) {
        self.emit(Event::Message { text: text.into() });
    }

    // ---------- variable assembly ----------

    /// Resolve templated variable values, dependencies first (approximates Ansible's lazy evaluation).
    fn resolve_vars(&self, v: &mut Vars) {
        const SKIP: &[&str] = &["hostvars", "groups", "ansible_facts", "vars"];
        let mut pending: Vec<String> = v
            .iter()
            .filter(|(k, val)| !SKIP.contains(&k.as_str()) && tpl(val))
            .map(|(k, _)| k.clone())
            .collect();
        if pending.is_empty() {
            return;
        }
        let ident = |text: &str, name: &str| -> bool {
            let b = text.as_bytes();
            let mut from = 0;
            while let Some(i) = text[from..].find(name) {
                let s = from + i;
                let e = s + name.len();
                let before_ok = s == 0
                    || !(b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'_' || b[s - 1] == b'.');
                let after_ok = e >= b.len() || !(b[e].is_ascii_alphanumeric() || b[e] == b'_');
                if before_ok && after_ok {
                    return true;
                }
                from = e;
            }
            false
        };
        let mut guard = 0;
        while !pending.is_empty() && guard < 64 {
            guard += 1;
            let texts: Vec<(String, String)> = pending
                .iter()
                .map(|k| (k.clone(), serde_json::to_string(&v[k]).unwrap_or_default()))
                .collect();
            let ready: Vec<String> = texts
                .iter()
                .filter(|(k, t)| !texts.iter().any(|(o, _)| o != k && ident(t, o)))
                .map(|(k, _)| k.clone())
                .collect();
            // cycle: render everything that is left once
            let batch = if ready.is_empty() {
                pending.clone()
            } else {
                ready
            };
            let snap = v.clone();
            for k in &batch {
                if let Ok(n) = self.templar.render_value(&snap[k], &snap) {
                    v.insert(k.clone(), n);
                }
            }
            pending.retain(|k| !batch.contains(k));
        }
    }

    fn host_ctx(&self, h: &Host, env: &TaskEnv) -> Vars {
        self.host_ctx_t(h, env, None)
    }

    fn host_ctx_t(&self, h: &Host, env: &TaskEnv, tvars: Option<&Vars>) -> Vars {
        let mut v = h.base.clone();
        v.extend(h.facts.clone());
        v.extend(h.layer.clone());
        if let Some(tv) = tvars {
            v.extend(tv.clone());
        }
        v.extend(h.dynv.clone());
        v.extend(h.params.clone());
        v.insert("inventory_hostname".into(), h.name.clone().into());
        v.insert(
            "inventory_hostname_short".into(),
            h.name.split('.').next().unwrap_or("").into(),
        );
        let invr = self.inv.read().unwrap();
        let gn: Vec<Value> = invr
            .groups_of(&h.name)
            .into_iter()
            .filter(|g| {
                g != "ungrouped"
                    || self.inv.read().unwrap().groups["ungrouped"]
                        .hosts
                        .contains(&h.name)
            })
            .map(Value::from)
            .collect();
        v.insert("group_names".into(), Value::Array(gn));
        v.insert("groups".into(), env.groups.clone());
        v.insert("ansible_play_hosts".into(), env.play_hosts.clone());
        v.insert("ansible_play_hosts_all".into(), env.play_hosts.clone());
        v.insert("ansible_play_batch".into(), env.play_hosts.clone());
        v.insert("play_hosts".into(), env.play_hosts.clone());
        v.insert("omit".into(), crate::modules::OMIT.into());
        v.insert(
            "playbook_dir".into(),
            self.playbook_dir.display().to_string().into(),
        );
        v.insert(
            "inventory_dir".into(),
            self.inv
                .read()
                .unwrap()
                .base_dir
                .display()
                .to_string()
                .into(),
        );
        v.insert("ansible_check_mode".into(), self.opts.check.into());
        v.insert(
            "ansible_version".into(),
            json!({"full": "2.16.0", "major": 2, "minor": 16, "string": "2.16.0"}),
        );
        if let Some(hv) = env.hostvars {
            v.insert("hostvars".into(), hv.clone());
        }
        v
    }

    fn task_vars(&self, h: &Host, t: &Task, env: &TaskEnv, item: Option<(&str, &Value)>) -> Vars {
        let tv = match t.kw.get("vars") {
            Some(Value::Object(tv)) => Some(tv),
            _ => None,
        };
        let mut v = self.host_ctx_t(h, env, tv);
        v.extend(env.overlay.clone());
        if let Some((k, val)) = item {
            v.insert(k.into(), val.clone());
        }
        v.extend(self.opts.extra_vars.clone());
        v.insert("role_path".into(), t.dir.display().to_string().into());
        self.resolve_vars(&mut v);
        v
    }

    fn hostvars_snapshot(&self, hosts: &[Host], env_groups: &Value, play_hosts: &Value) -> Value {
        let mut out = Map::new();
        let overlay = Map::new();
        let dummy_play = Play::default();
        for h in hosts {
            let env = TaskEnv {
                play: &dummy_play,
                hostvars: None,
                groups: env_groups,
                play_hosts,
                overlay: &overlay,
            };
            let mut v = self.host_ctx(h, &env);
            v.remove("groups");
            self.resolve_vars(&mut v);
            out.insert(h.name.clone(), Value::Object(v));
        }
        Value::Object(out)
    }

    fn make_conn(&self, vars: &Vars, host: &str) -> Conn {
        let s = |ks: &[&str]| {
            ks.iter()
                .find_map(|k| vars.get(*k).filter(|v| !v.is_null()).map(vstr))
        };
        let connection = s(&["ansible_connection"]).unwrap_or_default();
        let ah = s(&["ansible_host", "ansible_ssh_host"]);
        let local = connection == "local"
            || (connection.is_empty()
                && ah.is_none()
                && (host == "localhost" || host == "127.0.0.1"));
        let kind = if local {
            crate::conn::Kind::Local
        } else if matches!(
            connection.as_str(),
            "docker"
                | "podman"
                | "nerdctl"
                | "community.docker.docker"
                | "containers.podman.podman"
        ) {
            crate::conn::Kind::Container(
                connection
                    .rsplit('.')
                    .next()
                    .unwrap_or("docker")
                    .to_string(),
            )
        } else {
            crate::conn::Kind::Ssh
        };
        let mut args = vec![];
        for k in ["ansible_ssh_common_args", "ansible_ssh_extra_args"] {
            if let Some(a) = s(&[k]) {
                args.push(a);
            }
        }
        Conn {
            kind,
            local,
            password: s(&[
                "ansible_password",
                "ansible_ssh_pass",
                "ansible_ssh_password",
            ]),
            become_password: s(&[
                "ansible_become_password",
                "ansible_become_pass",
                "ansible_sudo_pass",
            ]),
            mock: None,
            host: ah.unwrap_or_else(|| host.to_string()),
            port: s(&["ansible_port", "ansible_ssh_port"]).and_then(|p| p.parse().ok()),
            user: s(&["ansible_user", "ansible_ssh_user", "ansible_remote_user"]),
            key: s(&["ansible_ssh_private_key_file", "ansible_private_key_file"]),
            ssh_args: if args.is_empty() {
                None
            } else {
                Some(args.join(" "))
            },
            become_user: None,
            become_method: "sudo".into(),
            env: vec![],
        }
    }

    // ---------- tag selection ----------

    fn selected(&self, t: &Task) -> bool {
        let tags = to_list(t.kw.get("tags"));
        if tags.iter().any(|x| self.opts.skip_tags.contains(x)) {
            return false;
        }
        let hit = tags.iter().any(|x| self.opts.tags.contains(x));
        if tags.iter().any(|x| x == "never") && !hit {
            return false;
        }
        if self.opts.tags.is_empty() {
            return true;
        }
        hit || tags.iter().any(|x| x == "always")
    }

    // ---------- single task on a host ----------

    fn exec_item(
        &self,
        h: &mut Host,
        t: &Task,
        env: &TaskEnv,
        item: Option<(&str, &Value)>,
    ) -> ItemOut {
        let mut vars = self.task_vars(h, t, env, item);
        let fail = |msg: String| {
            let mut r = Map::new();
            r.insert("failed".into(), true.into());
            r.insert("changed".into(), false.into());
            r.insert("msg".into(), msg.into());
            ItemOut {
                res: r,
                kind: Kind::Failed,
            }
        };
        // when
        if let Some(w) = t.kw.get("when") {
            let conds: Vec<Value> = match w {
                Value::Array(a) => a.clone(),
                o => vec![o.clone()],
            };
            for c in conds {
                let ok = match &c {
                    Value::Bool(b) => Ok(*b),
                    Value::String(s) => self.templar.eval_bool(s, &vars),
                    Value::Null => Ok(true),
                    o => Ok(loose_bool(o).unwrap_or(true)),
                };
                match ok {
                    Ok(true) => {}
                    Ok(false) => {
                        let mut r = Map::new();
                        r.insert("skipped".into(), true.into());
                        r.insert("changed".into(), false.into());
                        r.insert("skip_reason".into(), "Conditional result was False".into());
                        r.insert("false_condition".into(), c);
                        return ItemOut {
                            res: r,
                            kind: Kind::Skipped,
                        };
                    }
                    Err(e) => return fail(format!("{e:#}")),
                }
            }
        }
        let max_tries = if t.kw.contains_key("until") {
            t.kw.get("retries").and_then(|v| v.as_u64()).unwrap_or(3) + 1
        } else {
            1
        };
        let delay = t.kw.get("delay").and_then(|v| v.as_u64()).unwrap_or(5);
        let mut tries = 0;
        loop {
            tries += 1;
            let mut out = self.run_module_once(h, t, env, &mut vars);
            if let Some(until) = t.kw.get("until") {
                let reg = t.kw.get("register").and_then(|r| r.as_str()).unwrap_or("_");
                let mut v2 = vars.clone();
                v2.insert(reg.into(), Value::Object(out.res.clone()));
                let done = match until {
                    Value::String(s) => self.templar.eval_bool(s, &v2).unwrap_or(false),
                    o => loose_bool(o).unwrap_or(true),
                };
                out.res.insert("attempts".into(), tries.into());
                if !done && tries < max_tries {
                    std::thread::sleep(std::time::Duration::from_secs(delay));
                    continue;
                }
                if !done && out.kind != Kind::Unreachable {
                    out.kind = Kind::Failed;
                    out.res.insert("failed".into(), true.into());
                    out.res.entry("msg").or_insert("Retries exhausted".into());
                }
            }
            return out;
        }
    }

    fn run_module_once(&self, h: &mut Host, t: &Task, env: &TaskEnv, vars: &mut Vars) -> ItemOut {
        let fail = |msg: String, unreachable: bool| {
            let mut r = Map::new();
            r.insert("failed".into(), true.into());
            r.insert("changed".into(), false.into());
            if unreachable {
                r.insert("unreachable".into(), true.into());
            }
            r.insert("msg".into(), msg.into());
            ItemOut {
                res: r,
                kind: if unreachable {
                    Kind::Unreachable
                } else {
                    Kind::Failed
                },
            }
        };
        let args = match self.templar.render_value(&t.args, vars) {
            Ok(v) if v.is_object() => match crate::modules::strip_omit(v) {
                Value::Object(m) => m,
                _ => unreachable!(),
            },
            Ok(_) => return fail("module arguments must be a mapping".into(), false),
            Err(e) => return fail(format!("{e:#}"), false),
        };
        // connection
        let delegate =
            t.kw.get("delegate_to")
                .and_then(|d| self.templar.render_string(&vstr(d), vars).ok());
        let (mut conn, cvars) = match &delegate {
            Some(d) if *d != h.name => {
                let dv = self.inv.read().unwrap().vars_for(d);
                (self.make_conn(&dv, d), dv)
            }
            _ => (self.make_conn(vars, &h.name), vars.clone()),
        };
        let truthy = |k: &str, vs: &Vars| -> Option<bool> {
            t.kw.get(k).map(|v| match v {
                Value::String(s) if s.contains("{{") => self
                    .templar
                    .render_str(s, vs)
                    .ok()
                    .and_then(|x| loose_bool(&x))
                    .unwrap_or(false),
                o => loose_bool(o).unwrap_or(false),
            })
        };
        let want_become = truthy("become", vars)
            .or_else(|| cvars.get("ansible_become").and_then(loose_bool))
            .unwrap_or(false);
        if want_become {
            let bu =
                t.kw.get("become_user")
                    .map(vstr)
                    .or_else(|| cvars.get("ansible_become_user").map(vstr))
                    .unwrap_or_else(|| "root".into());
            conn.become_user = Some(self.templar.render_string(&bu, vars).unwrap_or(bu));
            if let Some(m) =
                t.kw.get("become_method")
                    .map(vstr)
                    .or_else(|| cvars.get("ansible_become_method").map(vstr))
            {
                conn.become_method = m;
            }
        }
        if let Some(Value::Object(e)) = t.kw.get("environment") {
            for (k, v) in e {
                if let Ok(r) = self.templar.render_string(&vstr(v), vars) {
                    conn.env.push((k.clone(), r));
                }
            }
        }
        let async_secs = t.kw.get("async").and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        });
        let poll =
            t.kw.get("poll")
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .unwrap_or(15);
        let check = if t.kw.get("check_mode").and_then(loose_bool) == Some(false) {
            false
        } else {
            self.opts.check
        };
        let diff =
            t.kw.get("diff")
                .and_then(loose_bool)
                .unwrap_or(self.opts.diff);
        let hn = h.name.clone();
        let play_dir: PathBuf = env.play.dir.clone();
        let mctx = Ctx {
            host: &hn,
            conn: &conn,
            vars,
            templar: &self.templar,
            check,
            diff,
            dir: &t.dir,
            play_dir: &play_dir,
        };
        let result = match (async_secs, poll, t.module.as_str()) {
            (Some(secs), 0, "command" | "shell") => {
                modules::async_start(&t.module, &args, &mctx, secs)
            }
            _ => modules::run(&t.module, &args, &mctx),
        };
        let mut res = match result {
            Ok(r) => r,
            Err(e) => return fail(format!("{e:#}"), false),
        };
        // unreachable detection
        if res.get("rc").and_then(|r| r.as_i64()) == Some(255) && !conn.local {
            let se = res.get("stderr").and_then(|s| s.as_str()).unwrap_or("");
            if [
                "Could not resolve",
                "Connection refused",
                "Permission denied",
                "timed out",
                "No route to host",
                "Host key verification",
                "Connection closed",
            ]
            .iter()
            .any(|m| se.contains(m))
            {
                return fail(
                    format!("Failed to connect to the host via ssh: {}", se.trim()),
                    true,
                );
            }
        }
        if t.module == "add_host"
            && let Some(Value::Object(ah)) = res.get("add_host")
        {
            let name = ah
                .get("host_name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let groups: Vec<String> = ah
                .get("groups")
                .and_then(|g| g.as_array())
                .map(|g| {
                    g.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let vars = ah
                .get("host_vars")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            self.inv.write().unwrap().add_dynamic(&name, &groups, vars);
        }
        if t.module == "group_by"
            && let Some(g) = res.get("group_by").and_then(|g| g.as_str())
        {
            self.inv.write().unwrap().add_to_group(&h.name, g);
        }
        res.entry("changed").or_insert(false.into());
        let mut failed = res.get("failed").and_then(|f| f.as_bool()).unwrap_or(false);
        let skipped = res
            .get("skipped")
            .and_then(|f| f.as_bool())
            .unwrap_or(false);
        let mut changed = res
            .get("changed")
            .and_then(|f| f.as_bool())
            .unwrap_or(false);
        // changed_when / failed_when
        if t.kw.contains_key("changed_when") || t.kw.contains_key("failed_when") {
            let reg =
                t.kw.get("register")
                    .and_then(|r| r.as_str())
                    .unwrap_or("result")
                    .to_string();
            let mut v2 = vars.clone();
            v2.insert(reg, Value::Object(res.clone()));
            let ev = |k: &str, v2: &Vars| -> Option<Result<bool>> {
                t.kw.get(k).map(|c| {
                    let conds = match c {
                        Value::Array(a) => a.clone(),
                        o => vec![o.clone()],
                    };
                    let mut all = true;
                    for c in conds {
                        let b = match &c {
                            Value::String(s) => self.templar.eval_bool(s, v2)?,
                            o => loose_bool(o).unwrap_or(false),
                        };
                        all &= b;
                    }
                    Ok(all)
                })
            };
            match ev("changed_when", &v2) {
                Some(Ok(b)) => changed = b,
                Some(Err(e)) => return fail(format!("{e:#}"), false),
                None => {}
            }
            match ev("failed_when", &v2) {
                Some(Ok(b)) => {
                    failed = b;
                    if b && !res.contains_key("msg") {
                        res.insert("msg".into(), "failed_when condition was True".into());
                    }
                }
                Some(Err(e)) => return fail(format!("{e:#}"), false),
                None => {}
            }
        }
        res.insert("changed".into(), changed.into());
        if failed {
            res.insert("failed".into(), true.into());
        } else {
            res.remove("failed");
        }
        // facts
        if let Some(Value::Object(f)) = res.get("ansible_facts").cloned() {
            if t.module == "setup" {
                let stripped: Map<String, Value> = f
                    .iter()
                    .map(|(k, v)| (k.trim_start_matches("ansible_").to_string(), v.clone()))
                    .collect();
                h.facts
                    .insert("ansible_facts".into(), Value::Object(stripped));
            } else if !matches!(t.module.as_str(), "set_fact" | "include_vars") {
                let mut cur = match h.facts.get("ansible_facts") {
                    Some(Value::Object(o)) => o.clone(),
                    _ => Map::new(),
                };
                for (k, v) in &f {
                    cur.insert(k.trim_start_matches("ansible_").to_string(), v.clone());
                }
                h.facts.insert("ansible_facts".into(), Value::Object(cur));
            }
            let is_fact_mod = !matches!(t.module.as_str(), "set_fact" | "include_vars");
            for (k, v) in f {
                if is_fact_mod {
                    h.facts.insert(k, v);
                } else {
                    h.dynv.insert(k, v);
                }
            }
        }
        let kind = if failed {
            Kind::Failed
        } else if skipped {
            Kind::Skipped
        } else if changed {
            Kind::Changed
        } else {
            Kind::Ok
        };
        ItemOut { res, kind }
    }

    fn exec_task(&self, h: &mut Host, t: &Task, env: &TaskEnv) -> TaskRun {
        let mut run = TaskRun::default();
        let base_vars = self.task_vars(h, t, env, None);
        let hn = h.name.clone();
        let no_log = t.kw.get("no_log").and_then(loose_bool).unwrap_or(false);
        let loop_ctl =
            t.kw.get("loop_control")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
        let loop_var = loop_ctl
            .get("loop_var")
            .and_then(|v| v.as_str())
            .unwrap_or("item")
            .to_string();
        let index_var = loop_ctl
            .get("index_var")
            .and_then(|v| v.as_str())
            .map(String::from);
        let items: Option<Vec<Value>> = match t.kw.get("loop") {
            None => None,
            Some(l) => match self.templar.render_value(l, &base_vars).and_then(|v| {
                match t.kw.get("_with").and_then(|w| w.as_str()) {
                    Some(w) => with_transform(w, v, t, env.play).map(Value::Array),
                    None => Ok(v),
                }
            }) {
                Ok(Value::Array(a)) => {
                    if t.kw.contains_key("_flatten_loop") {
                        Some(
                            a.into_iter()
                                .flat_map(|x| match x {
                                    Value::Array(i) => i,
                                    o => vec![o],
                                })
                                .collect(),
                        )
                    } else {
                        Some(a)
                    }
                }
                Ok(other) => {
                    run.events.push(Event::HostResult { host: hn.clone(), status: "failed".into(), module: t.module.clone(), item: None, ignored: false, result: json!({"msg": format!("Invalid data passed to 'loop', it requires a list, got: {}", other)}) });
                    h.failed = true;
                    h.stats.failed += 1;
                    return run;
                }
                Err(e) => {
                    run.events.push(Event::HostResult {
                        host: hn.clone(),
                        status: "failed".into(),
                        module: t.module.clone(),
                        item: None,
                        ignored: false,
                        result: json!({"msg": format!("{e:#}")}),
                    });
                    h.failed = true;
                    h.stats.failed += 1;
                    return run;
                }
            },
        };
        let mut results: Vec<Value> = vec![];
        let (mut any_changed, mut any_failed, mut any_unreach, mut all_skipped) =
            (false, false, false, true);
        let mut any_chg_raw = false;
        let list: Vec<Option<Value>> = match &items {
            Some(v) => v.iter().cloned().map(Some).collect(),
            None => vec![None],
        };
        for (idx, it) in list.iter().enumerate() {
            let mut local_overlay;
            let env2;
            let envr: &TaskEnv = if let (Some(iv), Some(_)) = (&index_var, it) {
                local_overlay = env.overlay.clone();
                local_overlay.insert(iv.clone(), idx.into());
                env2 = TaskEnv {
                    play: env.play,
                    hostvars: env.hostvars,
                    groups: env.groups,
                    play_hosts: env.play_hosts,
                    overlay: &local_overlay,
                };
                &env2
            } else {
                env
            };
            let mut out = self.exec_item(h, t, envr, it.as_ref().map(|v| (loop_var.as_str(), v)));
            if let Some(v) = it {
                out.res.insert(loop_var.clone(), v.clone());
                out.res
                    .insert("ansible_loop_var".into(), loop_var.clone().into());
            }
            if no_log {
                out.res = Map::from_iter([
                    (
                        "censored".to_string(),
                        Value::from(
                            "the output has been hidden due to the fact that 'no_log: true' was specified for this result",
                        ),
                    ),
                    (
                        "changed".into(),
                        out.res.get("changed").cloned().unwrap_or(false.into()),
                    ),
                    (
                        "failed".into(),
                        out.res.get("failed").cloned().unwrap_or(false.into()),
                    ),
                ]);
            }
            let label = it.as_ref().map(|v| {
                let l = loop_ctl
                    .get("label")
                    .and_then(|l| l.as_str())
                    .and_then(|l| {
                        let mut vv = base_vars.clone();
                        vv.insert(loop_var.clone(), v.clone());
                        self.templar.render_string(l, &vv).ok()
                    });
                l.unwrap_or_else(|| vstr(v))
            });
            run.events.push(Event::HostResult {
                host: hn.clone(),
                status: out.kind.as_str().into(),
                module: t.module.clone(),
                item: label,
                ignored: t
                    .kw
                    .get("ignore_errors")
                    .and_then(loose_bool)
                    .unwrap_or(false),
                result: Value::Object(out.res.clone()),
            });
            any_changed |= out.kind == Kind::Changed;
            any_chg_raw |= out
                .res
                .get("changed")
                .and_then(|c| c.as_bool())
                .unwrap_or(false);
            any_failed |= out.kind == Kind::Failed;
            any_unreach |= out.kind == Kind::Unreachable;
            all_skipped &= out.kind == Kind::Skipped;
            let stop = matches!(out.kind, Kind::Failed | Kind::Unreachable);
            results.push(Value::Object(out.res));
            if stop
                && !t
                    .kw
                    .get("ignore_errors")
                    .and_then(loose_bool)
                    .unwrap_or(false)
            {
                break;
            }
        }
        let agg: Map<String, Value> = if items.is_some() {
            let mut m = Map::new();
            m.insert("results".into(), Value::Array(results.clone()));
            m.insert("changed".into(), any_changed.into());
            m.insert("failed".into(), any_failed.into());
            m.insert(
                "skipped".into(),
                (all_skipped && !results.is_empty()).into(),
            );
            if results.is_empty() {
                m.insert("skipped".into(), true.into());
                m.insert("skip_reason".into(), "No items in the list".into());
            }
            m
        } else {
            match results.pop() {
                Some(Value::Object(o)) => o,
                _ => Map::new(),
            }
        };
        if let Some(reg) = t.kw.get("register").and_then(|r| r.as_str()) {
            h.dynv.insert(reg.into(), Value::Object(agg.clone()));
            run.touched.push(reg.into());
        }
        if let Some(Value::Object(f)) = agg.get("ansible_facts") {
            run.touched.extend(f.keys().cloned());
            if t.module == "setup" {
                run.touched.push("ansible_facts".into());
            }
        }
        if let Some(Value::Array(rs)) = agg.get("results") {
            for r in rs {
                if let Some(Value::Object(f)) = r.get("ansible_facts") {
                    run.touched.extend(f.keys().cloned());
                }
            }
        }
        if any_changed {
            for n in to_list(t.kw.get("notify"))
                .into_iter()
                .chain(std::iter::empty())
            {
                let name = self.templar.render_string(&n, &base_vars).unwrap_or(n);
                h.notified.insert(name);
            }
            if let Some(Value::Array(a)) = t.kw.get("notify") {
                for n in a {
                    let s = vstr(n);
                    h.notified
                        .insert(self.templar.render_string(&s, &base_vars).unwrap_or(s));
                }
            }
        }
        let ignore =
            t.kw.get("ignore_errors")
                .and_then(loose_bool)
                .unwrap_or(false);
        let ignore_unreach =
            t.kw.get("ignore_unreachable")
                .and_then(loose_bool)
                .unwrap_or(false);
        if any_unreach {
            h.stats.unreachable += 1;
            if !ignore_unreach {
                h.failed = true;
                h.last_fail = Some((
                    t.name.clone().unwrap_or_else(|| t.module.clone()),
                    Value::Object(agg.clone()),
                ));
            }
        } else if any_failed {
            if ignore {
                h.stats.ignored += 1;
                h.stats.ok += 1;
                if any_chg_raw {
                    h.stats.changed += 1;
                }
            } else {
                h.failed = true;
                h.stats.failed += 1;
                h.last_fail = Some((
                    t.name.clone().unwrap_or_else(|| t.module.clone()),
                    Value::Object(agg.clone()),
                ));
            }
        } else if all_skipped && !results.is_empty()
            || (items.is_none() && agg.get("skipped").and_then(|s| s.as_bool()) == Some(true))
        {
            h.stats.skipped += 1;
        } else {
            h.stats.ok += 1;
            if any_changed {
                h.stats.changed += 1;
            }
        }
        run
    }

    // ---------- task list execution ----------

    fn active(&self, ps: &PlayState, idxs: &[usize]) -> Vec<usize> {
        idxs.iter()
            .copied()
            .filter(|&i| !ps.hosts[i].failed && !ps.hosts[i].ended)
            .collect()
    }

    fn run_tasks(
        &self,
        ps: &mut PlayState,
        idxs: &[usize],
        tasks: &[Task],
        overlay: &Map<String, Value>,
    ) -> Result<()> {
        for t in tasks {
            let act = self.active(ps, idxs);
            if act.is_empty() {
                break;
            }
            if t.is_block {
                self.run_block(ps, &act, t, overlay)?;
                continue;
            }
            match t.module.as_str() {
                "include_tasks" | "import_tasks" => self.run_include_tasks(ps, &act, t, overlay)?,
                "include_role" | "import_role" => self.run_include_role(ps, &act, t, overlay)?,
                "meta" => self.run_meta(ps, &act, t, overlay)?,
                _ => self.run_leaf(ps, &act, t, overlay, None)?,
            }
        }
        Ok(())
    }

    fn run_meta(
        &self,
        ps: &mut PlayState,
        act: &[usize],
        t: &Task,
        overlay: &Map<String, Value>,
    ) -> Result<()> {
        let what = t
            .args
            .get("_raw_params")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        match what.as_str() {
            "flush_handlers" => self.flush_handlers(ps, act, overlay)?,
            "end_play" | "end_batch" => {
                for &i in act {
                    ps.hosts[i].ended = true;
                }
            }
            "end_host" => {
                for &i in act {
                    ps.hosts[i].ended = true;
                }
            }
            "clear_facts" => {
                for &i in act {
                    ps.hosts[i].facts.clear();
                }
            }
            "noop" | "refresh_inventory" | "clear_host_errors" | "reset_connection" => {}
            other => return Err(anyhow!("unsupported meta action '{other}'")),
        }
        Ok(())
    }

    fn run_leaf(
        &self,
        ps: &mut PlayState,
        act: &[usize],
        t: &Task,
        overlay: &Map<String, Value>,
        title: Option<&str>,
    ) -> Result<()> {
        if let Some(n) = &self.opts.start_at_task {
            use std::sync::atomic::Ordering;
            if !self.started.load(Ordering::Relaxed) {
                if t.name.as_deref() == Some(n.as_str()) {
                    self.started.store(true, Ordering::Relaxed);
                } else {
                    return Ok(());
                }
            }
        }
        if !self.selected(t) {
            return Ok(());
        }
        let hv = if self.need_hostvars {
            Some(self.hostvars_snapshot(&ps.hosts, &ps.groups, &ps.play_hosts))
        } else {
            None
        };
        let name = match (title, &t.name) {
            (Some(n), _) => n.to_string(),
            (None, Some(n)) => {
                let first = act[0];
                let env = TaskEnv {
                    play: ps.play,
                    hostvars: hv.as_ref(),
                    groups: &ps.groups,
                    play_hosts: &ps.play_hosts,
                    overlay,
                };
                let v = self.host_ctx(&ps.hosts[first], &env);
                self.templar
                    .render_string(n, &v)
                    .unwrap_or_else(|_| n.clone())
            }
            (None, None) => t.module.clone(),
        };
        let name = match &t.role {
            Some(r) if title.is_none() => format!("{r} : {name}"),
            _ => name,
        };
        self.banner("TASK", &name);
        let run_once = t.kw.get("run_once").and_then(loose_bool).unwrap_or(false);
        let target: Vec<usize> = if run_once { vec![act[0]] } else { act.to_vec() };
        let env = TaskEnv {
            play: ps.play,
            hostvars: hv.as_ref(),
            groups: &ps.groups,
            play_hosts: &ps.play_hosts,
            overlay,
        };
        let forks = if t.kw.get("throttle").and_then(|v| v.as_u64()).is_some() {
            t.kw["throttle"].as_u64().unwrap() as usize
        } else {
            self.opts.forks.max(1)
        };
        let mut refs: Vec<(usize, &mut Host)> = ps
            .hosts
            .iter_mut()
            .enumerate()
            .filter(|(i, _)| target.contains(i))
            .collect();
        let mut results: Vec<(usize, TaskRun)> = vec![];
        for chunk in refs.chunks_mut(forks) {
            std::thread::scope(|s| {
                let handles: Vec<_> = chunk
                    .iter_mut()
                    .map(|(i, h)| {
                        let i = *i;
                        let h: &mut Host = h;
                        let env = &env;
                        s.spawn(move || (i, self.exec_task(h, t, env)))
                    })
                    .collect();
                for hd in handles {
                    results.push(hd.join().expect("task thread panicked"));
                }
            });
        }
        results.sort_by_key(|r| r.0);
        let mut touched_first: Vec<String> = vec![];
        for (n, (_, r)) in results.iter().enumerate() {
            for ev in &r.events {
                self.sink.emit(ev);
            }
            if n == 0 {
                touched_first = r.touched.clone();
            }
        }
        if run_once && act.len() > 1 {
            let mut src: Vars = ps.hosts[act[0]].facts.clone();
            src.extend(ps.hosts[act[0]].dynv.clone());
            for &i in &act[1..] {
                for k in &touched_first {
                    if let Some(v) = src.get(k) {
                        ps.hosts[i].dynv.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        let any_fatal = ps
            .play
            .kw
            .get("any_errors_fatal")
            .and_then(loose_bool)
            .unwrap_or(false);
        if any_fatal && act.iter().any(|&i| ps.hosts[i].failed) {
            for &i in act {
                ps.hosts[i].failed = true;
            }
        }
        if let Some(pct) = ps.play.kw.get("max_fail_percentage").and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        }) {
            let total = ps.hosts.len().max(1) as f64;
            let failed = ps.hosts.iter().filter(|h| h.failed).count() as f64;
            if failed / total * 100.0 > pct {
                for h in ps.hosts.iter_mut() {
                    h.failed = true;
                }
            }
        }
        Ok(())
    }

    fn run_block(
        &self,
        ps: &mut PlayState,
        act: &[usize],
        t: &Task,
        overlay: &Map<String, Value>,
    ) -> Result<()> {
        self.run_tasks(ps, act, &t.block, overlay)?;
        let failed: Vec<usize> = act
            .iter()
            .copied()
            .filter(|&i| ps.hosts[i].failed)
            .collect();
        let mut still_failed: Vec<usize> = failed.clone();
        if !t.rescue.is_empty() && !failed.is_empty() {
            for &i in &failed {
                ps.hosts[i].failed = false;
                if let Some((name, result)) = ps.hosts[i].last_fail.clone() {
                    ps.hosts[i]
                        .dynv
                        .insert("ansible_failed_task".into(), json!({"name": name}));
                    ps.hosts[i]
                        .dynv
                        .insert("ansible_failed_result".into(), result);
                }
            }
            self.run_tasks(ps, &failed, &t.rescue, overlay)?;
            still_failed = failed
                .iter()
                .copied()
                .filter(|&i| ps.hosts[i].failed)
                .collect();
            for &i in &failed {
                if !ps.hosts[i].failed {
                    ps.hosts[i].stats.rescued += 1;
                    ps.hosts[i].stats.failed = ps.hosts[i].stats.failed.saturating_sub(1);
                }
            }
        }
        if !t.always.is_empty() {
            for &i in &still_failed {
                ps.hosts[i].failed = false;
            }
            let all: Vec<usize> = act
                .iter()
                .copied()
                .filter(|&i| !ps.hosts[i].ended)
                .collect();
            self.run_tasks(ps, &all, &t.always, overlay)?;
            for &i in &still_failed {
                ps.hosts[i].failed = true;
            }
        }
        Ok(())
    }

    fn include_path(&self, t: &Task, play: &Play, name: &str) -> Result<PathBuf> {
        let p = Path::new(name);
        if p.is_absolute() && p.exists() {
            return Ok(p.to_path_buf());
        }
        let cands = [
            t.dir.join("tasks").join(name),
            t.dir.join(name),
            play.dir.join(name),
            play.dir.join("tasks").join(name),
        ];
        cands
            .into_iter()
            .find(|c| c.is_file())
            .ok_or_else(|| anyhow!("could not find included file '{name}'"))
    }

    fn include_when(
        &self,
        ps: &PlayState,
        act: &[usize],
        t: &Task,
        overlay: &Map<String, Value>,
    ) -> Vec<usize> {
        let env = TaskEnv {
            play: ps.play,
            hostvars: None,
            groups: &ps.groups,
            play_hosts: &ps.play_hosts,
            overlay,
        };
        act.iter()
            .copied()
            .filter(|&i| {
                let v = self.task_vars(&ps.hosts[i], t, &env, None);
                match t.kw.get("when") {
                    None => true,
                    Some(Value::Array(a)) => a.iter().all(|c| match c {
                        Value::String(s) => self.templar.eval_bool(s, &v).unwrap_or(false),
                        o => loose_bool(o).unwrap_or(true),
                    }),
                    Some(o) => loose_bool(o).unwrap_or(true),
                }
            })
            .collect()
    }

    fn run_include_tasks(
        &self,
        ps: &mut PlayState,
        act: &[usize],
        t: &Task,
        overlay: &Map<String, Value>,
    ) -> Result<()> {
        if !self.selected(t) && t.module == "include_tasks" && !to_list(t.kw.get("tags")).is_empty()
        {
            // tags on include apply to children; still descend
        }
        let sub = self.include_when(ps, act, t, overlay);
        if sub.is_empty() {
            return Ok(());
        }
        let (hv, first) = (None::<Value>, sub[0]);
        let env = TaskEnv {
            play: ps.play,
            hostvars: hv.as_ref(),
            groups: &ps.groups,
            play_hosts: &ps.play_hosts,
            overlay,
        };
        let vars = self.task_vars(&ps.hosts[first], t, &env, None);
        let raw = t
            .args
            .get("file")
            .or(t.args.get("_raw_params"))
            .map(vstr)
            .ok_or_else(|| anyhow!("include_tasks requires a file"))?;
        let file = self.templar.render_string(&raw, &vars)?;
        let path = self.include_path(t, ps.play, &file)?;
        let mut tasks = crate::playbook::parse_tasks(&read_yaml(&path)?, &t.dir)?;
        if let Some(r) = &t.role {
            crate::playbook::tag_role(&mut tasks, r);
        }
        let mut kw = t.kw.clone();
        kw.remove("when");
        kw.remove("loop");
        kw.remove("loop_control");
        kw.remove("name");
        kw.remove("register");
        let mut pvars = kw.get("vars").cloned();
        if let Some(Value::Object(extra)) = t
            .args
            .as_object()
            .map(|a| {
                a.iter()
                    .filter(|(k, _)| !["file", "_raw_params", "apply"].contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Map<_, _>>()
            })
            .map(Value::Object)
            && !extra.is_empty()
        {
            let mut m = pvars
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            m.extend(extra);
            pvars = Some(Value::Object(m));
        }
        if let Some(v) = pvars {
            kw.insert("vars".into(), v);
        }
        if let Some(Value::Object(ap)) = t.args.get("apply") {
            for (k, v) in ap {
                kw.insert(k.clone(), v.clone());
            }
        }
        apply_inherit(&kw, &mut tasks);
        match t.kw.get("loop") {
            None => self.run_tasks(ps, &sub, &tasks, overlay),
            Some(l) => {
                let items = match self.templar.render_value(l, &vars)? {
                    Value::Array(a) => a,
                    o => return Err(anyhow!("include loop requires a list, got {o}")),
                };
                let lv =
                    t.kw.get("loop_control")
                        .and_then(|c| c.get("loop_var"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("item")
                        .to_string();
                for it in items {
                    let mut ov = overlay.clone();
                    ov.insert(lv.clone(), it);
                    self.run_tasks(ps, &sub, &tasks, &ov)?;
                }
                Ok(())
            }
        }
    }

    fn load_role_tree(
        &self,
        name: &str,
        play_dir: &Path,
        tasks_from: Option<&str>,
        params: &Map<String, Value>,
        seen: &mut HashSet<String>,
        out: &mut Vec<(Role, Map<String, Value>)>,
    ) -> Result<()> {
        let role = load_role(name, play_dir, tasks_from)?;
        for d in &role.deps {
            if !seen.contains(&d.name) {
                self.load_role_tree(&d.name, play_dir, None, &d.vars, seen, out)?;
            }
        }
        if seen.insert(name.to_string()) {
            out.push((role, params.clone()));
        }
        Ok(())
    }

    fn run_include_role(
        &self,
        ps: &mut PlayState,
        act: &[usize],
        t: &Task,
        overlay: &Map<String, Value>,
    ) -> Result<()> {
        let sub = self.include_when(ps, act, t, overlay);
        if sub.is_empty() {
            return Ok(());
        }
        let name = t
            .args
            .get("name")
            .map(vstr)
            .ok_or_else(|| anyhow!("include_role requires name"))?;
        let name = {
            let env = TaskEnv {
                play: ps.play,
                hostvars: None,
                groups: &ps.groups,
                play_hosts: &ps.play_hosts,
                overlay,
            };
            let vars = self.task_vars(&ps.hosts[sub[0]], t, &env, None);
            self.templar.render_string(&name, &vars)?
        };
        let tf = t.args.get("tasks_from").map(vstr);
        let mut params = Map::new();
        if let Some(Value::Object(v)) = t.args.get("vars") {
            params.extend(v.clone());
        }
        let mut seen = HashSet::new();
        let mut roles = vec![];
        self.load_role_tree(
            &name,
            &ps.play.dir,
            tf.as_deref(),
            &params,
            &mut seen,
            &mut roles,
        )?;
        for (role, params) in roles {
            for &i in &sub {
                for (k, v) in &role.defaults {
                    ps.hosts[i].base.entry(k.clone()).or_insert(v.clone());
                }
                ps.hosts[i].layer.extend(role.vars.clone());
                ps.hosts[i].params.extend(params.clone());
            }
            let mut tasks = role.tasks.clone();
            let mut kw = t.kw.clone();
            for k in ["when", "name", "register", "loop"] {
                kw.remove(k);
            }
            apply_inherit(&kw, &mut tasks);
            let mut hs = role.handlers.clone();
            apply_inherit(&kw, &mut hs);
            ps.handlers.extend(hs);
            self.run_tasks(ps, &sub, &tasks, overlay)?;
        }
        Ok(())
    }

    fn flush_handlers(
        &self,
        ps: &mut PlayState,
        act: &[usize],
        overlay: &Map<String, Value>,
    ) -> Result<()> {
        for _ in 0..10 {
            let mut ran = false;
            let handlers = ps.handlers.clone();
            for hd in &handlers {
                let mut names = to_list(hd.kw.get("listen"));
                if let Some(n) = &hd.name {
                    names.push(n.clone());
                }
                let todo: Vec<usize> = act
                    .iter()
                    .copied()
                    .filter(|&i| {
                        !ps.hosts[i].failed
                            && !ps.hosts[i].ended
                            && names.iter().any(|n| ps.hosts[i].notified.contains(n))
                    })
                    .collect();
                if todo.is_empty() {
                    continue;
                }
                for &i in &todo {
                    for n in &names {
                        ps.hosts[i].notified.remove(n);
                    }
                }
                ran = true;
                let mut hd2 = hd.clone();
                hd2.kw.remove("tags");
                let title = hd
                    .name
                    .clone()
                    .unwrap_or_else(|| hd.module.clone())
                    .to_string();
                if hd2.is_block {
                    self.run_tasks(ps, &todo, std::slice::from_ref(&hd2), overlay)?;
                } else if matches!(hd2.module.as_str(), "include_tasks" | "import_tasks") {
                    self.run_include_tasks(ps, &todo, &hd2, overlay)?;
                } else {
                    let saved = (self.opts.tags.clone(), ());
                    let _ = saved;
                    self.banner("RUNNING HANDLER", &title);
                    self.run_leaf_nobanner(ps, &todo, &hd2, overlay)?;
                }
            }
            if !ran {
                break;
            }
        }
        Ok(())
    }

    fn run_leaf_nobanner(
        &self,
        ps: &mut PlayState,
        act: &[usize],
        t: &Task,
        overlay: &Map<String, Value>,
    ) -> Result<()> {
        // handlers ignore tag filtering; reuse run_leaf but suppress its banner by capturing stdout ordering
        let hv = if self.need_hostvars {
            Some(self.hostvars_snapshot(&ps.hosts, &ps.groups, &ps.play_hosts))
        } else {
            None
        };
        let env = TaskEnv {
            play: ps.play,
            hostvars: hv.as_ref(),
            groups: &ps.groups,
            play_hosts: &ps.play_hosts,
            overlay,
        };
        let forks = self.opts.forks.max(1);
        let mut refs: Vec<(usize, &mut Host)> = ps
            .hosts
            .iter_mut()
            .enumerate()
            .filter(|(i, _)| act.contains(i))
            .collect();
        let mut results: Vec<(usize, TaskRun)> = vec![];
        for chunk in refs.chunks_mut(forks) {
            std::thread::scope(|s| {
                let hs: Vec<_> = chunk
                    .iter_mut()
                    .map(|(i, h)| {
                        let i = *i;
                        let h: &mut Host = h;
                        let env = &env;
                        s.spawn(move || (i, self.exec_task(h, t, env)))
                    })
                    .collect();
                for hd in hs {
                    results.push(hd.join().expect("handler thread panicked"));
                }
            });
        }
        results.sort_by_key(|r| r.0);
        for (_, r) in &results {
            for ev in &r.events {
                self.sink.emit(ev);
            }
        }
        Ok(())
    }

    // ---------- plays ----------

    fn list_tasks(&self, tasks: &[Task], indent: usize) {
        for t in tasks {
            let tags = to_list(t.kw.get("tags"));
            let tg = if tags.is_empty() {
                String::new()
            } else {
                format!("\tTAGS: [{}]", tags.join(", "))
            };
            if t.is_block {
                self.msg(format!("{}block{}", " ".repeat(indent), tg));
                self.list_tasks(&t.block, indent + 2);
                self.list_tasks(&t.rescue, indent + 2);
                self.list_tasks(&t.always, indent + 2);
            } else if self.selected(t) {
                self.msg(format!(
                    "{}{}{}",
                    " ".repeat(indent),
                    t.name.clone().unwrap_or_else(|| t.module.clone()),
                    tg
                ));
            }
        }
    }

    /// One play pass over `batch` (gather facts, pre_tasks, roles, tasks, post_tasks, handlers).
    fn run_batch(
        &self,
        ps: &mut PlayState,
        batch: &[usize],
        role_tasks: &[Task],
        ov: &Map<String, Value>,
    ) -> Result<()> {
        let play = ps.play;
        if play.gather_facts {
            let t = Task {
                name: Some("Gathering Facts".into()),
                module: "setup".into(),
                args: json!({}),
                dir: play.dir.clone(),
                ..Default::default()
            };
            let act = self.active(ps, batch);
            if !act.is_empty() {
                self.run_leaf(ps, &act, &t, ov, Some("Gathering Facts"))?;
            }
        }
        self.run_tasks(ps, batch, &play.pre_tasks, ov)?;
        let act = self.active(ps, batch);
        self.flush_handlers(ps, &act, ov)?;
        self.run_tasks(ps, batch, role_tasks, ov)?;
        self.run_tasks(ps, batch, &play.tasks, ov)?;
        let act = self.active(ps, batch);
        self.flush_handlers(ps, &act, ov)?;
        self.run_tasks(ps, batch, &play.post_tasks, ov)?;
        let act = self.active(ps, batch);
        self.flush_handlers(ps, &act, ov)?;
        Ok(())
    }

    pub fn run_play(&self, play: &Play) -> Result<bool> {
        let mut names = self.inv.read().unwrap().select(&play.hosts);
        if let Some(l) = &self.opts.limit {
            let lim = self.inv.read().unwrap().select(l);
            names.retain(|h| lim.contains(h));
        }
        if names.is_empty() && play.hosts == "localhost" {
            // implicit localhost
            names.push("localhost".into());
        }
        // roles (tree, deps first)
        let mut seen = HashSet::new();
        type RoleEntry = (Role, Map<String, Value>, Map<String, Value>);
        let mut roles: Vec<RoleEntry> = vec![];
        for rr in &play.roles {
            let mut tmp = vec![];
            self.load_role_tree(&rr.name, &play.dir, None, &rr.vars, &mut seen, &mut tmp)?;
            for (r, p) in tmp {
                let mut kw = rr.kw.clone();
                let mut tags = to_list(kw.get("tags"));
                tags.push(r.name.clone());
                kw.insert(
                    "tags".into(),
                    Value::Array(tags.into_iter().map(Value::from).collect()),
                );
                roles.push((r, p, kw));
            }
        }
        let mut role_tasks: Vec<Task> = vec![];
        let mut handlers: Vec<Task> = play.handlers.clone();
        let mut defaults = Map::new();
        let mut rvars = Map::new();
        let mut rparams = Map::new();
        for (r, params, kw) in &roles {
            defaults.extend(r.defaults.clone());
            rvars.extend(r.vars.clone());
            rparams.extend(params.clone());
            let mut ts = r.tasks.clone();
            let mut pk = play.kw.clone();
            pk.extend(kw.clone());
            apply_inherit(&pk, &mut ts);
            role_tasks.extend(ts);
            let mut hs = r.handlers.clone();
            apply_inherit(&play.kw, &mut hs);
            handlers.extend(hs);
        }
        if self.opts.list_hosts {
            self.msg(format!(
                "  play #{} ({}): {}\tTAGS: []\n    pattern: [{:?}]\n    hosts ({}):",
                1,
                play.name,
                play.name,
                play.hosts,
                names.len()
            ));
            for n in &names {
                self.msg(format!("      {n}"));
            }
            return Ok(true);
        }
        if self.opts.list_tags {
            let mut tags = std::collections::BTreeSet::new();
            fn collect(ts: &[Task], out: &mut std::collections::BTreeSet<String>) {
                for t in ts {
                    out.extend(to_list(t.kw.get("tags")));
                    collect(&t.block, out);
                    collect(&t.rescue, out);
                    collect(&t.always, out);
                }
            }
            for l in [&play.pre_tasks, &role_tasks, &play.tasks, &play.post_tasks] {
                collect(l, &mut tags);
            }
            self.msg(format!(
                "\n  play: {}\n    TASK TAGS: [{}]",
                play.name,
                tags.into_iter().collect::<Vec<_>>().join(", ")
            ));
            return Ok(true);
        }
        if self.opts.list_tasks {
            self.msg(format!("\n  play: {}\n    tasks:", play.name));
            self.list_tasks(&play.pre_tasks, 6);
            self.list_tasks(&role_tasks, 6);
            self.list_tasks(&play.tasks, 6);
            self.list_tasks(&play.post_tasks, 6);
            return Ok(true);
        }
        self.banner("PLAY", &play.name);
        if names.is_empty() {
            self.msg("skipping: no hosts matched");
            return Ok(true);
        }
        let mut groups = Map::new();
        let inv = self.inv.read().unwrap();
        for g in inv.groups.keys() {
            groups.insert(
                g.clone(),
                Value::Array(inv.group_hosts(g).into_iter().map(Value::from).collect()),
            );
        }
        let groups = Value::Object(groups);
        let play_hosts = Value::Array(names.iter().cloned().map(Value::from).collect());
        let mut hosts = vec![];
        let overlay0 = Map::new();
        for n in &names {
            let mut base = defaults.clone();
            base.extend(inv.vars_for(n));
            let mut h = Host {
                name: n.clone(),
                base,
                facts: Map::new(),
                layer: Map::new(),
                params: Map::new(),
                dynv: Map::new(),
                failed: false,
                ended: false,
                notified: BTreeSet::new(),
                last_fail: None,
                stats: Stats::default(),
            };
            if n == "localhost" && !inv.hosts.contains(n) {
                h.base.insert("ansible_connection".into(), "local".into());
            }
            // vars_files
            let env = TaskEnv {
                play,
                hostvars: None,
                groups: &groups,
                play_hosts: &play_hosts,
                overlay: &overlay0,
            };
            let mut ctx = self.host_ctx(&h, &env);
            ctx.extend(play.vars.clone());
            for vf in &play.vars_files {
                let cands: Vec<String> = match vf {
                    Value::Array(a) => a.iter().map(vstr).collect(),
                    o => vec![vstr(o)],
                };
                for c in cands {
                    let c = self.templar.render_string(&c, &ctx).unwrap_or(c);
                    let p = play.dir.join(&c);
                    if p.is_file() {
                        let mut m = Map::new();
                        crate::inventory::load_vars_file(&p, &mut m);
                        h.layer.extend(m);
                        break;
                    }
                }
            }
            // play vars sit below vars_files; re-apply order: play vars < vars_files < role vars
            let vf = std::mem::take(&mut h.layer);
            h.layer.extend(play.vars.clone());
            h.layer.extend(vf);
            h.layer.extend(rvars.clone());
            h.params.extend(rparams.clone());
            hosts.push(h);
        }
        drop(inv);
        let mut ps = PlayState {
            play,
            hosts,
            handlers,
            groups,
            play_hosts,
        };
        // serial batches
        let total = ps.hosts.len();
        let mut batches: Vec<Vec<usize>> = vec![];
        let mut start = 0;
        let mut si = 0;
        while start < total {
            let sz = match play.serial.get(si.min(play.serial.len().saturating_sub(1))) {
                None => total,
                Some(Value::String(s)) if s.ends_with('%') => {
                    ((total as f64) * s.trim_end_matches('%').parse::<f64>().unwrap_or(100.0)
                        / 100.0)
                        .ceil()
                        .max(1.0) as usize
                }
                Some(v) => v
                    .as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    .unwrap_or(total as u64)
                    .max(1) as usize,
            };
            let end = (start + sz).min(total);
            batches.push((start..end).collect());
            start = end;
            si += 1;
        }
        let ov = Map::new();
        for batch in &batches {
            if play.strategy_free && batch.len() > 1 {
                // free strategy: every host runs the whole play independently, at its own pace
                let results: Vec<(usize, Host, Result<()>)> = std::thread::scope(|s| {
                    let handles: Vec<_> = batch
                        .iter()
                        .map(|&i| {
                            let host = ps.hosts[i].clone();
                            let (handlers, groups, play_hosts) = (
                                ps.handlers.clone(),
                                ps.groups.clone(),
                                ps.play_hosts.clone(),
                            );
                            let (role_tasks, ov) = (&role_tasks, &ov);
                            s.spawn(move || {
                                let mut mini = PlayState {
                                    play,
                                    hosts: vec![host],
                                    handlers,
                                    groups,
                                    play_hosts,
                                };
                                let r = self.run_batch(&mut mini, &[0], role_tasks, ov);
                                (i, mini.hosts.pop().unwrap(), r)
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| h.join().expect("host thread panicked"))
                        .collect()
                });
                for (i, host, r) in results {
                    ps.hosts[i] = host;
                    r?;
                }
            } else {
                self.run_batch(&mut ps, batch, &role_tasks, &ov)?;
            }
        }
        let mut ok = true;
        let mut st = self.all_stats.lock().unwrap();
        for h in &ps.hosts {
            if h.failed || h.stats.failed > 0 || h.stats.unreachable > 0 {
                ok = ok && h.stats.failed == 0 && h.stats.unreachable == 0 && !h.failed;
            }
            if let Some(e) = st.iter_mut().find(|(n, _)| *n == h.name) {
                let s = &mut e.1;
                s.ok += h.stats.ok;
                s.changed += h.stats.changed;
                s.unreachable += h.stats.unreachable;
                s.failed += h.stats.failed;
                s.skipped += h.stats.skipped;
                s.rescued += h.stats.rescued;
                s.ignored += h.stats.ignored;
            } else {
                st.push((h.name.clone(), h.stats.clone()));
            }
        }
        Ok(ok)
    }

    pub fn recap(&self) -> i32 {
        let st = self.all_stats.lock().unwrap();
        let mut code = 0;
        let mut stats = vec![];
        for (n, s) in st.iter() {
            stats.push(HostStat {
                host: n.clone(),
                ok: s.ok,
                changed: s.changed,
                unreachable: s.unreachable,
                failed: s.failed,
                skipped: s.skipped,
                rescued: s.rescued,
                ignored: s.ignored,
            });
            if s.unreachable > 0 {
                code = code.max(4);
            }
            if s.failed > 0 {
                code = 2;
            }
        }
        self.emit(Event::Recap { stats });
        code
    }
}

#[allow(dead_code)]
fn _unused(_: &Path) {
    let _ = find_role;
}

fn with_transform(kind: &str, v: Value, t: &Task, play: &Play) -> Result<Vec<Value>> {
    let list = |v: &Value| -> Vec<Value> {
        match v {
            Value::Array(a) => a.clone(),
            Value::Null => vec![],
            o => vec![o.clone()],
        }
    };
    Ok(match kind {
        "first_found" => {
            let (files, paths, skip) = match &v {
                Value::Object(o) => (
                    list(o.get("files").unwrap_or(&Value::Null)),
                    list(o.get("paths").unwrap_or(&Value::Null)),
                    o.get("skip").and_then(loose_bool).unwrap_or(false),
                ),
                Value::Array(a) if a.iter().any(|x| x.is_object()) => {
                    let (mut f, mut p, mut sk) = (vec![], vec![], false);
                    for x in a {
                        match x {
                            Value::Object(o) => {
                                f.extend(list(o.get("files").unwrap_or(&Value::Null)));
                                p.extend(list(o.get("paths").unwrap_or(&Value::Null)));
                                sk |= o.get("skip").and_then(loose_bool).unwrap_or(false);
                            }
                            other => f.push(other.clone()),
                        }
                    }
                    (f, p, sk)
                }
                other => (list(other), vec![], false),
            };
            let mut dirs: Vec<PathBuf> = vec![];
            for p in &paths {
                let p = PathBuf::from(vstr(p));
                dirs.push(if p.is_absolute() { p } else { t.dir.join(p) });
            }
            for sub in ["", "vars", "tasks", "files", "templates", "defaults"] {
                dirs.push(t.dir.join(sub));
                dirs.push(play.dir.join(sub));
            }
            for f in &files {
                let f = vstr(f);
                if Path::new(&f).is_absolute() && Path::new(&f).exists() {
                    return Ok(vec![Value::String(f)]);
                }
                for d in &dirs {
                    let c = d.join(&f);
                    if c.is_file() {
                        return Ok(vec![Value::String(c.display().to_string())]);
                    }
                }
            }
            if skip {
                vec![]
            } else {
                return Err(anyhow!("No file was found when using first_found"));
            }
        }
        "fileglob" => {
            let mut out = vec![];
            for pat in list(&v) {
                let pat = vstr(&pat);
                let full = if Path::new(&pat).is_absolute() {
                    PathBuf::from(&pat)
                } else {
                    t.dir.join("files").join(&pat)
                };
                let (dir, fp) = (
                    full.parent().unwrap_or(Path::new(".")).to_path_buf(),
                    full.file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_default(),
                );
                let re = regex::Regex::new(&format!(
                    "^{}$",
                    regex::escape(&fp).replace("\\*", ".*").replace("\\?", ".")
                ))?;
                let mut hits: Vec<String> = std::fs::read_dir(&dir)
                    .map(|rd| {
                        rd.flatten()
                            .filter(|e| {
                                e.path().is_file() && re.is_match(&e.file_name().to_string_lossy())
                            })
                            .map(|e| e.path().display().to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                hits.sort();
                out.extend(hits.into_iter().map(Value::from));
            }
            out
        }
        "nested" | "product" | "cartesian" => {
            let mut acc: Vec<Vec<Value>> = vec![vec![]];
            for l in list(&v) {
                let items = list(&l);
                acc = acc
                    .into_iter()
                    .flat_map(|p| {
                        items.iter().map(move |i| {
                            let mut q = p.clone();
                            q.push(i.clone());
                            q
                        })
                    })
                    .collect();
            }
            acc.into_iter().map(Value::Array).collect()
        }
        "together" | "zip" => {
            let ls: Vec<Vec<Value>> = list(&v).iter().map(list).collect();
            let n = ls.iter().map(|l| l.len()).min().unwrap_or(0);
            (0..n)
                .map(|i| Value::Array(ls.iter().map(|l| l[i].clone()).collect()))
                .collect()
        }
        "indexed_items" => list(&v)
            .into_iter()
            .enumerate()
            .map(|(i, x)| json!([i, x]))
            .collect(),
        "flattened" => {
            fn fl(v: &Value, out: &mut Vec<Value>) {
                match v {
                    Value::Array(a) => a.iter().for_each(|x| fl(x, out)),
                    o => out.push(o.clone()),
                }
            }
            let mut out = vec![];
            fl(&v, &mut out);
            out
        }
        "random_choice" => list(&v).into_iter().take(1).collect(),
        "subelements" => {
            let l = list(&v);
            let (items, key) = (
                l.first().cloned().unwrap_or(Value::Null),
                l.get(1).map(vstr).unwrap_or_default(),
            );
            let mut out = vec![];
            for it in list(&items) {
                for sub in list(it.get(&key).unwrap_or(&Value::Null)) {
                    out.push(json!([it, sub]));
                }
            }
            out
        }
        "sequence" => {
            let o = v.as_object().cloned().unwrap_or_default();
            let g = |k: &str| {
                o.get(k).and_then(|x| {
                    x.as_i64()
                        .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
                })
            };
            let (start, step) = (g("start").unwrap_or(1), g("stride").unwrap_or(1).max(1));
            let end = g("end")
                .or_else(|| g("count").map(|c| start + (c - 1) * step))
                .unwrap_or(start);
            let fmt = o.get("format").and_then(|f| f.as_str());
            let mut out = vec![];
            let mut i = start;
            while i <= end {
                out.push(Value::String(match fmt {
                    Some(f) => f
                        .replace("%02d", &format!("{i:02}"))
                        .replace("%d", &i.to_string()),
                    None => i.to_string(),
                }));
                i += step;
            }
            out
        }
        other => return Err(anyhow!("unsupported loop construct with_{other}")),
    })
}
