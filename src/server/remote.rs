//! Remote execution: workers (`ransible worker`) claim jobs over the API, fetch a bundle with the
//! project and credentials, run the playbook locally and stream events back.
use super::jobs::{self, Live};
use super::{ApiError, AppState, R, bad, db, forbidden, not_found};
use crate::events::Event;
use axum::{
    Json,
    extract::{FromRequestParts, Path, State},
    http::{StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
};
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value, json};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// A worker lost for this long (no events/heartbeat) while running a job fails the job.
const WORKER_LOST_SECS: u64 = 120;

// ---------- worker authentication ----------

pub struct WorkerAuth {
    pub name: String,
}

impl FromRequestParts<AppState> for WorkerAuth {
    type Rejection = ApiError;
    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let unauth = || ApiError(StatusCode::UNAUTHORIZED, "Invalid worker token.".into());
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(|t| t.trim().to_string())
            .ok_or_else(unauth)?;
        let c = state.db.lock().unwrap();
        let rows = db::query(
            &c,
            "SELECT id, name FROM workers WHERE token_hash = ?1",
            &[SqlValue::Text(db::token_hash(&token))],
        )?;
        let w = rows.into_iter().next().ok_or_else(unauth)?;
        let _ = c.execute(
            "UPDATE workers SET last_seen = datetime('now') WHERE id = ?1",
            [w["id"].as_i64()],
        );
        Ok(WorkerAuth {
            name: w["name"].as_str().unwrap_or("").to_string(),
        })
    }
}

// ---------- worker management (admin) ----------

pub async fn create_worker(
    State(st): State<AppState>,
    u: super::AuthUser,
    Json(body): Json<Map<String, Value>>,
) -> R<(StatusCode, Json<Value>)> {
    if !u.can("write") {
        return Err(forbidden());
    }
    let name = body
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || "-_.".contains(c))
    {
        return Err(bad("name is required (letters, digits, - _ .)"));
    }
    let token = db::new_token();
    let id = {
        let c = st.db.lock().unwrap();
        c.execute(
            "INSERT INTO workers (name, token_hash) VALUES (?1, ?2)",
            rusqlite::params![name, db::token_hash(&token)],
        )
        .map_err(|e| ApiError::from(anyhow::Error::from(e)))?;
        c.last_insert_rowid()
    };
    st.activity(
        &u.username,
        "create",
        "worker",
        id,
        &format!("created worker {name}"),
    );
    Ok((
        StatusCode::CREATED,
        Json(
            json!({"id": id, "name": name, "token": token, "url": format!("/api/v2/workers/{id}/")}),
        ),
    ))
}

pub fn worker_online(last_seen: Option<&str>) -> bool {
    let Some(ls) = last_seen else { return false };
    // SQLite datetime('now') is UTC "YYYY-MM-DD HH:MM:SS"
    let p: Vec<i64> = ls
        .split(|c: char| !c.is_ascii_digit())
        .filter(|x| !x.is_empty())
        .filter_map(|x| x.parse().ok())
        .collect();
    if p.len() < 6 {
        return false;
    }
    let (y, m, d) = (p[0], p[1], p[2]);
    // days from civil
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let then = days * 86400 + p[3] * 3600 + p[4] * 60 + p[5];
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    now - then < 60
}

// ---------- server-side job lifecycle for remote jobs ----------

/// Waits for a worker to run the job; keeps the live channel registered for SSE/cancel.
pub async fn run_remote_job(st: AppState, job_id: i64) {
    let (live, _tx) = Live::new();
    let cancel = live.cancel.clone();
    st.live.lock().unwrap().insert(job_id, live);
    loop {
        let status = {
            let c = st.db.lock().unwrap();
            db::get(&c, "jobs", job_id)
                .ok()
                .flatten()
                .and_then(|j| j["status"].as_str().map(String::from))
                .unwrap_or_default()
        };
        if !["pending", "running"].contains(&status.as_str()) {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = cancel.notified() => {
                // cancel before any worker claimed the job
                let claimed_none = {
                    let c = st.db.lock().unwrap();
                    c.execute("UPDATE jobs SET status = 'canceled', finished = datetime('now') WHERE id = ?1 AND status = 'pending'", [job_id]).unwrap_or(0) > 0
                };
                if claimed_none {
                    jobs::ingest_event(&st, job_id, &Event::Message { text: "Job canceled before a worker picked it up.".into() });
                    break;
                }
            }
        }
        if status == "running" {
            let idle = st
                .live
                .lock()
                .unwrap()
                .get(&job_id)
                .map(|l| l.last_activity.lock().unwrap().elapsed().as_secs())
                .unwrap_or(0);
            if idle > WORKER_LOST_SECS {
                jobs::ingest_event(
                    &st,
                    job_id,
                    &Event::Message {
                        text: format!(
                            "ERROR! lost contact with the worker ({idle}s without events)"
                        ),
                    },
                );
                jobs::finish_job(&st, job_id, "error", None);
                break;
            }
        }
    }
    st.live.lock().unwrap().remove(&job_id);
    let _ = std::fs::remove_dir_all(st.data_dir.join("jobs").join(job_id.to_string()));
}

// ---------- worker API ----------

fn take_pending(st: &AppState, worker: &str) -> Option<i64> {
    let c = st.db.lock().unwrap();
    let id = db::query(
        &c,
        "SELECT id FROM jobs WHERE worker = ?1 AND status = 'pending' ORDER BY id LIMIT 1",
        &[SqlValue::Text(worker.to_string())],
    )
    .ok()?
    .into_iter()
    .next()?["id"]
        .as_i64()?;
    let n = c.execute("UPDATE jobs SET status = 'running', started = datetime('now') WHERE id = ?1 AND status = 'pending'", [id]).ok()?;
    (n > 0).then_some(id)
}

pub async fn claim(State(st): State<AppState>, w: WorkerAuth) -> R {
    for _ in 0..40 {
        if let Some(id) = take_pending(&st, &w.name) {
            if let Some(l) = st.live.lock().unwrap().get(&id) {
                *l.last_activity.lock().unwrap() = Instant::now();
            }
            let params: Map<String, Value> = {
                let c = st.db.lock().unwrap();
                db::get(&c, "jobs", id)
                    .ok()
                    .flatten()
                    .and_then(|j| serde_json::from_str(j["params"].as_str().unwrap_or("{}")).ok())
                    .unwrap_or_default()
            };
            jobs::ingest_event(
                &st,
                id,
                &Event::Message {
                    text: format!("Job picked up by worker {}", w.name),
                },
            );
            match jobs::prepare(&st, id, &params) {
                Ok(p) => {
                    let (dir, cwd) = (p.dir.display().to_string(), p.cwd.display().to_string());
                    let args: Vec<String> = p
                        .args
                        .iter()
                        .map(|a| a.replace(&dir, "@JOB@").replace(&cwd, "@PROJ@"))
                        .collect();
                    tokio::spawn(jobs::notify(st.clone(), id, "start".into()));
                    return Ok(Json(
                        json!({"job": {"id": id, "args": args, "bundle": format!("/api/v2/worker/jobs/{id}/bundle/")}}),
                    ));
                }
                Err(e) => {
                    jobs::ingest_event(
                        &st,
                        id,
                        &Event::Message {
                            text: format!("ERROR! {e:#}"),
                        },
                    );
                    jobs::finish_job(&st, id, "error", None);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(Json(json!({"job": null})))
}

fn assigned(st: &AppState, w: &WorkerAuth, id: i64) -> Result<(), ApiError> {
    let c = st.db.lock().unwrap();
    let job = db::get(&c, "jobs", id)?.ok_or_else(not_found)?;
    if job["worker"].as_str() != Some(w.name.as_str()) {
        return Err(forbidden());
    }
    Ok(())
}

/// tar.gz with `job/` (inventory, credentials, extra vars) and `project/`.
#[cfg(unix)]
fn build_bundle(
    id: i64,
    job_dir: &std::path::Path,
    project_dir: &std::path::Path,
) -> anyhow::Result<Vec<u8>> {
    let stage = job_dir.with_file_name(format!("{id}-stage"));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    std::os::unix::fs::symlink(job_dir, stage.join("job"))?;
    std::os::unix::fs::symlink(project_dir, stage.join("project"))?;
    let out = std::process::Command::new("tar")
        .args(["-czhf", "-", "-C"])
        .arg(&stage)
        .args(["job", "project"])
        .output()?;
    let _ = std::fs::remove_dir_all(&stage);
    if !out.status.success() {
        anyhow::bail!("tar failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(out.stdout)
}

#[cfg(not(unix))]
fn build_bundle(
    _id: i64,
    _job_dir: &std::path::Path,
    _project_dir: &std::path::Path,
) -> anyhow::Result<Vec<u8>> {
    anyhow::bail!(
        "remote workers need a Unix server (the job bundle is built with symlinks and tar)"
    )
}

pub async fn bundle(State(st): State<AppState>, w: WorkerAuth, Path(id): Path<i64>) -> R<Response> {
    assigned(&st, &w, id)?;
    let params: Map<String, Value> = {
        let c = st.db.lock().unwrap();
        db::get(&c, "jobs", id)?
            .and_then(|j| serde_json::from_str(j["params"].as_str().unwrap_or("{}")).ok())
            .unwrap_or_default()
    };
    let (job_dir, project_dir) = {
        let c = st.db.lock().unwrap();
        let project = params
            .get("project_id")
            .and_then(|p| p.as_i64())
            .and_then(|p| db::get(&c, "projects", p).ok().flatten())
            .ok_or_else(|| bad("project missing"))?;
        (
            st.data_dir.join("jobs").join(id.to_string()),
            jobs::project_dir(&st, &project),
        )
    };
    let bytes = tokio::task::spawn_blocking(move || build_bundle(id, &job_dir, &project_dir))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))??;
    Ok(([(header::CONTENT_TYPE, "application/gzip")], bytes).into_response())
}

pub async fn events(
    State(st): State<AppState>,
    w: WorkerAuth,
    Path(id): Path<i64>,
    body: axum::body::Bytes,
) -> R {
    assigned(&st, &w, id)?;
    for line in String::from_utf8_lossy(&body)
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        let ev = serde_json::from_str::<Event>(line).unwrap_or(Event::Message {
            text: line.to_string(),
        });
        jobs::ingest_event(&st, id, &ev);
    }
    let (cancel, ok) = match st.live.lock().unwrap().get(&id) {
        Some(l) => {
            *l.last_activity.lock().unwrap() = Instant::now();
            (l.canceled.load(Ordering::Relaxed), true)
        }
        None => (false, false),
    };
    Ok(Json(json!({"cancel": cancel, "ok": ok})))
}

pub async fn finish(
    State(st): State<AppState>,
    w: WorkerAuth,
    Path(id): Path<i64>,
    Json(body): Json<Map<String, Value>>,
) -> R {
    assigned(&st, &w, id)?;
    let rc = body.get("rc").and_then(|r| r.as_i64());
    let canceled = body
        .get("canceled")
        .and_then(|c| c.as_bool())
        .unwrap_or(false);
    if let Some(e) = body.get("error").and_then(|e| e.as_str()) {
        jobs::ingest_event(
            &st,
            id,
            &Event::Message {
                text: format!("ERROR! {e}"),
            },
        );
    }
    let status = if canceled {
        "canceled"
    } else {
        match rc {
            Some(0) => "successful",
            Some(_) => "failed",
            None => "error",
        }
    };
    jobs::finish_job(&st, id, status, rc);
    Ok(Json(json!({"status": status})))
}
