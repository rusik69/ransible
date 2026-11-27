//! End-to-end test of `ransible serve`: REST API + job execution (uses curl as the HTTP client).
#![cfg(feature = "server")]
use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    base: String,
    dir: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn curl_ok() -> bool {
    Command::new("curl")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn start(tag: &str) -> Server {
    start_with(tag, &[])
}

fn start_with(tag: &str, extra: &[&str]) -> Server {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dir = std::env::temp_dir().join(format!("ransible-srv-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("adminpw"), "admin-password-1\n").unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_ransible"))
        .args([
            "serve",
            "--listen",
            &format!("127.0.0.1:{port}"),
            "--data-dir",
            dir.join("data").to_str().unwrap(),
            "--admin-password-file",
            dir.join("adminpw").to_str().unwrap(),
        ])
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let s = Server {
        child,
        base: format!("http://127.0.0.1:{port}"),
        dir,
    };
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(15) {
        if curl(&s, "GET", "/api/v2/ping/", None, None).0 == 200 {
            return s;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("server did not start");
}

fn curl(
    s: &Server,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (u16, String) {
    let mut c = Command::new("curl");
    c.args([
        "-s",
        "-m",
        "30",
        "-X",
        method,
        "-w",
        "\n%{http_code}",
        "-H",
        "Content-Type: application/json",
    ]);
    if let Some(t) = token {
        c.args(["-H", &format!("Authorization: Bearer {t}")]);
    }
    if let Some(b) = body {
        c.args(["-d", &b.to_string()]);
    }
    c.arg(format!("{}{}", s.base, path));
    let o = c.output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout).to_string();
    let (b, code) = out.rsplit_once('\n').unwrap_or(("", "0"));
    (code.trim().parse().unwrap_or(0), b.to_string())
}

fn j(r: (u16, String)) -> Value {
    serde_json::from_str(&r.1).unwrap_or(Value::Null)
}

fn login(s: &Server, user: &str, pw: &str) -> (u16, String) {
    curl(
        s,
        "POST",
        "/api/v2/auth/login/",
        None,
        Some(json!({"username": user, "password": pw})),
    )
}

#[test]
fn auth_rbac_and_secrets() {
    if !curl_ok() {
        return;
    }
    let s = start("auth");
    assert_eq!(curl(&s, "GET", "/api/v2/projects/", None, None).0, 401);
    assert_eq!(login(&s, "admin", "wrong").0, 401);
    let tok = j(login(&s, "admin", "admin-password-1"))["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        j(curl(&s, "GET", "/api/v2/me/", Some(&tok), None))["role"],
        "admin"
    );

    // secrets are write-only
    let cred = j(curl(
        &s,
        "POST",
        "/api/v2/credentials/",
        Some(&tok),
        Some(
            json!({"name": "c1", "username": "u", "secrets": {"password": "TOPSECRET", "ssh_private_key": "KEYDATA"}}),
        ),
    ));
    assert_eq!(cred["secrets"]["password"], "$encrypted$");
    let listed = curl(&s, "GET", "/api/v2/credentials/", Some(&tok), None).1;
    assert!(
        !listed.contains("TOPSECRET")
            && !listed.contains("KEYDATA")
            && !listed.contains("ANSIBLE_VAULT")
    );
    // updating other fields keeps secrets
    let id = cred["id"].as_i64().unwrap();
    curl(
        &s,
        "PUT",
        &format!("/api/v2/credentials/{id}/"),
        Some(&tok),
        Some(json!({"username": "u2", "secrets": {"password": "$encrypted$"}})),
    );
    let after = j(curl(
        &s,
        "GET",
        &format!("/api/v2/credentials/{id}/"),
        Some(&tok),
        None,
    ));
    assert_eq!(after["username"], "u2");
    assert_eq!(after["secrets"]["ssh_private_key"], "$encrypted$");

    // users + roles
    let (code, _) = curl(
        &s,
        "POST",
        "/api/v2/users/",
        Some(&tok),
        Some(json!({"username": "view", "password": "viewer-pass-1", "role": "viewer"})),
    );
    assert_eq!(code, 201);
    assert_eq!(
        curl(
            &s,
            "POST",
            "/api/v2/users/",
            Some(&tok),
            Some(json!({"username": "x", "password": "short"}))
        )
        .0,
        400
    );
    let vt = j(login(&s, "view", "viewer-pass-1"))["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(curl(&s, "GET", "/api/v2/projects/", Some(&vt), None).0, 200);
    assert_eq!(
        curl(
            &s,
            "POST",
            "/api/v2/projects/",
            Some(&vt),
            Some(json!({"name": "p"}))
        )
        .0,
        403
    );
    assert_eq!(curl(&s, "GET", "/api/v2/users/", Some(&vt), None).0, 200); // read-only
    // duplicate names are a clean 400
    curl(
        &s,
        "POST",
        "/api/v2/projects/",
        Some(&tok),
        Some(json!({"name": "dup"})),
    );
    assert_eq!(
        curl(
            &s,
            "POST",
            "/api/v2/projects/",
            Some(&tok),
            Some(json!({"name": "dup"}))
        )
        .0,
        400
    );
    // SPA is served with security headers
    let r = Command::new("curl")
        .args(["-sI", &format!("{}/", s.base)])
        .output()
        .unwrap();
    let h = String::from_utf8_lossy(&r.stdout).to_lowercase();
    assert!(h.contains("content-security-policy") && h.contains("x-content-type-options: nosniff"));
}

fn wait_job(s: &Server, tok: &str, id: i64) -> Value {
    let t = Instant::now();
    loop {
        let job = j(curl(
            s,
            "GET",
            &format!("/api/v2/jobs/{id}/"),
            Some(tok),
            None,
        ));
        let st = job["status"].as_str().unwrap_or("").to_string();
        if !["pending", "running"].contains(&st.as_str()) {
            return job;
        }
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "job timed out: {job}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn launch_job_and_stream_output() {
    if !curl_ok() {
        return;
    }
    let s = start("job");
    let tok = j(login(&s, "admin", "admin-password-1"))["token"]
        .as_str()
        .unwrap()
        .to_string();
    // project = local directory with a playbook
    let proj_dir = s.dir.join("proj");
    std::fs::create_dir_all(&proj_dir).unwrap();
    std::fs::write(proj_dir.join("site.yml"), "- hosts: all\n  gather_facts: no\n  tasks:\n    - debug: msg=\"hello {{ who }}\"\n    - command: echo done\n    - fail: msg=boom\n      when: fail_me | default(false) | bool\n").unwrap();
    let p = j(curl(
        &s,
        "POST",
        "/api/v2/projects/",
        Some(&tok),
        Some(json!({"name": "p", "local_path": proj_dir.to_str().unwrap()})),
    ));
    let pid = p["id"].as_i64().unwrap();
    assert_eq!(
        j(curl(
            &s,
            "POST",
            &format!("/api/v2/projects/{pid}/sync/"),
            Some(&tok),
            None
        ))["status"],
        "successful"
    );
    assert_eq!(
        j(curl(
            &s,
            "GET",
            &format!("/api/v2/projects/{pid}/playbooks/"),
            Some(&tok),
            None
        )),
        json!(["site.yml"])
    );
    let inv = j(curl(
        &s,
        "POST",
        "/api/v2/inventories/",
        Some(&tok),
        Some(json!({"name": "i", "content": "[web]\nlocalhost ansible_connection=local\n"})),
    ));
    let iid = inv["id"].as_i64().unwrap();
    assert_eq!(
        j(curl(
            &s,
            "GET",
            &format!("/api/v2/inventories/{iid}/hosts/"),
            Some(&tok),
            None
        ))["hosts"],
        json!(["localhost"])
    );
    let t = j(curl(
        &s,
        "POST",
        "/api/v2/job_templates/",
        Some(&tok),
        Some(
            json!({"name": "t", "project_id": pid, "playbook": "site.yml", "inventory_id": iid, "extra_vars": "who: world"}),
        ),
    ));
    let tid = t["id"].as_i64().unwrap();

    let (code, body) = curl(
        &s,
        "POST",
        &format!("/api/v2/job_templates/{tid}/launch/"),
        Some(&tok),
        Some(json!({})),
    );
    assert_eq!(code, 201, "{body}");
    let id = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_i64()
        .unwrap();
    let job = wait_job(&s, &tok, id);
    assert_eq!(job["status"], "successful", "{job}");
    assert_eq!(job["summary"][0]["host"], "localhost");
    let out = curl(
        &s,
        "GET",
        &format!("/api/v2/jobs/{id}/stdout/"),
        Some(&tok),
        None,
    )
    .1;
    assert!(
        out.contains("hello world") && out.contains("PLAY RECAP") && out.contains("ok=2"),
        "{out}"
    );
    let ev = j(curl(
        &s,
        "GET",
        &format!("/api/v2/jobs/{id}/job_events/"),
        Some(&tok),
        None,
    ));
    assert!(ev["count"].as_i64().unwrap() > 5);
    // SSE replay of a finished job ends with a done event
    let sse = Command::new("curl")
        .args([
            "-sN",
            "-m",
            "10",
            &format!("{}/api/v2/jobs/{id}/stream/?token={tok}", s.base),
        ])
        .output()
        .unwrap();
    let sse = String::from_utf8_lossy(&sse.stdout);
    assert!(
        sse.contains("hello world") && sse.contains("event: done") && sse.contains("successful"),
        "{sse}"
    );

    // launch-time overrides: failing run -> status failed
    let (_, body) = curl(
        &s,
        "POST",
        &format!("/api/v2/job_templates/{tid}/launch/"),
        Some(&tok),
        Some(json!({"extra_vars": {"who": "x", "fail_me": true}})),
    );
    let fid = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_i64()
        .unwrap();
    assert_eq!(wait_job(&s, &tok, fid)["status"], "failed");
    // relaunch
    let (c2, b2) = curl(
        &s,
        "POST",
        &format!("/api/v2/jobs/{id}/relaunch/"),
        Some(&tok),
        None,
    );
    assert_eq!(c2, 201);
    let rid = serde_json::from_str::<Value>(&b2).unwrap()["id"]
        .as_i64()
        .unwrap();
    assert_eq!(wait_job(&s, &tok, rid)["status"], "successful");
    // credentials never end up in the job record
    assert!(
        !curl(&s, "GET", &format!("/api/v2/jobs/{id}/"), Some(&tok), None)
            .1
            .contains("password")
    );
    let dash = j(curl(&s, "GET", "/api/v2/dashboard/", Some(&tok), None));
    assert_eq!(dash["counts"]["jobs"], 3);
}

#[test]
fn cancel_running_job() {
    if !curl_ok() {
        return;
    }
    let s = start("cancel");
    let tok = j(login(&s, "admin", "admin-password-1"))["token"]
        .as_str()
        .unwrap()
        .to_string();
    let proj_dir = s.dir.join("proj");
    std::fs::create_dir_all(&proj_dir).unwrap();
    std::fs::write(
        proj_dir.join("slow.yml"),
        "- hosts: all\n  gather_facts: no\n  tasks:\n    - command: sleep 30\n",
    )
    .unwrap();
    let pid = j(curl(
        &s,
        "POST",
        "/api/v2/projects/",
        Some(&tok),
        Some(json!({"name": "p", "local_path": proj_dir.to_str().unwrap()})),
    ))["id"]
        .as_i64()
        .unwrap();
    let iid = j(curl(
        &s,
        "POST",
        "/api/v2/inventories/",
        Some(&tok),
        Some(json!({"name": "i", "content": "localhost ansible_connection=local\n"})),
    ))["id"]
        .as_i64()
        .unwrap();
    let tid = j(curl(
        &s,
        "POST",
        "/api/v2/job_templates/",
        Some(&tok),
        Some(json!({"name": "t", "project_id": pid, "playbook": "slow.yml", "inventory_id": iid})),
    ))["id"]
        .as_i64()
        .unwrap();
    let id = j(curl(
        &s,
        "POST",
        &format!("/api/v2/job_templates/{tid}/launch/"),
        Some(&tok),
        None,
    ))["id"]
        .as_i64()
        .unwrap();
    let t = Instant::now();
    loop {
        let st = j(curl(
            &s,
            "GET",
            &format!("/api/v2/jobs/{id}/"),
            Some(&tok),
            None,
        ))["status"]
            .as_str()
            .unwrap()
            .to_string();
        if st == "running" {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        curl(
            &s,
            "POST",
            &format!("/api/v2/jobs/{id}/cancel/"),
            Some(&tok),
            None
        )
        .0,
        200
    );
    let job = wait_job(&s, &tok, id);
    assert_eq!(job["status"], "canceled");
    assert!(
        t.elapsed() < Duration::from_secs(25),
        "cancel must not wait for the 30s sleep"
    );
}

#[test]
fn schedules_notifications_and_activity() {
    if !curl_ok() {
        return;
    }
    let s = start("misc");
    let tok = j(login(&s, "admin", "admin-password-1"))["token"]
        .as_str()
        .unwrap()
        .to_string();
    // webhook receiver
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let hook_port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for stream in listener.incoming().flatten().take(4) {
            let mut stream = stream;
            let mut buf = [0u8; 8192];
            let mut got = Vec::new();
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        got.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&got).to_string();
                        if let Some(i) = text.find("\r\n\r\n") {
                            let cl = text
                                .to_lowercase()
                                .split("content-length: ")
                                .nth(1)
                                .and_then(|r| r.split("\r\n").next())
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if got.len() >= i + 4 + cl {
                                break;
                            }
                        }
                    }
                }
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let text = String::from_utf8_lossy(&got).to_string();
            let _ = tx.send(text.split("\r\n\r\n").nth(1).unwrap_or("").to_string());
        }
    });
    // invalid inputs
    assert_eq!(
        curl(
            &s,
            "POST",
            "/api/v2/notification_templates/",
            Some(&tok),
            Some(json!({"name": "n", "webhook_url": "ftp://x"}))
        )
        .0,
        400
    );
    let n = j(curl(
        &s,
        "POST",
        "/api/v2/notification_templates/",
        Some(&tok),
        Some(
            json!({"name": "hook", "kind": "webhook", "webhook_url": format!("http://127.0.0.1:{hook_port}/hook"), "on_success": true, "on_failure": true}),
        ),
    ));
    assert_eq!(n["webhook_url"], "$encrypted$");
    assert!(
        !curl(
            &s,
            "GET",
            "/api/v2/notification_templates/",
            Some(&tok),
            None
        )
        .1
        .contains("127.0.0.1")
    );

    let proj_dir = s.dir.join("proj");
    std::fs::create_dir_all(&proj_dir).unwrap();
    std::fs::write(
        proj_dir.join("site.yml"),
        "- hosts: all\n  gather_facts: no\n  tasks:\n    - debug: msg=hi\n",
    )
    .unwrap();
    let pid = j(curl(
        &s,
        "POST",
        "/api/v2/projects/",
        Some(&tok),
        Some(json!({"name": "p", "local_path": proj_dir.to_str().unwrap()})),
    ))["id"]
        .as_i64()
        .unwrap();
    let iid = j(curl(
        &s,
        "POST",
        "/api/v2/inventories/",
        Some(&tok),
        Some(json!({"name": "i", "content": "localhost ansible_connection=local\n"})),
    ))["id"]
        .as_i64()
        .unwrap();
    let tid = j(curl(
        &s,
        "POST",
        "/api/v2/job_templates/",
        Some(&tok),
        Some(json!({"name": "t", "project_id": pid, "playbook": "site.yml", "inventory_id": iid})),
    ))["id"]
        .as_i64()
        .unwrap();

    // schedules: validation + CRUD
    assert_eq!(
        curl(
            &s,
            "POST",
            "/api/v2/schedules/",
            Some(&tok),
            Some(json!({"name": "bad", "template_id": tid, "cron": "nope"}))
        )
        .0,
        400
    );
    let sc = j(curl(
        &s,
        "POST",
        "/api/v2/schedules/",
        Some(&tok),
        Some(json!({"name": "nightly", "template_id": tid, "cron": "0 3 * * *", "enabled": true})),
    ));
    assert_eq!(sc["cron"], "0 3 * * *");
    assert_eq!(sc["enabled"], 1);

    let id = j(curl(
        &s,
        "POST",
        &format!("/api/v2/job_templates/{tid}/launch/"),
        Some(&tok),
        None,
    ))["id"]
        .as_i64()
        .unwrap();
    assert_eq!(wait_job(&s, &tok, id)["status"], "successful");
    let body = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("webhook not delivered");
    let payload: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(payload["status"], "successful");
    assert_eq!(payload["id"], id);

    // audit trail
    let act = curl(
        &s,
        "GET",
        "/api/v2/activity_stream/?page_size=100",
        Some(&tok),
        None,
    )
    .1;
    assert!(
        act.contains("\"launch\"") && act.contains("\"create\""),
        "{act}"
    );
    assert_eq!(
        curl(
            &s,
            "POST",
            "/api/v2/activity_stream/",
            Some(&tok),
            Some(json!({"action": "x"}))
        )
        .0,
        403
    );
}

fn wait_wf(s: &Server, tok: &str, id: i64, until: impl Fn(&Value) -> bool) -> Value {
    let t = Instant::now();
    loop {
        let wj = j(curl(
            s,
            "GET",
            &format!("/api/v2/workflow_jobs/{id}/"),
            Some(tok),
            None,
        ));
        if until(&wj) {
            return wj;
        }
        assert!(
            t.elapsed() < Duration::from_secs(90),
            "workflow timed out: {wj}"
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

#[test]
fn workflow_with_failure_path_and_approval() {
    if !curl_ok() {
        return;
    }
    let s = start("wf");
    let tok = j(login(&s, "admin", "admin-password-1"))["token"]
        .as_str()
        .unwrap()
        .to_string();
    let proj_dir = s.dir.join("proj");
    std::fs::create_dir_all(&proj_dir).unwrap();
    std::fs::write(
        proj_dir.join("ok.yml"),
        "- hosts: all\n  gather_facts: no\n  tasks:\n    - debug: msg=ok\n",
    )
    .unwrap();
    std::fs::write(
        proj_dir.join("bad.yml"),
        "- hosts: all\n  gather_facts: no\n  tasks:\n    - fail: msg=boom\n",
    )
    .unwrap();
    let pid = j(curl(
        &s,
        "POST",
        "/api/v2/projects/",
        Some(&tok),
        Some(json!({"name": "p", "local_path": proj_dir.to_str().unwrap()})),
    ))["id"]
        .as_i64()
        .unwrap();
    let iid = j(curl(
        &s,
        "POST",
        "/api/v2/inventories/",
        Some(&tok),
        Some(json!({"name": "i", "content": "localhost ansible_connection=local\n"})),
    ))["id"]
        .as_i64()
        .unwrap();
    let mk = |name: &str, pb: &str| {
        j(curl(
            &s,
            "POST",
            "/api/v2/job_templates/",
            Some(&tok),
            Some(json!({"name": name, "project_id": pid, "playbook": pb, "inventory_id": iid})),
        ))["id"]
            .as_i64()
            .unwrap()
    };
    let (ok, bad) = (mk("ok", "ok.yml"), mk("bad", "bad.yml"));
    let wf = j(curl(
        &s,
        "POST",
        "/api/v2/workflow_templates/",
        Some(&tok),
        Some(json!({"name": "wf"})),
    ));
    let wid = wf["id"].as_i64().unwrap();
    let graph = json!({
        "nodes": [
            {"key": "a", "label": "a", "template_id": ok},
            {"key": "b", "label": "b", "template_id": bad},
            {"key": "c", "label": "c", "template_id": ok},
            {"key": "d", "label": "d", "template_id": ok},
            {"key": "gate", "label": "gate", "kind": "approval"},
            {"key": "e", "label": "e", "template_id": ok}
        ],
        "edges": [
            {"from": "a", "to": "b", "kind": "success"},
            {"from": "b", "to": "c", "kind": "failure"},
            {"from": "b", "to": "d", "kind": "success"},
            {"from": "c", "to": "gate", "kind": "success"},
            {"from": "gate", "to": "e", "kind": "success"}
        ]
    });
    // invalid graphs are rejected
    let cyc = json!({"nodes": [{"key": "x", "template_id": ok}, {"key": "y", "template_id": ok}], "edges": [{"from": "x", "to": "y"}, {"from": "y", "to": "x"}]});
    assert_eq!(
        curl(
            &s,
            "PUT",
            &format!("/api/v2/workflow_templates/{wid}/graph/"),
            Some(&tok),
            Some(cyc)
        )
        .0,
        400
    );
    assert_eq!(
        curl(
            &s,
            "POST",
            &format!("/api/v2/workflow_templates/{wid}/launch/"),
            Some(&tok),
            None
        )
        .0,
        400
    ); // empty
    assert_eq!(
        curl(
            &s,
            "PUT",
            &format!("/api/v2/workflow_templates/{wid}/graph/"),
            Some(&tok),
            Some(graph)
        )
        .0,
        200
    );
    assert_eq!(
        j(curl(
            &s,
            "GET",
            &format!("/api/v2/workflow_templates/{wid}/graph/"),
            Some(&tok),
            None
        ))["nodes"]
            .as_array()
            .unwrap()
            .len(),
        6
    );

    let wj = j(curl(
        &s,
        "POST",
        &format!("/api/v2/workflow_templates/{wid}/launch/"),
        Some(&tok),
        None,
    ))["id"]
        .as_i64()
        .unwrap();
    let st = wait_wf(&s, &tok, wj, |w| w["state"]["gate"]["status"] == "waiting");
    assert_eq!(st["state"]["a"]["status"], "successful");
    assert_eq!(st["state"]["b"]["status"], "failed");
    assert_eq!(st["state"]["c"]["status"], "successful");
    assert_eq!(st["state"]["d"]["status"], "skipped");
    assert_eq!(st["status"], "running");
    // approve the gate
    assert_eq!(
        curl(
            &s,
            "POST",
            &format!("/api/v2/workflow_jobs/{wj}/nodes/gate/approve/"),
            Some(&tok),
            None
        )
        .0,
        200
    );
    let done = wait_wf(&s, &tok, wj, |w| {
        !["pending", "running"].contains(&w["status"].as_str().unwrap_or(""))
    });
    assert_eq!(done["state"]["e"]["status"], "successful", "{done}");
    assert_eq!(done["status"], "successful"); // the failure was handled by the failure edge
    // child jobs are linked to the workflow job
    let a_job = done["state"]["a"]["job_id"].as_i64().unwrap();
    assert_eq!(
        j(curl(
            &s,
            "GET",
            &format!("/api/v2/jobs/{a_job}/"),
            Some(&tok),
            None
        ))["status"],
        "successful"
    );

    // deny path: workflow fails
    let graph2 = json!({"nodes": [{"key": "gate", "kind": "approval"}, {"key": "x", "template_id": ok}], "edges": [{"from": "gate", "to": "x", "kind": "success"}]});
    curl(
        &s,
        "PUT",
        &format!("/api/v2/workflow_templates/{wid}/graph/"),
        Some(&tok),
        Some(graph2),
    );
    let wj2 = j(curl(
        &s,
        "POST",
        &format!("/api/v2/workflow_templates/{wid}/launch/"),
        Some(&tok),
        None,
    ))["id"]
        .as_i64()
        .unwrap();
    wait_wf(&s, &tok, wj2, |w| w["state"]["gate"]["status"] == "waiting");
    assert_eq!(
        curl(
            &s,
            "POST",
            &format!("/api/v2/workflow_jobs/{wj2}/nodes/gate/deny/"),
            Some(&tok),
            None
        )
        .0,
        200
    );
    let d2 = wait_wf(&s, &tok, wj2, |w| {
        !["pending", "running"].contains(&w["status"].as_str().unwrap_or(""))
    });
    assert_eq!(d2["status"], "failed");
    assert_eq!(d2["state"]["x"]["status"], "skipped");
    // cancel a waiting workflow
    let wj3 = j(curl(
        &s,
        "POST",
        &format!("/api/v2/workflow_templates/{wid}/launch/"),
        Some(&tok),
        None,
    ))["id"]
        .as_i64()
        .unwrap();
    wait_wf(&s, &tok, wj3, |w| w["state"]["gate"]["status"] == "waiting");
    assert_eq!(
        curl(
            &s,
            "POST",
            &format!("/api/v2/workflow_jobs/{wj3}/cancel/"),
            Some(&tok),
            None
        )
        .0,
        200
    );
    assert_eq!(
        wait_wf(&s, &tok, wj3, |w| w["status"] == "canceled")["state"]["x"]["status"],
        "canceled"
    );
}

#[test]
fn survey_validation_and_secret_answers() {
    if !curl_ok() {
        return;
    }
    let s = start("survey");
    let tok = j(login(&s, "admin", "admin-password-1"))["token"]
        .as_str()
        .unwrap()
        .to_string();
    let proj_dir = s.dir.join("proj");
    std::fs::create_dir_all(&proj_dir).unwrap();
    std::fs::write(proj_dir.join("site.yml"), "- hosts: all\n  gather_facts: no\n  tasks:\n    - debug: msg=\"env={{ env }} n={{ n }}\"\n    - copy: {content: \"{{ pw }}\", dest: \"/tmp/ransible-survey-pw-{{ n }}\"}\n").unwrap();
    let pid = j(curl(
        &s,
        "POST",
        "/api/v2/projects/",
        Some(&tok),
        Some(json!({"name": "p", "local_path": proj_dir.to_str().unwrap()})),
    ))["id"]
        .as_i64()
        .unwrap();
    let iid = j(curl(
        &s,
        "POST",
        "/api/v2/inventories/",
        Some(&tok),
        Some(json!({"name": "i", "content": "localhost ansible_connection=local\n"})),
    ))["id"]
        .as_i64()
        .unwrap();
    let spec = json!([
        {"variable": "env", "type": "multiplechoice", "choices": ["dev", "prod"], "default": "dev"},
        {"variable": "n", "type": "integer", "required": true, "min": 1, "max": 100},
        {"variable": "pw", "type": "password", "required": true}
    ]);
    // a malformed spec is rejected
    assert_eq!(
        curl(
            &s,
            "POST",
            "/api/v2/job_templates/",
            Some(&tok),
            Some(json!({"name": "bad", "survey_spec": "{nope"}))
        )
        .0,
        400
    );
    let tid = j(curl(&s, "POST", "/api/v2/job_templates/", Some(&tok), Some(json!({"name": "t", "project_id": pid, "playbook": "site.yml", "inventory_id": iid, "survey_enabled": true, "survey_spec": spec.to_string()}))))["id"].as_i64().unwrap();
    let launch = |ev: Value| {
        curl(
            &s,
            "POST",
            &format!("/api/v2/job_templates/{tid}/launch/"),
            Some(&tok),
            Some(json!({"extra_vars": ev})),
        )
    };
    let (c, b) = launch(json!({"pw": "x"}));
    assert_eq!(c, 400);
    assert!(b.contains("'n' is required") || b.contains("n"), "{b}");
    assert_eq!(launch(json!({"n": 1000, "pw": "x"})).0, 400);
    assert_eq!(launch(json!({"n": 5, "pw": "x", "env": "stage"})).0, 400);
    let secret = format!("s3cr3t-{}", std::process::id());
    let (c, b) = launch(json!({"n": 7, "pw": secret}));
    assert_eq!(c, 201, "{b}");
    let id = serde_json::from_str::<Value>(&b).unwrap()["id"]
        .as_i64()
        .unwrap();
    assert_eq!(wait_job(&s, &tok, id)["status"], "successful");
    // defaults applied, answers used, password reached the playbook but is never exposed by the API
    let out = curl(
        &s,
        "GET",
        &format!("/api/v2/jobs/{id}/stdout/"),
        Some(&tok),
        None,
    )
    .1;
    assert!(out.contains("env=dev n=7"), "{out}");
    assert_eq!(
        std::fs::read_to_string("/tmp/ransible-survey-pw-7").unwrap_or_default(),
        secret
    );
    let _ = std::fs::remove_file("/tmp/ransible-survey-pw-7");
    let job = curl(&s, "GET", &format!("/api/v2/jobs/{id}/"), Some(&tok), None).1;
    assert!(
        !job.contains(&secret) && job.contains("$encrypted$"),
        "{job}"
    );
    assert!(
        !curl(
            &s,
            "GET",
            &format!("/api/v2/jobs/{id}/job_events/"),
            Some(&tok),
            None
        )
        .1
        .contains(&secret)
    );
}

#[test]
fn teams_and_object_level_grants() {
    if !curl_ok() {
        return;
    }
    let s = start("rbac");
    let tok = j(login(&s, "admin", "admin-password-1"))["token"]
        .as_str()
        .unwrap()
        .to_string();
    let proj_dir = s.dir.join("proj");
    std::fs::create_dir_all(&proj_dir).unwrap();
    std::fs::write(
        proj_dir.join("site.yml"),
        "- hosts: all\n  gather_facts: no\n  tasks:\n    - debug: msg=hi\n",
    )
    .unwrap();
    let pid = j(curl(
        &s,
        "POST",
        "/api/v2/projects/",
        Some(&tok),
        Some(json!({"name": "p", "local_path": proj_dir.to_str().unwrap()})),
    ))["id"]
        .as_i64()
        .unwrap();
    let iid = j(curl(
        &s,
        "POST",
        "/api/v2/inventories/",
        Some(&tok),
        Some(json!({"name": "i", "content": "localhost ansible_connection=local\n"})),
    ))["id"]
        .as_i64()
        .unwrap();
    let mk = |n: &str| {
        j(curl(
            &s,
            "POST",
            "/api/v2/job_templates/",
            Some(&tok),
            Some(
                json!({"name": n, "project_id": pid, "playbook": "site.yml", "inventory_id": iid}),
            ),
        ))["id"]
            .as_i64()
            .unwrap()
    };
    let (t1, t2) = (mk("t1"), mk("t2"));
    let uid = j(curl(
        &s,
        "POST",
        "/api/v2/users/",
        Some(&tok),
        Some(json!({"username": "v1", "password": "viewer-pass-1", "role": "viewer"})),
    ))["id"]
        .as_i64()
        .unwrap();
    let vt = j(login(&s, "v1", "viewer-pass-1"))["token"]
        .as_str()
        .unwrap()
        .to_string();
    let launch = |t: i64| {
        curl(
            &s,
            "POST",
            &format!("/api/v2/job_templates/{t}/launch/"),
            Some(&vt),
            None,
        )
    };

    // nothing granted yet
    assert_eq!(launch(t1).0, 403);
    // only admins manage teams/grants
    assert_eq!(
        curl(
            &s,
            "POST",
            "/api/v2/teams/",
            Some(&vt),
            Some(json!({"name": "x"}))
        )
        .0,
        403
    );
    let team = j(curl(
        &s,
        "POST",
        "/api/v2/teams/",
        Some(&tok),
        Some(json!({"name": "ops"})),
    ));
    let team_id = team["id"].as_i64().unwrap();
    assert_eq!(
        curl(
            &s,
            "POST",
            &format!("/api/v2/teams/{team_id}/members/"),
            Some(&tok),
            Some(json!({"user_id": uid}))
        )
        .0,
        204
    );
    assert_eq!(
        j(curl(
            &s,
            "GET",
            &format!("/api/v2/teams/{team_id}/members/"),
            Some(&tok),
            None
        ))["results"][0]["username"],
        "v1"
    );
    // validation
    for bad in [
        json!({"subject_type": "team", "subject_id": team_id, "object_type": "users", "object_id": 1, "role": "execute"}),
        json!({"subject_type": "team", "subject_id": 999, "object_type": "job_templates", "object_id": t1, "role": "execute"}),
        json!({"subject_type": "team", "subject_id": team_id, "object_type": "job_templates", "object_id": t1, "role": "owner"}),
    ] {
        assert_eq!(
            curl(&s, "POST", "/api/v2/grants/", Some(&tok), Some(bad)).0,
            400
        );
    }
    // grant the team execute on t1 only
    let g = j(curl(
        &s,
        "POST",
        "/api/v2/grants/",
        Some(&tok),
        Some(
            json!({"subject_type": "team", "subject_id": team_id, "object_type": "job_templates", "object_id": t1, "role": "execute"}),
        ),
    ));
    let (code, body) = launch(t1);
    assert_eq!(code, 201, "{body}");
    let jid = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_i64()
        .unwrap();
    assert_eq!(wait_job(&s, &tok, jid)["status"], "successful");
    assert_eq!(launch(t2).0, 403);
    assert_eq!(
        curl(
            &s,
            "POST",
            &format!("/api/v2/jobs/{jid}/relaunch/"),
            Some(&vt),
            None
        )
        .0,
        201
    );
    // execute does not allow editing; admin grant on the object does
    let rename = |t: i64, n: &str| {
        curl(
            &s,
            "PUT",
            &format!("/api/v2/job_templates/{t}/"),
            Some(&vt),
            Some(json!({"name": n})),
        )
        .0
    };
    assert_eq!(rename(t1, "t1b"), 403);
    curl(
        &s,
        "POST",
        "/api/v2/grants/",
        Some(&tok),
        Some(
            json!({"subject_type": "user", "subject_id": uid, "object_type": "job_templates", "object_id": t1, "role": "admin"}),
        ),
    );
    assert_eq!(rename(t1, "t1b"), 200);
    assert_eq!(rename(t2, "t2b"), 403);
    assert_eq!(
        curl(
            &s,
            "DELETE",
            &format!("/api/v2/job_templates/{t2}/"),
            Some(&vt),
            None
        )
        .0,
        403
    );
    // project sync needs a grant on the project
    assert_eq!(
        curl(
            &s,
            "POST",
            &format!("/api/v2/projects/{pid}/sync/"),
            Some(&vt),
            None
        )
        .0,
        403
    );
    // revoking removes access
    assert_eq!(
        curl(
            &s,
            "DELETE",
            &format!("/api/v2/grants/{}/", g["id"]),
            Some(&tok),
            None
        )
        .0,
        204
    );
    let gl = j(curl(&s, "GET", "/api/v2/grants/", Some(&tok), None));
    assert_eq!(gl["count"], 1); // the user-level admin grant remains
    // deleting the user cleans up grants and memberships
    assert_eq!(
        curl(
            &s,
            "DELETE",
            &format!("/api/v2/users/{uid}/"),
            Some(&tok),
            None
        )
        .0,
        204
    );
    assert_eq!(
        j(curl(&s, "GET", "/api/v2/grants/", Some(&tok), None))["count"],
        0
    );
}

#[test]
fn serves_https_with_tls_cert_and_key() {
    if !curl_ok() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("ransible-tls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    let made = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=IP:127.0.0.1,DNS:localhost",
            "-keyout",
        ])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output();
    if !made.map(|o| o.status.success()).unwrap_or(false) {
        eprintln!("openssl not available; skipping TLS test");
        return;
    }
    std::fs::write(dir.join("adminpw"), "admin-password-1\n").unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = Command::new(env!("CARGO_BIN_EXE_ransible"))
        .args([
            "serve",
            "--listen",
            &format!("127.0.0.1:{port}"),
            "--data-dir",
            dir.join("data").to_str().unwrap(),
            "--admin-password-file",
            dir.join("adminpw").to_str().unwrap(),
            "--tls-cert",
            cert.to_str().unwrap(),
            "--tls-key",
            key.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let srv = Server {
        child,
        base: format!("https://127.0.0.1:{port}"),
        dir: dir.clone(),
    };
    let get = |args: &[&str], url: &str| {
        Command::new("curl")
            .args(["-s", "-m", "10", "-o", "/dev/null", "-w", "%{http_code}"])
            .args(args)
            .arg(url)
            .output()
            .unwrap()
    };
    let t = Instant::now();
    let mut code = String::new();
    while t.elapsed() < Duration::from_secs(15) {
        code = String::from_utf8_lossy(
            &get(
                &["--cacert", cert.to_str().unwrap()],
                &format!("{}/api/v2/ping/", srv.base),
            )
            .stdout,
        )
        .to_string();
        if code == "200" {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(code, "200", "HTTPS ping with the trusted certificate");
    // an untrusted client must be rejected by certificate verification; plain HTTP must not work
    let untrusted = get(&[], &format!("{}/api/v2/ping/", srv.base));
    assert!(
        !untrusted.status.success(),
        "curl without --cacert must fail verification"
    );
    let plain = get(&[], &format!("http://127.0.0.1:{port}/api/v2/ping/"));
    assert_ne!(String::from_utf8_lossy(&plain.stdout), "200");
    // login works over TLS
    let out = Command::new("curl")
        .args([
            "-s",
            "-m",
            "20",
            "--cacert",
            cert.to_str().unwrap(),
            "-H",
            "Content-Type: application/json",
            "-d",
            r#"{"username":"admin","password":"admin-password-1"}"#,
            &format!("{}/api/v2/auth/login/", srv.base),
        ])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("token"));
}

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_worker(s: &Server, token: &str) -> Worker {
    Worker(
        Command::new(env!("CARGO_BIN_EXE_ransible"))
            .args(["worker", "--server", &s.base, "--token", token])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

#[test]
fn remote_worker_runs_jobs_streams_output_and_can_be_canceled() {
    if !curl_ok() {
        return;
    }
    let s = start("worker");
    let tok = j(login(&s, "admin", "admin-password-1"))["token"]
        .as_str()
        .unwrap()
        .to_string();
    let proj_dir = s.dir.join("proj");
    std::fs::create_dir_all(&proj_dir).unwrap();
    std::fs::write(proj_dir.join("site.yml"), "- hosts: all\n  gather_facts: no\n  tasks:\n    - debug: msg=\"remote hello {{ who }}\"\n    - command: echo done\n").unwrap();
    std::fs::write(
        proj_dir.join("slow.yml"),
        "- hosts: all\n  gather_facts: no\n  tasks:\n    - command: sleep 30\n",
    )
    .unwrap();
    let pid = j(curl(
        &s,
        "POST",
        "/api/v2/projects/",
        Some(&tok),
        Some(json!({"name": "p", "local_path": proj_dir.to_str().unwrap()})),
    ))["id"]
        .as_i64()
        .unwrap();
    let iid = j(curl(
        &s,
        "POST",
        "/api/v2/inventories/",
        Some(&tok),
        Some(json!({"name": "i", "content": "localhost ansible_connection=local\n"})),
    ))["id"]
        .as_i64()
        .unwrap();
    // unknown worker names are rejected
    assert_eq!(
        curl(
            &s,
            "POST",
            "/api/v2/job_templates/",
            Some(&tok),
            Some(json!({"name": "x", "project_id": pid, "playbook": "site.yml", "worker": "nope"}))
        )
        .0,
        400
    );
    // worker management: admin only, token shown once
    let w = j(curl(
        &s,
        "POST",
        "/api/v2/workers/",
        Some(&tok),
        Some(json!({"name": "node-1"})),
    ));
    let wtok = w["token"].as_str().unwrap().to_string();
    assert!(
        !curl(&s, "GET", "/api/v2/workers/", Some(&tok), None)
            .1
            .contains(&wtok)
    );
    assert_eq!(
        curl(
            &s,
            "POST",
            "/api/v2/workers/",
            Some(&tok),
            Some(json!({"name": "bad name!"}))
        )
        .0,
        400
    );
    let mk = |name: &str, pb: &str| {
        j(curl(&s, "POST", "/api/v2/job_templates/", Some(&tok), Some(json!({"name": name, "project_id": pid, "playbook": pb, "inventory_id": iid, "worker": "node-1", "extra_vars": "who: worker"}))))["id"].as_i64().unwrap()
    };
    let (fast, slow) = (mk("fast", "site.yml"), mk("slow", "slow.yml"));

    // no worker online yet: the job waits, and can be canceled while pending
    let queued = j(curl(
        &s,
        "POST",
        &format!("/api/v2/job_templates/{fast}/launch/"),
        Some(&tok),
        None,
    ))["id"]
        .as_i64()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        j(curl(
            &s,
            "GET",
            &format!("/api/v2/jobs/{queued}/"),
            Some(&tok),
            None
        ))["status"],
        "pending"
    );
    assert_eq!(
        curl(
            &s,
            "POST",
            &format!("/api/v2/jobs/{queued}/cancel/"),
            Some(&tok),
            None
        )
        .0,
        200
    );
    assert_eq!(wait_job(&s, &tok, queued)["status"], "canceled");

    // a worker with a wrong token is rejected
    let mut bad_worker = Command::new(env!("CARGO_BIN_EXE_ransible"))
        .args(["worker", "--server", &s.base, "--token", "wrong"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let t = Instant::now();
    while bad_worker.try_wait().unwrap().is_none() {
        assert!(
            t.elapsed() < Duration::from_secs(15),
            "worker with a bad token should exit"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(!bad_worker.wait().unwrap().success());

    let _worker = spawn_worker(&s, &wtok);
    // run a job on the worker
    let id = j(curl(
        &s,
        "POST",
        &format!("/api/v2/job_templates/{fast}/launch/"),
        Some(&tok),
        None,
    ))["id"]
        .as_i64()
        .unwrap();
    let job = wait_job(&s, &tok, id);
    assert_eq!(job["status"], "successful", "{job}");
    assert_eq!(job["summary"][0]["host"], "localhost");
    let out = curl(
        &s,
        "GET",
        &format!("/api/v2/jobs/{id}/stdout/"),
        Some(&tok),
        None,
    )
    .1;
    assert!(
        out.contains("picked up by worker node-1")
            && out.contains("remote hello worker")
            && out.contains("PLAY RECAP"),
        "{out}"
    );
    assert!(
        j(curl(&s, "GET", "/api/v2/workers/", Some(&tok), None))["results"][0]["online"]
            .as_bool()
            .unwrap()
    );
    // relaunch keeps using the worker
    let (c, b) = curl(
        &s,
        "POST",
        &format!("/api/v2/jobs/{id}/relaunch/"),
        Some(&tok),
        None,
    );
    assert_eq!(c, 201);
    assert_eq!(
        wait_job(
            &s,
            &tok,
            serde_json::from_str::<Value>(&b).unwrap()["id"]
                .as_i64()
                .unwrap()
        )["status"],
        "successful"
    );

    // cancel a running remote job: the worker kills the runner promptly
    let sid = j(curl(
        &s,
        "POST",
        &format!("/api/v2/job_templates/{slow}/launch/"),
        Some(&tok),
        None,
    ))["id"]
        .as_i64()
        .unwrap();
    let t = Instant::now();
    loop {
        let out = curl(
            &s,
            "GET",
            &format!("/api/v2/jobs/{sid}/stdout/"),
            Some(&tok),
            None,
        )
        .1;
        if out.contains("TASK [command]") {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "slow job never started: {out}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let started = Instant::now();
    assert_eq!(
        curl(
            &s,
            "POST",
            &format!("/api/v2/jobs/{sid}/cancel/"),
            Some(&tok),
            None
        )
        .0,
        200
    );
    assert_eq!(wait_job(&s, &tok, sid)["status"], "canceled");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "cancel must not wait for sleep 30"
    );
}

fn curl_h(s: &Server, path: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    let mut c = Command::new("curl");
    c.args(["-s", "-m", "20", "-w", "\n%{http_code}"]);
    for (k, v) in headers {
        c.args(["-H", &format!("{k}: {v}")]);
    }
    c.arg(format!("{}{}", s.base, path));
    let out = String::from_utf8_lossy(&c.output().unwrap().stdout).to_string();
    let (b, code) = out.rsplit_once('\n').unwrap_or(("", "0"));
    (
        code.trim().parse().unwrap_or(0),
        serde_json::from_str(b).unwrap_or(Value::Null),
    )
}

#[test]
fn reverse_proxy_authentication_with_group_mapping() {
    if !curl_ok() {
        return;
    }
    let s = start_with(
        "proxy",
        &[
            "--proxy-auth-header",
            "X-Remote-User",
            "--proxy-groups-header",
            "X-Remote-Groups",
            "--proxy-admin-group",
            "ransible-admins",
            "--proxy-operator-group",
            "ransible-ops",
        ],
    );
    // no identity, no token: rejected
    assert_eq!(curl_h(&s, "/api/v2/me/", &[]).0, 401);
    // identity header from the trusted (loopback) peer: account created on first sight
    let (c, me) = curl_h(
        &s,
        "/api/v2/me/",
        &[
            ("X-Remote-User", "alice"),
            ("X-Remote-Groups", "staff, ransible-admins"),
        ],
    );
    assert_eq!(
        (c, me["username"].as_str(), me["role"].as_str()),
        (200, Some("alice"), Some("admin"))
    );
    assert_eq!(
        curl_h(
            &s,
            "/api/v2/me/",
            &[
                ("X-Remote-User", "olga"),
                ("X-Remote-Groups", "ransible-ops")
            ]
        )
        .1["role"],
        "operator"
    );
    assert_eq!(
        curl_h(
            &s,
            "/api/v2/me/",
            &[("X-Remote-User", "bob"), ("X-Remote-Groups", "staff")]
        )
        .1["role"],
        "viewer"
    );
    // groups drive the role on every request (demotion works)
    assert_eq!(
        curl_h(
            &s,
            "/api/v2/me/",
            &[("X-Remote-User", "alice"), ("X-Remote-Groups", "staff")]
        )
        .1["role"],
        "viewer"
    );
    // proxy-created accounts have no usable password
    assert_eq!(login(&s, "alice", "").0, 401);
    assert_eq!(login(&s, "bob", "proxy-managed").0, 401);
    // control characters in the identity are rejected
    assert_eq!(curl_h(&s, "/api/v2/me/", &[("X-Remote-User", "")]).0, 401);
    // local password login keeps working next to proxy auth
    assert_eq!(login(&s, "admin", "admin-password-1").0, 200);
}

#[test]
fn identity_header_is_ignored_from_untrusted_peers() {
    if !curl_ok() {
        return;
    }
    // loopback is NOT in the trusted list, so a spoofed header must not authenticate
    let s = start_with(
        "proxy-untrusted",
        &[
            "--proxy-auth-header",
            "X-Remote-User",
            "--proxy-trusted-ip",
            "10.9.8.7",
            "--proxy-admin-group",
            "x",
        ],
    );
    let (c, _) = curl_h(&s, "/api/v2/me/", &[("X-Remote-User", "admin")]);
    assert_eq!(c, 401);
    assert_eq!(
        curl_h(
            &s,
            "/api/v2/projects/",
            &[("X-Remote-User", "mallory"), ("X-Remote-Groups", "x")]
        )
        .0,
        401
    );
}
