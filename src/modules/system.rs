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

pub(super) fn user(a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn group(a: &Args, ctx: &Ctx) -> Result<Res> {
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
