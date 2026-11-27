//! `rancible galaxy install`: install roles from requirements.yml (git sources or galaxy.ansible.com).
use crate::playbook::read_yaml;
use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, PartialEq)]
pub struct RoleReq {
    pub name: String,
    pub src: String,
    pub version: Option<String>,
}

pub fn default_roles_path() -> PathBuf {
    if let Some(rp) = &crate::config::get().roles_path
        && let Some(first) = rp.split(':').next()
    {
        return PathBuf::from(first);
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".ansible/roles")
}

/// Parse a requirements file (list form, or `{roles: [...], collections: [...]}`).
pub fn parse_requirements(v: &Value) -> (Vec<RoleReq>, Vec<String>) {
    let (roles_v, colls_v) = match v {
        Value::Array(a) => (a.clone(), vec![]),
        Value::Object(o) => (
            o.get("roles")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default(),
            o.get("collections")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default(),
        ),
        _ => (vec![], vec![]),
    };
    let mut roles = vec![];
    for r in roles_v {
        match r {
            Value::String(s) => roles.push(RoleReq {
                name: derive_name(&s),
                src: s,
                version: None,
            }),
            Value::Object(o) => {
                let src = o
                    .get("src")
                    .or(o.get("name"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if src.is_empty() {
                    continue;
                }
                let name = o
                    .get("name")
                    .and_then(|x| x.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| derive_name(&src));
                let version = o.get("version").and_then(|x| x.as_str()).map(String::from);
                roles.push(RoleReq { name, src, version });
            }
            _ => {}
        }
    }
    let colls = colls_v
        .iter()
        .filter_map(|c| match c {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("name").and_then(|n| n.as_str()).map(String::from),
            _ => None,
        })
        .collect();
    (roles, colls)
}

fn derive_name(src: &str) -> String {
    if is_git_src(src) {
        let s = src.trim_end_matches('/').trim_end_matches(".git");
        let last = s.rsplit(['/', ':']).next().unwrap_or(s);
        return last
            .strip_prefix("ansible-role-")
            .unwrap_or(last)
            .to_string();
    }
    src.to_string() // namespace.name
}

fn is_git_src(src: &str) -> bool {
    src.contains("://") || src.starts_with("git@") || src.starts_with("git+")
}

fn run(cmd: &mut Command) -> Result<String> {
    let o = cmd
        .output()
        .map_err(|e| anyhow!("failed to run {:?}: {e}", cmd.get_program()))?;
    if !o.status.success() {
        bail!(
            "{:?} failed: {}",
            cmd.get_program(),
            String::from_utf8_lossy(&o.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Resolve `namespace.name` to (git url, latest version) through the Galaxy API.
fn galaxy_lookup(name: &str) -> Result<(String, Option<String>)> {
    let (ns, role) = name
        .split_once('.')
        .ok_or_else(|| anyhow!("'{name}' is neither a git URL nor namespace.name"))?;
    let url = format!("https://galaxy.ansible.com/api/v1/roles/?owner__username={ns}&name={role}");
    let body = run(Command::new("curl").args(["-fsSL", "--max-time", "30", &url]))?;
    let v: Value = serde_json::from_str(&body)?;
    let r = v
        .get("results")
        .and_then(|r| r.get(0))
        .ok_or_else(|| anyhow!("role {name} not found on galaxy.ansible.com"))?;
    let (user, repo) = (
        r.get("github_user")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("no github_user for {name}"))?,
        r.get("github_repo")
            .and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("no github_repo for {name}"))?,
    );
    let ver = r
        .get("summary_fields")
        .and_then(|s| s.get("versions"))
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.get("name"))
        .and_then(|n| n.as_str())
        .map(String::from);
    Ok((format!("https://github.com/{user}/{repo}.git"), ver))
}

pub fn install_role(req: &RoleReq, dest_root: &Path, force: bool) -> Result<PathBuf> {
    let dest = dest_root.join(&req.name);
    if dest.exists() {
        if !force {
            println!(
                "- {} is already installed, skipping (use --force to reinstall).",
                req.name
            );
            return Ok(dest);
        }
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::create_dir_all(dest_root)?;
    let (url, ver) = if is_git_src(&req.src) {
        (
            req.src.trim_start_matches("git+").to_string(),
            req.version.clone(),
        )
    } else {
        let (u, latest) = galaxy_lookup(&req.src)?;
        (u, req.version.clone().or(latest))
    };
    let mut c = Command::new("git");
    c.args(["clone", "-q", "--depth", "1"]);
    if let Some(v) = &ver {
        c.args(["--branch", v]);
    }
    c.arg(&url).arg(&dest);
    run(&mut c)?;
    let _ = std::fs::remove_dir_all(dest.join(".git"));
    println!(
        "- {} was installed successfully ({}{})",
        req.name,
        url,
        ver.map(|v| format!(" @ {v}")).unwrap_or_default()
    );
    Ok(dest)
}

pub fn install_file(file: &Path, dest_root: &Path, force: bool) -> Result<()> {
    let (roles, colls) = parse_requirements(&read_yaml(file)?);
    for r in &roles {
        install_role(r, dest_root, force)?;
    }
    for c in colls {
        println!(
            "- collection {c}: skipped (rancible implements the common collection modules natively; Python collection plugins are not executed)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_list_and_dict_forms() {
        let v = json!(["geerlingguy.nginx", {"src": "https://github.com/u/ansible-role-foo.git", "version": "v1"}, {"src": "git+https://h/x/bar.git", "name": "custom"}]);
        let (roles, colls) = parse_requirements(&v);
        assert!(colls.is_empty());
        assert_eq!(
            roles[0],
            RoleReq {
                name: "geerlingguy.nginx".into(),
                src: "geerlingguy.nginx".into(),
                version: None
            }
        );
        assert_eq!(roles[1].name, "foo");
        assert_eq!(roles[1].version.as_deref(), Some("v1"));
        assert_eq!(roles[2].name, "custom");
        assert_eq!(derive_name("geerlingguy.git"), "geerlingguy.git");
        let (r2, c2) = parse_requirements(
            &json!({"roles": ["a.b"], "collections": [{"name": "community.general"}, "ansible.posix"]}),
        );
        assert_eq!(r2.len(), 1);
        assert_eq!(c2, vec!["community.general", "ansible.posix"]);
    }
}
