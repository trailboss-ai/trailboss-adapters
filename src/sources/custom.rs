//! Custom sources: any agent that writes JSONL with token usage, described by a field
//! mapping in the config, so new agents don't need code. The mapping's shape follows the
//! insights Herdr mod's custom adapter (MIT, Gagan Devagiri; see THIRD_PARTY_NOTICES.md).
//!
//! ```json
//! { "id": "myagent", "roots": ["~/.myagent/sessions"], "mapping": {
//!     "session": "sessionId", "timestamp": "ts", "messageId": "id", "project": "cwd",
//!     "model": "message.model", "provider": { "literal": "anthropic" },
//!     "role": "message.role", "roles": ["assistant"],
//!     "include": [{ "path": "type", "equals": "message" }],
//!     "usage": { "input": "usage.in", "output": "usage.out", "cacheRead": "usage.cr",
//!                "cacheWrite": "usage.cw", "reasoning": "usage.think" },
//!     "taskDurationMs": "durationMs" } }
//! ```
//!
//! A field is a dot path (`"message.model"`), `{ "path": …, "default": … }` or
//! `{ "literal": … }`. `session`, `timestamp` and at least one usage field are required.

use crate::model::{Batch, Session, Task, Usage, iso_from_ms, widen};
use serde_json::Value;
use std::path::Path;

fn at<'a>(rec: &'a Value, path: &str) -> &'a Value {
    path.split('.').fold(rec, |v, k| &v[k])
}

fn field(rec: &Value, spec: &Value) -> Option<Value> {
    let found = match spec {
        Value::String(p) => at(rec, p).clone(),
        Value::Object(o) if o.contains_key("literal") => o["literal"].clone(),
        Value::Object(o) => match o.get("path").and_then(Value::as_str) {
            Some(p) if !at(rec, p).is_null() => at(rec, p).clone(),
            _ => o.get("default").cloned().unwrap_or(Value::Null),
        },
        _ => Value::Null,
    };
    (!found.is_null()).then_some(found)
}

fn text(v: Option<Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A timestamp as ISO-8601: strings as they are, numbers as epoch milliseconds (or seconds,
/// when too small to be milliseconds).
fn timestamp(v: Option<Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s),
        Value::Number(n) => {
            let x = n.as_f64()?;
            Some(iso_from_ms(if x < 1e12 { x * 1000.0 } else { x } as i64))
        }
        _ => None,
    }
}

/// What's wrong with a mapping, if anything.
pub fn check(m: &Value) -> Option<String> {
    let usage = ["input", "output", "cacheRead", "cacheWrite", "reasoning"];
    if m["session"].is_null() || m["timestamp"].is_null() {
        Some("mapping needs session and timestamp".into())
    } else if !usage.iter().any(|k| !m["usage"][*k].is_null()) {
        Some("mapping needs at least one usage field".into())
    } else {
        None
    }
}

fn included(m: &Value, rec: &Value) -> bool {
    let rules_ok = m["include"].as_array().into_iter().flatten().all(|r| {
        let v = r["path"].as_str().map_or(&Value::Null, |p| at(rec, p));
        match (r.get("equals"), r["exists"].as_bool()) {
            (Some(want), _) => v == want,
            (None, Some(exists)) => exists == !v.is_null(),
            _ => true,
        }
    });
    let role_ok = match (m["roles"].as_array(), m.get("role")) {
        (Some(roles), Some(spec)) => field(rec, spec).is_some_and(|r| roles.contains(&r)),
        _ => true,
    };
    rules_ok && role_ok
}

pub fn parse(agent: &'static str, m: &Value, path: &Path, bytes: &[u8]) -> Batch {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let mut b = Batch {
        session: Session {
            agent,
            id: stem,
            path: path.display().to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut named = false;
    for (n, line) in bytes.split(|&c| c == b'\n').enumerate() {
        let Ok(rec) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if !rec.is_object() || !included(m, &rec) {
            continue;
        }
        // ponytail: one session per file, named by its first record. Split by the session
        // field if an agent ever mixes sessions in one file.
        if !named && let Some(id) = text(field(&rec, &m["session"])) {
            b.session.id = id;
            named = true;
        }
        let ts = timestamp(field(&rec, &m["timestamp"]));
        if let Some(t) = &ts {
            widen(&mut b.session, t);
        }
        if b.session.cwd.is_none() {
            b.session.cwd = m.get("project").and_then(|p| text(field(&rec, p)));
        }
        let get = |k: &str| field(&rec, &m["usage"][k]).and_then(|v| v.as_i64());
        let parts = ["input", "output", "cacheRead", "cacheWrite", "reasoning"].map(get);
        if parts.iter().any(Option::is_some) {
            let id = m
                .get("messageId")
                .and_then(|s| text(field(&rec, s)))
                .unwrap_or_else(|| {
                    format!("{}:{}:{n}", b.session.id, ts.as_deref().unwrap_or_default())
                });
            b.usage.push(Usage {
                message_id: id,
                ts: ts.clone(),
                model: m
                    .get("model")
                    .and_then(|s| text(field(&rec, s)))
                    .unwrap_or_default(),
                provider: m
                    .get("provider")
                    .and_then(|s| text(field(&rec, s)))
                    .unwrap_or_default(),
                input: parts[0].unwrap_or(0),
                output: parts[1].unwrap_or(0),
                cache_read: parts[2].unwrap_or(0),
                cache_write_5m: parts[3].unwrap_or(0),
                cache_write_1h: 0,
                reasoning: parts[4].unwrap_or(0),
                cwd: None,
                reported_cost: None,
                branch: None,
                fast: false,
            });
        }
        if let Some(ms) = m
            .get("taskDurationMs")
            .and_then(|s| field(&rec, s))
            .and_then(|v| v.as_i64())
        {
            b.tasks.push(Task {
                id: format!(
                    "{}:{}:{n}:task",
                    b.session.id,
                    ts.as_deref().unwrap_or_default()
                ),
                ts: ts.clone(),
                duration_ms: ms,
            });
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mapping() -> Value {
        json!({
            "session": "sid", "timestamp": "ts", "messageId": "id", "project": "cwd",
            "model": { "path": "m", "default": "house-model" }, "provider": { "literal": "acme" },
            "role": "role", "roles": ["assistant"],
            "include": [{ "path": "type", "equals": "msg" }, { "path": "u", "exists": true }],
            "usage": { "input": "u.in", "output": "u.out", "cacheRead": "u.cr" },
            "taskDurationMs": "took"
        })
    }

    #[test]
    fn maps_fields() {
        let lines = br#"{"type":"msg","role":"user","sid":"s1","ts":"2026-10-01T10:00:00Z","u":{"in":1}}
{"type":"msg","role":"assistant","sid":"s1","ts":"2026-10-01T10:00:05Z","id":"a1","cwd":"/w","u":{"in":10,"out":5,"cr":100},"took":4000}
{"type":"other","role":"assistant","sid":"s1","ts":"2026-10-01T10:00:06Z","u":{"in":99}}
{"type":"msg","role":"assistant","sid":"s1","ts":1790000000000,"m":"named","u":{"out":7}}
"#;
        let b = parse("acme", &mapping(), Path::new("/x/f.jsonl"), lines);
        assert_eq!(b.session.id, "s1");
        assert_eq!(b.session.cwd.as_deref(), Some("/w"));
        assert_eq!(b.usage.len(), 2, "user role and excluded type dropped");
        let u = &b.usage[0];
        assert_eq!(
            (u.message_id.as_str(), u.model.as_str(), u.provider.as_str()),
            ("a1", "house-model", "acme")
        );
        assert_eq!((u.input, u.output, u.cache_read), (10, 5, 100));
        assert_eq!(b.usage[1].model, "named");
        assert_eq!(
            b.usage[1].ts.as_deref(),
            Some("2026-09-21T14:13:20.000Z"),
            "epoch ms"
        );
        assert_eq!(b.tasks[0].duration_ms, 4000);
    }

    #[test]
    fn checks_required_fields() {
        assert_eq!(check(&mapping()), None);
        assert!(check(&json!({"session": "s", "usage": {"input": "i"}})).is_some());
        assert!(check(&json!({"session": "s", "timestamp": "t", "usage": {}})).is_some());
    }
}
