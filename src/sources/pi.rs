//! pi. Sessions are `~/.pi/agent/sessions/<folder>/<time>_<id>.jsonl`: a `session` header
//! (id, cwd), then `message` records. Compaction and branch summaries also carry usage.
//! A forked or spawned session's header names its parent's file (`parentSession`).
//! Sub-agent transcripts from the pi-subagents extension
//! (`<folder>/subagent-artifacts/<run>_<agent>.jsonl`) have no header: each record is the
//! message itself, marked `recordType: message`. They don't name their parent session.

use crate::model::{
    Batch, Message, Session, Usage, preview, text_at, text_of, thinking_and_tools, widen,
};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const AGENT: &str = "pi";

pub fn roots() -> Vec<PathBuf> {
    dirs::home_dir()
        .map(|h| vec![h.join(".pi").join("agent").join("sessions")])
        .unwrap_or_default()
}

pub fn parse(path: &Path, bytes: &[u8]) -> Batch {
    // The header is only in the first chunk, so the file name is the stable id.
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let mut b = Batch {
        session: Session {
            agent: AGENT,
            id: id.clone(),
            path: path.display().to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    for line in bytes.split(|&c| c == b'\n') {
        let Ok(r) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let sub = r["recordType"] == "message";
        let m = if sub { &r } else { &r["message"] };
        let ts = text_at(&r["timestamp"]).or_else(|| text_at(&m["timestamp"]));
        if let Some(t) = &ts {
            widen(&mut b.session, t);
        }
        let key = || {
            text_at(&r["id"])
                .or_else(|| text_at(&m["responseId"]))
                .unwrap_or_else(|| format!("{id}:{}", ts.as_deref().unwrap_or_default()))
        };
        let kind = if sub {
            Some("message")
        } else {
            r["type"].as_str()
        };
        match kind {
            Some("session") => {
                b.session.cwd = text_at(&r["cwd"]);
                // Session ids are file stems, so the parent's path gives its id.
                b.session.parent = text_at(&r["parentSession"])
                    .and_then(|f| Some(Path::new(&f).file_stem()?.to_str()?.to_string()));
            }
            Some("message") => {
                if sub && b.session.cwd.is_none() {
                    b.session.cwd = text_at(&r["cwd"]);
                }
                let role = match m["role"].as_str() {
                    Some("user") => "user",
                    Some("assistant") => "assistant",
                    Some("toolResult") => {
                        let out = text_of(&m["content"]);
                        b.messages.extend(Message::extra(key(), ts, "tool", &out));
                        continue;
                    }
                    _ => continue,
                };
                let thinking = thinking_and_tools(&m["content"]).0;
                b.messages.extend(Message::extra(
                    format!("{}:thinking", key()),
                    ts.clone(),
                    "thinking",
                    &thinking,
                ));
                if role == "assistant"
                    && let Some(u) =
                        usage(key(), ts.clone(), &m["usage"], &m["model"], &m["provider"])
                {
                    b.usage.push(u);
                }
                let text = text_of(&m["content"]);
                if text.is_empty() {
                    continue;
                }
                if role == "user" && b.session.first_prompt.is_none() {
                    b.session.first_prompt = Some(preview(&text));
                }
                b.messages.push(Message {
                    uuid: key(),
                    ts,
                    role,
                    text,
                });
            }
            Some("usage" | "compaction" | "branch_summary") => {
                if let Some(u) = usage(key(), ts, &r["usage"], &r["model"], &r["provider"]) {
                    b.usage.push(u);
                }
            }
            _ => {}
        }
    }
    b
}

/// pi counts input without cache, like Anthropic, and records its own cost.
fn usage(
    message_id: String,
    ts: Option<String>,
    u: &Value,
    model: &Value,
    provider: &Value,
) -> Option<Usage> {
    if !u.is_object() {
        return None;
    }
    let n = |k: &str| u[k].as_i64().unwrap_or(0);
    Some(Usage {
        message_id,
        ts,
        model: text_at(model).unwrap_or_default(),
        input: n("input"),
        output: n("output"),
        cache_read: n("cacheRead"),
        cache_write_5m: n("cacheWrite"),
        cache_write_1h: 0,
        reported_cost: u["cost"]["total"].as_f64(),
        branch: None,
        provider: text_at(provider).unwrap_or_default(),
        reasoning: n("reasoning"),
        cwd: None,
        fast: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pi/2026-10-01T09-00-00-000Z_pi-1.jsonl");
        let b = parse(&p, &std::fs::read(&p).unwrap());
        let s = &b.session;
        assert_eq!(s.id, "2026-10-01T09-00-00-000Z_pi-1");
        assert_eq!(s.cwd.as_deref(), Some("/work/cli"));
        assert_eq!(s.first_prompt.as_deref(), Some("Summarize the README"));
        assert_eq!(s.ended.as_deref(), Some("2026-10-01T09:00:04.000Z"));

        // The assistant reply and the compaction both cost tokens; the tool result doesn't.
        assert_eq!(b.usage.len(), 2);
        let u = &b.usage[0];
        assert_eq!(u.model, "claude-sonnet-4-6");
        assert_eq!(
            (u.input, u.output, u.cache_read, u.cache_write_5m),
            (100, 20, 300, 40)
        );
        assert_eq!(u.reported_cost, Some(0.0123));
        assert_eq!(u.provider, "anthropic");
        assert_eq!(b.usage[1].message_id, "c1");

        let roles: Vec<_> = b.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, ["user", "thinking", "assistant", "tool"]);
        assert_eq!(b.messages[1].text, "Skim the headings first");
        assert_eq!(
            (b.messages[3].uuid.as_str(), b.messages[3].text.as_str()),
            ("m3", "file contents")
        );
    }

    fn fixture(name: &str) -> (PathBuf, Vec<u8>) {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pi")
            .join(name);
        let bytes = std::fs::read(&p).unwrap();
        (p, bytes)
    }

    #[test]
    fn forked_session_links_to_parent_by_file_name() {
        let (p, bytes) = fixture("2026-10-01T09-10-00-000Z_pi-2.jsonl");
        let b = parse(&p, &bytes);
        assert_eq!(
            b.session.parent.as_deref(),
            Some("2026-10-01T09-00-00-000Z_pi-1")
        );
    }

    #[test]
    fn subagent_transcript() {
        let (p, bytes) = fixture("subagent-artifacts/run1_worker.jsonl");
        let b = parse(&p, &bytes);
        let s = &b.session;
        assert_eq!(s.id, "run1_worker");
        assert_eq!(s.parent, None, "pi-subagents doesn't record it");
        assert_eq!(s.cwd.as_deref(), Some("/work/cli"));
        assert_eq!(s.first_prompt.as_deref(), Some("Find the config loader"));
        // The unknown `tool_progress` record is skipped.
        let roles: Vec<_> = b.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, ["user", "assistant"]);
        assert_eq!(b.usage.len(), 1);
        let u = &b.usage[0];
        assert_eq!(u.message_id, "resp-1");
        assert_eq!(
            (u.model.as_str(), u.provider.as_str()),
            ("claude-haiku-4-5", "anthropic")
        );
        assert_eq!((u.input, u.output, u.cache_read), (10, 5, 20));
        assert_eq!(u.reported_cost, Some(0.001));
    }

    #[test]
    fn truncated_subagent_transcript_keeps_whole_lines() {
        let (p, bytes) = fixture("subagent-artifacts/run1_worker.jsonl");
        let cut = &bytes[..bytes.len() - 40];
        let b = parse(&p, cut);
        assert_eq!(b.messages.len(), 1);
        assert!(b.usage.is_empty());
    }
}
