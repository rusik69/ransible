mod config;
mod conn;
mod events;
mod executor;
mod galaxy;
mod inventory;
mod lookup;
mod modules;
mod modules_extra;
mod mysql;
mod playbook;
mod pyfmt;
#[cfg(feature = "server")]
mod server;
mod template;
mod vault;
mod worker;

use anyhow::{Result, anyhow};
use clap::{Args, Parser, Subcommand};
use serde_json::{Map, Value};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "ransible",
    version,
    about = "Ansible-compatible automation runner written in Rust"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Clone)]
struct Common {
    /// Inventory file, directory or comma separated host list
    #[arg(short = 'i', long = "inventory")]
    inventory: Option<String>,
    /// Further limit selected hosts to a pattern
    #[arg(short = 'l', long = "limit")]
    limit: Option<String>,
    /// Extra variables: key=value, JSON or @file
    #[arg(short = 'e', long = "extra-vars")]
    extra_vars: Vec<String>,
    /// Only run plays and tasks tagged with these values
    #[arg(short = 't', long = "tags")]
    tags: Vec<String>,
    #[arg(long = "skip-tags")]
    skip_tags: Vec<String>,
    /// Run without making changes
    #[arg(short = 'C', long = "check")]
    check: bool,
    #[arg(short = 'D', long = "diff")]
    diff: bool,
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,
    #[arg(short = 'f', long = "forks")]
    forks: Option<usize>,
    /// Output format: `default` (Ansible-style text) or `json` (one event per line)
    #[arg(short = 'o', long = "output", default_value = "default")]
    output: String,
    /// Prompt for the SSH connection password
    #[arg(short = 'k', long = "ask-pass")]
    ask_pass: bool,
    /// Prompt for the privilege escalation password
    #[arg(short = 'K', long = "ask-become-pass")]
    ask_become_pass: bool,
    #[arg(long = "become-method")]
    become_method: Option<String>,
    /// Connection timeout in seconds
    #[arg(short = 'T', long = "timeout")]
    timeout: Option<u64>,
    #[arg(long = "ssh-common-args")]
    ssh_common_args: Option<String>,
    #[arg(long = "ssh-extra-args")]
    ssh_extra_args: Option<String>,
    /// Accepted for compatibility (no effect)
    #[arg(long = "force-handlers")]
    force_handlers: bool,
    /// Accepted for compatibility (no effect)
    #[arg(short = 'M', long = "module-path")]
    module_path: Option<String>,
    /// Accepted for compatibility (no effect)
    #[arg(long = "flush-cache")]
    flush_cache: bool,
    /// Run connections as this user
    #[arg(short = 'u', long = "user")]
    user: Option<String>,
    /// Use this SSH private key
    #[arg(long = "private-key", visible_alias = "key-file")]
    private_key: Option<String>,
    /// Escalate privileges (sudo)
    #[arg(short = 'b', long = "become")]
    become_: bool,
    #[arg(long = "become-user")]
    become_user: Option<String>,
    #[arg(short = 'c', long = "connection")]
    connection: Option<String>,
    /// Vault password file (or executable script printing the password)
    #[arg(long = "vault-password-file", visible_alias = "vault-pass-file")]
    vault_password_file: Option<String>,
    /// Prompt for the vault password
    #[arg(long = "ask-vault-pass", visible_alias = "ask-vault-password")]
    ask_vault_pass: bool,
    /// Vault identity: `label@file`, `label@prompt`, `file` or `prompt` (repeatable)
    #[arg(long = "vault-id")]
    vault_id: Vec<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a playbook
    Playbook {
        playbook: Vec<PathBuf>,
        #[command(flatten)]
        common: Common,
        #[arg(long = "list-tasks")]
        list_tasks: bool,
        #[arg(long = "list-hosts")]
        list_hosts: bool,
        #[arg(long = "syntax-check")]
        syntax_check: bool,
        #[arg(long = "list-tags")]
        list_tags: bool,
        /// Start the playbook at the task matching this name
        #[arg(long = "start-at-task")]
        start_at_task: Option<String>,
        /// Accepted for compatibility (no effect)
        #[arg(long = "step")]
        step: bool,
    },
    /// Run a single module on hosts
    Adhoc {
        pattern: String,
        #[arg(short = 'm', long = "module-name", default_value = "command")]
        module: String,
        #[arg(short = 'a', long = "args", default_value = "")]
        args: String,
        #[command(flatten)]
        common: Common,
    },
    /// Encrypt/decrypt/view Ansible Vault data
    Vault {
        /// encrypt | decrypt | view | encrypt_string | rekey
        action: String,
        /// Files (or the string for encrypt_string)
        args: Vec<String>,
        #[arg(long = "vault-password-file", visible_alias = "vault-pass-file")]
        vault_password_file: Option<String>,
        #[arg(long = "new-vault-password-file")]
        new_vault_password_file: Option<String>,
        #[arg(long = "ask-vault-pass")]
        ask_vault_pass: bool,
        /// Vault identity: `label@file`, `label@prompt`, `file` or `prompt` (repeatable)
        #[arg(long = "vault-id")]
        vault_id: Vec<String>,
        /// Variable name for encrypt_string
        #[arg(short = 'n', long = "name")]
        name: Option<String>,
        /// Vault id label
        #[arg(long = "encrypt-vault-id")]
        label: Option<String>,
    },
    /// Install roles from a requirements file (`ransible galaxy install -r requirements.yml`)
    Galaxy {
        /// install
        action: String,
        /// Role names / git URLs to install
        names: Vec<String>,
        #[arg(short = 'r', long = "role-file")]
        role_file: Option<PathBuf>,
        /// Directory to install roles into (default: roles_path or ~/.ansible/roles)
        #[arg(short = 'p', long = "roles-path")]
        roles_path: Option<PathBuf>,
        #[arg(short = 'f', long = "force")]
        force: bool,
    },
    /// Run the web UI and REST API (AWX-style) in this process
    #[cfg(feature = "server")]
    Serve {
        /// Address to listen on
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: String,
        /// Directory for the database, projects and the master key (default: ~/.ransible)
        #[arg(long, env = "RANSIBLE_DATA_DIR")]
        data_dir: Option<PathBuf>,
        /// File with the initial admin password (first start only; otherwise one is generated)
        #[arg(long)]
        admin_password_file: Option<PathBuf>,
        /// Maximum number of jobs running at once
        #[arg(long, default_value_t = 4)]
        max_concurrent_jobs: usize,
        /// TLS certificate chain (PEM); enables HTTPS together with --tls-key
        #[arg(long, requires = "tls_key")]
        tls_cert: Option<PathBuf>,
        /// TLS private key (PEM)
        #[arg(long, requires = "tls_cert")]
        tls_key: Option<PathBuf>,
        /// Trust an authenticating reverse proxy: header carrying the user name (e.g. X-Forwarded-User)
        #[arg(long)]
        proxy_auth_header: Option<String>,
        /// Header carrying the user's groups, comma separated (e.g. X-Forwarded-Groups)
        #[arg(long, requires = "proxy_auth_header")]
        proxy_groups_header: Option<String>,
        /// Members of this group become admins
        #[arg(long)]
        proxy_admin_group: Option<String>,
        /// Members of this group become operators (others are viewers)
        #[arg(long)]
        proxy_operator_group: Option<String>,
        /// Peer IP addresses allowed to set the identity header (repeatable; default: loopback)
        #[arg(long = "proxy-trusted-ip")]
        proxy_trusted_ip: Vec<std::net::IpAddr>,
    },
    /// Run jobs for a ransible server (remote execution node)
    Worker {
        /// Server URL, e.g. https://ransible.example.com
        #[arg(long, env = "RANSIBLE_SERVER")]
        server: String,
        /// Worker token (create one under Workers in the UI or POST /api/v2/workers/)
        #[arg(long, env = "RANSIBLE_WORKER_TOKEN", hide_env_values = true)]
        token: String,
        /// CA certificate (PEM) to trust for the server
        #[arg(long)]
        cacert: Option<String>,
        /// Do not verify the server certificate (insecure)
        #[arg(long)]
        insecure_skip_verify: bool,
        /// Allow plain http:// to a non-loopback server (credentials travel unencrypted!)
        #[arg(long)]
        allow_insecure_http: bool,
        /// Exit after running this many jobs
        #[arg(long, hide = true)]
        max_jobs: Option<u32>,
    },
    /// Print shell completions (bash, zsh, fish, elvish, powershell)
    Completions { shell: clap_complete::Shell },
    /// List inventory
    Inventory {
        #[command(flatten)]
        common: Common,
        #[arg(default_value = "all")]
        pattern: String,
    },
}

fn parse_extra(items: &[String]) -> Result<Map<String, Value>> {
    let mut out = Map::new();
    for it in items {
        if let Some(f) = it.strip_prefix('@') {
            let v = playbook::read_yaml(Path::new(f))?;
            if let Value::Object(m) = v {
                out.extend(m);
            }
        } else if it.trim_start().starts_with('{') {
            if let Value::Object(m) = serde_json::from_str::<Value>(it)? {
                out.extend(m);
            }
        } else if let Some(kv) = playbook::parse_kv(it) {
            for (k, v) in kv {
                let v = match v.as_str().map(|s| s.parse::<i64>()) {
                    Some(Ok(i)) => Value::from(i),
                    _ => v,
                };
                out.insert(k, v);
            }
        } else {
            return Err(anyhow!("bad --extra-vars '{it}'"));
        }
    }
    Ok(out)
}

fn contains_hostvars(dir: &Path, depth: usize) -> bool {
    if depth > 4 {
        return false;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        if p.is_dir() {
            if contains_hostvars(&p, depth + 1) {
                return true;
            }
        } else if let Ok(md) = p.metadata()
            && md.len() < 1_000_000
            && std::fs::read_to_string(&p)
                .map(|t| t.contains("hostvars"))
                .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn build_runner(c: &Common, dir: &Path) -> Result<executor::Runner> {
    let cfg = config::get();
    let default = Path::new("/etc/ansible/hosts");
    let mut inv = match c.inventory.as_ref().or(cfg.inventory.as_ref()) {
        Some(i) => inventory::Inventory::load(i)?,
        None if default.exists() => inventory::Inventory::load("/etc/ansible/hosts")?,
        None => inventory::Inventory::empty_localhost(),
    };
    // CLI connection overrides go into "all" group vars (lowest) -> use extra to keep simple
    let abs = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if abs(dir) != abs(&inv.base_dir) {
        inv.extra_var_dirs.push(dir.to_path_buf());
    }
    let mut extra = parse_extra(&c.extra_vars)?;
    {
        let all = inv.groups.entry("all".into()).or_default();
        if let Some(u) = c.user.as_ref().or(cfg.remote_user.as_ref()) {
            all.vars.insert("ansible_user".into(), u.clone().into());
        }
        if let Some(m) = &cfg.become_method {
            all.vars
                .insert("ansible_become_method".into(), m.clone().into());
        }
        if c.private_key.is_none()
            && let Some(k) = &cfg.private_key_file
        {
            all.vars
                .insert("ansible_ssh_private_key_file".into(), k.clone().into());
        }
        if let Some(m) = &c.become_method {
            all.vars
                .insert("ansible_become_method".into(), m.clone().into());
        }
        if c.ask_pass {
            let pw = rpassword::prompt_password("SSH password: ")?;
            all.vars.insert("ansible_password".into(), pw.into());
        }
        if c.ask_become_pass {
            let pw = rpassword::prompt_password("BECOME password: ")?;
            all.vars.insert("ansible_become_password".into(), pw.into());
            all.vars.insert("ansible_become".into(), true.into());
        }
        let mut sargs = vec![];
        for a in [&c.ssh_common_args, &c.ssh_extra_args]
            .into_iter()
            .flatten()
        {
            sargs.push(a.clone());
        }
        if !sargs.is_empty() {
            all.vars
                .insert("ansible_ssh_extra_args".into(), sargs.join(" ").into());
        }
        if !c.become_ && cfg.become_ == Some(true) {
            all.vars.insert("ansible_become".into(), true.into());
        }
        if c.become_user.is_none()
            && let Some(bu) = &cfg.become_user
        {
            all.vars
                .insert("ansible_become_user".into(), bu.clone().into());
        }
        if let Some(k) = &c.private_key {
            all.vars
                .insert("ansible_ssh_private_key_file".into(), k.clone().into());
        }
        if let Some(cn) = &c.connection {
            all.vars
                .insert("ansible_connection".into(), cn.clone().into());
        }
        if c.become_ {
            all.vars.insert("ansible_become".into(), true.into());
        }
        if let Some(bu) = &c.become_user {
            all.vars
                .insert("ansible_become_user".into(), bu.clone().into());
        }
    }
    extra.retain(|_, _| true);
    let split = |v: &Vec<String>| {
        v.iter()
            .flat_map(|s| s.split(',').map(|x| x.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
    };
    let need_hostvars = contains_hostvars(dir, 0);
    Ok(executor::Runner {
        inv: std::sync::RwLock::new(inv),
        opts: executor::Opts {
            check: c.check,
            diff: c.diff,
            verbose: c.verbose,
            tags: split(&c.tags),
            skip_tags: split(&c.skip_tags),
            extra_vars: extra,
            limit: c.limit.clone(),
            forks: c.forks.or(cfg.forks).unwrap_or(5),
            ..Default::default()
        },
        templar: template::Templar::new(),
        sink: if c.output == "json" {
            std::sync::Arc::new(events::JsonSink(std::sync::Mutex::new(std::io::stdout())))
        } else {
            std::sync::Arc::new(events::ConsoleSink {
                color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
                verbose: c.verbose,
            })
        },
        need_hostvars,
        playbook_dir: std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf()),
        all_stats: Default::default(),
        started: std::sync::atomic::AtomicBool::new(false),
    })
}

fn init_vault(file: &Option<String>, ask: bool, ids: &[String]) -> Result<()> {
    let mut pws: Vec<(Option<String>, String)> = vec![];
    for id in ids {
        let (label, src) = match id.split_once('@') {
            Some((l, s)) => (Some(l.to_string()).filter(|l| !l.is_empty()), s),
            None => (None, id.as_str()),
        };
        let pw = if src == "prompt" {
            rpassword::prompt_password(format!(
                "Vault password ({}): ",
                label.as_deref().unwrap_or("default")
            ))?
        } else {
            vault::load_password_file(src)?
        };
        pws.push((label, pw));
    }
    if let Some(f) = file {
        pws.push((None, vault::load_password_file(f)?));
    } else if let Some(f) = &config::get().vault_password_file {
        pws.push((None, vault::load_password_file(f)?));
    }
    if ask {
        pws.push((None, rpassword::prompt_password("Vault password: ")?));
    }
    vault::set_passwords(pws);
    Ok(())
}

fn vault_cmd(
    action: &str,
    args: &[String],
    new_pw_file: &Option<String>,
    name: &Option<String>,
    label: &Option<String>,
) -> Result<i32> {
    let pw = vault::password_for(label.as_deref()).ok_or_else(|| {
        anyhow!(
            "a vault password is required (--vault-password-file, --vault-id or --ask-vault-pass)"
        )
    })?;
    match action {
        "encrypt" => {
            for f in args {
                let data = std::fs::read(f)?;
                if vault::is_vault(&data) {
                    return Err(anyhow!("{f} is already encrypted"));
                }
                std::fs::write(f, vault::encrypt(&data, &pw, label.as_deref())?)?;
                println!("Encryption successful");
            }
        }
        "decrypt" => {
            for f in args {
                let data = std::fs::read_to_string(f)?;
                std::fs::write(f, vault::decrypt(&data, &pw)?)?;
                println!("Decryption successful");
            }
        }
        "view" => {
            for f in args {
                let data = std::fs::read_to_string(f)?;
                print!("{}", String::from_utf8_lossy(&vault::decrypt(&data, &pw)?));
            }
        }
        "rekey" => {
            let npf = new_pw_file
                .as_ref()
                .ok_or_else(|| anyhow!("--new-vault-password-file is required"))?;
            let npw = vault::load_password_file(npf)?;
            for f in args {
                let data = std::fs::read_to_string(f)?;
                let plain = vault::decrypt(&data, &pw)?;
                std::fs::write(f, vault::encrypt(&plain, &npw, label.as_deref())?)?;
                println!("Rekey successful");
            }
        }
        "encrypt_string" | "encrypt-string" => {
            let text = args.join(" ");
            let enc = vault::encrypt(text.as_bytes(), &pw, label.as_deref())?;
            let indented: String = enc.lines().map(|l| format!("    {l}\n")).collect();
            match name {
                Some(n) => print!("{n}: !vault |\n{indented}"),
                None => print!("!vault |\n{indented}"),
            }
        }
        other => return Err(anyhow!("unknown vault action '{other}'")),
    }
    Ok(0)
}

fn run() -> Result<i32> {
    let cli = Cli::parse_from(dispatch_args());
    if let Cmd::Playbook { common, .. }
    | Cmd::Adhoc { common, .. }
    | Cmd::Inventory { common, .. } = &cli.cmd
    {
        config::init(|c| {
            if let Some(t) = common.timeout {
                c.timeout = Some(t);
            }
        });
    }
    match &cli.cmd {
        Cmd::Playbook { common, .. }
        | Cmd::Adhoc { common, .. }
        | Cmd::Inventory { common, .. } => init_vault(
            &common.vault_password_file,
            common.ask_vault_pass,
            &common.vault_id,
        )?,
        Cmd::Vault {
            vault_password_file,
            ask_vault_pass,
            vault_id,
            ..
        } => init_vault(vault_password_file, *ask_vault_pass, vault_id)?,
        _ => {}
    }
    match cli.cmd {
        Cmd::Galaxy {
            action,
            names,
            role_file,
            roles_path,
            force,
        } => {
            // `galaxy install ...` | `galaxy role install ...` | `galaxy collection install ...`
            let (kind, mut names) = match action.as_str() {
                "install" => ("both", names),
                "role" | "collection" => {
                    if names.first().map(|s| s.as_str()) != Some("install") {
                        return Err(anyhow!("usage: galaxy {action} install <names...>"));
                    }
                    (action.as_str(), names[1..].to_vec())
                }
                other => {
                    return Err(anyhow!(
                        "unsupported galaxy action '{other}' (install, role install, collection install)"
                    ));
                }
            };
            let dest = roles_path
                .clone()
                .unwrap_or_else(galaxy::default_roles_path);
            if let Some(f) = role_file {
                galaxy::install_file(&f, &dest, force)?;
            }
            if kind == "collection" {
                let croot = roles_path.unwrap_or_else(galaxy::default_collections_path);
                for n in names.drain(..) {
                    galaxy::install_collection(&n, &croot, force)?;
                }
            }
            for n in names {
                let req = galaxy::parse_requirements(&serde_json::json!([n]))
                    .0
                    .remove(0);
                galaxy::install_role(&req, &dest, force)?;
            }
            Ok(0)
        }
        #[cfg(feature = "server")]
        Cmd::Serve {
            listen,
            data_dir,
            admin_password_file,
            max_concurrent_jobs,
            tls_cert,
            tls_key,
            proxy_auth_header,
            proxy_groups_header,
            proxy_admin_group,
            proxy_operator_group,
            proxy_trusted_ip,
        } => {
            let data_dir = data_dir.unwrap_or_else(|| {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                    .join(".ransible")
            });
            let admin_password = match admin_password_file {
                Some(f) => Some(std::fs::read_to_string(f)?.trim().to_string()),
                None => std::env::var("RANSIBLE_ADMIN_PASSWORD")
                    .ok()
                    .filter(|p| !p.is_empty()),
            };
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(server::serve(server::Options {
                listen,
                data_dir,
                admin_password,
                max_jobs: max_concurrent_jobs,
                tls: tls_cert.zip(tls_key),
                proxy_auth: proxy_auth_header.map(|header| server::ProxyAuth {
                    header,
                    groups_header: proxy_groups_header,
                    admin_group: proxy_admin_group,
                    operator_group: proxy_operator_group,
                    trusted: if proxy_trusted_ip.is_empty() {
                        vec!["127.0.0.1".parse().unwrap(), "::1".parse().unwrap()]
                    } else {
                        proxy_trusted_ip
                    },
                }),
            }))?;
            Ok(0)
        }
        Cmd::Worker {
            server,
            token,
            cacert,
            insecure_skip_verify,
            allow_insecure_http,
            max_jobs,
        } => {
            worker::run(worker::Options {
                server,
                token,
                cacert,
                insecure_skip_verify,
                allow_insecure_http,
                max_jobs,
            })?;
            Ok(0)
        }
        Cmd::Completions { shell } => {
            use clap::CommandFactory;
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "ransible",
                &mut std::io::stdout(),
            );
            Ok(0)
        }
        Cmd::Vault {
            action,
            args,
            new_vault_password_file,
            name,
            label,
            ..
        } => vault_cmd(&action, &args, &new_vault_password_file, &name, &label),
        Cmd::Playbook {
            playbook: pbs,
            common,
            list_tasks,
            list_hosts,
            syntax_check,
            list_tags,
            start_at_task,
            ..
        } => {
            if pbs.is_empty() {
                return Err(anyhow!("no playbook given"));
            }
            let dir = pbs[0]
                .parent()
                .map(|p| {
                    if p.as_os_str().is_empty() {
                        PathBuf::from(".")
                    } else {
                        p.to_path_buf()
                    }
                })
                .unwrap_or_else(|| PathBuf::from("."));
            let mut runner = build_runner(&common, &dir)?;
            runner.opts.list_tasks = list_tasks;
            runner.opts.list_hosts = list_hosts;
            runner.opts.list_tags = list_tags;
            runner.opts.start_at_task = start_at_task;
            let mut plays = vec![];
            for pb in &pbs {
                plays.extend(playbook::load_playbook(pb)?);
            }
            if syntax_check {
                println!("playbook: {}", pbs[0].display());
                return Ok(0);
            }
            if list_tasks || list_hosts || list_tags {
                println!("\nplaybook: {}", pbs[0].display());
            }
            runner.sink.emit(&events::Event::PlaybookStart {
                playbook: pbs[0].display().to_string(),
            });
            let mut ok = true;
            for play in &plays {
                if !runner.run_play(play)? {
                    ok = false;
                    // Ansible stops once all hosts in a play failed; continue for remaining hosts
                }
            }
            if list_tasks || list_hosts || list_tags {
                return Ok(0);
            }
            let code = runner.recap();
            let rc = if code != 0 {
                code
            } else if ok {
                0
            } else {
                2
            };
            runner.sink.emit(&events::Event::PlaybookEnd { rc });
            Ok(rc)
        }
        Cmd::Adhoc {
            pattern,
            module,
            args,
            common,
        } => {
            let mut runner = build_runner(&common, Path::new("."))?;
            runner.need_hostvars = false;
            let m = playbook::norm_module(&module);
            let freeform = ["command", "shell", "raw", "script"].contains(&m.as_str());
            let margs = if args.is_empty() {
                Value::Object(Map::new())
            } else if freeform {
                serde_json::json!({ "_raw_params": args })
            } else if let Some(kv) = playbook::parse_kv(&args) {
                Value::Object(kv)
            } else {
                serde_json::json!({ "_raw_params": args })
            };
            let task = playbook::Task {
                name: Some(m.to_string()),
                module: m,
                args: margs,
                dir: PathBuf::from("."),
                ..Default::default()
            };
            let play = playbook::Play {
                name: pattern.clone(),
                hosts: pattern,
                tasks: vec![task],
                dir: PathBuf::from("."),
                gather_facts: false,
                ..Default::default()
            };
            runner.run_play(&play)?;
            Ok(runner.recap())
        }
        Cmd::Inventory { common, pattern } => {
            let runner = build_runner(&common, Path::new("."))?;
            for h in runner.inv.read().unwrap().select(&pattern) {
                println!("{h}");
            }
            Ok(0)
        }
    }
}

/// Drop-in names: `ansible-playbook`, `ansible`, `ansible-vault`, `ansible-inventory`.
fn dispatch_args() -> Vec<String> {
    let mut args: Vec<String> = std::env::args().collect();
    let exe = args
        .first()
        .and_then(|a| {
            Path::new(a)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
        })
        .unwrap_or_default();
    let sub = match exe.as_str() {
        "ansible-playbook" => Some("playbook"),
        "ansible" => Some("adhoc"),
        "ansible-vault" => Some("vault"),
        "ansible-inventory" => Some("inventory"),
        "ansible-galaxy" => Some("galaxy"),
        _ => None,
    };
    if let Some(s) = sub {
        args.insert(1, s.to_string());
    }
    args
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("ERROR! {e:#}");
            std::process::exit(1);
        }
    }
}
