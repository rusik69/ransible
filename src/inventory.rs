//! Inventory loading: INI and YAML, group/host vars, patterns.
use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Default, Debug, Clone)]
pub struct Group {
    pub hosts: BTreeSet<String>,
    pub children: BTreeSet<String>,
    pub vars: Map<String, Value>,
}

#[derive(Default, Debug)]
pub struct Inventory {
    pub groups: indexmap::IndexMap<String, Group>,
    pub host_vars: BTreeMap<String, Map<String, Value>>,
    pub hosts: Vec<String>, // in order of appearance
    pub base_dir: PathBuf,
    pub extra_var_dirs: Vec<PathBuf>, // e.g. playbook dir: group_vars/host_vars next to the playbook
}

fn parse_scalar(s: &str) -> Value {
    let t = s.trim();
    if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
    {
        return Value::String(t[1..t.len() - 1].to_string());
    }
    match t {
        "true" | "True" | "yes" | "Yes" => return Value::Bool(true),
        "false" | "False" | "no" | "No" => return Value::Bool(false),
        _ => {}
    }
    if let Ok(i) = t.parse::<i64>() {
        return Value::from(i);
    }
    if let Ok(f) = t.parse::<f64>() {
        return Value::from(f);
    }
    if (t.starts_with('[') || t.starts_with('{'))
        && let Ok(v) = serde_yaml::from_str::<Value>(t)
    {
        return v;
    }
    Value::String(t.to_string())
}

/// Split a line on whitespace honoring quotes.
fn split_quoted(line: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut q: Option<char> = None;
    let mut depth = 0;
    for c in line.chars() {
        match (q, c) {
            (Some(qc), c) if c == qc => {
                q = None;
                cur.push(c)
            }
            (Some(_), c) => cur.push(c),
            (None, '"') | (None, '\'') => {
                q = Some(c);
                cur.push(c)
            }
            (None, '[') | (None, '{') => {
                depth += 1;
                cur.push(c)
            }
            (None, ']') | (None, '}') => {
                depth -= 1;
                cur.push(c)
            }
            (None, c) if c.is_whitespace() && depth <= 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Expand `web[01:05].example.com` style ranges.
pub fn expand_range(pat: &str) -> Vec<String> {
    let (Some(s), Some(e)) = (pat.find('['), pat.find(']')) else {
        return vec![pat.to_string()];
    };
    if e < s {
        return vec![pat.to_string()];
    }
    let inner = &pat[s + 1..e];
    let parts: Vec<&str> = inner.split(':').collect();
    if parts.len() < 2 {
        return vec![pat.to_string()];
    }
    let (pre, post) = (&pat[..s], &pat[e + 1..]);
    let step: usize = parts.get(2).and_then(|x| x.parse().ok()).unwrap_or(1);
    let mut items: Vec<String> = vec![];
    if let (Ok(a), Ok(b)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
        let width = if parts[0].starts_with('0') {
            parts[0].len()
        } else {
            0
        };
        let mut i = a;
        while i <= b {
            items.push(format!("{i:0width$}"));
            i += step as u32;
        }
    } else if parts[0].len() == 1 && parts[1].len() == 1 {
        let (a, b) = (parts[0].as_bytes()[0], parts[1].as_bytes()[0]);
        let mut i = a;
        while i <= b {
            items.push((i as char).to_string());
            i += step as u8;
        }
    } else {
        return vec![pat.to_string()];
    }
    items
        .into_iter()
        .flat_map(|m| expand_range(&format!("{pre}{m}{post}")))
        .collect()
}

impl Inventory {
    pub fn load(path: &str) -> Result<Inventory> {
        let p = Path::new(path);
        let mut inv = Inventory::default();
        inv.groups.insert("all".into(), Group::default());
        inv.groups.insert("ungrouped".into(), Group::default());
        if p.is_dir() {
            inv.base_dir = p.to_path_buf();
            let mut files: Vec<_> = std::fs::read_dir(p)?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .collect();
            files.sort();
            for f in files {
                if !f.is_file() {
                    continue;
                }
                let name = f.file_name().unwrap().to_string_lossy().to_string();
                if name.starts_with('.') || name.ends_with('~') || name.ends_with(".retry") {
                    continue;
                }
                inv.load_file(&f)?;
            }
        } else if p.exists() {
            inv.base_dir = p.parent().map(|x| x.to_path_buf()).unwrap_or_default();
            inv.load_file(p)?;
        } else if path.contains(',') || !path.is_empty() && !path.contains('/') && !p.exists() {
            // inline host list "a,b,"
            for h in path.split(',').filter(|h| !h.is_empty()) {
                inv.add_host(h, "ungrouped");
            }
        } else {
            return Err(anyhow!("inventory '{path}' not found"));
        }
        inv.finalize();
        Ok(inv)
    }

    /// Runtime `add_host`.
    pub fn add_dynamic(&mut self, host: &str, groups: &[String], vars: Map<String, Value>) {
        if groups.is_empty() {
            self.add_host(host, "ungrouped");
        }
        for g in groups {
            self.add_host(host, g);
        }
        self.host_vars
            .entry(host.to_string())
            .or_default()
            .extend(vars);
        self.finalize();
    }

    /// Runtime `group_by`.
    pub fn add_to_group(&mut self, host: &str, group: &str) {
        self.add_host(host, group);
        self.finalize();
    }

    pub fn empty_localhost() -> Inventory {
        let mut inv = Inventory::default();
        inv.groups.insert("all".into(), Group::default());
        inv.groups.insert("ungrouped".into(), Group::default());
        inv.add_host("localhost", "ungrouped");
        inv.host_vars
            .entry("localhost".into())
            .or_default()
            .insert("ansible_connection".into(), "local".into());
        inv.finalize();
        inv
    }

    fn add_host(&mut self, host: &str, group: &str) {
        if !self.hosts.iter().any(|h| h == host) {
            self.hosts.push(host.to_string());
        }
        self.groups
            .entry(group.to_string())
            .or_default()
            .hosts
            .insert(host.to_string());
        self.host_vars.entry(host.to_string()).or_default();
    }

    /// Executable inventory script: `script --list` JSON (groups, `_meta.hostvars`).
    #[cfg_attr(not(unix), allow(dead_code))]
    fn load_script(&mut self, f: &Path) -> Result<()> {
        let out = std::process::Command::new(f)
            .arg("--list")
            .output()
            .with_context(|| format!("running inventory script {}", f.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "inventory script {} failed: {}",
                f.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let v: Value = serde_json::from_slice(&out.stdout).with_context(|| {
            format!("inventory script {} did not print valid JSON", f.display())
        })?;
        let Value::Object(m) = v else {
            return Err(anyhow!("inventory script output must be a JSON object"));
        };
        let hostvars = m
            .get("_meta")
            .and_then(|x| x.get("hostvars"))
            .and_then(|x| x.as_object())
            .cloned()
            .unwrap_or_default();
        for (g, body) in &m {
            if g == "_meta" {
                continue;
            }
            self.groups.entry(g.clone()).or_default();
            match body {
                Value::Array(hosts) => {
                    for h in hosts.iter().filter_map(|h| h.as_str()) {
                        self.add_host(h, g);
                    }
                }
                Value::Object(b) => {
                    for h in b
                        .get("hosts")
                        .and_then(|h| h.as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|h| h.as_str())
                    {
                        self.add_host(h, g);
                    }
                    if let Some(Value::Object(vars)) = b.get("vars") {
                        self.groups.get_mut(g).unwrap().vars.extend(vars.clone());
                    }
                    for c in b
                        .get("children")
                        .and_then(|h| h.as_array())
                        .into_iter()
                        .flatten()
                        .filter_map(|h| h.as_str())
                    {
                        self.groups.entry(c.to_string()).or_default();
                        self.groups
                            .get_mut(g)
                            .unwrap()
                            .children
                            .insert(c.to_string());
                    }
                }
                _ => {}
            }
        }
        for (h, vars) in hostvars {
            if let Value::Object(vm) = vars {
                if !self.hosts.contains(&h) {
                    self.add_host(&h, "ungrouped");
                }
                self.host_vars.entry(h).or_default().extend(vm);
            }
        }
        Ok(())
    }

    fn load_file(&mut self, f: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if f.metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
            {
                return self.load_script(f);
            }
        }
        let text =
            std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
        let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("");
        let looks_yaml =
            matches!(ext, "yml" | "yaml" | "json") || text.trim_start().starts_with("---") || {
                // `all:` style top-level key
                text.lines()
                    .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
                    .map(|l| l.trim_end().ends_with(':') && !l.starts_with('['))
                    .unwrap_or(false)
            };
        if looks_yaml {
            let v: Value = crate::playbook::parse_yaml_str(&text)
                .with_context(|| format!("parsing {}", f.display()))?;
            if let Value::Object(m) = v {
                for (g, body) in m {
                    self.yaml_group(&g, &body, None)?;
                }
            }
            Ok(())
        } else {
            self.load_ini(&text)
        }
    }

    fn yaml_group(&mut self, name: &str, body: &Value, parent: Option<&str>) -> Result<()> {
        self.groups.entry(name.to_string()).or_default();
        if let Some(p) = parent {
            self.groups
                .get_mut(p)
                .unwrap()
                .children
                .insert(name.to_string());
        }
        let Value::Object(b) = body else {
            return Ok(());
        };
        if let Some(Value::Object(hosts)) = b.get("hosts") {
            for (h, vars) in hosts {
                for hn in expand_range(h) {
                    self.add_host(&hn, name);
                    if let Value::Object(vm) = vars {
                        self.host_vars.get_mut(&hn).unwrap().extend(vm.clone());
                    }
                }
            }
        }
        if let Some(Value::Object(vars)) = b.get("vars") {
            self.groups.get_mut(name).unwrap().vars.extend(vars.clone());
        }
        if let Some(Value::Object(ch)) = b.get("children") {
            for (c, cb) in ch {
                self.yaml_group(c, cb, Some(name))?;
            }
        }
        Ok(())
    }

    fn load_ini(&mut self, text: &str) -> Result<()> {
        #[derive(PartialEq)]
        enum Kind {
            Hosts,
            Children,
            Vars,
        }
        let mut cur = ("ungrouped".to_string(), Kind::Hosts);
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                let sec = &line[1..line.len() - 1];
                cur = if let Some(g) = sec.strip_suffix(":children") {
                    (g.to_string(), Kind::Children)
                } else if let Some(g) = sec.strip_suffix(":vars") {
                    (g.to_string(), Kind::Vars)
                } else {
                    (sec.to_string(), Kind::Hosts)
                };
                self.groups.entry(cur.0.clone()).or_default();
                continue;
            }
            match cur.1 {
                Kind::Hosts => {
                    let toks = split_quoted(line);
                    let Some(first) = toks.first() else { continue };
                    for hn in expand_range(first) {
                        self.add_host(&hn, &cur.0);
                        for t in &toks[1..] {
                            if let Some((k, v)) = t.split_once('=') {
                                self.host_vars
                                    .get_mut(&hn)
                                    .unwrap()
                                    .insert(k.to_string(), parse_scalar(v));
                            }
                        }
                    }
                }
                Kind::Children => {
                    self.groups.entry(line.to_string()).or_default();
                    self.groups
                        .get_mut(&cur.0)
                        .unwrap()
                        .children
                        .insert(line.to_string());
                }
                Kind::Vars => {
                    let (k, v) = line
                        .split_once('=')
                        .ok_or_else(|| anyhow!("inventory line {}: bad var '{line}'", n + 1))?;
                    self.groups
                        .get_mut(&cur.0)
                        .unwrap()
                        .vars
                        .insert(k.trim().to_string(), parse_scalar(v));
                }
            }
        }
        Ok(())
    }

    fn finalize(&mut self) {
        // all group contains every host; ungrouped = hosts only in no other group
        let all: BTreeSet<String> = self.hosts.iter().cloned().collect();
        let mut grouped = BTreeSet::new();
        for (n, _) in self.groups.clone() {
            if n == "all" || n == "ungrouped" {
                continue;
            }
            grouped.extend(self.group_hosts(&n));
        }
        self.groups.get_mut("all").unwrap().hosts = all.clone();
        let ug = self.groups.get_mut("ungrouped").unwrap();
        ug.hosts = all.difference(&grouped).cloned().collect();
    }

    /// All hosts in a group, including children (recursive).
    pub fn group_hosts(&self, g: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut seen = BTreeSet::new();
        self.collect(g, &mut out, &mut seen);
        out
    }

    fn collect(&self, g: &str, out: &mut BTreeSet<String>, seen: &mut BTreeSet<String>) {
        if !seen.insert(g.to_string()) {
            return;
        }
        if let Some(gr) = self.groups.get(g) {
            out.extend(gr.hosts.iter().cloned());
            for c in &gr.children {
                self.collect(c, out, seen);
            }
        }
    }

    /// Groups a host belongs to (direct + via children), ordered parents-first for var precedence.
    pub fn groups_of(&self, host: &str) -> Vec<String> {
        let mut gs: Vec<String> = self
            .groups
            .keys()
            .filter(|g| g.as_str() != "all" && self.group_hosts(g).contains(host))
            .cloned()
            .collect();
        // depth ordering: groups with more hosts (parents) first, then name
        gs.sort_by(|a, b| {
            self.group_hosts(b)
                .len()
                .cmp(&self.group_hosts(a).len())
                .then(a.cmp(b))
        });
        gs
    }

    /// Merged inventory vars for a host (all < parents < children < host).
    pub fn vars_for(&self, host: &str) -> Map<String, Value> {
        let mut out = Map::new();
        let read_dir_vars = |kind: &str, name: &str, out: &mut Map<String, Value>| {
            for base in std::iter::once(&self.base_dir).chain(self.extra_var_dirs.iter()) {
                let d = base.join(kind);
                for cand in [
                    d.join(name),
                    d.join(format!("{name}.yml")),
                    d.join(format!("{name}.yaml")),
                    d.join(format!("{name}.json")),
                ] {
                    if cand.is_file() {
                        load_vars_file(&cand, out);
                    } else if cand.is_dir() {
                        let mut fs: Vec<_> = walk_files(&cand);
                        fs.sort();
                        for f in fs {
                            load_vars_file(&f, out);
                        }
                    }
                }
            }
        };
        // inventory-file group vars < group_vars/all < group_vars/* < inline host vars < host_vars/*
        let mut order = vec!["all".to_string()];
        order.extend(self.groups_of(host));
        for g in &order {
            if let Some(gr) = self.groups.get(g) {
                out.extend(gr.vars.clone());
            }
        }
        for g in &order {
            read_dir_vars("group_vars", g, &mut out);
        }
        if let Some(hv) = self.host_vars.get(host) {
            out.extend(hv.clone());
        }
        read_dir_vars("host_vars", host, &mut out);
        out
    }

    /// Resolve a host pattern like `web:&prod:!db`, `all`, `web*`.
    pub fn select(&self, pattern: &str) -> Vec<String> {
        let mut sel: Vec<String> = vec![];
        let norm = pattern.replace(',', ":");
        for (i, part) in norm.split(':').filter(|p| !p.is_empty()).enumerate() {
            let (op, name) = match part.chars().next() {
                Some('&') => ('&', &part[1..]),
                Some('!') => ('!', &part[1..]),
                _ => ('+', part),
            };
            let matched = self.match_name(name);
            match op {
                '+' => {
                    for h in matched {
                        if !sel.contains(&h) {
                            sel.push(h);
                        }
                    }
                }
                '&' => sel.retain(|h| matched.contains(h)),
                '!' => sel.retain(|h| !matched.contains(h)),
                _ => {}
            }
            let _ = i;
        }
        // keep inventory order
        let order: BTreeMap<&String, usize> =
            self.hosts.iter().enumerate().map(|(i, h)| (h, i)).collect();
        sel.sort_by_key(|h| order.get(h).copied().unwrap_or(usize::MAX));
        sel
    }

    fn match_name(&self, name: &str) -> Vec<String> {
        if name == "all" || name == "*" {
            return self.hosts.clone();
        }
        // group[idx] subscript not supported; plain group, host, or glob
        if self.groups.contains_key(name) {
            let hs = self.group_hosts(name);
            return self
                .hosts
                .iter()
                .filter(|h| hs.contains(*h))
                .cloned()
                .collect();
        }
        if let Some(rest) = name.strip_prefix('~')
            && let Ok(re) = regex::Regex::new(rest)
        {
            return self
                .hosts
                .iter()
                .filter(|h| re.is_match(h))
                .cloned()
                .collect();
        }
        if name.contains('*') || name.contains('?') {
            let re = format!(
                "^{}$",
                regex::escape(name).replace("\\*", ".*").replace("\\?", ".")
            );
            if let Ok(re) = regex::Regex::new(&re) {
                let mut v: Vec<String> = self
                    .groups
                    .iter()
                    .filter(|(g, _)| re.is_match(g))
                    .flat_map(|(g, _)| self.group_hosts(g))
                    .collect();
                v.extend(self.hosts.iter().filter(|h| re.is_match(h)).cloned());
                v.sort();
                v.dedup();
                return v;
            }
        }
        if self.hosts.iter().any(|h| h == name) {
            return vec![name.to_string()];
        }
        vec![]
    }
}

fn walk_files(d: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(d) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk_files(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// Load a YAML/JSON (optionally vault-encrypted) vars file into `out`.
pub fn try_load_vars_file(p: &Path, out: &mut Map<String, Value>) -> Result<()> {
    let t = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    match crate::playbook::parse_yaml_str(&t)
        .with_context(|| format!("loading vars file {}", p.display()))?
    {
        Value::Object(m) => out.extend(m),
        Value::Null => {}
        _ => return Err(anyhow!("vars file {} must be a mapping", p.display())),
    }
    Ok(())
}

/// Lenient variant for inventory directories (group_vars/host_vars): warns and continues.
pub fn load_vars_file(p: &Path, out: &mut Map<String, Value>) {
    if let Err(e) = try_load_vars_file(p, out) {
        eprintln!("[WARNING]: {e:#}");
    }
}

impl Inventory {
    /// Groups that are not a child of another group, in definition order.
    fn top_level_groups(&self) -> Vec<String> {
        let children: std::collections::HashSet<&String> = self
            .groups
            .values()
            .flat_map(|g| g.children.iter())
            .collect();
        self.groups
            .keys()
            .filter(|g| g.as_str() != "all" && g.as_str() != "ungrouped" && !children.contains(g))
            .cloned()
            .collect()
    }

    fn ordered_hosts(&self, g: &Group) -> Vec<String> {
        self.hosts
            .iter()
            .filter(|h| g.hosts.contains(*h))
            .cloned()
            .collect()
    }

    /// `ansible-inventory --list` (legacy profile) as JSON.
    pub fn to_list_json(&self) -> Value {
        let mut out = Map::new();
        let mut hostvars = Map::new();
        for h in &self.hosts {
            hostvars.insert(h.clone(), Value::Object(self.vars_for(h)));
        }
        let mut meta = Map::new();
        meta.insert("hostvars".into(), Value::Object(hostvars));
        meta.insert("profile".into(), "inventory_legacy".into());
        out.insert("_meta".into(), Value::Object(meta));
        let mut all_children: Vec<Value> = vec!["ungrouped".into()];
        all_children.extend(self.top_level_groups().into_iter().map(Value::from));
        out.insert(
            "all".into(),
            serde_json::json!({ "children": all_children }),
        );
        for (name, g) in &self.groups {
            if name == "all" {
                continue;
            }
            let mut m = Map::new();
            let hosts = if name == "ungrouped" {
                self.ordered_hosts(g)
            } else {
                self.ordered_hosts(g)
            };
            if !hosts.is_empty() {
                m.insert(
                    "hosts".into(),
                    Value::Array(hosts.into_iter().map(Value::from).collect()),
                );
            }
            if !g.children.is_empty() {
                let kids: Vec<Value> = self
                    .groups
                    .keys()
                    .filter(|k| g.children.contains(*k))
                    .cloned()
                    .map(Value::from)
                    .collect();
                m.insert("children".into(), Value::Array(kids));
            }
            if !m.is_empty() {
                out.insert(name.clone(), Value::Object(m));
            }
        }
        Value::Object(out)
    }

    /// `ansible-inventory --graph`.
    pub fn to_graph(&self) -> String {
        fn rec(inv: &Inventory, name: &str, prefix: &str, out: &mut String, top: bool) {
            let g = inv.groups.get(name).cloned().unwrap_or_default();
            out.push_str(&format!(
                "{prefix}{}@{name}:\n",
                if top { "" } else { "|--" }
            ));
            let child_prefix = if top {
                "  ".to_string()
            } else {
                format!("{}|  ", prefix.replace("|--", ""))
            };
            let kids: Vec<String> = if name == "all" {
                let mut k = vec!["ungrouped".to_string()];
                k.extend(inv.top_level_groups());
                k
            } else {
                inv.groups
                    .keys()
                    .filter(|k| g.children.contains(*k))
                    .cloned()
                    .collect()
            };
            for k in kids {
                rec(inv, &k, &child_prefix, out, false);
            }
            if name != "all" {
                for h in inv.ordered_hosts(&g) {
                    out.push_str(&format!("{child_prefix}|--{h}\n"));
                }
            }
        }
        let mut out = String::new();
        rec(self, "all", "", &mut out, true);
        out
    }
}
