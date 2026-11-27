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
