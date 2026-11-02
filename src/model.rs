//! The normalized records every adapter emits and the store saves.

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
