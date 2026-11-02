//! The normalized records every adapter emits and the store saves, plus small helpers
//! adapters share.

use serde_json::Value;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Session {
    pub agent: &'static str,
    pub id: String,
    /// Set on sub-agent sessions: the session that spawned them.
    pub parent: Option<String>,
    pub path: String,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    pub started: Option<String>,
    pub ended: Option<String>,
}

/// Token counts for one model response. `message_id` is unique per agent, so a response
/// copied into a resumed session's file isn't counted twice.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Usage {
    pub message_id: String,
    pub ts: Option<String>,
    pub model: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    /// The agent's own cost figure, used when trailboss has no price for the model.
    pub reported_cost: Option<f64>,
    /// The git branch the response was made on, when the agent records it per message.
    pub branch: Option<String>,
}

/// Searchable text of one user or assistant message.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub uuid: String,
    pub ts: Option<String>,
    pub role: &'static str,
    pub text: String,
}

/// What parsing one chunk of one session file yields.
#[derive(Debug, Default)]
pub struct Batch {
    pub session: Session,
    pub usage: Vec<Usage>,
    pub messages: Vec<Message>,
}

/// A non-empty string, or None.
pub fn text_at(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// The human-readable text of a message: a plain string, or its text blocks (`text`,
/// `input_text`, `output_text`). Thinking, tool calls and tool results are left out.
pub fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.trim().to_string(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| {
                matches!(
                    b["type"].as_str(),
                    Some("text" | "input_text" | "output_text")
                )
            })
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

/// First 200 characters on one line, for session previews.
pub fn preview(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    one_line.chars().take(200).collect()
}

/// Earliest and latest timestamps seen. ISO-8601 UTC strings compare correctly as text.
pub fn widen(s: &mut Session, ts: &str) {
    if s.started.as_deref().is_none_or(|x| ts < x) {
        s.started = Some(ts.to_string());
    }
    if s.ended.as_deref().is_none_or(|x| ts > x) {
        s.ended = Some(ts.to_string());
    }
}

/// Seconds since the epoch for an ISO-8601 time with any offset. SQLite (already here for
/// the index) does the calendar math.
pub fn epoch(iso: &str) -> Option<i64> {
    rusqlite::Connection::open_in_memory()
        .ok()?
        .query_row("SELECT unixepoch(?1)", [iso], |r| r.get(0))
        .ok()?
}

/// Today's local date `days` days ago, as YYYY-MM-DD.
pub fn date_days_ago(days: i64) -> String {
    rusqlite::Connection::open_in_memory()
        .and_then(|c| {
            c.query_row(
                "SELECT date('now', 'localtime', ?1)",
                [format!("-{days} days")],
                |r| r.get(0),
            )
        })
        .unwrap_or_default()
}
