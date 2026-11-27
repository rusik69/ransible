//! Runner: tasks.
#![allow(unused_imports)]
use super::*;

impl Runner {
    // ---------- tag selection ----------

    pub(super) fn selected(&self, t: &Task) -> bool {
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

    pub(super) fn exec_item(
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

    pub(super) fn run_module_once(
        &self,
        h: &mut Host,
        t: &Task,
        env: &TaskEnv,
        vars: &mut Vars,
    ) -> ItemOut {
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

    pub(super) fn exec_task(&self, h: &mut Host, t: &Task, env: &TaskEnv) -> TaskRun {
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
            let envr: &TaskEnv = if let Some(item_v) = it {
                local_overlay = env.overlay.clone();
                if let Some(iv) = &index_var {
                    local_overlay.insert(iv.clone(), idx.into());
                }
                let n = list.len();
                let mut al = json!({
                    "index": idx + 1, "index0": idx, "first": idx == 0, "last": idx + 1 == n,
                    "length": n, "revindex": n - idx, "revindex0": n - idx - 1,
                });
                if loop_ctl
                    .get("extended")
                    .and_then(loose_bool)
                    .unwrap_or(false)
                {
                    al["allitems"] = Value::Array(list.iter().flatten().cloned().collect());
                    if idx > 0 {
                        al["previtem"] = list[idx - 1].clone().unwrap_or(Value::Null);
                    }
                    if idx + 1 < n {
                        al["nextitem"] = list[idx + 1].clone().unwrap_or(Value::Null);
                    }
                }
                let _ = item_v;
                local_overlay.insert("ansible_loop".into(), al);
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
                    t.name.clone().unwrap_or_default(),
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
                    t.name.clone().unwrap_or_default(),
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
