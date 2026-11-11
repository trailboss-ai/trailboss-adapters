//! Gemini CLI. Sessions are `~/.gemini/tmp/<project>/chats/session-<time>-<id8>.jsonl`: a
//! header (sessionId, startTime, kind), then messages. A message is written again whole when
//! it changes (its tokens arrive after the reply, tool calls and results after that), so
//! everything is keyed by the message id. `$set` records update the header (the summary);
//! `$patch` and `$rewindTo` edit or drop earlier messages and are skipped: tokens a rewound
//! reply used were still spent. A sub-agent's file is `chats/<parent session id>/<id>.jsonl`.
//! Older versions wrote one JSON document (`session-*.json`) with a `messages` array, which
//! the core reads whole. The project folder isn't in the transcript: `<project>/.project_root`
//! names it (older hash-named folders have none).

use crate::model::{Batch, FileEdit, Message, Session, Usage, absolute, preview, text_at, widen};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub const AGENT: &str = "gemini";

pub fn roots() -> Vec<PathBuf> {
    dirs::home_dir()
        .map(|h| vec![h.join(".gemini").join("tmp")])
        .unwrap_or_default()
}

/// Session files only: logs, checkpoints and tool output sit beside `chats/`.
pub fn is_session_file(p: &Path) -> bool {
    p.extension().is_some_and(|x| x == "json" || x == "jsonl")
        && p.ancestors().skip(1).any(|a| a.ends_with("chats"))
}

fn is_header(r: &Value) -> bool {
    r["sessionId"].is_string() && r["projectHash"].is_string() && r.get("id").is_none()
}

/// The header, which is only in the file's first line (or, for a chunk read later, on disk).
fn header_on_disk(path: &Path) -> Option<Value> {
    let mut line = String::new();
    BufReader::new(std::fs::File::open(path).ok()?)
        .read_line(&mut line)
        .ok()?;
    serde_json::from_str(&line).ok().filter(is_header)
}

/// The folder a session ran in, from its project's `.project_root`.
fn project_root(path: &Path) -> Option<String> {
    let project = path.ancestors().find(|a| a.ends_with("chats"))?.parent()?;
    let text = std::fs::read_to_string(project.join(".project_root")).ok()?;
    Some(text.trim().to_string()).filter(|t| !t.is_empty())
}

pub fn parse(path: &Path, bytes: &[u8]) -> Batch {
    // An old whole-document session, or JSONL records.
    let records: Vec<Value> = match serde_json::from_slice::<Value>(bytes) {
        Ok(mut doc) if doc["messages"].is_array() => {
            let msgs = doc["messages"].take();
            std::iter::once(doc)
                .chain(msgs.as_array().into_iter().flatten().cloned())
                .collect()
        }
        _ => bytes
            .split(|&c| c == b'\n')
            .filter_map(|l| serde_json::from_slice(l).ok())
            .collect(),
    };
    let header = match records.first() {
        Some(r) if is_header(r) => Some(r.clone()),
        _ => header_on_disk(path),
    };
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let h = header.unwrap_or_default();
    let sub = h["kind"] == "subagent";
    let mut b = Batch {
        session: Session {
            agent: AGENT,
            id: text_at(&h["sessionId"]).unwrap_or_else(|| stem.to_string()),
            path: path.display().to_string(),
            cwd: project_root(path),
            // A sub-agent's folder is named after its parent session.
            parent: sub
                .then(|| path.parent()?.file_name()?.to_str().map(String::from))
                .flatten()
                .filter(|p| p != "chats"),
            ..Default::default()
        },
        ..Default::default()
    };
    for r in &records {
        if is_header(r) {
            title(&mut b.session, r);
            for k in ["startTime", "lastUpdated"] {
                if let Some(t) = text_at(&r[k]) {
                    widen(&mut b.session, &t);
                }
            }
        } else if r["$set"].is_object() {
            title(&mut b.session, &r["$set"]);
            if let Some(t) = text_at(&r["$set"]["lastUpdated"]) {
                widen(&mut b.session, &t);
            }
        } else if let Some(id) = text_at(&r["id"]) {
            message(&mut b, &id, r);
        }
    }
    b
}

fn title(s: &mut Session, r: &Value) {
    if let Some(t) = text_at(&r["summary"]) {
        s.title = Some(preview(&t));
    }
}

fn message(b: &mut Batch, id: &str, r: &Value) {
    let ts = text_at(&r["timestamp"]);
    if let Some(t) = &ts {
        widen(&mut b.session, t);
    }
    match r["type"].as_str() {
        Some("user") => {
            // What the user typed; `content` adds the files an @-mention pulled in.
            let shown = text(&r["displayContent"]);
            let text = if shown.is_empty() {
                text(&r["content"])
            } else {
                shown
            };
            if text.is_empty() {
                return;
            }
            if b.session.first_prompt.is_none() {
                b.session.first_prompt = Some(preview(&text));
            }
            b.messages.push(Message {
                uuid: id.to_string(),
                ts,
                role: "user",
                text,
            });
        }
        Some("gemini") => {
            let thoughts: Vec<String> = r["thoughts"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|t| {
                    let part = |k| t[k].as_str().unwrap_or_default();
                    format!("{}\n{}", part("subject"), part("description"))
                })
                .collect();
            b.messages.extend(Message::extra(
                format!("{id}:thinking"),
                ts.clone(),
                "thinking",
                &thoughts.join("\n"),
            ));
            for tc in r["toolCalls"].as_array().into_iter().flatten() {
                tool_call(b, ts.clone(), tc);
            }
            if let Some(u) = usage(id, ts.clone(), r) {
                b.usage.push(u);
            }
            let text = text(&r["content"]);
            if !text.is_empty() {
                b.messages.push(Message {
                    uuid: id.to_string(),
                    ts,
                    role: "assistant",
                    text,
                });
            }
        }
        // info, error, warning: the CLI's own notices.
        _ => {}
    }
}

fn tool_call(b: &mut Batch, ts: Option<String>, tc: &Value) {
    let Some(call) = text_at(&tc["id"]) else {
        return;
    };
    let ts = text_at(&tc["timestamp"]).or(ts);
    let out = match &tc["resultDisplay"] {
        Value::String(s) => s.clone(),
        _ => text(&tc["result"]),
    };
    b.messages.extend(Message::extra(
        format!("{call}:result"),
        ts.clone(),
        "tool",
        &out,
    ));
    let name = tc["name"].as_str().unwrap_or_default();
    if matches!(name, "write_file" | "replace")
        && tc["status"] == "success"
        && let Some(p) = text_at(&tc["args"]["file_path"])
    {
        b.edits.push(FileEdit {
            id: call,
            ts,
            path: absolute(&p, b.session.cwd.as_deref()),
            tool: name.to_string(),
        });
    }
}

/// The text of a `PartListUnion`: a string, a part, or a list of them. Thought parts are
/// left out; a tool's response counts as text so its output is searchable.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_string(),
        Value::Array(parts) => parts
            .iter()
            .map(text)
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) if v["thought"] == true => String::new(),
        Value::Object(_) => {
            let r = &v["functionResponse"]["response"];
            text_at(&v["text"])
                .or_else(|| text_at(&r["output"]))
                .or_else(|| text_at(&r["error"]))
                .map(|t| t.trim().to_string())
                .unwrap_or_default()
        }
        _ => String::new(),
    }
}

/// Gemini counts cached tokens inside `input` and thinking apart from `output`; the model
/// bills thinking as output.
fn usage(id: &str, ts: Option<String>, r: &Value) -> Option<Usage> {
    let t = &r["tokens"];
    if !t.is_object() {
        return None;
    }
    let n = |k: &str| t[k].as_i64().unwrap_or(0);
    let cached = n("cached");
    Some(Usage {
        message_id: id.to_string(),
        ts,
        model: text_at(&r["model"]).unwrap_or_default(),
        input: (n("input") - cached).max(0) + n("tool"),
        output: n("output") + n("thoughts"),
        cache_read: cached,
        provider: "google".into(),
        reasoning: n("thoughts"),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARENT: &str = "0f6c1a2e-1111-4aaa-8bbb-000000000001";

    fn fixture(name: &str) -> (PathBuf, Vec<u8>) {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/gemini/tmp/shop/chats")
            .join(name);
        let bytes = std::fs::read(&p).unwrap();
        (p, bytes)
    }

    #[test]
    fn session() {
        let (p, bytes) = fixture("session-2026-10-01T09-00-0f6c1a2e.jsonl");
        let b = parse(&p, &bytes);
        let s = &b.session;
        assert_eq!(s.id, PARENT);
        assert_eq!(s.parent, None);
        assert_eq!(s.cwd.as_deref(), Some("/work/shop"), "from .project_root");
        assert_eq!(s.title.as_deref(), Some("Add a cart badge"));
        assert_eq!(
            s.first_prompt.as_deref(),
            Some("Add a badge to the cart icon")
        );
        assert_eq!(s.started.as_deref(), Some("2026-10-01T09:00:00.000Z"));
        assert_eq!(s.ended.as_deref(), Some("2026-10-01T09:00:09.000Z"));

        // The reply is written three times (text, then tokens, then its tool call); one usage
        // row per write, all under the message id, so the store keeps one.
        let ids: Vec<_> = b.usage.iter().map(|u| u.message_id.as_str()).collect();
        assert_eq!(ids, ["g1", "g1"]);
        let u = &b.usage[1];
        assert_eq!(
            (u.model.as_str(), u.provider.as_str()),
            ("gemini-2.5-pro", "google")
        );
        // 1200 input of which 1000 cached, plus 30 tool-use prompt; 40 output + 25 thinking.
        assert_eq!(
            (u.input, u.cache_read, u.output, u.reasoning),
            (230, 1000, 65, 25)
        );
        assert_eq!(u.reported_cost, None);

        let roles: Vec<_> = b
            .messages
            .iter()
            .map(|m| (m.role, m.uuid.as_str()))
            .collect();
        assert_eq!(
            roles,
            [
                ("user", "u1"),
                ("thinking", "g1:thinking"),
                ("assistant", "g1"),
                ("thinking", "g1:thinking"),
                ("assistant", "g1"),
                ("thinking", "g1:thinking"),
                ("tool", "write_file-1:result"),
                ("assistant", "g1"),
            ]
        );
        assert_eq!(
            b.messages[0].text, "Add a badge to the cart icon",
            "displayContent"
        );
        assert_eq!(b.messages[1].text, "Plan\nFind the cart component first");
        assert_eq!(b.messages[6].text, "Wrote src/cart.css");
        assert_eq!(
            b.edits,
            [FileEdit {
                id: "write_file-1".into(),
                ts: Some("2026-10-01T09:00:05.000Z".into()),
                path: "/work/shop/src/cart.css".into(),
                tool: "write_file".into(),
            }]
        );
    }

    #[test]
    fn unknown_records_and_bad_lines_are_skipped() {
        let (p, bytes) = fixture("session-2026-10-02T10-00-5a5a5a5a.jsonl");
        let b = parse(&p, &bytes);
        assert_eq!(b.session.id, "5a5a5a5a-2222-4ccc-8ddd-000000000002");
        let roles: Vec<_> = b.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, ["user", "assistant"]);
        assert_eq!(b.usage.len(), 1);
        assert_eq!(b.usage[0].model, "gemini-3-flash-preview");
    }

    #[test]
    fn truncated_session_keeps_whole_lines() {
        let (p, bytes) = fixture("session-2026-10-01T09-00-0f6c1a2e.jsonl");
        let cut = &bytes[..bytes.len() - 60];
        let b = parse(&p, cut);
        assert_eq!(b.session.id, PARENT);
        assert_eq!(b.usage.len(), 1, "the last write, cut, is skipped");
        assert!(b.edits.is_empty());
    }

    #[test]
    fn later_chunk_reads_the_header_from_disk() {
        let (p, bytes) = fixture("session-2026-10-01T09-00-0f6c1a2e.jsonl");
        let first = bytes.iter().position(|&c| c == b'\n').unwrap();
        let b = parse(&p, &bytes[first + 1..]);
        assert_eq!(b.session.id, PARENT);
        assert_eq!(
            b.session.first_prompt.as_deref(),
            Some("Add a badge to the cart icon")
        );
    }

    #[test]
    fn subagent_links_to_parent_by_folder() {
        let (p, bytes) = fixture(&format!(
            "{PARENT}/9e9e9e9e-3333-4eee-8fff-000000000003.jsonl"
        ));
        let b = parse(&p, &bytes);
        assert_eq!(b.session.id, "9e9e9e9e-3333-4eee-8fff-000000000003");
        assert_eq!(b.session.parent.as_deref(), Some(PARENT));
        assert_eq!(b.session.cwd.as_deref(), Some("/work/shop"));
        assert_eq!(b.usage.len(), 1);
    }

    #[test]
    fn old_whole_document_session() {
        let (p, bytes) = fixture("session-2025-09-01T08-00-7b7b7b7b.json");
        let b = parse(&p, &bytes);
        let s = &b.session;
        assert_eq!(s.id, "7b7b7b7b-4444-4aaa-8bbb-000000000004");
        assert_eq!(s.first_prompt.as_deref(), Some("Explain the build"));
        assert_eq!(s.ended.as_deref(), Some("2025-09-01T08:00:05.000Z"));
        assert_eq!(b.usage.len(), 1);
        assert_eq!((b.usage[0].input, b.usage[0].output), (500, 50));
    }

    #[test]
    fn only_chat_files_are_sessions() {
        assert!(is_session_file(Path::new(
            "/h/.gemini/tmp/x/chats/session-a.jsonl"
        )));
        assert!(is_session_file(Path::new(
            "/h/.gemini/tmp/x/chats/p/s.jsonl"
        )));
        assert!(!is_session_file(Path::new("/h/.gemini/tmp/x/logs.json")));
        assert!(!is_session_file(Path::new(
            "/h/.gemini/tmp/x/chats/s.jsonl.tmp-12"
        )));
    }
}
