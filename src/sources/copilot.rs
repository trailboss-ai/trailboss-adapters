//! GitHub Copilot CLI (`copilot`). Each session is a folder,
//! `~/.copilot/session-state/<id>/` (`$COPILOT_HOME` replaces `~/.copilot`), holding an
//! append-only event log, `events.jsonl`, and `workspace.yaml` (the folder, a name or
//! summary). Older versions wrote the log flat, as `session-state/<id>.jsonl`. When the log
//! is rotated, the archive keeps a numbered name in the same folder and the new head starts
//! by replaying history in records marked `checkpoint: true`; those are skipped so nothing
//! counts twice. Events marked `ephemeral` aren't normally written, and are skipped if they are.
//!
//! What's recorded: prompts (`user.message`), replies (`assistant.message`), thinking,
//! tool output (`tool.execution_complete`) and the files tools changed. Token usage isn't
//! recorded per call (`assistant.usage` is never written); `session.shutdown` carries the
//! session's totals per model, rebuilt on resume from `session.usage_checkpoint`, so a
//! resumed session's later shutdown replaces the earlier one. A session that never shut
//! down cleanly (still running, or crashed) has no usage yet.
//! Sub-agents run inside the same log (events carry an `agentId`) and their tokens are in
//! the shutdown totals.

use crate::model::{
    Batch, FileEdit, Message, Session, Usage, absolute, patch_paths, preview, text_at, widen,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const AGENT: &str = "copilot";

/// Copilot doesn't say who served a model; it's billed through GitHub either way.
const PROVIDER: &str = "github";

pub fn roots() -> Vec<PathBuf> {
    let home = std::env::var_os("COPILOT_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".copilot")));
    home.map(|h| vec![h.join("session-state")])
        .unwrap_or_default()
}

/// `session-state/<id>.jsonl` (older, flat), or `session-state/<id>/events*.jsonl` (the log
/// and its numbered archives). Plans, checkpoints and other files in the folder aren't.
pub fn is_session_file(p: &Path) -> bool {
    let (Some(name), Some(parent)) = (p.file_name().and_then(|n| n.to_str()), p.parent()) else {
        return false;
    };
    if !name.ends_with(".jsonl") {
        return false;
    }
    if parent.ends_with("session-state") {
        return true;
    }
    name.starts_with("events")
        && parent
            .parent()
            .is_some_and(|g| g.ends_with("session-state"))
}

/// The session id: the folder's name, or a flat file's. Every chunk of the file finds it,
/// without needing the `session.start` record at the top.
fn session_id(path: &Path) -> String {
    let parent = path.parent();
    let in_folder = parent
        .and_then(Path::parent)
        .is_some_and(|g| g.ends_with("session-state"));
    let named = if in_folder {
        parent.and_then(Path::file_name)
    } else {
        path.file_stem()
    };
    named
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

/// `cwd` and a title from the session folder's `workspace.yaml`.
// ponytail: top-level `key: value` lines only, no YAML parser; a block scalar (`|`, `>`)
// is left out rather than misread.
fn workspace(path: &Path) -> (Option<String>, Option<String>) {
    let Some(dir) = path.parent().filter(|d| !d.ends_with("session-state")) else {
        return (None, None);
    };
    let Ok(text) = std::fs::read_to_string(dir.join("workspace.yaml")) else {
        return (None, None);
    };
    let mut keys: HashMap<&str, String> = HashMap::new();
    for line in text.lines() {
        if line.starts_with([' ', '\t', '#', '-']) {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        if v.is_empty() || v.starts_with(['|', '>']) {
            continue;
        }
        let unquoted = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(v);
        keys.insert(k.trim(), unquoted.to_string());
    }
    let title = keys
        .get("name")
        .or_else(|| keys.get("summary"))
        .map(|t| preview(t));
    (keys.remove("cwd"), title)
}

pub fn parse(path: &Path, bytes: &[u8]) -> Batch {
    let (cwd, title) = workspace(path);
    let mut b = Batch {
        session: Session {
            agent: AGENT,
            id: session_id(path),
            path: path.display().to_string(),
            cwd,
            title,
            ..Default::default()
        },
        ..Default::default()
    };
    // A tool's name and arguments arrive at its start; whether it worked, at its end.
    let mut started: HashMap<String, (String, Value)> = HashMap::new();
    for line in bytes.split(|&c| c == b'\n') {
        let Ok(r) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if r["checkpoint"] == true || r["ephemeral"] == true {
            continue;
        }
        let ts = text_at(&r["timestamp"]);
        if let Some(t) = &ts {
            widen(&mut b.session, t);
        }
        let id = text_at(&r["id"]).unwrap_or_default();
        let d = &r["data"];
        match r["type"].as_str().unwrap_or_default() {
            "session.start" | "session.resume" => {
                let c = &d["context"];
                if b.session.cwd.is_none() {
                    b.session.cwd = text_at(&c["cwd"]);
                }
                if let Some(branch) = text_at(&c["branch"]) {
                    b.session.git_branch = Some(branch);
                }
                if let Some(p) = text_at(&d["detachedFromSpawningParentSessionId"]) {
                    b.session.parent = Some(p);
                }
            }
            "user.message" => user(&mut b, id, ts, &r),
            "assistant.message" => {
                b.messages.extend(Message::extra(
                    format!("{id}:thinking"),
                    ts.clone(),
                    "thinking",
                    d["reasoningText"].as_str().unwrap_or_default(),
                ));
                let text = d["content"].as_str().unwrap_or_default().trim();
                if !text.is_empty() {
                    b.messages.push(Message {
                        uuid: id,
                        ts,
                        role: "assistant",
                        text: text.to_string(),
                    });
                }
            }
            "assistant.reasoning" => b.messages.extend(Message::extra(
                id,
                ts,
                "thinking",
                d["content"].as_str().unwrap_or_default(),
            )),
            "tool.execution_start" => {
                if let (Some(call), Some(name)) =
                    (text_at(&d["toolCallId"]), text_at(&d["toolName"]))
                {
                    started.insert(call, (name, d["arguments"].clone()));
                }
            }
            "tool.execution_complete" => tool_done(&mut b, ts, d, &mut started),
            "session.shutdown" => {
                for (model, m) in d["modelMetrics"].as_object().into_iter().flatten() {
                    b.usage
                        .push(usage(&b.session.id, model, ts.clone(), &m["usage"]));
                }
            }
            // Turn markers, mode and model changes, hooks, compaction, sub-agent lifecycle
            // and anything newer: nothing to index.
            _ => {}
        }
    }
    b
}

fn user(b: &mut Batch, id: String, ts: Option<String>, r: &Value) {
    let d = &r["data"];
    let text = d["content"].as_str().unwrap_or_default().trim().to_string();
    if text.is_empty() {
        return;
    }
    // Typed by the user, not injected (a skill's hidden prompt, a message between agents,
    // an autopilot continuation) or sent to a sub-agent.
    let typed = d["source"].as_str().is_none_or(|s| s == "user")
        && d["isAutopilotContinuation"] != true
        && r["agentId"].is_null();
    if typed && b.session.first_prompt.is_none() {
        b.session.first_prompt = Some(preview(&text));
    }
    b.messages.push(Message {
        uuid: id,
        ts,
        role: "user",
        text,
    });
}

fn tool_done(
    b: &mut Batch,
    ts: Option<String>,
    d: &Value,
    started: &mut HashMap<String, (String, Value)>,
) {
    let Some(call) = text_at(&d["toolCallId"]) else {
        return;
    };
    let res = &d["result"];
    let out = text_at(&res["content"])
        .or_else(|| text_at(&d["error"]["message"]))
        .unwrap_or_default();
    b.messages.extend(Message::extra(
        format!("{call}:result"),
        ts.clone(),
        "tool",
        &out,
    ));
    let (name, args) = started.remove(&call).unwrap_or_default();
    if d["success"] != true {
        return;
    }
    let cwd = b.session.cwd.clone();
    // Newer versions list the files a call changed; otherwise go by the tool.
    let listed: Vec<String> = d["fileEdits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| text_at(&e["path"]))
        .collect();
    let paths = if !listed.is_empty() {
        listed
    } else {
        match name.as_str() {
            "edit" | "create" | "str_replace_editor" | "write" => text_at(&args["path"])
                .filter(|_| args["command"].as_str().is_none_or(|c| c != "view"))
                .into_iter()
                .collect(),
            "apply_patch" => {
                let patch = text_at(&args["input"])
                    .or_else(|| text_at(&args["patch"]))
                    .or_else(|| text_at(&args))
                    .unwrap_or_default();
                patch_paths(&patch, cwd.as_deref())
            }
            _ => Vec::new(),
        }
    };
    let tool = if name.is_empty() { "edit".into() } else { name };
    let many = paths.len() > 1;
    for (i, p) in paths.into_iter().enumerate() {
        b.edits.push(FileEdit {
            id: if many {
                format!("{call}:{i}")
            } else {
                call.clone()
            },
            ts: ts.clone(),
            path: absolute(&p, cwd.as_deref()),
            tool: tool.clone(),
        });
    }
}

/// One row per model, keyed by session and model so a later shutdown's totals replace an
/// earlier one's. Copilot counts cached input inside `inputTokens`, OpenAI-style, and
/// reasoning inside `outputTokens`.
fn usage(session: &str, model: &str, ts: Option<String>, u: &Value) -> Usage {
    let n = |k: &str| u[k].as_i64().unwrap_or(0);
    let (read, write) = (n("cacheReadTokens"), n("cacheWriteTokens"));
    Usage {
        message_id: format!("{session}:{model}"),
        ts,
        model: model.to_string(),
        input: (n("inputTokens") - read - write).max(0),
        output: n("outputTokens"),
        cache_read: read,
        cache_write_5m: write,
        provider: PROVIDER.into(),
        reasoning: n("reasoningTokens"),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "3c1d2e4f-1111-4aaa-8bbb-000000000001";
    const ODD: &str = "3c1d2e4f-2222-4ccc-8ddd-000000000002";
    const FLAT: &str = "3c1d2e4f-3333-4eee-8fff-000000000003";

    fn fixture(rel: &str) -> (PathBuf, Vec<u8>) {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/copilot/session-state")
            .join(rel);
        let bytes = std::fs::read(&p).unwrap();
        (p, bytes)
    }

    #[test]
    fn session() {
        let (p, bytes) = fixture(&format!("{ID}/events.jsonl"));
        let b = parse(&p, &bytes);
        let s = &b.session;
        assert_eq!(s.id, ID);
        assert_eq!(s.parent, None);
        assert_eq!(s.cwd.as_deref(), Some("/work/shop"), "workspace.yaml");
        assert_eq!(s.title.as_deref(), Some("Add a cart badge"));
        assert_eq!(s.git_branch.as_deref(), Some("cart-badge"));
        assert_eq!(
            s.first_prompt.as_deref(),
            Some("Add a badge to the cart icon"),
            "not the skill's hidden prompt"
        );
        assert_eq!(s.started.as_deref(), Some("2026-10-01T09:00:00.000Z"));
        assert_eq!(s.ended.as_deref(), Some("2026-10-01T09:20:00.000Z"));

        let roles: Vec<_> = b
            .messages
            .iter()
            .map(|m| (m.role, m.uuid.as_str()))
            .collect();
        assert_eq!(
            roles,
            [
                ("user", "e-skill"),
                ("user", "e-u1"),
                ("thinking", "e-r1"),
                ("thinking", "e-a1:thinking"),
                ("assistant", "e-a1"),
                ("tool", "call-1:result"),
                ("tool", "call-3:result"),
                ("assistant", "e-a2"),
                ("user", "e-u2"),
                ("assistant", "e-a3"),
            ]
        );
        assert_eq!(b.messages[5].text, "Created src/cart.css");
        // call-2 printed nothing, so it has no entry.
        assert_eq!(b.messages[6].text, "no such file", "a failure's error");

        // `create` by its argument, `edit` by the listed fileEdits; the failed edit isn't one.
        assert_eq!(
            b.edits,
            [
                FileEdit {
                    id: "call-1".into(),
                    ts: Some("2026-10-01T09:00:06.000Z".into()),
                    path: "/work/shop/src/cart.css".into(),
                    tool: "create".into(),
                },
                FileEdit {
                    id: "call-2".into(),
                    ts: Some("2026-10-01T09:00:08.000Z".into()),
                    path: "/work/shop/src/cart.tsx".into(),
                    tool: "edit".into(),
                },
            ]
        );

        // Two shutdowns (the session was resumed); the second's totals replace the first's.
        let rows: Vec<_> = b
            .usage
            .iter()
            .map(|u| (u.message_id.as_str(), u.model.as_str()))
            .collect();
        let main = format!("{ID}:claude-sonnet-4.5");
        let mini = format!("{ID}:gpt-5-mini");
        assert_eq!(
            rows,
            [
                (main.as_str(), "claude-sonnet-4.5"),
                (main.as_str(), "claude-sonnet-4.5"),
                (mini.as_str(), "gpt-5-mini"),
            ]
        );
        let u = &b.usage[1];
        // 9000 input of which 6000 read from cache and 500 written to it.
        assert_eq!(
            (
                u.input,
                u.cache_read,
                u.cache_write_5m,
                u.output,
                u.reasoning
            ),
            (2500, 6000, 500, 700, 100)
        );
        assert_eq!(u.ts.as_deref(), Some("2026-10-01T09:20:00.000Z"));
        assert_eq!(u.provider, "github");
        assert_eq!(u.reported_cost, None, "premium requests aren't dollars");
    }

    #[test]
    fn unknown_records_and_bad_lines_are_skipped() {
        let (p, bytes) = fixture(&format!("{ODD}/events.jsonl"));
        let b = parse(&p, &bytes);
        assert_eq!(b.session.id, ODD);
        assert_eq!(
            b.session.cwd.as_deref(),
            Some("/work/api"),
            "no workspace.yaml"
        );
        assert_eq!(b.session.title, None);
        assert_eq!(
            b.session.parent.as_deref(),
            Some(ID),
            "a detached run rolls up into the session that started it"
        );
        let roles: Vec<_> = b
            .messages
            .iter()
            .map(|m| (m.role, m.uuid.as_str()))
            .collect();
        // The replayed checkpoint and the ephemeral record aren't counted again.
        assert_eq!(roles, [("user", "o-u1"), ("assistant", "o-a1")]);
        assert!(b.usage.is_empty(), "no shutdown yet");
    }

    #[test]
    fn truncated_session_keeps_whole_lines() {
        let (p, bytes) = fixture(&format!("{ID}/events.jsonl"));
        let cut = &bytes[..bytes.len() - 40];
        let b = parse(&p, cut);
        assert_eq!(b.session.id, ID);
        assert_eq!(b.usage.len(), 1, "the second shutdown, cut, is skipped");
        assert_eq!(b.edits.len(), 2);
    }

    #[test]
    fn later_chunk_still_knows_its_session() {
        let (p, bytes) = fixture(&format!("{ID}/events.jsonl"));
        let half = bytes.len() / 2;
        let start = half + bytes[half..].iter().position(|&c| c == b'\n').unwrap() + 1;
        let b = parse(&p, &bytes[start..]);
        assert_eq!(b.session.id, ID);
        assert_eq!(b.session.cwd.as_deref(), Some("/work/shop"));
    }

    #[test]
    fn older_flat_log() {
        let (p, bytes) = fixture(&format!("{FLAT}.jsonl"));
        let b = parse(&p, &bytes);
        assert_eq!(b.session.id, FLAT);
        assert_eq!(
            b.session.cwd.as_deref(),
            Some("/work/docs"),
            "session.start"
        );
        assert_eq!(b.session.first_prompt.as_deref(), Some("Explain the build"));
        assert_eq!(b.usage.len(), 1);
        assert_eq!((b.usage[0].input, b.usage[0].output), (400, 50));
    }

    #[test]
    fn only_event_logs_are_sessions() {
        let yes = [
            "/h/.copilot/session-state/abc.jsonl",
            "/h/.copilot/session-state/abc/events.jsonl",
            "/h/.copilot/session-state/abc/events.1.jsonl",
        ];
        let no = [
            "/h/.copilot/session-state/abc/workspace.yaml",
            "/h/.copilot/session-state/abc/checkpoints/x.jsonl",
            "/h/.copilot/session-state/abc/files/notes.jsonl",
            "/h/.copilot/logs/process-1.log",
            "/h/.copilot/session-state/abc/events.jsonl.tmp",
        ];
        for p in yes {
            assert!(is_session_file(Path::new(p)), "{p}");
        }
        for p in no {
            assert!(!is_session_file(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn workspace_yaml_skips_block_scalars() {
        let dir = std::env::temp_dir().join(format!("tb-copilot-{}", std::process::id()));
        let s = dir.join("session-state/x");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(
            s.join("workspace.yaml"),
            "id: x\ncwd: '/w/a'\nsummary: |\n  line one\n  cwd: /nope\n",
        )
        .unwrap();
        let (cwd, title) = workspace(&s.join("events.jsonl"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!((cwd.as_deref(), title), (Some("/w/a"), None));
    }
}
