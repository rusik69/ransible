//! Job execution: runs `ransible playbook ... -o json` as a subprocess and streams its events.
use super::AppState;
use super::db;
use crate::events::{ConsoleSink, Event};
use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Notify, broadcast};

/// Message sent to live subscribers: (seq, rendered text).
pub type LiveMsg = (i64, String);

pub struct Live {
    pub tx: broadcast::Sender<LiveMsg>,
    pub cancel: std::sync::Arc<Notify>,
}

pub fn render(e: &Event) -> String {
    ConsoleSink {
        color: true,
        verbose: 0,
    }
    .render(e)
}

fn s(v: &Map<String, Value>, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn i(v: &Map<String, Value>, k: &str) -> Option<i64> {
    v.get(k).and_then(|x| x.as_i64())
}

#[cfg(unix)]
fn private(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn private(_: &Path) {}

pub fn project_dir(state: &AppState, project: &Map<String, Value>) -> PathBuf {
    let local = s(project, "local_path");
    if !local.is_empty() {
        PathBuf::from(local)
    } else {
        state
            .data_dir
            .join("projects")
            .join(project["id"].as_i64().unwrap_or(0).to_string())
    }
}

/// Snapshot of everything needed to run a job (stored in jobs.params).
pub fn resolve_launch(template: &Map<String, Value>, overrides: &Map<String, Value>) -> Value {
    let pick = |k: &str| {
        overrides
            .get(k)
            .cloned()
            .unwrap_or_else(|| template.get(k).cloned().unwrap_or(Value::Null))
    };
    json!({
        "project_id": template.get("project_id"),
        "playbook": template.get("playbook"),
        "inventory_id": template.get("inventory_id"),
        "credential_id": template.get("credential_id"),
        "vault_credential_id": template.get("vault_credential_id"),
        "extra_vars": pick("extra_vars"),
        "limit": pick("limit_hosts").as_str().map(String::from).or_else(|| overrides.get("limit").and_then(|x| x.as_str()).map(String::from)),
        "tags": pick("tags"),
        "skip_tags": pick("skip_tags"),
        "verbosity": pick("verbosity"),
        "forks": pick("forks"),
        "job_type": pick("job_type"),
        "become": pick("become_enabled"),
        "diff": pick("diff_mode"),
    })
}

fn persist_event(state: &AppState, job_id: i64, seq: i64, ev: &Event) {
    if let Ok(c) = state.db.lock() {
        let _ = c.execute(
            "INSERT INTO job_events (job_id, seq, event) VALUES (?1, ?2, ?3)",
            rusqlite::params![job_id, seq, serde_json::to_string(ev).unwrap_or_default()],
        );
    }
}

fn set_status(state: &AppState, id: i64, sql_extra: &str, status: &str) {
    if let Ok(c) = state.db.lock() {
        let _ = c.execute(
            &format!("UPDATE jobs SET status = ?1{sql_extra} WHERE id = ?2"),
            rusqlite::params![status, id],
        );
    }
}

struct Prepared {
    args: Vec<String>,
    dir: PathBuf,
    cwd: PathBuf,
}

fn prepare(state: &AppState, job_id: i64, params: &Map<String, Value>) -> Result<Prepared> {
    let dir = state.data_dir.join("jobs").join(job_id.to_string());
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let (project, inventory, cred, vault_cred) = {
        let c = state.db.lock().unwrap();
        (
            i(params, "project_id").and_then(|id| db::get(&c, "projects", id).ok().flatten()),
            i(params, "inventory_id").and_then(|id| db::get(&c, "inventories", id).ok().flatten()),
            i(params, "credential_id").and_then(|id| db::get(&c, "credentials", id).ok().flatten()),
            i(params, "vault_credential_id")
                .and_then(|id| db::get(&c, "credentials", id).ok().flatten()),
        )
    };
    let project = project.ok_or_else(|| anyhow!("job template has no project"))?;
    let pdir = project_dir(state, &project);
    let playbook = s(params, "playbook");
    if playbook.is_empty() || playbook.contains("..") || playbook.starts_with('/') {
        return Err(anyhow!("invalid playbook path '{playbook}'"));
    }
    let pb_path = pdir.join(&playbook);
    if !pb_path.is_file() {
        return Err(anyhow!(
            "playbook {} not found in project (sync the project?)",
            pb_path.display()
        ));
    }
    let mut args: Vec<String> = vec![
        "playbook".into(),
        pb_path.display().to_string(),
        "-o".into(),
        "json".into(),
    ];
    // inventory
    if let Some(inv) = inventory {
        let content = s(&inv, "content");
        let path = if s(&inv, "kind") == "script" {
            let p = dir.join("inventory.script");
            std::fs::write(&p, &content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700))?;
            }
            p
        } else {
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
            let p = dir.join(format!("inventory.{ext}"));
            std::fs::write(&p, &content)?;
            p
        };
        args.extend(["-i".into(), path.display().to_string()]);
    }
    // extra vars (+ secrets) go through a 0600 file
    let mut extra = Map::new();
    match params.get("extra_vars") {
        Some(Value::Object(o)) => extra.extend(o.clone()),
        Some(Value::String(t)) if !t.trim().is_empty() => {
            if let Value::Object(o) = crate::playbook::parse_yaml_str(t)? {
                extra.extend(o);
            }
        }
        _ => {}
    }
    if let Some(cr) = &cred {
        let sec: Map<String, Value> =
            serde_json::from_str(&db::decrypt_secret(&state.master, &s(cr, "secrets"))?)
                .unwrap_or_default();
        let user = s(cr, "username");
        if !user.is_empty() {
            args.extend(["-u".into(), user]);
        }
        if let Some(k) = sec
            .get("ssh_private_key")
            .and_then(|k| k.as_str())
            .filter(|k| !k.is_empty())
        {
            let p = dir.join("id_key");
            let mut key = k.to_string();
            if !key.ends_with('\n') {
                key.push('\n');
            }
            std::fs::write(&p, key)?;
            private(&p);
            args.extend(["--private-key".into(), p.display().to_string()]);
        }
        if let Some(pw) = sec
            .get("password")
            .and_then(|k| k.as_str())
            .filter(|k| !k.is_empty())
        {
            extra.insert("ansible_password".into(), pw.into());
        }
        if let Some(pw) = sec
            .get("become_password")
            .and_then(|k| k.as_str())
            .filter(|k| !k.is_empty())
        {
            extra.insert("ansible_become_password".into(), pw.into());
        }
    }
    if let Some(vc) = &vault_cred {
        let sec: Map<String, Value> =
            serde_json::from_str(&db::decrypt_secret(&state.master, &s(vc, "secrets"))?)
                .unwrap_or_default();
        if let Some(pw) = sec.get("vault_password").and_then(|k| k.as_str()) {
            let p = dir.join("vault_pass");
            std::fs::write(&p, format!("{pw}\n"))?;
            private(&p);
            args.extend(["--vault-password-file".into(), p.display().to_string()]);
        }
    }
    if !extra.is_empty() {
        let p = dir.join("extra_vars.json");
        std::fs::write(&p, serde_json::to_string(&Value::Object(extra))?)?;
        private(&p);
        args.extend(["-e".into(), format!("@{}", p.display())]);
    }
    if let Some(l) = params
        .get("limit")
        .and_then(|l| l.as_str())
        .filter(|l| !l.is_empty())
    {
        args.extend(["-l".into(), l.into()]);
    }
    for (key, flag) in [("tags", "-t"), ("skip_tags", "--skip-tags")] {
        if let Some(t) = params
            .get(key)
            .and_then(|l| l.as_str())
            .filter(|l| !l.is_empty())
        {
            args.extend([flag.into(), t.into()]);
        }
    }
    if let Some(f) = i(params, "forks").filter(|f| *f > 0) {
        args.extend(["-f".into(), f.to_string()]);
    }
    if s(params, "job_type") == "check" {
        args.push("--check".into());
    }
    if params.get("diff").and_then(|d| d.as_i64()).unwrap_or(0) == 1 {
        args.push("--diff".into());
    }
    if params.get("become").and_then(|d| d.as_i64()).unwrap_or(0) == 1 {
        args.push("--become".into());
    }
    for _ in 0..i(params, "verbosity").unwrap_or(0).clamp(0, 5) {
        args.push("-v".into());
    }
    Ok(Prepared {
        args,
        dir,
        cwd: pdir,
    })
}

/// Send webhook/Slack notifications for a job event (`start`, or a final status).
pub async fn notify(state: AppState, job_id: i64, event: String) {
    let (job, hooks) = {
        let Ok(c) = state.db.lock() else { return };
        let job = db::get(&c, "jobs", job_id).ok().flatten();
        let col = match event.as_str() {
            "start" => "on_start",
            "successful" => "on_success",
            "failed" | "error" => "on_failure",
            _ => return,
        };
        let hooks = db::query(
            &c,
            &format!("SELECT * FROM notification_templates WHERE enabled = 1 AND {col} = 1"),
            &[],
        )
        .unwrap_or_default();
        (job, hooks)
    };
    let Some(job) = job else { return };
    let base = std::env::var("RANSIBLE_EXTERNAL_URL").unwrap_or_default();
    let link = format!("{}/#/jobs/{job_id}", base.trim_end_matches('/'));
    let label = if event == "start" {
        "started".to_string()
    } else {
        event.clone()
    };
    for h in hooks {
        let Ok(url) = db::decrypt_secret(&state.master, h["webhook_url"].as_str().unwrap_or(""))
        else {
            continue;
        };
        if url.is_empty() {
            continue;
        }
        let payload = if h["kind"].as_str() == Some("slack") {
            json!({"text": format!("Job #{job_id} {} {label} {link}", job["name"].as_str().unwrap_or(""))})
        } else {
            json!({"id": job_id, "name": job["name"], "status": label, "playbook": job["playbook"], "launched_by": job["launched_by"], "url": link})
        };
        let mut c = tokio::process::Command::new("curl");
        c.args([
            "-sS",
            "-m",
            "10",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/json",
            "-d",
            &payload.to_string(),
            "-K",
            "-",
        ]);
        c.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut child) = c.spawn() {
            if let Some(mut si) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                let _ = si
                    .write_all(
                        format!(
                            "url = \"{}\"\n",
                            url.replace('\\', "\\\\").replace('"', "\\\"")
                        )
                        .as_bytes(),
                    )
                    .await;
            }
            let _ = child.wait().await;
        }
    }
}

pub async fn run_job(state: AppState, job_id: i64) {
    let live = {
        let (tx, _) = broadcast::channel(1024);
        let l = Live {
            tx: tx.clone(),
            cancel: std::sync::Arc::new(Notify::new()),
        };
        let cancel = l.cancel.clone();
        state.live.lock().unwrap().insert(job_id, l);
        (tx, cancel)
    };
    let (tx, cancel) = live;
    let finish = |status: &str, rc: Option<i64>| {
        let c = state.db.lock().unwrap();
        let summary = db::query(
            &c,
            "SELECT event FROM job_events WHERE job_id = ?1 ORDER BY seq DESC",
            &[rusqlite::types::Value::Integer(job_id)],
        )
        .ok()
        .and_then(|rows| {
            rows.into_iter().find_map(|r| {
                match serde_json::from_str::<Event>(r["event"].as_str().unwrap_or("")) {
                    Ok(Event::Recap { stats }) => {
                        Some(serde_json::to_string(&stats).unwrap_or_default())
                    }
                    _ => None,
                }
            })
        })
        .unwrap_or_else(|| "[]".into());
        let _ = c.execute(
            "UPDATE jobs SET status = ?1, finished = datetime('now'), rc = ?2, summary = ?3 WHERE id = ?4",
            rusqlite::params![status, rc, summary, job_id],
        );
        drop(c);
        tokio::spawn(notify(state.clone(), job_id, status.to_string()));
    };
    let mut seq: i64 = 0;
    let mut emit = |ev: Event, tx: &broadcast::Sender<LiveMsg>| {
        seq += 1;
        persist_event(&state, job_id, seq, &ev);
        let _ = tx.send((seq, render(&ev)));
    };

    // wait for a slot
    let permit = tokio::select! {
        p = state.sem.clone().acquire_owned() => p.ok(),
        _ = cancel.notified() => None,
    };
    let Some(_permit) = permit else {
        emit(
            Event::Message {
                text: "Job canceled before start.".into(),
            },
            &tx,
        );
        finish("canceled", None);
        state.live.lock().unwrap().remove(&job_id);
        return;
    };
    set_status(&state, job_id, ", started = datetime('now')", "running");
    tokio::spawn(notify(state.clone(), job_id, "start".into()));

    let params: Map<String, Value> = {
        let c = state.db.lock().unwrap();
        db::get(&c, "jobs", job_id)
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str(j["params"].as_str().unwrap_or("{}")).ok())
            .unwrap_or_default()
    };
    let prepared = match prepare(&state, job_id, &params) {
        Ok(p) => p,
        Err(e) => {
            emit(
                Event::Message {
                    text: format!("ERROR! {e:#}"),
                },
                &tx,
            );
            finish("error", None);
            state.live.lock().unwrap().remove(&job_id);
            return;
        }
    };
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            emit(
                Event::Message {
                    text: format!("ERROR! cannot locate executable: {e}"),
                },
                &tx,
            );
            finish("error", None);
            state.live.lock().unwrap().remove(&job_id);
            return;
        }
    };
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(&prepared.args)
        .current_dir(&prepared.cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    cmd.env_clear();
    for k in ["PATH", "HOME", "LANG", "SSH_AUTH_SOCK", "TMPDIR"] {
        if let Ok(v) = std::env::var(k) {
            cmd.env(k, v);
        }
    }
    cmd.env("NO_COLOR", "1");
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            emit(
                Event::Message {
                    text: format!("ERROR! failed to start runner: {e}"),
                },
                &tx,
            );
            finish("error", None);
            state.live.lock().unwrap().remove(&job_id);
            let _ = std::fs::remove_dir_all(&prepared.dir);
            return;
        }
    };
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<(bool, String)>();
    let l2 = line_tx.clone();
    tokio::spawn(async move {
        let mut r = BufReader::new(stdout).lines();
        while let Ok(Some(l)) = r.next_line().await {
            let _ = l2.send((true, l));
        }
    });
    tokio::spawn(async move {
        let mut r = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = r.next_line().await {
            let _ = line_tx.send((false, l));
        }
    });
    let mut canceled = false;
    let mut status_code: Option<i64> = None;
    let mut exited = false;
    loop {
        tokio::select! {
            line = line_rx.recv() => {
                match line {
                    Some((is_out, l)) => {
                        let ev = if is_out { serde_json::from_str::<Event>(&l).unwrap_or(Event::Message { text: l }) } else { Event::Message { text: l } };
                        if let Event::PlaybookEnd { rc } = &ev { status_code = Some(*rc as i64); }
                        emit(ev, &tx);
                    }
                    None => break,
                }
            }
            _ = cancel.notified(), if !canceled => {
                canceled = true;
                let _ = child.start_kill();
                emit(Event::Message { text: "\nJob canceled by user.".into() }, &tx);
            }
            st = child.wait(), if !exited => {
                exited = true;
                if status_code.is_none() { status_code = st.ok().and_then(|s| s.code()).map(|c| c as i64); }
            }
        }
    }
    if !exited && let Ok(st) = child.wait().await {
        status_code = status_code.or(st.code().map(|c| c as i64));
    }
    let status = if canceled {
        "canceled"
    } else {
        match status_code {
            Some(0) => "successful",
            Some(_) => "failed",
            None => "error",
        }
    };
    finish(status, status_code);
    let _ = std::fs::remove_dir_all(&prepared.dir);
    state.live.lock().unwrap().remove(&job_id);
}
