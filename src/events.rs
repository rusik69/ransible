//! Structured run events and output sinks.
//!
//! The executor emits [`Event`]s; sinks render them. `ConsoleSink` reproduces Ansible's default
//! callback output, `JsonSink` writes one JSON object per line (for the web UI / API / `-o json`).
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HostStat {
    pub host: String,
    pub ok: u32,
    pub changed: u32,
    pub unreachable: u32,
    pub failed: u32,
    pub skipped: u32,
    pub rescued: u32,
    pub ignored: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    PlaybookStart {
        playbook: String,
    },
    /// `kind` is PLAY | TASK | RUNNING HANDLER
    Banner {
        kind: String,
        name: String,
        role: Option<String>,
    },
    HostResult {
        host: String,
        /// ok | changed | failed | skipped | unreachable
        status: String,
        module: String,
        item: Option<String>,
        ignored: bool,
        result: Value,
    },
    /// Free-form line (list modes, notices)
    Message {
        text: String,
    },
    Recap {
        stats: Vec<HostStat>,
    },
    PlaybookEnd {
        rc: i32,
    },
}

pub trait Sink: Send + Sync {
    fn emit(&self, e: &Event);
}

/// Fan out to several sinks (used by the server: DB + websocket + console).
#[allow(dead_code)]
pub struct MultiSink(pub Vec<Arc<dyn Sink>>);

impl Sink for MultiSink {
    fn emit(&self, e: &Event) {
        for s in &self.0 {
            s.emit(e);
        }
    }
}

/// Collects events in memory (tests, API).
#[derive(Default)]
#[allow(dead_code)]
pub struct MemorySink(pub Mutex<Vec<Event>>);

impl Sink for MemorySink {
    fn emit(&self, e: &Event) {
        self.0.lock().unwrap().push(e.clone());
    }
}

/// JSON lines.
pub struct JsonSink<W: Write + Send>(pub Mutex<W>);

impl<W: Write + Send> Sink for JsonSink<W> {
    fn emit(&self, e: &Event) {
        if let Ok(mut w) = self.0.lock() {
            let _ = serde_json::to_writer(&mut *w, e);
            let _ = w.write_all(b"\n");
            let _ = w.flush();
        }
    }
}

pub struct ConsoleSink {
    pub color: bool,
    pub verbose: u8,
}

impl ConsoleSink {
    fn paint(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    fn banner(&self, kind: &str, name: &str, role: Option<&str>) -> String {
        let name = match role {
            Some(r) if kind == "TASK" => format!("{r} : {name}"),
            _ => name.to_string(),
        };
        let head = if name.is_empty() {
            format!("{kind} ")
        } else {
            format!("{kind} [{name}] ")
        };
        let pad = 79usize.saturating_sub(head.chars().count());
        format!("\n{}{}\n", head, "*".repeat(pad.max(3)))
    }

    fn fmt_result(
        &self,
        host: &str,
        status: &str,
        res: &Value,
        module: &str,
        item: Option<&str>,
        ignore: bool,
    ) -> String {
        let label = item.map(|i| format!(" (item={i})")).unwrap_or_default();
        let obj = res.as_object().cloned().unwrap_or_default();
        let compact = |r: &Map<String, Value>| pyjson(&Value::Object(r.clone()));
        match status {
            "ok" | "changed" => {
                let (code, word) = if status == "ok" {
                    ("32", "ok")
                } else {
                    ("33", "changed")
                };
                let head = self.paint(code, &format!("{word}: [{host}]{label}"));
                if module == "debug" {
                    let mut r = obj.clone();
                    for k in ["changed", "item", "ansible_loop_var"] {
                        r.remove(k);
                    }
                    format!("{head} => {}", pretty4(&Value::Object(r)))
                } else if self.verbose > 0 {
                    format!("{head} => {}", compact(&obj))
                } else {
                    head
                }
            }
            "skipped" => self.paint("36", &format!("skipping: [{host}]{label}")),
            "unreachable" => self.paint(
                "31",
                &format!("fatal: [{host}]: UNREACHABLE! => {}", compact(&obj)),
            ),
            _ => {
                let mut r = obj;
                r.remove("ansible_loop_var");
                r.remove("failed");
                let s = self.paint(
                    "31",
                    &format!("fatal: [{host}]: FAILED!{label} => {}", compact(&r)),
                );
                if ignore {
                    format!("{s}\n{}", self.paint("36", "...ignoring"))
                } else {
                    s
                }
            }
        }
    }
}

impl ConsoleSink {
    /// Render an event to text (what the console prints; also used by the server for job output).
    pub fn render(&self, e: &Event) -> String {
        match e {
            Event::PlaybookStart { .. } | Event::PlaybookEnd { .. } => String::new(),
            Event::Banner { kind, name, role } => self.banner(kind, name, role.as_deref()),
            Event::HostResult {
                host,
                status,
                module,
                item,
                ignored,
                result,
            } => {
                format!(
                    "{}\n",
                    self.fmt_result(host, status, result, module, item.as_deref(), *ignored)
                )
            }
            Event::Message { text } => format!("{text}\n"),
            Event::Recap { stats } => {
                let mut out = self.banner("PLAY RECAP", "", None);
                let w = stats.iter().map(|s| s.host.len()).max().unwrap_or(0);
                for s in stats {
                    let c = |code: &str, k: &str, v: u32| {
                        if v > 0 {
                            self.paint(code, &format!("{k}={v:<4}"))
                        } else {
                            format!("{k}={v:<4}")
                        }
                    };
                    out += &format!(
                        "{:<w$} : {} {} {} {} {} {} {}\n",
                        s.host,
                        self.paint("32", &format!("ok={:<4}", s.ok)),
                        c("33", "changed", s.changed),
                        c("31", "unreachable", s.unreachable),
                        c("31", "failed", s.failed),
                        c("36", "skipped", s.skipped),
                        format_args!("rescued={:<4}", s.rescued),
                        format_args!("ignored={:<4}", s.ignored),
                    );
                }
                out.push('\n');
                out
            }
        }
    }
}

impl Sink for ConsoleSink {
    fn emit(&self, e: &Event) {
        print!("{}", self.render(e));
    }
}

/// JSON with 4-space indentation (like Ansible's debug output).
pub fn pretty4(v: &Value) -> String {
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    let _ = serde::Serialize::serialize(v, &mut ser);
    String::from_utf8(buf).unwrap_or_default()
}

/// JSON with Python-style separators (", " and ": "), like Ansible's default callback.
pub fn pyjson(v: &Value) -> String {
    struct Py;
    impl serde_json::ser::Formatter for Py {
        fn begin_array_value<W: ?Sized + std::io::Write>(
            &mut self,
            w: &mut W,
            first: bool,
        ) -> std::io::Result<()> {
            if first { Ok(()) } else { w.write_all(b", ") }
        }
        fn begin_object_key<W: ?Sized + std::io::Write>(
            &mut self,
            w: &mut W,
            first: bool,
        ) -> std::io::Result<()> {
            if first { Ok(()) } else { w.write_all(b", ") }
        }
        fn begin_object_value<W: ?Sized + std::io::Write>(
            &mut self,
            w: &mut W,
        ) -> std::io::Result<()> {
            w.write_all(b": ")
        }
    }
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, Py);
    let _ = serde::Serialize::serialize(v, &mut ser);
    String::from_utf8(buf).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pyjson_uses_python_separators() {
        assert_eq!(
            pyjson(&serde_json::json!({"a": [1, 2], "b": "x"})),
            r#"{"a": [1, 2], "b": "x"}"#
        );
    }

    #[test]
    fn events_serialize_with_tag() {
        let e = Event::Banner {
            kind: "TASK".into(),
            name: "x".into(),
            role: None,
        };
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(j["event"], "banner");
        assert_eq!(j["kind"], "TASK");
    }

    #[test]
    fn memory_sink_collects() {
        let m = MemorySink::default();
        m.emit(&Event::Message { text: "hi".into() });
        assert_eq!(m.0.lock().unwrap().len(), 1);
    }
}
