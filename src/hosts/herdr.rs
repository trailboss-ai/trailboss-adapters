//! Herdr, the first host: terminals that survive a reboot (they come back as bare shells),
//! driven through its `hc` CLI. Ported from agent-resume-plus's `main.js`.
//!
//! ponytail: a plain module while it's the only host. When tmux lands, pull the four verbs
//! used here (list, get, send, read) into the `Host` trait from docs/architecture.md.

use crate::fleet::live::{self, Running};
use crate::fleet::resume::{Entry, Skip, State, Todo, last_words, plan};
use crate::model::text_at;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SHELLS: [&str; 7] = ["zsh", "bash", "fish", "sh", "dash", "nu", "pwsh"];
/// After a reboot, how long to wait for restored shells to start.
const START_WAIT: u32 = 12;
/// A restored shell runs this long before anything is typed into it.
const SETTLE_MS: i64 = 1500;
/// After typing, how long agents get before checking they came up.
const VERIFY: Duration = Duration::from_secs(5);

fn hc(args: &[&str]) -> std::result::Result<String, String> {
    let out = Command::new("hc")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn hc_json(args: &[&str]) -> Option<Value> {
    let mut a = args.to_vec();
    a.push("--json");
    serde_json::from_str(&hc(&a).ok()?).ok()
}

fn pid(v: &Value) -> Option<u32> {
    v.as_u64().and_then(|p| u32::try_from(p).ok())
}

/// A Herdr terminal with an agent (or a tracked one that lost it).
pub struct Terminal {
    pub address: String,
    pub location: Option<String>,
    pub status: Option<String>,
    pub running: Option<Running>,
}

#[derive(Default)]
pub struct Seen {
    pub terminals: Vec<Terminal>,
    /// Keys of tracked terminals that lost their agent: re-created, with a shell in front.
    pub lost: Vec<String>,
    /// Tracked terminals whose shell hasn't started yet (just after a reboot).
    pub pending: usize,
    /// Agent processes running in Herdr terminals, so they aren't tracked twice.
    pub agent_pids: HashSet<u32>,
    /// Each looked-at terminal's own process, to place agents running in its background.
    pub shells: HashMap<u32, String>,
    /// Sessions forgotten because their agent was quit on purpose.
    pub forgotten: Vec<(String, Entry)>,
}

impl Seen {
    /// The terminal whose shell `pid` descends from, if any.
    pub fn terminal_of(&self, pid: u32) -> Option<&String> {
        live::ancestors(pid).iter().find_map(|a| self.shells.get(a))
    }
}

/// Update `state` from Herdr's live terminals. None when Herdr isn't running.
///
/// - An agent in front: remember its session and how to resume it.
/// - The terminal's own shell in front, terminal not re-created: the agent was quit on
///   purpose, so forget it.
/// - The shell in front of a re-created terminal: the agent was lost. Keep it, to resume.
/// - Terminal gone: its tab was closed, so forget it.
pub fn track(state: &mut State, boot: Option<&str>) -> Option<Seen> {
    let ls = hc_json(&["ls"])?;
    let mut seen = Seen::default();
    let mut present = HashSet::new();
    for t in ls["terminals"].as_array()? {
        let (Some(address), true) = (t["address"].as_str(), t["state"] == "live") else {
            continue;
        };
        let key = format!("herdr:{address}");
        present.insert(key.clone());
        let known = state.get(&key).cloned();
        // A bare shell we don't track: nothing to look at.
        if known.is_none() && t["agent"].as_str().is_some_and(|a| SHELLS.contains(&a)) {
            continue;
        }
        let Some(g) = hc_json(&["get", address]) else {
            continue;
        };
        let (shell, created) = (pid(&g["pid"]), g["created_ms"].as_i64());
        if let Some(sh) = shell {
            seen.shells.insert(sh, address.to_string());
        }
        let Some(fg) = pid(&g["foreground"]["pid"]) else {
            seen.pending += usize::from(known.is_some());
            continue;
        };
        // Look even when the foreground is the terminal's own process: `hc run claude`
        // runs the agent with no shell.
        let running = live::detect(fg);
        let location = text_at(&t["where"]);
        match running.as_ref().and_then(|r| Entry::from_running(r, boot)) {
            Some(mut e) => {
                e.created_ms = created;
                e.shell_pid = shell;
                e.location = location.clone();
                e.parent = text_at(&g["parent"]);
                e.name = text_at(&g["name"]);
                state.insert(key.clone(), e);
            }
            None => {
                if let Some(k) = &known
                    && Some(fg) == shell
                {
                    if quit_on_purpose(k, boot, created, shell) {
                        if let Some(e) = state.remove(&key) {
                            seen.forgotten.push((key.clone(), e));
                        }
                    } else {
                        seen.lost.push(key.clone());
                    }
                }
            }
        }
        if let Some(r) = &running {
            seen.agent_pids.insert(r.pid);
        }
        if running.is_some() || seen.lost.last() == Some(&key) {
            seen.terminals.push(Terminal {
                address: address.to_string(),
                location,
                status: text_at(&t["status"]),
                running,
            });
        }
    }
    state.retain(|k, _| !k.starts_with("herdr:") || present.contains(k));
    Some(seen)
}

/// The terminal's own shell is in front where `known`'s agent ran. Quit on purpose only if
/// it's the very same shell in the same boot. Herdr restores a terminal after a reboot with
/// its original `created_ms`, so the creation time alone can't tell a lost agent from a quit
/// one; a forget there would pre-empt the resume a reboot needs.
fn quit_on_purpose(
    known: &Entry,
    boot: Option<&str>,
    created: Option<i64>,
    shell: Option<u32>,
) -> bool {
    // No boot id (not Linux): fall back to the shell's pid, which a restore changes.
    let same_boot = boot.is_none() || known.boot.is_none() || known.boot.as_deref() == boot;
    let same_terminal = match known.created_ms {
        Some(c) => created == Some(c),
        None => true,
    };
    same_boot && same_terminal && known.shell_pid == shell
}

pub struct Outcome {
    /// What was (or, on a dry run, would be) typed.
    pub planned: Vec<Todo>,
    /// Typed and confirmed running. Empty on a dry run.
    pub resumed: Vec<Todo>,
    pub skipped: Vec<Skip>,
    /// Sessions forgotten while looking (their agent was quit on purpose).
    pub forgotten: Vec<(String, Entry)>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Resume lost agents by typing their resume commands into their terminals. `auto` (run
/// when Herdr starts) only resumes what a reboot lost, after waiting for restored shells;
/// a core restart alone is left for a manual run. None when Herdr isn't running.
pub fn resume(state: &mut State, boot: Option<&str>, auto: bool, dry_run: bool) -> Option<Outcome> {
    let mut seen = track(state, boot)?;
    let mut forgotten = std::mem::take(&mut seen.forgotten);
    for _ in 0..START_WAIT {
        if !auto || seen.pending == 0 {
            break;
        }
        sleep(Duration::from_secs(1));
        seen = track(state, boot)?;
        forgotten.append(&mut seen.forgotten);
    }
    let pick: Vec<(String, Entry)> = seen
        .lost
        .iter()
        .filter_map(|k| Some((k.clone(), state.get(k)?.clone())))
        .filter(|(_, e)| !auto || (boot.is_some() && e.boot.is_some() && e.boot.as_deref() != boot))
        .collect();
    let open: HashSet<String> = state
        .iter()
        .filter(|(k, _)| !seen.lost.contains(k))
        .map(|(_, e)| format!("{}:{}", e.agent, e.session))
        .collect();
    let (planned, mut skipped) = plan(&pick, &open, |p| Path::new(p).exists());
    if dry_run {
        return Some(Outcome {
            planned,
            resumed: Vec::new(),
            skipped,
            forgotten,
        });
    }

    let skip = |t: &Todo, reason: String| Skip {
        key: t.key.clone(),
        location: t.location.clone(),
        reason,
    };
    let mut sent = Vec::new();
    for t in &planned {
        let address = t.key.trim_start_matches("herdr:");
        if !t.note {
            // Only type into the terminal's own shell, never into whatever else is running.
            let g = hc_json(&["get", address]);
            let shell_in_front = g.as_ref().is_some_and(|g| {
                pid(&g["foreground"]["pid"]).is_some() && g["foreground"]["pid"] == g["pid"]
            });
            if !shell_in_front {
                skipped.push(skip(t, "something else is running there".into()));
                continue;
            }
            let age = now_ms() - g.and_then(|g| g["created_ms"].as_i64()).unwrap_or(0);
            if age < SETTLE_MS {
                sleep(Duration::from_millis((SETTLE_MS - age) as u64));
            }
        }
        match hc(&["send", address, &t.text]) {
            Ok(_) if !t.note => sent.push(t.clone()),
            Ok(_) => {}
            Err(e) => skipped.push(skip(t, format!("couldn't type into it: {e}"))),
        }
    }

    // An agent that exits right away leaves its shell in front again.
    let mut resumed = Vec::new();
    if !sent.is_empty() {
        sleep(VERIFY);
        for t in sent {
            let address = t.key.trim_start_matches("herdr:");
            let back_to_shell = hc_json(&["get", address]).is_some_and(|g| {
                g["foreground"]["pid"].is_u64() && g["foreground"]["pid"] == g["pid"]
            });
            if back_to_shell {
                let said = hc(&["read", address]).ok().and_then(|s| last_words(&s));
                let reason = match said {
                    Some(w) => format!("exited right away: {w}"),
                    None => "exited right away".into(),
                };
                skipped.push(skip(&t, reason));
            } else {
                resumed.push(t);
            }
        }
    }
    Some(Outcome {
        planned,
        resumed,
        skipped,
        forgotten,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked(boot: &str) -> Entry {
        Entry {
            agent: "claude".into(),
            session: "s1".into(),
            cwd: Some("/w".into()),
            boot: Some(boot.into()),
            created_ms: Some(1_000),
            shell_pid: Some(10),
            ..Default::default()
        }
    }

    #[test]
    fn a_reboot_keeps_restored_terminals_lost_until_resumed() {
        let e = tracked("boot1");
        // Same boot, same shell in front: the agent was quit.
        assert!(quit_on_purpose(&e, Some("boot1"), Some(1_000), Some(10)));
        // After a reboot Herdr restores the terminal with its old created_ms and a new shell.
        // Whatever looks first (a timer's `track`, or `resume --auto`) must not forget it.
        assert!(!quit_on_purpose(&e, Some("boot2"), Some(1_000), Some(4242)));
        // Even if the new shell happens to get the same pid.
        assert!(!quit_on_purpose(&e, Some("boot2"), Some(1_000), Some(10)));
        // Without a boot id, a new shell still means the terminal was restored.
        assert!(!quit_on_purpose(&e, None, Some(1_000), Some(4242)));
        // Re-created in the same boot (Herdr restarted): lost, not quit.
        assert!(!quit_on_purpose(&e, Some("boot1"), Some(2_000), Some(11)));
    }
}
