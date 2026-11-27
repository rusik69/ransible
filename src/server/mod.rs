//! `ransible serve`: AWX-style REST API (`/api/v2/`) and embedded web UI in the same binary.
mod cron;
mod db;
mod jobs;

use anyhow::{Context, Result, anyhow};
use axum::{
    Json, Router,
    extract::{FromRequestParts, Path, Query, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::{IntoResponse, Response, Sse, sse},
    routing::{get, post},
};
use futures_util::StreamExt;
use rusqlite::{Connection, types::Value as SqlValue};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(rust_embed::RustEmbed)]
#[folder = "web/"]
struct Assets;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Mutex<Connection>>,
    pub master: Arc<String>,
    pub data_dir: Arc<PathBuf>,
    pub live: Arc<Mutex<HashMap<i64, jobs::Live>>>,
    pub sem: Arc<tokio::sync::Semaphore>,
    pub login_failures: Arc<Mutex<HashMap<String, Vec<std::time::Instant>>>>,
}

impl AppState {
    /// Append to the audit trail (activity stream).
    pub fn activity(&self, user: &str, action: &str, otype: &str, oid: i64, summary: &str) {
        if let Ok(c) = self.db.lock() {
            let _ = c.execute(
                "INSERT INTO activity_stream (username, action, object_type, object_id, summary) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![user, action, otype, oid, summary],
            );
        }
    }
}

pub struct Options {
    pub listen: String,
    pub data_dir: PathBuf,
    pub admin_password: Option<String>,
    pub max_jobs: usize,
}

// ---------- errors ----------

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"detail": self.1}))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        let msg = format!("{e:#}");
        if msg.contains("UNIQUE constraint") {
            return ApiError(
                StatusCode::BAD_REQUEST,
                "a resource with that name already exists".into(),
            );
        }
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        anyhow::Error::from(e).into()
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}
fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "Not found.".into())
}
fn forbidden() -> ApiError {
    ApiError(
        StatusCode::FORBIDDEN,
        "You do not have permission to perform this action.".into(),
    )
}

type R<T = Json<Value>> = Result<T, ApiError>;

// ---------- auth ----------

#[derive(Clone, Debug)]
pub struct AuthUser {
    pub id: i64,
    pub username: String,
    pub role: String,
}

impl AuthUser {
    fn is_admin(&self) -> bool {
        self.role == "admin"
    }
    /// operate = launch/sync/cancel; write = manage resources
    fn can(&self, level: &str) -> bool {
        match level {
            "read" => true,
            "operate" => self.role == "admin" || self.role == "operator",
            _ => self.role == "admin",
        }
    }
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let unauth = || {
            ApiError(
                StatusCode::UNAUTHORIZED,
                "Authentication credentials were not provided.".into(),
            )
        };
        let mut token: Option<String> = None;
        let mut basic: Option<(String, String)> = None;
        if let Some(h) = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
        {
            if let Some(t) = h.strip_prefix("Bearer ") {
                token = Some(t.trim().to_string());
            } else if let Some(b) = h.strip_prefix("Basic ") {
                basic = decode_basic(b.trim());
            }
        }
        if token.is_none() && basic.is_none() {
            // EventSource cannot set headers: accept ?token=
            if let Some(q) = parts.uri.query() {
                token = q
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("token="))
                    .map(String::from);
            }
        }
        let c = state.db.lock().unwrap();
        if let Some(t) = token {
            let rows = db::query(
                &c,
                "SELECT u.id, u.username, u.role FROM tokens t JOIN users u ON u.id = t.user_id WHERE t.token_hash = ?1",
                &[SqlValue::Text(db::token_hash(&t))],
            )?;
            let r = rows.into_iter().next().ok_or_else(unauth)?;
            return Ok(AuthUser {
                id: r["id"].as_i64().unwrap_or(0),
                username: r["username"].as_str().unwrap_or("").into(),
                role: r["role"].as_str().unwrap_or("viewer").into(),
            });
        }
        if let Some((u, p)) = basic {
            let rows = db::query(
                &c,
                "SELECT id, username, role, pw_hash FROM users WHERE username = ?1",
                &[SqlValue::Text(u)],
            )?;
            let r = rows.into_iter().next().ok_or_else(unauth)?;
            if db::verify_password(&p, r["pw_hash"].as_str().unwrap_or("")) {
                return Ok(AuthUser {
                    id: r["id"].as_i64().unwrap_or(0),
                    username: r["username"].as_str().unwrap_or("").into(),
                    role: r["role"].as_str().unwrap_or("viewer").into(),
                });
            }
        }
        Err(unauth())
    }
}

fn decode_basic(b64: &str) -> Option<(String, String)> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let s = String::from_utf8(raw).ok()?;
    let (u, p) = s.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

// ---------- resource table ----------

struct Res {
    table: &'static str,
    cols: &'static [&'static str],
    /// who may create/update/delete
    write: &'static str,
}

fn resource(name: &str) -> Option<Res> {
    Some(match name {
        "projects" => Res {
            table: "projects",
            cols: &["name", "description", "scm_url", "scm_branch", "local_path"],
            write: "write",
        },
        "inventories" => Res {
            table: "inventories",
            cols: &["name", "description", "kind", "content"],
            write: "write",
        },
        "credentials" => Res {
            table: "credentials",
            cols: &["name", "description", "kind", "username"],
            write: "write",
        },
        "job_templates" => Res {
            table: "job_templates",
            cols: &[
                "name",
                "description",
                "project_id",
                "playbook",
                "inventory_id",
                "credential_id",
                "vault_credential_id",
                "extra_vars",
                "limit_hosts",
                "tags",
                "skip_tags",
                "verbosity",
                "forks",
                "job_type",
                "become_enabled",
                "diff_mode",
            ],
            write: "write",
        },
        "schedules" => Res {
            table: "schedules",
            cols: &[
                "name",
                "description",
                "template_id",
                "cron",
                "enabled",
                "extra_vars",
            ],
            write: "write",
        },
        "notification_templates" => Res {
            table: "notification_templates",
            cols: &[
                "name",
                "description",
                "kind",
                "enabled",
                "on_start",
                "on_success",
                "on_failure",
            ],
            write: "write",
        },
        "activity_stream" => Res {
            table: "activity_stream",
            cols: &["ts", "username", "action", "object_type"],
            write: "write",
        },
        "users" => Res {
            table: "users",
            cols: &["username", "role"],
            write: "write",
        },
        "jobs" => Res {
            table: "jobs",
            cols: &[
                "name",
                "status",
                "launched_by",
                "created",
                "started",
                "finished",
            ],
            write: "write",
        },
        _ => return None,
    })
}

fn sanitize(state: &AppState, res: &str, mut row: Map<String, Value>) -> Map<String, Value> {
    row.remove("pw_hash");
    if res == "credentials" {
        let keys: Vec<String> = db::decrypt_secret(
            &state.master,
            row.get("secrets").and_then(|s| s.as_str()).unwrap_or(""),
        )
        .ok()
        .and_then(|t| serde_json::from_str::<Map<String, Value>>(&t).ok())
        .map(|m| {
            m.into_iter()
                .filter(|(_, v)| v.as_str().map(|s| !s.is_empty()).unwrap_or(false))
                .map(|(k, _)| k)
                .collect()
        })
        .unwrap_or_default();
        row.insert(
            "secrets".into(),
            Value::Object(
                keys.into_iter()
                    .map(|k| (k, Value::String("$encrypted$".into())))
                    .collect(),
            ),
        );
    }
    if res == "notification_templates" {
        let set = row
            .get("webhook_url")
            .and_then(|u| u.as_str())
            .map(|u| !u.is_empty())
            .unwrap_or(false);
        row.insert(
            "webhook_url".into(),
            if set { "$encrypted$".into() } else { "".into() },
        );
    }
    if res == "jobs" {
        if let Some(Value::String(p)) = row.get("params").cloned() {
            row.insert(
                "params".into(),
                serde_json::from_str(&p).unwrap_or(Value::Null),
            );
        }
        if let Some(Value::String(p)) = row.get("summary").cloned() {
            row.insert(
                "summary".into(),
                serde_json::from_str(&p).unwrap_or(Value::Null),
            );
        }
    }
    let id = row.get("id").and_then(|i| i.as_i64()).unwrap_or(0);
    row.insert("url".into(), format!("/api/v2/{res}/{id}/").into());
    row
}

#[derive(serde::Deserialize, Default)]
struct ListQuery {
    page: Option<i64>,
    page_size: Option<i64>,
    search: Option<String>,
    order_by: Option<String>,
}

async fn list_res(
    State(st): State<AppState>,
    _u: AuthUser,
    Path(name): Path<String>,
    Query(q): Query<ListQuery>,
) -> R {
    let res = resource(&name).ok_or_else(not_found)?;
    let o = db::ListOpts {
        page: q.page.unwrap_or(1).max(1),
        page_size: q.page_size.unwrap_or(25).clamp(1, 200),
        search: q.search.clone(),
        order_by: q.order_by.clone(),
    };
    let (total, rows) = {
        let c = st.db.lock().unwrap();
        let mut cols: Vec<&str> = res.cols.to_vec();
        if name == "jobs" {
            cols.extend(["id", "status"]);
        }
        db::list(&c, res.table, &cols, &o)?
    };
    let results: Vec<Value> = rows
        .into_iter()
        .map(|r| Value::Object(sanitize(&st, &name, r)))
        .collect();
    let next = (o.page * o.page_size < total).then(|| {
        format!(
            "/api/v2/{name}/?page={}&page_size={}",
            o.page + 1,
            o.page_size
        )
    });
    let prev = (o.page > 1).then(|| {
        format!(
            "/api/v2/{name}/?page={}&page_size={}",
            o.page - 1,
            o.page_size
        )
    });
    Ok(Json(
        json!({"count": total, "next": next, "previous": prev, "results": results}),
    ))
}

async fn get_res(
    State(st): State<AppState>,
    _u: AuthUser,
    Path((name, id)): Path<(String, i64)>,
) -> R {
    let res = resource(&name).ok_or_else(not_found)?;
    let row = {
        let c = st.db.lock().unwrap();
        db::get(&c, res.table, id)?
    };
    Ok(Json(Value::Object(sanitize(
        &st,
        &name,
        row.ok_or_else(not_found)?,
    ))))
}

fn prepare_body(
    st: &AppState,
    name: &str,
    body: &mut Map<String, Value>,
    existing: Option<&Map<String, Value>>,
) -> Result<(), ApiError> {
    match name {
        "users" => {
            if let Some(role) = body.get("role").and_then(|r| r.as_str())
                && !["admin", "operator", "viewer"].contains(&role)
            {
                return Err(bad("role must be admin, operator or viewer"));
            }
            if let Some(pw) = body.get("password").and_then(|p| p.as_str()) {
                if pw.len() < 8 {
                    return Err(bad("password must be at least 8 characters"));
                }
                body.insert("pw_hash".into(), db::hash_password(pw).into());
            }
        }
        "credentials" => {
            if let Some(Value::Object(new)) = body.get("secrets").cloned() {
                let mut cur: Map<String, Value> = existing
                    .and_then(|e| {
                        db::decrypt_secret(
                            &st.master,
                            e.get("secrets").and_then(|s| s.as_str()).unwrap_or(""),
                        )
                        .ok()
                    })
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_default();
                for (k, v) in new {
                    if v.as_str() == Some("$encrypted$") {
                        continue;
                    }
                    cur.insert(k, v);
                }
                body.insert(
                    "secrets".into(),
                    db::encrypt_secret(&st.master, &Value::Object(cur).to_string())?.into(),
                );
            }
            if let Some(k) = body.get("kind").and_then(|k| k.as_str())
                && !["machine", "vault"].contains(&k)
            {
                return Err(bad("kind must be machine or vault"));
            }
        }
        "schedules" => {
            if let Some(c) = body.get("cron").and_then(|c| c.as_str())
                && !cron::validate(c)
            {
                return Err(bad(
                    "cron must be 5 fields (min hour dom mon dow) or @hourly/@daily/@weekly/@monthly",
                ));
            }
            if let Some(Value::Bool(b)) = body.get("enabled").cloned() {
                body.insert("enabled".into(), (b as i64).into());
            }
            if let Some(ev) = body.get("extra_vars")
                && !ev.is_string()
            {
                body.insert("extra_vars".into(), ev.to_string().into());
            }
        }
        "notification_templates" => {
            if let Some(k) = body.get("kind").and_then(|k| k.as_str())
                && !["webhook", "slack"].contains(&k)
            {
                return Err(bad("kind must be webhook or slack"));
            }
            for k in ["enabled", "on_start", "on_success", "on_failure"] {
                if let Some(Value::Bool(b)) = body.get(k).cloned() {
                    body.insert(k.into(), (b as i64).into());
                }
            }
            if let Some(u) = body
                .get("webhook_url")
                .and_then(|u| u.as_str())
                .map(String::from)
            {
                if u == "$encrypted$" {
                    body.remove("webhook_url");
                } else if !(u.starts_with("http://") || u.starts_with("https://")) {
                    return Err(bad("webhook_url must start with http:// or https://"));
                } else {
                    body.insert(
                        "webhook_url".into(),
                        db::encrypt_secret(&st.master, &u)?.into(),
                    );
                }
            }
        }
        "inventories" => {
            if let Some(k) = body.get("kind").and_then(|k| k.as_str())
                && !["static", "script"].contains(&k)
            {
                return Err(bad("kind must be static or script"));
            }
        }
        "job_templates" => {
            if let Some(ev) = body.get("extra_vars") {
                let text = match ev {
                    Value::String(s) => s.clone(),
                    o => o.to_string(),
                };
                if !text.trim().is_empty() && crate::playbook::parse_yaml_str(&text).is_err() {
                    return Err(bad("extra_vars is not valid YAML/JSON"));
                }
                body.insert("extra_vars".into(), text.into());
            }
            for k in ["become_enabled", "diff_mode"] {
                if let Some(b) = body.get(k).and_then(|b| b.as_bool()) {
                    body.insert(k.into(), (b as i64).into());
                }
            }
            if let Some(t) = body.get("job_type").and_then(|t| t.as_str())
                && !["run", "check"].contains(&t)
            {
                return Err(bad("job_type must be run or check"));
            }
        }
        _ => {}
    }
    Ok(())
}

fn extra_cols(name: &str) -> &'static [&'static str] {
    match name {
        "users" => &["pw_hash"],
        "credentials" => &["secrets"],
        "notification_templates" => &["webhook_url"],
        _ => &[],
    }
}

async fn create_res(
    State(st): State<AppState>,
    u: AuthUser,
    Path(name): Path<String>,
    Json(mut body): Json<Map<String, Value>>,
) -> R<(StatusCode, Json<Value>)> {
    let res = resource(&name).ok_or_else(not_found)?;
    if name == "jobs" || name == "activity_stream" || !u.can(res.write) {
        return Err(forbidden());
    }
    if name == "users" && !body.contains_key("password") {
        return Err(bad("password is required"));
    }
    let key = if name == "users" { "username" } else { "name" };
    if body
        .get(key)
        .and_then(|n| n.as_str())
        .map(|n| n.trim().is_empty())
        .unwrap_or(true)
    {
        return Err(bad(format!("{key} is required")));
    }
    prepare_body(&st, &name, &mut body, None)?;
    let mut cols: Vec<&str> = res.cols.to_vec();
    cols.extend(extra_cols(&name));
    let row = {
        let c = st.db.lock().unwrap();
        let id = db::insert(&c, res.table, &cols, &body)?;
        db::get(&c, res.table, id)?.ok_or_else(not_found)?
    };
    let rid = row.get("id").and_then(|i| i.as_i64()).unwrap_or(0);
    st.activity(
        &u.username,
        "create",
        &name,
        rid,
        &format!(
            "created {} {}",
            name,
            row.get("name")
                .or(row.get("username"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
        ),
    );
    Ok((
        StatusCode::CREATED,
        Json(Value::Object(sanitize(&st, &name, row))),
    ))
}

async fn update_res(
    State(st): State<AppState>,
    u: AuthUser,
    Path((name, id)): Path<(String, i64)>,
    Json(mut body): Json<Map<String, Value>>,
) -> R {
    let res = resource(&name).ok_or_else(not_found)?;
    if name == "jobs" || name == "activity_stream" || !u.can(res.write) {
        return Err(forbidden());
    }
    let existing = {
        let c = st.db.lock().unwrap();
        db::get(&c, res.table, id)?.ok_or_else(not_found)?
    };
    prepare_body(&st, &name, &mut body, Some(&existing))?;
    let mut cols: Vec<&str> = res.cols.to_vec();
    cols.extend(extra_cols(&name));
    let row = {
        let c = st.db.lock().unwrap();
        db::update(&c, res.table, &cols, id, &body)?;
        db::get(&c, res.table, id)?.ok_or_else(not_found)?
    };
    st.activity(
        &u.username,
        "update",
        &name,
        id,
        &format!(
            "updated {} {}",
            name,
            row.get("name")
                .or(row.get("username"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
        ),
    );
    Ok(Json(Value::Object(sanitize(&st, &name, row))))
}

async fn delete_res(
    State(st): State<AppState>,
    u: AuthUser,
    Path((name, id)): Path<(String, i64)>,
) -> R<StatusCode> {
    let res = resource(&name).ok_or_else(not_found)?;
    if name == "activity_stream" || !u.can(res.write) {
        return Err(forbidden());
    }
    if name == "users" && id == u.id {
        return Err(bad("you cannot delete yourself"));
    }
    let c = st.db.lock().unwrap();
    if name == "jobs" {
        c.execute("DELETE FROM job_events WHERE job_id = ?1", [id])?;
    }
    if name == "users" {
        c.execute("DELETE FROM tokens WHERE user_id = ?1", [id])?;
    }
    if !db::delete(&c, res.table, id)? {
        return Err(not_found());
    }
    drop(c);
    st.activity(
        &u.username,
        "delete",
        &name,
        id,
        &format!("deleted {name} {id}"),
    );
    Ok(StatusCode::NO_CONTENT)
}

// ---------- auth endpoints ----------

async fn ping() -> Json<Value> {
    Json(json!({"ha": false, "version": env!("CARGO_PKG_VERSION"), "active_node": "ransible"}))
}

async fn login(State(st): State<AppState>, Json(body): Json<Map<String, Value>>) -> R {
    let user = body
        .get("username")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    let pass = body
        .get("password")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    {
        let mut f = st.login_failures.lock().unwrap();
        let v = f.entry(user.clone()).or_default();
        v.retain(|t| t.elapsed().as_secs() < 60);
        if v.len() >= 5 {
            return Err(ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many failed logins; try again in a minute.".into(),
            ));
        }
    }
    let row = {
        let c = st.db.lock().unwrap();
        db::query(
            &c,
            "SELECT id, username, role, pw_hash FROM users WHERE username = ?1",
            &[SqlValue::Text(user.clone())],
        )?
        .into_iter()
        .next()
    };
    let ok = row
        .as_ref()
        .map(|r| db::verify_password(&pass, r["pw_hash"].as_str().unwrap_or("")))
        .unwrap_or_else(|| {
            let _ = db::verify_password(&pass, "pbkdf2-sha256$120000$00$00"); // timing parity
            false
        });
    if !ok {
        st.login_failures
            .lock()
            .unwrap()
            .entry(user)
            .or_default()
            .push(std::time::Instant::now());
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "Invalid username or password.".into(),
        ));
    }
    let row = row.unwrap();
    let token = db::new_token();
    {
        let c = st.db.lock().unwrap();
        c.execute(
            "INSERT INTO tokens (user_id, token_hash, description) VALUES (?1, ?2, 'login')",
            rusqlite::params![row["id"].as_i64(), db::token_hash(&token)],
        )?;
    }
    Ok(Json(
        json!({"token": token, "user": {"id": row["id"], "username": row["username"], "role": row["role"]}}),
    ))
}

async fn me(u: AuthUser) -> Json<Value> {
    Json(json!({"id": u.id, "username": u.username, "role": u.role, "is_superuser": u.is_admin()}))
}

async fn logout(State(st): State<AppState>, headers: HeaderMap) -> R<StatusCode> {
    if let Some(t) = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
    {
        let c = st.db.lock().unwrap();
        c.execute(
            "DELETE FROM tokens WHERE token_hash = ?1",
            [db::token_hash(t.trim())],
        )?;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------- projects / inventories ----------

async fn sync_project(State(st): State<AppState>, u: AuthUser, Path(id): Path<i64>) -> R {
    if !u.can("operate") {
        return Err(forbidden());
    }
    let project = {
        let c = st.db.lock().unwrap();
        db::get(&c, "projects", id)?.ok_or_else(not_found)?
    };
    let dir = jobs::project_dir(&st, &project);
    let url = project["scm_url"].as_str().unwrap_or("").to_string();
    let branch = project["scm_branch"].as_str().unwrap_or("").to_string();
    let out = tokio::task::spawn_blocking(move || -> Result<String> {
        if url.is_empty() {
            return if dir.is_dir() {
                Ok(format!("Using local path {}", dir.display()))
            } else {
                Err(anyhow!("local path {} does not exist", dir.display()))
            };
        }
        let git = |args: &[&str]| -> Result<String> {
            let o = std::process::Command::new("git")
                .args(args)
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .context("running git")?;
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            if o.status.success() {
                Ok(text)
            } else {
                Err(anyhow!("git {} failed: {}", args.join(" "), text.trim()))
            }
        };
        let d = dir.display().to_string();
        let mut log = String::new();
        if dir.join(".git").is_dir() {
            let b = if branch.is_empty() { "HEAD" } else { &branch };
            log += &git(&["-C", &d, "fetch", "--depth", "1", "origin", b])?;
            log += &git(&["-C", &d, "reset", "--hard", "FETCH_HEAD"])?;
        } else {
            std::fs::create_dir_all(dir.parent().unwrap_or(&dir))?;
            let mut args = vec!["clone", "--depth", "1"];
            if !branch.is_empty() {
                args.extend(["--branch", &branch]);
            }
            args.extend([url.as_str(), d.as_str()]);
            log += &git(&args)?;
        }
        let req = dir.join("roles/requirements.yml");
        if req.is_file() {
            log += "Installing roles from roles/requirements.yml\n";
            crate::galaxy::install_file(&req, &dir.join("roles"), false)?;
        }
        Ok(log)
    })
    .await
    .map_err(|e| anyhow!("{e}"))?;
    let (status, log) = match &out {
        Ok(l) => ("successful", l.clone()),
        Err(e) => ("failed", format!("{e:#}")),
    };
    {
        let c = st.db.lock().unwrap();
        c.execute("UPDATE projects SET status = ?1, last_sync = datetime('now'), sync_output = ?2 WHERE id = ?3", rusqlite::params![status, log, id])?;
    }
    Ok(Json(json!({"status": status, "output": log})))
}

async fn project_playbooks(State(st): State<AppState>, _u: AuthUser, Path(id): Path<i64>) -> R {
    let project = {
        let c = st.db.lock().unwrap();
        db::get(&c, "projects", id)?.ok_or_else(not_found)?
    };
    let dir = jobs::project_dir(&st, &project);
    let mut found = vec![];
    fn walk(base: &std::path::Path, dir: &std::path::Path, depth: usize, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            if p.is_dir() {
                if depth < 3
                    && ![
                        "roles",
                        "group_vars",
                        "host_vars",
                        "collections",
                        "templates",
                        "files",
                        "vars",
                        "tasks",
                        "handlers",
                        "defaults",
                        "meta",
                    ]
                    .contains(&name.as_str())
                {
                    walk(base, &p, depth + 1, out);
                }
            } else if (name.ends_with(".yml") || name.ends_with(".yaml"))
                && std::fs::read_to_string(&p)
                    .map(|t| t.contains("hosts:"))
                    .unwrap_or(false)
                && let Ok(r) = p.strip_prefix(base)
            {
                out.push(r.display().to_string());
            }
        }
    }
    walk(&dir, &dir, 0, &mut found);
    found.sort();
    Ok(Json(json!(found)))
}

async fn inventory_hosts(State(st): State<AppState>, _u: AuthUser, Path(id): Path<i64>) -> R {
    let inv = {
        let c = st.db.lock().unwrap();
        db::get(&c, "inventories", id)?.ok_or_else(not_found)?
    };
    if inv["kind"].as_str() == Some("script") {
        return Ok(Json(
            json!({"hosts": [], "groups": {}, "note": "dynamic inventory is evaluated at launch time"}),
        ));
    }
    let content = inv["content"].as_str().unwrap_or("");
    let ext = if content.trim_start().starts_with("---")
        || content
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim_end().ends_with(':') && !l.starts_with('['))
            .unwrap_or(false)
    {
        "yml"
    } else {
        "ini"
    };
    let tmp = st
        .data_dir
        .join(format!("inv-preview-{id}-{}.{ext}", std::process::id()));
    std::fs::write(&tmp, content).map_err(|e| anyhow!("{e}"))?;
    let loaded = crate::inventory::Inventory::load(&tmp.display().to_string());
    let _ = std::fs::remove_file(&tmp);
    let inv = loaded.map_err(|e| bad(format!("{e:#}")))?;
    let groups: Map<String, Value> = inv
        .groups
        .keys()
        .map(|g| {
            (
                g.clone(),
                json!(inv.group_hosts(g).into_iter().collect::<Vec<_>>()),
            )
        })
        .collect();
    let _ = groups.len();
    Ok(Json(json!({"hosts": inv.hosts, "groups": groups})))
}

// ---------- jobs ----------

/// Insert a job for `template` (with launch-time overrides) and start it.
fn create_job(
    st: &AppState,
    template: &Map<String, Value>,
    overrides: &Map<String, Value>,
    by: &str,
) -> Result<i64> {
    let params = jobs::resolve_launch(template, overrides);
    let job_type = params["job_type"].as_str().unwrap_or("run").to_string();
    let job_id = {
        let c = st.db.lock().unwrap();
        c.execute(
            "INSERT INTO jobs (template_id, name, launched_by, job_type, playbook, params) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![template["id"].as_i64(), template["name"].as_str().unwrap_or(""), by, job_type, template["playbook"].as_str().unwrap_or(""), params.to_string()],
        )?;
        c.last_insert_rowid()
    };
    tokio::spawn(jobs::run_job(st.clone(), job_id));
    Ok(job_id)
}

async fn launch(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
    body: axum::body::Bytes,
) -> R<(StatusCode, Json<Value>)> {
    if !u.can("operate") {
        return Err(forbidden());
    }
    let overrides: Map<String, Value> = if body.iter().all(|b| b.is_ascii_whitespace()) {
        Map::new()
    } else {
        serde_json::from_slice(&body).map_err(|e| bad(format!("invalid JSON body: {e}")))?
    };
    let template = {
        let c = st.db.lock().unwrap();
        db::get(&c, "job_templates", id)?.ok_or_else(not_found)?
    };
    if template["project_id"].is_null() || template["playbook"].as_str().unwrap_or("").is_empty() {
        return Err(bad("template needs a project and a playbook"));
    }
    let job_id = create_job(&st, &template, &overrides, &u.username)?;
    st.activity(
        &u.username,
        "launch",
        "job",
        job_id,
        &format!(
            "launched template {}",
            template["name"].as_str().unwrap_or("")
        ),
    );
    let row = {
        let c = st.db.lock().unwrap();
        db::get(&c, "jobs", job_id)?.ok_or_else(not_found)?
    };
    Ok((
        StatusCode::CREATED,
        Json(Value::Object(sanitize(&st, "jobs", row))),
    ))
}

async fn relaunch(
    State(st): State<AppState>,
    u: AuthUser,
    Path(id): Path<i64>,
) -> R<(StatusCode, Json<Value>)> {
    if !u.can("operate") {
        return Err(forbidden());
    }
    let (job,) = {
        let c = st.db.lock().unwrap();
        (db::get(&c, "jobs", id)?.ok_or_else(not_found)?,)
    };
    let job_id = {
        let c = st.db.lock().unwrap();
        c.execute(
            "INSERT INTO jobs (template_id, name, launched_by, job_type, playbook, params) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![job["template_id"].as_i64(), job["name"].as_str().unwrap_or(""), u.username, job["job_type"].as_str().unwrap_or("run"), job["playbook"].as_str().unwrap_or(""), job["params"].as_str().unwrap_or("{}")],
        )?;
        c.last_insert_rowid()
    };
    tokio::spawn(jobs::run_job(st.clone(), job_id));
    st.activity(
        &u.username,
        "relaunch",
        "job",
        job_id,
        &format!("relaunched job {id}"),
    );
    let row = {
        let c = st.db.lock().unwrap();
        db::get(&c, "jobs", job_id)?.ok_or_else(not_found)?
    };
    Ok((
        StatusCode::CREATED,
        Json(Value::Object(sanitize(&st, "jobs", row))),
    ))
}

async fn cancel_job(State(st): State<AppState>, u: AuthUser, Path(id): Path<i64>) -> R {
    if !u.can("operate") {
        return Err(forbidden());
    }
    let cancel = st.live.lock().unwrap().get(&id).map(|l| l.cancel.clone());
    match cancel {
        Some(n) => {
            st.activity(&u.username, "cancel", "job", id, "canceled job");
            n.notify_one();
            Ok(Json(json!({"canceled": true})))
        }
        None => Err(bad("job is not running")),
    }
}

fn job_events_text(st: &AppState, id: i64, after: i64) -> Result<Vec<(i64, String)>> {
    let c = st.db.lock().unwrap();
    let rows = db::query(
        &c,
        "SELECT seq, event FROM job_events WHERE job_id = ?1 AND seq > ?2 ORDER BY seq",
        &[SqlValue::Integer(id), SqlValue::Integer(after)],
    )?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let ev: crate::events::Event = serde_json::from_str(r["event"].as_str().unwrap_or(""))
                .unwrap_or(crate::events::Event::Message {
                    text: String::new(),
                });
            (r["seq"].as_i64().unwrap_or(0), jobs::render(&ev))
        })
        .collect())
}

async fn job_stdout(State(st): State<AppState>, _u: AuthUser, Path(id): Path<i64>) -> R<Response> {
    {
        let c = st.db.lock().unwrap();
        db::get(&c, "jobs", id)?.ok_or_else(not_found)?;
    }
    let text: String = job_events_text(&st, id, 0)?
        .into_iter()
        .map(|(_, t)| t)
        .collect();
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response())
}

async fn job_events(State(st): State<AppState>, _u: AuthUser, Path(id): Path<i64>) -> R {
    let rows = {
        let c = st.db.lock().unwrap();
        db::get(&c, "jobs", id)?.ok_or_else(not_found)?;
        db::query(
            &c,
            "SELECT seq, event FROM job_events WHERE job_id = ?1 ORDER BY seq",
            &[SqlValue::Integer(id)],
        )?
    };
    let results: Vec<Value> = rows.into_iter().map(|r| json!({"counter": r["seq"], "event": serde_json::from_str::<Value>(r["event"].as_str().unwrap_or("null")).unwrap_or(Value::Null)})).collect();
    Ok(Json(
        json!({"count": results.len(), "next": null, "previous": null, "results": results}),
    ))
}

async fn job_stream(
    State(st): State<AppState>,
    _u: AuthUser,
    Path(id): Path<i64>,
) -> R<Sse<impl futures_util::Stream<Item = Result<sse::Event, Infallible>>>> {
    {
        let c = st.db.lock().unwrap();
        db::get(&c, "jobs", id)?.ok_or_else(not_found)?;
    }
    // subscribe first, then replay what is stored, then follow live output
    let rx = st.live.lock().unwrap().get(&id).map(|l| l.tx.subscribe());
    let replay = job_events_text(&st, id, 0)?;
    let last = replay.last().map(|r| r.0).unwrap_or(0);
    let replay_stream = futures_util::stream::iter(replay);
    let live_stream = match rx {
        Some(rx) => tokio_stream::wrappers::BroadcastStream::new(rx)
            .filter_map(|m| async move { m.ok() })
            .boxed(),
        None => futures_util::stream::empty().boxed(),
    };
    let chunks = replay_stream
        .chain(live_stream.filter(move |(seq, _)| futures_util::future::ready(*seq > last)));
    let st2 = st.clone();
    let events = chunks
        .map(|(seq, text)| {
            Ok(sse::Event::default()
                .id(seq.to_string())
                .data(json!({"seq": seq, "text": text}).to_string()))
        })
        .chain(futures_util::stream::once(async move {
            let status = {
                let c = st2.db.lock().unwrap();
                db::get(&c, "jobs", id)
                    .ok()
                    .flatten()
                    .and_then(|j| j["status"].as_str().map(String::from))
                    .unwrap_or_default()
            };
            Ok(sse::Event::default().event("done").data(status))
        }));
    Ok(Sse::new(events).keep_alive(sse::KeepAlive::default()))
}

async fn dashboard(State(st): State<AppState>, _u: AuthUser) -> R {
    let c = st.db.lock().unwrap();
    let count = |t: &str| -> i64 {
        c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))
            .unwrap_or(0)
    };
    let by_status = db::query(
        &c,
        "SELECT status, COUNT(*) AS n FROM jobs GROUP BY status",
        &[],
    )?;
    let recent = db::query(
        &c,
        "SELECT id, name, status, launched_by, created, finished FROM jobs ORDER BY id DESC LIMIT 10",
        &[],
    )?;
    Ok(Json(json!({
        "counts": {"projects": count("projects"), "inventories": count("inventories"), "credentials": count("credentials"), "job_templates": count("job_templates"), "jobs": count("jobs"), "users": count("users"), "schedules": count("schedules")},
        "jobs_by_status": by_status,
        "recent_jobs": recent,
    })))
}

async fn password_change(
    State(st): State<AppState>,
    u: AuthUser,
    Json(body): Json<Map<String, Value>>,
) -> R<StatusCode> {
    let old = body
        .get("old_password")
        .and_then(|p| p.as_str())
        .unwrap_or("");
    let new = body
        .get("new_password")
        .and_then(|p| p.as_str())
        .unwrap_or("");
    if new.len() < 8 {
        return Err(bad("password must be at least 8 characters"));
    }
    let c = st.db.lock().unwrap();
    let row = db::get(&c, "users", u.id)?.ok_or_else(not_found)?;
    if !db::verify_password(old, row["pw_hash"].as_str().unwrap_or("")) {
        return Err(bad("old password is incorrect"));
    }
    c.execute(
        "UPDATE users SET pw_hash = ?1 WHERE id = ?2",
        rusqlite::params![db::hash_password(new), u.id],
    )?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- static UI ----------

async fn static_files(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let (file, name) = match Assets::get(path) {
        Some(f) if !path.is_empty() => (f, path.to_string()),
        _ => match Assets::get("index.html") {
            Some(f) => (f, "index.html".to_string()),
            None => return (StatusCode::NOT_FOUND, "UI assets missing").into_response(),
        },
    };
    let mime = match name.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "json" => "application/json",
        _ => "application/octet-stream",
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::CACHE_CONTROL, "no-cache"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'self'; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'",
            ),
            (header::REFERRER_POLICY, "same-origin"),
        ],
        file.data.into_owned(),
    )
        .into_response()
}

// ---------- assembly ----------

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/v2/ping/", get(ping))
        .route("/api/v2/auth/login/", post(login))
        .route("/api/v2/auth/logout/", post(logout))
        .route("/api/v2/me/", get(me))
        .route("/api/v2/me/password/", post(password_change))
        .route("/api/v2/dashboard/", get(dashboard))
        .route("/api/v2/projects/{id}/sync/", post(sync_project))
        .route("/api/v2/projects/{id}/playbooks/", get(project_playbooks))
        .route("/api/v2/inventories/{id}/hosts/", get(inventory_hosts))
        .route("/api/v2/job_templates/{id}/launch/", post(launch))
        .route("/api/v2/jobs/{id}/relaunch/", post(relaunch))
        .route("/api/v2/jobs/{id}/cancel/", post(cancel_job))
        .route("/api/v2/jobs/{id}/stdout/", get(job_stdout))
        .route("/api/v2/jobs/{id}/job_events/", get(job_events))
        .route("/api/v2/jobs/{id}/stream/", get(job_stream))
        .route("/api/v2/{res}/", get(list_res).post(create_res))
        .route(
            "/api/v2/{res}/{id}/",
            get(get_res)
                .put(update_res)
                .patch(update_res)
                .delete(delete_res),
        )
        .fallback(static_files)
        .with_state(state)
}

fn load_master(dir: &std::path::Path) -> Result<String> {
    if let Ok(k) = std::env::var("RANSIBLE_SECRET_KEY")
        && !k.is_empty()
    {
        return Ok(k);
    }
    let p = dir.join("secret.key");
    if let Ok(k) = std::fs::read_to_string(&p) {
        return Ok(k.trim().to_string());
    }
    let k = db::new_token();
    std::fs::write(&p, &k)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(k)
}

pub fn build_state(
    data_dir: &std::path::Path,
    max_jobs: usize,
    conn: Connection,
) -> Result<AppState> {
    std::fs::create_dir_all(data_dir)?;
    Ok(AppState {
        db: Arc::new(Mutex::new(conn)),
        master: Arc::new(load_master(data_dir)?),
        data_dir: Arc::new(data_dir.to_path_buf()),
        live: Default::default(),
        sem: Arc::new(tokio::sync::Semaphore::new(max_jobs.max(1))),
        login_failures: Default::default(),
    })
}

fn bootstrap_admin(st: &AppState, password: Option<String>) -> Result<()> {
    let c = st.db.lock().unwrap();
    let n: i64 = c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    if n > 0 {
        return Ok(());
    }
    let (pw, generated) = match password {
        Some(p) => (p, false),
        None => (db::new_token()[..20].to_string(), true),
    };
    c.execute(
        "INSERT INTO users (username, pw_hash, role) VALUES ('admin', ?1, 'admin')",
        [db::hash_password(&pw)],
    )?;
    if generated {
        eprintln!(
            "ransible: created user 'admin' with generated password: {pw}\n          (change it after first login; it will not be shown again)"
        );
    } else {
        eprintln!("ransible: created user 'admin'");
    }
    Ok(())
}

/// Fire schedules whose cron expression matches the current minute (UTC).
async fn scheduler(st: AppState) {
    let mut last_minute = -1i64;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let minute = now / 60;
        if minute == last_minute {
            continue;
        }
        last_minute = minute;
        let due: Vec<Map<String, Value>> = {
            let c = st.db.lock().unwrap();
            db::query(&c, "SELECT * FROM schedules WHERE enabled = 1", &[]).unwrap_or_default()
        };
        for s in due {
            if !cron::matches(s["cron"].as_str().unwrap_or(""), minute * 60) {
                continue;
            }
            let template = {
                let c = st.db.lock().unwrap();
                s["template_id"]
                    .as_i64()
                    .and_then(|id| db::get(&c, "job_templates", id).ok().flatten())
            };
            let Some(template) = template else { continue };
            let mut overrides = Map::new();
            if let Some(text) = s["extra_vars"].as_str().filter(|t| !t.trim().is_empty()) {
                let mut merged = match crate::playbook::parse_yaml_str(
                    template["extra_vars"].as_str().unwrap_or(""),
                ) {
                    Ok(Value::Object(o)) => o,
                    _ => Map::new(),
                };
                if let Ok(Value::Object(o)) = crate::playbook::parse_yaml_str(text) {
                    merged.extend(o);
                }
                overrides.insert("extra_vars".into(), Value::Object(merged));
            }
            let by = format!("schedule:{}", s["name"].as_str().unwrap_or(""));
            match create_job(&st, &template, &overrides, &by) {
                Ok(job_id) => {
                    st.activity(&by, "launch", "job", job_id, "launched by schedule");
                    if let Ok(c) = st.db.lock() {
                        let _ = c.execute(
                            "UPDATE schedules SET last_run = datetime('now') WHERE id = ?1",
                            [s["id"].as_i64()],
                        );
                    }
                }
                Err(e) => eprintln!("ransible: schedule {} failed to launch: {e:#}", by),
            }
        }
    }
}

pub async fn serve(opts: Options) -> Result<()> {
    std::fs::create_dir_all(&opts.data_dir)?;
    let conn = db::open(&opts.data_dir.join("ransible.db"))?;
    let st = build_state(&opts.data_dir, opts.max_jobs, conn)?;
    bootstrap_admin(&st, opts.admin_password)?;
    // jobs left running by a previous process cannot be resumed
    {
        let c = st.db.lock().unwrap();
        c.execute("UPDATE jobs SET status = 'error', finished = datetime('now') WHERE status IN ('pending','running')", [])?;
    }
    tokio::spawn(scheduler(st.clone()));
    let app = router(st);
    let listener = tokio::net::TcpListener::bind(&opts.listen)
        .await
        .with_context(|| format!("binding {}", opts.listen))?;
    eprintln!("ransible: listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
