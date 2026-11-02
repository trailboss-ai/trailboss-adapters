//! Agents trailboss lists sessions for but doesn't read usage from: it reads the first 8 KB
//! of each session file for an id, a folder and a preview, like the session-history Herdr
//! mod. The agent list and where each keeps its sessions come from that mod's catalog; the
//! code is trailboss's own.

use crate::model::{Batch, Session, preview, text_at};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct Agent {
    pub name: &'static str,
    /// Folders under the home directory.
    pub roots: &'static [&'static str],
    pub id: &'static [&'static str],
    pub cwd: &'static [&'static str],
    pub preview: &'static [&'static str],
    /// The resume command; `{id}` is replaced by the session id.
    pub resume: &'static [&'static str],
}

const ID: &[&str] = &["id", "sessionId"];
const CWD: &[&str] = &["cwd"];
const TITLE: &[&str] = &["title"];
const TITLE_OR_PROMPT: &[&str] = &["title", "firstPrompt"];

macro_rules! agent {
    ($name:literal, $roots:expr, $resume:expr) => {
        agent!($name, $roots, ID, CWD, TITLE, $resume)
    };
    ($name:literal, $roots:expr, $id:expr, $cwd:expr, $preview:expr, $resume:expr) => {
        Agent {
            name: $name,
            roots: $roots,
            id: $id,
            cwd: $cwd,
            preview: $preview,
            resume: $resume,
        }
    };
}

pub const AGENTS: &[Agent] = &[
    agent!(
        "gemini",
        &[".gemini/tmp"],
        &["sessionId", "id"],
        &[],
        TITLE,
        &["gemini"]
    ),
    agent!(
        "cursor",
        &[".cursor/chats"],
        ID,
        CWD,
        TITLE_OR_PROMPT,
        &["cursor-agent", "--resume", "{id}"]
    ),
    agent!(
        "droid",
        &[".factory/sessions"],
        ID,
        &["cwd", "workingDirectory"],
        TITLE,
        &["droid", "--resume", "{id}"]
    ),
    agent!(
        "muse",
        &[".local/share/muse/sessions"],
        ID,
        CWD,
        TITLE_OR_PROMPT,
        &["muse", "resume", "{id}"]
    ),
    agent!("qoder", &[".qoder/projects"], &["qoder"]),
    agent!("codebuddy", &[".codebuddy/projects"], &["codebuddy"]),
    agent!("kimi", &[".kimi/sessions"], &["kimi"]),
    agent!("kiro", &[".kiro/sessions"], &["kiro"]),
    agent!(
        "copilot",
        &[".copilot/sessions", ".copilot/cli/sessions"],
        &["copilot"]
    ),
    agent!("grok", &[".grok/sessions"], &["grok"]),
    agent!("omp", &[".omp/sessions"], &["omp"]),
    agent!("devin", &[".devin/sessions"], &["devin"]),
    agent!("openclaw", &[".openclaw/sessions"], &["openclaw"]),
    agent!(
        "antigravity",
        &[".antigravity/sessions", ".gemini/antigravity"],
        &["id"],
        CWD,
        TITLE,
        &["antigravity"]
    ),
    agent!(
        "prime",
        &[".prime/sessions"],
        &["id"],
        CWD,
        TITLE,
        &["prime-agent"]
    ),
    agent!("zcode", &[".zcode/projects"], &["zcode"]),
    agent!(
        "hermes",
        &[".hermes/sessions"],
        &["id"],
        CWD,
        TITLE,
        &["hermes"]
    ),
    agent!(
        "cline",
        &[".cline/sessions", ".cline/projects"],
        ID,
        CWD,
        TITLE_OR_PROMPT,
        &["cline"]
    ),
    agent!("rovo", &[".rovo/sessions"], &["rovo"]),
    agent!("jcode", &[".jcode/sessions", ".jcode/projects"], &["jcode"]),
];

pub fn find(name: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|a| a.name == name)
}

pub fn roots(a: &Agent) -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    a.roots.iter().map(|r| home.join(r)).collect()
}

pub fn is_session_file(p: &Path) -> bool {
    p.extension().is_some_and(|x| x == "json" || x == "jsonl")
}

/// The first JSON records in `head` (up to 10 lines), or the whole head as one document.
fn records(head: &[u8]) -> Vec<Value> {
    let lines: Vec<Value> = head
        .split(|&c| c == b'\n')
        .take(10)
        .filter_map(|l| serde_json::from_slice(l).ok())
        .filter(Value::is_object)
        .collect();
    if lines.is_empty() {
        serde_json::from_slice(head).ok().into_iter().collect()
    } else {
        lines
    }
}

/// A session from the start of its file. Each field comes from the first record with it.
pub fn parse(a: &Agent, path: &Path, head: &[u8]) -> Batch {
    let recs = records(head);
    let first = |keys: &[&str]| {
        recs.iter()
            .find_map(|r| keys.iter().find_map(|k| text_at(&r[*k])))
    };
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    Batch {
        session: Session {
            agent: a.name,
            id: first(a.id).unwrap_or(stem),
            path: path.display().to_string(),
            cwd: first(a.cwd),
            first_prompt: first(a.preview).map(|p| preview(&p)),
            ..Default::default()
        },
        ..Default::default()
    }
}

pub fn resume_argv(a: &Agent, id: &str) -> Vec<String> {
    a.resume.iter().map(|w| w.replace("{id}", id)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_fields_from_the_first_records() {
        let droid = find("droid").unwrap();
        let head = br#"{"type":"meta","id":"d-1"}
{"type":"x","workingDirectory":"/work/d","title":"Port the parser"}
"#;
        let b = parse(droid, Path::new("/h/.factory/sessions/f.jsonl"), head);
        assert_eq!(b.session.id, "d-1");
        assert_eq!(b.session.cwd.as_deref(), Some("/work/d"));
        assert_eq!(b.session.first_prompt.as_deref(), Some("Port the parser"));
        assert_eq!(resume_argv(droid, "d-1"), ["droid", "--resume", "d-1"]);
    }

    #[test]
    fn whole_document_and_fallback_id() {
        let gemini = find("gemini").unwrap();
        let head = b"{\n  \"title\": \"Fix CI\",\n  \"messages\": []\n}";
        let b = parse(gemini, Path::new("/h/.gemini/tmp/abc/chat-7.json"), head);
        assert_eq!(b.session.id, "chat-7", "no id field: the file name");
        assert_eq!(b.session.first_prompt.as_deref(), Some("Fix CI"));
        assert_eq!(resume_argv(gemini, "chat-7"), ["gemini"]);
    }

    #[test]
    fn every_agent_is_unique() {
        let mut names: Vec<_> = AGENTS.iter().map(|a| a.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), AGENTS.len());
    }
}
