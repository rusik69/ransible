//! Built-in modules.
mod command;
mod control;
mod files;
mod net;
mod pkg;
mod system;
#[allow(unused_imports)]
pub(crate) use command::*;
#[allow(unused_imports)]
pub(crate) use control::*;
#[allow(unused_imports)]
pub(crate) use files::*;
#[allow(unused_imports)]
pub(crate) use net::*;
#[allow(unused_imports)]
pub(crate) use pkg::*;
#[allow(unused_imports)]
pub(crate) use system::*;

use crate::conn::Conn;
use crate::playbook::loose_bool;
use crate::template::{Templar, Vars, shell_quote as q};
use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

pub const OMIT: &str = "__omit_place_holder__";

pub fn strip_omit(v: Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(
            o.into_iter()
                .filter(|(_, x)| x.as_str() != Some(OMIT))
                .map(|(k, x)| (k, strip_omit(x)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(
            a.into_iter()
                .filter(|x| x.as_str() != Some(OMIT))
                .map(strip_omit)
                .collect(),
        ),
        o => o,
    }
}

pub type Res = Map<String, Value>;
pub type Args = Map<String, Value>;

pub struct Ctx<'a> {
    pub host: &'a str,
    pub conn: &'a Conn,
    pub vars: &'a Vars,
    pub templar: &'a Templar,
    pub check: bool,
    pub diff: bool,
    pub dir: &'a Path,
    pub play_dir: &'a Path,
}

pub(crate) fn st(a: &Args, k: &str) -> Option<String> {
    a.get(k).and_then(|v| match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    })
}
pub(crate) fn st_any(a: &Args, ks: &[&str]) -> Option<String> {
    ks.iter().find_map(|k| st(a, k))
}
pub(crate) fn bo(a: &Args, k: &str, d: bool) -> bool {
    a.get(k).and_then(loose_bool).unwrap_or(d)
}
pub(crate) fn list(a: &Args, k: &str) -> Vec<String> {
    match a.get(k) {
        Some(Value::Array(v)) => v
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
pub(crate) fn res(changed: bool) -> Res {
    let mut r = Map::new();
    r.insert("changed".into(), changed.into());
    r
}
pub(crate) fn fail(msg: impl Into<String>) -> Res {
    let mut r = res(false);
    r.insert("failed".into(), true.into());
    r.insert("msg".into(), Value::String(msg.into()));
    r
}
pub(crate) fn lines(s: &str) -> Value {
    Value::Array(s.lines().map(|l| Value::String(l.to_string())).collect())
}

pub fn find_src(ctx: &Ctx, kind: &str, name: &str) -> Result<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() {
        return Ok(p.to_path_buf());
    }
    let cands = [
        ctx.dir.join(kind).join(name),
        ctx.dir.join(name),
        ctx.play_dir.join(kind).join(name),
        ctx.play_dir.join(name),
        PathBuf::from(name),
    ];
    cands.iter().find(|c| c.exists()).cloned().ok_or_else(|| {
        anyhow!(
            "could not find or access '{name}' (looked in {})",
            ctx.dir.join(kind).display()
        )
    })
}

pub fn run(module: &str, a: &Args, ctx: &Ctx) -> Result<Res> {
    // module arguments use underscores; hyphenated spellings (e.g. `daemon-reload`) are aliases
    let normalized;
    let a = if a.keys().any(|k| k.contains('-')) {
        normalized = a
            .iter()
            .map(|(k, v)| (k.replace('-', "_"), v.clone()))
            .collect::<Args>();
        &normalized
    } else {
        a
    };
    match module {
        "command" | "shell" | "raw" => command(module, a, ctx),
        "script" => script(a, ctx),
        "copy" => copy(a, ctx, false),
        "template" => copy(a, ctx, true),
        "file" => file(a, ctx),
        "lineinfile" => lineinfile(a, ctx),
        "blockinfile" => blockinfile(a, ctx),
        "replace" => replace(a, ctx),
        "stat" => stat(a, ctx),
        "slurp" => slurp(a, ctx),
        "debug" => debug(a, ctx),
        "set_fact" => {
            let mut facts = Map::new();
            for (k, v) in a {
                if k != "cacheable" {
                    facts.insert(k.clone(), v.clone());
                }
            }
            let mut r = res(false);
            r.insert("ansible_facts".into(), Value::Object(facts));
            Ok(r)
        }
        "include_vars" => include_vars(a, ctx),
        "fail" => Ok(fail(
            st(a, "msg").unwrap_or_else(|| "Failed as requested from task".into()),
        )),
        "assert" => assert_mod(a, ctx),
        "ping" => {
            let mut r = res(false);
            r.insert(
                "ping".into(),
                st(a, "data").unwrap_or_else(|| "pong".into()).into(),
            );
            Ok(r)
        }
        "pause" => {
            let secs = a.get("seconds").and_then(|v| v.as_f64()).unwrap_or(0.0)
                + a.get("minutes").and_then(|v| v.as_f64()).unwrap_or(0.0) * 60.0;
            if !ctx.check {
                std::thread::sleep(std::time::Duration::from_secs_f64(secs));
            }
            Ok(res(false))
        }
        "wait_for" => wait_for(a, ctx),
        "wait_for_connection" => {
            for _ in 0..60 {
                if ctx.conn.exec("true").map(|o| o.rc == 0).unwrap_or(false) {
                    return Ok(res(false));
                }
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
            Ok(fail("timeout waiting for connection"))
        }
        "meta" | "gather_facts" | "noop" => Ok(res(false)),
        "setup" => setup(ctx),
        "apt" | "yum" | "dnf" | "package" | "apk" | "homebrew" | "brew" => package(module, a, ctx),
        "service" | "systemd" | "systemd_service" => service(a, ctx),
        "user" => user(a, ctx),
        "group" => group(a, ctx),
        "git" => git(a, ctx),
        "pip" => pip(a, ctx),
        "add_host" => {
            let name = st_any(a, &["name", "hostname"])
                .ok_or_else(|| anyhow!("add_host requires name"))?;
            let groups = if a.contains_key("groups") {
                list(a, "groups")
            } else {
                list(a, "group")
            };
            let vars: Map<String, Value> = a
                .iter()
                .filter(|(k, _)| !["name", "hostname", "groups", "group"].contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let mut r = res(true);
            r.insert(
                "add_host".into(),
                json!({"host_name": name, "groups": groups, "host_vars": vars}),
            );
            Ok(r)
        }
        "group_by" => {
            let key = st(a, "key").ok_or_else(|| anyhow!("group_by requires key"))?;
            let mut r = res(true);
            r.insert("group_by".into(), key.replace(' ', "_").into());
            r.insert("changed".into(), true.into());
            Ok(r)
        }
        "ini_file" | "sysctl" | "known_hosts" | "reboot" | "set_stats" | "assemble" | "archive"
        | "npm" | "gem" | "debconf" | "ufw" | "yum_repository" | "alternatives" | "mount"
        | "snap" | "firewalld" | "rpm_key" | "modprobe" | "selinux" | "git_config"
        | "lineinfile_multi" => crate::modules_extra::run(module, a, ctx),
        "async_status" => async_status(a, ctx),
        "mysql_db" => crate::mysql::mysql_db(a, ctx),
        "mysql_user" => crate::mysql::mysql_user(a, ctx),
        "mysql_query" => crate::mysql::mysql_query(a, ctx),
        "mysql_variables" => crate::mysql::mysql_variables(a, ctx),
        "find" => find(a, ctx),
        "locale_gen" => locale_gen(a, ctx),
        "deb822_repository" => deb822_repository(a, ctx),
        "apt_repository" => apt_repository(a, ctx),
        "apt_key" => apt_key(a, ctx),
        "service_facts" => service_facts(ctx),
        "package_facts" => package_facts(ctx),
        "getent" => getent(a, ctx),
        "timezone" => timezone(a, ctx),
        "authorized_key" => authorized_key(a, ctx),
        "cron" => cron(a, ctx),
        "unarchive" => unarchive(a, ctx),
        "hostname" => hostname(a, ctx),
        "tempfile" => tempfile(a, ctx),
        "get_url" => get_url(a, ctx),
        "uri" => uri(a, ctx),
        "fetch" => fetch(a, ctx),
        "synchronize" => synchronize(a, ctx),
        other => bail!("unsupported module '{other}'"),
    }
}

// ---------- command family ----------

pub(crate) fn split_args(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut q: Option<char> = None;
    let mut has = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match (q, c) {
            (Some(qc), c) if c == qc => q = None,
            (Some('"'), '\\') if matches!(chars.peek(), Some('"') | Some('\\')) => {
                cur.push(chars.next().unwrap())
            }
            (Some(_), c) => cur.push(c),
            (None, '"') | (None, '\'') => {
                q = Some(c);
                has = true
            }
            (None, '\\') if chars.peek().is_some() => {
                cur.push(chars.next().unwrap());
                has = true
            }
            (None, c) if c.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::{Conn, Output};
    use std::sync::{Arc, Mutex};

    fn out(rc: i32, so: &str) -> Output {
        Output {
            rc,
            stdout: so.into(),
            stderr: String::new(),
        }
    }

    type Seen = Vec<(String, Option<Vec<u8>>)>;

    /// Run a module against a mock connection; returns the result and every (command, stdin) seen.
    fn run_mock(module: &str, args: Value, files: &[(&str, &str)], check: bool) -> (Res, Seen) {
        let log: Arc<Mutex<Seen>> = Default::default();
        let l2 = log.clone();
        let files: Vec<(String, String)> = files
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let conn = Conn::mock(move |cmd, stdin| {
            l2.lock()
                .unwrap()
                .push((cmd.to_string(), stdin.map(|s| s.to_vec())));
            for (p, c) in &files {
                if cmd.contains(&format!("test -e '{p}'")) || cmd.contains(&format!("test -e {p}"))
                {
                    return out(0, c);
                }
            }
            if cmd.starts_with("test -e") || cmd.starts_with("test -d") || cmd.contains("test -e") {
                return out(1, "");
            }
            out(0, "")
        });
        let templar = Templar::new();
        let vars = Vars::new();
        let dir = std::path::Path::new(".");
        let ctx = Ctx {
            host: "h",
            conn: &conn,
            vars: &vars,
            templar: &templar,
            check,
            diff: false,
            dir,
            play_dir: dir,
        };
        let a = args.as_object().unwrap().clone();
        let r = run(module, &a, &ctx).unwrap();
        let seen = log.lock().unwrap().clone();
        (r, seen)
    }

    fn wrote(seen: &[(String, Option<Vec<u8>>)]) -> Option<String> {
        seen.iter()
            .find(|(c, s)| c.starts_with("cat >") && s.is_some())
            .map(|(_, s)| String::from_utf8_lossy(s.as_ref().unwrap()).into_owned())
    }

    #[test]
    fn copy_content_creates_file_and_reports_changed() {
        let (r, seen) = run_mock(
            "copy",
            json!({"content": "hello\n", "dest": "/tmp/x"}),
            &[],
            false,
        );
        assert_eq!(r["changed"], true);
        assert_eq!(wrote(&seen).as_deref(), Some("hello\n"));
    }

    #[test]
    fn copy_is_idempotent_when_content_matches() {
        let (r, seen) = run_mock(
            "copy",
            json!({"content": "same\n", "dest": "/tmp/x"}),
            &[("/tmp/x", "same\n")],
            false,
        );
        assert_eq!(r["changed"], false);
        assert!(wrote(&seen).is_none());
    }

    #[test]
    fn check_mode_never_writes() {
        let (r, seen) = run_mock(
            "copy",
            json!({"content": "new\n", "dest": "/tmp/x"}),
            &[("/tmp/x", "old\n")],
            true,
        );
        assert_eq!(r["changed"], true);
        assert!(wrote(&seen).is_none());
    }

    #[test]
    fn lineinfile_replaces_matching_line() {
        let (r, seen) = run_mock(
            "lineinfile",
            json!({"path": "/etc/conf", "regexp": "^port=", "line": "port=81"}),
            &[("/etc/conf", "a=1\nport=80\nb=2\n")],
            false,
        );
        assert_eq!(r["changed"], true);
        assert_eq!(wrote(&seen).as_deref(), Some("a=1\nport=81\nb=2\n"));
    }

    #[test]
    fn lineinfile_noop_when_line_present() {
        let (r, _) = run_mock(
            "lineinfile",
            json!({"path": "/etc/conf", "line": "a=1"}),
            &[("/etc/conf", "a=1\n")],
            false,
        );
        assert_eq!(r["changed"], false);
    }

    #[test]
    fn unsupported_module_errors() {
        let conn = Conn::mock(|_, _| out(0, ""));
        let templar = Templar::new();
        let vars = Vars::new();
        let dir = std::path::Path::new(".");
        let ctx = Ctx {
            host: "h",
            conn: &conn,
            vars: &vars,
            templar: &templar,
            check: false,
            diff: false,
            dir,
            play_dir: dir,
        };
        assert!(run("definitely_not_a_module", &Map::new(), &ctx).is_err());
    }

    #[test]
    fn command_creates_skips_when_path_exists() {
        let (r, seen) = run_mock(
            "command",
            json!({"_raw_params": "echo hi", "creates": "/tmp/marker"}),
            &[("/tmp/marker", "")],
            false,
        );
        assert_eq!(r["skipped"], true);
        assert!(!seen.iter().any(|(c, _)| c.contains("echo hi")));
    }
}
