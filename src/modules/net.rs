//! Built-in modules: net.
#![allow(unused_imports)]
use super::*;

pub(super) fn git(a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn get_url(a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn uri(a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn fetch(a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn synchronize(a: &Args, ctx: &Ctx) -> Result<Res> {
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
