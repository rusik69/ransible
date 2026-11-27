//! Runner: vars.
#![allow(unused_imports)]
use super::*;

impl Runner {
    // ---------- variable assembly ----------

    /// Resolve templated variable values, dependencies first (approximates Ansible's lazy evaluation).
    pub(super) fn resolve_vars(&self, v: &mut Vars) {
        const SKIP: &[&str] = &["hostvars", "groups", "ansible_facts", "vars"];
        let mut pending: Vec<String> = v
            .iter()
            .filter(|(k, val)| !SKIP.contains(&k.as_str()) && tpl(val))
            .map(|(k, _)| k.clone())
            .collect();
        if pending.is_empty() {
            return;
        }
        let ident = |text: &str, name: &str| -> bool {
            let b = text.as_bytes();
            let mut from = 0;
            while let Some(i) = text[from..].find(name) {
                let s = from + i;
                let e = s + name.len();
                let before_ok = s == 0
                    || !(b[s - 1].is_ascii_alphanumeric() || b[s - 1] == b'_' || b[s - 1] == b'.');
                let after_ok = e >= b.len() || !(b[e].is_ascii_alphanumeric() || b[e] == b'_');
                if before_ok && after_ok {
                    return true;
                }
                from = e;
            }
            false
        };
        let mut guard = 0;
        while !pending.is_empty() && guard < 64 {
            guard += 1;
            let texts: Vec<(String, String)> = pending
                .iter()
                .map(|k| (k.clone(), serde_json::to_string(&v[k]).unwrap_or_default()))
                .collect();
            let ready: Vec<String> = texts
                .iter()
                .filter(|(k, t)| !texts.iter().any(|(o, _)| o != k && ident(t, o)))
                .map(|(k, _)| k.clone())
                .collect();
            // cycle: render everything that is left once
            let batch = if ready.is_empty() {
                pending.clone()
            } else {
                ready
            };
            let snap = v.clone();
            for k in &batch {
                if let Ok(n) = self.templar.render_value(&snap[k], &snap) {
                    v.insert(k.clone(), n);
                }
            }
            pending.retain(|k| !batch.contains(k));
        }
    }

    pub(super) fn host_ctx(&self, h: &Host, env: &TaskEnv) -> Vars {
        self.host_ctx_t(h, env, None, None)
    }

    pub(super) fn host_ctx_t(
        &self,
        h: &Host,
        env: &TaskEnv,
        tvars: Option<&Vars>,
        role_params: Option<&Vars>,
    ) -> Vars {
        let mut v = h.base.clone();
        v.extend(h.facts.clone());
        v.extend(h.layer.clone());
        if let Some(tv) = tvars {
            v.extend(tv.clone());
        }
        v.extend(h.dynv.clone());
        v.extend(h.params.clone());
        if let Some(rp) = role_params {
            v.extend(rp.clone());
        }
        v.insert("inventory_hostname".into(), h.name.clone().into());
        v.insert(
            "inventory_hostname_short".into(),
            h.name.split('.').next().unwrap_or("").into(),
        );
        let invr = self.inv.read().unwrap();
        let gn: Vec<Value> = invr
            .groups_of(&h.name)
            .into_iter()
            .filter(|g| {
                g != "ungrouped"
                    || self.inv.read().unwrap().groups["ungrouped"]
                        .hosts
                        .contains(&h.name)
            })
            .map(Value::from)
            .collect();
        v.insert("group_names".into(), Value::Array(gn));
        v.insert("groups".into(), env.groups.clone());
        let (all, active) = (
            env.play_hosts["all"].clone(),
            env.play_hosts["active"].clone(),
        );
        v.insert("ansible_play_hosts".into(), active.clone());
        v.insert("ansible_play_hosts_all".into(), all);
        v.insert("ansible_play_batch".into(), env.play_hosts["batch"].clone());
        v.insert("play_hosts".into(), active);
        v.insert("omit".into(), crate::modules::OMIT.into());
        v.insert(
            "playbook_dir".into(),
            self.playbook_dir.display().to_string().into(),
        );
        v.insert(
            "inventory_dir".into(),
            self.inv
                .read()
                .unwrap()
                .base_dir
                .display()
                .to_string()
                .into(),
        );
        v.insert("ansible_check_mode".into(), self.opts.check.into());
        v.insert(
            "ansible_version".into(),
            json!({"full": "2.16.0", "major": 2, "minor": 16, "string": "2.16.0"}),
        );
        if let Some(hv) = env.hostvars {
            v.insert("hostvars".into(), hv.clone());
        }
        v
    }

    pub(super) fn task_vars(
        &self,
        h: &Host,
        t: &Task,
        env: &TaskEnv,
        item: Option<(&str, &Value)>,
    ) -> Vars {
        let tv = match t.kw.get("vars") {
            Some(Value::Object(tv)) => Some(tv),
            _ => None,
        };
        let mut v = self.host_ctx_t(h, env, tv, t.role_params.as_deref());
        v.extend(env.overlay.clone());
        if let Some((k, val)) = item {
            v.insert(k.into(), val.clone());
        }
        v.extend(self.opts.extra_vars.clone());
        v.insert("role_path".into(), t.dir.display().to_string().into());
        self.resolve_vars(&mut v);
        v
    }

    pub(super) fn hostvars_snapshot(
        &self,
        hosts: &[Host],
        env_groups: &Value,
        play_hosts: &Value,
    ) -> Value {
        let mut out = Map::new();
        let overlay = Map::new();
        let dummy_play = Play::default();
        for h in hosts {
            let env = TaskEnv {
                play: &dummy_play,
                hostvars: None,
                groups: env_groups,
                play_hosts,
                overlay: &overlay,
            };
            let mut v = self.host_ctx(h, &env);
            v.remove("groups");
            self.resolve_vars(&mut v);
            out.insert(h.name.clone(), Value::Object(v));
        }
        Value::Object(out)
    }

    pub(super) fn make_conn(&self, vars: &Vars, host: &str) -> Conn {
        let s = |ks: &[&str]| {
            ks.iter()
                .find_map(|k| vars.get(*k).filter(|v| !v.is_null()).map(vstr))
        };
        let connection = s(&["ansible_connection"]).unwrap_or_default();
        let ah = s(&["ansible_host", "ansible_ssh_host"]);
        let local = connection == "local"
            || (connection.is_empty()
                && ah.is_none()
                && (host == "localhost" || host == "127.0.0.1"));
        let kind = if local {
            crate::conn::Kind::Local
        } else if matches!(
            connection.as_str(),
            "docker"
                | "podman"
                | "nerdctl"
                | "community.docker.docker"
                | "containers.podman.podman"
        ) {
            crate::conn::Kind::Container(
                connection
                    .rsplit('.')
                    .next()
                    .unwrap_or("docker")
                    .to_string(),
            )
        } else {
            crate::conn::Kind::Ssh
        };
        let mut args = vec![];
        for k in ["ansible_ssh_common_args", "ansible_ssh_extra_args"] {
            if let Some(a) = s(&[k]) {
                args.push(a);
            }
        }
        Conn {
            kind,
            local,
            password: s(&[
                "ansible_password",
                "ansible_ssh_pass",
                "ansible_ssh_password",
            ]),
            become_password: s(&[
                "ansible_become_password",
                "ansible_become_pass",
                "ansible_sudo_pass",
            ]),
            mock: None,
            host: ah.unwrap_or_else(|| host.to_string()),
            port: s(&["ansible_port", "ansible_ssh_port"]).and_then(|p| p.parse().ok()),
            user: s(&["ansible_user", "ansible_ssh_user", "ansible_remote_user"]),
            key: s(&["ansible_ssh_private_key_file", "ansible_private_key_file"]),
            ssh_args: if args.is_empty() {
                None
            } else {
                Some(args.join(" "))
            },
            become_user: None,
            become_method: "sudo".into(),
            env: vec![],
        }
    }
}
