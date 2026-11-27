//! ansible.cfg + ANSIBLE_* environment configuration.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Default, Debug, Clone)]
pub struct Config {
    pub inventory: Option<String>,
    pub roles_path: Option<String>,
    pub remote_user: Option<String>,
    pub forks: Option<usize>,
    pub host_key_checking: Option<bool>,
    pub private_key_file: Option<String>,
    pub vault_password_file: Option<String>,
    pub become_: Option<bool>,
    pub become_user: Option<String>,
    pub become_method: Option<String>,
    pub ssh_args: Option<String>,
    pub timeout: Option<u64>,
    pub path: Option<PathBuf>,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Initialise the global config, letting the CLI override file/env values.
pub fn init(f: impl FnOnce(&mut Config)) {
    let mut c = load();
    f(&mut c);
    let _ = CONFIG.set(c);
}

pub fn get() -> &'static Config {
    CONFIG.get_or_init(load)
}

fn truthy(s: &str) -> bool {
    matches!(
        s.trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "y" | "t"
    )
}

fn parse_ini(text: &str) -> HashMap<(String, String), String> {
    let mut out = HashMap::new();
    let mut sec = String::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with(';') {
            continue;
        }
        if l.starts_with('[') && l.ends_with(']') {
            sec = l[1..l.len() - 1].to_string();
            continue;
        }
        if let Some((k, v)) = l.split_once('=') {
            let v = v
                .split(" #")
                .next()
                .unwrap_or(v)
                .split(" ;")
                .next()
                .unwrap_or(v);
            out.insert((sec.clone(), k.trim().to_string()), v.trim().to_string());
        }
    }
    out
}

fn expand(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/")
        && let Ok(h) = std::env::var("HOME")
    {
        return format!("{h}/{rest}");
    }
    p.to_string()
}

pub fn load() -> Config {
    let mut cands: Vec<PathBuf> = vec![];
    if let Ok(p) = std::env::var("ANSIBLE_CONFIG") {
        cands.push(PathBuf::from(p));
    }
    cands.push(PathBuf::from("ansible.cfg"));
    if let Ok(h) = std::env::var("HOME") {
        cands.push(PathBuf::from(h).join(".ansible.cfg"));
    }
    cands.push(PathBuf::from("/etc/ansible/ansible.cfg"));
    let mut c = Config::default();
    let mut ini = HashMap::new();
    for p in cands {
        if p.is_file()
            && let Ok(t) = std::fs::read_to_string(&p)
        {
            ini = parse_ini(&t);
            c.path = Some(p);
            break;
        }
    }
    let get = |sec: &str, k: &str| ini.get(&(sec.to_string(), k.to_string())).cloned();
    c.inventory = get("defaults", "inventory").map(|s| expand(&s));
    c.roles_path = get("defaults", "roles_path")
        .map(|s| s.split(':').map(expand).collect::<Vec<_>>().join(":"));
    c.remote_user = get("defaults", "remote_user");
    c.forks = get("defaults", "forks").and_then(|s| s.parse().ok());
    c.host_key_checking = get("defaults", "host_key_checking").map(|s| truthy(&s));
    c.private_key_file = get("defaults", "private_key_file").map(|s| expand(&s));
    c.vault_password_file = get("defaults", "vault_password_file").map(|s| expand(&s));
    c.timeout = get("defaults", "timeout").and_then(|s| s.parse().ok());
    c.become_ = get("privilege_escalation", "become").map(|s| truthy(&s));
    c.become_user = get("privilege_escalation", "become_user");
    c.become_method = get("privilege_escalation", "become_method");
    c.ssh_args = get("ssh_connection", "ssh_args");
    // environment overrides
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    if let Some(v) = env("ANSIBLE_INVENTORY") {
        c.inventory = Some(v);
    }
    if let Some(v) = env("ANSIBLE_ROLES_PATH") {
        c.roles_path = Some(v);
    }
    if let Some(v) = env("ANSIBLE_REMOTE_USER") {
        c.remote_user = Some(v);
    }
    if let Some(v) = env("ANSIBLE_FORKS").and_then(|v| v.parse().ok()) {
        c.forks = Some(v);
    }
    if let Some(v) = env("ANSIBLE_HOST_KEY_CHECKING") {
        c.host_key_checking = Some(truthy(&v));
    }
    if let Some(v) = env("ANSIBLE_PRIVATE_KEY_FILE") {
        c.private_key_file = Some(v);
    }
    if let Some(v) = env("ANSIBLE_VAULT_PASSWORD_FILE") {
        c.vault_password_file = Some(v);
    }
    if let Some(v) = env("ANSIBLE_BECOME") {
        c.become_ = Some(truthy(&v));
    }
    if let Some(v) = env("ANSIBLE_BECOME_USER") {
        c.become_user = Some(v);
    }
    if let Some(v) = env("ANSIBLE_BECOME_METHOD") {
        c.become_method = Some(v);
    }
    if let Some(v) = env("ANSIBLE_SSH_ARGS") {
        c.ssh_args = Some(v);
    }
    if let Some(v) = env("ANSIBLE_TIMEOUT").and_then(|v| v.parse().ok()) {
        c.timeout = Some(v);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ini_parsing() {
        let m = parse_ini(
            "[defaults]\nforks = 20 # fast\ninventory=~/inv\n[ssh_connection]\nssh_args = -o A=b\n",
        );
        assert_eq!(m[&("defaults".into(), "forks".into())], "20");
        assert_eq!(m[&("ssh_connection".into(), "ssh_args".into())], "-o A=b");
    }
}
