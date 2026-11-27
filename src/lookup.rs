//! Ansible lookup plugins (`lookup()` / `query()`), implemented natively.
use crate::template::Vars;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// Result of a lookup: a list of terms.
pub type Terms = Vec<Value>;

fn vstr(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        o => o.to_string(),
    }
}

fn list_of(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Null => vec![],
        o => vec![o.clone()],
    }
}

fn flatten(v: &Value, out: &mut Vec<Value>) {
    match v {
        Value::Array(a) => a.iter().for_each(|x| flatten(x, out)),
        o => out.push(o.clone()),
    }
}

/// Directories a relative file may live in: role/playbook `files`, `templates`, `vars`, the dir itself.
fn search_dirs(vars: &Vars, subs: &[&str]) -> Vec<PathBuf> {
    let mut out = vec![];
    for k in ["role_path", "playbook_dir"] {
        if let Some(Value::String(b)) = vars.get(k) {
            for s in subs {
                out.push(Path::new(b).join(s));
            }
            out.push(PathBuf::from(b));
        }
    }
    out
}

pub fn find_file(vars: &Vars, name: &str, subs: &[&str]) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() || name.starts_with("~/") {
        let p = if let (Some(rest), Ok(h)) = (name.strip_prefix("~/"), std::env::var("HOME")) {
            Path::new(&h).join(rest)
        } else {
            p.to_path_buf()
        };
        return p.exists().then_some(p);
    }
    for d in search_dirs(vars, subs) {
        let c = d.join(name);
        if c.exists() {
            return Some(c);
        }
    }
    p.exists().then(|| p.to_path_buf())
}

pub struct Ctx<'a> {
    pub vars: &'a Vars,
    /// render a template string with the current variables (for the `template` lookup)
    pub render: &'a dyn Fn(&str) -> Result<String, String>,
}

pub fn run(kind: &str, args: &[Value], kw: &Map<String, Value>, cx: &Ctx) -> Result<Terms, String> {
    let kind = kind.rsplit('.').next().unwrap_or(kind);
    let first = || args.first().map(vstr).unwrap_or_default();
    let default = kw.get("default").cloned();
    match kind {
        "env" => Ok(args
            .iter()
            .map(|a| {
                Value::String(
                    std::env::var(vstr(a))
                        .unwrap_or_else(|_| default.as_ref().map(vstr).unwrap_or_default()),
                )
            })
            .collect()),
        "file" | "lines" | "template" | "fileglob" | "unvault" => {
            let mut out = vec![];
            for a in args {
                let name = vstr(a);
                if kind == "fileglob" {
                    let full = find_file(
                        cx.vars,
                        Path::new(&name)
                            .parent()
                            .and_then(|p| p.to_str())
                            .unwrap_or("."),
                        &["files"],
                    )
                    .unwrap_or_else(|| PathBuf::from("."));
                    let pat = Path::new(&name)
                        .file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let re = regex::Regex::new(&format!(
                        "^{}$",
                        regex::escape(&pat).replace("\\*", ".*").replace("\\?", ".")
                    ))
                    .map_err(|e| e.to_string())?;
                    let mut hits: Vec<String> = std::fs::read_dir(&full)
                        .map(|rd| {
                            rd.flatten()
                                .filter(|e| {
                                    e.path().is_file()
                                        && re.is_match(&e.file_name().to_string_lossy())
                                })
                                .map(|e| e.path().display().to_string())
                                .collect()
                        })
                        .unwrap_or_default();
                    hits.sort();
                    out.extend(hits.into_iter().map(Value::String));
                    continue;
                }
                let subs: &[&str] = if kind == "template" {
                    &["templates", "files"]
                } else {
                    &["files", "templates"]
                };
                let path = find_file(cx.vars, &name, subs)
                    .ok_or_else(|| format!("could not locate file in lookup: {name}"))?;
                let bytes = crate::vault::read_file(&path)
                    .map_err(|e| format!("lookup {kind} {}: {e}", path.display()))?;
                let text = String::from_utf8_lossy(&bytes).into_owned();
                match kind {
                    "lines" => out.extend(text.lines().map(|l| Value::String(l.to_string()))),
                    "template" => out.push(Value::String((cx.render)(&text)?)),
                    _ => out.push(Value::String(
                        if kw.get("rstrip").and_then(|r| r.as_bool()).unwrap_or(true) {
                            text.trim_end_matches('\n').to_string()
                        } else {
                            text
                        },
                    )),
                }
            }
            Ok(out)
        }
        "pipe" => args
            .iter()
            .map(|a| {
                let o = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(vstr(a))
                    .output()
                    .map_err(|e| e.to_string())?;
                Ok(Value::String(
                    String::from_utf8_lossy(&o.stdout).trim_end().to_string(),
                ))
            })
            .collect(),
        "vars" => args
            .iter()
            .map(|a| match cx.vars.get(&vstr(a)) {
                Some(v) => Ok(v.clone()),
                None => default
                    .clone()
                    .ok_or_else(|| format!("No variable found with this name: {}", vstr(a))),
            })
            .collect(),
        "varnames" => {
            let mut out = vec![];
            for a in args {
                let re = regex::Regex::new(&vstr(a)).map_err(|e| e.to_string())?;
                let mut names: Vec<&String> = cx.vars.keys().filter(|k| re.is_match(k)).collect();
                names.sort();
                out.extend(names.into_iter().map(|n| Value::String(n.clone())));
            }
            Ok(out)
        }
        "items" | "list" | "flattened" => {
            let mut out = vec![];
            for a in args {
                if kind == "items" || kind == "flattened" {
                    flatten(a, &mut out);
                } else {
                    out.extend(list_of(a));
                }
            }
            Ok(out)
        }
        "indexed_items" => Ok(args
            .iter()
            .flat_map(list_of)
            .enumerate()
            .map(|(i, x)| Value::Array(vec![Value::from(i), x]))
            .collect()),
        "random_choice" => Ok(args.iter().flat_map(list_of).take(1).collect()),
        "dict" => Ok(args
            .iter()
            .flat_map(|a| {
                a.as_object()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(k, v)| serde_json::json!({"key": k, "value": v}))
                    .collect::<Vec<_>>()
            })
            .collect()),
        "together" | "zip" => {
            let ls: Vec<Vec<Value>> = args.iter().map(list_of).collect();
            let n = ls.iter().map(|l| l.len()).min().unwrap_or(0);
            Ok((0..n)
                .map(|i| Value::Array(ls.iter().map(|l| l[i].clone()).collect()))
                .collect())
        }
        "nested" | "product" | "cartesian" => {
            let mut acc: Vec<Vec<Value>> = vec![vec![]];
            for a in args {
                let items = list_of(a);
                acc = acc
                    .into_iter()
                    .flat_map(|p| {
                        items
                            .iter()
                            .map(move |i| {
                                let mut q = p.clone();
                                q.push(i.clone());
                                q
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect();
            }
            Ok(acc.into_iter().map(Value::Array).collect())
        }
        "subelements" => {
            let items = args.first().map(list_of).unwrap_or_default();
            let key = args.get(1).map(vstr).unwrap_or_default();
            let mut out = vec![];
            for it in items {
                for sub in list_of(it.get(&key).unwrap_or(&Value::Null)) {
                    out.push(Value::Array(vec![it.clone(), sub]));
                }
            }
            Ok(out)
        }
        "sequence" => {
            let mut out = vec![];
            for a in args {
                let mut start = 1i64;
                let mut end: Option<i64> = None;
                let mut count: Option<i64> = None;
                let mut stride = 1i64;
                let mut format = None;
                let spec: Vec<(String, String)> = match a {
                    Value::String(s) => s
                        .split_whitespace()
                        .filter_map(|kv| kv.split_once('='))
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                    Value::Object(o) => o.iter().map(|(k, v)| (k.clone(), vstr(v))).collect(),
                    _ => vec![],
                };
                for (k, v) in spec {
                    match k.as_str() {
                        "start" => start = v.parse().map_err(|_| "bad start")?,
                        "end" => end = Some(v.parse().map_err(|_| "bad end")?),
                        "count" => count = Some(v.parse().map_err(|_| "bad count")?),
                        "stride" => stride = v.parse().map_err(|_| "bad stride")?,
                        "format" => format = Some(v),
                        _ => {}
                    }
                }
                let end = end
                    .or(count.map(|c| start + (c - 1) * stride.max(1)))
                    .ok_or("sequence needs end or count")?;
                let mut i = start;
                while i <= end && stride > 0 {
                    out.push(Value::String(match &format {
                        Some(f) => f
                            .replace("%02d", &format!("{i:02}"))
                            .replace("%03d", &format!("{i:03}"))
                            .replace("%d", &i.to_string()),
                        None => i.to_string(),
                    }));
                    i += stride;
                }
            }
            Ok(out)
        }
        "first_found" => {
            let arg = args.first().cloned().unwrap_or(Value::Null);
            let (files, paths, skip) = match &arg {
                Value::Object(o) => (
                    list_of(o.get("files").unwrap_or(&Value::Null)),
                    list_of(o.get("paths").unwrap_or(&Value::Null)),
                    o.get("skip").and_then(|s| s.as_bool()).unwrap_or(false),
                ),
                other => (
                    list_of(other),
                    vec![],
                    kw.get("skip").and_then(|s| s.as_bool()).unwrap_or(false),
                ),
            };
            let mut dirs: Vec<PathBuf> = vec![];
            for p in &paths {
                let p = vstr(p);
                if p.starts_with('/') {
                    dirs.push(PathBuf::from(p));
                } else {
                    dirs.extend(search_dirs(cx.vars, &[]).into_iter().map(|b| b.join(&p)));
                }
            }
            dirs.extend(search_dirs(
                cx.vars,
                &["vars", "tasks", "files", "templates", "defaults"],
            ));
            for f in &files {
                let f = vstr(f);
                if f.starts_with('/') && Path::new(&f).exists() {
                    return Ok(vec![Value::String(f)]);
                }
                if let Some(c) = dirs.iter().map(|d| d.join(&f)).find(|c| c.is_file()) {
                    return Ok(vec![Value::String(c.display().to_string())]);
                }
            }
            if skip {
                Ok(vec![])
            } else {
                Err("No file was found when using first_found".into())
            }
        }
        "ini" => {
            // lookup('ini', 'key section=sec file=path default=x')
            let spec = first();
            let mut parts = spec.split_whitespace();
            let key = parts.next().unwrap_or("").to_string();
            let (mut section, mut file) = (String::new(), String::from("ansible.ini"));
            for p in parts {
                if let Some((k, v)) = p.split_once('=') {
                    match k {
                        "section" => section = v.into(),
                        "file" => file = v.into(),
                        _ => {}
                    }
                }
            }
            let path = find_file(cx.vars, &file, &["files"])
                .ok_or_else(|| format!("ini file {file} not found"))?;
            let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let mut sec = String::new();
            for l in text.lines() {
                let l = l.trim();
                if l.starts_with('[') && l.ends_with(']') {
                    sec = l[1..l.len() - 1].to_string();
                } else if sec == section
                    && let Some((k, v)) = l.split_once('=')
                    && k.trim() == key
                {
                    return Ok(vec![Value::String(v.trim().to_string())]);
                }
            }
            default
                .map(|d| vec![d])
                .ok_or_else(|| format!("key {key} not found in {file}"))
        }
        other => Err(format!("unsupported lookup plugin '{other}'")),
    }
}

/// `lookup()` semantics: list of terms; strings are comma-joined unless `wantlist`, otherwise a list.
pub fn lookup_value(terms: Terms, wantlist: bool) -> Value {
    if wantlist {
        return Value::Array(terms);
    }
    if terms.len() == 1 {
        return terms.into_iter().next().unwrap();
    }
    if terms.iter().all(|t| t.is_string()) {
        return Value::String(terms.iter().map(vstr).collect::<Vec<_>>().join(","));
    }
    Value::Array(terms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cx<'a>(vars: &'a Vars, r: &'a dyn Fn(&str) -> Result<String, String>) -> Ctx<'a> {
        Ctx { vars, render: r }
    }

    #[test]
    fn list_lookups() {
        let vars = Vars::new();
        let r = |s: &str| Ok(s.to_string());
        let c = cx(&vars, &r);
        let none = Map::new();
        assert_eq!(
            run("items", &[json!([1, [2, 3]]), json!(4)], &none, &c).unwrap(),
            vec![json!(1), json!(2), json!(3), json!(4)]
        );
        assert_eq!(
            run("sequence", &[json!("start=2 end=8 stride=3")], &none, &c).unwrap(),
            vec![json!("2"), json!("5"), json!("8")]
        );
        assert_eq!(
            run(
                "sequence",
                &[json!({"count": 2, "format": "n%02d"})],
                &none,
                &c
            )
            .unwrap(),
            vec![json!("n01"), json!("n02")]
        );
        assert_eq!(
            run("nested", &[json!([1, 2]), json!(["a"])], &none, &c).unwrap(),
            vec![json!([1, "a"]), json!([2, "a"])]
        );
        assert!(run("nope", &[], &none, &c).is_err());
    }

    #[test]
    fn vars_and_result_shape() {
        let mut vars = Vars::new();
        vars.insert("x".into(), json!(7));
        let r = |s: &str| Ok(s.to_string());
        let c = cx(&vars, &r);
        assert_eq!(
            run("vars", &[json!("x")], &Map::new(), &c).unwrap(),
            vec![json!(7)]
        );
        assert!(run("vars", &[json!("missing")], &Map::new(), &c).is_err());
        let mut kw = Map::new();
        kw.insert("default".into(), json!("d"));
        assert_eq!(
            run("vars", &[json!("missing")], &kw, &c).unwrap(),
            vec![json!("d")]
        );
        assert_eq!(
            lookup_value(vec![json!("a"), json!("b")], false),
            json!("a,b")
        );
        assert_eq!(lookup_value(vec![json!(1), json!(2)], false), json!([1, 2]));
        assert_eq!(
            lookup_value(vec![json!("a"), json!("b")], true),
            json!(["a", "b"])
        );
    }
}
