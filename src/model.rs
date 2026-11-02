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
    /// Who served the model (anthropic, openai, ...). Empty: the session's last known one.
    pub provider: String,
    /// Reasoning tokens, already counted inside `output`.
    pub reasoning: i64,
    /// The folder the response was made in, when the agent records it per message
    /// (sessions can move between folders).
    pub cwd: Option<String>,
}

/// An agent's tool call that changed a file (absolute path).
#[derive(Debug, Clone, PartialEq)]
pub struct FileEdit {
    pub id: String,
    pub ts: Option<String>,
    pub path: String,
    /// Edit, Write, apply_patch, ...
    pub tool: String,
}

/// How long one turn took: from the prompt to the agent's final reply.
#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub id: String,
    pub ts: Option<String>,
    pub duration_ms: i64,
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
    pub tasks: Vec<Task>,
    pub edits: Vec<FileEdit>,
}

/// The files a Codex-style patch touches (`*** Add File:`, `*** Update File:`,
/// `*** Delete File:`, `*** Move to:`), made absolute against `cwd`.
pub fn patch_paths(patch: &str, cwd: Option<&str>) -> Vec<String> {
    patch
        .lines()
        .filter_map(|l| {
            [
                "*** Add File: ",
                "*** Update File: ",
                "*** Delete File: ",
                "*** Move to: ",
            ]
            .iter()
            .find_map(|p| l.strip_prefix(p))
        })
        .map(|p| absolute(p.trim(), cwd))
        .collect()
}

/// `p` with symlinks resolved, so the same file always has one name (git reports resolved
/// paths; agents record what they were given, e.g. macOS's /var for /private/var). A path
/// that no longer exists keeps its name under its resolved folder; failing that, as is.
pub fn canonical(p: &str) -> String {
    let path = std::path::Path::new(p);
    if let Ok(c) = std::fs::canonicalize(path) {
        return c.display().to_string();
    }
    match (
        path.parent().and_then(|d| std::fs::canonicalize(d).ok()),
        path.file_name(),
    ) {
        (Some(dir), Some(name)) => dir.join(name).display().to_string(),
        _ => p.to_string(),
    }
}

/// `p` made absolute against `cwd` (unchanged if already absolute, or no cwd).
pub fn absolute(p: &str, cwd: Option<&str>) -> String {
    match cwd {
        Some(c) if !p.starts_with('/') => {
            format!("{}/{}", c.trim_end_matches('/'), p.trim_start_matches("./"))
        }
        _ => p.to_string(),
    }
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

/// Milliseconds since the epoch as an ISO-8601 UTC time, e.g. `2026-09-21T14:13:20.000Z`.
pub fn iso_from_ms(ms: i64) -> String {
    let (secs, milli) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{milli:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patches_name_their_files() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-x\n+y\n*** Add File: /abs/new.rs\n+z\n*** Delete File: ./old.rs\n*** Update File: b.rs\n*** Move to: c.rs\n*** End Patch";
        assert_eq!(
            patch_paths(patch, Some("/repo/")),
            [
                "/repo/src/a.rs",
                "/abs/new.rs",
                "/repo/old.rs",
                "/repo/b.rs",
                "/repo/c.rs"
            ]
        );
        assert_eq!(absolute("x", None), "x");
    }

    #[test]
    fn iso_dates() {
        assert_eq!(iso_from_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_from_ms(1_790_000_000_000), "2026-09-21T14:13:20.000Z");
        assert_eq!(iso_from_ms(951_782_400_123), "2000-02-29T00:00:00.123Z");
    }
}
