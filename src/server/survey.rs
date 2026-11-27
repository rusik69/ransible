//! Job template surveys: typed launch-time prompts (AWX-style `survey_spec`).
use serde_json::{Map, Value};

pub fn validate_spec(spec: &Value) -> Result<(), String> {
    let items = spec
        .as_array()
        .ok_or("survey_spec must be a JSON list of questions")?;
    let mut seen = std::collections::HashSet::new();
    for q in items {
        let var = q.get("variable").and_then(|v| v.as_str()).unwrap_or("");
        if var.is_empty() || !var.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Err(format!("invalid survey variable name '{var}'"));
        }
        if !seen.insert(var.to_string()) {
            return Err(format!("duplicate survey variable '{var}'"));
        }
        let ty = q.get("type").and_then(|t| t.as_str()).unwrap_or("text");
        if ![
            "text",
            "password",
            "integer",
            "float",
            "multiplechoice",
            "multiselect",
        ]
        .contains(&ty)
        {
            return Err(format!("survey variable '{var}': unknown type '{ty}'"));
        }
        if ["multiplechoice", "multiselect"].contains(&ty) && choices(q).is_empty() {
            return Err(format!("survey variable '{var}': {ty} needs choices"));
        }
    }
    Ok(())
}

pub fn choices(q: &Value) -> Vec<String> {
    match q.get("choices") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|c| {
                c.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| c.to_string())
            })
            .collect(),
        Some(Value::String(s)) => s
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        _ => vec![],
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// (plain variables, secret variables)
pub type Applied = (Map<String, Value>, Map<String, Value>);

/// Apply survey answers over the template's own variables.
/// Returns (plain variables, secret variables). Errors are user-facing messages.
pub fn apply(
    spec: &Value,
    base: &Map<String, Value>,
    answers: &Map<String, Value>,
) -> Result<Applied, String> {
    let mut plain = base.clone();
    let mut secret = Map::new();
    let items = spec.as_array().cloned().unwrap_or_default();
    let survey_vars: std::collections::HashSet<String> = items
        .iter()
        .filter_map(|q| q["variable"].as_str().map(String::from))
        .collect();
    for (k, v) in answers {
        if !survey_vars.contains(k) {
            plain.insert(k.clone(), v.clone());
        }
    }
    for q in &items {
        let var = q["variable"].as_str().unwrap_or("").to_string();
        let ty = q.get("type").and_then(|t| t.as_str()).unwrap_or("text");
        let required = q.get("required").and_then(|r| r.as_bool()).unwrap_or(false);
        let given = answers.get(&var).filter(|v| {
            !v.is_null() && v.as_str() != Some("") && v.as_str() != Some("$encrypted$")
        });
        let val = match given.cloned().or_else(|| {
            q.get("default")
                .filter(|d| !d.is_null() && d.as_str() != Some(""))
                .cloned()
        }) {
            Some(v) => v,
            None => {
                if required {
                    return Err(format!("survey question '{var}' is required"));
                }
                continue;
            }
        };
        let (lo, hi) = (q.get("min").and_then(as_f64), q.get("max").and_then(as_f64));
        let out = match ty {
            "integer" | "float" => {
                let n = as_f64(&val).ok_or_else(|| format!("'{var}' must be a number"))?;
                if ty == "integer" && n.fract() != 0.0 {
                    return Err(format!("'{var}' must be an integer"));
                }
                if lo.map(|l| n < l).unwrap_or(false) || hi.map(|h| n > h).unwrap_or(false) {
                    return Err(format!(
                        "'{var}' must be between {} and {}",
                        lo.map(|l| l.to_string()).unwrap_or("-inf".into()),
                        hi.map(|h| h.to_string()).unwrap_or("inf".into())
                    ));
                }
                if ty == "integer" {
                    Value::from(n as i64)
                } else {
                    Value::from(n)
                }
            }
            "multiplechoice" => {
                let s = val
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| val.to_string());
                if !choices(q).contains(&s) {
                    return Err(format!("'{var}' must be one of: {}", choices(q).join(", ")));
                }
                Value::String(s)
            }
            "multiselect" => {
                let picked: Vec<String> = match &val {
                    Value::Array(a) => a
                        .iter()
                        .map(|x| {
                            x.as_str()
                                .map(String::from)
                                .unwrap_or_else(|| x.to_string())
                        })
                        .collect(),
                    Value::String(s) => s
                        .split([',', '\n'])
                        .map(|x| x.trim().to_string())
                        .filter(|x| !x.is_empty())
                        .collect(),
                    other => vec![other.to_string()],
                };
                let allowed = choices(q);
                if let Some(bad) = picked.iter().find(|p| !allowed.contains(p)) {
                    return Err(format!("'{var}': '{bad}' is not a valid choice"));
                }
                Value::Array(picked.into_iter().map(Value::String).collect())
            }
            _ => {
                let s = val
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| val.to_string());
                if lo.map(|l| (s.chars().count() as f64) < l).unwrap_or(false)
                    || hi.map(|h| (s.chars().count() as f64) > h).unwrap_or(false)
                {
                    return Err(format!(
                        "'{var}' length must be between {} and {}",
                        lo.unwrap_or(0.0),
                        hi.map(|h| h.to_string()).unwrap_or("inf".into())
                    ));
                }
                Value::String(s)
            }
        };
        if ty == "password" {
            plain.remove(&var);
            secret.insert(var, out);
        } else {
            plain.insert(var, out);
        }
    }
    Ok((plain, secret))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn m(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn spec_validation() {
        assert!(validate_spec(&json!([{"variable": "a", "type": "text"}])).is_ok());
        assert!(validate_spec(&json!([{"variable": "a b"}])).is_err());
        assert!(validate_spec(&json!([{"variable": "a", "type": "nope"}])).is_err());
        assert!(validate_spec(&json!([{"variable": "a", "type": "multiplechoice"}])).is_err());
        assert!(validate_spec(&json!([{"variable": "a"}, {"variable": "a"}])).is_err());
        assert!(validate_spec(&json!({"x": 1})).is_err());
    }

    #[test]
    fn defaults_required_and_types() {
        let spec = json!([
            {"variable": "env", "type": "multiplechoice", "choices": ["dev", "prod"], "default": "dev"},
            {"variable": "count", "type": "integer", "min": 1, "max": 5, "required": true},
            {"variable": "ratio", "type": "float"},
            {"variable": "name", "type": "text", "min": 2}
        ]);
        let base = m(json!({"keep": 1}));
        assert!(
            apply(&spec, &base, &m(json!({})))
                .unwrap_err()
                .contains("count")
        );
        let (p, s) = apply(
            &spec,
            &base,
            &m(json!({"count": "3", "ratio": 0.5, "extra": "x"})),
        )
        .unwrap();
        assert_eq!(p["env"], "dev");
        assert_eq!(p["count"], 3);
        assert_eq!(p["ratio"], 0.5);
        assert_eq!(p["keep"], 1);
        assert_eq!(p["extra"], "x");
        assert!(s.is_empty());
        assert!(apply(&spec, &base, &m(json!({"count": 9}))).is_err());
        assert!(apply(&spec, &base, &m(json!({"count": 2.5}))).is_err());
        assert!(apply(&spec, &base, &m(json!({"count": 2, "env": "stage"}))).is_err());
        assert!(apply(&spec, &base, &m(json!({"count": 2, "name": "x"}))).is_err());
    }

    #[test]
    fn passwords_are_split_out_and_multiselect_works() {
        let spec = json!([
            {"variable": "pw", "type": "password", "required": true},
            {"variable": "tags", "type": "multiselect", "choices": "a\nb\nc"}
        ]);
        let (p, s) = apply(
            &spec,
            &m(json!({"pw": "template-default"})),
            &m(json!({"pw": "hunter2", "tags": "a, c"})),
        )
        .unwrap();
        assert!(!p.contains_key("pw"));
        assert_eq!(s["pw"], "hunter2");
        assert_eq!(p["tags"], json!(["a", "c"]));
        assert!(apply(&spec, &m(json!({})), &m(json!({"pw": "x", "tags": ["z"]}))).is_err());
        // "$encrypted$" placeholder counts as not provided
        assert!(apply(&spec, &m(json!({})), &m(json!({"pw": "$encrypted$"}))).is_err());
    }
}
