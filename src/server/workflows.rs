//! Workflow engine: runs a DAG of job templates (and approval gates) with success/failure/always edges.
use super::{AppState, create_job, db};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Notify;

#[derive(Clone, Debug)]
pub struct Node {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub template_id: Option<i64>,
    pub extra_vars: String,
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String,
}

/// Validate a graph: unique keys, known edge endpoints and kinds, job nodes with a template, no cycles.
pub fn validate(nodes: &[Node], edges: &[Edge]) -> Result<(), String> {
    let mut keys = HashSet::new();
    for n in nodes {
        if n.key.is_empty() || !keys.insert(n.key.clone()) {
            return Err(format!("duplicate or empty node key '{}'", n.key));
        }
        if !["job", "approval"].contains(&n.kind.as_str()) {
            return Err(format!("node '{}': kind must be job or approval", n.key));
        }
        if n.kind == "job" && n.template_id.is_none() {
            return Err(format!("node '{}': job nodes need a template", n.key));
        }
    }
    for e in edges {
        if !keys.contains(&e.from) || !keys.contains(&e.to) {
            return Err(format!(
                "edge {} -> {} references an unknown node",
                e.from, e.to
            ));
        }
        if !["success", "failure", "always"].contains(&e.kind.as_str()) {
            return Err(format!(
                "edge {} -> {}: kind must be success, failure or always",
                e.from, e.to
            ));
        }
    }
    // Kahn's algorithm
    let mut indeg: HashMap<&str, usize> = nodes.iter().map(|n| (n.key.as_str(), 0)).collect();
    for e in edges {
        *indeg.get_mut(e.to.as_str()).unwrap() += 1;
    }
    let mut stack: Vec<&str> = indeg
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(k, _)| *k)
        .collect();
    let mut seen = 0;
    while let Some(k) = stack.pop() {
        seen += 1;
        for e in edges.iter().filter(|e| e.from == k) {
            let d = indeg.get_mut(e.to.as_str()).unwrap();
            *d -= 1;
            if *d == 0 {
                stack.push(e.to.as_str());
            }
        }
    }
    if seen != nodes.len() {
        return Err("workflow graph contains a cycle".into());
    }
    Ok(())
}

fn terminal(s: &str) -> bool {
    matches!(
        s,
        "successful" | "failed" | "skipped" | "canceled" | "denied"
    )
}

/// Does the parent's final state satisfy an edge of this kind?
pub fn edge_satisfied(kind: &str, parent: &str) -> bool {
    match kind {
        "success" => parent == "successful",
        "failure" => parent == "failed" || parent == "denied",
        _ => matches!(parent, "successful" | "failed" | "canceled" | "denied"),
    }
}

/// Overall status from final node states (a failure handled by a failure/always edge does not fail the workflow).
pub fn final_status(
    nodes: &[Node],
    edges: &[Edge],
    state: &HashMap<String, String>,
    canceled: bool,
) -> &'static str {
    if canceled {
        return "canceled";
    }
    let handled = |k: &str| {
        edges
            .iter()
            .any(|e| e.from == k && (e.kind == "failure" || e.kind == "always"))
    };
    let bad = nodes.iter().any(|n| {
        matches!(
            state.get(&n.key).map(|s| s.as_str()),
            Some("failed") | Some("denied")
        ) && !handled(&n.key)
    });
    if bad { "failed" } else { "successful" }
}

pub fn load_graph(st: &AppState, workflow_id: i64) -> (Vec<Node>, Vec<Edge>) {
    let c = st.db.lock().unwrap();
    let nodes = db::query(
        &c,
        "SELECT * FROM workflow_nodes WHERE workflow_id = ?1 ORDER BY id",
        &[rusqlite::types::Value::Integer(workflow_id)],
    )
    .unwrap_or_default()
    .into_iter()
    .map(|r| Node {
        key: r["key"].as_str().unwrap_or("").into(),
        label: r["label"].as_str().unwrap_or("").into(),
        kind: r["kind"].as_str().unwrap_or("job").into(),
        template_id: r["template_id"].as_i64(),
        extra_vars: r["extra_vars"].as_str().unwrap_or("").into(),
    })
    .collect();
    let edges = db::query(
        &c,
        "SELECT * FROM workflow_edges WHERE workflow_id = ?1 ORDER BY id",
        &[rusqlite::types::Value::Integer(workflow_id)],
    )
    .unwrap_or_default()
    .into_iter()
    .map(|r| Edge {
        from: r["from_key"].as_str().unwrap_or("").into(),
        to: r["to_key"].as_str().unwrap_or("").into(),
        kind: r["kind"].as_str().unwrap_or("success").into(),
    })
    .collect();
    (nodes, edges)
}

pub fn graph_json(nodes: &[Node], edges: &[Edge]) -> Value {
    json!({
        "nodes": nodes.iter().map(|n| json!({"key": n.key, "label": n.label, "kind": n.kind, "template_id": n.template_id, "extra_vars": n.extra_vars})).collect::<Vec<_>>(),
        "edges": edges.iter().map(|e| json!({"from": e.from, "to": e.to, "kind": e.kind})).collect::<Vec<_>>(),
    })
}

fn save_state(
    st: &AppState,
    wj: i64,
    state: &HashMap<String, (String, Option<i64>)>,
    status: &str,
    finished: bool,
) {
    let obj: Map<String, Value> = state
        .iter()
        .map(|(k, (s, j))| (k.clone(), json!({"status": s, "job_id": j})))
        .collect();
    if let Ok(c) = st.db.lock() {
        let _ = c.execute(
            &format!(
                "UPDATE workflow_jobs SET state = ?1, status = ?2{} WHERE id = ?3",
                if finished {
                    ", finished = datetime('now')"
                } else {
                    ""
                }
            ),
            rusqlite::params![Value::Object(obj).to_string(), status, wj],
        );
    }
}

pub async fn run_workflow(st: AppState, wj: i64, nodes: Vec<Node>, edges: Vec<Edge>, name: String) {
    let cancel = Arc::new(Notify::new());
    st.wf.lock().unwrap().insert(wj, cancel.clone());
    if let Ok(c) = st.db.lock() {
        let _ = c.execute(
            "UPDATE workflow_jobs SET status = 'running', started = datetime('now') WHERE id = ?1",
            [wj],
        );
    }
    let mut state: HashMap<String, (String, Option<i64>)> = nodes
        .iter()
        .map(|n| (n.key.clone(), ("pending".to_string(), None)))
        .collect();
    let mut canceled = false;
    loop {
        // 1. resolve pending nodes whose parents are all final
        let snapshot: HashMap<String, String> = state
            .iter()
            .map(|(k, (s, _))| (k.clone(), s.clone()))
            .collect();
        for n in &nodes {
            if snapshot[&n.key] != "pending" || canceled {
                continue;
            }
            let incoming: Vec<&Edge> = edges.iter().filter(|e| e.to == n.key).collect();
            let ready = if incoming.is_empty() {
                Some(true)
            } else if incoming.iter().all(|e| terminal(&snapshot[&e.from])) {
                Some(
                    incoming
                        .iter()
                        .any(|e| edge_satisfied(&e.kind, &snapshot[&e.from])),
                )
            } else {
                None
            };
            match ready {
                Some(true) => {
                    if n.kind == "approval" {
                        state.insert(n.key.clone(), ("waiting".into(), None));
                    } else {
                        let template = {
                            let c = st.db.lock().unwrap();
                            n.template_id
                                .and_then(|id| db::get(&c, "job_templates", id).ok().flatten())
                        };
                        match template {
                            Some(t) => {
                                let mut ov = Map::new();
                                if !n.extra_vars.trim().is_empty() {
                                    ov.insert("extra_vars".into(), n.extra_vars.clone().into());
                                }
                                match create_job(&st, &t, &ov, &format!("workflow:{name}")) {
                                    Ok(job_id) => {
                                        if let Ok(c) = st.db.lock() {
                                            let _ = c.execute("UPDATE jobs SET workflow_job_id = ?1 WHERE id = ?2", [wj, job_id]);
                                        }
                                        state.insert(
                                            n.key.clone(),
                                            ("running".into(), Some(job_id)),
                                        );
                                    }
                                    Err(_) => {
                                        state.insert(n.key.clone(), ("failed".into(), None));
                                    }
                                }
                            }
                            None => {
                                state.insert(n.key.clone(), ("failed".into(), None));
                            }
                        }
                    }
                }
                Some(false) => {
                    state.insert(n.key.clone(), ("skipped".into(), None));
                }
                None => {}
            }
        }
        // 2. poll running jobs and approvals
        for n in &nodes {
            let (s, job) = state[&n.key].clone();
            if s == "running" {
                if let Some(id) = job {
                    let status = {
                        let c = st.db.lock().unwrap();
                        db::get(&c, "jobs", id)
                            .ok()
                            .flatten()
                            .and_then(|j| j["status"].as_str().map(String::from))
                            .unwrap_or_default()
                    };
                    let mapped = match status.as_str() {
                        "successful" => Some("successful"),
                        "failed" | "error" => Some("failed"),
                        "canceled" => Some("canceled"),
                        _ => None,
                    };
                    if let Some(m) = mapped {
                        state.insert(n.key.clone(), (m.into(), job));
                    }
                }
            } else if s == "waiting" {
                let decision = {
                    let c = st.db.lock().unwrap();
                    db::query(&c, "SELECT decision FROM workflow_approvals WHERE workflow_job_id = ?1 AND key = ?2 ORDER BY id DESC LIMIT 1", &[rusqlite::types::Value::Integer(wj), rusqlite::types::Value::Text(n.key.clone())])
                        .ok()
                        .and_then(|r| r.into_iter().next())
                        .and_then(|r| r["decision"].as_str().map(String::from))
                };
                match decision.as_deref() {
                    Some("approve") => {
                        state.insert(n.key.clone(), ("successful".into(), None));
                    }
                    Some("deny") => {
                        state.insert(n.key.clone(), ("denied".into(), None));
                    }
                    _ => {}
                }
            }
        }
        save_state(&st, wj, &state, "running", false);
        // 3. done?
        let active = state
            .values()
            .any(|(s, _)| matches!(s.as_str(), "running" | "waiting"));
        let pending = state.values().any(|(s, _)| s == "pending");
        if canceled || (!active && !pending) {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(400)) => {}
            _ = cancel.notified() => {
                canceled = true;
                for (s, job) in state.values_mut() {
                    match (s.as_str(), job.as_ref()) {
                        ("running", Some(id)) => {
                            if let Some(l) = st.live.lock().unwrap().get(id) {
                                l.cancel.notify_one();
                            }
                        }
                        ("pending" | "waiting", _) => *s = "canceled".into(),
                        _ => {}
                    }
                }
            }
        }
    }
    // let canceled child jobs settle
    if canceled {
        for _ in 0..50 {
            let mut all_done = true;
            for (s, job) in state.iter_mut().map(|(_, v)| v) {
                if s == "running" {
                    let status = job.and_then(|id| {
                        let c = st.db.lock().unwrap();
                        db::get(&c, "jobs", id)
                            .ok()
                            .flatten()
                            .and_then(|j| j["status"].as_str().map(String::from))
                    });
                    if matches!(status.as_deref(), Some("pending") | Some("running")) {
                        all_done = false;
                    } else {
                        *s = "canceled".into();
                    }
                }
            }
            if all_done {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    let final_map: HashMap<String, String> = state
        .iter()
        .map(|(k, (s, _))| (k.clone(), s.clone()))
        .collect();
    let status = final_status(&nodes, &edges, &final_map, canceled);
    save_state(&st, wj, &state, status, true);
    st.wf.lock().unwrap().remove(&wj);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(k: &str, kind: &str, t: Option<i64>) -> Node {
        Node {
            key: k.into(),
            label: k.into(),
            kind: kind.into(),
            template_id: t,
            extra_vars: String::new(),
        }
    }
    fn e(a: &str, b: &str, k: &str) -> Edge {
        Edge {
            from: a.into(),
            to: b.into(),
            kind: k.into(),
        }
    }

    #[test]
    fn validation() {
        let nodes = vec![n("a", "job", Some(1)), n("b", "job", Some(2))];
        assert!(validate(&nodes, &[e("a", "b", "success")]).is_ok());
        assert!(
            validate(&nodes, &[e("a", "b", "success"), e("b", "a", "failure")])
                .unwrap_err()
                .contains("cycle")
        );
        assert!(validate(&nodes, &[e("a", "zzz", "success")]).is_err());
        assert!(validate(&nodes, &[e("a", "b", "sometimes")]).is_err());
        assert!(validate(&[n("a", "job", None)], &[]).is_err());
        assert!(validate(&[n("a", "job", Some(1)), n("a", "job", Some(1))], &[]).is_err());
        assert!(validate(&[n("g", "approval", None)], &[]).is_ok());
    }

    #[test]
    fn edge_semantics() {
        assert!(edge_satisfied("success", "successful") && !edge_satisfied("success", "failed"));
        assert!(edge_satisfied("failure", "failed") && !edge_satisfied("failure", "successful"));
        assert!(
            edge_satisfied("always", "failed")
                && edge_satisfied("always", "successful")
                && !edge_satisfied("always", "skipped")
        );
    }

    #[test]
    fn final_status_rules() {
        let nodes = vec![n("a", "job", Some(1)), n("b", "job", Some(1))];
        let st = |a: &str, b: &str| -> HashMap<String, String> {
            [
                ("a".to_string(), a.to_string()),
                ("b".to_string(), b.to_string()),
            ]
            .into()
        };
        assert_eq!(
            final_status(&nodes, &[], &st("successful", "successful"), false),
            "successful"
        );
        assert_eq!(
            final_status(
                &nodes,
                &[e("a", "b", "success")],
                &st("failed", "skipped"),
                false
            ),
            "failed"
        );
        // failure handled by a failure edge: workflow still succeeds
        assert_eq!(
            final_status(
                &nodes,
                &[e("a", "b", "failure")],
                &st("failed", "successful"),
                false
            ),
            "successful"
        );
        assert_eq!(
            final_status(&nodes, &[], &st("successful", "canceled"), true),
            "canceled"
        );
    }
}
