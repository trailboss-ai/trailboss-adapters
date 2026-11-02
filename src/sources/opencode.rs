//! opencode. Everything lives in one SQLite database, `~/.local/share/opencode/opencode.db`:
//! newer versions use `session_v2` + `session_message`, older ones `session` + `message` +
//! `part`. The database also holds account credentials, so it's opened read-only and only
//! the session and message tables are ever queried.

use crate::error::{Error, Result};
use crate::model::{Batch, Message, Session, Usage, preview, text_at, text_of};
use crate::store::Store;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const AGENT: &str = "opencode";

pub fn databases() -> Vec<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")));
    data.map(|d| d.join("opencode").join("opencode.db"))
        .into_iter()
        .filter(|p| p.is_file())
        .collect()
}

/// Index what changed since the last run. Messages are rewritten while they stream, so the
/// bookmark is the newest `time_updated` seen, not a file offset.
pub fn index(st: &mut Store, db: &Path) -> Result<bool> {
    let key = format!("opencode:{}", db.display());
    let since = st.offset(&key)?;
    let src = |source| Error::Source {
        path: db.to_path_buf(),
        source,
    };
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(src)?;
    conn.busy_timeout(Duration::from_secs(2)).map_err(src)?;
    let (batches, newest) = read(&conn, since).map_err(src)?;
    if newest <= since {
        return Ok(false);
    }
    st.save(&key, &batches, newest)?;
    Ok(true)
}

/// Sessions with messages updated at or after `since` (ms since the epoch), and the newest
/// update time seen. Rows at exactly `since` are read again; saving them twice is harmless.
pub fn read(conn: &Connection, since: i64) -> rusqlite::Result<(Vec<Batch>, i64)> {
    let v2: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'session_v2'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    const ISO: &str = "'%Y-%m-%dT%H:%M:%fZ'";
    let (sessions, messages) = if v2 {
        (
            "session_v2".to_string(),
            format!(
                "SELECT id, session_id, type, data, time_updated, NULL,
                        strftime({ISO}, time_created / 1000.0, 'unixepoch')
                 FROM session_message
                 WHERE time_updated >= ?1 AND type IN ('user', 'assistant')
                 ORDER BY time_created, seq"
            ),
        )
    } else {
        (
            "session".to_string(),
            format!(
                "SELECT m.id, m.session_id, json_extract(m.data, '$.role'), m.data, m.time_updated,
                        (SELECT group_concat(json_extract(p.data, '$.text'), char(10)) FROM part p
                         WHERE p.message_id = m.id AND json_extract(p.data, '$.type') = 'text'),
                        strftime({ISO}, m.time_created / 1000.0, 'unixepoch')
                 FROM message m
                 WHERE m.time_updated >= ?1
                 ORDER BY m.time_created"
            ),
        )
    };

    let mut batches: BTreeMap<String, Batch> = BTreeMap::new();
    let mut newest = since;
    let mut st = conn.prepare(&format!(
        "SELECT id, parent_id, directory, title, time_updated,
                strftime({ISO}, time_created / 1000.0, 'unixepoch'),
                strftime({ISO}, time_updated / 1000.0, 'unixepoch')
         FROM {sessions}"
    ))?;
    let mut rows = st.query([])?;
    let mut all = BTreeMap::new();
    while let Some(r) = rows.next()? {
        let id: String = r.get(0)?;
        let updated: i64 = r.get(4)?;
        let s = Session {
            agent: AGENT,
            id: id.clone(),
            parent: r.get(1)?,
            path: String::new(),
            cwd: r.get(2)?,
            title: r.get::<_, Option<String>>(3)?.filter(|t| !t.is_empty()),
            started: r.get(5)?,
            ended: r.get(6)?,
            ..Default::default()
        };
        if updated >= since {
            newest = newest.max(updated);
            batches.insert(id.clone(), batch(s.clone()));
        }
        all.insert(id, s);
    }

    let mut st = conn.prepare(&messages)?;
    let mut rows = st.query([since])?;
    while let Some(r) = rows.next()? {
        let (id, sid): (String, String) = (r.get(0)?, r.get(1)?);
        let role: Option<String> = r.get(2)?;
        let data: Value = serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default();
        newest = newest.max(r.get(4)?);
        let parts: Option<String> = r.get(5)?;
        let ts: Option<String> = r.get(6)?;
        let Some(session) = all.get(&sid) else {
            continue;
        };
        let b = batches.entry(sid).or_insert_with(|| batch(session.clone()));
        // File edits from opencode's edit / write / patch tools (newer schema).
        for part in data["content"].as_array().into_iter().flatten() {
            let tool = part["tool"].as_str().unwrap_or_default();
            if part["type"] != "tool" || !matches!(tool, "edit" | "write" | "patch" | "multiedit") {
                continue;
            }
            let Some(path) = text_at(&part["state"]["input"]["filePath"]) else {
                continue;
            };
            b.edits.push(crate::model::FileEdit {
                id: format!(
                    "{id}:{}",
                    text_at(&part["callID"]).unwrap_or_else(|| path.clone())
                ),
                ts: ts.clone(),
                path: crate::model::absolute(&path, b.session.cwd.as_deref()),
                tool: tool.to_string(),
            });
        }
        let (role, text) = match role.as_deref() {
            Some("user") => (
                "user",
                parts.or_else(|| text_at(&data["text"])).unwrap_or_default(),
            ),
            Some("assistant") => (
                "assistant",
                parts.unwrap_or_else(|| text_of(&data["content"])),
            ),
            _ => continue,
        };
        let text = text.trim().to_string();
        // Count usage once the response is complete; until then its tokens keep changing.
        if role == "assistant" && data["time"]["completed"].is_number() {
            let t = &data["tokens"];
            let n = |v: &Value| v.as_i64().unwrap_or(0);
            b.usage.push(Usage {
                message_id: id.clone(),
                ts: ts.clone(),
                model: text_at(&data["model"]["id"])
                    .or_else(|| text_at(&data["modelID"]))
                    .unwrap_or_default(),
                input: n(&t["input"]),
                // opencode reports reasoning apart from output; both are billed as output.
                output: n(&t["output"]) + n(&t["reasoning"]),
                cache_read: n(&t["cache"]["read"]),
                cache_write_5m: n(&t["cache"]["write"]),
                cache_write_1h: 0,
                reported_cost: data["cost"].as_f64(),
                branch: None,
                provider: text_at(&data["model"]["providerID"])
                    .or_else(|| text_at(&data["providerID"]))
                    .unwrap_or_default(),
                reasoning: n(&t["reasoning"]),
                cwd: text_at(&data["path"]["cwd"]),
            });
        }
        // A streaming reply's text is partial; index it once it's complete.
        let unfinished = role == "assistant" && !data["time"]["completed"].is_number();
        if text.is_empty() || unfinished {
            continue;
        }
        if role == "user" && b.session.first_prompt.is_none() {
            b.session.first_prompt = Some(preview(&text));
        }
        b.messages.push(Message {
            uuid: id,
            ts,
            role,
            text,
        });
    }
    Ok((batches.into_values().collect(), newest))
}

fn batch(session: Session) -> Batch {
    Batch {
        session,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            r#"
            CREATE TABLE session_v2 (id TEXT, parent_id TEXT, directory TEXT, title TEXT,
              time_created INTEGER, time_updated INTEGER);
            CREATE TABLE session_message (id TEXT, session_id TEXT, type TEXT, seq INTEGER,
              time_created INTEGER, time_updated INTEGER, data TEXT);
            CREATE TABLE credential (value TEXT);
            INSERT INTO session_v2 VALUES ('ses_1', NULL, '/work/web', 'Fix the navbar',
              1790000000000, 1790000009000);
            INSERT INTO session_message VALUES
             ('msg_u', 'ses_1', 'user', 0, 1790000000000, 1790000000000,
              '{"text":"The navbar overlaps the logo","time":{"created":1790000000000}}'),
             ('msg_a', 'ses_1', 'assistant', 1, 1790000001000, 1790000009000,
              '{"model":{"id":"claude-sonnet-4-6","providerID":"anthropic"},"content":[{"type":"reasoning","text":"hmm"},{"type":"tool","tool":"edit","callID":"c1","state":{"input":{"filePath":"src/nav.css"}}},{"type":"text","text":"Raised the z-index."}],"cost":0.5,"tokens":{"input":10,"output":20,"reasoning":5,"cache":{"read":300,"write":40}},"time":{"created":1790000001000,"completed":1790000009000}}'),
             ('msg_s', 'ses_1', 'assistant', 2, 1790000010000, 1790000010000,
              '{"model":{"id":"claude-sonnet-4-6"},"content":[],"tokens":{"input":1},"time":{"created":1790000010000}}'),
             ('msg_x', 'ses_1', 'idle', 3, 1790000011000, 1790000011000, '{}');
            "#,
        )
        .unwrap();
        c
    }

    #[test]
    fn reads_v2_and_skips_unfinished_responses() {
        let (batches, newest) = read(&v2(), 0).unwrap();
        assert_eq!(newest, 1790000010000, "the idle row is filtered out");
        let b = &batches[0];
        assert_eq!(b.session.id, "ses_1");
        assert_eq!(b.session.title.as_deref(), Some("Fix the navbar"));
        assert_eq!(b.session.cwd.as_deref(), Some("/work/web"));
        assert_eq!(
            b.session.started.as_deref(),
            Some("2026-09-21T14:13:20.000Z")
        );
        assert_eq!(
            b.session.first_prompt.as_deref(),
            Some("The navbar overlaps the logo")
        );

        // Only the completed response counts; reasoning is billed as output.
        assert_eq!(b.usage.len(), 1);
        let u = &b.usage[0];
        assert_eq!(
            (u.input, u.output, u.cache_read, u.cache_write_5m),
            (10, 25, 300, 40)
        );
        assert_eq!(u.reported_cost, Some(0.5));
        assert_eq!((u.provider.as_str(), u.reasoning), ("anthropic", 5));
        assert_eq!(b.messages.len(), 2);
        assert_eq!(b.edits.len(), 1);
        assert_eq!(
            (b.edits[0].path.as_str(), b.edits[0].tool.as_str()),
            ("/work/web/src/nav.css", "edit")
        );
        assert_eq!(b.messages[1].text, "Raised the z-index.");

        let (batches, _) = read(&v2(), newest + 1).unwrap();
        assert!(batches.is_empty(), "nothing new");
    }

    #[test]
    fn reads_v1() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            r#"
            CREATE TABLE session (id TEXT, parent_id TEXT, directory TEXT, title TEXT,
              time_created INTEGER, time_updated INTEGER);
            CREATE TABLE message (id TEXT, session_id TEXT, time_created INTEGER,
              time_updated INTEGER, data TEXT);
            CREATE TABLE part (id TEXT, message_id TEXT, data TEXT);
            INSERT INTO session VALUES ('ses_old', NULL, '/work/old', 'Old one', 1, 3000);
            INSERT INTO message VALUES
             ('m1', 'ses_old', 1000, 1000, '{"role":"user","time":{"created":1000}}'),
             ('m2', 'ses_old', 2000, 3000, '{"role":"assistant","modelID":"gpt-5.5","tokens":{"input":7,"output":3,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":2000,"completed":3000}}');
            INSERT INTO part VALUES ('p1', 'm1', '{"type":"text","text":"hello there"}'),
                                    ('p2', 'm2', '{"type":"step-start"}'),
                                    ('p3', 'm2', '{"type":"text","text":"hi"}');
            "#,
        )
        .unwrap();
        let (batches, _) = read(&c, 0).unwrap();
        let b = &batches[0];
        assert_eq!(b.session.first_prompt.as_deref(), Some("hello there"));
        assert_eq!(b.usage[0].model, "gpt-5.5");
        assert_eq!(b.messages[1].text, "hi");
    }
}
