//! Output formats matching Python's `json.dumps` and PyYAML's `yaml.dump` as used by Ansible's
//! `to_json`, `to_nice_json`, `to_yaml` and `to_nice_yaml` filters.
use serde_json::Value;

thread_local! {
    static LEAF_FLOW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn sorted_keys(o: &serde_json::Map<String, Value>, sort: bool) -> Vec<&String> {
    let mut k: Vec<&String> = o.keys().collect();
    if sort {
        k.sort();
    }
    k
}

fn json_str(s: &str, ascii: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ascii && (c as u32) > 0x7e => {
                let mut b = [0u16; 2];
                for u in c.encode_utf16(&mut b) {
                    out.push_str(&format!("\\u{:04x}", u));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    let s = format!("{f}");
    if s.contains('.') || s.contains('e') {
        s
    } else {
        format!("{s}.0")
    }
}

/// Python `json.dumps(v, indent=indent, sort_keys=sort, ensure_ascii=ascii)`.
/// `indent: None` uses the compact-with-spaces style `{"a": 1, "b": [1, 2]}`.
pub fn json_dumps(v: &Value, indent: Option<usize>, sort: bool, ascii: bool) -> String {
    fn go(v: &Value, ind: Option<usize>, depth: usize, sort: bool, ascii: bool, out: &mut String) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => match n.as_f64() {
                Some(f) if n.is_f64() => out.push_str(&float_repr(f)),
                _ => out.push_str(&n.to_string()),
            },
            Value::String(s) => out.push_str(&json_str(s, ascii)),
            Value::Array(a) if a.is_empty() => out.push_str("[]"),
            Value::Object(o) if o.is_empty() => out.push_str("{}"),
            Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    match ind {
                        Some(n) => {
                            if i > 0 {
                                out.push(',');
                            }
                            out.push('\n');
                            out.push_str(&" ".repeat(n * (depth + 1)));
                        }
                        None if i > 0 => out.push_str(", "),
                        None => {}
                    }
                    go(x, ind, depth + 1, sort, ascii, out);
                }
                if let Some(n) = ind {
                    out.push('\n');
                    out.push_str(&" ".repeat(n * depth));
                }
                out.push(']');
            }
            Value::Object(o) => {
                out.push('{');
                for (i, k) in sorted_keys(o, sort).into_iter().enumerate() {
                    match ind {
                        Some(n) => {
                            if i > 0 {
                                out.push(',');
                            }
                            out.push('\n');
                            out.push_str(&" ".repeat(n * (depth + 1)));
                        }
                        None if i > 0 => out.push_str(", "),
                        None => {}
                    }
                    out.push_str(&json_str(k, ascii));
                    out.push_str(": ");
                    go(&o[k], ind, depth + 1, sort, ascii, out);
                }
                if let Some(n) = ind {
                    out.push('\n');
                    out.push_str(&" ".repeat(n * depth));
                }
                out.push('}');
            }
        }
    }
    let mut out = String::new();
    go(v, indent, 0, sort, ascii, &mut out);
    out
}

// ---------- YAML ----------

fn looks_like_other_type(s: &str) -> bool {
    let l = s.to_lowercase();
    if matches!(
        l.as_str(),
        "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "~" | "=" | "<<"
    ) {
        return true;
    }
    let num = regex::Regex::new(
        r"^[-+]?(0b[0-1_]+|0[0-7_]+|(0|[1-9][0-9_]*)|0x[0-9a-fA-F_]+|[1-9][0-9_]*(:[0-5]?[0-9])+)$",
    )
    .unwrap();
    let flt = regex::Regex::new(r"^([-+]?([0-9][0-9_]*)\.[0-9_]*([eE][-+][0-9]+)?|\.[0-9_]+([eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(:[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(inf|Inf|INF)|\.(nan|NaN|NAN))$").unwrap();
    let date = regex::Regex::new(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}([Tt ]+[0-9]{1,2}:[0-9]{2}:[0-9]{2}(\.[0-9]*)?( ?(Z|[-+][0-9]{1,2}(:[0-9]{2})?))?)?$").unwrap();
    num.is_match(s) || flt.is_match(s) || date.is_match(s)
}

/// Can `s` be written as a plain (unquoted) scalar the way PyYAML decides?
fn plain_ok(s: &str) -> bool {
    if s.is_empty()
        || s.starts_with(' ')
        || s.ends_with(' ')
        || s.contains('\n')
        || s.contains('\r')
    {
        return false;
    }
    if looks_like_other_type(s) {
        return false;
    }
    let first = s.chars().next().unwrap();
    if "-?:,[]{}#&*!|>'\"%@`".contains(first)
        && !(matches!(first, '-' | '?' | ':')
            && s.chars()
                .nth(1)
                .map(|c| !c.is_whitespace())
                .unwrap_or(false))
    {
        return false;
    }
    if s.starts_with("---") || s.starts_with("...") {
        return false;
    }
    if s.contains(": ") || s.contains(" #") || s.ends_with(':') {
        return false;
    }
    if s.chars()
        .any(|c| (c as u32) < 0x20 || c == '\u{7f}' || c == '\u{feff}')
    {
        return false;
    }
    // flow indicators inside plain scalars are fine in block context
    true
}

fn yaml_scalar(s: &str) -> String {
    if plain_ok(s) {
        return s.to_string();
    }
    let needs_double = s.contains('\n')
        || s.contains('\r')
        || s.chars()
            .any(|c| (c as u32) < 0x20 && c != '\t' || c == '\u{7f}');
    if needs_double {
        let mut out = String::from('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02X}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        return out;
    }
    format!("'{}'", s.replace('\'', "''"))
}

fn yaml_leaf(v: &Value) -> Option<String> {
    Some(match v {
        Value::Null => "null".into(),
        Value::Bool(b) => (if *b { "true" } else { "false" }).into(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() => {
                let r = float_repr(f).to_lowercase();
                if f.is_nan() {
                    ".nan".into()
                } else if f.is_infinite() {
                    if f > 0.0 {
                        ".inf".into()
                    } else {
                        "-.inf".into()
                    }
                } else {
                    r
                }
            }
            _ => n.to_string(),
        },
        Value::String(s) => yaml_scalar(s),
        Value::Array(a) if a.is_empty() => "[]".into(),
        Value::Object(o) if o.is_empty() => "{}".into(),
        _ => return None,
    })
}

/// PyYAML `yaml.dump(v, indent=indent, default_flow_style=False, allow_unicode=True)` (keys sorted).
pub fn yaml_dump(v: &Value, indent: usize, sort: bool) -> String {
    yaml_dump_opts(v, indent, sort, false)
}

/// Collections made only of scalars (PyYAML `default_flow_style=None` writes them in flow style).
fn is_leaf_collection(v: &Value) -> bool {
    match v {
        Value::Array(a) => {
            !a.is_empty()
                && a.iter()
                    .all(|x| !matches!(x, Value::Array(_) | Value::Object(_)))
        }
        Value::Object(o) => {
            !o.is_empty()
                && o.values()
                    .all(|x| !matches!(x, Value::Array(_) | Value::Object(_)))
        }
        _ => false,
    }
}

fn flow_scalar(v: &Value) -> String {
    match v {
        Value::String(s) if s.chars().any(|c| ",[]{}".contains(c)) && plain_ok(s) => {
            format!("'{}'", s.replace('\'', "''"))
        }
        other => yaml_leaf(other).unwrap_or_default(),
    }
}

fn flow(v: &Value, sort: bool) -> String {
    match v {
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(flow_scalar).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(o) => format!(
            "{{{}}}",
            sorted_keys(o, sort)
                .into_iter()
                .map(|k| format!(
                    "{}: {}",
                    flow_scalar(&Value::String(k.clone())),
                    flow_scalar(&o[k])
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => flow_scalar(other),
    }
}

/// `yaml.dump` with `default_flow_style=False` (`leaf_flow = false`) or `None` (`true`).
pub fn yaml_dump_opts(v: &Value, indent: usize, sort: bool, leaf_flow: bool) -> String {
    fn map(
        o: &serde_json::Map<String, Value>,
        col: usize,
        n: usize,
        sort: bool,
        first_inline: bool,
        out: &mut String,
    ) {
        let lf = LEAF_FLOW.with(|l| l.get());
        for (i, k) in sorted_keys(o, sort).into_iter().enumerate() {
            if i > 0 || !first_inline {
                out.push('\n');
                out.push_str(&" ".repeat(col));
            }
            out.push_str(&yaml_scalar(k));
            out.push(':');
            if lf && is_leaf_collection(&o[k]) {
                out.push(' ');
                out.push_str(&flow(&o[k], sort));
            } else {
                value(&o[k], col, n, sort, true, out);
            }
        }
    }
    fn seq(a: &[Value], col: usize, n: usize, sort: bool, first_inline: bool, out: &mut String) {
        for (i, x) in a.iter().enumerate() {
            if i > 0 || !first_inline {
                out.push('\n');
                out.push_str(&" ".repeat(col));
            }
            out.push('-');
            match x {
                c if LEAF_FLOW.with(|l| l.get()) && is_leaf_collection(c) => {
                    out.push(' ');
                    out.push_str(&flow(c, sort));
                }
                Value::Object(o) if !o.is_empty() => {
                    out.push_str(&" ".repeat(n.saturating_sub(1).max(1)));
                    map(o, col + n, n, sort, true, out);
                }
                Value::Array(a2) if !a2.is_empty() => {
                    out.push_str(&" ".repeat(n.saturating_sub(1).max(1)));
                    seq(a2, col + n, n, sort, true, out);
                }
                leaf => {
                    out.push(' ');
                    out.push_str(&yaml_leaf(leaf).unwrap_or_default());
                }
            }
        }
    }
    /// value after `key:`; `in_map` sequences are indentless
    fn value(v: &Value, col: usize, n: usize, sort: bool, in_map: bool, out: &mut String) {
        match v {
            Value::Object(o) if !o.is_empty() => map(o, col + n, n, sort, false, out),
            Value::Array(a) if !a.is_empty() => {
                let c = if in_map { col } else { col + n };
                seq(a, c, n, sort, false, out)
            }
            leaf => {
                out.push(' ');
                out.push_str(&yaml_leaf(leaf).unwrap_or_default());
            }
        }
    }
    let n = indent.max(1);
    LEAF_FLOW.with(|l| l.set(leaf_flow));
    let mut out = String::new();
    if leaf_flow && is_leaf_collection(v) {
        return format!("{}\n", flow(v, sort));
    }
    match v {
        Value::Object(o) if !o.is_empty() => map(o, 0, n, sort, true, &mut out),
        Value::Array(a) if !a.is_empty() => seq(a, 0, n, sort, true, &mut out),
        leaf => out.push_str(&yaml_leaf(leaf).unwrap_or_default()),
    }
    out.push('\n');
    if matches!(
        v,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    ) {
        // PyYAML terminates bare scalars with the document end marker
        out.push_str("...\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_styles() {
        let v = json!({"b": 1, "a": [1, "x", null], "e": {}, "u": "é"});
        assert_eq!(
            json_dumps(&v, None, false, true),
            r#"{"b": 1, "a": [1, "x", null], "e": {}, "u": "\u00e9"}"#
        );
        assert_eq!(
            json_dumps(&json!({"b": 1, "a": [1, 2]}), Some(4), true, true),
            "{\n    \"a\": [\n        1,\n        2\n    ],\n    \"b\": 1\n}"
        );
    }

    #[test]
    fn yaml_matches_pyyaml() {
        let v = json!({"num": 5, "str": "hello: world", "t": true, "none": null, "list": [1, 2, {"k": "v", "j": 1}], "yes": "yes", "n": "123", "empty": [], "m": {"x": {"y": 1}}});
        assert_eq!(
            yaml_dump(&v, 2, true),
            "empty: []\nlist:\n- 1\n- 2\n- j: 1\n  k: v\nm:\n  x:\n    y: 1\nn: '123'\nnone: null\nnum: 5\nstr: 'hello: world'\nt: true\n'yes': 'yes'\n"
        );
        assert_eq!(
            yaml_dump(&json!({"a": [{"b": 1, "c": 2}]}), 4, true),
            "a:\n-   b: 1\n    c: 2\n"
        );
        assert_eq!(
            yaml_dump(&json!({"a": {"b": 1}}), 4, true),
            "a:\n    b: 1\n"
        );
    }
}
