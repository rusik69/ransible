//! SQLite storage: schema, generic JSON CRUD helpers, auth helpers, secret encryption.
use anyhow::{Result, anyhow};
use rusqlite::{Connection, ToSql, params_from_iter, types::Value as SqlValue};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
  id INTEGER PRIMARY KEY, username TEXT UNIQUE NOT NULL, pw_hash TEXT NOT NULL,
  role TEXT NOT NULL DEFAULT 'viewer', created TEXT NOT NULL DEFAULT (datetime('now')));
CREATE TABLE IF NOT EXISTS tokens (
  id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, token_hash TEXT UNIQUE NOT NULL,
  description TEXT NOT NULL DEFAULT '', created TEXT NOT NULL DEFAULT (datetime('now')));
CREATE TABLE IF NOT EXISTS projects (
  id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL, description TEXT NOT NULL DEFAULT '',
  scm_url TEXT NOT NULL DEFAULT '', scm_branch TEXT NOT NULL DEFAULT '', local_path TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'never updated', last_sync TEXT, sync_output TEXT NOT NULL DEFAULT '',
  created TEXT NOT NULL DEFAULT (datetime('now')));
CREATE TABLE IF NOT EXISTS inventories (
  id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL, description TEXT NOT NULL DEFAULT '',
  kind TEXT NOT NULL DEFAULT 'static', content TEXT NOT NULL DEFAULT '',
  created TEXT NOT NULL DEFAULT (datetime('now')));
CREATE TABLE IF NOT EXISTS credentials (
  id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL, description TEXT NOT NULL DEFAULT '',
  kind TEXT NOT NULL DEFAULT 'machine', username TEXT NOT NULL DEFAULT '', secrets TEXT NOT NULL DEFAULT '',
  created TEXT NOT NULL DEFAULT (datetime('now')));
CREATE TABLE IF NOT EXISTS job_templates (
  id INTEGER PRIMARY KEY, name TEXT UNIQUE NOT NULL, description TEXT NOT NULL DEFAULT '',
  project_id INTEGER, playbook TEXT NOT NULL DEFAULT '', inventory_id INTEGER,
  credential_id INTEGER, vault_credential_id INTEGER,
  extra_vars TEXT NOT NULL DEFAULT '', limit_hosts TEXT NOT NULL DEFAULT '', tags TEXT NOT NULL DEFAULT '',
  skip_tags TEXT NOT NULL DEFAULT '', verbosity INTEGER NOT NULL DEFAULT 0, forks INTEGER NOT NULL DEFAULT 5,
  job_type TEXT NOT NULL DEFAULT 'run', become_enabled INTEGER NOT NULL DEFAULT 0, diff_mode INTEGER NOT NULL DEFAULT 0,
  created TEXT NOT NULL DEFAULT (datetime('now')));
CREATE TABLE IF NOT EXISTS jobs (
  id INTEGER PRIMARY KEY, template_id INTEGER, name TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'pending',
  launched_by TEXT NOT NULL DEFAULT '', job_type TEXT NOT NULL DEFAULT 'run', playbook TEXT NOT NULL DEFAULT '',
  params TEXT NOT NULL DEFAULT '{}', created TEXT NOT NULL DEFAULT (datetime('now')),
  started TEXT, finished TEXT, rc INTEGER, summary TEXT NOT NULL DEFAULT '[]');
CREATE TABLE IF NOT EXISTS job_events (
  id INTEGER PRIMARY KEY, job_id INTEGER NOT NULL, seq INTEGER NOT NULL, event TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS job_events_job ON job_events(job_id, seq);
"#;

pub fn open(path: &Path) -> Result<Connection> {
    let c = Connection::open(path)?;
    c.pragma_update(None, "journal_mode", "WAL")?;
    c.pragma_update(None, "foreign_keys", "ON")?;
    c.execute_batch(SCHEMA)?;
    Ok(c)
}

#[cfg(test)]
pub fn open_memory() -> Result<Connection> {
    let c = Connection::open_in_memory()?;
    c.execute_batch(SCHEMA)?;
    Ok(c)
}

fn to_json(v: SqlValue) -> Value {
    match v {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(i) => Value::from(i),
        SqlValue::Real(f) => Value::from(f),
        SqlValue::Text(s) => Value::String(s),
        SqlValue::Blob(b) => Value::String(hex::encode(b)),
    }
}

fn from_json(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => n
            .as_i64()
            .map(SqlValue::Integer)
            .unwrap_or_else(|| SqlValue::Real(n.as_f64().unwrap_or(0.0))),
        Value::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Text(other.to_string()),
    }
}

pub fn query(c: &Connection, sql: &str, params: &[SqlValue]) -> Result<Vec<Map<String, Value>>> {
    let mut st = c.prepare(sql)?;
    let cols: Vec<String> = st.column_names().map(|s| s.to_string()).collect();
    let rows = st.query_map(params_from_iter(params.iter()), |r| {
        let mut m = Map::new();
        for (i, name) in cols.iter().enumerate() {
            m.insert(name.clone(), to_json(r.get::<_, SqlValue>(i)?));
        }
        Ok(m)
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

pub fn get(c: &Connection, table: &str, id: i64) -> Result<Option<Map<String, Value>>> {
    Ok(query(
        c,
        &format!("SELECT * FROM {table} WHERE id = ?1"),
        &[SqlValue::Integer(id)],
    )?
    .into_iter()
    .next())
}

pub struct ListOpts {
    pub page: i64,
    pub page_size: i64,
    pub search: Option<String>,
    pub order_by: Option<String>,
}

/// Paginated list; `cols` whitelists order_by columns, search matches `name`.
pub fn list(
    c: &Connection,
    table: &str,
    cols: &[&str],
    o: &ListOpts,
) -> Result<(i64, Vec<Map<String, Value>>)> {
    let mut where_ = String::new();
    let mut p: Vec<SqlValue> = vec![];
    if let Some(s) = &o.search
        && cols.contains(&"name")
    {
        where_ = " WHERE name LIKE ?1".into();
        p.push(SqlValue::Text(format!("%{s}%")));
    }
    let total: i64 = c.query_row(
        &format!("SELECT COUNT(*) FROM {table}{where_}"),
        params_from_iter(p.iter()),
        |r| r.get(0),
    )?;
    let mut order = "id".to_string();
    let mut dir = "ASC";
    if let Some(ob) = &o.order_by {
        let (name, d) = ob
            .strip_prefix('-')
            .map(|n| (n, "DESC"))
            .unwrap_or((ob.as_str(), "ASC"));
        if name == "id" || cols.contains(&name) {
            order = name.to_string();
            dir = d;
        }
    }
    let sql = format!(
        "SELECT * FROM {table}{where_} ORDER BY {order} {dir} LIMIT {} OFFSET {}",
        o.page_size,
        (o.page - 1) * o.page_size
    );
    Ok((total, query(c, &sql, &p)?))
}

pub fn insert(
    c: &Connection,
    table: &str,
    cols: &[&str],
    body: &Map<String, Value>,
) -> Result<i64> {
    let mut names = vec![];
    let mut vals: Vec<SqlValue> = vec![];
    for col in cols {
        if let Some(v) = body.get(*col) {
            names.push(*col);
            vals.push(from_json(v));
        }
    }
    if names.is_empty() {
        return Err(anyhow!("no valid fields supplied"));
    }
    let ph: Vec<String> = (1..=names.len()).map(|i| format!("?{i}")).collect();
    c.execute(
        &format!(
            "INSERT INTO {table} ({}) VALUES ({})",
            names.join(","),
            ph.join(",")
        ),
        params_from_iter(vals.iter()),
    )?;
    Ok(c.last_insert_rowid())
}

pub fn update(
    c: &Connection,
    table: &str,
    cols: &[&str],
    id: i64,
    body: &Map<String, Value>,
) -> Result<bool> {
    let mut sets = vec![];
    let mut vals: Vec<SqlValue> = vec![];
    for col in cols {
        if let Some(v) = body.get(*col) {
            vals.push(from_json(v));
            sets.push(format!("{col} = ?{}", vals.len()));
        }
    }
    if sets.is_empty() {
        return Ok(get(c, table, id)?.is_some());
    }
    vals.push(SqlValue::Integer(id));
    let n = c.execute(
        &format!(
            "UPDATE {table} SET {} WHERE id = ?{}",
            sets.join(", "),
            vals.len()
        ),
        params_from_iter(vals.iter()),
    )?;
    Ok(n > 0)
}

pub fn delete(c: &Connection, table: &str, id: i64) -> Result<bool> {
    Ok(c.execute(
        &format!("DELETE FROM {table} WHERE id = ?1"),
        [&id as &dyn ToSql],
    )? > 0)
}

// ---------- passwords & tokens ----------

const ITER: u32 = 120_000;

pub fn hash_password(pw: &str) -> String {
    use rand::RngCore;
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(pw.as_bytes(), &salt, ITER, &mut out);
    format!(
        "pbkdf2-sha256${ITER}${}${}",
        hex::encode(salt),
        hex::encode(out)
    )
}

pub fn verify_password(pw: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 4 || parts[0] != "pbkdf2-sha256" {
        return false;
    }
    let (Ok(iter), Ok(salt), Ok(want)) = (
        parts[1].parse::<u32>(),
        hex::decode(parts[2]),
        hex::decode(parts[3]),
    ) else {
        return false;
    };
    let mut out = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(pw.as_bytes(), &salt, iter, &mut out);
    // constant-time compare
    out.len() == want.len() && out.iter().zip(&want).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
}

pub fn new_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

pub fn token_hash(t: &str) -> String {
    hex::encode(Sha256::digest(t.as_bytes()))
}

// ---------- secrets (Ansible-Vault format, keyed by the server master key) ----------

pub fn encrypt_secret(master: &str, plain: &str) -> Result<String> {
    crate::vault::encrypt(plain.as_bytes(), master, None)
}

pub fn decrypt_secret(master: &str, enc: &str) -> Result<String> {
    if enc.is_empty() {
        return Ok(String::new());
    }
    Ok(String::from_utf8(crate::vault::decrypt(enc, master)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn password_roundtrip() {
        let h = hash_password("s3cret");
        assert!(verify_password("s3cret", &h));
        assert!(!verify_password("nope", &h));
        assert!(!verify_password("s3cret", "garbage"));
    }

    #[test]
    fn secret_roundtrip() {
        let e = encrypt_secret("master", r#"{"password":"x"}"#).unwrap();
        assert!(e.starts_with("$ANSIBLE_VAULT"));
        assert_eq!(decrypt_secret("master", &e).unwrap(), r#"{"password":"x"}"#);
        assert!(decrypt_secret("other", &e).is_err());
    }

    #[test]
    fn crud_and_pagination() {
        let c = open_memory().unwrap();
        for n in ["a", "b", "c"] {
            insert(
                &c,
                "projects",
                &["name", "scm_url"],
                json!({"name": n, "scm_url": "u", "bogus": 1})
                    .as_object()
                    .unwrap(),
            )
            .unwrap();
        }
        let (total, rows) = list(
            &c,
            "projects",
            &["name"],
            &ListOpts {
                page: 1,
                page_size: 2,
                search: None,
                order_by: Some("-name".into()),
            },
        )
        .unwrap();
        assert_eq!(total, 3);
        assert_eq!(rows[0]["name"], "c");
        assert_eq!(rows.len(), 2);
        let (t, _) = list(
            &c,
            "projects",
            &["name"],
            &ListOpts {
                page: 1,
                page_size: 10,
                search: Some("b".into()),
                order_by: None,
            },
        )
        .unwrap();
        assert_eq!(t, 1);
        assert!(
            update(
                &c,
                "projects",
                &["name"],
                1,
                json!({"name": "z"}).as_object().unwrap()
            )
            .unwrap()
        );
        assert_eq!(get(&c, "projects", 1).unwrap().unwrap()["name"], "z");
        assert!(delete(&c, "projects", 1).unwrap());
        assert!(get(&c, "projects", 1).unwrap().is_none());
    }
}
