//! Built-in modules.
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

fn out_to_res(
    o: crate::conn::Output,
    cmd: &str,
    started: std::time::SystemTime,
    as_list: bool,
) -> Res {
    let mut r = res(true);
    let fmt = |t: std::time::SystemTime| {
        let secs = t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0) as i64;
        let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
        // civil-from-days (Howard Hinnant)
        let z = days + 719468;
        let era = z.div_euclid(146097);
        let doe = z.rem_euclid(146097);
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!(
            "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:06}",
            rem / 3600,
            rem % 3600 / 60,
            rem % 60,
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_micros())
                .unwrap_or(0)
        )
    };
    let end = std::time::SystemTime::now();
    let dur = end.duration_since(started).unwrap_or_default();
    r.insert("start".into(), fmt(started).into());
    r.insert("end".into(), fmt(end).into());
    r.insert(
        "delta".into(),
        format!("0:00:{:02}.{:06}", dur.as_secs() % 60, dur.subsec_micros()).into(),
    );
    if as_list {
        r.insert(
            "cmd".into(),
            Value::Array(split_args(cmd).into_iter().map(Value::from).collect()),
        );
    } else {
        r.insert("cmd".into(), cmd.into());
    }
    r.insert("rc".into(), o.rc.into());
    r.insert("stdout".into(), o.stdout.trim_end_matches('\n').into());
    r.insert("stderr".into(), o.stderr.trim_end_matches('\n').into());
    r.insert("stdout_lines".into(), lines(&o.stdout));
    r.insert("stderr_lines".into(), lines(&o.stderr));
    if o.rc != 0 {
        r.insert("failed".into(), true.into());
        r.insert("msg".into(), "non-zero return code".into());
        if as_list {
            r.insert(
                "msg".into(),
                "The command exited with a non-zero return code.".into(),
            );
        }
    }
    r
}

fn command(module: &str, a: &Args, ctx: &Ctx) -> Result<Res> {
    let raw = st_any(a, &["_raw_params", "cmd"]);
    let cmdline = if let Some(Value::Array(argv)) = a.get("argv") {
        argv.iter()
            .map(|x| q(x.as_str().unwrap_or("")))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        let raw = raw.ok_or_else(|| anyhow!("no command given"))?;
        match module {
            "command" => split_args(&raw)
                .iter()
                .map(|x| q(x))
                .collect::<Vec<_>>()
                .join(" "),
            _ => raw,
        }
    };
    if module != "raw" {
        if let Some(c) = st(a, "creates")
            && ctx.conn.exec(&format!("test -e {}", q(&c)))?.rc == 0
        {
            let mut r = res(false);
            r.insert("skipped".into(), true.into());
            r.insert("msg".into(), format!("skipped, since {c} exists").into());
            return Ok(r);
        }
        if let Some(c) = st(a, "removes")
            && ctx.conn.exec(&format!("test -e {}", q(&c)))?.rc != 0
        {
            let mut r = res(false);
            r.insert("skipped".into(), true.into());
            r.insert(
                "msg".into(),
                format!("skipped, since {c} does not exist").into(),
            );
            return Ok(r);
        }
    }
    if ctx.check {
        let mut r = res(false);
        r.insert("skipped".into(), true.into());
        r.insert("msg".into(), "skipped in check mode".into());
        return Ok(r);
    }
    let mut full = match st(a, "chdir") {
        Some(d) => format!("cd {} && {cmdline}", q(&d)),
        None => cmdline.clone(),
    };
    if module == "shell"
        && let Some(e) = st(a, "executable")
    {
        full = format!("{} -c {}", q(&e), q(&full));
    }
    let started = std::time::SystemTime::now();
    let o = match st(a, "stdin") {
        Some(s) => ctx.conn.exec_stdin(&full, s.as_bytes())?,
        None => ctx.conn.exec(&full)?,
    };
    Ok(out_to_res(o, &cmdline, started, module == "command"))
}

fn script(a: &Args, ctx: &Ctx) -> Result<Res> {
    let raw = st_any(a, &["_raw_params", "cmd"]).ok_or_else(|| anyhow!("script: no path given"))?;
    let mut parts = split_args(&raw);
    let file = parts.remove(0);
    let path = find_src(ctx, "files", &file)?;
    let data = std::fs::read(&path)?;
    if ctx.check {
        return Ok(res(false));
    }
    let interp = std::str::from_utf8(&data)
        .ok()
        .and_then(|t| t.lines().next())
        .and_then(|l| l.strip_prefix("#!"))
        .map(|l| l.trim().to_string())
        .unwrap_or_else(|| "/bin/sh".into());
    let args = parts.iter().map(|x| q(x)).collect::<Vec<_>>().join(" ");
    let o = ctx.conn.exec_stdin(
        &format!(
            "{} {}",
            if interp.contains("sh") {
                format!("{interp} -s --")
            } else {
                format!("{interp} -")
            },
            args
        ),
        &data,
    )?;
    Ok(out_to_res(o, &raw, std::time::SystemTime::now(), false))
}

// ---------- files ----------

fn mode_s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn attrs(conn: &Conn, path: &str) -> Option<(String, String, String)> {
    let o = conn
        .exec(&format!(
            "stat -L -c '%a %U %G' {p} 2>/dev/null || stat -L -f '%Lp %Su %Sg' {p}",
            p = q(path)
        ))
        .ok()?;
    if o.rc != 0 {
        return None;
    }
    let mut it = o.stdout.split_whitespace();
    Some((
        it.next()?.to_string(),
        it.next()?.to_string(),
        it.next()?.to_string(),
    ))
}

/// Apply mode/owner/group; returns whether something changed.
pub(crate) fn apply_attrs(a: &Args, ctx: &Ctx, path: &str) -> Result<bool> {
    let mode = a.get("mode").filter(|v| !v.is_null()).map(mode_s);
    let owner = st(a, "owner");
    let grp = st(a, "group");
    if mode.is_none() && owner.is_none() && grp.is_none() {
        return Ok(false);
    }
    let cur = attrs(ctx.conn, path);
    let mut changed = false;
    let rec = if bo(a, "recurse", false) { "-R " } else { "" };
    if let Some(m) = mode {
        let numeric = m.chars().all(|c| c.is_ascii_digit());
        let differs = match (&cur, numeric) {
            (Some((cm, _, _)), true) => cm.trim_start_matches('0') != m.trim_start_matches('0'),
            _ => false,
        };
        if differs || (cur.is_none() && !ctx.check) {
            changed = true;
            if !ctx.check {
                let o = ctx
                    .conn
                    .exec(&format!("chmod {rec}{} {}", q(&m), q(path)))?;
                if o.rc != 0 {
                    bail!("chmod failed: {}", o.stderr.trim());
                }
            }
        } else if !numeric && !ctx.check {
            ctx.conn
                .exec(&format!("chmod {rec}{} {}", q(&m), q(path)))?;
        }
    }
    for (kind, want, idx) in [("owner", owner, 1), ("group", grp, 2)] {
        if let Some(w) = want {
            let have = cur
                .as_ref()
                .map(|c| if idx == 1 { c.1.clone() } else { c.2.clone() });
            if have.as_deref() != Some(w.as_str()) {
                changed = true;
                if !ctx.check {
                    let cmd = if kind == "owner" {
                        format!("chown {rec}{} {}", q(&w), q(path))
                    } else {
                        format!("chgrp {rec}{} {}", q(&w), q(path))
                    };
                    let o = ctx.conn.exec(&cmd)?;
                    if o.rc != 0 {
                        bail!("{cmd} failed: {}", o.stderr.trim());
                    }
                }
            }
        }
    }
    Ok(changed)
}

pub(crate) fn dir_of(p: &str) -> String {
    match p.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => p[..i].into(),
        None => ".".into(),
    }
}

pub(crate) fn unified_diff(old: &str, new: &str, path: &str) -> String {
    let mut out = format!("--- before: {path}\n+++ after: {path}\n");
    let (o, n): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let (mut i, mut j) = (0, 0);
    while i < o.len() || j < n.len() {
        if i < o.len() && j < n.len() && o[i] == n[j] {
            i += 1;
            j += 1;
        } else if let Some(k) = n[j.min(n.len())..]
            .iter()
            .position(|x| i < o.len() && *x == o[i])
            .filter(|k| *k < 50)
        {
            for x in &n[j..j + k] {
                out.push_str(&format!("+{x}\n"));
            }
            j += k;
        } else {
            if i < o.len() {
                out.push_str(&format!("-{}\n", o[i]));
                i += 1;
            }
            if j < n.len() && !(i < o.len() && n[j] == o[i]) {
                out.push_str(&format!("+{}\n", n[j]));
                j += 1;
            }
        }
    }
    out
}

/// Write content to dest if different. Returns result.
/// Run the `validate` command (with %s = temp file) before installing content.
fn validate_content(a: &Args, ctx: &Ctx, data: &[u8]) -> Result<Option<Res>> {
    let Some(v) = st(a, "validate") else {
        return Ok(None);
    };
    if !v.contains("%s") {
        return Ok(Some(fail("validate must contain %s")));
    }
    let tmp = ctx
        .conn
        .exec("mktemp /tmp/ransible-validate.XXXXXX")?
        .stdout
        .trim()
        .to_string();
    ctx.conn.put(&tmp, data, None)?;
    let o = ctx.conn.exec(&v.replace("%s", &q(&tmp)))?;
    let _ = ctx.conn.exec(&format!("rm -f {}", q(&tmp)));
    if o.rc != 0 {
        let mut r = fail(format!(
            "failed to validate: rc:{} error:{}",
            o.rc,
            o.stderr.trim()
        ));
        r.insert("exit_status".into(), o.rc.into());
        return Ok(Some(r));
    }
    Ok(None)
}

pub(crate) fn put_content(a: &Args, ctx: &Ctx, dest: &str, data: &[u8]) -> Result<Res> {
    let cur = ctx.conn.get_bytes(dest)?;
    let mut changed = cur.as_deref() != Some(data);
    if !bo(a, "force", true) && cur.is_some() {
        changed = false;
    }
    let mut r = res(false);
    if changed {
        if ctx.diff {
            let old = cur
                .as_ref()
                .map(|c| String::from_utf8_lossy(c).into_owned())
                .unwrap_or_default();
            r.insert(
                "diff".into(),
                unified_diff(&old, &String::from_utf8_lossy(data), dest).into(),
            );
        }
        if !ctx.check {
            if bo(a, "backup", false) && cur.is_some() {
                ctx.conn
                    .exec(&format!("cp -p {p} {p}.ransible.bak", p = q(dest)))?;
            }
            let o = ctx.conn.exec(&format!("mkdir -p {}", q(&dir_of(dest))))?;
            if o.rc != 0 {
                bail!(
                    "could not create destination directory: {}",
                    o.stderr.trim()
                );
            }
            if let Some(f) = validate_content(a, ctx, data)? {
                return Ok(f);
            }
            ctx.conn.put(dest, data, None)?;
        }
    }
    let ac = apply_attrs(a, ctx, dest)?;
    r.insert("changed".into(), (changed || ac).into());
    r.insert("dest".into(), dest.into());
    Ok(r)
}

fn copy(a: &Args, ctx: &Ctx, templ: bool) -> Result<Res> {
    let dest =
        st_any(a, &["dest", "path"]).ok_or_else(|| anyhow!("missing required argument: dest"))?;
    let data: Vec<u8>;
    let mut dest = dest;
    let basename: String;
    if let Some(c) = a.get("content") {
        data = match c {
            Value::String(s) => s.clone().into_bytes(),
            other => serde_json::to_string(other)?.into_bytes(),
        };
        basename = String::new();
    } else {
        let src = st(a, "src").ok_or_else(|| anyhow!("missing required argument: src"))?;
        basename = src.rsplit('/').next().unwrap_or("").to_string();
        if templ {
            let p = find_src(ctx, "templates", &src)?;
            let text = String::from_utf8(crate::vault::read_file(&p)?)
                .map_err(|e| anyhow!("{}: {e}", p.display()))?;
            let mut vars = ctx.vars.clone();
            vars.entry("ansible_managed")
                .or_insert("Ansible managed".into());
            vars.entry("template_path")
                .or_insert(p.display().to_string().into());
            data = ctx.templar.render_text(&text, &vars)?.into_bytes();
        } else if bo(a, "remote_src", false) {
            data = ctx
                .conn
                .get_bytes(&src)?
                .ok_or_else(|| anyhow!("remote src {src} not found"))?;
        } else {
            let p = find_src(ctx, "files", &src)?;
            if p.is_dir() {
                bail!("copy of directories is not supported yet: {}", p.display());
            }
            data = crate::vault::read_file(&p).map_err(|e| anyhow!("{}: {e}", p.display()))?;
        }
    }
    if dest.ends_with('/') || ctx.conn.exec(&format!("test -d {}", q(&dest)))?.rc == 0 {
        let base = if templ {
            basename.trim_end_matches(".j2").to_string()
        } else {
            basename
        };
        dest = format!("{}/{}", dest.trim_end_matches('/'), base);
    }
    put_content(a, ctx, &dest, &data)
}

fn file(a: &Args, ctx: &Ctx) -> Result<Res> {
    let path = st_any(a, &["path", "dest", "name"])
        .ok_or_else(|| anyhow!("missing required argument: path"))?;
    let state = st(a, "state").unwrap_or_else(|| "file".into());
    let exists = ctx
        .conn
        .exec(&format!("test -e {p} || test -L {p}", p = q(&path)))?
        .rc
        == 0;
    let is_dir = ctx.conn.exec(&format!("test -d {}", q(&path)))?.rc == 0;
    let mut changed = false;
    let sh = |cmd: String| -> Result<()> {
        let o = ctx.conn.exec(&cmd)?;
        if o.rc != 0 {
            bail!("{}", o.stderr.trim());
        }
        Ok(())
    };
    match state.as_str() {
        "absent" => {
            if exists {
                changed = true;
                if !ctx.check {
                    sh(format!("rm -rf {}", q(&path)))?;
                }
            }
            let mut r = res(changed);
            r.insert("path".into(), path.into());
            r.insert("state".into(), "absent".into());
            return Ok(r);
        }
        "directory" => {
            if !is_dir {
                changed = true;
                if !ctx.check {
                    sh(format!("mkdir -p {}", q(&path)))?;
                }
            }
        }
        "touch" => {
            changed = true;
            if !ctx.check {
                sh(format!("touch {}", q(&path)))?;
            }
        }
        "link" | "hard" => {
            let src = st(a, "src").ok_or_else(|| anyhow!("src is required for state=link"))?;
            let cur = ctx.conn.exec(&format!("readlink {}", q(&path)))?;
            if state == "link" && cur.rc == 0 && cur.stdout.trim() == src {
            } else {
                changed = true;
                if !ctx.check {
                    let flag = if state == "link" { "-sfn" } else { "-f" };
                    sh(format!("ln {flag} {} {}", q(&src), q(&path)))?;
                }
            }
        }
        _ => {
            if !exists {
                return Ok(fail(format!(
                    "file ({path}) is absent, cannot continue; use state=touch or state=directory"
                )));
            }
        }
    }
    let ac = if exists || changed {
        apply_attrs(a, ctx, &path)?
    } else {
        false
    };
    let mut r = res(changed || ac);
    r.insert("path".into(), path.into());
    r.insert("state".into(), state.into());
    Ok(r)
}

pub(crate) fn read_text(ctx: &Ctx, path: &str, create: bool) -> Result<Option<String>> {
    match ctx.conn.get_bytes(path)? {
        Some(b) => Ok(Some(String::from_utf8_lossy(&b).into_owned())),
        None if create => Ok(None),
        None => bail!("Destination {path} does not exist !"),
    }
}

pub(crate) fn write_text(
    a: &Args,
    ctx: &Ctx,
    path: &str,
    old: Option<&str>,
    new: &str,
    mut r: Res,
) -> Result<Res> {
    let changed = old != Some(new);
    if changed {
        if ctx.diff {
            r.insert(
                "diff".into(),
                unified_diff(old.unwrap_or(""), new, path).into(),
            );
        }
        if !ctx.check {
            ctx.conn.exec(&format!("mkdir -p {}", q(&dir_of(path))))?;
            if let Some(f) = validate_content(a, ctx, new.as_bytes())? {
                return Ok(f);
            }
            ctx.conn.put(path, new.as_bytes(), None)?;
        }
    }
    let ac = apply_attrs(a, ctx, path)?;
    r.insert("changed".into(), (changed || ac).into());
    Ok(r)
}

fn lineinfile(a: &Args, ctx: &Ctx) -> Result<Res> {
    let path = st_any(a, &["path", "dest", "name"]).ok_or_else(|| anyhow!("missing path"))?;
    let create = bo(a, "create", false);
    let old = read_text(ctx, &path, create)?;
    let text = old.clone().unwrap_or_default();
    let mut ls: Vec<String> = text.lines().map(String::from).collect();
    let state = st(a, "state").unwrap_or_else(|| "present".into());
    let line = st(a, "line");
    let re = st(a, "regexp")
        .map(|r| regex::Regex::new(&r))
        .transpose()
        .map_err(|e| anyhow!("bad regexp: {e}"))?;
    let mut msg = String::new();
    if state == "absent" {
        let before = ls.len();
        ls.retain(|l| match (&re, &line) {
            (Some(r), _) => !r.is_match(l),
            (None, Some(x)) => l != x,
            _ => true,
        });
        if ls.len() != before {
            msg = format!("{} line(s) removed", before - ls.len());
        }
    } else {
        let line =
            line.ok_or_else(|| anyhow!("one of line or regexp is required with state=present"))?;
        let backrefs = bo(a, "backrefs", false);
        let found = match &re {
            Some(r) => {
                let idxs: Vec<usize> = ls
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| r.is_match(l))
                    .map(|(i, _)| i)
                    .collect();
                if bo(a, "firstmatch", false) {
                    idxs.first().copied()
                } else {
                    idxs.last().copied()
                }
            }
            None => ls.iter().position(|l| *l == line),
        };
        match found {
            Some(i) => {
                let newl = if backrefs {
                    re.as_ref()
                        .unwrap()
                        .replace(&ls[i], crate::template::py_repl(&line).as_str())
                        .into_owned()
                } else {
                    line.clone()
                };
                if ls[i] != newl {
                    ls[i] = newl;
                    msg = "line replaced".into();
                }
            }
            None if backrefs => {}
            None => {
                let mut pos = ls.len();
                if let Some(b) = st(a, "insertbefore") {
                    if b == "BOF" {
                        pos = 0;
                    } else if let Ok(r) = regex::Regex::new(&b)
                        && let Some(i) = ls.iter().rposition(|l| r.is_match(l))
                    {
                        pos = i;
                    }
                } else if let Some(af) = st(a, "insertafter")
                    && af != "EOF"
                    && let Ok(r) = regex::Regex::new(&af)
                    && let Some(i) = ls.iter().rposition(|l| r.is_match(l))
                {
                    pos = i + 1;
                }
                ls.insert(pos, line);
                msg = "line added".into();
            }
        }
    }
    let mut new = ls.join("\n");
    if !ls.is_empty() {
        new.push('\n');
    }
    let mut r = res(false);
    r.insert("msg".into(), msg.into());
    write_text(a, ctx, &path, old.as_deref(), &new, r)
}

fn blockinfile(a: &Args, ctx: &Ctx) -> Result<Res> {
    let path = st_any(a, &["path", "dest", "name"]).ok_or_else(|| anyhow!("missing path"))?;
    let old = read_text(ctx, &path, bo(a, "create", false))?;
    let text = old.clone().unwrap_or_default();
    let marker = st(a, "marker").unwrap_or_else(|| "# {mark} ANSIBLE MANAGED BLOCK".into());
    let begin = marker.replace(
        "{mark}",
        &st(a, "marker_begin").unwrap_or_else(|| "BEGIN".into()),
    );
    let end = marker.replace(
        "{mark}",
        &st(a, "marker_end").unwrap_or_else(|| "END".into()),
    );
    let mut ls: Vec<String> = text.lines().map(String::from).collect();
    let bi = ls.iter().position(|l| *l == begin);
    let ei = bi.and_then(|b| ls.iter().skip(b).position(|l| *l == end).map(|e| e + b));
    let absent = st(a, "state").as_deref() == Some("absent");
    let block = st(a, "block").unwrap_or_default();
    let mut newblock: Vec<String> = vec![begin.clone()];
    newblock.extend(block.lines().map(String::from));
    newblock.push(end);
    match (bi, ei) {
        (Some(b), Some(e)) => {
            ls.splice(b..=e, if absent { vec![] } else { newblock });
        }
        _ if absent => {}
        _ => {
            let pos = match st(a, "insertbefore").as_deref() {
                Some("BOF") => 0,
                _ => ls.len(),
            };
            ls.splice(pos..pos, newblock);
        }
    }
    let mut new = ls.join("\n");
    if !ls.is_empty() {
        new.push('\n');
    }
    write_text(a, ctx, &path, old.as_deref(), &new, res(false))
}

fn replace(a: &Args, ctx: &Ctx) -> Result<Res> {
    let path = st_any(a, &["path", "dest", "name"]).ok_or_else(|| anyhow!("missing path"))?;
    let old = read_text(ctx, &path, false)?.unwrap_or_default();
    let re = regex::RegexBuilder::new(&st(a, "regexp").ok_or_else(|| anyhow!("missing regexp"))?)
        .multi_line(true)
        .build()?;
    let rep = crate::template::py_repl(&st(a, "replace").unwrap_or_default());
    let new = re.replace_all(&old, rep.as_str()).into_owned();
    write_text(a, ctx, &path, Some(&old), &new, res(false))
}

fn stat(a: &Args, ctx: &Ctx) -> Result<Res> {
    let path = st_any(a, &["path", "dest"]).ok_or_else(|| anyhow!("missing path"))?;
    let p = q(&path);
    let script = format!(
        r#"p={p}
if [ -L "$p" ]; then echo islnk=1; echo lnk_source=$(readlink "$p"); else echo islnk=0; fi
if [ -e "$p" ]; then echo exists=1
[ -d "$p" ] && echo isdir=1 || echo isdir=0
[ -f "$p" ] && echo isreg=1 || echo isreg=0
echo size=$(wc -c < "$p" 2>/dev/null | tr -d ' ')
echo mode=$(stat -c %a "$p" 2>/dev/null || stat -f %Lp "$p")
echo pw_name=$(stat -c %U "$p" 2>/dev/null || stat -f %Su "$p")
echo gr_name=$(stat -c %G "$p" 2>/dev/null || stat -f %Sg "$p")
echo mtime=$(stat -c %Y "$p" 2>/dev/null || stat -f %m "$p")
[ -f "$p" ] && echo checksum=$( (sha1sum "$p" 2>/dev/null || shasum "$p") | cut -d' ' -f1)
else echo exists=0; fi"#
    );
    let o = ctx.conn.exec(&script)?;
    let mut s = Map::new();
    for l in o.stdout.lines() {
        if let Some((k, v)) = l.split_once('=') {
            let val = match k {
                "exists" | "isdir" | "isreg" | "islnk" => Value::Bool(v == "1"),
                "size" | "mtime" => v.parse::<i64>().map(Value::from).unwrap_or(Value::Null),
                _ => Value::String(v.to_string()),
            };
            s.insert(k.to_string(), val);
        }
    }
    if s.get("exists") != Some(&Value::Bool(true)) {
        s.insert("exists".into(), false.into());
    }
    let mut r = res(false);
    r.insert("stat".into(), Value::Object(s));
    Ok(r)
}

fn slurp(a: &Args, ctx: &Ctx) -> Result<Res> {
    use base64::Engine;
    let path = st_any(a, &["src", "path"]).ok_or_else(|| anyhow!("missing src"))?;
    match ctx.conn.get_bytes(&path)? {
        Some(b) => {
            let mut r = res(false);
            r.insert(
                "content".into(),
                base64::engine::general_purpose::STANDARD.encode(b).into(),
            );
            r.insert("encoding".into(), "base64".into());
            r.insert("source".into(), path.into());
            Ok(r)
        }
        None => Ok(fail(format!("file not found: {path}"))),
    }
}

fn include_vars(a: &Args, ctx: &Ctx) -> Result<Res> {
    let f = st_any(a, &["file", "_raw_params"]);
    let mut out = Map::new();
    if let Some(f) = f {
        let p = find_src(ctx, "vars", &f)?;
        crate::inventory::try_load_vars_file(&p, &mut out)?;
    } else if let Some(d) = st(a, "dir") {
        let p = find_src(ctx, "vars", &d)?;
        let mut fs: Vec<_> = std::fs::read_dir(&p)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("yml" | "yaml" | "json")
                )
            })
            .collect();
        fs.sort();
        for f in fs {
            crate::inventory::try_load_vars_file(&f, &mut out)?;
        }
    }
    let out = match st(a, "name") {
        Some(n) => Map::from_iter([(n, Value::Object(out))]),
        None => out,
    };
    let mut r = res(false);
    r.insert("ansible_facts".into(), Value::Object(out));
    Ok(r)
}

// ---------- control ----------

fn debug(a: &Args, ctx: &Ctx) -> Result<Res> {
    let mut r = res(false);
    if let Some(v) = st(a, "var") {
        let expr = format!("{{{{ {v} }}}}");
        let val = ctx
            .templar
            .render_str(&expr, ctx.vars)
            .unwrap_or_else(|_| Value::String("VARIABLE IS NOT DEFINED!".into()));
        r.insert(v, val);
    } else {
        r.insert(
            "msg".into(),
            a.get("msg")
                .cloned()
                .unwrap_or_else(|| "Hello world!".into()),
        );
    }
    Ok(r)
}

fn assert_mod(a: &Args, ctx: &Ctx) -> Result<Res> {
    let conds: Vec<String> = match a.get("that") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(v)) => v
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => bail!("assert requires 'that'"),
    };
    for c in &conds {
        if !ctx.templar.eval_bool(c, ctx.vars)? {
            let mut r =
                fail(st_any(a, &["fail_msg", "msg"]).unwrap_or_else(|| "Assertion failed".into()));
            r.insert("assertion".into(), c.clone().into());
            r.insert("evaluated_to".into(), false.into());
            return Ok(r);
        }
    }
    let mut r = res(false);
    r.insert(
        "msg".into(),
        st_any(a, &["success_msg", "msg"])
            .unwrap_or_else(|| "All assertions passed".into())
            .into(),
    );
    Ok(r)
}

fn wait_for(a: &Args, ctx: &Ctx) -> Result<Res> {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::{Duration, Instant};
    let timeout = a.get("timeout").and_then(|v| v.as_f64()).unwrap_or(300.0);
    if let Some(d) = a.get("delay").and_then(|v| v.as_f64()) {
        std::thread::sleep(Duration::from_secs_f64(d));
    }
    let state = st(a, "state").unwrap_or_else(|| "started".into());
    let start = Instant::now();
    let sleep = Duration::from_secs_f64(a.get("sleep").and_then(|v| v.as_f64()).unwrap_or(1.0));
    loop {
        let ok = if let Some(port) = a.get("port").and_then(|p| {
            p.as_u64()
                .or_else(|| p.as_str().and_then(|s| s.parse().ok()))
        }) {
            let host = st(a, "host").unwrap_or_else(|| {
                if ctx.conn.local {
                    "127.0.0.1".into()
                } else {
                    ctx.conn.host.clone()
                }
            });
            let open = format!("{host}:{port}")
                .to_socket_addrs()
                .ok()
                .and_then(|mut it| it.next())
                .map(|ad| TcpStream::connect_timeout(&ad, Duration::from_secs(2)).is_ok())
                .unwrap_or(false);
            if state == "stopped" || state == "drained" {
                !open
            } else {
                open
            }
        } else if let Some(p) = st(a, "path") {
            let ex = ctx.conn.exec(&format!("test -e {}", q(&p)))?.rc == 0;
            if state == "absent" { !ex } else { ex }
        } else {
            std::thread::sleep(Duration::from_secs_f64(
                a.get("timeout")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
                    .min(timeout),
            ));
            true
        };
        if ok {
            let mut r = res(false);
            r.insert("elapsed".into(), (start.elapsed().as_secs() as i64).into());
            return Ok(r);
        }
        if start.elapsed().as_secs_f64() > timeout {
            return Ok(fail(format!(
                "Timeout when waiting for {}",
                st_any(a, &["path", "port"]).unwrap_or_default()
            )));
        }
        std::thread::sleep(sleep);
    }
}

// ---------- facts ----------

fn setup(ctx: &Ctx) -> Result<Res> {
    let script = r#"echo system=$(uname -s); echo kernel=$(uname -r); echo arch=$(uname -m)
echo hostname=$(hostname -s 2>/dev/null || hostname); echo fqdn=$(hostname -f 2>/dev/null || hostname)
echo user=$(id -un); echo uid=$(id -u); echo home=$HOME; echo shell=$SHELL
if [ -f /etc/os-release ]; then . /etc/os-release; echo distro="$NAME"; echo distver="$VERSION_ID"; echo osid="$ID"; echo idlike="$ID_LIKE"; echo codename="${VERSION_CODENAME:-$UBUNTU_CODENAME}"; fi
if [ "$(uname -s)" = Darwin ]; then echo distro=MacOSX; echo distver=$(sw_vers -productVersion); fi
echo epoch=$(date +%s); echo tz=$(date +%Z); echo ip4=$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n "s/.* src \\([0-9.]*\\).*/\\1/p" | head -1); echo iface=$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n "s/.* dev \\([^ ]*\\).*/\\1/p" | head -1); echo virt=$(systemd-detect-virt 2>/dev/null); echo selinux=$(getenforce 2>/dev/null || echo Disabled)
echo cpus=$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null)
echo mem=$(awk '/MemTotal/ {print int($2/1024)}' /proc/meminfo 2>/dev/null || echo $(( $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1048576 )))
for m in apt-get dnf yum zypper apk pacman brew; do command -v $m >/dev/null 2>&1 && { echo pkgmgr=$m; break; }; done
[ -d /run/systemd/system ] && echo svcmgr=systemd || echo svcmgr=service
PY=$(command -v python3 || command -v python); [ -n "$PY" ] && echo pyexe=$PY && echo pyver=$($PY -c 'import sys;print("%d.%d.%d"%sys.version_info[:3])' 2>/dev/null)"#;
    let o = ctx.conn.exec(script)?;
    if o.rc != 0 && o.stdout.is_empty() {
        return Ok(fail(format!("fact gathering failed: {}", o.stderr.trim())));
    }
    let mut m: std::collections::HashMap<String, String> = Default::default();
    for l in o.stdout.lines() {
        if let Some((k, v)) = l.split_once('=') {
            m.insert(k.to_string(), v.trim().to_string());
        }
    }
    let g = |k: &str| m.get(k).cloned().unwrap_or_default();
    let osid = g("osid");
    let like = format!("{} {}", osid, g("idlike"));
    let family = if g("system") == "Darwin" {
        "Darwin"
    } else if like.contains("debian") || like.contains("ubuntu") {
        "Debian"
    } else if like.contains("rhel") || like.contains("fedora") || like.contains("centos") {
        "RedHat"
    } else if like.contains("suse") {
        "Suse"
    } else if like.contains("alpine") {
        "Alpine"
    } else if like.contains("arch") {
        "Archlinux"
    } else {
        &g("system")
    };
    let ver = g("distver");
    let distro = match g("distro").as_str() {
        "Red Hat Enterprise Linux" => "RedHat".to_string(),
        "CentOS Linux" | "CentOS Stream" => "CentOS".to_string(),
        d => d.split_whitespace().next().unwrap_or("").to_string(),
    };
    let mut f = Map::new();
    let mut ins = |k: &str, v: Value| {
        f.insert(format!("ansible_{k}"), v);
    };
    ins("system", g("system").into());
    ins("kernel", g("kernel").into());
    ins("architecture", g("arch").into());
    ins("machine", g("arch").into());
    ins("hostname", g("hostname").into());
    ins("nodename", g("fqdn").into());
    ins("fqdn", g("fqdn").into());
    ins("user_id", g("user").into());
    ins("user_uid", g("uid").parse::<i64>().unwrap_or(0).into());
    ins("user_dir", g("home").into());
    ins("user_shell", g("shell").into());
    ins("distribution", distro.into());
    ins("distribution_version", ver.clone().into());
    ins(
        "distribution_major_version",
        ver.split('.').next().unwrap_or("").into(),
    );
    ins("os_family", family.into());
    ins("distribution_release", g("codename").into());
    ins("distribution_file_variety", family.into());
    ins(
        "date_time",
        json!({"epoch": g("epoch"), "tz": g("tz"), "iso8601": "", "date": "", "time": ""}),
    );
    ins(
        "selinux",
        json!({"status": if g("selinux") == "Disabled" || g("selinux").is_empty() { "disabled" } else { "enabled" }}),
    );
    ins("virtualization_type", g("virt").into());
    ins(
        "virtualization_role",
        if g("virt").is_empty() || g("virt") == "none" {
            "NA"
        } else {
            "guest"
        }
        .into(),
    );
    if !g("ip4").is_empty() {
        ins(
            "default_ipv4",
            json!({"address": g("ip4"), "interface": g("iface")}),
        );
    }
    ins(
        "processor_vcpus",
        g("cpus").parse::<i64>().unwrap_or(1).into(),
    );
    ins(
        "processor_count",
        g("cpus").parse::<i64>().unwrap_or(1).into(),
    );
    ins("memtotal_mb", g("mem").parse::<i64>().unwrap_or(0).into());
    ins(
        "pkg_mgr",
        match g("pkgmgr").as_str() {
            "apt-get" => "apt".to_string(),
            x => x.to_string(),
        }
        .into(),
    );
    ins("service_mgr", g("svcmgr").into());
    if let Some(pv) = m.get("pyver").filter(|v| !v.is_empty()) {
        let p: Vec<i64> = pv.split('.').map(|x| x.parse().unwrap_or(0)).collect();
        f.insert("ansible_python".into(), json!({"executable": g("pyexe"), "version": {"major": p[0], "minor": p.get(1).copied().unwrap_or(0), "micro": p.get(2).copied().unwrap_or(0), "releaselevel": "final", "serial": 0}, "version_info": p}));
        f.insert("ansible_python_version".into(), pv.clone().into());
    }
    let mut r = res(false);
    r.insert("ansible_facts".into(), Value::Object(f));
    Ok(r)
}

// ---------- packages / services / users ----------

fn detect_pkg(ctx: &Ctx) -> Result<String> {
    for m in ["apt-get", "dnf", "yum", "apk", "brew"] {
        if ctx.conn.exec(&format!("command -v {m}"))?.rc == 0 {
            return Ok(if m == "apt-get" {
                "apt".into()
            } else {
                m.into()
            });
        }
    }
    bail!("no supported package manager found")
}

fn package(module: &str, a: &Args, ctx: &Ctx) -> Result<Res> {
    let mgr = match module {
        "apt" => "apt".to_string(),
        "yum" | "dnf" | "apk" => module.to_string(),
        "homebrew" | "brew" => "brew".into(),
        _ => detect_pkg(ctx)?,
    };
    let state = st(a, "state").unwrap_or_else(|| "present".into());
    let names = if a.contains_key("name") {
        list(a, "name")
    } else {
        list(a, "pkg")
    };
    let mut changed = false;
    let mut msgs = vec![];
    if bo(a, "update_cache", false) && !ctx.check {
        let cmd = match mgr.as_str() {
            "apt" => "apt-get update",
            "apk" => "apk update",
            "brew" => "brew update",
            "dnf" | "yum" => "true",
            _ => "true",
        };
        let o = ctx.conn.exec(cmd)?;
        if o.rc != 0 {
            return Ok(fail(format!("update_cache failed: {}", o.stderr.trim())));
        }
        if mgr == "apt" && o.stdout.lines().any(|l| l.starts_with("Get:")) {
            changed = true;
        }
    }
    let installed = |p: &str| -> Result<bool> {
        let cmd = match mgr.as_str() {
            "apt" => match p.split_once('=') {
                Some((n, v)) => format!(
                    "dpkg-query -W -f='${{Status}} ${{Version}}' {} 2>/dev/null | grep -q 'install ok installed' && case \"$(dpkg-query -W -f='${{Version}}' {} 2>/dev/null)\" in {}) true;; *) false;; esac",
                    q(n),
                    q(n),
                    v
                ),
                None if p.contains('*') => format!(
                    "dpkg-query -W -f='${{Status}}\\n' {} 2>/dev/null | grep -q 'install ok installed'",
                    q(p)
                ),
                None => format!(
                    "dpkg-query -W -f='${{Status}}' {} 2>/dev/null | grep -q 'install ok installed'",
                    q(p)
                ),
            },
            "apk" => format!("apk info -e {}", q(p)),
            "brew" => format!(
                "brew list --formula {p} >/dev/null 2>&1 || brew list --cask {p} >/dev/null 2>&1",
                p = q(p)
            ),
            _ => format!("rpm -q {} >/dev/null 2>&1", q(p)),
        };
        Ok(ctx.conn.exec(&cmd)?.rc == 0)
    };
    let (mut to_install, mut to_remove) = (vec![], vec![]);
    for n in &names {
        let inst = installed(n)?;
        match state.as_str() {
            "absent" | "removed" => {
                if inst {
                    to_remove.push(n.clone())
                }
            }
            "latest" => to_install.push(n.clone()),
            _ => {
                if !inst {
                    to_install.push(n.clone())
                }
            }
        }
    }
    let joined = |v: &[String]| v.iter().map(|x| q(x)).collect::<Vec<_>>().join(" ");
    if !to_install.is_empty() {
        let cmd = match mgr.as_str() {
            "apt" => format!(
                "DEBIAN_FRONTEND=noninteractive apt-get install -y -qq -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold {}",
                joined(&to_install)
            ),
            "apk" => format!("apk add {}", joined(&to_install)),
            "brew" => format!("brew install {}", joined(&to_install)),
            m => format!("{m} install -y {}", joined(&to_install)),
        };
        if state == "latest" {
            // detect no-op via simulation
            let sim = match mgr.as_str() {
                "apt" => Some(format!(
                    "apt-get -s install {} | grep -E '^Inst ' | wc -l",
                    joined(&to_install)
                )),
                _ => None,
            };
            let pending = match sim {
                Some(s) => ctx.conn.exec(&s)?.stdout.trim().parse::<i64>().unwrap_or(1) > 0,
                None => true,
            };
            if pending {
                changed = true;
                if !ctx.check {
                    let o = ctx.conn.exec(&cmd)?;
                    if o.rc != 0 {
                        return Ok(fail(format!(
                            "{} failed: {}{}",
                            mgr,
                            o.stdout.trim(),
                            o.stderr.trim()
                        )));
                    }
                }
            }
        } else {
            changed = true;
            if !ctx.check {
                let o = ctx.conn.exec(&cmd)?;
                if o.rc != 0 {
                    return Ok(fail(format!(
                        "{} install failed: {}{}",
                        mgr,
                        o.stdout.trim(),
                        o.stderr.trim()
                    )));
                }
            }
        }
        msgs.push(format!("installed: {}", to_install.join(", ")));
    }
    if !to_remove.is_empty() {
        changed = true;
        if !ctx.check {
            let cmd = match mgr.as_str() {
                "apt" => format!(
                    "DEBIAN_FRONTEND=noninteractive apt-get remove -y -qq -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold {}",
                    joined(&to_remove)
                ),
                "apk" => format!("apk del {}", joined(&to_remove)),
                "brew" => format!("brew uninstall {}", joined(&to_remove)),
                m => format!("{m} remove -y {}", joined(&to_remove)),
            };
            let o = ctx.conn.exec(&cmd)?;
            if o.rc != 0 {
                return Ok(fail(format!("{} remove failed: {}", mgr, o.stderr.trim())));
            }
        }
        msgs.push(format!("removed: {}", to_remove.join(", ")));
    }
    let mut r = res(changed);
    r.insert("msg".into(), msgs.join("; ").into());
    Ok(r)
}

fn service(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = match st_any(a, &["name", "service", "unit"]) {
        Some(n) => n,
        None if bo(a, "daemon_reload", false) || bo(a, "daemon_reexec", false) => {
            if !ctx.check {
                let o = ctx.conn.exec(if bo(a, "daemon_reexec", false) {
                    "systemctl daemon-reexec"
                } else {
                    "systemctl daemon-reload"
                })?;
                if o.rc != 0 {
                    return Ok(fail(o.stderr.trim().to_string()));
                }
            }
            return Ok(res(false));
        }
        None => return Ok(fail("missing required arguments: name")),
    };
    let systemd = ctx
        .conn
        .exec("command -v systemctl >/dev/null && test -d /run/systemd/system")?
        .rc
        == 0;
    let mut changed = false;
    let n = q(&name);
    let run = |cmd: String| -> Result<()> {
        let o = ctx.conn.exec(&cmd)?;
        if o.rc != 0 {
            bail!("{cmd}: {}{}", o.stdout.trim(), o.stderr.trim());
        }
        Ok(())
    };
    if bo(a, "daemon_reload", false) && systemd && !ctx.check {
        run("systemctl daemon-reload".into())?;
    }
    if let Some(en) = a.get("enabled").and_then(loose_bool) {
        if !systemd {
            if ctx.conn.exec("command -v update-rc.d")?.rc == 0 && !ctx.check {
                let _ = ctx.conn.exec(&format!(
                    "update-rc.d {n} {}",
                    if en { "defaults" } else { "disable" }
                ));
            }
        } else {
            let cur = ctx
                .conn
                .exec(&format!("systemctl is-enabled {n}"))?
                .stdout
                .trim()
                == "enabled";
            if cur != en {
                changed = true;
                if !ctx.check {
                    run(format!(
                        "systemctl {} {n}",
                        if en { "enable" } else { "disable" }
                    ))?;
                }
            }
        }
    }
    if let Some(state) = st(a, "state") {
        let active = if systemd {
            ctx.conn
                .exec(&format!("systemctl is-active {n}"))?
                .stdout
                .trim()
                == "active"
        } else {
            ctx.conn.exec(&format!("service {n} status"))?.rc == 0
        };
        let action = match state.as_str() {
            "started" if !active => Some("start"),
            "stopped" if active => Some("stop"),
            "restarted" => Some("restart"),
            "reloaded" => Some("reload"),
            _ => None,
        };
        if let Some(act) = action {
            changed = true;
            if !ctx.check {
                run(if systemd {
                    format!("systemctl {act} {n}")
                } else {
                    format!("service {n} {act}")
                })?;
            }
        }
    }
    let mut r = res(changed);
    r.insert("name".into(), name.into());
    Ok(r)
}

fn user(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let state = st(a, "state").unwrap_or_else(|| "present".into());
    let exists = ctx.conn.exec(&format!("id -u {}", q(&name)))?.rc == 0;
    let n = q(&name);
    let run = |cmd: String| -> Result<()> {
        let o = ctx.conn.exec(&cmd)?;
        if o.rc != 0 {
            bail!("{cmd}: {}", o.stderr.trim());
        }
        Ok(())
    };
    if state == "absent" {
        if exists && !ctx.check {
            run(format!(
                "userdel {}{n}",
                if bo(a, "remove", false) { "-r " } else { "" }
            ))?;
        }
        return Ok(res(exists));
    }
    let mut flags = String::new();
    if let Some(v) = st(a, "uid") {
        flags += &format!(" -u {}", q(&v));
    }
    if let Some(v) = st(a, "group") {
        flags += &format!(" -g {}", q(&v));
    }
    if let Some(v) = st(a, "shell") {
        flags += &format!(" -s {}", q(&v));
    }
    if let Some(v) = st(a, "home") {
        flags += &format!(" -d {}", q(&v));
    }
    if let Some(v) = st(a, "comment") {
        flags += &format!(" -c {}", q(&v));
    }
    if let Some(v) = st(a, "password") {
        flags += &format!(" -p {}", q(&v));
    }
    let groups = list(a, "groups");
    if !groups.is_empty() {
        flags += &format!(
            " {}-G {}",
            if bo(a, "append", false) && exists {
                "-a "
            } else {
                ""
            },
            q(&groups.join(","))
        );
    }
    if !exists {
        if !ctx.check {
            if bo(a, "system", false) {
                flags += " -r";
            }
            if bo(a, "create_home", true) {
                flags += " -m";
            }
            run(format!("useradd{flags} {n}"))?;
        }
        return Ok(res(true));
    }
    // existing user: only modify when a requested attribute differs
    let mut changed = false;
    let info = ctx.conn.exec(&format!("getent passwd {n}"))?.stdout;
    let f: Vec<&str> = info.trim().split(':').collect();
    let differs = |idx: usize, want: Option<String>| {
        want.map(|w| f.get(idx).map(|h| *h != w).unwrap_or(false))
            .unwrap_or(false)
    };
    if f.len() >= 7
        && (differs(2, st(a, "uid"))
            || differs(6, st(a, "shell"))
            || differs(5, st(a, "home"))
            || differs(4, st(a, "comment")))
    {
        changed = true;
    }
    if !groups.is_empty() {
        let have = ctx.conn.exec(&format!("id -nG {n}"))?.stdout;
        let have: Vec<&str> = have.split_whitespace().collect();
        if !groups.iter().all(|g| have.contains(&g.as_str())) {
            changed = true;
        }
    }
    if changed && !ctx.check {
        run(format!("usermod{flags} {n}"))?;
    }
    Ok(res(changed))
}

fn group(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let exists = ctx.conn.exec(&format!("getent group {}", q(&name)))?.rc == 0;
    let absent = st(a, "state").as_deref() == Some("absent");
    let cmd = if absent && exists {
        Some(format!("groupdel {}", q(&name)))
    } else if !absent && !exists {
        let mut c = String::from("groupadd");
        if let Some(g) = st(a, "gid") {
            c += &format!(" -g {}", q(&g));
        }
        if bo(a, "system", false) {
            c += " -r";
        }
        Some(format!("{c} {}", q(&name)))
    } else {
        None
    };
    if let Some(c) = &cmd
        && !ctx.check
    {
        let o = ctx.conn.exec(c)?;
        if o.rc != 0 {
            return Ok(fail(o.stderr.trim().to_string()));
        }
    }
    Ok(res(cmd.is_some()))
}

// ---------- network / vcs ----------

fn git(a: &Args, ctx: &Ctx) -> Result<Res> {
    let repo = st(a, "repo").ok_or_else(|| anyhow!("missing repo"))?;
    let dest = st(a, "dest").ok_or_else(|| anyhow!("missing dest"))?;
    let version = st(a, "version");
    let d = q(&dest);
    let sh = |cmd: &str| -> Result<crate::conn::Output> { ctx.conn.exec(cmd) };
    if sh(&format!("test -d {d}/.git"))?.rc != 0 {
        if !bo(a, "clone", true) {
            return Ok(fail("repository does not exist and clone=no"));
        }
        if !ctx.check {
            let mut cmd = String::from("git clone");
            if let Some(depth) = st(a, "depth") {
                cmd += &format!(" --depth {depth}");
            }
            if let Some(v) = &version {
                cmd += &format!(" -b {}", q(v));
            }
            let o = sh(&format!("{cmd} {} {d}", q(&repo)))?;
            if o.rc != 0 {
                return Ok(fail(format!("git clone failed: {}", o.stderr.trim())));
            }
        }
        return Ok(res(true));
    }
    if !bo(a, "update", true) {
        return Ok(res(false));
    }
    let before = sh(&format!("git -C {d} rev-parse HEAD"))?
        .stdout
        .trim()
        .to_string();
    if ctx.check {
        return Ok(res(false));
    }
    let o = sh(&format!("git -C {d} fetch --tags -q origin"))?;
    if o.rc != 0 {
        return Ok(fail(format!("git fetch failed: {}", o.stderr.trim())));
    }
    if let Some(v) = &version {
        let force = if bo(a, "force", false) { "-f " } else { "" };
        let o = sh(&format!("git -C {d} checkout -q {force}{}", q(v)))?;
        if o.rc != 0 {
            return Ok(fail(format!("git checkout failed: {}", o.stderr.trim())));
        }
    }
    let _ = sh(&format!(
        "git -C {d} merge --ff-only -q '@{{u}}' 2>/dev/null"
    ));
    let after = sh(&format!("git -C {d} rev-parse HEAD"))?
        .stdout
        .trim()
        .to_string();
    let mut r = res(before != after);
    r.insert("before".into(), before.into());
    r.insert("after".into(), after.into());
    Ok(r)
}

fn get_url(a: &Args, ctx: &Ctx) -> Result<Res> {
    let url = st(a, "url").ok_or_else(|| anyhow!("missing url"))?;
    let mut dest = st(a, "dest").ok_or_else(|| anyhow!("missing dest"))?;
    if ctx.conn.exec(&format!("test -d {}", q(&dest)))?.rc == 0 {
        dest = format!(
            "{}/{}",
            dest.trim_end_matches('/'),
            url.rsplit('/').next().unwrap_or("download")
        );
    }
    let exists = ctx.conn.exec(&format!("test -f {}", q(&dest)))?.rc == 0;
    let mut changed = false;
    if !exists || bo(a, "force", false) {
        changed = true;
        if !ctx.check {
            let tmp = format!("{dest}.ransible.tmp");
            let k = if bo(a, "validate_certs", true) {
                ""
            } else {
                "-k "
            };
            let t = st(a, "timeout").unwrap_or_else(|| "10".into());
            let o = ctx.conn.exec(&format!(
                "curl -fsSL {k}--max-time {t} -o {} {} && mv -f {} {}",
                q(&tmp),
                q(&url),
                q(&tmp),
                q(&dest)
            ))?;
            if o.rc != 0 {
                return Ok(fail(format!("Request failed: {}", o.stderr.trim())));
            }
        }
    }
    if !ctx.check || exists {
        changed |= apply_attrs(a, ctx, &dest)?;
    }
    let mut r = res(changed);
    r.insert("dest".into(), dest.into());
    r.insert("url".into(), url.into());
    Ok(r)
}

fn uri(a: &Args, ctx: &Ctx) -> Result<Res> {
    let url = st(a, "url").ok_or_else(|| anyhow!("missing url"))?;
    let method = st(a, "method")
        .unwrap_or_else(|| "GET".into())
        .to_uppercase();
    let mut cmd = format!(
        "curl -sS -X {} --max-time {}",
        q(&method),
        st(a, "timeout").unwrap_or_else(|| "30".into())
    );
    if !bo(a, "validate_certs", true) {
        cmd += " -k";
    }
    if !bo(a, "follow_redirects", true) || st(a, "follow_redirects").as_deref() == Some("none") {
    } else {
        cmd += " -L";
    }
    let json_body = st(a, "body_format").as_deref() == Some("json");
    if let Some(Value::Object(h)) = a.get("headers") {
        for (k, v) in h {
            cmd += &format!(
                " -H {}",
                q(&format!(
                    "{k}: {}",
                    v.as_str()
                        .map(String::from)
                        .unwrap_or_else(|| v.to_string())
                ))
            );
        }
    }
    if json_body {
        cmd += " -H 'Content-Type: application/json'";
    }
    if let (Some(u), Some(p)) = (
        st(a, "url_username").or(st(a, "user")),
        st(a, "url_password").or(st(a, "password")),
    ) {
        cmd += &format!(" -u {}", q(&format!("{u}:{p}")));
    }
    let body: Option<Vec<u8>> = a.get("body").map(|b| match b {
        Value::String(s) => s.clone().into_bytes(),
        other => serde_json::to_vec(other).unwrap_or_default(),
    });
    if body.is_some() {
        cmd += " --data-binary @-";
    }
    cmd += &format!(" -w '\\n%{{http_code}}' {}", q(&url));
    if ctx.check && method != "GET" && method != "HEAD" {
        return Ok(res(false));
    }
    let o = match &body {
        Some(b) => ctx.conn.exec_stdin(&cmd, b)?,
        None => ctx.conn.exec(&cmd)?,
    };
    if o.rc != 0 {
        let mut r = fail(format!("Request failed: {}", o.stderr.trim()));
        r.insert("status".into(), (-1).into());
        return Ok(r);
    }
    let (content, status) = o.stdout.rsplit_once('\n').unwrap_or(("", &o.stdout));
    let status: i64 = status.trim().parse().unwrap_or(0);
    let ok_codes: Vec<i64> = match a.get("status_code") {
        Some(Value::Array(v)) => v
            .iter()
            .filter_map(|x| {
                x.as_i64()
                    .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
            })
            .collect(),
        Some(v) => vec![
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                .unwrap_or(200),
        ],
        None => vec![200],
    };
    let mut r = if ok_codes.contains(&status) {
        res(false)
    } else {
        fail(format!("Status code was {status} and not {ok_codes:?}"))
    };
    r.insert("status".into(), status.into());
    r.insert("url".into(), url.into());
    if let Ok(j) = serde_json::from_str::<Value>(content) {
        r.insert("json".into(), j);
    }
    if bo(a, "return_content", false) {
        r.insert("content".into(), content.into());
    }
    Ok(r)
}

fn fetch(a: &Args, ctx: &Ctx) -> Result<Res> {
    let src = st(a, "src").ok_or_else(|| anyhow!("missing src"))?;
    let dest = st(a, "dest").ok_or_else(|| anyhow!("missing dest"))?;
    let flat = bo(a, "flat", false);
    let local = if flat {
        dest.clone()
    } else {
        format!(
            "{}/{}/{}",
            dest.trim_end_matches('/'),
            ctx.host,
            src.trim_start_matches('/')
        )
    };
    let local = if flat && dest.ends_with('/') {
        format!("{dest}{}", src.rsplit('/').next().unwrap_or("file"))
    } else {
        local
    };
    let Some(data) = ctx.conn.get_bytes(&src)? else {
        return Ok(fail(format!("remote file does not exist: {src}")));
    };
    let changed = std::fs::read(&local).ok().as_deref() != Some(&data[..]);
    if changed && !ctx.check {
        if let Some(p) = Path::new(&local).parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&local, &data)?;
    }
    let mut r = res(changed);
    r.insert("dest".into(), local.into());
    Ok(r)
}

fn synchronize(a: &Args, ctx: &Ctx) -> Result<Res> {
    let src = st(a, "src").ok_or_else(|| anyhow!("missing src"))?;
    let dest = st(a, "dest").ok_or_else(|| anyhow!("missing dest"))?;
    let pull = st(a, "mode").as_deref() == Some("pull");
    let mut cmd = std::process::Command::new("rsync");
    cmd.arg("--itemize-changes");
    cmd.arg(if bo(a, "archive", true) {
        "-a"
    } else if bo(a, "recursive", true) {
        "-r"
    } else {
        "-d"
    });
    if bo(a, "delete", false) {
        cmd.arg("--delete");
    }
    if bo(a, "compress", true) {
        cmd.arg("-z");
    }
    if ctx.check {
        cmd.arg("-n");
    }
    if let Some(Value::Array(ex)) = a.get("rsync_opts") {
        for e in ex {
            if let Some(s) = e.as_str() {
                cmd.arg(s);
            }
        }
    }
    if let Some(Value::Array(ex)) = a.get("exclude").or(a.get("rsync_exclude")) {
        for e in ex {
            if let Some(s) = e.as_str() {
                cmd.arg(format!("--exclude={s}"));
            }
        }
    }
    if !ctx.conn.local {
        let mut e = String::from("ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new");
        if let Some(p) = ctx.conn.port {
            e += &format!(" -p {p}");
        }
        if let Some(k) = &ctx.conn.key {
            e += &format!(" -i {}", q(k));
        }
        cmd.args(["-e", &e]);
    }
    let remote = |p: &str| {
        if ctx.conn.local {
            p.to_string()
        } else {
            match &ctx.conn.user {
                Some(u) => format!("{u}@{}:{p}", ctx.conn.host),
                None => format!("{}:{p}", ctx.conn.host),
            }
        }
    };
    let (s, d) = if pull {
        (remote(&src), dest)
    } else {
        let p = if Path::new(&src).is_absolute() {
            src.clone()
        } else {
            ctx.play_dir.join(&src).display().to_string()
        };
        let p = if src.ends_with('/') && !p.ends_with('/') {
            format!("{p}/")
        } else {
            p
        };
        (p, remote(&dest))
    };
    cmd.arg(s).arg(d);
    let o = cmd
        .output()
        .map_err(|e| anyhow!("failed to run rsync: {e}"))?;
    if !o.status.success() {
        return Ok(fail(String::from_utf8_lossy(&o.stderr).trim().to_string()));
    }
    let out = String::from_utf8_lossy(&o.stdout);
    let changed = out.lines().any(|l| {
        l.len() > 11
            && !l.starts_with(".d")
            && !l.starts_with("sending")
            && !l.starts_with("total")
            && !l.starts_with("sent")
    });
    let mut r = res(changed);
    r.insert("stdout_lines".into(), lines(&out));
    let _ = json!(null);
    Ok(r)
}

fn pip(a: &Args, ctx: &Ctx) -> Result<Res> {
    let exe = st(a, "executable").unwrap_or_else(|| "pip3".into());
    let state = st(a, "state").unwrap_or_else(|| "present".into());
    let mut names = list(a, "name");
    if let Some(r) = st(a, "requirements") {
        let o = ctx
            .conn
            .exec(&format!("{} install -r {}", q(&exe), q(&r)))?;
        if o.rc != 0 {
            return Ok(fail(o.stderr.trim().to_string()));
        }
        return Ok(res(o.stdout.contains("Successfully installed")));
    }
    let ver = st(a, "version");
    if names.len() == 1
        && let Some(v) = &ver
    {
        names[0] = format!("{}=={}", names[0], v);
    }
    let mut changed = false;
    for n in &names {
        let base = n.split(|c| "=<>!~[ ".contains(c)).next().unwrap_or(n);
        let have = ctx
            .conn
            .exec(&format!("{} show {} 2>/dev/null", q(&exe), q(base)))?;
        let installed = have.rc == 0;
        let pinned_ok = match n.split_once("==") {
            Some((_, v)) => have
                .stdout
                .lines()
                .any(|l| l.trim() == format!("Version: {v}")),
            None => true,
        };
        let cmd = match state.as_str() {
            "absent" if installed => Some(format!("{} uninstall -y {}", q(&exe), q(base))),
            "latest" => Some(format!("{} install -U {}", q(&exe), q(n))),
            "present" | "forcereinstall" if !installed || !pinned_ok => {
                let extra = st(a, "extra_args")
                    .map(|e| format!(" {e}"))
                    .unwrap_or_default();
                Some(format!("{} install{extra} {}", q(&exe), q(n)))
            }
            _ => None,
        };
        if let Some(c) = cmd {
            if state == "latest" {
                if ctx.check {
                    continue;
                }
                let o = ctx.conn.exec(&c)?;
                if o.rc != 0 {
                    return Ok(fail(o.stderr.trim().to_string()));
                }
                changed |= o.stdout.contains("Successfully installed");
                continue;
            }
            changed = true;
            if !ctx.check {
                let o = ctx.conn.exec(&c)?;
                if o.rc != 0 {
                    return Ok(fail(format!("{}{}", o.stdout.trim(), o.stderr.trim())));
                }
            }
        }
    }
    Ok(res(changed))
}

fn unarchive(a: &Args, ctx: &Ctx) -> Result<Res> {
    let src = st(a, "src").ok_or_else(|| anyhow!("missing src"))?;
    let dest = st(a, "dest").ok_or_else(|| anyhow!("missing dest"))?;
    let remote = bo(a, "remote_src", false);
    if let Some(c) = st(a, "creates")
        && ctx.conn.exec(&format!("test -e {}", q(&c)))?.rc == 0
    {
        let mut r = res(false);
        r.insert("skipped".into(), true.into());
        return Ok(r);
    }
    if ctx.check {
        return Ok(res(true));
    }
    let flag = if src.ends_with(".zip") {
        ""
    } else if src.ends_with(".tar.gz") || src.ends_with(".tgz") {
        "z"
    } else if src.ends_with(".tar.bz2") {
        "j"
    } else if src.ends_with(".tar.xz") {
        "J"
    } else {
        ""
    };
    let strip = st(a, "extra_opts").unwrap_or_default();
    ctx.conn.exec(&format!("mkdir -p {}", q(&dest)))?;
    let (data, path);
    if remote {
        data = None;
        path = src.clone();
    } else {
        let p = find_src(ctx, "files", &src)?;
        data = Some(std::fs::read(&p)?);
        path = String::new();
    }
    let cmd = if src.ends_with(".zip") {
        if remote {
            format!("unzip -o -q {} -d {}", q(&path), q(&dest))
        } else {
            format!(
                "cat > /tmp/.ransible.zip && unzip -o -q /tmp/.ransible.zip -d {} && rm -f /tmp/.ransible.zip",
                q(&dest)
            )
        }
    } else if remote {
        format!("tar -x{flag}f {} -C {} {strip}", q(&path), q(&dest))
    } else {
        format!("tar -x{flag}f - -C {} {strip}", q(&dest))
    };
    let o = match data {
        Some(d) => ctx.conn.exec_stdin(&cmd, &d)?,
        None => ctx.conn.exec(&cmd)?,
    };
    if o.rc != 0 {
        return Ok(fail(o.stderr.trim().to_string()));
    }
    let mut r = res(true);
    r.insert("dest".into(), dest.clone().into());
    let ac = apply_attrs(a, ctx, &dest)?;
    let _ = ac;
    Ok(r)
}

fn hostname(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let cur = ctx.conn.exec("hostname")?.stdout.trim().to_string();
    if cur == name {
        return Ok(res(false));
    }
    if !ctx.check {
        let o = ctx.conn.exec(&format!(
            "hostname {n} && echo {n} > /etc/hostname",
            n = q(&name)
        ))?;
        if o.rc != 0 {
            return Ok(fail(o.stderr.trim().to_string()));
        }
    }
    Ok(res(true))
}

fn tempfile(a: &Args, ctx: &Ctx) -> Result<Res> {
    let state = st(a, "state").unwrap_or_else(|| "file".into());
    let suffix = st(a, "suffix").unwrap_or_default();
    let prefix = st(a, "prefix").unwrap_or_else(|| "ansible.".into());
    let base = st(a, "path").unwrap_or_else(|| "/tmp".into());
    let tmpl = format!("{}/{}XXXXXX{}", base.trim_end_matches('/'), prefix, suffix);
    let o = ctx.conn.exec(&format!(
        "mktemp {} {}",
        if state == "directory" { "-d" } else { "" },
        q(&tmpl)
    ))?;
    if o.rc != 0 {
        return Ok(fail(o.stderr.trim().to_string()));
    }
    let mut r = res(true);
    r.insert("path".into(), o.stdout.trim().into());
    Ok(r)
}

fn timezone(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    if ctx
        .conn
        .exec("command -v timedatectl >/dev/null && test -d /run/systemd/system")?
        .rc
        == 0
    {
        let cur = ctx
            .conn
            .exec("timedatectl show -p Timezone --value")?
            .stdout
            .trim()
            .to_string();
        if cur == name {
            return Ok(res(false));
        }
        if !ctx.check {
            let o = ctx
                .conn
                .exec(&format!("timedatectl set-timezone {}", q(&name)))?;
            if o.rc != 0 {
                return Ok(fail(o.stderr.trim().to_string()));
            }
        }
        return Ok(res(true));
    }
    let cur = ctx
        .conn
        .exec("readlink -f /etc/localtime | sed 's|.*/zoneinfo/||'")?
        .stdout
        .trim()
        .to_string();
    let want = ctx
        .conn
        .exec(&format!(
            "readlink -f /usr/share/zoneinfo/{} | sed 's|.*/zoneinfo/||'",
            q(&name)
        ))?
        .stdout
        .trim()
        .to_string();
    if cur == name || (!want.is_empty() && cur == want) {
        return Ok(res(false));
    }
    if !ctx.check {
        let o = ctx.conn.exec(&format!("test -f /usr/share/zoneinfo/{n} && ln -sf /usr/share/zoneinfo/{n} /etc/localtime && readlink -f /etc/localtime | sed 's|.*/zoneinfo/||' > /etc/timezone", n = q(&name)))?;
        if o.rc != 0 {
            return Ok(fail(format!("unknown timezone {name}")));
        }
    }
    Ok(res(true))
}

fn authorized_key(a: &Args, ctx: &Ctx) -> Result<Res> {
    let user = st(a, "user").ok_or_else(|| anyhow!("missing user"))?;
    let key = st(a, "key").ok_or_else(|| anyhow!("missing key"))?;
    let key = key.trim().to_string();
    let home = ctx
        .conn
        .exec(&format!("getent passwd {} | cut -d: -f6", q(&user)))?
        .stdout
        .trim()
        .to_string();
    if home.is_empty() {
        return Ok(fail(format!("user {user} does not exist")));
    }
    let path = st(a, "path").unwrap_or_else(|| format!("{home}/.ssh/authorized_keys"));
    let cur = ctx
        .conn
        .get_bytes(&path)?
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let absent = st(a, "state").as_deref() == Some("absent");
    let present = cur.lines().any(|l| l.trim() == key);
    if present != absent {
        return Ok(res(false));
    }
    if !ctx.check {
        let new = if absent {
            cur.lines()
                .filter(|l| l.trim() != key)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        } else {
            format!(
                "{}{}{key}\n",
                cur,
                if cur.is_empty() || cur.ends_with('\n') {
                    ""
                } else {
                    "\n"
                }
            )
        };
        let o = ctx.conn.exec(&format!(
            "install -d -m 700 -o {u} {d}",
            u = q(&user),
            d = q(&dir_of(&path))
        ))?;
        if o.rc != 0 {
            return Ok(fail(o.stderr.trim().to_string()));
        }
        ctx.conn.put(&path, new.as_bytes(), Some("600"))?;
        ctx.conn.exec(&format!("chown {} {}", q(&user), q(&path)))?;
    }
    Ok(res(true))
}

fn cron(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let user = st(a, "user");
    let absent = st(a, "state").as_deref() == Some("absent");
    let uflag = user
        .as_ref()
        .map(|u| format!("-u {}", q(u)))
        .unwrap_or_default();
    let cur = ctx
        .conn
        .exec(&format!("crontab -l {uflag} 2>/dev/null"))?
        .stdout;
    let marker = format!("#Ansible: {name}");
    let mut ls: Vec<String> = cur.lines().map(String::from).collect();
    let idx = ls.iter().position(|l| *l == marker);
    let g = |k: &str, d: &str| st(a, k).unwrap_or_else(|| d.to_string());
    let disabled = bo(a, "disabled", false);
    let job = st(a, "job").unwrap_or_default();
    let line = if let Some(sp) = st(a, "special_time") {
        format!("{}@{sp} {job}", if disabled { "#" } else { "" })
    } else {
        format!(
            "{}{} {} {} {} {} {job}",
            if disabled { "#" } else { "" },
            g("minute", "*"),
            g("hour", "*"),
            g("day", "*"),
            g("month", "*"),
            g("weekday", "*")
        )
    };
    let orig = ls.clone();
    match (idx, absent) {
        (Some(i), true) => {
            ls.drain(i..(i + 2).min(ls.len()));
        }
        (Some(i), false) => {
            if i + 1 < ls.len() {
                ls[i + 1] = line
            } else {
                ls.push(line)
            }
        }
        (None, true) => {}
        (None, false) => {
            ls.push(marker);
            ls.push(line);
        }
    }
    if ls == orig {
        return Ok(res(false));
    }
    if !ctx.check {
        let o = ctx.conn.exec_stdin(
            &format!("crontab {uflag} -"),
            (ls.join("\n") + "\n").as_bytes(),
        )?;
        if o.rc != 0 {
            return Ok(fail(o.stderr.trim().to_string()));
        }
    }
    Ok(res(true))
}

fn service_facts(ctx: &Ctx) -> Result<Res> {
    let mut svcs = Map::new();
    let systemd = ctx
        .conn
        .exec("command -v systemctl >/dev/null && test -d /run/systemd/system")?
        .rc
        == 0;
    if systemd {
        let o = ctx
            .conn
            .exec("systemctl list-units --no-pager --no-legend --all --type=service")?;
        for l in o.stdout.lines() {
            let l = l.trim_start_matches(|c: char| !c.is_alphanumeric());
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 4 {
                continue;
            }
            let state = match f[3] {
                "running" => "running",
                "exited" | "dead" => "stopped",
                s => s,
            };
            let name = f[0].to_string();
            let en = ctx
                .conn
                .exec(&format!("systemctl is-enabled {} 2>/dev/null", q(&name)))?
                .stdout
                .trim()
                .to_string();
            svcs.insert(name.clone(), json!({"name": name, "state": state, "status": if en.is_empty() { "unknown".into() } else { en }, "source": "systemd"}));
        }
    } else {
        let o = ctx.conn.exec("for s in /etc/init.d/*; do [ -x \"$s\" ] && n=$(basename $s) && { service $n status >/dev/null 2>&1 && echo \"$n running\" || echo \"$n stopped\"; }; done")?;
        for l in o.stdout.lines() {
            let mut it = l.split_whitespace();
            if let (Some(n), Some(s)) = (it.next(), it.next()) {
                svcs.insert(
                    n.to_string(),
                    json!({"name": n, "state": s, "source": "sysv"}),
                );
            }
        }
    }
    let mut f = Map::new();
    f.insert("services".into(), Value::Object(svcs));
    let mut r = res(false);
    r.insert("ansible_facts".into(), Value::Object(f));
    Ok(r)
}

fn package_facts(ctx: &Ctx) -> Result<Res> {
    let (cmd, mgr) = if ctx.conn.exec("command -v dpkg-query")?.rc == 0 {
        (
            "dpkg-query -W -f='${Package}\\t${Version}\\t${Architecture}\\n'",
            "apt",
        )
    } else {
        (
            "rpm -qa --qf '%{NAME}\\t%{VERSION}-%{RELEASE}\\t%{ARCH}\\n'",
            "rpm",
        )
    };
    let o = ctx.conn.exec(cmd)?;
    let mut pk: Map<String, Value> = Map::new();
    for l in o.stdout.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() < 3 {
            continue;
        }
        let e = json!({"name": f[0], "version": f[1], "arch": f[2], "source": mgr});
        pk.entry(f[0].to_string())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .unwrap()
            .push(e);
    }
    let mut f = Map::new();
    f.insert("packages".into(), Value::Object(pk));
    let mut r = res(false);
    r.insert("ansible_facts".into(), Value::Object(f));
    Ok(r)
}

fn getent(a: &Args, ctx: &Ctx) -> Result<Res> {
    let db = st(a, "database").ok_or_else(|| anyhow!("missing database"))?;
    let key = st(a, "key").unwrap_or_default();
    let split = st(a, "split").unwrap_or_else(|| ":".into());
    let o = ctx.conn.exec(&format!("getent {} {}", q(&db), q(&key)))?;
    if o.rc != 0 {
        return Ok(if bo(a, "fail_key", true) {
            fail("One or more supplied key could not be found in the database.".to_string())
        } else {
            res(false)
        });
    }
    let mut m = Map::new();
    for l in o.stdout.lines() {
        let f: Vec<Value> = l
            .split(split.as_str())
            .map(|x| Value::String(x.to_string()))
            .collect();
        if let Some(k) = f.first().and_then(|k| k.as_str()) {
            m.insert(k.to_string(), Value::Array(f[1..].to_vec()));
        }
    }
    let mut facts = Map::new();
    facts.insert(format!("getent_{db}"), Value::Object(m));
    let mut r = res(false);
    r.insert("ansible_facts".into(), Value::Object(facts));
    Ok(r)
}

fn deb822_repository(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let path = format!("/etc/apt/sources.list.d/{name}.sources");
    if st(a, "state").as_deref() == Some("absent") {
        let ex = ctx.conn.exec(&format!("test -e {}", q(&path)))?.rc == 0;
        if ex && !ctx.check {
            ctx.conn.exec(&format!("rm -f {}", q(&path)))?;
        }
        return Ok(res(ex));
    }
    let join = |k: &str| list(a, k).join(" ");
    let mut text = String::new();
    let types = if list(a, "types").is_empty() {
        "deb".to_string()
    } else {
        join("types")
    };
    text.push_str(&format!("Types: {types}\n"));
    let mut put = |k: &str, v: String| {
        if !v.is_empty() {
            text.push_str(&format!("{k}: {v}\n"));
        }
    };
    put("URIs", join("uris"));
    put("Suites", join("suites"));
    put("Components", join("components"));
    put("Architectures", join("architectures"));
    if !bo(a, "enabled", true) {
        put("Enabled", "no".into());
    }
    let mut changed = false;
    if let Some(sb) = st(a, "signed_by") {
        let keyfile = if sb.starts_with("http://") || sb.starts_with("https://") {
            let kf = format!("/etc/apt/keyrings/{name}.asc");
            let have = ctx.conn.exec(&format!("test -s {}", q(&kf)))?.rc == 0;
            if !have {
                changed = true;
                if !ctx.check {
                    let o = ctx.conn.exec(&format!(
                        "mkdir -p /etc/apt/keyrings && curl -fsSL -o {} {}",
                        q(&kf),
                        q(&sb)
                    ))?;
                    if o.rc != 0 {
                        return Ok(fail(format!("failed to fetch key: {}", o.stderr.trim())));
                    }
                }
            }
            kf
        } else if sb.contains("BEGIN PGP") {
            let kf = format!("/etc/apt/keyrings/{name}.asc");
            let cur = ctx.conn.get_bytes(&kf)?;
            if cur.as_deref() != Some(sb.as_bytes()) {
                changed = true;
                if !ctx.check {
                    ctx.conn.exec("mkdir -p /etc/apt/keyrings")?;
                    ctx.conn.put(&kf, sb.as_bytes(), Some("644"))?;
                }
            }
            kf
        } else {
            sb
        };
        text.push_str(&format!("Signed-By: {keyfile}\n"));
    }
    let mut r = put_content(&Map::new(), ctx, &path, text.as_bytes())?;
    if changed {
        r.insert("changed".into(), true.into());
    }
    Ok(r)
}

fn apt_repository(a: &Args, ctx: &Ctx) -> Result<Res> {
    let repo = st(a, "repo").ok_or_else(|| anyhow!("missing repo"))?;
    let fname = st(a, "filename").unwrap_or_else(|| {
        repo.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
            .trim_matches('_')
            .to_string()
    });
    let path = format!("/etc/apt/sources.list.d/{fname}.list");
    if st(a, "state").as_deref() == Some("absent") {
        let ex = ctx.conn.exec(&format!("test -e {}", q(&path)))?.rc == 0;
        if ex && !ctx.check {
            ctx.conn.exec(&format!("rm -f {}", q(&path)))?;
        }
        return Ok(res(ex));
    }
    let repo = repo.trim().to_string();
    let r = put_content(&Map::new(), ctx, &path, format!("{repo}\n").as_bytes())?;
    if r.get("changed").and_then(|c| c.as_bool()) == Some(true)
        && bo(a, "update_cache", true)
        && !ctx.check
    {
        ctx.conn.exec("apt-get update -qq")?;
    }
    Ok(r)
}

fn apt_key(a: &Args, ctx: &Ctx) -> Result<Res> {
    let data = st(a, "data");
    let url = st(a, "url");
    let id = st(a, "id").unwrap_or_default();
    if !id.is_empty() {
        let o = ctx.conn.exec(&format!(
            "apt-key list 2>/dev/null | tr -d ' ' | grep -qi {}",
            q(&id)
        ))?;
        if o.rc == 0 && st(a, "state").as_deref() != Some("absent") {
            return Ok(res(false));
        }
    }
    if ctx.check {
        return Ok(res(true));
    }
    let o = match (data, url) {
        (Some(d), _) => ctx.conn.exec_stdin("apt-key add -", d.as_bytes())?,
        (None, Some(u)) => ctx
            .conn
            .exec(&format!("curl -fsSL {} | apt-key add -", q(&u)))?,
        _ => return Ok(fail("one of data or url required")),
    };
    if o.rc != 0 {
        return Ok(fail(o.stderr.trim().to_string()));
    }
    Ok(res(true))
}

fn locale_gen(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let absent = st(a, "state").as_deref() == Some("absent");
    let norm = name.to_lowercase().replace("utf-8", "utf8");
    let have = ctx
        .conn
        .exec("locale -a")?
        .stdout
        .lines()
        .any(|l| l.trim().to_lowercase().replace("utf-8", "utf8") == norm);
    if have != absent {
        return Ok(res(false));
    }
    if !ctx.check {
        let cmd = if absent {
            format!(
                "sed -i 's/^{n}/# {n}/' /etc/locale.gen && locale-gen",
                n = name.replace('.', "\\.")
            )
        } else {
            format!(
                "(grep -qE '^# *{n}' /etc/locale.gen && sed -i -E 's/^# *({n})/\\1/' /etc/locale.gen || grep -qE '^{n}' /etc/locale.gen || echo '{full} UTF-8' >> /etc/locale.gen) && locale-gen",
                n = name.replace('.', "\\."),
                full = name.trim_end_matches(" UTF-8")
            )
        };
        let o = ctx.conn.exec(&cmd)?;
        if o.rc != 0 {
            return Ok(fail(format!("locale-gen failed: {}", o.stderr.trim())));
        }
    }
    Ok(res(true))
}

fn find(a: &Args, ctx: &Ctx) -> Result<Res> {
    let paths = list(a, "paths");
    let patterns = list(a, "patterns");
    let ftype = st(a, "file_type").unwrap_or_else(|| "file".into());
    let use_regex = bo(a, "use_regex", false);
    let mut cmd = String::new();
    for p in &paths {
        let mut c = format!("find {}", q(p));
        if !bo(a, "recurse", false) {
            c += " -maxdepth 1 -mindepth 1";
        } else {
            c += " -mindepth 1";
        }
        match ftype.as_str() {
            "file" => c += " -type f",
            "directory" => c += " -type d",
            "link" => c += " -type l",
            _ => {}
        }
        if let Some(age) = st(a, "age") {
            let (neg, rest) = if let Some(r) = age.strip_prefix('-') {
                (true, r.to_string())
            } else {
                (false, age.clone())
            };
            let (n, unit) = rest.split_at(rest.len().saturating_sub(1));
            let secs: i64 = n.parse::<i64>().unwrap_or(0)
                * match unit {
                    "m" => 60,
                    "h" => 3600,
                    "d" => 86400,
                    "w" => 604800,
                    _ => 1,
                };
            c += &format!(" -newermt '-{secs} seconds'").replace(
                " -newermt '-",
                if neg {
                    " -newermt '-"
                } else {
                    " ! -newermt '-"
                },
            );
        }
        c += " -print0";
        cmd += &format!("{c} 2>/dev/null; ");
    }
    let o = ctx.conn.exec(&cmd)?;
    let re_list: Vec<regex::Regex> = patterns
        .iter()
        .filter_map(|p| {
            let pat = if use_regex {
                p.clone()
            } else {
                format!(
                    "^{}$",
                    regex::escape(p).replace("\\*", ".*").replace("\\?", ".")
                )
            };
            regex::Regex::new(&pat).ok()
        })
        .collect();
    let excl: Vec<regex::Regex> = list(a, "excludes")
        .iter()
        .filter_map(|p| {
            regex::Regex::new(&format!(
                "^{}$",
                regex::escape(p).replace("\\*", ".*").replace("\\?", ".")
            ))
            .ok()
        })
        .collect();
    let hidden = bo(a, "hidden", false);
    let mut files = vec![];
    let mut examined = 0;
    for p in o.stdout.split('\0').filter(|x| !x.is_empty()) {
        examined += 1;
        let base = p.rsplit('/').next().unwrap_or(p);
        if !hidden && base.starts_with('.') {
            continue;
        }
        if !re_list.is_empty() && !re_list.iter().any(|r| r.is_match(base)) {
            continue;
        }
        if excl.iter().any(|r| r.is_match(base)) {
            continue;
        }
        if let Some(c) = st(a, "contains")
            && (ftype != "file"
                || ctx
                    .conn
                    .exec(&format!(
                        "grep -qE {} {}",
                        q(&c.replace("\\s", "[[:space:]]")),
                        q(p)
                    ))?
                    .rc
                    != 0)
        {
            continue;
        }
        let info = attrs(ctx.conn, p);
        files.push(json!({"path": p, "mode": info.as_ref().map(|i| format!("{:0>4}", i.0)), "pw_name": info.as_ref().map(|i| i.1.clone()), "gr_name": info.as_ref().map(|i| i.2.clone()), "isdir": ftype == "directory"}));
    }
    files.sort_by(|x, y| x["path"].as_str().cmp(&y["path"].as_str()));
    let mut r = res(false);
    r.insert("matched".into(), files.len().into());
    r.insert("examined".into(), examined.into());
    r.insert("files".into(), Value::Array(files));
    Ok(r)
}

/// Fire-and-forget execution of command/shell (`async: N`, `poll: 0`).
pub fn async_start(module: &str, a: &Args, ctx: &Ctx, secs: u64) -> Result<Res> {
    let raw = st_any(a, &["_raw_params", "cmd"]).ok_or_else(|| anyhow!("no command given"))?;
    let cmd = if module == "command" {
        split_args(&raw)
            .iter()
            .map(|x| q(x))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        raw
    };
    let cmd = match st(a, "chdir") {
        Some(d) => format!("cd {} && {cmd}", q(&d)),
        None => cmd,
    };
    let jid = format!(
        "{}.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        std::process::id()
    );
    let dir = "$HOME/.ansible_async";
    if ctx.check {
        return Ok(res(false));
    }
    let script = format!(
        "mkdir -p {dir} && F={dir}/{jid} && (nohup sh -c {inner} >/dev/null 2>&1 &) ; echo started",
        inner = q(&format!(
            "timeout {secs} sh -c {c} >$HOME/.ansible_async/{jid}.out 2>$HOME/.ansible_async/{jid}.err; echo $? >$HOME/.ansible_async/{jid}.rc",
            c = q(&cmd)
        ))
    );
    let o = ctx.conn.exec(&script)?;
    if o.rc != 0 {
        return Ok(fail(o.stderr.trim().to_string()));
    }
    let mut r = res(true);
    r.insert("ansible_job_id".into(), jid.into());
    r.insert("started".into(), 1.into());
    r.insert("finished".into(), 0.into());
    Ok(r)
}

fn async_status(a: &Args, ctx: &Ctx) -> Result<Res> {
    let jid = st(a, "jid").ok_or_else(|| anyhow!("missing jid"))?;
    let base = format!("$HOME/.ansible_async/{jid}");
    let rc = ctx.conn.exec(&format!("cat {base}.rc 2>/dev/null"))?;
    let mut r = res(false);
    r.insert("ansible_job_id".into(), jid.clone().into());
    if rc.rc != 0 || rc.stdout.trim().is_empty() {
        r.insert("finished".into(), 0.into());
        r.insert("started".into(), 1.into());
        return Ok(r);
    }
    let out = ctx.conn.exec(&format!("cat {base}.out; "))?.stdout;
    let err = ctx.conn.exec(&format!("cat {base}.err"))?.stdout;
    let code: i64 = rc.stdout.trim().parse().unwrap_or(-1);
    r.insert("finished".into(), 1.into());
    r.insert("rc".into(), code.into());
    r.insert("stdout".into(), out.trim_end().into());
    r.insert("stderr".into(), err.trim_end().into());
    r.insert("stdout_lines".into(), lines(&out));
    r.insert("stderr_lines".into(), lines(&err));
    r.insert("changed".into(), true.into());
    if code != 0 {
        r.insert("failed".into(), true.into());
        r.insert("msg".into(), "non-zero return code".into());
    }
    Ok(r)
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
