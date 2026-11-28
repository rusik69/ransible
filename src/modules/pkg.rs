//! Built-in modules: pkg.
#![allow(unused_imports)]
use super::*;

pub(super) fn detect_pkg(ctx: &Ctx) -> Result<String> {
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

pub(super) fn package(module: &str, a: &Args, ctx: &Ctx) -> Result<Res> {
    if module == "apt" || (module == "package" && detect_pkg(ctx).is_ok_and(|m| m == "apt")) {
        return apt(a, ctx);
    }
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

pub(super) fn pip(a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn package_facts(ctx: &Ctx) -> Result<Res> {
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

pub(super) fn deb822_repository(a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn apt_repository(a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn apt_key(a: &Args, ctx: &Ctx) -> Result<Res> {
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

/// `apt` with Ansible's option set, result keys and idempotence rules.
pub(super) fn apt(a: &Args, ctx: &Ctx) -> Result<Res> {
    let state = st(a, "state").unwrap_or_else(|| "present".into());
    let names = if a.contains_key("name") {
        list(a, "name")
    } else {
        list(a, "pkg")
    };
    let purge = bo(a, "purge", false);
    let mut out = res(false);
    let (mut stdout, mut stderr) = (String::new(), String::new());
    let mut changed = false;
    let mut ran = false; // a package-changing command ran
    let apt_get = |ctx: &Ctx, args: &str| -> Result<crate::conn::Output> {
        let mut cmd = String::from(
            "DEBIAN_FRONTEND=noninteractive LC_ALL=C apt-get -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold",
        );
        if !list(a, "dpkg_options").is_empty() {
            cmd = format!(
                "DEBIAN_FRONTEND=noninteractive LC_ALL=C apt-get -y {}",
                list(a, "dpkg_options")
                    .iter()
                    .map(|o| format!("-o Dpkg::Options::={}", q(&format!("--{o}"))))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        if bo(a, "allow_unauthenticated", false) {
            cmd += " --allow-unauthenticated";
        }
        if bo(a, "allow_downgrade", false) {
            cmd += " --allow-downgrades";
        }
        if bo(a, "force", false) {
            cmd += " --force-yes";
        }
        if let Some(r) = st(a, "default_release") {
            cmd += &format!(" -t {}", q(&r));
        }
        if !bo(a, "install_recommends", true) {
            cmd += " --no-install-recommends";
        }
        ctx.conn.exec(&format!("{cmd} {args}"))
    };
    let apt_failed = |cmd: &str, o: &crate::conn::Output, stdout: &str, stderr: &str| -> Res {
        let mut r = fail(format!("'{cmd}' failed: {}", o.stderr.trim()));
        r.insert("rc".into(), o.rc.into());
        r.insert("stdout".into(), format!("{stdout}{}", o.stdout).into());
        r.insert("stderr".into(), format!("{stderr}{}", o.stderr).into());
        r
    };
    // --- cache
    let want_cache = bo(a, "update_cache", false) || a.contains_key("cache_valid_time");
    let stamp = "/var/lib/apt/periodic/update-success-stamp";
    let mtime = |ctx: &Ctx| -> i64 {
        ctx.conn
            .exec(&format!(
                "stat -c %Y {stamp} 2>/dev/null || stat -c %Y /var/lib/apt/lists 2>/dev/null || echo 0"
            ))
            .ok()
            .and_then(|o| o.stdout.lines().next().and_then(|l| l.trim().parse().ok()))
            .unwrap_or(0)
    };
    let mut cache_updated = false;
    if bo(a, "update_cache", false) || a.contains_key("cache_valid_time") {
        let valid = a
            .get("cache_valid_time")
            .and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(0);
        let now: i64 = ctx
            .conn
            .exec("date +%s")?
            .stdout
            .trim()
            .parse()
            .unwrap_or(0);
        let fresh = valid > 0 && now - mtime(ctx) < valid;
        let do_update = bo(a, "update_cache", false) && !fresh;
        if do_update && !ctx.check {
            let retries = a
                .get("update_cache_retries")
                .and_then(|v| v.as_i64())
                .unwrap_or(5)
                .max(1);
            let delay = a
                .get("update_cache_retry_max_delay")
                .and_then(|v| v.as_i64())
                .unwrap_or(12);
            let mut last = None;
            for i in 0..retries {
                let o = ctx
                    .conn
                    .exec("DEBIAN_FRONTEND=noninteractive apt-get update")?;
                if o.rc == 0 {
                    last = None;
                    break;
                }
                last = Some(o);
                if i + 1 < retries {
                    ctx.conn
                        .exec(&format!("sleep {}", (1i64 << i).min(delay)))?;
                }
            }
            if let Some(o) = last {
                return Ok(fail(format!(
                    "Failed to update apt cache: {}",
                    o.stderr.trim()
                )));
            }
            cache_updated = true;
        } else if do_update {
            cache_updated = true;
        }
    }
    // --- upgrade
    if let Some(up) = st(a, "upgrade").filter(|u| u != "no") {
        let sub = match up.as_str() {
            "full" | "dist" => "dist-upgrade",
            _ => "upgrade",
        };
        let sim = ctx
            .conn
            .exec(&format!("apt-get -s {sub} 2>/dev/null | grep -c '^Inst '"))?;
        if sim.stdout.trim().parse::<i64>().unwrap_or(0) > 0 {
            changed = true;
            if !ctx.check {
                let o = apt_get(ctx, sub)?;
                if o.rc != 0 {
                    return Ok(apt_failed(sub, &o, &stdout, &stderr));
                }
                stdout += &o.stdout;
                stderr += &o.stderr;
                ran = true;
            }
        }
    }
    // --- cleanup-only operations
    for (flag, sim_cmd, run_cmd) in [
        (
            "autoremove",
            "apt-get -s autoremove 2>/dev/null | grep -c '^Remv '",
            "autoremove",
        ),
        (
            "autoclean",
            "apt-get -s autoclean 2>/dev/null | grep -c '^Del '",
            "autoclean",
        ),
    ] {
        if bo(a, flag, false) && !(flag == "autoremove" && state == "absent" && !names.is_empty()) {
            let n = ctx
                .conn
                .exec(sim_cmd)?
                .stdout
                .trim()
                .parse::<i64>()
                .unwrap_or(0);
            if n > 0 {
                changed = true;
            }
            if !ctx.check {
                let o = apt_get(
                    ctx,
                    &format!(
                        "{run_cmd}{}",
                        if purge && flag == "autoremove" {
                            " --purge"
                        } else {
                            ""
                        }
                    ),
                )?;
                if o.rc != 0 {
                    return Ok(apt_failed(run_cmd, &o, &stdout, &stderr));
                }
                stdout += &o.stdout;
                stderr += &o.stderr;
                ran = true;
            }
        }
    }
    // --- .deb files
    if let Some(deb) = st(a, "deb") {
        let local = if deb.starts_with("http://") || deb.starts_with("https://") {
            let tmp = "/tmp/ransible_apt_deb.deb";
            if !ctx.check {
                let o = ctx.conn.exec(&format!("curl -fsSL -o {tmp} {}", q(&deb)))?;
                if o.rc != 0 {
                    return Ok(fail(format!(
                        "failed to download {deb}: {}",
                        o.stderr.trim()
                    )));
                }
            }
            tmp.to_string()
        } else {
            deb.clone()
        };
        if !ctx.check {
            let info = ctx.conn.exec(&format!(
                "dpkg-deb -f {l} Package Version | tr '\\n' ' '",
                l = q(&local)
            ))?;
            let mut it = info.stdout.split_whitespace();
            let (pk, ver) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
            let have = ctx
                .conn
                .exec(&format!(
                    "dpkg-query -W -f='${{Version}}' {} 2>/dev/null",
                    q(pk)
                ))?
                .stdout;
            if have.trim() != ver || ver.is_empty() {
                let o = apt_get(ctx, &format!("install {}", q(&local)))?;
                if o.rc != 0 {
                    return Ok(apt_failed("install", &o, &stdout, &stderr));
                }
                stdout += &o.stdout;
                stderr += &o.stderr;
                changed = true;
                ran = true;
            }
        } else {
            changed = true;
        }
    }
    // --- package lists
    let status_of = |p: &str| -> Result<(bool, bool, String)> {
        // (installed, config-files only, version)
        let o = ctx.conn.exec(&format!(
            "dpkg-query -W -f='${{Status}}|${{Version}}' {} 2>/dev/null",
            q(p)
        ))?;
        let t = o.stdout.trim();
        let (s, v) = t.split_once('|').unwrap_or(("", ""));
        Ok((
            s.ends_with(" installed"),
            s.ends_with(" config-files"),
            v.to_string(),
        ))
    };
    let expand = |p: &str| -> Result<Vec<String>> {
        if p.contains('*') || p.contains('?') {
            let o = ctx.conn.exec(&format!(
                "dpkg-query -W -f='${{Package}}\\n' {} 2>/dev/null",
                q(p)
            ))?;
            let v: Vec<String> = o.stdout.lines().map(|s| s.to_string()).collect();
            if v.is_empty() {
                bail!("No package(s) matching '{p}' available");
            }
            Ok(v)
        } else {
            Ok(vec![p.to_string()])
        }
    };
    let mut to_install: Vec<String> = vec![];
    let mut to_remove: Vec<String> = vec![];
    if state == "build-dep" {
        let o = ctx.conn.exec(&format!(
            "apt-get -s build-dep {} 2>/dev/null | grep -c '^Inst '",
            names.iter().map(|n| q(n)).collect::<Vec<_>>().join(" ")
        ))?;
        if o.stdout.trim().parse::<i64>().unwrap_or(0) > 0 {
            changed = true;
            if !ctx.check {
                let o = apt_get(
                    ctx,
                    &format!(
                        "build-dep {}",
                        names.iter().map(|n| q(n)).collect::<Vec<_>>().join(" ")
                    ),
                )?;
                if o.rc != 0 {
                    return Ok(apt_failed("build-dep", &o, &stdout, &stderr));
                }
                stdout += &o.stdout;
                stderr += &o.stderr;
                ran = true;
            }
        }
    } else if state == "fixed" {
        changed = true;
        if !ctx.check {
            let o = apt_get(ctx, "install -f")?;
            if o.rc != 0 {
                return Ok(apt_failed("install -f", &o, &stdout, &stderr));
            }
            stdout += &o.stdout;
            stderr += &o.stderr;
            ran = true;
        }
    } else {
        for spec in names.iter().filter(|n| n.as_str() != "*") {
            match state.as_str() {
                "absent" | "removed" => {
                    for p in expand(spec)? {
                        let (inst, rc_only, _) = status_of(&p)?;
                        if inst || (purge && rc_only) {
                            to_remove.push(p);
                        }
                    }
                }
                _ => {
                    let (pkg, ver) = match spec.split_once('=') {
                        Some((n, v)) => (n.to_string(), Some(v.to_string())),
                        None => (spec.clone(), None),
                    };
                    for p in expand(&pkg)? {
                        let (inst, _, have) = status_of(&p)?;
                        if st(a, "only_upgrade").is_some() && bo(a, "only_upgrade", false) && !inst
                        {
                            // must still exist in the archive
                            if ctx
                                .conn
                                .exec(&format!("apt-cache show {} >/dev/null 2>&1", q(&p)))?
                                .rc
                                != 0
                            {
                                return Ok(fail(format!("No package matching '{p}' is available")));
                            }
                            continue;
                        }
                        if !inst
                            && ctx
                                .conn
                                .exec(&format!("apt-cache show {} >/dev/null 2>&1", q(&p)))?
                                .rc
                                != 0
                        {
                            return Ok(fail(format!("No package matching '{p}' is available")));
                        }
                        let ok_ver = match &ver {
                            Some(v) => inst && glob_match(v, &have),
                            None => inst,
                        };
                        if let Some(v) = &ver
                            && !ok_ver
                        {
                            let m = ctx.conn.exec(&format!(
                                "apt-cache madison {} 2>/dev/null | awk '{{print $3}}'",
                                q(&p)
                            ))?;
                            if !m.stdout.lines().any(|c| glob_match(v, c.trim())) {
                                let mut r = fail(format!(
                                    "no available installation candidate for {p}={v}"
                                ));
                                r.insert("cache_updated".into(), cache_updated.into());
                                r.insert("cache_update_time".into(), mtime(ctx).into());
                                return Ok(r);
                            }
                        }
                        let item = match &ver {
                            Some(v) => format!("{p}={v}"),
                            None => p.clone(),
                        };
                        if state == "latest" && inst && ver.is_none() {
                            let sim = ctx.conn.exec(&format!(
                                "apt-get -s install {} 2>/dev/null | grep -c '^Inst '",
                                q(&p)
                            ))?;
                            if sim.stdout.trim().parse::<i64>().unwrap_or(0) > 0 {
                                to_install.push(item);
                            }
                        } else if !ok_ver {
                            to_install.push(item);
                        }
                    }
                }
            }
        }
        if !to_install.is_empty() {
            changed = true;
            if !ctx.check {
                let args = format!(
                    "{}install {}",
                    if bo(a, "only_upgrade", false) {
                        "--only-upgrade "
                    } else {
                        ""
                    },
                    to_install
                        .iter()
                        .map(|n| q(n))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                let o = apt_get(ctx, &args)?;
                if o.rc != 0 {
                    return Ok(apt_failed(&format!("apt-get {args}"), &o, &stdout, &stderr));
                }
                stdout += &o.stdout;
                stderr += &o.stderr;
                ran = true;
            }
        }
        if !to_remove.is_empty() {
            changed = true;
            if !ctx.check {
                let args = format!(
                    "{}{} {}",
                    if purge { "purge" } else { "remove" },
                    if bo(a, "autoremove", false) {
                        " --auto-remove"
                    } else {
                        ""
                    },
                    to_remove.iter().map(|n| q(n)).collect::<Vec<_>>().join(" ")
                );
                let o = apt_get(ctx, &args)?;
                if o.rc != 0 {
                    return Ok(apt_failed(&format!("apt-get {args}"), &o, &stdout, &stderr));
                }
                stdout += &o.stdout;
                stderr += &o.stderr;
                ran = true;
            }
        } else if bo(a, "autoremove", false) && state == "absent" && !names.is_empty() {
            let n = ctx
                .conn
                .exec("apt-get -s autoremove 2>/dev/null | grep -c '^Remv '")?
                .stdout
                .trim()
                .parse::<i64>()
                .unwrap_or(0);
            if n > 0 {
                changed = true;
                if !ctx.check {
                    let o = apt_get(ctx, "autoremove")?;
                    if o.rc != 0 {
                        return Ok(apt_failed("autoremove", &o, &stdout, &stderr));
                    }
                    stdout += &o.stdout;
                    stderr += &o.stderr;
                    ran = true;
                }
            }
        }
    }
    out.insert("changed".into(), changed.into());
    // Ansible reports the cache keys on every present/latest/update call, not on plain removals
    let installing = !names.is_empty()
        && !matches!(state.as_str(), "absent" | "removed" | "build-dep" | "fixed");
    if want_cache || installing || st(a, "upgrade").is_some_and(|u| u != "no") {
        out.insert("cache_updated".into(), cache_updated.into());
        out.insert("cache_update_time".into(), mtime(ctx).into());
    }
    if ran {
        out.insert("stdout".into(), stdout.clone().into());
        out.insert("stdout_lines".into(), lines(&stdout));
        out.insert("stderr".into(), stderr.clone().into());
        out.insert("stderr_lines".into(), lines(&stderr));
    }
    Ok(out)
}

/// shell-style `*`/`?` match (apt version globs like `1.2*`).
fn glob_match(pat: &str, s: &str) -> bool {
    fn m(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => m(&p[1..], s) || (!s.is_empty() && m(p, &s[1..])),
            (Some(b'?'), Some(_)) => m(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => m(&p[1..], &s[1..]),
            _ => false,
        }
    }
    m(pat.as_bytes(), s.as_bytes())
}
