//! Connections: local shell, ssh (system `ssh` with ControlMaster), docker/podman exec,
//! and an injectable mock for unit tests.
use crate::template::shell_quote;
use anyhow::{Result, anyhow};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

pub type MockFn = dyn Fn(&str, Option<&[u8]>) -> Output + Send + Sync;

#[derive(Clone)]
pub struct Mock(pub Arc<MockFn>);

impl std::fmt::Debug for Mock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Mock")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Local,
    Ssh,
    /// `docker` / `podman` / `nerdctl` exec into a running container (host = container name)
    Container(String),
}

#[derive(Clone, Debug)]
pub struct Conn {
    pub kind: Kind,
    /// true for the local connection (kept for module code that needs it)
    pub local: bool,
    pub host: String,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub key: Option<String>,
    pub password: Option<String>,
    pub ssh_args: Option<String>,
    pub become_user: Option<String>, // Some(user) when become is active
    pub become_method: String,
    pub become_password: Option<String>,
    pub env: Vec<(String, String)>,
    pub mock: Option<Mock>,
}

#[derive(Clone, Debug, Default)]
pub struct Output {
    pub rc: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Conn {
    /// A connection whose commands are answered by `f` (unit tests).
    #[allow(dead_code)]
    pub fn mock(f: impl Fn(&str, Option<&[u8]>) -> Output + Send + Sync + 'static) -> Conn {
        Conn {
            kind: Kind::Local,
            local: true,
            host: "mock".into(),
            port: None,
            user: None,
            key: None,
            password: None,
            ssh_args: None,
            become_user: None,
            become_method: "sudo".into(),
            become_password: None,
            env: vec![],
            mock: Some(Mock(Arc::new(f))),
        }
    }

    fn ssh_base(&self) -> Command {
        let cfg = crate::config::get();
        let use_pass = self.password.is_some() && have("sshpass");
        let mut c = if use_pass {
            let mut c = Command::new("sshpass");
            c.arg("-e")
                .env("SSHPASS", self.password.clone().unwrap_or_default())
                .arg("ssh");
            c
        } else {
            Command::new("ssh")
        };
        if !use_pass {
            c.args(["-o", "BatchMode=yes"]);
        }
        c.args([
            "-o",
            if cfg.host_key_checking == Some(false) {
                "StrictHostKeyChecking=no"
            } else {
                "StrictHostKeyChecking=accept-new"
            },
        ]);
        if let Some(t) = cfg.timeout {
            c.args(["-o", &format!("ConnectTimeout={t}")]);
        }
        if let Some(a) = &cfg.ssh_args {
            c.args(a.split_whitespace());
        }
        c.args(["-o", "ControlMaster=auto", "-o", "ControlPersist=60s"]);
        c.args(["-o", "ControlPath=/tmp/ransible-%C"]);
        if let Some(p) = self.port {
            c.args(["-p", &p.to_string()]);
        }
        if let Some(k) = &self.key {
            c.args(["-i", k]);
        }
        if let Some(a) = &self.ssh_args {
            c.args(a.split_whitespace());
        }
        let target = match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        };
        c.arg(target);
        c
    }

    fn wrap(&self, cmd: &str, use_become: bool) -> String {
        let mut full = String::new();
        for (k, v) in &self.env {
            full.push_str(&format!("{k}={} ", shell_quote(v)));
        }
        full.push_str(cmd);
        match (&self.become_user, use_become) {
            (Some(u), true) => {
                let inner = format!("sh -c {}", shell_quote(&full));
                match self.become_method.as_str() {
                    "su" => format!("su - {u} -c {}", shell_quote(&full)),
                    "doas" => format!("doas -u {u} {inner}"),
                    "sudo" if self.become_password.is_some() => {
                        format!("sudo -S -p '' -H -u {u} {inner}")
                    }
                    _ => format!("sudo -n -H -u {u} {inner}"),
                }
            }
            _ => full,
        }
    }

    fn command(&self, wrapped: &str) -> Command {
        match &self.kind {
            Kind::Local => {
                let mut c = Command::new("/bin/sh");
                c.arg("-c").arg(wrapped);
                c
            }
            Kind::Ssh => {
                let mut c = self.ssh_base();
                c.arg(format!("/bin/sh -c {}", shell_quote(wrapped)));
                c
            }
            Kind::Container(rt) => {
                let mut c = Command::new(rt);
                c.args(["exec", "-i"]);
                if let Some(u) = &self.user {
                    c.args(["-u", u]);
                }
                c.arg(&self.host).args(["sh", "-c", wrapped]);
                c
            }
        }
    }

    fn run(&self, cmd: &str, stdin: Option<&[u8]>, use_become: bool) -> Result<Output> {
        if let Some(m) = &self.mock {
            return Ok((m.0)(cmd, stdin));
        }
        let wrapped = self.wrap(cmd, use_become);
        // sudo -S reads the password from the first stdin line
        let mut input: Option<Vec<u8>> = stdin.map(|s| s.to_vec());
        if use_become
            && self.become_user.is_some()
            && self.become_method == "sudo"
            && let Some(pw) = &self.become_password
        {
            let mut v = format!("{pw}\n").into_bytes();
            v.extend(input.unwrap_or_default());
            input = Some(v);
        }
        let mut c = self.command(&wrapped);
        c.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = c.spawn().map_err(|e| anyhow!("failed to spawn: {e}"))?;
        let writer = input.map(|data| {
            let mut si = child.stdin.take().unwrap();
            std::thread::spawn(move || {
                let _ = si.write_all(&data);
            })
        });
        let o = child.wait_with_output()?;
        if let Some(h) = writer {
            let _ = h.join();
        }
        Ok(Output {
            rc: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).into(),
            stderr: String::from_utf8_lossy(&o.stderr).into(),
        })
    }

    /// Run a shell command (honours become).
    pub fn exec(&self, cmd: &str) -> Result<Output> {
        self.run(cmd, None, true)
    }

    pub fn exec_stdin(&self, cmd: &str, stdin: &[u8]) -> Result<Output> {
        self.run(cmd, Some(stdin), true)
    }

    /// Read raw bytes (binary-safe) of a remote file.
    pub fn get_bytes(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let cmd = format!("test -e {p} && cat {p}", p = shell_quote(path));
        if let Some(m) = &self.mock {
            let o = (m.0)(&cmd, None);
            return Ok(if o.rc == 0 {
                Some(o.stdout.into_bytes())
            } else {
                None
            });
        }
        let wrapped = self.wrap(&cmd, true);
        let mut input = None;
        if self.become_user.is_some() && self.become_method == "sudo" {
            input = self
                .become_password
                .as_ref()
                .map(|p| format!("{p}\n").into_bytes());
        }
        let mut c = self.command(&wrapped);
        c.stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = c.spawn()?;
        if let Some(d) = input {
            let mut si = child.stdin.take().unwrap();
            std::thread::spawn(move || {
                let _ = si.write_all(&d);
            });
        }
        let o = child.wait_with_output()?;
        if !o.status.success() {
            return Ok(None);
        }
        Ok(Some(o.stdout))
    }

    /// Write a remote file atomically-ish.
    pub fn put(&self, path: &str, data: &[u8], mode: Option<&str>) -> Result<()> {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = format!(
            "{path}.ransible.{}.{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let mut cmd = format!(
            "cat > {t} && mv -f {t} {p}",
            t = shell_quote(&tmp),
            p = shell_quote(path)
        );
        if let Some(m) = mode {
            cmd = format!(
                "cat > {t} && chmod {m} {t} && mv -f {t} {p}",
                t = shell_quote(&tmp),
                p = shell_quote(path)
            );
        }
        let o = self.run(&cmd, Some(data), true)?;
        if o.rc != 0 {
            return Err(anyhow!("failed to write {path}: {}", o.stderr.trim()));
        }
        Ok(())
    }
}

fn have(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}
