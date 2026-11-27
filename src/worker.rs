//! `ransible worker`: pulls jobs from a ransible server, runs them locally, streams events back.
//!
//! Uses `curl` for HTTP so the worker needs no extra dependencies; the token is passed through a
//! curl config on stdin (never on the command line).
use crate::events::Event;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub struct Options {
    pub server: String,
    pub token: String,
    pub cacert: Option<String>,
    pub insecure_skip_verify: bool,
    pub allow_insecure_http: bool,
    /// exit after this many jobs (tests); None = run forever
    pub max_jobs: Option<u32>,
}

struct Client {
    base: String,
    token: String,
    cacert: Option<String>,
    insecure: bool,
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1") || host.starts_with("127.")
}

impl Client {
    /// Returns (status, body).
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        timeout: u64,
        save_to: Option<&Path>,
    ) -> Result<(u16, Vec<u8>)> {
        let mut c = Command::new("curl");
        c.args([
            "-sS",
            "-m",
            &timeout.to_string(),
            "-X",
            method,
            "-K",
            "-",
            "-w",
            "\n%{http_code}",
        ]);
        if let Some(ca) = &self.cacert {
            c.args(["--cacert", ca]);
        }
        if self.insecure {
            c.arg("-k");
        }
        let mut tmp: Option<PathBuf> = None;
        if let Some(b) = body {
            let p = std::env::temp_dir().join(format!(
                "ransible-worker-body-{}-{}",
                std::process::id(),
                Instant::now().elapsed().as_nanos()
            ));
            std::fs::write(&p, b)?;
            c.args(["-H", "Content-Type: application/json", "--data-binary"])
                .arg(format!("@{}", p.display()));
            tmp = Some(p);
        }
        if let Some(f) = save_to {
            c.arg("-o").arg(f);
        }
        c.arg(format!("{}{}", self.base, path));
        c.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().context("running curl")?;
        child.stdin.take().unwrap().write_all(
            format!(
                "header = \"Authorization: Bearer {}\"\n",
                self.token.replace('"', "")
            )
            .as_bytes(),
        )?;
        let out = child.wait_with_output()?;
        if let Some(t) = tmp {
            let _ = std::fs::remove_file(t);
        }
        if !out.status.success() {
            bail!("curl: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        // body ends with "\n<code>" (when saving to a file the body is just the code line)
        let text = out.stdout;
        let split = text
            .iter()
            .rposition(|b| *b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let code: u16 = String::from_utf8_lossy(&text[split..])
            .trim()
            .parse()
            .unwrap_or(0);
        Ok((code, text[..split.saturating_sub(1)].to_vec()))
    }

    fn json(&self, method: &str, path: &str, body: Option<&Value>, timeout: u64) -> Result<Value> {
        let b = body.map(|v| v.to_string());
        let (code, out) = self.request(
            method,
            path,
            b.as_deref().map(|s| s.as_bytes()),
            timeout,
            None,
        )?;
        if code == 401 {
            bail!("the server rejected the worker token (401)");
        }
        if !(200..300).contains(&code) {
            bail!(
                "server returned {code}: {}",
                String::from_utf8_lossy(&out).trim()
            );
        }
        Ok(serde_json::from_slice(&out).unwrap_or(Value::Null))
    }
}

pub fn run(o: Options) -> Result<()> {
    let base = o.server.trim_end_matches('/').to_string();
    let host = base
        .split("://")
        .nth(1)
        .unwrap_or("")
        .split(['/', ':'])
        .next()
        .unwrap_or("")
        .to_string();
    if base.starts_with("http://") && !is_loopback(&host) && !o.allow_insecure_http {
        bail!(
            "refusing to send the worker token and credentials over plain HTTP to {host}; use https:// (or --allow-insecure-http)"
        );
    }
    if !base.starts_with("http://") && !base.starts_with("https://") {
        bail!("--server must start with http:// or https://");
    }
    let client = Client {
        base,
        token: o.token,
        cacert: o.cacert,
        insecure: o.insecure_skip_verify,
    };
    eprintln!("ransible worker: connected to {}", client.base);
    let mut done = 0u32;
    loop {
        match client.json("POST", "/api/v2/worker/claim/", None, 40) {
            Ok(v) => {
                if let Some(job) = v.get("job").filter(|j| !j.is_null()) {
                    if let Err(e) = run_job(&client, job) {
                        eprintln!("ransible worker: job failed: {e:#}");
                    }
                    done += 1;
                    if o.max_jobs.map(|m| done >= m).unwrap_or(false) {
                        return Ok(());
                    }
                }
            }
            Err(e) => {
                if e.to_string().contains("(401)") {
                    return Err(e);
                }
                eprintln!("ransible worker: {e:#}; retrying in 3s");
                std::thread::sleep(Duration::from_secs(3));
            }
        }
    }
}

fn post_events(client: &Client, id: i64, lines: &[String]) -> bool {
    let body = lines.join("\n");
    match client.request(
        "POST",
        &format!("/api/v2/worker/jobs/{id}/events/"),
        Some(body.as_bytes()),
        20,
        None,
    ) {
        Ok((200, out)) => serde_json::from_slice::<Value>(&out)
            .ok()
            .and_then(|v| v.get("cancel").and_then(|c| c.as_bool()))
            .unwrap_or(false),
        _ => false,
    }
}

fn run_job(client: &Client, job: &Value) -> Result<()> {
    let id = job["id"]
        .as_i64()
        .ok_or_else(|| anyhow!("job without id"))?;
    let work = std::env::temp_dir()
        .join(format!("ransible-worker-{}", std::process::id()))
        .join(id.to_string());
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o700))?;
    }
    let result = (|| -> Result<(Option<i64>, bool)> {
        let tarball = work.join("bundle.tar.gz");
        let path = job["bundle"]
            .as_str()
            .ok_or_else(|| anyhow!("no bundle url"))?;
        let (code, _) = client.request("GET", path, None, 300, Some(&tarball))?;
        if code != 200 {
            bail!("bundle download failed ({code})");
        }
        let st = Command::new("tar")
            .arg("-xzf")
            .arg(&tarball)
            .arg("-C")
            .arg(&work)
            .status()?;
        if !st.success() {
            bail!("could not extract the job bundle");
        }
        let (jobdir, proj) = (work.join("job"), work.join("project"));
        let args: Vec<String> = job["args"]
            .as_array()
            .ok_or_else(|| anyhow!("no args"))?
            .iter()
            .filter_map(|a| a.as_str())
            .map(|a| {
                a.replace("@JOB@", &jobdir.display().to_string())
                    .replace("@PROJ@", &proj.display().to_string())
            })
            .collect();
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.args(&args)
            .current_dir(&proj)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .env("NO_COLOR", "1");
        for k in ["PATH", "HOME", "LANG", "SSH_AUTH_SOCK", "TMPDIR"] {
            if let Ok(v) = std::env::var(k) {
                cmd.env(k, v);
            }
        }
        let mut child = cmd.spawn().context("starting the playbook runner")?;
        let (tx, rx) = mpsc::channel::<String>();
        for (is_out, stream) in [
            (
                true,
                child
                    .stdout
                    .take()
                    .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            ),
            (
                false,
                child
                    .stderr
                    .take()
                    .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            ),
        ] {
            if let Some(s) = stream {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    for l in BufReader::new(s).lines().map_while(Result::ok) {
                        let line = if is_out {
                            l
                        } else {
                            serde_json::to_string(&Event::Message { text: l }).unwrap_or_default()
                        };
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                });
            }
        }
        drop(tx);
        let mut canceled = false;
        let mut last_post = Instant::now();
        let mut buf: Vec<String> = vec![];
        loop {
            match rx.recv_timeout(Duration::from_millis(300)) {
                Ok(l) => buf.push(l),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if !buf.is_empty() {
                        post_events(client, id, &buf);
                    }
                    break;
                }
            }
            if !buf.is_empty()
                && (buf.len() >= 50 || last_post.elapsed() > Duration::from_millis(300))
                || last_post.elapsed() > Duration::from_secs(5)
            {
                if post_events(client, id, &buf) && !canceled {
                    canceled = true;
                    let _ = child.kill();
                }
                buf.clear();
                last_post = Instant::now();
            }
        }
        let status = child.wait()?;
        Ok((status.code().map(|c| c as i64), canceled))
    })();
    let _ = std::fs::remove_dir_all(&work);
    let body = match &result {
        Ok((rc, canceled)) => serde_json::json!({"rc": rc, "canceled": canceled}),
        Err(e) => serde_json::json!({"rc": null, "error": format!("{e:#}")}),
    };
    client.json(
        "POST",
        &format!("/api/v2/worker/jobs/{id}/finish/"),
        Some(&body),
        30,
    )?;
    result.map(|_| ())
}
