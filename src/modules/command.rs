//! Built-in modules: command.
#![allow(unused_imports)]
use super::*;

pub(super) fn out_to_res(
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

pub(super) fn command(module: &str, a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn script(a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn async_status(a: &Args, ctx: &Ctx) -> Result<Res> {
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
