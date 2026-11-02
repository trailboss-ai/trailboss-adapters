//! Claude Code. Sessions are `~/.claude/projects/<folder>/<session>.jsonl`, one JSON record
//! per line; sub-agents are `<folder>/<session>/subagents/agent-<id>.jsonl`.

use crate::model::{
    Backup, Batch, FileEdit, Message, Session, Task, Usage, absolute, preview, text_at, text_of,
    widen,
};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const AGENT: &str = "claude";

/// `~/.claude/projects` (or `$CLAUDE_CONFIG_DIR/projects`), plus every ReClaude account's
/// `~/.rec/claude/accounts/<name>/config/projects`.
pub fn roots() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut out = vec![match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir).join("projects"),
        None => home.join(".claude").join("projects"),
    }];
    if let Ok(accounts) = std::fs::read_dir(home.join(".rec/claude/accounts")) {
        out.extend(accounts.flatten().map(|a| a.path().join("config/projects")));
    }
    out
}

/// Parse complete lines of a session file. Lines that aren't JSON, and record types we
/// don't know, are skipped: Claude Code adds new ones often.
pub fn parse(path: &Path, bytes: &[u8]) -> Batch {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let is_sub = path
        .parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "subagents");
    let mut b = Batch {
        session: Session {
            agent: AGENT,
            id,
            path: path.display().to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    // Claude Code writes one record per content block, each repeating the response's usage.
    let mut seen = HashSet::new();
    for line in bytes.split(|&c| c == b'\n') {
        if let Ok(r) = serde_json::from_slice::<Value>(line) {
            record(&mut b, &r, is_sub, &mut seen);
        }
    }
    b
}

/// One file-history entry. Relative paths come with the file's resolved folder.
fn backup(b: &mut Batch, path: &str, v: &Value) {
    let Some(version) = v["version"].as_i64() else {
        return;
    };
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        match (text_at(&v["realParentDir"]), path.rsplit('/').next()) {
            (Some(dir), Some(name)) => format!("{dir}/{name}"),
            _ => absolute(path, b.session.cwd.as_deref()),
        }
    };
    b.backups.push(Backup {
        path,
        version,
        backup: text_at(&v["backupFileName"]),
        ts: text_at(&v["backupTime"]),
    });
}

fn record(b: &mut Batch, r: &Value, is_sub: bool, seen: &mut HashSet<String>) {
    let ts = text_at(&r["timestamp"]);
    let s = &mut b.session;
    if let Some(t) = &ts {
        widen(s, t);
    }
    if s.cwd.is_none() {
        s.cwd = text_at(&r["cwd"]);
    }
    if let Some(branch) = text_at(&r["gitBranch"]) {
        s.git_branch = Some(branch);
    }
    if is_sub && s.parent.is_none() {
        s.parent = text_at(&r["sessionId"]);
    }
    let uuid = text_at(&r["uuid"]);

    match r["type"].as_str() {
        // Claude Code's file history: what each file was before the session changed it.
        Some("file-history-delta") => {
            if let Some(p) = text_at(&r["trackingPath"]) {
                backup(b, &p, &r["backup"]);
            }
        }
        Some("file-history-snapshot") => {
            if let Some(files) = r["snapshot"]["trackedFileBackups"].as_object() {
                for (p, v) in files {
                    backup(b, p, v);
                }
            }
        }
        // Claude Code times each turn itself.
        Some("system") if r["subtype"] == "turn_duration" => {
            if let (Some(id), Some(ms)) = (uuid, r["durationMs"].as_i64()) {
                b.tasks.push(Task {
                    id,
                    ts,
                    duration_ms: ms,
                });
            }
        }
        Some("ai-title") => {
            if let Some(t) = text_at(&r["aiTitle"]) {
                s.title = Some(t);
            }
        }
        Some("user") if r["isMeta"] != true => {
            let text = text_of(&r["message"]["content"]);
            if text.is_empty() {
                return; // tool results
            }
            // Slash commands and injected context start with a tag; they make poor previews.
            if s.first_prompt.is_none() && !text.starts_with('<') {
                s.first_prompt = Some(preview(&text));
            }
            if let Some(uuid) = uuid {
                b.messages.push(Message {
                    uuid,
                    ts,
                    role: "user",
                    text,
                });
            }
        }
        Some("assistant") => {
            let m = &r["message"];
            // File edits, from the tools that write files. Shell commands that change files
            // aren't visible here; `trailboss changes --worktree` estimates those.
            for block in m["content"].as_array().into_iter().flatten() {
                let tool = block["name"].as_str().unwrap_or_default();
                if block["type"] != "tool_use"
                    || !matches!(tool, "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
                {
                    continue;
                }
                let input = &block["input"];
                let Some(path) =
                    text_at(&input["file_path"]).or_else(|| text_at(&input["notebook_path"]))
                else {
                    continue;
                };
                b.edits.push(FileEdit {
                    id: text_at(&block["id"])
                        .unwrap_or_else(|| format!("{}:{path}", ts.as_deref().unwrap_or_default())),
                    ts: ts.clone(),
                    path: absolute(&path, b.session.cwd.as_deref()),
                    tool: tool.to_string(),
                });
            }
            let text = text_of(&m["content"]);
            if let (false, Some(uuid)) = (text.is_empty(), uuid) {
                b.messages.push(Message {
                    uuid,
                    ts: ts.clone(),
                    role: "assistant",
                    text,
                });
            }
            let (Some(id), Some(model)) = (m["id"].as_str(), m["model"].as_str()) else {
                return;
            };
            // "<synthetic>" marks messages Claude Code made up locally; no tokens were used.
            if model.starts_with('<') || !seen.insert(id.to_string()) {
                return;
            }
            let u = &m["usage"];
            let n = |v: &Value| v.as_i64().unwrap_or(0);
            let (w5, w1) = match &u["cache_creation"] {
                c @ Value::Object(_) => (
                    n(&c["ephemeral_5m_input_tokens"]),
                    n(&c["ephemeral_1h_input_tokens"]),
                ),
                _ => (n(&u["cache_creation_input_tokens"]), 0),
            };
            b.usage.push(Usage {
                message_id: id.to_string(),
                ts,
                model: model.to_string(),
                input: n(&u["input_tokens"]),
                output: n(&u["output_tokens"]),
                cache_read: n(&u["cache_read_input_tokens"]),
                cache_write_5m: w5,
                cache_write_1h: w1,
                reported_cost: None,
                branch: text_at(&r["gitBranch"]),
                provider: text_at(&m["provider"]).unwrap_or_else(|| "anthropic".into()),
                reasoning: n(&u["output_tokens_details"]["thinking_tokens"]),
                cwd: text_at(&r["cwd"]),
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> (PathBuf, Vec<u8>) {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/claude")
            .join(name);
        let bytes = std::fs::read(&p).unwrap();
        (p, bytes)
    }

    #[test]
    fn normal_session() {
        let (p, bytes) = fixture("s-normal.jsonl");
        let b = parse(&p, &bytes);
        let s = &b.session;
        assert_eq!(s.id, "s-normal");
        assert_eq!(s.parent, None);
        assert_eq!(s.cwd.as_deref(), Some("/work/app"));
        assert_eq!(s.git_branch.as_deref(), Some("main"));
        assert_eq!(s.title.as_deref(), Some("Fix flaky login test"));
        assert_eq!(s.first_prompt.as_deref(), Some("Fix the flaky login test"));
        assert_eq!(s.started.as_deref(), Some("2026-10-01T10:00:00.000Z"));
        assert_eq!(s.ended.as_deref(), Some("2026-10-01T10:00:09.000Z"));

        // One response split over two records, plus a synthetic one: counted once.
        assert_eq!(b.usage.len(), 1);
        let u = &b.usage[0];
        assert_eq!(u.model, "claude-opus-5-5");
        assert_eq!((u.input, u.output, u.cache_read), (10, 50, 1000));
        assert_eq!((u.cache_write_5m, u.cache_write_1h), (0, 200));
        assert_eq!(u.provider, "anthropic");
        assert_eq!(b.tasks.len(), 1);
        assert_eq!(b.backups.len(), 2);
        assert_eq!(b.backups[0].backup.as_deref(), Some("abc123@v1"));
        assert_eq!(
            b.backups[1].path, "/work/app/src/new.rs",
            "relative path, resolved folder"
        );
        assert_eq!(b.backups[1].backup, None, "created by the session");
        assert_eq!(
            (b.tasks[0].id.as_str(), b.tasks[0].duration_ms),
            ("td1", 8000)
        );

        // The meta message, thinking-only record and tool result aren't searchable text.
        let roles: Vec<_> = b.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, ["user", "assistant", "assistant"]);
        assert!(b.messages[1].text.contains("session cookie"));
    }

    #[test]
    fn unknown_records_and_bad_lines_are_skipped() {
        let (p, bytes) = fixture("s-unknown.jsonl");
        let b = parse(&p, &bytes);
        assert_eq!(b.messages.len(), 1);
        assert_eq!(b.session.first_prompt.as_deref(), Some("still parsed"));
    }

    #[test]
    fn subagent_links_to_parent() {
        let p = Path::new("/x/proj/s-normal/subagents/agent-a1.jsonl");
        let line = br#"{"type":"user","uuid":"q1","sessionId":"s-normal","isSidechain":true,"timestamp":"2026-10-01T10:00:01.000Z","message":{"role":"user","content":"look around"}}"#;
        let b = parse(p, line);
        assert_eq!(b.session.id, "agent-a1");
        assert_eq!(b.session.parent.as_deref(), Some("s-normal"));
    }

    #[test]
    fn old_usage_without_cache_breakdown() {
        let line = br#"{"type":"assistant","uuid":"a","message":{"id":"m","model":"claude-x","content":[],"usage":{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":30}}}"#;
        let b = parse(Path::new("/x/s.jsonl"), line);
        assert_eq!(
            (b.usage[0].cache_write_5m, b.usage[0].cache_write_1h),
            (30, 0)
        );
    }
}
