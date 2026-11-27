//! Native replacements for community.mysql modules, driven through the `mysql` CLI on the
//! target (so no PyMySQL needed). Credentials come from `login_*` args or the target user's ~/.my.cnf.
use crate::conn::Output;
use crate::modules::{Args, Ctx, Res, bo, fail, list, res, st};
use crate::template::shell_quote as q;
use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

fn ident(s: &str) -> String {
    format!("`{}`", s.replace('`', "``"))
}

fn base_cmd(a: &Args) -> String {
    let mut c = String::new();
    if let Some(p) = st(a, "login_password") {
        c += &format!("MYSQL_PWD={} ", q(&p));
    }
    c += "mysql -N -B";
    if let Some(u) = st(a, "login_user") {
        c += &format!(" -u {}", q(&u));
    }
    if let Some(h) = st(a, "login_host") {
        c += &format!(" -h {}", q(&h));
    }
    if let Some(p) = st(a, "login_port") {
        c += &format!(" -P {}", q(&p));
    }
    if let Some(s) = st(a, "login_unix_socket") {
        c += &format!(" -S {}", q(&s));
    }
    if let Some(f) = st(a, "config_file") {
        c = c.replacen("mysql ", &format!("mysql --defaults-file={} ", q(&f)), 1);
    }
    c
}

fn sql(a: &Args, ctx: &Ctx, db: Option<&str>, stmt: &str) -> Result<Output> {
    let mut c = base_cmd(a);
    if let Some(d) = db {
        c += &format!(" {}", q(d));
    }
    ctx.conn.exec_stdin(&c, stmt.as_bytes())
}

fn sql_ok(
    a: &Args,
    ctx: &Ctx,
    db: Option<&str>,
    stmt: &str,
) -> Result<std::result::Result<String, String>> {
    let o = sql(a, ctx, db, stmt)?;
    Ok(if o.rc == 0 {
        Ok(o.stdout)
    } else {
        Err(format!(
            "unable to connect or run query: {}",
            o.stderr.trim()
        ))
    })
}

pub fn mysql_db(a: &Args, ctx: &Ctx) -> Result<Res> {
    let names = list(a, "name");
    if names.is_empty() {
        return Ok(fail("missing required arguments: name"));
    }
    let state = st(a, "state").unwrap_or_else(|| "present".into());
    let mut changed = false;
    for name in &names {
        let exists = match sql_ok(
            a,
            ctx,
            None,
            &format!("SHOW DATABASES LIKE '{}'", esc(name).replace('_', "\\_")),
        )? {
            Ok(o) => !o.trim().is_empty(),
            Err(e) => return Ok(fail(e)),
        };
        match state.as_str() {
            "present" => {
                if !exists {
                    changed = true;
                    if !ctx.check {
                        let mut s = format!("CREATE DATABASE {}", ident(name));
                        if let Some(e) = st(a, "encoding") {
                            s += &format!(" CHARACTER SET '{}'", esc(&e));
                        }
                        if let Some(c) = st(a, "collation") {
                            s += &format!(" COLLATE '{}'", esc(&c));
                        }
                        if let Err(e) = sql_ok(a, ctx, None, &s)? {
                            return Ok(fail(e));
                        }
                    }
                }
            }
            "absent" => {
                if exists {
                    changed = true;
                    if !ctx.check
                        && let Err(e) =
                            sql_ok(a, ctx, None, &format!("DROP DATABASE {}", ident(name)))?
                    {
                        return Ok(fail(e));
                    }
                }
            }
            "import" => {
                let target =
                    st(a, "target").ok_or_else(|| anyhow!("target required for import"))?;
                changed = true;
                if !ctx.check {
                    if !exists {
                        let _ = sql_ok(a, ctx, None, &format!("CREATE DATABASE {}", ident(name)))?;
                    }
                    let dec = if target.ends_with(".gz") {
                        format!("gunzip -c {} | ", q(&target))
                    } else {
                        format!("cat {} | ", q(&target))
                    };
                    let o = ctx
                        .conn
                        .exec(&format!("{dec}{} {}", base_cmd(a), q(name)))?;
                    if o.rc != 0 {
                        return Ok(fail(o.stderr.trim().to_string()));
                    }
                }
            }
            "dump" => {
                let target = st(a, "target").ok_or_else(|| anyhow!("target required for dump"))?;
                changed = true;
                if !ctx.check {
                    let mut dump = base_cmd(a).replacen("mysql -N -B", "mysqldump", 1);
                    dump += &format!(" {}", q(name));
                    let cmd = if target.ends_with(".gz") {
                        format!("{dump} | gzip > {}", q(&target))
                    } else {
                        format!("{dump} > {}", q(&target))
                    };
                    let o = ctx.conn.exec(&format!("set -o pipefail; {cmd}"))?;
                    if o.rc != 0 {
                        return Ok(fail(o.stderr.trim().to_string()));
                    }
                }
            }
            other => return Ok(fail(format!("unsupported state '{other}'"))),
        }
    }
    let mut r = res(changed);
    r.insert("db".into(), names.join(",").into());
    Ok(r)
}

/// "db.tbl:PRIV,PRIV/db2.*:ALL" -> {"`db`.`tbl`": {PRIV..}}
fn parse_priv(spec: &Value) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut add = |target: &str, privs: &str| {
        let (db, tbl) = target.rsplit_once('.').unwrap_or((target, "*"));
        let norm = |x: &str| {
            if x == "*" {
                "*".to_string()
            } else {
                ident(x.trim_matches('`'))
            }
        };
        let key = format!("{}.{}", norm(db), norm(tbl));
        let set = out.entry(key).or_default();
        for p in privs.split(',') {
            let p = p.trim().to_uppercase();
            let p = if p == "ALL PRIVILEGES" {
                "ALL".to_string()
            } else {
                p
            };
            if !p.is_empty() && p != "USAGE" {
                set.insert(p);
            }
        }
    };
    match spec {
        Value::String(s) => {
            for part in s.split('/') {
                if let Some((t, p)) = part.split_once(':') {
                    add(t, p);
                }
            }
        }
        Value::Object(m) => {
            for (t, p) in m {
                add(t, p.as_str().unwrap_or(""));
            }
        }
        _ => {}
    }
    out
}

fn current_grants(
    a: &Args,
    ctx: &Ctx,
    user: &str,
    host: &str,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let o = sql_ok(
        a,
        ctx,
        None,
        &format!("SHOW GRANTS FOR '{}'@'{}'", esc(user), esc(host)),
    )?
    .map_err(|e| anyhow!(e))?;
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for l in o.lines() {
        let l = l.trim();
        let Some(rest) = l.strip_prefix("GRANT ") else {
            continue;
        };
        let Some((privs, tail)) = rest.split_once(" ON ") else {
            continue;
        };
        let Some((target, _)) = tail.split_once(" TO ") else {
            continue;
        };
        let target = target.trim().to_string();
        let target = if target == "*.*" {
            "*.*".to_string()
        } else {
            target.replace('\'', "`")
        };
        let set = out.entry(target.clone()).or_default();
        for p in privs.split(',') {
            let p = p.trim().to_uppercase();
            let p = if p == "ALL PRIVILEGES" {
                "ALL".to_string()
            } else {
                p
            };
            if p != "USAGE" {
                set.insert(p);
            }
        }
        if l.contains("WITH GRANT OPTION") {
            out.get_mut(&target).unwrap().insert("GRANT".into());
        }
    }
    Ok(out)
}

pub fn mysql_user(a: &Args, ctx: &Ctx) -> Result<Res> {
    let name = st(a, "name").ok_or_else(|| anyhow!("missing required arguments: name"))?;
    let hosts: Vec<String> = if bo(a, "host_all", false) {
        let o = match sql_ok(
            a,
            ctx,
            None,
            &format!("SELECT Host FROM mysql.user WHERE User='{}'", esc(&name)),
        )? {
            Ok(o) => o,
            Err(e) => return Ok(fail(e)),
        };
        o.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    } else {
        vec![st(a, "host").unwrap_or_else(|| "localhost".into())]
    };
    let state = st(a, "state").unwrap_or_else(|| "present".into());
    let mut changed = false;
    for host in &hosts {
        let who = format!("'{}'@'{}'", esc(&name), esc(host));
        let exists = match sql_ok(
            a,
            ctx,
            None,
            &format!(
                "SELECT COUNT(*) FROM mysql.user WHERE User='{}' AND Host='{}'",
                esc(&name),
                esc(host)
            ),
        )? {
            Ok(o) => o.trim() != "0",
            Err(e) => return Ok(fail(e)),
        };
        if state == "absent" {
            if exists {
                changed = true;
                if !ctx.check
                    && let Err(e) = sql_ok(a, ctx, None, &format!("DROP USER {who}"))?
                {
                    return Ok(fail(e));
                }
            }
            continue;
        }
        let password = st(a, "password");
        let plugin = st(a, "plugin");
        if !exists {
            changed = true;
            if !ctx.check {
                let mut s = format!("CREATE USER {who}");
                match (&plugin, &password) {
                    (Some(pl), _) => {
                        s += &format!(" IDENTIFIED WITH '{}'", esc(pl));
                        if let Some(b) = st(a, "plugin_auth_string") {
                            s += &format!(" BY '{}'", esc(&b));
                        }
                    }
                    (None, Some(p)) if bo(a, "encrypted", false) => {
                        s += &format!(" IDENTIFIED BY PASSWORD '{}'", esc(p))
                    }
                    (None, Some(p)) => s += &format!(" IDENTIFIED BY '{}'", esc(p)),
                    _ => {}
                }
                if let Some(Value::Object(t)) = a.get("tls_requires")
                    && !t.is_empty()
                {
                    s += " REQUIRE ";
                    s += &t
                        .iter()
                        .map(|(k, v)| {
                            format!("{} '{}'", k.to_uppercase(), esc(v.as_str().unwrap_or("")))
                        })
                        .collect::<Vec<_>>()
                        .join(" AND ");
                }
                if let Err(e) = sql_ok(a, ctx, None, &s)? {
                    return Ok(fail(e));
                }
            }
        } else if let (Some(p), "always", None) = (
            &password,
            st(a, "update_password").as_deref().unwrap_or("always"),
            &plugin,
        ) {
            // idempotence: password already valid if we can log in with it
            let ok = if bo(a, "encrypted", false) {
                sql_ok(a, ctx, None, &format!("SELECT COUNT(*) FROM mysql.user WHERE User='{}' AND Host='{}' AND authentication_string='{}'", esc(&name), esc(host), esc(p)))?.map(|o| o.trim() == "1").unwrap_or(false)
            } else {
                let o = ctx.conn.exec(&format!(
                    "MYSQL_PWD={} mysql -N -B -u {} -h 127.0.0.1 -e 'select 1' 2>/dev/null",
                    q(p),
                    q(&name)
                ))?;
                o.rc == 0 && host != "%" || o.rc == 0
            };
            if !ok {
                changed = true;
                if !ctx.check {
                    let s = if bo(a, "encrypted", false) {
                        format!(
                            "ALTER USER {who} IDENTIFIED WITH mysql_native_password AS '{}'",
                            esc(p)
                        )
                    } else {
                        format!("ALTER USER {who} IDENTIFIED BY '{}'", esc(p))
                    };
                    if let Err(e) = sql_ok(a, ctx, None, &s)? {
                        return Ok(fail(e));
                    }
                }
            }
        }
        // privileges
        if let Some(pv) = a.get("priv").filter(|v| !v.is_null()) {
            let want = parse_priv(pv);
            let have = if exists {
                current_grants(a, ctx, &name, host)?
            } else {
                BTreeMap::new()
            };
            let append = bo(a, "append_privs", false);
            let mut stmts: Vec<String> = vec![];
            for (target, wp) in &want {
                let hp = have.get(target).cloned().unwrap_or_default();
                let missing: Vec<&String> = wp
                    .iter()
                    .filter(|p| !hp.contains(*p) && *p != "GRANT")
                    .collect();
                let extra: Vec<&String> = hp
                    .iter()
                    .filter(|p| !wp.contains(*p) && *p != "GRANT")
                    .collect();
                let grant_opt = wp.contains("GRANT");
                if !missing.is_empty() || (grant_opt && !hp.contains("GRANT")) {
                    let ps = missing
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let ps = if ps.is_empty() { "USAGE".into() } else { ps };
                    stmts.push(format!(
                        "GRANT {ps} ON {target} TO {who}{}",
                        if grant_opt { " WITH GRANT OPTION" } else { "" }
                    ));
                }
                if !append && !extra.is_empty() {
                    stmts.push(format!(
                        "REVOKE {} ON {target} FROM {who}",
                        extra
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            if !append {
                for (target, hp) in &have {
                    if !want.contains_key(target) && !hp.is_empty() {
                        stmts.push(format!("REVOKE ALL PRIVILEGES ON {target} FROM {who}"));
                    }
                }
            }
            if !stmts.is_empty() {
                changed = true;
                if !ctx.check {
                    stmts.push("FLUSH PRIVILEGES".into());
                    if let Err(e) = sql_ok(a, ctx, None, &stmts.join(";\n"))? {
                        return Ok(fail(e));
                    }
                }
            }
        }
    }
    let mut r = res(changed);
    r.insert("user".into(), name.into());
    Ok(r)
}

pub fn mysql_query(a: &Args, ctx: &Ctx) -> Result<Res> {
    let queries = list(a, "query");
    let db = st(a, "login_db");
    let mut results = vec![];
    let mut changed = false;
    let mut rowcount = vec![];
    for qy in &queries {
        let is_select = qy.trim_start().to_lowercase().starts_with("select")
            || qy.trim_start().to_lowercase().starts_with("show");
        let mut c = base_cmd(a).replace("-N -B", "-B");
        if let Some(d) = &db {
            c += &format!(" {}", q(d));
        }
        if ctx.check && !is_select {
            continue;
        }
        let o = ctx.conn.exec_stdin(&c, qy.as_bytes())?;
        if o.rc != 0 {
            return Ok(fail(format!(
                "Cannot execute SQL '{qy}': {}",
                o.stderr.trim()
            )));
        }
        if is_select {
            let mut ls = o.stdout.lines();
            let hdr: Vec<String> = ls
                .next()
                .map(|h| h.split('\t').map(String::from).collect())
                .unwrap_or_default();
            let rows: Vec<Value> = ls
                .map(|l| {
                    let mut m = Map::new();
                    for (h, v) in hdr.iter().zip(l.split('\t')) {
                        m.insert(
                            h.clone(),
                            if v == "NULL" {
                                Value::Null
                            } else {
                                v.parse::<i64>()
                                    .map(Value::from)
                                    .unwrap_or_else(|_| Value::String(v.to_string()))
                            },
                        );
                    }
                    Value::Object(m)
                })
                .collect();
            rowcount.push(rows.len());
            results.push(Value::Array(rows));
        } else {
            changed = true;
            rowcount.push(0);
        }
    }
    let mut r = res(changed);
    r.insert(
        "query".into(),
        queries.first().cloned().unwrap_or_default().into(),
    );
    r.insert("query_result".into(), Value::Array(results));
    r.insert("rowcount".into(), json!(rowcount));
    Ok(r)
}

pub fn mysql_variables(a: &Args, ctx: &Ctx) -> Result<Res> {
    let var = st(a, "variable").ok_or_else(|| anyhow!("missing variable"))?;
    let o = match sql_ok(
        a,
        ctx,
        None,
        &format!("SHOW VARIABLES LIKE '{}'", esc(&var)),
    )? {
        Ok(o) => o,
        Err(e) => return Ok(fail(e)),
    };
    let cur = o.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut r = res(false);
    r.insert("variable".into(), var.clone().into());
    r.insert("msg".into(), cur.clone().into());
    if let Some(v) = st(a, "value")
        && v != cur
    {
        if !ctx.check
            && let Err(e) = sql_ok(a, ctx, None, &format!("SET GLOBAL {} = '{}'", var, esc(&v)))?
        {
            return Ok(fail(e));
        }
        r.insert("changed".into(), true.into());
    }
    Ok(r)
}
