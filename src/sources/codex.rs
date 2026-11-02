//! Codex. Sessions are `~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<uuid>.jsonl` (and
//! `archived_sessions/` once archived): one `{timestamp, type, payload}` record per line.

use crate::model::{
    Batch, FileEdit, Message, Session, Task, Usage, patch_paths, preview, text_at, text_of, widen,
};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const AGENT: &str = "codex";

pub fn roots() -> Vec<PathBuf> {
    let home = match std::env::var_os("CODEX_HOME") {
        Some(h) => PathBuf::from(h),
        None => match dirs::home_dir() {
            Some(h) => h.join(".codex"),
            None => return Vec::new(),
        },
    };
    vec![home.join("sessions"), home.join("archived_sessions")]
}

/// The session id is the uuid that ends the file name, so chunks after the first (which
/// holds `session_meta`) still know their session, and archiving the file keeps it.
fn session_id(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    // `rollout-….jsonl.zst` has `rollout-….jsonl` as its stem.
    let stem = stem.strip_suffix(".jsonl").unwrap_or(stem);
    match stem.char_indices().rev().nth(35) {
        Some((i, _)) if stem.len() - i == 36 => stem[i..].to_string(),
        _ => stem.to_string(),
    }
}

pub fn parse(path: &Path, bytes: &[u8]) -> Batch {
    let id = session_id(path);
    let mut b = Batch {
        session: Session {
            agent: AGENT,
            id: id.clone(),
            path: path.display().to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    // Set by each turn's `turn_context`. Usage before the first one in this chunk gets ""
    // and the store fills in the session's last known model.
    let mut model = String::new();
    // From `session_meta`, so only in the first chunk; later ones leave it to the store.
    let mut provider = String::new();
    let mut last_total = None;
    // Patch paths are relative to the turn's folder.
    let mut cwd: Option<String> = None;

    for line in bytes.split(|&c| c == b'\n') {
        let Ok(r) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let ts = text_at(&r["timestamp"]);
        if let Some(t) = &ts {
            widen(&mut b.session, t);
        }
        let p = &r["payload"];
        match (r["type"].as_str(), p["type"].as_str()) {
            (Some("session_meta"), _) => {
                b.session.cwd = b.session.cwd.take().or_else(|| text_at(&p["cwd"]));
                b.session.git_branch = text_at(&p["git"]["branch"]);
                provider = text_at(&p["model_provider"]).unwrap_or_default();
                // A spawned sub-agent names the thread that started it; thread ids are the
                // rollout uuids, i.e. our session ids. Review/compact sub-agents name none.
                b.session.parent =
                    text_at(&p["source"]["subagent"]["thread_spawn"]["parent_thread_id"]);
            }
            (Some("turn_context"), _) => {
                if let Some(m) = text_at(&p["model"]) {
                    model = m;
                }
                if let Some(c) = text_at(&p["cwd"]) {
                    cwd = Some(c);
                }
                if b.session.cwd.is_none() {
                    b.session.cwd = text_at(&p["cwd"]);
                }
            }
            (Some("response_item"), Some("custom_tool_call" | "function_call"))
                if p["name"] == "apply_patch" =>
            {
                // The patch is the call's input, or `{"input": …}` in its arguments.
                let patch = text_at(&p["input"]).or_else(|| {
                    let args = text_at(&p["arguments"])?;
                    let v: Value = serde_json::from_str(&args).ok()?;
                    text_at(&v["input"]).or(Some(args))
                });
                let call = text_at(&p["call_id"]).unwrap_or_else(|| ts.clone().unwrap_or_default());
                let here = cwd.as_deref().or(b.session.cwd.as_deref());
                for path in patch_paths(patch.as_deref().unwrap_or_default(), here) {
                    b.edits.push(FileEdit {
                        id: format!("{id}:{call}:{path}"),
                        ts: ts.clone(),
                        path,
                        tool: "apply_patch".into(),
                    });
                }
            }
            (Some("response_item"), Some("message")) => {
                let role = match p["role"].as_str() {
                    Some("user") => "user",
                    Some("assistant") => "assistant",
                    _ => continue, // developer/system instructions
                };
                let text = text_of(&p["content"]);
                // Codex injects context (`<environment_context>`, ...) as user messages.
                if text.is_empty() || text.starts_with('<') {
                    continue;
                }
                if role == "user" && b.session.first_prompt.is_none() {
                    b.session.first_prompt = Some(preview(&text));
                }
                let uuid = text_at(&p["id"]).unwrap_or_else(|| {
                    format!("{id}:{}:{role}", ts.as_deref().unwrap_or_default())
                });
                b.messages.push(Message {
                    uuid,
                    ts,
                    role,
                    text,
                });
            }
            (Some("event_msg"), Some("task_complete")) => {
                if let Some(ms) = p["duration_ms"].as_i64() {
                    b.tasks.push(Task {
                        id: format!("{id}:{}", text_at(&p["turn_id"]).unwrap_or_default()),
                        ts,
                        duration_ms: ms,
                    });
                }
            }
            (Some("event_msg"), Some("token_count")) => {
                let info = &p["info"];
                let last = &info["last_token_usage"];
                if !last.is_object() {
                    continue;
                }
                // Codex repeats the event when nothing changed; the running total tells.
                let total = info["total_token_usage"]["total_tokens"].as_i64();
                if total.is_some() && total == last_total {
                    continue;
                }
                last_total = total;
                let n = |k: &str| last[k].as_i64().unwrap_or(0);
                let cached = n("cached_input_tokens");
                b.usage.push(Usage {
                    message_id: match total {
                        Some(t) => format!("{id}:{t}"),
                        None => format!("{id}:{}", ts.as_deref().unwrap_or_default()),
                    },
                    ts,
                    model: model.clone(),
                    // OpenAI counts cached tokens inside input_tokens; reasoning inside output.
                    input: (n("input_tokens") - cached).max(0),
                    output: n("output_tokens"),
                    cache_read: cached,
                    cache_write_5m: n("cache_write_input_tokens"),
                    cache_write_1h: 0,
                    reported_cost: None,
                    branch: None,
                    provider: provider.clone(),
                    reasoning: n("reasoning_output_tokens"),
                    cwd: cwd.clone(),
                });
            }
            _ => {}
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "rollout-2026-10-01T10-00-00-0199aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee.jsonl";

    fn fixture() -> (PathBuf, Vec<u8>) {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex")
            .join(NAME);
        let bytes = std::fs::read(&p).unwrap();
        (p, bytes)
    }

    #[test]
    fn rollout() {
        let (p, bytes) = fixture();
        let b = parse(&p, &bytes);
        let s = &b.session;
        assert_eq!(s.id, "0199aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee");
        assert_eq!(s.cwd.as_deref(), Some("/work/api"));
        assert_eq!(s.git_branch.as_deref(), Some("dev"));
        assert_eq!(
            s.first_prompt.as_deref(),
            Some("Add rate limiting to the API")
        );

        // The repeated token_count is skipped; cached tokens are split out of input.
        assert_eq!(b.usage.len(), 1);
        let u = &b.usage[0];
        assert_eq!(u.model, "gpt-5.5");
        assert_eq!((u.input, u.cache_read, u.output), (200, 800, 50));
        assert_eq!((u.provider.as_str(), u.reasoning), ("openai", 10));
        assert_eq!(b.tasks[0].duration_ms, 4300);

        // Developer instructions and injected context aren't indexed.
        let roles: Vec<_> = b.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, ["user", "assistant"]);
    }

    #[test]
    fn compressed_rollouts_are_indexed_whole() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "tests/fixtures/codex/rollout-2026-10-02T10-00-00-0199aaaa-bbbb-7ccc-8ddd-ffffffffffff.jsonl.zst",
        );
        let mut st = crate::store::Store::in_memory().unwrap();
        assert!(st.index_zst(&p, &parse).unwrap());
        assert!(!st.index_zst(&p, &parse).unwrap(), "read once");
        let s = st
            .session(AGENT, "0199aaaa-bbbb-7ccc-8ddd-ffffffffffff")
            .unwrap()
            .unwrap();
        assert_eq!(s["title"], "Add rate limiting to the API");
    }

    #[test]
    fn spawned_subagent_links_to_parent_and_its_cost_rolls_up() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex");
        let child =
            dir.join("rollout-2026-10-01T10-05-00-0199aaaa-bbbb-7ccc-8ddd-111111111111.jsonl");
        let b = parse(&child, &std::fs::read(&child).unwrap());
        assert_eq!(
            b.session.parent.as_deref(),
            Some("0199aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee")
        );

        let mut st = crate::store::Store::in_memory().unwrap();
        st.index_file(&dir.join(NAME), &parse).unwrap();
        st.index_file(&child, &parse).unwrap();
        let listed = st.sessions(10).unwrap();
        assert_eq!(listed.len(), 1, "the sub-agent isn't listed on its own");
        // Parent: 1000 input (800 cached) + 50 output; child: 300 input + 30 output.
        assert_eq!(listed[0]["tokens"], 1380);
    }

    #[test]
    fn review_subagent_has_no_parent() {
        let line =
            br#"{"type":"session_meta","payload":{"id":"x","source":{"subagent":"review"}}}"#;
        assert_eq!(parse(Path::new("/a/x.jsonl"), line).session.parent, None);
    }

    #[test]
    fn later_chunk_keeps_session_and_leaves_model_to_the_store() {
        let line = br#"{"timestamp":"2026-10-01T11:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":99},"last_token_usage":{"input_tokens":5,"output_tokens":1}}}}"#;
        let b = parse(Path::new(&format!("/a/{NAME}")), line);
        assert_eq!(b.session.id, "0199aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee");
        assert_eq!(b.usage[0].model, "");
        assert_eq!(
            b.usage[0].message_id,
            "0199aaaa-bbbb-7ccc-8ddd-eeeeeeeeeeee:99"
        );
    }
}
