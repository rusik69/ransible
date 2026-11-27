//! Additional built-in modules (system, packaging, config-file helpers).
use crate::modules::{
    Args, Ctx, Res, apply_attrs, attrs as _attrs, bo, dir_of, fail, list, put_content, read_text,
    res, st, write_text,
};
use crate::template::shell_quote as q;
use anyhow::{Result, anyhow};
use serde_json::Value;

pub fn run(module: &str, a: &Args, ctx: &Ctx) -> Result<Res> {
    let _ = _attrs;
    match module {
        "ini_file" => ini_file(a, ctx),
        "sysctl" => sysctl(a, ctx),
        "known_hosts" => known_hosts(a, ctx),
        "reboot" => reboot(a, ctx),
        "set_stats" => Ok(res(false)),
        "assemble" => assemble(a, ctx),
        "archive" => archive(a, ctx),
        "npm" => npm(a, ctx),
        "gem" => gem(a, ctx),
        "debconf" => debconf(a, ctx),
        "ufw" => ufw(a, ctx),
        "yum_repository" => yum_repository(a, ctx),
        "alternatives" => alternatives(a, ctx),
        "mount" => mount(a, ctx),
        "snap" => snap(a, ctx),
        "firewalld" => firewalld(a, ctx),
        "rpm_key" => rpm_key(a, ctx),
        "modprobe" => modprobe(a, ctx),
        "selinux" => selinux(a, ctx),
        "git_config" => git_config(a, ctx),
        other => Err(anyhow!("unsupported module '{other}'")),
    }
}

fn run_sh(ctx: &Ctx, cmd: &str) -> Result<crate::conn::Output> {
    ctx.conn.exec(cmd)
}

fn ok_or_fail(o: crate::conn::Output, what: &str) -> Option<Res> {
    if o.rc != 0 {
        Some(fail(format!(
            "{what} failed: {}{}",
            o.stdout.trim(),
            o.stderr.trim()
        )))
    } else {
        None
    }
}

// ---------- ini_file ----------

/// Edit INI text like Ansible's `ini_file`; returns the new text and Ansible's `msg`.
/// `values`: the option's value lines (empty = no value). `new_file`: the file did not exist.
pub fn ini_edit(
    text: &str,
    section: &str,
    option: Option<&str>,
    values: &[String],
    absent: bool,
    no_extra_spaces: bool,
    new_file: bool,
) -> (String, &'static str) {
    let mut ls: Vec<String> = text.lines().map(String::from).collect();
    if new_file || (ls.is_empty() && !section.is_empty()) {
        // Ansible starts a fresh file with a blank line before the first section
        ls.push(String::new());
    }
    let is_header = |l: &str| l.trim().starts_with('[') && l.trim().ends_with(']');
    let header_of = |l: &str| l.trim()[1..l.trim().len() - 1].to_string();
    let mut start: Option<usize> = if section.is_empty() { Some(0) } else { None };
    if !section.is_empty() {
        for (i, l) in ls.iter().enumerate() {
            if is_header(l) && header_of(l) == section {
                start = Some(i + 1);
                break;
            }
        }
    }
    let end = start
        .map(|s| {
            ls.iter()
                .enumerate()
                .skip(s)
                .find(|(_, l)| is_header(l))
                .map(|(i, _)| i)
                .unwrap_or(ls.len())
        })
        .unwrap_or(ls.len());
    let line_for = |opt: &str, v: Option<&String>| match v {
        Some(v) if no_extra_spaces => format!("{opt}={v}"),
        Some(v) => format!("{opt} = {v}"),
        None => opt.to_string(),
    };
    let msg: &'static str;
    match (option, absent) {
        (None, true) => match start {
            Some(s) if !section.is_empty() => {
                ls.drain(s - 1..end);
                msg = "section removed";
            }
            _ => msg = "OK",
        },
        (Some(opt), true) => {
            let mut removed = false;
            if let Some(s) = start {
                let mut i = s;
                let mut e = end;
                while i < e.min(ls.len()) {
                    if line_has_option(&ls[i], opt) {
                        ls.remove(i);
                        e -= 1;
                        removed = true;
                    } else {
                        i += 1;
                    }
                }
            }
            msg = if removed { "option changed" } else { "OK" };
        }
        (Some(opt), false) => {
            let wanted: Vec<String> = if values.is_empty() {
                vec![line_for(opt, None)]
            } else {
                values.iter().map(|v| line_for(opt, Some(v))).collect()
            };
            match start {
                Some(s) => {
                    let existing: Vec<usize> = (s..end.min(ls.len()))
                        .filter(|&i| line_has_option(&ls[i], opt))
                        .collect();
                    if existing.is_empty() {
                        let mut at = end.min(ls.len());
                        while at > s && ls[at - 1].trim().is_empty() {
                            at -= 1;
                        }
                        for (k, w) in wanted.into_iter().enumerate() {
                            ls.insert(at + k, w);
                        }
                        msg = "option added";
                    } else {
                        let cur: Vec<&String> = existing.iter().map(|&i| &ls[i]).collect();
                        if cur.len() == wanted.len()
                            && cur.iter().zip(&wanted).all(|(a, b)| a.trim() == b.trim())
                        {
                            msg = "OK";
                        } else {
                            let at = existing[0];
                            for &i in existing.iter().rev() {
                                ls.remove(i);
                            }
                            for (k, w) in wanted.into_iter().enumerate() {
                                ls.insert(at + k, w);
                            }
                            msg = "option changed";
                        }
                    }
                }
                None => {
                    ls.push(format!("[{section}]"));
                    ls.extend(wanted);
                    msg = "section and option added";
                }
            }
        }
        (None, false) => {
            if start.is_none() {
                ls.push(format!("[{section}]"));
                msg = "section added";
            } else {
                msg = "OK";
            }
        }
    }
    let mut out = ls.join("\n");
    if !ls.is_empty() {
        out.push('\n');
    }
    (out, msg)
}

#[cfg(test)]
pub fn ini_set(
    text: &str,
    section: &str,
    option: Option<&str>,
    value: Option<&str>,
    absent: bool,
    no_extra_spaces: bool,
) -> String {
    let vals: Vec<String> = value.map(|v| vec![v.to_string()]).unwrap_or_default();
    ini_edit(text, section, option, &vals, absent, no_extra_spaces, false).0
}

fn line_has_option(l: &str, opt: &str) -> bool {
    let t = l.trim_start();
    if t.starts_with('#') || t.starts_with(';') {
        return false;
    }
    let key = t.split(['=', ':']).next().unwrap_or("").trim();
    key == opt
}

fn ini_file(a: &Args, ctx: &Ctx) -> Result<Res> {
    let path = st(a, "path")
        .or_else(|| st(a, "dest"))
        .ok_or_else(|| anyhow!("missing path"))?;
    let create = bo(a, "create", true);
    let old = read_text(ctx, &path, true)?;
    if old.is_none() && !create {
        return Ok(fail(format!("Destination {path} does not exist!")));
    }
    let text = old.clone().unwrap_or_default();
    let absent = st(a, "state").as_deref() == Some("absent");
    let as_s = |v: &Value| match v {
        Value::String(s) => s.clone(),
        o => o.to_string(),
    };
    let mut values: Vec<String> = match a.get("values") {
        Some(Value::Array(v)) => v.iter().map(as_s).collect(),
        _ => vec![],
    };
    if values.is_empty()
        && let Some(v) = a.get("value").filter(|v| !v.is_null())
    {
        values.push(as_s(v));
    }
    let option = st(a, "option");
    if !absent && option.is_some() && values.is_empty() && !bo(a, "allow_no_value", false) {
        return Ok(fail(
            "Parameter 'value(s)' must be defined if state=present and allow_no_value=False.",
        ));
    }
    let (new, msg) = ini_edit(
        &text,
        &st(a, "section").unwrap_or_default(),
        option.as_deref(),
        &values,
        absent,
        bo(a, "no_extra_spaces", false),
        old.is_none(),
    );
    let mut r0 = res(false);
    r0.insert("msg".into(), msg.into());
    write_text(a, ctx, &path, old.as_deref(), &new, r0)
}

// ---------- sysctl ----------

fn sysctl(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let file = st(a, "sysctl_file").unwrap_or_else(|| "/etc/sysctl.conf".into());
    let absent = st(a, "state").as_deref() == Some("absent");
    let value = st(a, "value").unwrap_or_default();
    let old = read_text(ctx, &file, true)?;
    let mut ls: Vec<String> = old
        .clone()
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect();
    let idx = ls.iter().position(|l| {
        let t = l.trim_start();
        !t.starts_with('#')
            && t.split('=')
                .next()
                .map(|k| k.trim() == name)
                .unwrap_or(false)
    });
    match (idx, absent) {
        (Some(i), true) => {
            ls.remove(i);
        }
        (Some(i), false) => ls[i] = format!("{name}={value}"),
        (None, false) => ls.push(format!("{name}={value}")),
        _ => {}
    }
    let new = if ls.is_empty() {
        String::new()
    } else {
        ls.join("\n") + "\n"
    };
    let mut r = write_text(a, ctx, &file, old.as_deref(), &new, res(false))?;
    if !absent && !ctx.check && bo(a, "sysctl_set", false) {
        let cur = run_sh(ctx, &format!("sysctl -n {}", q(&name)))?
            .stdout
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if cur != value.split_whitespace().collect::<Vec<_>>().join(" ") {
            if let Some(f) = ok_or_fail(
                run_sh(ctx, &format!("sysctl -w {}={}", q(&name), q(&value)))?,
                "sysctl -w",
            ) {
                return Ok(f);
            }
            r.insert("changed".into(), true.into());
        }
    } else if r.get("changed") == Some(&Value::Bool(true)) && bo(a, "reload", true) && !ctx.check {
        let _ = run_sh(ctx, &format!("sysctl -p {}", q(&file)))?;
    }
    Ok(r)
}

// ---------- known_hosts ----------

fn known_hosts(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let path = st(a, "path").unwrap_or_else(|| "~/.ssh/known_hosts".into());
    let path = if let Some(rest) = path.strip_prefix("~/") {
        let h = run_sh(ctx, "echo $HOME")?.stdout.trim().to_string();
        format!("{h}/{rest}")
    } else {
        path
    };
    let old = read_text(ctx, &path, true)?;
    let mut ls: Vec<String> = old
        .clone()
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect();
    let present = |l: &String| {
        l.split_whitespace()
            .next()
            .map(|h| h.split(',').any(|x| x == name))
            .unwrap_or(false)
    };
    if st(a, "state").as_deref() == Some("absent") {
        ls.retain(|l| !present(l));
    } else {
        let key = st(a, "key").ok_or_else(|| anyhow!("missing key"))?;
        let want = if key.starts_with(&name) {
            key.clone()
        } else {
            format!("{name} {}", key.trim())
        };
        match ls.iter().position(present) {
            Some(i) => ls[i] = want,
            None => ls.push(want),
        }
    }
    let new = if ls.is_empty() {
        String::new()
    } else {
        ls.join("\n") + "\n"
    };
    if !ctx.check {
        let _ = run_sh(ctx, &format!("mkdir -p {}", q(&dir_of(&path))));
    }
    write_text(a, ctx, &path, old.as_deref(), &new, res(false))
}

// ---------- reboot ----------

fn reboot(a: &Args, ctx: &Ctx) -> Result<Res> {
    if ctx.check {
        return Ok(res(true));
    }
    let boot = || {
        run_sh(
            ctx,
            "cat /proc/sys/kernel/random/boot_id 2>/dev/null || sysctl -n kern.boottime",
        )
        .map(|o| o.stdout.trim().to_string())
    };
    let before = boot()?;
    let msg = st(a, "msg").unwrap_or_else(|| "Reboot initiated by ransible".into());
    let cmd = st(a, "reboot_command").unwrap_or_else(|| format!("shutdown -r now {}", q(&msg)));
    let t = a
        .get("pre_reboot_delay")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let _ = run_sh(
        ctx,
        &format!(
            "nohup sh -c 'sleep {} ; {}' >/dev/null 2>&1 &",
            t.max(1),
            cmd.replace('\'', "'\"'\"'")
        ),
    );
    let timeout = a
        .get("reboot_timeout")
        .and_then(|v| v.as_u64())
        .unwrap_or(600);
    let start = std::time::Instant::now();
    std::thread::sleep(std::time::Duration::from_secs(t.max(1) + 3));
    loop {
        if let Ok(now) = boot()
            && !now.is_empty()
            && now != before
        {
            let delay = a
                .get("post_reboot_delay")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            std::thread::sleep(std::time::Duration::from_secs(delay));
            if let Some(tc) = st(a, "test_command")
                && run_sh(ctx, &tc).map(|o| o.rc != 0).unwrap_or(true)
            {
                std::thread::sleep(std::time::Duration::from_secs(5));
                continue;
            }
            let mut r = res(true);
            r.insert("rebooted".into(), true.into());
            r.insert("elapsed".into(), (start.elapsed().as_secs() as i64).into());
            return Ok(r);
        }
        if start.elapsed().as_secs() > timeout {
            return Ok(fail(format!(
                "Timed out waiting for last boot time check (timeout={timeout})"
            )));
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

// ---------- assemble / archive ----------

fn assemble(a: &Args, ctx: &Ctx) -> Result<Res> {
    let src = st(a, "src").ok_or_else(|| anyhow!("missing src"))?;
    let dest = st(a, "dest").ok_or_else(|| anyhow!("missing dest"))?;
    let re = st(a, "regexp").map(|r| regex::Regex::new(&r)).transpose()?;
    let delim = st(a, "delimiter").unwrap_or_default();
    let names = run_sh(ctx, &format!("ls -1 {} | LC_ALL=C sort", q(&src)))?;
    let mut out: Vec<u8> = vec![];
    let mut first = true;
    for f in names.stdout.lines() {
        if let Some(r) = &re
            && !r.is_match(f)
        {
            continue;
        }
        if let Some(b) = ctx
            .conn
            .get_bytes(&format!("{}/{f}", src.trim_end_matches('/')))?
        {
            if !first && !delim.is_empty() {
                out.extend(delim.as_bytes());
                out.push(b'\n');
            }
            out.extend(&b);
            if !b.ends_with(b"\n") && bo(a, "ignore_hidden", false) {
                out.push(b'\n');
            }
            first = false;
        }
    }
    put_content(a, ctx, &dest, &out)
}

fn archive(a: &Args, ctx: &Ctx) -> Result<Res> {
    let paths = list(a, "path");
    let dest = st(a, "dest").ok_or_else(|| anyhow!("missing dest"))?;
    let fmt = st(a, "format").unwrap_or_else(|| "gz".into());
    if run_sh(ctx, &format!("test -e {}", q(&dest)))?.rc == 0
        && !bo(a, "force", false)
        && st(a, "remove").is_none()
    {
        return Ok(res(false));
    }
    if ctx.check {
        return Ok(res(true));
    }
    let srcs = paths.iter().map(|p| q(p)).collect::<Vec<_>>().join(" ");
    let cmd = match fmt.as_str() {
        "zip" => format!("zip -qr {} {srcs}", q(&dest)),
        "tar" => format!("tar -cf {} {srcs}", q(&dest)),
        "bz2" => format!("tar -cjf {} {srcs}", q(&dest)),
        "xz" => format!("tar -cJf {} {srcs}", q(&dest)),
        _ => format!("tar -czf {} {srcs}", q(&dest)),
    };
    if let Some(f) = ok_or_fail(run_sh(ctx, &cmd)?, "archive") {
        return Ok(f);
    }
    if bo(a, "remove", false) {
        let _ = run_sh(ctx, &format!("rm -rf {srcs}"));
    }
    Ok(res(true))
}

// ---------- language package managers ----------

fn npm(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name");
    let exe = st(a, "executable").unwrap_or_else(|| "npm".into());
    let global = if bo(a, "global", false) { " -g" } else { "" };
    let pre = st(a, "path")
        .map(|p| format!("cd {} && ", q(&p)))
        .unwrap_or_default();
    let state = st(a, "state").unwrap_or_else(|| "present".into());
    let Some(name) = name else {
        if ctx.check {
            return Ok(res(true));
        }
        let o = run_sh(ctx, &format!("{pre}{} install", q(&exe)))?;
        return Ok(ok_or_fail(o.clone(), "npm install")
            .unwrap_or_else(|| res(!o.stdout.contains("up to date"))));
    };
    let spec = match st(a, "version") {
        Some(v) => format!("{name}@{v}"),
        None => name.clone(),
    };
    let have = run_sh(
        ctx,
        &format!(
            "{pre}{} ls{global} --depth=0 {} 2>/dev/null",
            q(&exe),
            q(&name)
        ),
    )?;
    let installed = have.rc == 0 && have.stdout.contains(&name);
    let version_ok = st(a, "version")
        .map(|v| have.stdout.contains(&format!("{name}@{v}")))
        .unwrap_or(true);
    let cmd = match state.as_str() {
        "absent" if installed => Some(format!("{pre}{} uninstall{global} {}", q(&exe), q(&name))),
        "latest" => Some(format!(
            "{pre}{} install{global} {}@latest",
            q(&exe),
            q(&name)
        )),
        "present" if !installed || !version_ok => {
            Some(format!("{pre}{} install{global} {}", q(&exe), q(&spec)))
        }
        _ => None,
    };
    match cmd {
        Some(c) => {
            if !ctx.check
                && let Some(f) = ok_or_fail(run_sh(ctx, &c)?, "npm")
            {
                return Ok(f);
            }
            Ok(res(state != "latest" || !ctx.check))
        }
        None => Ok(res(false)),
    }
}

fn gem(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let exe = st(a, "executable").unwrap_or_else(|| "gem".into());
    let state = st(a, "state").unwrap_or_else(|| "present".into());
    let ver = st(a, "version");
    let vflag = ver
        .as_ref()
        .map(|v| format!(" -v {}", q(v)))
        .unwrap_or_default();
    let installed = run_sh(ctx, &format!("{} list -i{vflag} {}", q(&exe), q(&name)))?
        .stdout
        .trim()
        == "true";
    let user = if bo(a, "user_install", true) && !ctx.conn.become_user.is_some() {
        " --user-install"
    } else {
        ""
    };
    let cmd = match state.as_str() {
        "absent" if installed => Some(format!("{} uninstall -x -a {}{vflag}", q(&exe), q(&name))),
        "present" | "latest" if !installed || state == "latest" => Some(format!(
            "{} install --no-document{user}{vflag} {}",
            q(&exe),
            q(&name)
        )),
        _ => None,
    };
    match cmd {
        Some(c) => {
            if !ctx.check
                && let Some(f) = ok_or_fail(run_sh(ctx, &c)?, "gem")
            {
                return Ok(f);
            }
            Ok(res(true))
        }
        None => Ok(res(false)),
    }
}

fn debconf(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let (Some(question), Some(vtype)) = (st(a, "question"), st(a, "vtype")) else {
        return Ok(res(false));
    };
    let value = st(a, "value").unwrap_or_default();
    let cur = run_sh(
        ctx,
        &format!(
            "debconf-show {} 2>/dev/null | grep -F ' {question}:' | sed 's/^[* ]*[^:]*: *//'",
            q(&name)
        ),
    )?
    .stdout
    .trim()
    .to_string();
    if cur == value {
        return Ok(res(false));
    }
    if !ctx.check {
        let o = ctx.conn.exec_stdin(
            "debconf-set-selections",
            format!("{name} {question} {vtype} {value}\n").as_bytes(),
        )?;
        if let Some(f) = ok_or_fail(o, "debconf-set-selections") {
            return Ok(f);
        }
    }
    Ok(res(true))
}

fn snap(a: &Args, ctx: &Ctx) -> Result<Res> {
    let names = list(a, "name");
    let absent = st(a, "state").as_deref() == Some("absent");
    let mut changed = false;
    for n in names {
        let have = run_sh(ctx, &format!("snap list {} >/dev/null 2>&1", q(&n)))?.rc == 0;
        if have == !absent {
            continue;
        }
        changed = true;
        if !ctx.check {
            let cmd = if absent {
                format!("snap remove {}", q(&n))
            } else {
                format!(
                    "snap install {}{}",
                    if bo(a, "classic", false) {
                        "--classic "
                    } else {
                        ""
                    },
                    q(&n)
                )
            };
            if let Some(f) = ok_or_fail(run_sh(ctx, &cmd)?, "snap") {
                return Ok(f);
            }
        }
    }
    Ok(res(changed))
}

// ---------- repos / firewall / system ----------

fn yum_repository(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let file = st(a, "file").unwrap_or_else(|| name.clone());
    let path = format!("/etc/yum.repos.d/{file}.repo");
    if st(a, "state").as_deref() == Some("absent") {
        let ex = run_sh(ctx, &format!("test -e {}", q(&path)))?.rc == 0;
        if ex && !ctx.check {
            run_sh(ctx, &format!("rm -f {}", q(&path)))?;
        }
        return Ok(res(ex));
    }
    let mut t = format!(
        "[{name}]\nname={}\n",
        st(a, "description").unwrap_or_else(|| name.clone())
    );
    for k in [
        "baseurl",
        "mirrorlist",
        "metalink",
        "gpgkey",
        "exclude",
        "includepkgs",
        "proxy",
        "sslverify",
        "priority",
        "module_hotfixes",
    ] {
        if let Some(v) = a.get(k).filter(|v| !v.is_null()) {
            let s = match v {
                Value::Array(arr) => arr
                    .iter()
                    .map(|x| x.as_str().unwrap_or("").to_string())
                    .collect::<Vec<_>>()
                    .join(" "),
                Value::Bool(b) => (if *b { "1" } else { "0" }).into(),
                Value::String(s) => s.clone(),
                o => o.to_string(),
            };
            t += &format!("{k}={s}\n");
        }
    }
    for (k, d) in [("enabled", true), ("gpgcheck", true)] {
        let v = a.get(k).and_then(crate::playbook::loose_bool).unwrap_or(d);
        t += &format!("{k}={}\n", if v { 1 } else { 0 });
    }
    put_content(a, ctx, &path, t.as_bytes())
}

fn alternatives(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let path = st(a, "path").ok_or_else(|| anyhow!("missing path"))?;
    let cur = run_sh(
        ctx,
        &format!(
            "update-alternatives --query {} 2>/dev/null | sed -n 's/^Value: //p'",
            q(&name)
        ),
    )?
    .stdout
    .trim()
    .to_string();
    if cur == path {
        return Ok(res(false));
    }
    if !ctx.check {
        let cmd = match (st(a, "link"), st(a, "priority")) {
            (Some(l), p) => format!(
                "update-alternatives --install {} {} {} {} && update-alternatives --set {} {}",
                q(&l),
                q(&name),
                q(&path),
                p.unwrap_or_else(|| "50".into()),
                q(&name),
                q(&path)
            ),
            _ => format!("update-alternatives --set {} {}", q(&name), q(&path)),
        };
        if let Some(f) = ok_or_fail(run_sh(ctx, &cmd)?, "update-alternatives") {
            return Ok(f);
        }
    }
    Ok(res(true))
}

fn mount(a: &Args, ctx: &Ctx) -> Result<Res> {
    let path = st(a, "path").ok_or_else(|| anyhow!("missing path"))?;
    let state = st(a, "state").ok_or_else(|| anyhow!("missing state"))?;
    let fstab = st(a, "fstab").unwrap_or_else(|| "/etc/fstab".into());
    let mut changed = false;
    let mounted = run_sh(ctx, &format!("findmnt -n {} >/dev/null 2>&1", q(&path)))?.rc == 0;
    if matches!(
        state.as_str(),
        "present" | "mounted" | "absent" | "unmounted"
    ) && state != "unmounted"
    {
        let old = read_text(ctx, &fstab, true)?;
        let mut ls: Vec<String> = old
            .clone()
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect();
        let idx = ls.iter().position(|l| {
            !l.trim_start().starts_with('#') && l.split_whitespace().nth(1) == Some(path.as_str())
        });
        if state == "absent" {
            if let Some(i) = idx {
                ls.remove(i);
            }
        } else {
            let line = format!(
                "{} {} {} {} {} {}",
                st(a, "src").ok_or_else(|| anyhow!("missing src"))?,
                path,
                st(a, "fstype").ok_or_else(|| anyhow!("missing fstype"))?,
                st(a, "opts").unwrap_or_else(|| "defaults".into()),
                st(a, "dump").unwrap_or_else(|| "0".into()),
                st(a, "passno").unwrap_or_else(|| "0".into())
            );
            match idx {
                Some(i) => ls[i] = line,
                None => ls.push(line),
            }
        }
        let new = ls.join("\n") + "\n";
        if old.as_deref() != Some(new.as_str()) {
            changed = true;
            if !ctx.check {
                ctx.conn.put(&fstab, new.as_bytes(), None)?;
            }
        }
    }
    let act = match (state.as_str(), mounted) {
        ("mounted", false) => Some(format!("mkdir -p {p} && mount {p}", p = q(&path))),
        ("unmounted" | "absent", true) => Some(format!("umount {}", q(&path))),
        ("remounted", _) => Some(format!("mount -o remount {}", q(&path))),
        _ => None,
    };
    if let Some(c) = act {
        changed = true;
        if !ctx.check
            && let Some(f) = ok_or_fail(run_sh(ctx, &c)?, "mount")
        {
            return Ok(f);
        }
    }
    Ok(res(changed))
}

fn firewalld(a: &Args, ctx: &Ctx) -> Result<Res> {
    let state = st(a, "state").unwrap_or_else(|| "enabled".into());
    let zone = st(a, "zone")
        .map(|z| format!(" --zone={z}"))
        .unwrap_or_default();
    let (kind, item) = if let Some(s) = st(a, "service") {
        ("service", s)
    } else if let Some(p) = st(a, "port") {
        ("port", p)
    } else if let Some(r) = st(a, "rich_rule") {
        ("rich-rule", r)
    } else {
        return Ok(fail("one of service, port or rich_rule is required"));
    };
    let query = run_sh(
        ctx,
        &format!("firewall-cmd{zone} --query-{kind}={}", q(&item)),
    )?;
    let on = query.rc == 0;
    if on == (state == "enabled") {
        return Ok(res(false));
    }
    if !ctx.check {
        let verb = if state == "enabled" { "add" } else { "remove" };
        if bo(a, "permanent", false)
            && let Some(f) = ok_or_fail(
                run_sh(
                    ctx,
                    &format!(
                        "firewall-cmd --permanent{zone} --{verb}-{kind}={}",
                        q(&item)
                    ),
                )?,
                "firewall-cmd",
            )
        {
            return Ok(f);
        }
        if (bo(a, "immediate", false) || !bo(a, "permanent", false))
            && let Some(f) = ok_or_fail(
                run_sh(
                    ctx,
                    &format!("firewall-cmd{zone} --{verb}-{kind}={}", q(&item)),
                )?,
                "firewall-cmd",
            )
        {
            return Ok(f);
        }
    }
    Ok(res(true))
}

fn ufw(a: &Args, ctx: &Ctx) -> Result<Res> {
    let mut changed = false;
    let status = run_sh(ctx, "ufw status verbose")?.stdout;
    if let Some(st_) = st(a, "state") {
        let active = status.contains("Status: active");
        let (want_active, cmd) = match st_.as_str() {
            "enabled" => (true, "ufw --force enable"),
            "disabled" => (false, "ufw disable"),
            "reset" => (false, "ufw --force reset"),
            _ => (active, "ufw reload"),
        };
        if st_ == "reset" || st_ == "reloaded" || want_active != active {
            changed = true;
            if !ctx.check
                && let Some(f) = ok_or_fail(run_sh(ctx, cmd)?, "ufw")
            {
                return Ok(f);
            }
        }
    }
    if let Some(p) = st(a, "policy") {
        let dir = st(a, "direction").unwrap_or_else(|| "incoming".into());
        let have = status
            .lines()
            .find(|l| l.starts_with("Default:"))
            .unwrap_or("")
            .to_lowercase();
        if !have.contains(&format!("{} ({dir})", p.to_lowercase())) {
            changed = true;
            if !ctx.check {
                run_sh(ctx, &format!("ufw default {} {}", q(&p), q(&dir)))?;
            }
        }
    }
    if let Some(rule) = st(a, "rule") {
        let mut cmd = String::from("ufw");
        if bo(a, "delete", false) {
            cmd += " delete";
        }
        cmd += &format!(" {rule}");
        if let Some(d) = st(a, "direction") {
            cmd += &format!(" {d}");
        }
        if let Some(i) = st(a, "interface") {
            cmd += &format!(" on {i}");
        }
        cmd += &format!(" from {}", st(a, "from_ip").unwrap_or_else(|| "any".into()));
        if let Some(fp) = st(a, "from_port") {
            cmd += &format!(" port {fp}");
        }
        cmd += &format!(" to {}", st(a, "to_ip").unwrap_or_else(|| "any".into()));
        if let Some(port) = st(a, "port").or_else(|| st(a, "to_port")) {
            cmd += &format!(" port {port}");
        }
        if let Some(pr) = st(a, "proto") {
            cmd += &format!(" proto {pr}");
        }
        if ctx.check {
            return Ok(res(true));
        }
        let o = run_sh(ctx, &format!("{cmd} 2>&1"))?;
        if let Some(f) = ok_or_fail(o.clone(), "ufw") {
            return Ok(f);
        }
        if !o.stdout.contains("existing")
            && !o.stdout.contains("Could not delete non-existent")
            && !o.stdout.contains("Skipping")
        {
            changed = true;
        }
    }
    Ok(res(changed))
}

fn rpm_key(a: &Args, ctx: &Ctx) -> Result<Res> {
    let key = st(a, "key").ok_or_else(|| anyhow!("missing key"))?;
    if let Some(fp) = st(a, "fingerprint") {
        let fp = fp.replace(' ', "").to_lowercase();
        if run_sh(
            ctx,
            &format!(
                "rpm -q gpg-pubkey --qf '%{{version}}\\n' | grep -qi {}",
                q(&fp[fp.len().saturating_sub(8)..])
            ),
        )?
        .rc == 0
        {
            return Ok(res(false));
        }
    }
    if ctx.check {
        return Ok(res(true));
    }
    if let Some(f) = ok_or_fail(
        run_sh(ctx, &format!("rpm --import {}", q(&key)))?,
        "rpm --import",
    ) {
        return Ok(f);
    }
    Ok(res(true))
}

fn modprobe(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let absent = st(a, "state").as_deref() == Some("absent");
    let loaded = run_sh(
        ctx,
        &format!(
            "lsmod | awk '{{print $1}}' | grep -qx {}",
            q(&name.replace('-', "_"))
        ),
    )?
    .rc == 0;
    if loaded == !absent {
        return Ok(res(false));
    }
    if !ctx.check {
        let cmd = if absent {
            format!("modprobe -r {}", q(&name))
        } else {
            format!(
                "modprobe {} {}",
                q(&name),
                st(a, "params").unwrap_or_default()
            )
        };
        if let Some(f) = ok_or_fail(run_sh(ctx, &cmd)?, "modprobe") {
            return Ok(f);
        }
    }
    Ok(res(true))
}

fn selinux(a: &Args, ctx: &Ctx) -> Result<Res> {
    let state = st(a, "state").unwrap_or_default();
    let cfg = "/etc/selinux/config";
    let old = read_text(ctx, cfg, true)?;
    let text = old.clone().unwrap_or_default();
    let mut ls: Vec<String> = text.lines().map(String::from).collect();
    if let Some(i) = ls.iter().position(|l| l.starts_with("SELINUX=")) {
        ls[i] = format!("SELINUX={state}");
    } else {
        ls.push(format!("SELINUX={state}"));
    }
    let new = ls.join("\n") + "\n";
    let mut r = write_text(a, ctx, cfg, old.as_deref(), &new, res(false))?;
    if !ctx.check && state != "disabled" {
        let _ = run_sh(
            ctx,
            &format!("setenforce {}", if state == "enforcing" { 1 } else { 0 }),
        );
    }
    let _ = &mut r;
    Ok(r)
}

fn git_config(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let scope = st(a, "scope").unwrap_or_else(|| "global".into());
    let repo = st(a, "repo")
        .map(|r| format!("-C {} ", q(&r)))
        .unwrap_or_default();
    let flag = if scope == "file" {
        format!("--file {}", q(&st(a, "file").unwrap_or_default()))
    } else {
        format!("--{scope}")
    };
    let cur = run_sh(ctx, &format!("git {repo}config {flag} --get {}", q(&name)))?
        .stdout
        .trim()
        .to_string();
    if st(a, "state").as_deref() == Some("absent") {
        if cur.is_empty() {
            return Ok(res(false));
        }
        if !ctx.check {
            run_sh(
                ctx,
                &format!("git {repo}config {flag} --unset {}", q(&name)),
            )?;
        }
        return Ok(res(true));
    }
    let value = st(a, "value").unwrap_or_default();
    if cur == value {
        return Ok(res(false));
    }
    if !ctx.check
        && let Some(f) = ok_or_fail(
            run_sh(
                ctx,
                &format!("git {repo}config {flag} {} {}", q(&name), q(&value)),
            )?,
            "git config",
        )
    {
        return Ok(f);
    }
    let _ = apply_attrs;
    Ok(res(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_adds_section_and_option() {
        let out = ini_set("[a]\nx = 1\n", "b", Some("y"), Some("2"), false, false);
        assert_eq!(out, "[a]\nx = 1\n[b]\ny = 2\n");
    }

    #[test]
    fn ini_replaces_and_removes() {
        let t = "[a]\nx = 1\ny = 2\n[b]\nx = 9\n";
        assert_eq!(
            ini_set(t, "a", Some("x"), Some("5"), false, false),
            "[a]\nx = 5\ny = 2\n[b]\nx = 9\n"
        );
        assert_eq!(
            ini_set(t, "a", Some("y"), None, true, false),
            "[a]\nx = 1\n[b]\nx = 9\n"
        );
        assert_eq!(ini_set(t, "a", None, None, true, false), "[b]\nx = 9\n");
    }

    #[test]
    fn ini_appends_inside_existing_section() {
        let out = ini_set(
            "[a]\nx = 1\n\n[b]\nq = 1\n",
            "a",
            Some("z"),
            Some("3"),
            false,
            true,
        );
        assert_eq!(out, "[a]\nx = 1\nz=3\n\n[b]\nq = 1\n");
    }
}
