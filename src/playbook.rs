//! Playbook / role / task models parsed from existing Ansible YAML.
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub const KEYWORDS: &[&str] = &[
    "name",
    "when",
    "loop",
    "loop_control",
    "register",
    "tags",
    "become",
    "become_user",
    "become_method",
    "become_flags",
    "ignore_errors",
    "ignore_unreachable",
    "changed_when",
    "failed_when",
    "until",
    "retries",
    "delay",
    "delegate_to",
    "delegate_facts",
    "run_once",
    "environment",
    "vars",
    "notify",
    "listen",
    "no_log",
    "check_mode",
    "diff",
    "args",
    "async",
    "poll",
    "any_errors_fatal",
    "connection",
    "throttle",
    "collections",
    "module_defaults",
    "action",
    "local_action",
    "block",
    "rescue",
    "always",
    "remote_user",
    "port",
    "timeout",
    "debugger",
    "max_fail_percentage",
];

const INHERIT: &[&str] = &[
    "become",
    "become_user",
    "become_method",
    "environment",
    "ignore_errors",
    "ignore_unreachable",
    "check_mode",
    "diff",
    "no_log",
    "delegate_to",
    "run_once",
    "any_errors_fatal",
    "connection",
    "remote_user",
    "port",
];

const FREEFORM: &[&str] = &[
    "command",
    "shell",
    "raw",
    "script",
    "include_tasks",
    "import_tasks",
    "include_role",
    "import_role",
    "meta",
    "include_vars",
    "import_playbook",
    "set_fact_free",
    "add_host_free",
];

#[derive(Debug, Clone, Default)]
pub struct Task {
    pub name: Option<String>,
    pub module: String,
    pub args: Value,
    pub kw: Map<String, Value>,
    pub block: Vec<Task>,
    pub rescue: Vec<Task>,
    pub always: Vec<Task>,
    pub dir: PathBuf, // base dir for files/templates lookup
    pub is_block: bool,
    pub role: Option<String>,
    /// parameters passed to the role this task belongs to (scoped to that role)
    pub role_params: Option<std::sync::Arc<Map<String, Value>>>,
}

#[derive(Debug, Clone, Default)]
pub struct RoleRef {
    pub name: String,
    pub vars: Map<String, Value>,
    pub kw: Map<String, Value>,
}

#[derive(Debug, Clone, Default)]
pub struct Play {
    pub name: String,
    pub hosts: String,
    pub vars: Map<String, Value>,
    pub vars_files: Vec<Value>,
    pub pre_tasks: Vec<Task>,
    pub tasks: Vec<Task>,
    pub post_tasks: Vec<Task>,
    pub handlers: Vec<Task>,
    pub roles: Vec<RoleRef>,
    pub gather_facts: bool,
    pub serial: Vec<Value>,
    pub kw: Map<String, Value>,
    pub dir: PathBuf,
    pub strategy_free: bool,
}

#[derive(Debug, Default)]
pub struct Role {
    pub name: String,
    pub tasks: Vec<Task>,
    pub handlers: Vec<Task>,
    pub defaults: Map<String, Value>,
    pub vars: Map<String, Value>,
    pub deps: Vec<RoleRef>,
    pub allow_duplicates: bool,
}

pub fn norm_module(m: &str) -> String {
    m.rsplit('.').next().unwrap_or(m).to_string()
}

pub fn loose_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.to_lowercase().as_str() {
            "yes" | "true" | "on" | "1" => Some(true),
            "no" | "false" | "off" | "0" => Some(false),
            _ => None,
        },
        Value::Number(n) => n.as_i64().map(|i| i != 0),
        _ => None,
    }
}

/// Convert YAML to JSON values, resolving `!vault` (decrypt) and `!unsafe` tags.
pub fn yaml_to_json(v: serde_yaml::Value) -> Result<Value> {
    use serde_yaml::Value as Y;
    Ok(match v {
        Y::Null => Value::Null,
        Y::Bool(b) => Value::Bool(b),
        Y::Number(n) => serde_json::to_value(n)?,
        Y::String(s) => Value::String(s),
        Y::Sequence(a) => Value::Array(a.into_iter().map(yaml_to_json).collect::<Result<_>>()?),
        Y::Mapping(m) => {
            let mut o = Map::new();
            for (k, x) in m {
                let k = match k {
                    Y::String(s) => s,
                    other => serde_yaml::to_string(&other)?.trim().to_string(),
                };
                o.insert(k, yaml_to_json(x)?);
            }
            Value::Object(o)
        }
        Y::Tagged(t) => {
            let tag = t.tag.to_string();
            match (tag.trim_start_matches('!'), t.value) {
                ("vault", Y::String(s)) => {
                    let b = crate::vault::decrypt_auto(&s)?;
                    Value::String(
                        String::from_utf8_lossy(&b)
                            .trim_end_matches('\n')
                            .to_string(),
                    )
                }
                (_, other) => yaml_to_json(other)?,
            }
        }
    })
}

pub fn parse_yaml_str(t: &str) -> Result<Value> {
    if crate::vault::is_vault(t.as_bytes()) {
        let dec = crate::vault::decrypt_auto(t)?;
        return parse_yaml_str(&String::from_utf8_lossy(&dec));
    }
    if t.trim().is_empty() {
        return Ok(Value::Null);
    }
    yaml_to_json(serde_yaml::from_str(t)?)
}

pub fn read_yaml(p: &Path) -> Result<Value> {
    let t = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    parse_yaml_str(&t).with_context(|| format!("parsing {}", p.display()))
}

/// Parse `k=v k2="v 2"` into a map.
/// Free-form modules accept a few `key=value` options inside the command string
/// (`command: php installer.php chdir=/tmp creates=/usr/bin/x`); pull them out as arguments.
pub fn split_raw_params(module: &str, s: &str) -> Value {
    const KEYS: &[&str] = &[
        "creates",
        "removes",
        "chdir",
        "executable",
        "warn",
        "stdin",
        "stdin_add_newline",
        "strip_empty_ends",
    ];
    if !matches!(module, "command" | "shell" | "script" | "raw")
        || !KEYS.iter().any(|k| s.contains(&format!("{k}=")))
    {
        return serde_json::json!({ "_raw_params": s });
    }
    // tokenise on whitespace outside quotes and `{{ }}` / `{% %}`, keeping each token's original text
    let mut toks: Vec<String> = vec![];
    let mut cur = String::new();
    let mut q: Option<char> = None;
    let mut depth = 0i32;
    let cs: Vec<char> = s.chars().collect();
    let mut k = 0;
    while k < cs.len() {
        let c = cs[k];
        let two = cs.get(k + 1).map(|n| (c, *n));
        if matches!(two, Some(('{', '{')) | Some(('{', '%'))) {
            depth += 1;
            cur.push(c);
            cur.push(cs[k + 1]);
            k += 2;
            continue;
        }
        if depth > 0 && matches!(two, Some(('}', '}')) | Some(('%', '}'))) {
            depth -= 1;
            cur.push(c);
            cur.push(cs[k + 1]);
            k += 2;
            continue;
        }
        match (q, c) {
            (Some(qc), c) if c == qc && depth == 0 => {
                q = None;
                cur.push(c);
            }
            (Some(_), c) => cur.push(c),
            (None, '"') | (None, '\'') if depth == 0 => {
                q = Some(c);
                cur.push(c);
            }
            (None, c) if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
            }
            (None, c) => cur.push(c),
        }
        k += 1;
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    let mut args = Map::new();
    let mut rest = vec![];
    for t in toks {
        match t.split_once('=') {
            Some((k, v)) if KEYS.contains(&k) => {
                let v = v.trim_matches(|c| c == '"' || c == '\'');
                args.insert(k.to_string(), Value::String(v.to_string()));
            }
            _ => rest.push(t),
        }
    }
    args.insert("_raw_params".into(), Value::String(rest.join(" ")));
    Value::Object(args)
}

pub fn parse_kv(s: &str) -> Option<Map<String, Value>> {
    let mut out = Map::new();
    let mut cur = String::new();
    let mut toks = vec![];
    let mut q: Option<char> = None;
    let mut depth = 0i32; // inside {{ }} / {% %}
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        let two = if i + 1 < cs.len() {
            Some((c, cs[i + 1]))
        } else {
            None
        };
        if matches!(two, Some(('{', '{')) | Some(('{', '%'))) {
            depth += 1;
            cur.push(c);
            cur.push(cs[i + 1]);
            i += 2;
            continue;
        }
        if matches!(two, Some(('}', '}')) | Some(('%', '}'))) && depth > 0 {
            depth -= 1;
            cur.push(c);
            cur.push(cs[i + 1]);
            i += 2;
            continue;
        }
        match (q, c) {
            (Some(qc), c) if c == qc && depth == 0 => q = None,
            (Some(_), c) => cur.push(c),
            (None, '"') | (None, '\'') if depth == 0 => q = Some(c),
            (None, c) if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
            }
            (None, c) => cur.push(c),
        }
        i += 1;
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    for t in toks {
        let (k, v) = t.split_once('=')?;
        if k.is_empty()
            || !k
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            return None;
        }
        out.insert(k.to_string(), Value::String(v.to_string()));
    }
    if out.is_empty() { None } else { Some(out) }
}

pub fn parse_tasks(v: &Value, dir: &Path) -> Result<Vec<Task>> {
    match v {
        Value::Null => Ok(vec![]),
        Value::Array(a) => a.iter().map(|t| parse_task(t, dir)).collect(),
        _ => bail!("tasks must be a list"),
    }
}

pub fn parse_task(v: &Value, dir: &Path) -> Result<Task> {
    let m = v
        .as_object()
        .ok_or_else(|| anyhow!("task must be a mapping, got: {v}"))?;
    let mut t = Task {
        dir: dir.to_path_buf(),
        ..Default::default()
    };
    // normalise with_* loops
    let mut kw = Map::new();
    let mut module: Option<(String, Value)> = None;
    for (k, val) in m {
        if k == "action" || k == "local_action" {
            let (mname, margs) = match val {
                Value::String(s) => {
                    let (a, b) = s
                        .split_once(char::is_whitespace)
                        .unwrap_or((s.as_str(), ""));
                    (a.to_string(), Value::String(b.to_string()))
                }
                Value::Object(o) => {
                    let mut o = o.clone();
                    let mn = o
                        .remove("module")
                        .and_then(|x| x.as_str().map(String::from))
                        .ok_or_else(|| anyhow!("action without module"))?;
                    (mn, Value::Object(o))
                }
                _ => bail!("bad action"),
            };
            if k == "local_action" {
                kw.insert("delegate_to".into(), "localhost".into());
            }
            module = Some((mname, margs));
        } else if let Some(w) = k.strip_prefix("with_") {
            let looped = match w {
                "items" | "list" => val.clone(),
                "dict" => Value::String(format!(
                    "{{{{ ({}) | dict2items }}}}",
                    val.as_str()
                        .map(|s| s
                            .trim()
                            .trim_start_matches("{{")
                            .trim_end_matches("}}")
                            .trim()
                            .to_string())
                        .unwrap_or_else(|| val.to_string())
                )),
                "first_found" | "fileglob" | "subelements" | "nested" | "product" | "together"
                | "zip" | "sequence" | "indexed_items" | "flattened" | "random_choice"
                | "cartesian" => {
                    kw.insert("_with".into(), w.into());
                    val.clone()
                }
                _ => bail!("unsupported loop construct '{k}'"),
            };
            kw.insert("loop".into(), looped);
            if w == "items" {
                kw.insert("_flatten_loop".into(), true.into());
            }
        } else if KEYWORDS.contains(&k.as_str()) {
            kw.insert(k.clone(), val.clone());
        } else if module.is_none() {
            module = Some((k.clone(), val.clone()));
        } else {
            bail!(
                "unknown task keyword or second module '{k}' in task {:?}",
                m.get("name")
            );
        }
    }
    t.name = kw.get("name").and_then(|n| n.as_str()).map(String::from);
    if let Some(Value::Array(_)) = kw.get("block") {
        t.is_block = true;
        t.block = parse_tasks(&kw["block"], dir)?;
        t.rescue = parse_tasks(kw.get("rescue").unwrap_or(&Value::Null), dir)?;
        t.always = parse_tasks(kw.get("always").unwrap_or(&Value::Null), dir)?;
        t.module = "block".into();
        for k in ["block", "rescue", "always"] {
            kw.remove(k);
        }
        t.kw = kw;
        return Ok(t);
    }
    let (mname, margs) =
        module.ok_or_else(|| anyhow!("task has no module: {}", Value::Object(m.clone())))?;
    t.module = norm_module(&mname);
    t.args = match margs {
        Value::String(s) => {
            if FREEFORM.contains(&t.module.as_str()) {
                split_raw_params(&t.module, &s)
            } else if let Some(kv) = parse_kv(&s) {
                Value::Object(kv)
            } else {
                serde_json::json!({ "_raw_params": s })
            }
        }
        Value::Null => Value::Object(Map::new()),
        other => other,
    };
    if let (Some(Value::Object(extra)), Value::Object(a)) = (kw.get("args"), &mut t.args) {
        a.extend(extra.clone());
    }
    kw.remove("args");
    t.kw = kw;
    Ok(t)
}

fn as_list(v: Option<&Value>) -> Vec<Value> {
    match v {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::Null) | None => vec![],
        Some(x) => vec![x.clone()],
    }
}

/// Push parent keywords down into a task list (top-down, once).
pub fn apply_inherit(parent: &Map<String, Value>, tasks: &mut [Task]) {
    for t in tasks.iter_mut() {
        for k in INHERIT {
            if !t.kw.contains_key(*k)
                && let Some(v) = parent.get(*k)
            {
                t.kw.insert((*k).into(), v.clone());
            }
        }
        if let Some(Value::Object(pe)) = parent.get("environment") {
            let mut e = pe.clone();
            if let Some(Value::Object(ce)) = t.kw.get("environment") {
                e.extend(ce.clone());
            }
            t.kw.insert("environment".into(), Value::Object(e));
        }
        if let Some(Value::Object(pv)) = parent.get("vars") {
            let mut e = pv.clone();
            if let Some(Value::Object(cv)) = t.kw.get("vars") {
                e.extend(cv.clone());
            }
            t.kw.insert("vars".into(), Value::Object(e));
        }
        let mut tags = as_list(parent.get("tags"));
        for x in as_list(t.kw.get("tags")) {
            if !tags.contains(&x) {
                tags.push(x);
            }
        }
        if !tags.is_empty() {
            t.kw.insert("tags".into(), Value::Array(tags));
        }
        let mut when = as_list(parent.get("when"));
        when.extend(as_list(t.kw.get("when")));
        if !when.is_empty() {
            t.kw.insert("when".into(), Value::Array(when));
        }
        if t.is_block {
            let kw = t.kw.clone();
            apply_inherit(&kw, &mut t.block);
            apply_inherit(&kw, &mut t.rescue);
            apply_inherit(&kw, &mut t.always);
        }
    }
}

pub fn load_playbook(path: &Path) -> Result<Vec<Play>> {
    let dir = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let v = read_yaml(path)?;
    let items = v
        .as_array()
        .ok_or_else(|| anyhow!("{} is not a list of plays", path.display()))?;
    let mut plays = vec![];
    for item in items {
        let m = item
            .as_object()
            .ok_or_else(|| anyhow!("play must be a mapping"))?;
        if let Some(Value::String(pb)) = m.get("import_playbook") {
            plays.extend(load_playbook(&dir.join(pb))?);
            continue;
        }
        plays.push(parse_play(m, &dir)?);
    }
    Ok(plays)
}

fn parse_play(m: &Map<String, Value>, dir: &Path) -> Result<Play> {
    let mut p = Play {
        dir: dir.to_path_buf(),
        gather_facts: true,
        ..Default::default()
    };
    p.hosts = match m.get("hosts") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str())
            .collect::<Vec<_>>()
            .join(":"),
        _ => bail!("play is missing 'hosts'"),
    };
    p.name = m
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or(&p.hosts)
        .to_string();
    if let Some(Value::Object(v)) = m.get("vars") {
        p.vars = v.clone();
    }
    p.vars_files = as_list(m.get("vars_files"));
    let empty = Value::Null;
    p.pre_tasks = parse_tasks(m.get("pre_tasks").unwrap_or(&empty), dir)?;
    p.tasks = parse_tasks(m.get("tasks").unwrap_or(&empty), dir)?;
    p.post_tasks = parse_tasks(m.get("post_tasks").unwrap_or(&empty), dir)?;
    p.handlers = parse_tasks(m.get("handlers").unwrap_or(&empty), dir)?;
    if let Some(g) = m.get("gather_facts").and_then(loose_bool) {
        p.gather_facts = g;
    }
    p.serial = as_list(m.get("serial"));
    p.strategy_free = m.get("strategy").and_then(|s| s.as_str()) == Some("free");
    for r in as_list(m.get("roles")) {
        let mut rr = RoleRef::default();
        match &r {
            Value::String(s) => rr.name = s.clone(),
            Value::Object(o) => {
                rr.name = o
                    .get("role")
                    .or(o.get("name"))
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| anyhow!("role entry without name"))?
                    .to_string();
                for (k, v) in o {
                    if k == "role" || k == "name" {
                        continue;
                    }
                    if KEYWORDS.contains(&k.as_str()) {
                        rr.kw.insert(k.clone(), v.clone());
                    } else {
                        rr.vars.insert(k.clone(), v.clone());
                    }
                }
            }
            _ => bail!("bad role entry"),
        }
        p.roles.push(rr);
    }
    for k in INHERIT
        .iter()
        .chain(["tags", "when", "max_fail_percentage"].iter())
    {
        if let Some(v) = m.get(*k) {
            p.kw.insert((*k).into(), v.clone());
        }
    }
    let kw = p.kw.clone();
    for list in [
        &mut p.pre_tasks,
        &mut p.tasks,
        &mut p.post_tasks,
        &mut p.handlers,
    ] {
        apply_inherit(&kw, list);
    }
    Ok(p)
}

/// Directories searched for `ansible_collections/` (ANSIBLE_COLLECTIONS_PATH, next to the playbook, ~/.ansible/collections).
pub fn collection_paths(play_dir: &Path) -> Vec<PathBuf> {
    let mut v = vec![];
    for k in ["ANSIBLE_COLLECTIONS_PATH", "ANSIBLE_COLLECTIONS_PATHS"] {
        if let Ok(p) = std::env::var(k) {
            v.extend(p.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
        }
    }
    v.push(play_dir.join("collections"));
    v.push(play_dir.to_path_buf());
    if let Ok(h) = std::env::var("HOME") {
        v.push(Path::new(&h).join(".ansible/collections"));
    }
    v.push(PathBuf::from("/usr/share/ansible/collections"));
    v
}

pub fn find_role(name: &str, play_dir: &Path) -> Result<PathBuf> {
    if name.contains('/') {
        let p = Path::new(name);
        if p.is_dir() {
            return Ok(p.to_path_buf());
        }
    }
    let mut cands = vec![];
    // role shipped in a collection: namespace.collection.role
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() == 3 {
        for base in collection_paths(play_dir) {
            cands.push(
                base.join("ansible_collections")
                    .join(parts[0])
                    .join(parts[1])
                    .join("roles")
                    .join(parts[2]),
            );
        }
    }
    cands.extend([
        play_dir.join("roles").join(name),
        play_dir.join("..").join("roles").join(name),
    ]);
    if let Some(rp) = &crate::config::get().roles_path {
        cands.extend(rp.split(':').map(|d| Path::new(d).join(name)));
    }
    if let Ok(h) = std::env::var("HOME") {
        cands.push(Path::new(&h).join(".ansible/roles").join(name));
    }
    if let Ok(cwd) = std::env::current_dir() {
        cands.push(cwd.join("roles").join(name));
    }
    cands.push(PathBuf::from(format!("/etc/ansible/roles/{name}")));
    cands.into_iter().find(|p| p.is_dir()).ok_or_else(|| {
        anyhow!(
            "role '{name}' not found (searched relative to {})",
            play_dir.display()
        )
    })
}

fn main_file(dir: &Path) -> Option<PathBuf> {
    for n in ["main.yml", "main.yaml", "main"] {
        let p = dir.join(n);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Attach role parameters to every task (recursively) that does not have any yet.
pub fn set_role_params(ts: &mut [Task], p: &std::sync::Arc<Map<String, Value>>) {
    for t in ts {
        if t.role_params.is_none() {
            t.role_params = Some(p.clone());
        }
        set_role_params(&mut t.block, p);
        set_role_params(&mut t.rescue, p);
        set_role_params(&mut t.always, p);
    }
}

pub fn tag_role(ts: &mut [Task], role: &str) {
    for t in ts {
        if t.role.is_none() {
            t.role = Some(role.to_string());
        }
        tag_role(&mut t.block, role);
        tag_role(&mut t.rescue, role);
        tag_role(&mut t.always, role);
    }
}

pub fn load_role(name: &str, play_dir: &Path, tasks_from: Option<&str>) -> Result<Role> {
    let path = find_role(name, play_dir)?;
    let mut role = Role {
        name: name.to_string(),
        ..Default::default()
    };
    let tdir = path.join("tasks");
    let tfile = match tasks_from {
        Some(f) => {
            let f = if f.ends_with(".yml") || f.ends_with(".yaml") {
                f.to_string()
            } else {
                format!("{f}.yml")
            };
            Some(tdir.join(f))
        }
        None => main_file(&tdir),
    };
    if let Some(f) = tfile {
        role.tasks = parse_tasks(&read_yaml(&f)?, &path)?;
        tag_role(&mut role.tasks, name);
    }
    if let Some(f) = main_file(&path.join("handlers")) {
        role.handlers = parse_tasks(&read_yaml(&f)?, &path)?;
        tag_role(&mut role.handlers, name);
    }
    for (sub, target) in [("defaults", 0), ("vars", 1)] {
        if let Some(f) = main_file(&path.join(sub))
            && let Value::Object(m) = read_yaml(&f)?
        {
            if target == 0 {
                role.defaults = m
            } else {
                role.vars = m
            }
        }
    }
    if let Some(f) = main_file(&path.join("meta"))
        && let Value::Object(m) = read_yaml(&f)?
    {
        role.allow_duplicates = m
            .get("allow_duplicates")
            .and_then(loose_bool)
            .unwrap_or(false);
        for d in as_list(m.get("dependencies")) {
            let name = match &d {
                Value::String(s) => s.clone(),
                Value::Object(o) => o
                    .get("role")
                    .or(o.get("name"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                _ => continue,
            };
            let mut rr = RoleRef {
                name,
                ..Default::default()
            };
            if let Value::Object(o) = &d {
                for (k, v) in o {
                    if k == "role" || k == "name" {
                        continue;
                    }
                    // `vars: {a: 1}` and bare `a: 1` both pass role parameters
                    match (k.as_str(), v) {
                        ("vars", Value::Object(m)) => rr.vars.extend(m.clone()),
                        _ => {
                            rr.vars.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            role.deps.push(rr);
        }
    }
    Ok(role)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_params_keep_templates_intact() {
        let v = split_raw_params(
            "command",
            "{{ bin }} global require {{ item.name }}:{{ item.rel | default('x') }} creates={{ home }}/vendor/{{ item.name }}",
        );
        assert_eq!(
            v["_raw_params"],
            "{{ bin }} global require {{ item.name }}:{{ item.rel | default('x') }}"
        );
        assert_eq!(v["creates"], "{{ home }}/vendor/{{ item.name }}");
    }

    #[test]
    fn raw_params_option_extraction() {
        let v = split_raw_params(
            "command",
            "php installer.php --2 chdir=/tmp creates='/usr/bin/x y'",
        );
        assert_eq!(v["_raw_params"], "php installer.php --2");
        assert_eq!(v["chdir"], "/tmp");
        assert_eq!(v["creates"], "/usr/bin/x y");
        // untouched when no option is present, or for other modules
        assert_eq!(
            split_raw_params("command", "echo a=b")["_raw_params"],
            "echo a=b"
        );
        assert_eq!(
            split_raw_params("copy", "x chdir=/")["_raw_params"],
            "x chdir=/"
        );
    }
}
