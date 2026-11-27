//! Jinja2 templating with Ansible-flavoured filters, tests and lookups.
use anyhow::{Result, anyhow};
use minijinja::value::{Kwargs, Value as MValue};
use minijinja::{Environment, Error, ErrorKind, UndefinedBehavior};
use serde_json::{Map, Value};

pub type Vars = Map<String, Value>;

thread_local! {
    /// Variables of the render in progress on this thread (for lookups that need them).
    static CURRENT_VARS: std::cell::RefCell<Vec<*const Vars>> = const { std::cell::RefCell::new(Vec::new()) };
}

struct VarsGuard;
impl VarsGuard {
    fn new(vars: &Vars) -> VarsGuard {
        CURRENT_VARS.with(|c| c.borrow_mut().push(vars as *const Vars));
        VarsGuard
    }
}
impl Drop for VarsGuard {
    fn drop(&mut self) {
        CURRENT_VARS.with(|c| {
            c.borrow_mut().pop();
        });
    }
}

/// Run `f` with the variables of the render in progress (empty map outside a render).
fn with_vars<R>(f: impl FnOnce(&Vars) -> R) -> R {
    let ptr = CURRENT_VARS.with(|c| c.borrow().last().copied());
    match ptr {
        // SAFETY: the pointer is pushed by VarsGuard for the duration of a synchronous render on
        // this thread and popped when the guard drops, so it is valid whenever it is on the stack.
        Some(p) => f(unsafe { &*p }),
        None => f(&Vars::new()),
    }
}

pub struct Templar {
    env: Environment<'static>,
}

fn err(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidOperation, msg.into())
}

fn to_json_val(v: &MValue) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn from_json_val(v: &Value) -> MValue {
    MValue::from(minijinja::value::Serde(v))
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn parse_bool_str(s: &str) -> Option<bool> {
    match s.trim().to_lowercase().as_str() {
        "true" | "yes" | "on" | "1" | "y" | "t" => Some(true),
        "false" | "no" | "off" | "0" | "n" | "f" => Some(false),
        _ => None,
    }
}

fn mk_regex(p: &str, kw: &Kwargs) -> Result<regex::Regex, Error> {
    let ic: bool = kw.get("ignorecase").unwrap_or(false);
    let ml: bool = kw.get("multiline").unwrap_or(false);
    regex::RegexBuilder::new(p)
        .case_insensitive(ic)
        .multi_line(ml)
        .build()
        .map_err(|e| err(format!("bad regex: {e}")))
}

/// Convert python-style backrefs (\1, \g<name>) to the regex crate syntax.
pub fn py_repl(r: &str) -> String {
    let re = regex::Regex::new(r"\\(\d+)|\\g<(\w+)>").unwrap();
    let r = r.replace('$', "$$");
    re.replace_all(&r, |c: &regex::Captures| {
        let n = c.get(1).or(c.get(2)).unwrap().as_str();
        format!("${{{n}}}")
    })
    .into_owned()
}

fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let p = |s: &str| -> Vec<u64> {
        s.split(|c: char| !c.is_ascii_digit())
            .filter(|x| !x.is_empty())
            .map(|x| x.parse().unwrap_or(0))
            .collect()
    };
    let (mut x, mut y) = (p(a), p(b));
    let n = x.len().max(y.len());
    x.resize(n, 0);
    y.resize(n, 0);
    x.cmp(&y)
}

fn merge(a: &Value, b: &Value, recursive: bool) -> Value {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut out = x.clone();
            for (k, v) in y {
                let nv = match (out.get(k), recursive) {
                    (Some(old), true) => merge(old, v, true),
                    _ => v.clone(),
                };
                out.insert(k.clone(), nv);
            }
            Value::Object(out)
        }
        _ => b.clone(),
    }
}

fn flatten(v: &Value, out: &mut Vec<Value>) {
    match v {
        Value::Array(a) => a.iter().for_each(|x| flatten(x, out)),
        other => out.push(other.clone()),
    }
}

impl Templar {
    pub fn new() -> Self {
        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::Chainable);
        // `{% include %}` / `{% import %}` / `{% extends %}`: look in role/playbook templates dirs
        env.set_loader(|name| {
            Ok(with_vars(|vars| {
                crate::lookup::find_file(vars, name, &["templates", "files"])
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    // included templates lose one trailing newline, as in Jinja2's default
                    .map(|t| t.strip_suffix('\n').map(String::from).unwrap_or(t))
            }))
        });
        // like Ansible, `none` renders as an empty string inside text templates
        env.set_formatter(|out, state, value| {
            if value.is_none() {
                Ok(())
            } else {
                minijinja::escape_formatter(out, state, value)
            }
        });
        env.set_syntax(
            minijinja::syntax::SyntaxConfig::builder()
                .trim_blocks(true)
                .keep_trailing_newline(true)
                .build()
                .unwrap(),
        );
        minijinja_contrib::add_to_environment(&mut env);
        env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);

        env.add_filter("to_json", |v: MValue, kw: Kwargs| {
            let ind: Option<usize> = kw.get("indent").ok();
            crate::pyfmt::json_dumps(
                &to_json_val(&v),
                ind,
                kw.get("sort_keys").unwrap_or(false),
                kw.get("ensure_ascii").unwrap_or(true),
            )
        });
        env.add_filter("to_nice_json", |v: MValue, kw: Kwargs| {
            crate::pyfmt::json_dumps(
                &to_json_val(&v),
                Some(kw.get("indent").unwrap_or(4)),
                kw.get("sort_keys").unwrap_or(true),
                kw.get("ensure_ascii").unwrap_or(true),
            )
        });
        env.add_filter("to_yaml", |v: MValue, kw: Kwargs| {
            // PyYAML default_flow_style=None: collections of scalars use flow style unless told otherwise
            let flow_leaves = kw.get::<bool>("default_flow_style").is_err();
            crate::pyfmt::yaml_dump_opts(
                &to_json_val(&v),
                kw.get("indent").unwrap_or(2),
                kw.get("sort_keys").unwrap_or(true),
                flow_leaves,
            )
        });
        env.add_filter("to_nice_yaml", |v: MValue, kw: Kwargs| {
            crate::pyfmt::yaml_dump(
                &to_json_val(&v),
                kw.get("indent").unwrap_or(4),
                kw.get("sort_keys").unwrap_or(true),
            )
        });
        env.add_filter("from_json", |s: String| -> Result<MValue, Error> {
            let v: Value = serde_json::from_str(&s).map_err(|e| err(e.to_string()))?;
            Ok(from_json_val(&v))
        });
        env.add_filter("from_yaml", |s: String| -> Result<MValue, Error> {
            let v: Value = serde_yaml::from_str(&s).map_err(|e| err(e.to_string()))?;
            Ok(from_json_val(&v))
        });
        env.add_filter("b64encode", |s: String| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(s)
        });
        env.add_filter("b64decode", |s: String| -> Result<String, Error> {
            use base64::Engine;
            let b = base64::engine::general_purpose::STANDARD
                .decode(s.trim())
                .map_err(|e| err(e.to_string()))?;
            Ok(String::from_utf8_lossy(&b).into_owned())
        });
        env.add_filter("basename", |s: String| {
            s.rsplit('/').next().unwrap_or("").to_string()
        });
        env.add_filter("dirname", |s: String| match s.rfind('/') {
            Some(0) => "/".to_string(),
            Some(i) => s[..i].to_string(),
            None => String::new(),
        });
        env.add_filter("quote", |s: MValue| shell_quote(&s.to_string()));
        env.add_filter("bool", |v: MValue| -> bool {
            let j = to_json_val(&v);
            match &j {
                Value::String(s) => parse_bool_str(s).unwrap_or(false),
                other => truthy(other),
            }
        });
        env.add_filter(
            "mandatory",
            |v: MValue, msg: Option<String>| -> Result<MValue, Error> {
                if v.is_undefined() {
                    Err(err(msg.unwrap_or_else(|| {
                        "Mandatory variable not defined.".into()
                    })))
                } else {
                    Ok(v)
                }
            },
        );
        env.add_filter(
            "ternary",
            |v: MValue, a: MValue, b: MValue| if v.is_true() { a } else { b },
        );
        env.add_filter(
            "regex_replace",
            |s: String, pat: String, rep: Option<String>, kw: Kwargs| -> Result<String, Error> {
                let re = mk_regex(&pat, &kw)?;
                Ok(re
                    .replace_all(&s, py_repl(&rep.unwrap_or_default()).as_str())
                    .into_owned())
            },
        );
        env.add_filter(
            "regex_search",
            |s: String, pat: String, kw: Kwargs| -> Result<MValue, Error> {
                let re = mk_regex(&pat, &kw)?;
                Ok(match re.find(&s) {
                    Some(m) => MValue::from(m.as_str()),
                    None => MValue::from(()),
                })
            },
        );
        env.add_filter(
            "regex_findall",
            |s: String, pat: String, kw: Kwargs| -> Result<Vec<String>, Error> {
                let re = mk_regex(&pat, &kw)?;
                Ok(re.find_iter(&s).map(|m| m.as_str().to_string()).collect())
            },
        );
        env.add_filter("regex_escape", |s: String| regex::escape(&s));
        env.add_filter(
            "combine",
            |v: MValue, rest: minijinja::value::Rest<MValue>, kw: Kwargs| {
                let rec: bool = kw.get("recursive").unwrap_or(false);
                let mut acc = to_json_val(&v);
                for r in rest.iter() {
                    acc = merge(&acc, &to_json_val(r), rec);
                }
                from_json_val(&acc)
            },
        );
        env.add_filter("flatten", |v: MValue| {
            let mut out = vec![];
            flatten(&to_json_val(&v), &mut out);
            from_json_val(&Value::Array(out))
        });
        env.add_filter("union", |a: MValue, b: MValue| {
            let (a, b) = (to_json_val(&a), to_json_val(&b));
            let mut out: Vec<Value> = a.as_array().cloned().unwrap_or_default();
            for x in b.as_array().cloned().unwrap_or_default() {
                if !out.contains(&x) {
                    out.push(x);
                }
            }
            from_json_val(&Value::Array(out))
        });
        env.add_filter("intersect", |a: MValue, b: MValue| {
            let (a, b) = (to_json_val(&a), to_json_val(&b));
            let bb = b.as_array().cloned().unwrap_or_default();
            let out: Vec<Value> = a
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|x| bb.contains(x))
                .collect();
            from_json_val(&Value::Array(out))
        });
        env.add_filter("difference", |a: MValue, b: MValue| {
            let (a, b) = (to_json_val(&a), to_json_val(&b));
            let bb = b.as_array().cloned().unwrap_or_default();
            let out: Vec<Value> = a
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|x| !bb.contains(x))
                .collect();
            from_json_val(&Value::Array(out))
        });
        env.add_filter("dict2items", |v: MValue, kw: Kwargs| {
            let kn: String = kw.get("key_name").unwrap_or_else(|_| "key".to_string());
            let vn: String = kw.get("value_name").unwrap_or_else(|_| "value".to_string());
            let j = to_json_val(&v);
            let items: Vec<Value> = j
                .as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, v)| {
                            let mut m = Map::new();
                            m.insert(kn.clone(), Value::String(k.clone()));
                            m.insert(vn.clone(), v.clone());
                            Value::Object(m)
                        })
                        .collect()
                })
                .unwrap_or_default();
            from_json_val(&Value::Array(items))
        });
        env.add_filter("items2dict", |v: MValue, kw: Kwargs| {
            let kn: String = kw.get("key_name").unwrap_or_else(|_| "key".to_string());
            let vn: String = kw.get("value_name").unwrap_or_else(|_| "value".to_string());
            let j = to_json_val(&v);
            let mut m = Map::new();
            for it in j.as_array().cloned().unwrap_or_default() {
                if let Some(k) = it.get(&kn).and_then(|k| k.as_str()) {
                    m.insert(k.to_string(), it.get(&vn).cloned().unwrap_or(Value::Null));
                }
            }
            from_json_val(&Value::Object(m))
        });
        env.add_filter("comment", |s: String, style: Option<String>, kw: Kwargs| {
            let style = style.unwrap_or_else(|| "plain".into());
            let (deco, begin, end) = match style.as_str() {
                "c" => ("// ".to_string(), "//".to_string(), "//".to_string()),
                "cblock" => (" * ".to_string(), "/*".to_string(), " */".to_string()),
                "erlang" => ("% ".to_string(), "%".to_string(), "%".to_string()),
                "xml" => ("    ".to_string(), "<!--".to_string(), "-->".to_string()),
                _ => ("# ".to_string(), "#".to_string(), "#".to_string()),
            };
            let deco: String = kw.get("decoration").unwrap_or(deco);
            let begin: String = kw.get("prefix").unwrap_or(begin);
            let end: String = kw.get("postfix").unwrap_or(end);
            let mut out = vec![begin];
            for l in s.lines() {
                out.push(format!("{deco}{l}").trim_end().to_string());
            }
            out.push(end);
            out.join("\n")
        });
        env.add_filter(
            "json_query",
            |v: MValue, q: String| -> Result<MValue, Error> {
                let expr = jmespath::compile(&q).map_err(|e| err(format!("json_query: {e}")))?;
                let data = jmespath::Variable::from_serializable(to_json_val(&v))
                    .map_err(|e| err(e.to_string()))?;
                let out = expr
                    .search(data)
                    .map_err(|e| err(format!("json_query: {e}")))?;
                let j: Value = serde_json::to_value(&*out).map_err(|e| err(e.to_string()))?;
                Ok(from_json_val(&j))
            },
        );
        env.add_filter(
            "hash",
            |s: String, algo: Option<String>| -> Result<String, Error> {
                use sha2::Digest;
                let b = s.as_bytes();
                Ok(match algo.as_deref().unwrap_or("sha1") {
                    "md5" => hex::encode(md5::Md5::digest(b)),
                    "sha1" => hex::encode(sha1::Sha1::digest(b)),
                    "sha256" => hex::encode(sha2::Sha256::digest(b)),
                    "sha512" => hex::encode(sha2::Sha512::digest(b)),
                    o => return Err(err(format!("unsupported hash type {o}"))),
                })
            },
        );
        env.add_filter("checksum", |s: String| {
            use sha2::Digest;
            hex::encode(sha1::Sha1::digest(s.as_bytes()))
        });
        env.add_filter("path_join", |v: MValue| {
            let j = to_json_val(&v);
            let parts: Vec<String> = match j {
                Value::Array(a) => a
                    .iter()
                    .map(|x| {
                        x.as_str()
                            .map(String::from)
                            .unwrap_or_else(|| x.to_string())
                    })
                    .collect(),
                o => vec![o.as_str().unwrap_or("").to_string()],
            };
            let mut out = String::new();
            for p in parts {
                if p.starts_with('/') {
                    out = p;
                } else if out.is_empty() || out.ends_with('/') {
                    out += &p;
                } else {
                    out = format!("{out}/{p}");
                }
            }
            out
        });
        env.add_filter("splitext", |s: String| {
            let base = s.rsplit('/').next().unwrap_or(&s);
            match base.rfind('.') {
                Some(i) if i > 0 => {
                    let cut = s.len() - (base.len() - i);
                    vec![s[..cut].to_string(), s[cut..].to_string()]
                }
                _ => vec![s.clone(), String::new()],
            }
        });
        env.add_filter("expanduser", |s: String| {
            match (s.strip_prefix("~/"), std::env::var("HOME")) {
                (Some(r), Ok(h)) => format!("{h}/{r}"),
                _ => s,
            }
        });
        env.add_filter("realpath", |s: String| {
            std::fs::canonicalize(&s)
                .map(|p| p.display().to_string())
                .unwrap_or(s)
        });
        env.add_filter("type_debug", |v: MValue| {
            match to_json_val(&v) {
                Value::Null => "NoneType",
                Value::Bool(_) => "bool",
                Value::Number(n) => {
                    if n.is_f64() {
                        "float"
                    } else {
                        "int"
                    }
                }
                Value::String(_) => "str",
                Value::Array(_) => "list",
                Value::Object(_) => "dict",
            }
            .to_string()
        });
        env.add_filter("human_readable", |n: f64, kw: Kwargs| {
            let unit: String = kw.get("unit").unwrap_or_else(|_| "B".into());
            let mut v = n;
            for p in ["", "K", "M", "G", "T", "P"] {
                if v.abs() < 1024.0 {
                    return format!("{v:.2} {p}{unit}");
                }
                v /= 1024.0;
            }
            format!("{v:.2} E{unit}")
        });
        env.add_filter("human_to_bytes", |s: String| -> Result<i64, Error> {
            let t = s.trim().to_uppercase();
            let (num, unit) = t.split_at(t.find(|c: char| c.is_alphabetic()).unwrap_or(t.len()));
            let n: f64 = num.trim().parse().map_err(|_| err("bad size"))?;
            let m = match unit.trim_end_matches('B') {
                "" => 1.0,
                "K" => 1024.0,
                "M" => 1024f64.powi(2),
                "G" => 1024f64.powi(3),
                "T" => 1024f64.powi(4),
                _ => return Err(err("bad unit")),
            };
            Ok((n * m) as i64)
        });
        env.add_filter("zip", |a: MValue, b: MValue| {
            let (a, b) = (to_json_val(&a), to_json_val(&b));
            let (a, b) = (
                a.as_array().cloned().unwrap_or_default(),
                b.as_array().cloned().unwrap_or_default(),
            );
            from_json_val(&Value::Array(
                a.into_iter()
                    .zip(b)
                    .map(|(x, y)| Value::Array(vec![x, y]))
                    .collect(),
            ))
        });
        env.add_filter("zip_longest", |a: MValue, b: MValue| {
            let (a, b) = (to_json_val(&a), to_json_val(&b));
            let (a, b) = (
                a.as_array().cloned().unwrap_or_default(),
                b.as_array().cloned().unwrap_or_default(),
            );
            let n = a.len().max(b.len());
            from_json_val(&Value::Array(
                (0..n)
                    .map(|i| {
                        Value::Array(vec![
                            a.get(i).cloned().unwrap_or(Value::Null),
                            b.get(i).cloned().unwrap_or(Value::Null),
                        ])
                    })
                    .collect(),
            ))
        });
        env.add_filter("product", |a: MValue, b: MValue| {
            let (a, b) = (to_json_val(&a), to_json_val(&b));
            let (a, b) = (
                a.as_array().cloned().unwrap_or_default(),
                b.as_array().cloned().unwrap_or_default(),
            );
            from_json_val(&Value::Array(
                a.iter()
                    .flat_map(|x| {
                        b.iter()
                            .map(move |y| Value::Array(vec![x.clone(), y.clone()]))
                    })
                    .collect(),
            ))
        });
        env.add_filter("symmetric_difference", |a: MValue, b: MValue| {
            let (a, b) = (to_json_val(&a), to_json_val(&b));
            let (a, b) = (
                a.as_array().cloned().unwrap_or_default(),
                b.as_array().cloned().unwrap_or_default(),
            );
            let mut out: Vec<Value> = a.iter().filter(|x| !b.contains(x)).cloned().collect();
            out.extend(b.iter().filter(|x| !a.contains(x)).cloned());
            from_json_val(&Value::Array(out))
        });
        env.add_filter("random", |v: MValue| {
            let j = to_json_val(&v);
            let n = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(0);
            match j {
                Value::Array(a) if !a.is_empty() => from_json_val(&a[(n as usize) % a.len()]),
                Value::Number(m) => MValue::from((n % m.as_u64().unwrap_or(1).max(1)) as i64),
                o => from_json_val(&o),
            }
        });
        env.add_filter("shuffle", |v: MValue| {
            let mut a = to_json_val(&v).as_array().cloned().unwrap_or_default();
            let mut s = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(1)
                | 1;
            for i in (1..a.len()).rev() {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                a.swap(i, (s as usize) % (i + 1));
            }
            from_json_val(&Value::Array(a))
        });
        env.add_filter("strftime", |fmt: String, ts: Option<f64>| {
            let t = ts.unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0)
            }) as i64;
            let (days, rem) = (t.div_euclid(86400), t.rem_euclid(86400));
            let z = days + 719468;
            let era = z.div_euclid(146097);
            let doe = z.rem_euclid(146097);
            let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
            let y0 = yoe + era * 400;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let m = if mp < 10 { mp + 3 } else { mp - 9 };
            let y = if m <= 2 { y0 + 1 } else { y0 };
            let (hh, mm, ss) = (rem / 3600, rem % 3600 / 60, rem % 60);
            fmt.replace("%Y", &format!("{y:04}"))
                .replace("%m", &format!("{m:02}"))
                .replace("%d", &format!("{d:02}"))
                .replace("%H", &format!("{hh:02}"))
                .replace("%M", &format!("{mm:02}"))
                .replace("%S", &format!("{ss:02}"))
                .replace("%s", &t.to_string())
                .replace("%F", &format!("{y:04}-{m:02}-{d:02}"))
                .replace("%T", &format!("{hh:02}:{mm:02}:{ss:02}"))
                .replace("%Z", "UTC")
        });
        env.add_filter("urlsplit", |u: String, part: Option<String>| -> MValue {
            let (scheme, rest) = u.split_once("://").map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((String::new(), u.clone()));
            let (netloc, path_q) = match rest.find('/') { Some(i) => (rest[..i].to_string(), rest[i..].to_string()), None => (rest.clone(), String::new()) };
            let (path, query) = path_q.split_once('?').map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((path_q.clone(), String::new()));
            let (userinfo, hostport) = match netloc.rsplit_once('@') {
                Some((u, h)) => (Some(u.to_string()), h.to_string()),
                None => (None, netloc.clone()),
            };
            let (username, password) = match userinfo.as_deref().map(|u| u.split_once(':')) {
                Some(Some((u, p))) => (Some(u.to_string()), Some(p.to_string())),
                Some(None) => (userinfo.clone(), None),
                None => (None, None),
            };
            let (host, port) = match hostport.rsplit_once(':') { Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h.to_string(), p.parse::<i64>().ok()), _ => (netloc.clone(), None) };
            let all = serde_json::json!({"scheme": scheme, "netloc": netloc, "hostname": host, "port": port, "username": username, "password": password, "path": path, "query": query, "fragment": ""});
            match part { Some(p) => from_json_val(all.get(&p).unwrap_or(&Value::Null)), None => from_json_val(&all) }
        });
        env.add_filter("urlencode", |s: String| {
            s.bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect::<String>()
        });
        env.add_filter(
            "extract",
            |key: MValue, container: MValue, rest: Option<MValue>| {
                let (k, c) = (to_json_val(&key), to_json_val(&container));
                let get = |c: &Value, k: &Value| match (c, k) {
                    (Value::Object(o), Value::String(s)) => o.get(s).cloned(),
                    (Value::Array(a), Value::Number(n)) => {
                        a.get(n.as_u64().unwrap_or(0) as usize).cloned()
                    }
                    _ => None,
                };
                let mut cur = get(&c, &k).unwrap_or(Value::Null);
                if let Some(r) = rest {
                    for p in match to_json_val(&r) {
                        Value::Array(a) => a,
                        o => vec![o],
                    } {
                        cur = get(&cur, &p).unwrap_or(Value::Null);
                    }
                }
                from_json_val(&cur)
            },
        );
        env.add_filter("dictsort", |v: MValue, kw: Kwargs| {
            let by_value: bool = kw
                .get::<String>("by")
                .map(|b| b == "value")
                .unwrap_or(false);
            let rev: bool = kw.get("reverse").unwrap_or(false);
            let mut items: Vec<(String, Value)> = to_json_val(&v)
                .as_object()
                .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            items.sort_by(|a, b| {
                if by_value {
                    a.1.to_string().cmp(&b.1.to_string())
                } else {
                    a.0.to_lowercase().cmp(&b.0.to_lowercase())
                }
            });
            if rev {
                items.reverse();
            }
            from_json_val(&Value::Array(
                items
                    .into_iter()
                    .map(|(k, v)| Value::Array(vec![Value::String(k), v]))
                    .collect(),
            ))
        });
        env.add_filter("wordwrap", |s: String, width: Option<usize>, kw: Kwargs| {
            let width = width.or_else(|| kw.get("width").ok()).unwrap_or(79).max(1);
            let wrapstring: String = kw.get("wrapstring").unwrap_or_else(|_| "\n".to_string());
            let break_long: bool = kw.get("break_long_words").unwrap_or(true);
            let mut out_lines: Vec<String> = vec![];
            for para in s.split('\n') {
                let mut line = String::new();
                for word in para.split_whitespace() {
                    let mut w = word.to_string();
                    while break_long && w.chars().count() > width {
                        if !line.is_empty() {
                            out_lines.push(std::mem::take(&mut line));
                        }
                        let head: String = w.chars().take(width).collect();
                        w = w.chars().skip(width).collect();
                        out_lines.push(head);
                    }
                    if line.is_empty() {
                        line = w;
                    } else if line.chars().count() + 1 + w.chars().count() <= width {
                        line.push(' ');
                        line.push_str(&w);
                    } else {
                        out_lines.push(std::mem::replace(&mut line, w));
                    }
                }
                out_lines.push(line);
            }
            out_lines.join(&wrapstring)
        });
        env.add_filter("center", |s: String, width: Option<usize>| {
            let width = width.unwrap_or(80);
            let len = s.chars().count();
            if width <= len {
                return s;
            }
            let pad = width - len;
            let left = pad / 2 + (pad & width & 1);
            format!("{}{}{}", " ".repeat(left), s, " ".repeat(pad - left))
        });
        env.add_filter("split", |s: String, sep: Option<String>| -> Vec<String> {
            match sep {
                Some(p) => s.split(p.as_str()).map(String::from).collect(),
                None => s.split_whitespace().map(String::from).collect(),
            }
        });

        // tests
        env.add_test("failed", |v: MValue| {
            v.get_attr("failed").map(|x| x.is_true()).unwrap_or(false)
        });
        env.add_test("failure", |v: MValue| {
            v.get_attr("failed").map(|x| x.is_true()).unwrap_or(false)
        });
        env.add_test("succeeded", |v: MValue| {
            !v.get_attr("failed").map(|x| x.is_true()).unwrap_or(false)
        });
        env.add_test("success", |v: MValue| {
            !v.get_attr("failed").map(|x| x.is_true()).unwrap_or(false)
        });
        env.add_test("changed", |v: MValue| {
            v.get_attr("changed").map(|x| x.is_true()).unwrap_or(false)
        });
        env.add_test("skipped", |v: MValue| {
            v.get_attr("skipped").map(|x| x.is_true()).unwrap_or(false)
        });
        env.add_test("match", |s: String, p: String| {
            regex::Regex::new(&format!("^(?:{p})"))
                .map(|r| r.is_match(&s))
                .unwrap_or(false)
        });
        env.add_test("search", |s: String, p: String| {
            regex::Regex::new(&p)
                .map(|r| r.is_match(&s))
                .unwrap_or(false)
        });
        env.add_test("regex", |s: String, p: String| {
            regex::Regex::new(&p)
                .map(|r| r.is_match(&s))
                .unwrap_or(false)
        });
        env.add_test(
            "version_compare",
            |a: MValue, b: MValue, op: Option<String>| {
                let o = version_cmp(&a.to_string(), &b.to_string());
                use std::cmp::Ordering::*;
                match op.as_deref().unwrap_or("==") {
                    "<" | "lt" => o == Less,
                    "<=" | "le" => o != Greater,
                    ">" | "gt" => o == Greater,
                    ">=" | "ge" => o != Less,
                    "!=" | "ne" => o != Equal,
                    _ => o == Equal,
                }
            },
        );
        env.add_test("version", |a: MValue, b: MValue, op: Option<String>| {
            let o = version_cmp(&a.to_string(), &b.to_string());
            use std::cmp::Ordering::*;
            match op.as_deref().unwrap_or("==") {
                "<" | "lt" => o == Less,
                "<=" | "le" => o != Greater,
                ">" | "gt" => o == Greater,
                ">=" | "ge" => o != Less,
                "!=" | "ne" => o != Equal,
                _ => o == Equal,
            }
        });
        env.add_test("truthy", |v: MValue| v.is_true());
        env.add_test("falsy", |v: MValue| !v.is_true());
        env.add_test("file", |s: String| std::path::Path::new(&s).is_file());
        env.add_test("directory", |s: String| std::path::Path::new(&s).is_dir());

        // functions
        fn do_lookup(
            state: &minijinja::State,
            kind: &str,
            args: &[MValue],
            kw: &Kwargs,
        ) -> Result<Vec<Value>, Error> {
            let json_args: Vec<Value> = args.iter().map(to_json_val).collect();
            let mut kwm = Map::new();
            for k in kw.args() {
                if let Ok(v) = kw.get::<MValue>(k) {
                    kwm.insert(k.to_string(), to_json_val(&v));
                }
            }
            with_vars(|vars| {
                let render = |text: &str| {
                    state
                        .env()
                        .render_str(text, MValue::from(minijinja::value::Serde(vars)))
                        .map_err(|e| e.to_string())
                };
                crate::lookup::run(
                    kind,
                    &json_args,
                    &kwm,
                    &crate::lookup::Ctx {
                        vars,
                        render: &render,
                    },
                )
                .map_err(err)
            })
        }
        env.add_function(
            "lookup",
            |state: &minijinja::State,
             kind: String,
             args: minijinja::value::Rest<MValue>,
             kw: Kwargs|
             -> Result<MValue, Error> {
                let wantlist: bool = kw.get("wantlist").unwrap_or(false);
                let terms = do_lookup(state, &kind, &args, &kw)?;
                Ok(from_json_val(&crate::lookup::lookup_value(terms, wantlist)))
            },
        );
        env.add_function(
            "query",
            |state: &minijinja::State,
             kind: String,
             args: minijinja::value::Rest<MValue>,
             kw: Kwargs|
             -> Result<MValue, Error> {
                Ok(from_json_val(&Value::Array(do_lookup(
                    state, &kind, &args, &kw,
                )?)))
            },
        );
        env.add_function(
            "q",
            |state: &minijinja::State,
             kind: String,
             args: minijinja::value::Rest<MValue>,
             kw: Kwargs|
             -> Result<MValue, Error> {
                Ok(from_json_val(&Value::Array(do_lookup(
                    state, &kind, &args, &kw,
                )?)))
            },
        );
        env.add_function("now", || {
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            MValue::from(t)
        });
        Templar { env }
    }

    fn ctx(vars: &Vars) -> MValue {
        MValue::from(minijinja::value::Serde(vars))
    }

    /// `ansible.builtin.default` -> `default` (also `community.general.` / `ansible.utils.` ...).
    fn normalize(s: &str) -> std::borrow::Cow<'_, str> {
        const PREFIXES: [&str; 5] = [
            "ansible.builtin.",
            "ansible.utils.",
            "ansible.netcommon.",
            "community.general.",
            "community.crypto.",
        ];
        if !PREFIXES.iter().any(|p| s.contains(p)) {
            return std::borrow::Cow::Borrowed(s);
        }
        let mut out = s.to_string();
        for p in PREFIXES {
            out = out.replace(p, "");
        }
        std::borrow::Cow::Owned(out)
    }

    fn is_templated(s: &str) -> bool {
        s.contains("{{") || s.contains("{%") || s.contains("{#")
    }

    /// Template a string. A string that is a single `{{ expr }}` keeps its native type.
    pub fn render_str(&self, s: &str, vars: &Vars) -> Result<Value> {
        if !Self::is_templated(s) {
            return Ok(Value::String(s.to_string()));
        }
        let norm = Self::normalize(s);
        let s: &str = &norm;
        let _guard = VarsGuard::new(vars);
        let t = s;
        // a string that is exactly one `{{ expr }}` keeps the expression's native type
        let single = t.starts_with("{{") && t.ends_with("}}") && t.matches("{{").count() == 1 && {
            let inner = t[2..t.len() - 2]
                .trim_start_matches('-')
                .trim_end_matches('-');
            !inner.contains("}}") || self.env.compile_expression(inner).is_ok()
        };
        if single {
            let inner = &t[2..t.len() - 2];
            let inner = inner.trim_start_matches('-').trim_end_matches('-');
            let e = self
                .env
                .compile_expression(inner)
                .map_err(|e| anyhow!("template error in '{s}': {e}"))?;
            let v = e
                .eval(Self::ctx(vars))
                .map_err(|e| anyhow!("template error in '{s}': {e}"))?;
            if v.is_undefined() {
                return Err(anyhow!(
                    "'{}' is undefined (in template '{s}')",
                    inner.trim()
                ));
            }
            return Ok(to_json_val(&v));
        }
        let out = self
            .env
            .render_str(s, Self::ctx(vars))
            .map_err(|e| anyhow!("template error in '{s}': {e}"))?;
        Ok(Value::String(out))
    }

    pub fn render_string(&self, s: &str, vars: &Vars) -> Result<String> {
        match self.render_str(s, vars)? {
            Value::String(s) => Ok(s),
            Value::Null => Ok(String::new()),
            other => Ok(other.to_string()),
        }
    }

    /// Always renders to text (used for template files).
    pub fn render_text(&self, s: &str, vars: &Vars) -> Result<String> {
        let _guard = VarsGuard::new(vars);
        self.env
            .render_str(s, Self::ctx(vars))
            .map_err(|e| anyhow!("template error: {e:#}"))
    }

    pub fn render_value(&self, v: &Value, vars: &Vars) -> Result<Value> {
        Ok(match v {
            Value::String(s) => self.render_str(s, vars)?,
            Value::Array(a) => Value::Array(
                a.iter()
                    .map(|x| self.render_value(x, vars))
                    .collect::<Result<_>>()?,
            ),
            Value::Object(o) => {
                let mut m = Map::new();
                for (k, x) in o {
                    m.insert(self.render_string(k, vars)?, self.render_value(x, vars)?);
                }
                Value::Object(m)
            }
            other => other.clone(),
        })
    }

    pub fn eval_bool(&self, expr: &str, vars: &Vars) -> Result<bool> {
        let expr = if Self::is_templated(expr) {
            self.render_string(expr, vars)?
        } else {
            expr.to_string()
        };
        let _guard = VarsGuard::new(vars);
        let e = self
            .env
            .compile_expression(expr.trim())
            .map_err(|e| anyhow!("bad conditional '{expr}': {e}"))?;
        let v = e
            .eval(Self::ctx(vars))
            .map_err(|e| anyhow!("error evaluating conditional '{expr}': {e}"))?;
        if v.is_undefined() {
            return Err(anyhow!(
                "conditional '{expr}' references an undefined variable"
            ));
        }
        Ok(match to_json_val(&v) {
            Value::String(s) => parse_bool_str(&s).unwrap_or(!s.is_empty()),
            other => truthy(&other),
        })
    }
}

pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./=:,@+%".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}
