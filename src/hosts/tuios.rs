//! tuios: a terminal multiplexer that also answers Herdr's socket API (Herdr 0.9.5,
//! protocol 22) on a socket of its own, `$XDG_RUNTIME_DIR/tuios/tuios.sock.herdr`. That's a
//! different API from `hc`'s, so this host speaks the socket directly: one JSON request per
//! connection, one JSON line back, `{"id","method","params"}` → `{"id","result"}` or
//! `{"id","error":{"code","message"}}`.
//!
//! A Herdr workspace there is a tuios session and a pane is a tuios window, addressed
//! `w<session>:p<window>`; the address lasts as long as the window does. Called from outside
//! every tuios pane, Trailboss acts as the person; from inside one, tuios holds it to that
//! pane's grants (reading needs `read`, typing `write`).
//!
//! ponytail: the default server only; add `TUIOS_SERVER` / `--server` named servers
//! (`<dir>/servers/<name>`) if someone runs several.

use super::{Detail, Host, Listed};
use crate::model::text_at;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// tuios answers at once; this only stops a hung daemon from hanging Trailboss.
const TIMEOUT: Duration = Duration::from_secs(5);
/// A screen of text is far smaller; more than this is not an answer we want.
const MAX_REPLY: u64 = 16 << 20;
const SOCKET: &str = "tuios.sock.herdr";

pub struct Tuios {
    /// None when no tuios socket was found: tuios isn't running.
    socket: Option<PathBuf>,
}

impl Tuios {
    /// The default server's socket, wherever tuios puts it.
    pub fn new() -> Self {
        Self {
            socket: find_socket(),
        }
    }

    /// A host on a given socket (tests, or a server elsewhere).
    pub fn at(socket: PathBuf) -> Self {
        Self {
            socket: Some(socket),
        }
    }

    /// The socket this host talks to, if one was found.
    pub fn socket(&self) -> Option<&Path> {
        self.socket.as_deref()
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let path = self.socket.as_ref().ok_or("tuios isn't running")?;
        let mut stream = UnixStream::connect(path).map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(TIMEOUT))
            .and_then(|()| stream.set_write_timeout(Some(TIMEOUT)))
            .map_err(|e| e.to_string())?;
        let mut req = json!({ "id": "trailboss", "method": method, "params": params }).to_string();
        req.push('\n');
        stream
            .write_all(req.as_bytes())
            .map_err(|e| e.to_string())?;
        let mut line = String::new();
        BufReader::new(stream.take(MAX_REPLY))
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        let reply: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        if let Some(e) = reply.get("error") {
            // tuios's messages name the problem (a missing pane, a refused grant), never
            // what was being typed.
            let code = e["code"].as_str().unwrap_or("error");
            return Err(match e["message"].as_str() {
                Some(m) if !m.is_empty() => format!("{code}: {m}"),
                _ => code.to_string(),
            });
        }
        Ok(reply["result"].clone())
    }
}

impl Default for Tuios {
    fn default() -> Self {
        Self::new()
    }
}

/// Where the socket may be, from the environment only: inside a tuios pane, `TUIOS_SOCKET`
/// names its daemon's socket (named servers included), and the Herdr one sits beside it;
/// else the default server's, in `$XDG_RUNTIME_DIR/tuios`.
///
/// ponytail: tuios falls back to `/run/user/<uid>` and `/tmp/tuios-<uid>` without
/// `XDG_RUNTIME_DIR`; those aren't tried, so a cleared environment (tests) never reaches a
/// real tuios.
fn candidates() -> Vec<PathBuf> {
    let pane = std::env::var_os("TUIOS_SOCKET")
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut s = s;
            s.push(".herdr");
            PathBuf::from(s)
        });
    let default = dirs::runtime_dir().map(|rt| rt.join("tuios").join(SOCKET));
    pane.into_iter().chain(default).collect()
}

fn find_socket() -> Option<PathBuf> {
    candidates().into_iter().find(|p| ours(p))
}

/// A socket owned by this user. Whoever can make a socket in a shared folder (/tmp) could
/// otherwise read what Trailboss types.
fn ours(p: &Path) -> bool {
    let me = std::fs::metadata("/proc/self").map(|m| m.uid()).ok();
    std::fs::symlink_metadata(p)
        .is_ok_and(|m| m.file_type().is_socket() && me.is_none_or(|u| m.uid() == u))
}

fn pid(v: &Value) -> Option<u32> {
    v.as_u64()
        .and_then(|p| u32::try_from(p).ok())
        .filter(|p| *p > 0)
}

/// One pane of a `session.snapshot`; `sessions` maps a workspace id to its label.
fn listed(p: &Value, sessions: &HashMap<&str, &str>) -> Option<Listed> {
    let session = p["workspace_id"].as_str().and_then(|w| sessions.get(w));
    let pane = text_at(&p["label"]).or_else(|| text_at(&p["title"]));
    let place: Vec<&str> = session
        .copied()
        .into_iter()
        .chain(pane.as_deref())
        .collect();
    Some(Listed {
        address: text_at(&p["pane_id"])?,
        location: Some(
            format!("(tuios) {}", place.join(" / "))
                .trim_end()
                .to_string(),
        ),
        // Herdr's words: working, blocked, done (finished, not yet seen), idle.
        status: text_at(&p["agent_status"]).filter(|s| s != "unknown"),
        program: text_at(&p["agent"]),
    })
}

impl Host for Tuios {
    fn name(&self) -> &'static str {
        "tuios"
    }

    /// None when no tuios is listening.
    fn list(&self) -> Option<Vec<Listed>> {
        let snap = self.call("session.snapshot", json!({})).ok()?;
        let snap = &snap["snapshot"];
        let sessions: HashMap<&str, &str> = snap["workspaces"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|w| Some((w["workspace_id"].as_str()?, w["label"].as_str()?)))
            .collect();
        Some(
            snap["panes"]
                .as_array()?
                .iter()
                .filter_map(|p| listed(p, &sessions))
                .collect(),
        )
    }

    /// The pane's shell and its terminal's foreground process group, whose leader is the
    /// shell at a prompt or the program it runs (as tmux's `tpgid`).
    fn get(&self, address: &str) -> Option<Detail> {
        let r = self
            .call("pane.process_info", json!({ "pane_id": address }))
            .ok()?;
        let info = &r["process_info"];
        Some(Detail {
            shell_pid: pid(&info["shell_pid"]),
            foreground_pid: pid(&info["foreground_process_group_id"]),
            ..Default::default()
        })
    }

    /// tuios pastes the text (control characters dropped, bracketed paste when the program
    /// asks for it), then presses Enter.
    fn send(&self, address: &str, text: &str) -> Result<(), String> {
        self.call(
            "pane.send_input",
            json!({ "pane_id": address, "text": text, "keys": ["Enter"] }),
        )
        .map(|_| ())
    }

    fn read(&self, address: &str) -> Option<String> {
        let r = self
            .call(
                "pane.read",
                json!({ "pane_id": address, "source": "visible", "format": "text" }),
            )
            .ok()?;
        r["read"]["text"].as_str().map(String::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;

    /// A stand-in for tuios on a socket in a fresh temp folder: answers each request with
    /// `answer(method, params)` and passes every request back for checking.
    struct Fake {
        dir: PathBuf,
        socket: PathBuf,
        seen: mpsc::Receiver<Value>,
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn fake(answer: fn(&str, &Value) -> Value) -> Fake {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "trailboss-tuios-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join(SOCKET);
        let listener = UnixListener::bind(&socket).unwrap();
        let (tx, seen) = mpsc::channel();
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let mut line = String::new();
                BufReader::new(&conn).read_line(&mut line).unwrap();
                let req: Value = serde_json::from_str(&line).unwrap();
                let mut reply = answer(req["method"].as_str().unwrap(), &req["params"]);
                reply["id"] = req["id"].clone();
                conn.write_all(format!("{reply}\n").as_bytes()).unwrap();
                if tx.send(req).is_err() {
                    return;
                }
            }
        });
        Fake { dir, socket, seen }
    }

    fn tuios(answer: fn(&str, &Value) -> Value) -> (Fake, Tuios) {
        let f = fake(answer);
        let host = Tuios::at(f.socket.clone());
        (f, host)
    }

    #[test]
    fn lists_panes_from_the_snapshot() {
        let (f, host) = tuios(|method, _| {
            assert_eq!(method, "session.snapshot");
            json!({ "result": { "type": "session_snapshot", "snapshot": {
                "workspaces": [ { "workspace_id": "w1a2b3c4d5e6", "label": "api" } ],
                "panes": [
                    { "pane_id": "w1a2b3c4d5e6:p0f1e2d3c4b5a", "workspace_id": "w1a2b3c4d5e6",
                      "label": "server", "agent": "claude", "agent_status": "working" },
                    { "pane_id": "w1a2b3c4d5e6:p111111111111", "workspace_id": "w1a2b3c4d5e6",
                      "title": "zsh", "agent_status": "unknown" },
                    { "workspace_id": "w1a2b3c4d5e6", "agent_status": "idle" }
                ]
            } } })
        });
        let panes = host.list().unwrap();
        assert_eq!(panes.len(), 2, "a pane without an id is skipped");
        let p = &panes[0];
        assert_eq!(p.address, "w1a2b3c4d5e6:p0f1e2d3c4b5a");
        assert_eq!(p.location.as_deref(), Some("(tuios) api / server"));
        assert_eq!(p.status.as_deref(), Some("working"));
        assert_eq!(p.program.as_deref(), Some("claude"));
        let shell = &panes[1];
        assert_eq!(shell.location.as_deref(), Some("(tuios) api / zsh"));
        assert_eq!(shell.status, None, "unknown says nothing");
        assert_eq!(shell.program, None);
        assert_eq!(f.seen.recv().unwrap()["params"], json!({}));
    }

    #[test]
    fn gets_the_shell_and_the_foreground_group() {
        let (f, host) = tuios(|_, _| {
            json!({ "result": { "type": "pane_process_info", "process_info": {
                "pane_id": "w1:p1", "shell_pid": 4242, "foreground_process_group_id": 5151,
                "foreground_processes": [ { "pid": 5151, "name": "claude" } ]
            } } })
        });
        let d = host.get("w1:p1").unwrap();
        assert_eq!((d.shell_pid, d.foreground_pid), (Some(4242), Some(5151)));
        let req = f.seen.recv().unwrap();
        assert_eq!(req["method"], "pane.process_info");
        assert_eq!(req["params"]["pane_id"], "w1:p1");
    }

    #[test]
    fn a_pane_without_a_shell_yet_has_no_pids() {
        let (_f, host) = tuios(
            |_, _| json!({ "result": { "type": "pane_process_info", "process_info": { "pane_id": "w1:p1" } } }),
        );
        let d = host.get("w1:p1").unwrap();
        assert_eq!((d.shell_pid, d.foreground_pid), (None, None));
    }

    #[test]
    fn sends_the_text_then_enter_in_one_request() {
        let (f, host) = tuios(|_, _| json!({ "result": { "type": "ok" } }));
        host.send("w1:p1", "claude --resume 's 1'\nsecond line")
            .unwrap();
        let req = f.seen.recv().unwrap();
        assert_eq!(req["method"], "pane.send_input");
        assert_eq!(
            req["params"],
            json!({ "pane_id": "w1:p1", "text": "claude --resume 's 1'\nsecond line", "keys": ["Enter"] })
        );
    }

    #[test]
    fn reads_the_visible_screen() {
        let (f, host) = tuios(
            |_, _| json!({ "result": { "type": "pane_read", "read": { "pane_id": "w1:p1", "text": "$ ls\nsrc\n" } } }),
        );
        assert_eq!(host.read("w1:p1").as_deref(), Some("$ ls\nsrc\n"));
        let req = f.seen.recv().unwrap();
        assert_eq!(req["method"], "pane.read");
        assert_eq!(req["params"]["source"], "visible");
    }

    #[test]
    fn errors_are_reported_and_a_missing_daemon_is_not_running() {
        let (_f, host) = tuios(
            |_, _| json!({ "error": { "code": "forbidden", "message": "this pane may not type there" } }),
        );
        assert_eq!(
            host.send("w1:p1", "hi").unwrap_err(),
            "forbidden: this pane may not type there"
        );
        assert!(host.get("w1:p1").is_none());

        let gone = Tuios::at(std::env::temp_dir().join("trailboss-tuios-no-such.sock"));
        assert!(gone.list().is_none(), "nothing listening: not running");
        assert!(Tuios { socket: None }.list().is_none());
    }

    #[test]
    fn only_a_socket_of_ours_is_used() {
        let f = fake(|_, _| json!({ "result": {} }));
        assert!(ours(&f.socket));
        let plain = f.dir.join("not-a-socket");
        std::fs::write(&plain, "").unwrap();
        assert!(!ours(&plain));
        assert!(!ours(&f.dir.join("missing")));
        assert!(candidates().iter().all(|p| p.ends_with(SOCKET)));
    }
}
