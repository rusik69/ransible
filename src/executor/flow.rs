//! Runner: flow.
#![allow(unused_imports)]
use super::*;

impl Runner {
    // ---------- task list execution ----------

    pub(super) fn active(&self, ps: &PlayState, idxs: &[usize]) -> Vec<usize> {
        idxs.iter()
            .copied()
            .filter(|&i| !ps.hosts[i].failed && !ps.hosts[i].ended)
            .collect()
    }

    pub(super) fn run_tasks(
        &self,
        ps: &mut PlayState,
        idxs: &[usize],
        tasks: &[Task],
        overlay: &Map<String, Value>,
    ) -> Result<()> {
        for t in tasks {
            let act = self.active(ps, idxs);
            if act.is_empty() {
                if !ps.hosts.is_empty() && ps.hosts.iter().all(|h| h.failed) && !ps.no_hosts_shown {
                    ps.no_hosts_shown = true;
                    self.emit(Event::Banner {
                        kind: "NO MORE HOSTS LEFT".into(),
                        name: String::new(),
                        role: None,
                    });
                }
                break;
            }
            if t.is_block {
                self.run_block(ps, &act, t, overlay)?;
                continue;
            }
            ps.refresh_active();
            // dynamic includes are tasks themselves: they obey --tags/--skip-tags
            if matches!(t.module.as_str(), "include_tasks" | "include_role") && !self.selected(t) {
                continue;
            }
            match t.module.as_str() {
                "include_tasks" | "import_tasks" => self.run_include_tasks(ps, &act, t, overlay)?,
                "include_role" | "import_role" => self.run_include_role(ps, &act, t, overlay)?,
                "meta" => {
                    // `when:` applies per host (e.g. `meta: end_host` for some hosts only)
                    let sub = self.include_when(ps, &act, t, overlay);
                    if !sub.is_empty() {
                        self.run_meta(ps, &sub, t, overlay)?
                    }
                }
                _ => self.run_leaf(ps, &act, t, overlay, None)?,
            }
        }
        Ok(())
    }

    pub(super) fn run_meta(
        &self,
        ps: &mut PlayState,
        act: &[usize],
        t: &Task,
        overlay: &Map<String, Value>,
    ) -> Result<()> {
        let what = t
            .args
            .get("_raw_params")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        match what.as_str() {
            "flush_handlers" => self.flush_handlers(ps, act, overlay)?,
            "end_play" | "end_batch" => {
                for &i in act {
                    ps.hosts[i].ended = true;
                }
            }
            "end_host" => {
                for &i in act {
                    ps.hosts[i].ended = true;
                }
            }
            "clear_facts" => {
                for &i in act {
                    ps.hosts[i].facts.clear();
                }
            }
            "noop" | "refresh_inventory" | "clear_host_errors" | "reset_connection" => {}
            other => return Err(anyhow!("unsupported meta action '{other}'")),
        }
        Ok(())
    }

    pub(super) fn run_leaf(
        &self,
        ps: &mut PlayState,
        act: &[usize],
        t: &Task,
        overlay: &Map<String, Value>,
        title: Option<&str>,
    ) -> Result<()> {
        if let Some(n) = &self.opts.start_at_task {
            use std::sync::atomic::Ordering;
            if !self.started.load(Ordering::Relaxed) {
                if t.name.as_deref() == Some(n.as_str()) {
                    self.started.store(true, Ordering::Relaxed);
                } else {
                    return Ok(());
                }
            }
        }
        if !self.selected(t) {
            return Ok(());
        }
        let hv = if self.need_hostvars {
            Some(self.hostvars_snapshot(&ps.hosts, &ps.groups, &ps.play_hosts))
        } else {
            None
        };
        let name = match (title, &t.name) {
            (Some(n), _) => n.to_string(),
            (None, Some(n)) => {
                let first = act[0];
                let env = TaskEnv {
                    play: ps.play,
                    hostvars: hv.as_ref(),
                    groups: &ps.groups,
                    play_hosts: &ps.play_hosts,
                    overlay,
                };
                let v = self.host_ctx(&ps.hosts[first], &env);
                self.templar
                    .render_string(n, &v)
                    .unwrap_or_else(|_| n.clone())
            }
            (None, None) => t.module.clone(),
        };
        let name = match &t.role {
            Some(r) if title.is_none() => format!("{r} : {name}"),
            _ => name,
        };
        self.banner("TASK", &name);
        let run_once = t.kw.get("run_once").and_then(loose_bool).unwrap_or(false);
        let target: Vec<usize> = if run_once { vec![act[0]] } else { act.to_vec() };
        let env = TaskEnv {
            play: ps.play,
            hostvars: hv.as_ref(),
            groups: &ps.groups,
            play_hosts: &ps.play_hosts,
            overlay,
        };
        let forks = if t.kw.get("throttle").and_then(|v| v.as_u64()).is_some() {
            t.kw["throttle"].as_u64().unwrap() as usize
        } else {
            self.opts.forks.max(1)
        };
        let mut refs: Vec<(usize, &mut Host)> = ps
            .hosts
            .iter_mut()
            .enumerate()
            .filter(|(i, _)| target.contains(i))
            .collect();
        let mut results: Vec<(usize, TaskRun)> = vec![];
        for chunk in refs.chunks_mut(forks) {
            std::thread::scope(|s| {
                let handles: Vec<_> = chunk
                    .iter_mut()
                    .map(|(i, h)| {
                        let i = *i;
                        let h: &mut Host = h;
                        let env = &env;
                        s.spawn(move || (i, self.exec_task(h, t, env)))
                    })
                    .collect();
                for hd in handles {
                    results.push(hd.join().expect("task thread panicked"));
                }
            });
        }
        results.sort_by_key(|r| r.0);
        let mut touched_first: Vec<String> = vec![];
        for (n, (_, r)) in results.iter().enumerate() {
            for ev in &r.events {
                self.sink.emit(ev);
            }
            if n == 0 {
                touched_first = r.touched.clone();
            }
        }
        if run_once && act.len() > 1 {
            let mut src: Vars = ps.hosts[act[0]].facts.clone();
            src.extend(ps.hosts[act[0]].dynv.clone());
            for &i in &act[1..] {
                for k in &touched_first {
                    if let Some(v) = src.get(k) {
                        ps.hosts[i].dynv.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        let any_fatal = ps
            .play
            .kw
            .get("any_errors_fatal")
            .and_then(loose_bool)
            .unwrap_or(false);
        if any_fatal && act.iter().any(|&i| ps.hosts[i].failed) {
            for &i in act {
                ps.hosts[i].failed = true;
            }
        }
        if let Some(pct) = ps.play.kw.get("max_fail_percentage").and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        }) {
            let total = ps.hosts.len().max(1) as f64;
            let failed = ps.hosts.iter().filter(|h| h.failed).count() as f64;
            if failed / total * 100.0 > pct {
                for h in ps.hosts.iter_mut() {
                    h.failed = true;
                }
            }
        }
        Ok(())
    }

    pub(super) fn run_block(
        &self,
        ps: &mut PlayState,
        act: &[usize],
        t: &Task,
        overlay: &Map<String, Value>,
    ) -> Result<()> {
        self.run_tasks(ps, act, &t.block, overlay)?;
        let failed: Vec<usize> = act
            .iter()
            .copied()
            .filter(|&i| ps.hosts[i].failed)
            .collect();
        let mut still_failed: Vec<usize> = failed.clone();
        if !t.rescue.is_empty() && !failed.is_empty() {
            for &i in &failed {
                ps.hosts[i].failed = false;
                if let Some((name, result)) = ps.hosts[i].last_fail.clone() {
                    ps.hosts[i]
                        .dynv
                        .insert("ansible_failed_task".into(), json!({"name": name}));
                    ps.hosts[i]
                        .dynv
                        .insert("ansible_failed_result".into(), result);
                }
            }
            self.run_tasks(ps, &failed, &t.rescue, overlay)?;
            still_failed = failed
                .iter()
                .copied()
                .filter(|&i| ps.hosts[i].failed)
                .collect();
            for &i in &failed {
                if !ps.hosts[i].failed {
                    ps.hosts[i].stats.rescued += 1;
                    ps.hosts[i].stats.failed = ps.hosts[i].stats.failed.saturating_sub(1);
                }
            }
        }
        if !t.always.is_empty() {
            for &i in &still_failed {
                ps.hosts[i].failed = false;
            }
            let all: Vec<usize> = act
                .iter()
                .copied()
                .filter(|&i| !ps.hosts[i].ended)
                .collect();
            self.run_tasks(ps, &all, &t.always, overlay)?;
            for &i in &still_failed {
                ps.hosts[i].failed = true;
            }
        }
        Ok(())
    }

    pub(super) fn include_path(&self, t: &Task, play: &Play, name: &str) -> Result<PathBuf> {
        let p = Path::new(name);
        if p.is_absolute() && p.exists() {
            return Ok(p.to_path_buf());
        }
        let cands = [
            t.dir.join("tasks").join(name),
            t.dir.join(name),
            play.dir.join(name),
            play.dir.join("tasks").join(name),
        ];
        cands
            .into_iter()
            .find(|c| c.is_file())
            .ok_or_else(|| anyhow!("could not find included file '{name}'"))
    }

    pub(super) fn include_when(
        &self,
        ps: &PlayState,
        act: &[usize],
        t: &Task,
        overlay: &Map<String, Value>,
    ) -> Vec<usize> {
        let env = TaskEnv {
            play: ps.play,
            hostvars: None,
            groups: &ps.groups,
            play_hosts: &ps.play_hosts,
            overlay,
        };
        act.iter()
            .copied()
            .filter(|&i| {
                let v = self.task_vars(&ps.hosts[i], t, &env, None);
                match t.kw.get("when") {
                    None => true,
                    Some(Value::Array(a)) => a.iter().all(|c| match c {
                        Value::String(s) => self.templar.eval_bool(s, &v).unwrap_or(false),
                        o => loose_bool(o).unwrap_or(true),
                    }),
                    Some(o) => loose_bool(o).unwrap_or(true),
                }
            })
            .collect()
    }

    pub(super) fn run_include_tasks(
        &self,
        ps: &mut PlayState,
        act: &[usize],
        t: &Task,
        overlay: &Map<String, Value>,
    ) -> Result<()> {
        let sub = self.include_when(ps, act, t, overlay);
        if t.module == "include_tasks" {
            self.include_banner(ps, act, &sub, t, "include_tasks".into());
        }
        if sub.is_empty() {
            return Ok(());
        }
        let (hv, first) = (None::<Value>, sub[0]);
        let env = TaskEnv {
            play: ps.play,
            hostvars: hv.as_ref(),
            groups: &ps.groups,
            play_hosts: &ps.play_hosts,
            overlay,
        };
        let vars = self.task_vars(&ps.hosts[first], t, &env, None);
        let raw = t
            .args
            .get("file")
            .or(t.args.get("_raw_params"))
            .map(vstr)
            .ok_or_else(|| anyhow!("include_tasks requires a file"))?;
        let file = self.templar.render_string(&raw, &vars)?;
        let path = self.include_path(t, ps.play, &file)?;
        let mut tasks = crate::playbook::parse_tasks(&read_yaml(&path)?, &t.dir)?;
        if let Some(r) = &t.role {
            crate::playbook::tag_role(&mut tasks, r);
        }
        if let Some(p) = &t.role_params {
            crate::playbook::set_role_params(&mut tasks, p);
        }
        let mut kw = t.kw.clone();
        // dynamic include_tasks: tags on the include itself do not spread to the included tasks
        if t.module == "include_tasks" {
            kw.remove("tags");
        }
        kw.remove("when");
        kw.remove("loop");
        kw.remove("loop_control");
        kw.remove("name");
        kw.remove("register");
        let mut pvars = kw.get("vars").cloned();
        if let Some(Value::Object(extra)) = t
            .args
            .as_object()
            .map(|a| {
                a.iter()
                    .filter(|(k, _)| !["file", "_raw_params", "apply"].contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Map<_, _>>()
            })
            .map(Value::Object)
            && !extra.is_empty()
        {
            let mut m = pvars
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            m.extend(extra);
            pvars = Some(Value::Object(m));
        }
        if let Some(v) = pvars {
            kw.insert("vars".into(), v);
        }
        if let Some(Value::Object(ap)) = t.args.get("apply") {
            for (k, v) in ap {
                kw.insert(k.clone(), v.clone());
            }
        }
        apply_inherit(&kw, &mut tasks);
        match t.kw.get("loop") {
            None => self.run_tasks(ps, &sub, &tasks, overlay),
            Some(l) => {
                let items = match self.templar.render_value(l, &vars)? {
                    Value::Array(a) => a,
                    o => return Err(anyhow!("include loop requires a list, got {o}")),
                };
                let lv =
                    t.kw.get("loop_control")
                        .and_then(|c| c.get("loop_var"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("item")
                        .to_string();
                for it in items {
                    let mut ov = overlay.clone();
                    ov.insert(lv.clone(), it);
                    self.run_tasks(ps, &sub, &tasks, &ov)?;
                }
                Ok(())
            }
        }
    }

    /// Dynamic includes print a task banner and an ok/skipped line per host, like Ansible.
    fn include_banner(
        &self,
        ps: &mut PlayState,
        act: &[usize],
        sub: &[usize],
        t: &Task,
        default_name: String,
    ) {
        let name = t.name.clone().unwrap_or(default_name);
        self.emit(Event::Banner {
            kind: "TASK".into(),
            name,
            role: t.role.clone(),
        });
        for &i in act {
            let ran = sub.contains(&i);
            self.emit(Event::HostResult {
                host: ps.hosts[i].name.clone(),
                status: if ran { "ok" } else { "skipped" }.into(),
                module: t.module.clone(),
                item: None,
                ignored: false,
                result: json!({"changed": false}),
            });
            if ran {
                ps.hosts[i].stats.ok += 1;
            } else {
                ps.hosts[i].stats.skipped += 1;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn load_role_tree(
        &self,
        name: &str,
        play_dir: &Path,
        tasks_from: Option<&str>,
        params: &Map<String, Value>,
        seen: &mut HashSet<String>,
        out: &mut Vec<(Role, Map<String, Value>)>,
        force: bool,
    ) -> Result<()> {
        let role = load_role(name, play_dir, tasks_from)?;
        // a role runs once per distinct set of parameters, unless it allows duplicates
        let key = |n: &str, p: &Map<String, Value>| format!("{n}|{}", Value::Object(p.clone()));
        for d in &role.deps {
            let dep_role_dup = load_role(&d.name, play_dir, None)
                .map(|r| r.allow_duplicates)
                .unwrap_or(false);
            if dep_role_dup || !seen.contains(&key(&d.name, &d.vars)) {
                self.load_role_tree(&d.name, play_dir, None, &d.vars, seen, out, false)?;
            }
        }
        // `force`: an explicit include_role always runs the named role itself
        if (seen.insert(key(name, params)) || force) || role.allow_duplicates {
            out.push((role, params.clone()));
        }
        Ok(())
    }

    pub(super) fn run_include_role(
        &self,
        ps: &mut PlayState,
        act: &[usize],
        t: &Task,
        overlay: &Map<String, Value>,
    ) -> Result<()> {
        let sub = self.include_when(ps, act, t, overlay);
        let rname = t.args.get("name").map(vstr).unwrap_or_default();
        if t.module == "include_role" {
            self.include_banner(ps, act, &sub, t, format!("include_role : {rname}"));
        }
        if sub.is_empty() {
            return Ok(());
        }
        let name = t
            .args
            .get("name")
            .map(vstr)
            .ok_or_else(|| anyhow!("include_role requires name"))?;
        let name = {
            let env = TaskEnv {
                play: ps.play,
                hostvars: None,
                groups: &ps.groups,
                play_hosts: &ps.play_hosts,
                overlay,
            };
            let vars = self.task_vars(&ps.hosts[sub[0]], t, &env, None);
            self.templar.render_string(&name, &vars)?
        };
        let tf = t.args.get("tasks_from").map(vstr);
        let mut params = Map::new();
        if let Some(Value::Object(v)) = t.args.get("vars") {
            params.extend(v.clone());
        }
        let mut roles = vec![];
        // dependencies already run in this play are not repeated (same parameters)
        let mut seen = std::mem::take(&mut ps.role_seen);
        let loaded = self.load_role_tree(
            &name,
            &ps.play.dir,
            tf.as_deref(),
            &params,
            &mut seen,
            &mut roles,
            true,
        );
        ps.role_seen = seen;
        loaded?;
        for (role, params) in roles {
            for &i in &sub {
                for (k, v) in &role.defaults {
                    ps.hosts[i].base.entry(k.clone()).or_insert(v.clone());
                }
                ps.hosts[i].layer.extend(role.vars.clone());
            }
            let params_arc = std::sync::Arc::new(params);
            let mut tasks = role.tasks.clone();
            crate::playbook::set_role_params(&mut tasks, &params_arc);
            let mut kw = t.kw.clone();
            for k in ["when", "name", "register", "loop"] {
                kw.remove(k);
            }
            // dynamic include_role: tags on the include do not spread to the role's tasks
            if t.module == "include_role" {
                kw.remove("tags");
            }
            apply_inherit(&kw, &mut tasks);
            let mut hs = role.handlers.clone();
            crate::playbook::set_role_params(&mut hs, &params_arc);
            apply_inherit(&kw, &mut hs);
            for h in hs {
                if !ps
                    .handlers
                    .iter()
                    .any(|x| x.role == h.role && x.name == h.name && x.module == h.module)
                {
                    ps.handlers.push(h);
                }
            }
            self.run_tasks(ps, &sub, &tasks, overlay)?;
        }
        Ok(())
    }

    pub(super) fn flush_handlers(
        &self,
        ps: &mut PlayState,
        act: &[usize],
        overlay: &Map<String, Value>,
    ) -> Result<()> {
        for _ in 0..10 {
            let mut ran = false;
            let handlers = ps.handlers.clone();
            // every handler listening to a notified topic/name runs in this pass; names are cleared afterwards
            let snapshot: Vec<std::collections::BTreeSet<String>> =
                ps.hosts.iter().map(|h| h.notified.clone()).collect();
            for hd in &handlers {
                let mut names = to_list(hd.kw.get("listen"));
                if let Some(n) = &hd.name {
                    names.push(n.clone());
                }
                let todo: Vec<usize> = act
                    .iter()
                    .copied()
                    .filter(|&i| {
                        !ps.hosts[i].failed
                            && !ps.hosts[i].ended
                            && names.iter().any(|n| snapshot[i].contains(n))
                    })
                    .collect();
                if todo.is_empty() {
                    continue;
                }
                ran = true;
                let mut hd2 = hd.clone();
                hd2.kw.remove("tags");
                let title = hd
                    .name
                    .clone()
                    .unwrap_or_else(|| hd.module.clone())
                    .to_string();
                if hd2.is_block {
                    self.run_tasks(ps, &todo, std::slice::from_ref(&hd2), overlay)?;
                } else if matches!(hd2.module.as_str(), "include_tasks" | "import_tasks") {
                    self.run_include_tasks(ps, &todo, &hd2, overlay)?;
                } else {
                    let saved = (self.opts.tags.clone(), ());
                    let _ = saved;
                    self.emit(Event::Banner {
                        kind: "RUNNING HANDLER".into(),
                        name: title.clone(),
                        role: hd2.role.clone(),
                    });
                    self.run_leaf_nobanner(ps, &todo, &hd2, overlay)?;
                }
            }
            for (i, h) in ps.hosts.iter_mut().enumerate() {
                for n in &snapshot[i] {
                    h.notified.remove(n);
                }
            }
            if !ran {
                break;
            }
        }
        Ok(())
    }

    pub(super) fn run_leaf_nobanner(
        &self,
        ps: &mut PlayState,
        act: &[usize],
        t: &Task,
        overlay: &Map<String, Value>,
    ) -> Result<()> {
        // handlers ignore tag filtering; reuse run_leaf but suppress its banner by capturing stdout ordering
        let hv = if self.need_hostvars {
            Some(self.hostvars_snapshot(&ps.hosts, &ps.groups, &ps.play_hosts))
        } else {
            None
        };
        let env = TaskEnv {
            play: ps.play,
            hostvars: hv.as_ref(),
            groups: &ps.groups,
            play_hosts: &ps.play_hosts,
            overlay,
        };
        let forks = self.opts.forks.max(1);
        let mut refs: Vec<(usize, &mut Host)> = ps
            .hosts
            .iter_mut()
            .enumerate()
            .filter(|(i, _)| act.contains(i))
            .collect();
        let mut results: Vec<(usize, TaskRun)> = vec![];
        for chunk in refs.chunks_mut(forks) {
            std::thread::scope(|s| {
                let hs: Vec<_> = chunk
                    .iter_mut()
                    .map(|(i, h)| {
                        let i = *i;
                        let h: &mut Host = h;
                        let env = &env;
                        s.spawn(move || (i, self.exec_task(h, t, env)))
                    })
                    .collect();
                for hd in hs {
                    results.push(hd.join().expect("handler thread panicked"));
                }
            });
        }
        results.sort_by_key(|r| r.0);
        for (_, r) in &results {
            for ev in &r.events {
                self.sink.emit(ev);
            }
        }
        Ok(())
    }
}
