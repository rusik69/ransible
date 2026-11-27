//! Runner: play.
#![allow(unused_imports)]
use super::*;

impl Runner {
    // ---------- plays ----------

    pub(super) fn list_tasks(&self, tasks: &[Task], indent: usize) {
        for t in tasks {
            let tags = to_list(t.kw.get("tags"));
            let tg = if tags.is_empty() {
                String::new()
            } else {
                format!("\tTAGS: [{}]", tags.join(", "))
            };
            if t.is_block {
                self.msg(format!("{}block{}", " ".repeat(indent), tg));
                self.list_tasks(&t.block, indent + 2);
                self.list_tasks(&t.rescue, indent + 2);
                self.list_tasks(&t.always, indent + 2);
            } else if self.selected(t) {
                self.msg(format!(
                    "{}{}{}",
                    " ".repeat(indent),
                    t.name.clone().unwrap_or_else(|| t.module.clone()),
                    tg
                ));
            }
        }
    }

    /// One play pass over `batch` (gather facts, pre_tasks, roles, tasks, post_tasks, handlers).
    pub(super) fn run_batch(
        &self,
        ps: &mut PlayState,
        batch: &[usize],
        role_tasks: &[Task],
        ov: &Map<String, Value>,
    ) -> Result<()> {
        let play = ps.play;
        if play.gather_facts {
            let t = Task {
                name: Some("Gathering Facts".into()),
                module: "setup".into(),
                args: json!({}),
                dir: play.dir.clone(),
                ..Default::default()
            };
            let act = self.active(ps, batch);
            if !act.is_empty() {
                self.run_leaf(ps, &act, &t, ov, Some("Gathering Facts"))?;
            }
        }
        self.run_tasks(ps, batch, &play.pre_tasks, ov)?;
        let act = self.active(ps, batch);
        self.flush_handlers(ps, &act, ov)?;
        self.run_tasks(ps, batch, role_tasks, ov)?;
        self.run_tasks(ps, batch, &play.tasks, ov)?;
        let act = self.active(ps, batch);
        self.flush_handlers(ps, &act, ov)?;
        self.run_tasks(ps, batch, &play.post_tasks, ov)?;
        let act = self.active(ps, batch);
        self.flush_handlers(ps, &act, ov)?;
        Ok(())
    }

    pub fn run_play(&self, play: &Play) -> Result<bool> {
        let mut names = self.inv.read().unwrap().select(&play.hosts);
        if let Some(l) = &self.opts.limit {
            let lim = self.inv.read().unwrap().select(l);
            names.retain(|h| lim.contains(h));
        }
        if names.is_empty() && play.hosts == "localhost" {
            // implicit localhost
            names.push("localhost".into());
        }
        // roles (tree, deps first)
        let mut seen = HashSet::new();
        type RoleEntry = (Role, Map<String, Value>, Map<String, Value>);
        let mut roles: Vec<RoleEntry> = vec![];
        for rr in &play.roles {
            let mut tmp = vec![];
            self.load_role_tree(&rr.name, &play.dir, None, &rr.vars, &mut seen, &mut tmp)?;
            for (r, p) in tmp {
                let mut kw = rr.kw.clone();
                let mut tags = to_list(kw.get("tags"));
                tags.push(r.name.clone());
                kw.insert(
                    "tags".into(),
                    Value::Array(tags.into_iter().map(Value::from).collect()),
                );
                roles.push((r, p, kw));
            }
        }
        let mut role_tasks: Vec<Task> = vec![];
        let mut handlers: Vec<Task> = play.handlers.clone();
        let mut defaults = Map::new();
        let mut rvars = Map::new();
        let mut rparams = Map::new();
        for (r, params, kw) in &roles {
            defaults.extend(r.defaults.clone());
            rvars.extend(r.vars.clone());
            rparams.extend(params.clone());
            let mut ts = r.tasks.clone();
            let mut pk = play.kw.clone();
            pk.extend(kw.clone());
            apply_inherit(&pk, &mut ts);
            role_tasks.extend(ts);
            let mut hs = r.handlers.clone();
            apply_inherit(&play.kw, &mut hs);
            handlers.extend(hs);
        }
        if self.opts.list_hosts {
            self.msg(format!(
                "  play #{} ({}): {}\tTAGS: []\n    pattern: [{:?}]\n    hosts ({}):",
                1,
                play.name,
                play.name,
                play.hosts,
                names.len()
            ));
            for n in &names {
                self.msg(format!("      {n}"));
            }
            return Ok(true);
        }
        if self.opts.list_tags {
            let mut tags = std::collections::BTreeSet::new();
            fn collect(ts: &[Task], out: &mut std::collections::BTreeSet<String>) {
                for t in ts {
                    out.extend(to_list(t.kw.get("tags")));
                    collect(&t.block, out);
                    collect(&t.rescue, out);
                    collect(&t.always, out);
                }
            }
            for l in [&play.pre_tasks, &role_tasks, &play.tasks, &play.post_tasks] {
                collect(l, &mut tags);
            }
            self.msg(format!(
                "\n  play: {}\n    TASK TAGS: [{}]",
                play.name,
                tags.into_iter().collect::<Vec<_>>().join(", ")
            ));
            return Ok(true);
        }
        if self.opts.list_tasks {
            self.msg(format!("\n  play: {}\n    tasks:", play.name));
            self.list_tasks(&play.pre_tasks, 6);
            self.list_tasks(&role_tasks, 6);
            self.list_tasks(&play.tasks, 6);
            self.list_tasks(&play.post_tasks, 6);
            return Ok(true);
        }
        self.banner("PLAY", &play.name);
        if names.is_empty() {
            self.msg("skipping: no hosts matched");
            return Ok(true);
        }
        let mut groups = Map::new();
        let inv = self.inv.read().unwrap();
        for g in inv.groups.keys() {
            groups.insert(
                g.clone(),
                Value::Array(inv.group_hosts(g).into_iter().map(Value::from).collect()),
            );
        }
        let groups = Value::Object(groups);
        let play_hosts = Value::Array(names.iter().cloned().map(Value::from).collect());
        let mut hosts = vec![];
        let overlay0 = Map::new();
        for n in &names {
            let mut base = defaults.clone();
            base.extend(inv.vars_for(n));
            let mut h = Host {
                name: n.clone(),
                base,
                facts: Map::new(),
                layer: Map::new(),
                params: Map::new(),
                dynv: Map::new(),
                failed: false,
                ended: false,
                notified: BTreeSet::new(),
                last_fail: None,
                stats: Stats::default(),
            };
            if n == "localhost" && !inv.hosts.contains(n) {
                h.base.insert("ansible_connection".into(), "local".into());
            }
            // vars_files
            let env = TaskEnv {
                play,
                hostvars: None,
                groups: &groups,
                play_hosts: &play_hosts,
                overlay: &overlay0,
            };
            let mut ctx = self.host_ctx(&h, &env);
            ctx.extend(play.vars.clone());
            for vf in &play.vars_files {
                let cands: Vec<String> = match vf {
                    Value::Array(a) => a.iter().map(vstr).collect(),
                    o => vec![vstr(o)],
                };
                for c in cands {
                    let c = self.templar.render_string(&c, &ctx).unwrap_or(c);
                    let p = play.dir.join(&c);
                    if p.is_file() {
                        let mut m = Map::new();
                        crate::inventory::try_load_vars_file(&p, &mut m)?;
                        h.layer.extend(m);
                        break;
                    }
                }
            }
            // play vars sit below vars_files; re-apply order: play vars < vars_files < role vars
            let vf = std::mem::take(&mut h.layer);
            h.layer.extend(play.vars.clone());
            h.layer.extend(vf);
            h.layer.extend(rvars.clone());
            h.params.extend(rparams.clone());
            hosts.push(h);
        }
        drop(inv);
        let mut ps = PlayState {
            play,
            hosts,
            handlers,
            groups,
            play_hosts,
        };
        // serial batches
        let total = ps.hosts.len();
        let mut batches: Vec<Vec<usize>> = vec![];
        let mut start = 0;
        let mut si = 0;
        while start < total {
            let sz = match play.serial.get(si.min(play.serial.len().saturating_sub(1))) {
                None => total,
                Some(Value::String(s)) if s.ends_with('%') => {
                    ((total as f64) * s.trim_end_matches('%').parse::<f64>().unwrap_or(100.0)
                        / 100.0)
                        .ceil()
                        .max(1.0) as usize
                }
                Some(v) => v
                    .as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    .unwrap_or(total as u64)
                    .max(1) as usize,
            };
            let end = (start + sz).min(total);
            batches.push((start..end).collect());
            start = end;
            si += 1;
        }
        let ov = Map::new();
        for batch in &batches {
            if play.strategy_free && batch.len() > 1 {
                // free strategy: every host runs the whole play independently, at its own pace
                let results: Vec<(usize, Host, Result<()>)> = std::thread::scope(|s| {
                    let handles: Vec<_> = batch
                        .iter()
                        .map(|&i| {
                            let host = ps.hosts[i].clone();
                            let (handlers, groups, play_hosts) = (
                                ps.handlers.clone(),
                                ps.groups.clone(),
                                ps.play_hosts.clone(),
                            );
                            let (role_tasks, ov) = (&role_tasks, &ov);
                            s.spawn(move || {
                                let mut mini = PlayState {
                                    play,
                                    hosts: vec![host],
                                    handlers,
                                    groups,
                                    play_hosts,
                                };
                                let r = self.run_batch(&mut mini, &[0], role_tasks, ov);
                                (i, mini.hosts.pop().unwrap(), r)
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| h.join().expect("host thread panicked"))
                        .collect()
                });
                for (i, host, r) in results {
                    ps.hosts[i] = host;
                    r?;
                }
            } else {
                self.run_batch(&mut ps, batch, &role_tasks, &ov)?;
            }
        }
        let mut ok = true;
        let mut st = self.all_stats.lock().unwrap();
        for h in &ps.hosts {
            if h.failed || h.stats.failed > 0 || h.stats.unreachable > 0 {
                ok = ok && h.stats.failed == 0 && h.stats.unreachable == 0 && !h.failed;
            }
            if let Some(e) = st.iter_mut().find(|(n, _)| *n == h.name) {
                let s = &mut e.1;
                s.ok += h.stats.ok;
                s.changed += h.stats.changed;
                s.unreachable += h.stats.unreachable;
                s.failed += h.stats.failed;
                s.skipped += h.stats.skipped;
                s.rescued += h.stats.rescued;
                s.ignored += h.stats.ignored;
            } else {
                st.push((h.name.clone(), h.stats.clone()));
            }
        }
        Ok(ok)
    }

    pub fn recap(&self) -> i32 {
        let st = self.all_stats.lock().unwrap();
        let mut code = 0;
        let mut stats = vec![];
        for (n, s) in st.iter() {
            stats.push(HostStat {
                host: n.clone(),
                ok: s.ok,
                changed: s.changed,
                unreachable: s.unreachable,
                failed: s.failed,
                skipped: s.skipped,
                rescued: s.rescued,
                ignored: s.ignored,
            });
            if s.unreachable > 0 {
                code = code.max(4);
            }
            if s.failed > 0 {
                code = 2;
            }
        }
        self.emit(Event::Recap { stats });
        code
    }
}
