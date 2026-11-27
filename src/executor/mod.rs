//! Play / task execution engine (linear strategy).
use crate::conn::Conn;
use crate::events::{Event, HostStat, Sink};
use crate::inventory::Inventory;
use crate::modules::{self, Ctx};
use crate::playbook::{Play, Role, Task, apply_inherit, load_role, loose_bool, read_yaml};
use crate::template::{Templar, Vars};
use anyhow::{Result, anyhow};
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

mod flow;
mod play;
mod tasks;
mod vars;

#[derive(Default, Clone)]
pub struct Opts {
    pub check: bool,
    pub diff: bool,
    #[allow(dead_code)]
    pub verbose: u8,
    pub tags: Vec<String>,
    pub skip_tags: Vec<String>,
    pub extra_vars: Map<String, Value>,
    pub limit: Option<String>,
    pub forks: usize,
    pub list_tasks: bool,
    pub list_hosts: bool,
    pub list_tags: bool,
    pub start_at_task: Option<String>,
}

#[derive(Default, Clone)]
pub struct Stats {
    pub ok: u32,
    pub changed: u32,
    pub unreachable: u32,
    pub failed: u32,
    pub skipped: u32,
    pub rescued: u32,
    pub ignored: u32,
}

#[derive(Clone)]
pub struct Host {
    pub name: String,
    base: Vars,   // role defaults < inventory vars
    facts: Vars,  // gathered facts (setup, package_facts, ...)
    layer: Vars,  // play vars < vars_files < role vars
    params: Vars, // role params / include_role vars (above set_fact)
    dynv: Vars,   // set_fact / register / include_vars
    failed: bool,
    ended: bool,
    notified: BTreeSet<String>,
    last_fail: Option<(String, Value)>,
    pub stats: Stats,
}

pub struct Runner {
    pub inv: std::sync::RwLock<Inventory>,
    pub opts: Opts,
    pub templar: Templar,
    pub sink: std::sync::Arc<dyn Sink>,
    pub need_hostvars: bool,
    pub playbook_dir: PathBuf,
    pub all_stats: std::sync::Mutex<Vec<(String, Stats)>>,
    pub started: std::sync::atomic::AtomicBool,
}

impl PlayState<'_> {
    /// Recompute `ansible_play_hosts` (hosts that have not failed or ended).
    fn refresh_active(&mut self) {
        let active: Vec<Value> = self
            .hosts
            .iter()
            .filter(|h| !h.failed && !h.ended)
            .map(|h| Value::from(h.name.clone()))
            .collect();
        self.play_hosts["active"] = Value::Array(active);
    }
}

struct PlayState<'p> {
    play: &'p Play,
    hosts: Vec<Host>,
    handlers: Vec<Task>,
    groups: Value,
    play_hosts: Value,
}

struct TaskEnv<'a> {
    play: &'a Play,
    hostvars: Option<&'a Value>,
    groups: &'a Value,
    play_hosts: &'a Value,
    overlay: &'a Map<String, Value>,
}

#[derive(Default)]
struct TaskRun {
    events: Vec<Event>,
    touched: Vec<String>,
}

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    Ok,
    Changed,
    Failed,
    Skipped,
    Unreachable,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Ok => "ok",
            Kind::Changed => "changed",
            Kind::Failed => "failed",
            Kind::Skipped => "skipped",
            Kind::Unreachable => "unreachable",
        }
    }
}

struct ItemOut {
    res: Map<String, Value>,
    kind: Kind,
}

fn tpl(v: &Value) -> bool {
    match v {
        Value::String(s) => s.contains("{{") || s.contains("{%"),
        Value::Array(a) => a.iter().any(tpl),
        Value::Object(o) => o.values().any(tpl),
        _ => false,
    }
}

fn to_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| x.to_string())
            })
            .collect(),
        Some(Value::String(s)) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => vec![],
    }
}

fn vstr(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        o => o.to_string(),
    }
}

impl Runner {
    fn emit(&self, e: Event) {
        self.sink.emit(&e);
    }

    fn banner(&self, kind: &str, name: &str) {
        self.emit(Event::Banner {
            kind: kind.to_string(),
            name: name.to_string(),
            role: None,
        });
    }

    fn msg(&self, text: impl Into<String>) {
        self.emit(Event::Message { text: text.into() });
    }
}
