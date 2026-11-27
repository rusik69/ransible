//! Built-in modules: control.
#![allow(unused_imports)]
use super::*;

pub(super) fn debug(a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn assert_mod(a: &Args, ctx: &Ctx) -> Result<Res> {
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

pub(super) fn wait_for(a: &Args, ctx: &Ctx) -> Result<Res> {
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
