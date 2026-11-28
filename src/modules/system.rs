//! Built-in modules: system.
#![allow(unused_imports)]
use super::*;

pub(super) fn setup(ctx: &Ctx) -> Result<Res> {
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

pub(super) fn service(a: &Args, ctx: &Ctx) -> Result<Res> {
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

/// `getent <db> <key>` split into passwd/group fields (None when missing).
fn getent_rec(ctx: &Ctx, db: &str, key: &str) -> Result<Option<Vec<String>>> {
    let o = ctx.conn.exec(&format!("getent {db} {}", q(key)))?;
    if o.rc != 0 || o.stdout.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(
        o.stdout.trim().split(':').map(|s| s.to_string()).collect(),
    ))
}

fn num_or(v: &str) -> Value {
    v.parse::<i64>()
        .map(Value::from)
        .unwrap_or(Value::String(v.into()))
}

pub(super) fn user(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let state = st(a, "state").unwrap_or_else(|| "present".into());
    let n = q(&name);
    let pw = getent_rec(ctx, "passwd", &name)?;
    let exists = pw.is_some();
    let run = |cmd: String| -> Result<crate::conn::Output> {
        let o = ctx.conn.exec(&cmd)?;
        Ok(o)
    };
    let failed = |cmd: &str, o: &crate::conn::Output| -> Res {
        let mut r = fail(format!("{cmd}: {}", o.stderr.trim()));
        r.insert("name".into(), name.clone().into());
        r.insert("rc".into(), o.rc.into());
        r.insert("stderr".into(), o.stderr.clone().into());
        r.insert("stdout".into(), o.stdout.clone().into());
        r
    };
    let mut out = res(false);
    out.insert("name".into(), name.clone().into());
    if state == "absent" {
        out.insert("state".into(), "absent".into());
        if exists {
            out.insert("force".into(), bo(a, "force", false).into());
            out.insert("remove".into(), bo(a, "remove", false).into());
            if !ctx.check {
                let cmd = format!(
                    "userdel {}{}{n}",
                    if bo(a, "force", false) { "-f " } else { "" },
                    if bo(a, "remove", false) { "-r " } else { "" }
                );
                let o = run(cmd.clone())?;
                if o.rc != 0 {
                    return Ok(failed(&cmd, &o));
                }
                out.insert("stderr".into(), o.stderr.clone().into());
                out.insert("stderr_lines".into(), lines(&o.stderr));
            }
            out.insert("changed".into(), true.into());
        }
        return Ok(out);
    }
    let non_unique = bo(a, "non_unique", false);
    let groups = list(a, "groups");
    let append = bo(a, "append", false);
    let system = bo(a, "system", false);
    let create_home = bo(a, "create_home", true);
    let move_home = bo(a, "move_home", false);
    let expire_arg = |e: &str| -> Option<String> {
        let secs: f64 = e.parse().ok()?;
        if secs < 0.0 {
            return Some("''".into());
        }
        let days = (secs / 86400.0).floor() as i64;
        // civil-from-days (UTC)
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
        Some(format!("{y:04}-{m:02}-{d:02}"))
    };
    // Ansible passes `!<hash>` when the account must be locked
    let lock_pw = bo(a, "password_lock", false);
    let pw_arg = |p: &str| -> String {
        if lock_pw {
            format!("!{p}")
        } else {
            p.to_string()
        }
    };
    let mut flags = String::new();
    if let Some(v) = st(a, "uid") {
        flags += &format!(" -u {}", q(&v));
        if non_unique {
            flags += " -o";
        }
    }
    if let Some(v) = st(a, "group") {
        flags += &format!(" -g {}", q(&v));
    }
    if let Some(v) = st(a, "comment") {
        flags += &format!(" -c {}", q(&v));
    }
    if let Some(v) = st(a, "shell") {
        flags += &format!(" -s {}", q(&v));
    }
    if let Some(v) = st(a, "home") {
        flags += &format!(" -d {}", q(&v));
    }
    let gstr = groups.join(",");
    if !groups.is_empty()
        || a.get("groups")
            .is_some_and(|v| v.is_array() || v.is_string())
    {
        // handled per create/modify below
    }
    let mut changed = false;
    let mut created = false;
    if !exists {
        changed = true;
        created = true;
        if !ctx.check {
            let mut f = flags.clone();
            if !groups.is_empty() {
                f += &format!(" -G {}", q(&gstr));
            }
            if let Some(v) = st(a, "password") {
                f += &format!(" -p {}", q(&pw_arg(&v)));
            }
            if let Some(e) = st(a, "expires").and_then(|e| expire_arg(&e)) {
                f += &format!(" -e {e}");
            }
            if system {
                f += " -r";
            }
            f += if create_home { " -m" } else { " -M" };
            if let Some(v) = st(a, "skeleton")
                && create_home
            {
                f += &format!(" -k {}", q(&v));
            }
            let cmd = format!("useradd{f} {n}");
            let o = run(cmd.clone())?;
            if o.rc != 0 {
                return Ok(failed(&cmd, &o));
            }
            if bo(a, "password_lock", false) && st(a, "password").is_none() {
                let o = run(format!("usermod -L {n}"))?;
                if o.rc != 0 {
                    return Ok(failed("usermod -L", &o));
                }
            }
        }
    } else {
        let f = pw.clone().unwrap_or_default();
        let mut mods = String::new();
        let gid_of = |g: &str| -> Result<Option<String>> {
            if g.chars().all(|c| c.is_ascii_digit()) {
                return Ok(Some(g.to_string()));
            }
            Ok(getent_rec(ctx, "group", g)?.and_then(|v| v.get(2).cloned()))
        };
        if let Some(w) = st(a, "uid")
            && f.get(2) != Some(&w)
        {
            mods += &format!(" -u {}{}", q(&w), if non_unique { " -o" } else { "" });
        }
        if let Some(w) = st(a, "group")
            && let Some(gid) = gid_of(&w)?
            && f.get(3) != Some(&gid)
        {
            mods += &format!(" -g {}", q(&w));
        }
        if let Some(w) = st(a, "comment")
            && f.get(4) != Some(&w)
        {
            mods += &format!(" -c {}", q(&w));
        }
        if let Some(w) = st(a, "shell")
            && f.get(6) != Some(&w)
        {
            mods += &format!(" -s {}", q(&w));
        }
        if let Some(w) = st(a, "home")
            && f.get(5) != Some(&w)
        {
            mods += &format!(" {}-d {}", if move_home { "-m " } else { "" }, q(&w));
        }
        if a.get("groups").is_some() {
            let have = ctx.conn.exec(&format!(
                "id -nG {n} | tr ' ' '\\n' | grep -vx \"$(id -gn {n})\" | sort -u"
            ))?;
            let have: Vec<String> = have.stdout.lines().map(|s| s.to_string()).collect();
            let want: Vec<&String> = groups.iter().collect();
            let missing: Vec<&String> =
                want.iter().filter(|g| !have.contains(g)).copied().collect();
            let primary_gid = f.get(3).cloned().unwrap_or_default();
            let want_non_primary: Vec<&String> = want
                .iter()
                .filter(|g| gid_of(g).ok().flatten().as_deref() != Some(primary_gid.as_str()))
                .copied()
                .collect();
            if append {
                if !missing.is_empty() {
                    let mut all: Vec<String> = have.clone();
                    all.extend(missing.iter().map(|s| s.to_string()));
                    mods += &format!(" -G {}", q(&all.join(",")));
                }
            } else {
                let extra = have.iter().any(|h| !want_non_primary.contains(&h));
                let lacking = want_non_primary.iter().any(|w| !have.contains(w));
                if extra || lacking {
                    mods += &format!(" -G {}", q(&gstr));
                }
            }
        }
        let shadow = ctx
            .conn
            .exec(&format!("getent shadow {n}"))?
            .stdout
            .trim()
            .split(':')
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        if let Some(w) = st(a, "password")
            && st(a, "update_password").as_deref() != Some("on_create")
            && shadow.get(1) != Some(&pw_arg(&w))
        {
            mods += &format!(" -p {}", q(&pw_arg(&w)));
        }
        if let Some(e) = st(a, "expires") {
            let cur = shadow.get(7).cloned().unwrap_or_default();
            let want_days = e.parse::<f64>().ok().map(|s| {
                if s < 0.0 {
                    String::new()
                } else {
                    ((s / 86400.0).floor() as i64).to_string()
                }
            });
            if want_days.is_some_and(|w| w != cur)
                && let Some(arg) = expire_arg(&e)
            {
                mods += &format!(" -e {arg}");
            }
        }
        if !mods.is_empty() {
            changed = true;
            if !ctx.check {
                let cmd = format!("usermod{mods} {n}");
                let o = run(cmd.clone())?;
                if o.rc != 0 {
                    return Ok(failed(&cmd, &o));
                }
            }
        }
        if let Some(lock) = a
            .get("password_lock")
            .and_then(loose_bool)
            .filter(|_| st(a, "password").is_none())
        {
            let locked = shadow.get(1).is_some_and(|h| h.starts_with('!'));
            if lock != locked {
                changed = true;
                if !ctx.check {
                    let o = run(format!("usermod {} {n}", if lock { "-L" } else { "-U" }))?;
                    if o.rc != 0 {
                        return Ok(failed("usermod", &o));
                    }
                }
            }
        }
        // missing home directory
        if create_home && !ctx.check {
            let home = getent_rec(ctx, "passwd", &name)?
                .and_then(|v| v.get(5).cloned())
                .unwrap_or_default();
            if !home.is_empty()
                && ctx.conn.exec(&format!("test -d {}", q(&home)))?.rc != 0
                && st(a, "home").is_none()
            {
                run(format!(
                    "mkdir -p {h} && cp -rT /etc/skel {h} 2>/dev/null; chown -R {n}: {h} && chmod 0755 {h}",
                    h = q(&home)
                ))?;
                changed = true;
            }
        }
    }
    // ssh key
    let after = if ctx.check && !exists {
        None
    } else {
        getent_rec(ctx, "passwd", &name)?
    };
    if bo(a, "generate_ssh_key", false)
        && let Some(p) = &after
    {
        let home = p.get(5).cloned().unwrap_or_default();
        let file = st(a, "ssh_key_file").unwrap_or_else(|| {
            format!(
                "{home}/.ssh/id_{}",
                st(a, "ssh_key_type").unwrap_or_else(|| "rsa".into())
            )
        });
        let file = if file.starts_with('/') {
            file
        } else {
            format!("{home}/{file}")
        };
        let fq = q(&file);
        let has = ctx.conn.exec(&format!("test -f {fq}"))?.rc == 0;
        if !has && !ctx.check {
            let bits = st(a, "ssh_key_bits").unwrap_or_else(|| "0".into());
            let host = ctx.conn.exec("hostname")?.stdout.trim().to_string();
            let comment =
                st(a, "ssh_key_comment").unwrap_or_else(|| format!("ansible-generated on {host}"));
            let mut cmd = format!(
                "mkdir -p \"$(dirname {fq})\" && chown {n}: \"$(dirname {fq})\" && chmod 0700 \"$(dirname {fq})\" && ssh-keygen -q -t {} -C {} -N {} -f {fq}",
                q(&st(a, "ssh_key_type").unwrap_or_else(|| "rsa".into())),
                q(&comment),
                q(&st(a, "ssh_key_passphrase").unwrap_or_default())
            );
            if bits != "0" {
                cmd += &format!(" -b {}", q(&bits));
            }
            cmd += &format!(" && chown {n}: {fq} {fq}.pub");
            let o = run(cmd.clone())?;
            if o.rc != 0 {
                return Ok(failed("ssh-keygen", &o));
            }
            changed = true;
        } else if !has {
            changed = true;
        }
        out.insert("ssh_key_file".into(), file.clone().into());
        if let Ok(o) = ctx
            .conn
            .exec(&format!("ssh-keygen -l -f {fq}.pub 2>/dev/null"))
            && o.rc == 0
        {
            out.insert("ssh_fingerprint".into(), o.stdout.trim().to_string().into());
        }
        if let Ok(o) = ctx.conn.exec(&format!("cat {fq}.pub 2>/dev/null"))
            && o.rc == 0
        {
            out.insert("ssh_public_key".into(), o.stdout.trim().to_string().into());
        }
    }
    // result keys (what Ansible reports)
    if let Some(p) = after {
        out.insert(
            "uid".into(),
            num_or(p.get(2).map(String::as_str).unwrap_or("")),
        );
        out.insert(
            "group".into(),
            num_or(p.get(3).map(String::as_str).unwrap_or("")),
        );
        out.insert(
            "comment".into(),
            p.get(4).cloned().unwrap_or_default().into(),
        );
        out.insert("home".into(), p.get(5).cloned().unwrap_or_default().into());
        out.insert("shell".into(), p.get(6).cloned().unwrap_or_default().into());
    }
    out.insert("state".into(), "present".into());
    if created {
        out.insert("system".into(), system.into());
        out.insert("create_home".into(), create_home.into());
    } else {
        out.insert("append".into(), append.into());
        out.insert("move_home".into(), move_home.into());
    }
    out.insert("groups".into(), gstr.into());
    if a.get("password").is_some() {
        out.insert("password".into(), "NOT_LOGGING_PASSWORD".into());
    }
    out.insert("changed".into(), changed.into());
    Ok(out)
}

pub(super) fn group(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    let absent = st(a, "state").as_deref() == Some("absent");
    let cur = getent_rec(ctx, "group", &name)?;
    let non_unique = bo(a, "non_unique", false);
    let system = bo(a, "system", false);
    let mut out = res(false);
    out.insert("name".into(), name.clone().into());
    out.insert(
        "state".into(),
        if absent { "absent" } else { "present" }.into(),
    );
    let mut cmd: Option<String> = None;
    if absent {
        if cur.is_some() {
            cmd = Some(format!("groupdel {}", q(&name)));
        }
    } else {
        match &cur {
            None => {
                let mut c = String::from("groupadd");
                if let Some(g) = st(a, "gid") {
                    c += &format!(" -g {}", q(&g));
                    if non_unique {
                        c += " -o";
                    }
                }
                if system {
                    c += " -r";
                }
                cmd = Some(format!("{c} {}", q(&name)));
            }
            Some(f) => {
                if let Some(g) = st(a, "gid")
                    && f.get(2) != Some(&g)
                {
                    cmd = Some(format!(
                        "groupmod -g {}{} {}",
                        q(&g),
                        if non_unique { " -o" } else { "" },
                        q(&name)
                    ));
                }
            }
        }
        out.insert("system".into(), system.into());
    }
    let changed = cmd.is_some();
    if let Some(c) = &cmd
        && !ctx.check
    {
        let o = ctx.conn.exec(c)?;
        if o.rc != 0 {
            let mut r = fail(format!(
                "{}: {}",
                c.split(' ').next().unwrap_or(""),
                o.stderr.trim()
            ));
            r.insert("name".into(), name.into());
            return Ok(r);
        }
    }
    if !absent {
        let gid = if ctx.check && cur.is_none() {
            st(a, "gid")
        } else {
            getent_rec(ctx, "group", &name)?.and_then(|f| f.get(2).cloned())
        };
        if let Some(g) = gid {
            out.insert("gid".into(), num_or(&g));
        }
    }
    out.insert("changed".into(), changed.into());
    Ok(out)
}

// ---------- network / vcs ----------

pub(super) fn hostname(a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn timezone(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing name"))?;
    if ctx
        .conn
        .exec("command -v timedatectl >/dev/null && timedatectl >/dev/null 2>&1")?
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
            // Debian's timedated records the name in /etc/timezone; make that deterministic
            // (it is not always written before timedatectl returns)
            ctx.conn.exec(&format!(
                "test -f /etc/timezone && echo {} > /etc/timezone; true",
                q(&name)
            ))?;
        }
        return Ok(res(true));
    }
    // Debian-style: /etc/timezone holds the literal name (Ansible compares it verbatim);
    // otherwise fall back to the /etc/localtime symlink target.
    let has_file = ctx.conn.exec("test -f /etc/timezone")?.rc == 0;
    let cur = if has_file {
        ctx.conn
            .exec("cat /etc/timezone")?
            .stdout
            .trim()
            .to_string()
    } else {
        ctx.conn
            .exec("readlink -f /etc/localtime | sed 's|.*/zoneinfo/||'")?
            .stdout
            .trim()
            .to_string()
    };
    if cur == name {
        return Ok(res(false));
    }
    if !has_file {
        let want = ctx
            .conn
            .exec(&format!(
                "readlink -f /usr/share/zoneinfo/{} | sed 's|.*/zoneinfo/||'",
                q(&name)
            ))?
            .stdout
            .trim()
            .to_string();
        if !want.is_empty() && cur == want {
            return Ok(res(false));
        }
    }
    if !ctx.check {
        let o = ctx.conn.exec(&format!("test -f /usr/share/zoneinfo/{n} && ln -sf /usr/share/zoneinfo/{n} /etc/localtime && echo {n} > /etc/timezone", n = q(&name)))?;
        if o.rc != 0 {
            return Ok(fail(format!("unknown timezone {name}")));
        }
    }
    Ok(res(true))
}

pub(super) fn authorized_key(a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn cron(a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn service_facts(ctx: &Ctx) -> Result<Res> {
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

pub(super) fn getent(a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn locale_gen(a: &Args, ctx: &Ctx) -> Result<Res> {
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
