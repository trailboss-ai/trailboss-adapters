//! Herdr: terminals that survive a reboot (they come back as bare shells), driven through
//! its `hc` CLI. Ported from agent-resume-plus's `main.js`.

use super::{Detail, Host, Listed};
use crate::model::text_at;
use serde_json::Value;
use std::path::Path;
use std::process::Command;

pub struct Herdr;

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

/// `hc run`'s arguments for [`Host::start`]. The terminal outlives its parent's program:
/// what it was started for isn't over when the parent's is.
fn run_args<'a>(
    cwd: &'a str,
    argv: &'a [String],
    name: &'a str,
    parent: Option<&'a str>,
) -> Vec<&'a str> {
    let mut a = vec![
        "run",
        "--bg",
        "--json",
        "--outlive-parent",
        "--cwd",
        cwd,
        "--name",
        name,
    ];
    if let Some(p) = parent {
        a.extend(["--parent", p]);
    }
    a.push("--");
    a.extend(argv.iter().map(String::as_str));
    a
}

/// A terminal name Herdr accepts, made from a label: 1-32 of `a-z 0-9 - _ .`, starting with
/// a letter or digit, and not shaped like an address (`t7`, `t1b`). The label itself is
/// kept as given by whoever asked; only the terminal's name is cut to fit.
fn name_of(label: &str) -> String {
    let mut n = String::new();
    for c in label.trim().chars().flat_map(char::to_lowercase) {
        let c = if c.is_ascii_alphanumeric() || "-_.".contains(c) {
            c
        } else {
            '-'
        };
        if !(c == '-' && (n.is_empty() || n.ends_with('-'))) {
            n.push(c);
        }
    }
    let mut n = n
        .trim_start_matches(['-', '_', '.'])
        .trim_end_matches('-')
        .to_string();
    let addr = n.len() >= 2
        && n.starts_with('t')
        && n[1..2].bytes().all(|b| b.is_ascii_digit())
        && n.bytes().all(|b| b.is_ascii_alphanumeric());
    if n.is_empty() {
        n = "agent".into();
    } else if addr {
        n.insert_str(0, "n-");
    }
    n.truncate(32);
    n.trim_end_matches('-').to_string()
}

/// `base`, then `base-2` … `base-9`, each within Herdr's 32 characters: names are unique
/// among live terminals, and a second agent with the same label is common.
fn names(base: &str) -> impl Iterator<Item = String> + '_ {
    std::iter::once(base.to_string()).chain((2..=9).map(move |i| {
        let tail = format!("-{i}");
        format!(
            "{}{tail}",
            base[..base.len().min(32 - tail.len())].trim_end_matches('-')
        )
    }))
}

/// The new terminal's address in `hc run --json`'s reply.
fn address_of(reply: &str) -> Result<String, String> {
    serde_json::from_str::<Value>(reply)
        .ok()
        .and_then(|v| text_at(&v["address"]))
        .ok_or_else(|| "hc run didn't say which terminal it started".to_string())
}

fn pid(v: &Value) -> Option<u32> {
    v.as_u64().and_then(|p| u32::try_from(p).ok())
}

impl Host for Herdr {
    fn name(&self) -> &'static str {
        "herdr"
    }

    fn list(&self) -> Option<Vec<Listed>> {
        let ls = hc_json(&["ls"])?;
        Some(
            ls["terminals"]
                .as_array()?
                .iter()
                .filter(|t| t["state"] == "live")
                .filter_map(|t| {
                    Some(Listed {
                        address: t["address"].as_str()?.to_string(),
                        location: text_at(&t["where"]),
                        status: text_at(&t["status"]),
                        program: text_at(&t["agent"]),
                    })
                })
                .collect(),
        )
    }

    fn get(&self, address: &str) -> Option<Detail> {
        let g = hc_json(&["get", address])?;
        Some(Detail {
            shell_pid: pid(&g["pid"]),
            foreground_pid: pid(&g["foreground"]["pid"]),
            created_ms: g["created_ms"].as_i64(),
            parent: text_at(&g["parent"]),
            name: text_at(&g["name"]),
        })
    }

    /// `hc send` pastes, then presses Enter.
    fn send(&self, address: &str, text: &str) -> Result<(), String> {
        hc(&["send", address, text]).map(|_| ())
    }

    fn read(&self, address: &str) -> Option<String> {
        hc(&["read", address]).ok()
    }

    fn enter(&self, address: &str) -> Result<(), String> {
        hc(&["keys", address, "enter"]).map(|_| ())
    }

    fn start(
        &self,
        cwd: &Path,
        argv: &[String],
        name: &str,
        parent: Option<&str>,
    ) -> Result<String, String> {
        let cwd = cwd.to_str().ok_or("the folder's name isn't UTF-8")?;
        let base = name_of(name);
        let mut last = String::new();
        for n in names(&base) {
            match hc(&run_args(cwd, argv, &n, parent)) {
                Ok(reply) => return address_of(&reply),
                // Taken by a live terminal: try the next name. Anything else is the answer.
                Err(e) if e.contains("name_taken") => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_argv_after_the_options() {
        let argv = vec!["claude".to_string(), "--fix the bug".to_string()];
        assert_eq!(
            run_args("/w", &argv, "task-3", Some("t5")),
            [
                "run",
                "--bg",
                "--json",
                "--outlive-parent",
                "--cwd",
                "/w",
                "--name",
                "task-3",
                "--parent",
                "t5",
                "--",
                "claude",
                "--fix the bug"
            ]
        );
        assert!(!run_args("/w", &argv, "x", None).contains(&"--parent"));
        assert_eq!(
            address_of(r#"{"address":"t7","argv":["claude"]}"#).as_deref(),
            Ok("t7")
        );
        assert!(address_of("started").is_err());
    }

    #[test]
    fn labels_become_names_herdr_accepts() {
        assert_eq!(name_of("claude branch"), "claude-branch");
        assert_eq!(name_of("  Cart  Question! "), "cart-question");
        assert_eq!(name_of("task-3"), "task-3");
        assert_eq!(name_of("v1.2_fix"), "v1.2_fix");
        assert_eq!(name_of("-- é --"), "agent");
        assert_eq!(name_of("t7"), "n-t7", "not shaped like an address");
        assert_eq!(name_of("t1b"), "n-t1b");
        assert_eq!(name_of("tests"), "tests");
        let long = name_of(&"word ".repeat(20));
        assert!(long.len() <= 32 && !long.ends_with('-'), "{long}");
        let all: Vec<String> = names(&"x".repeat(32)).collect();
        assert_eq!(all.len(), 9);
        assert!(all.iter().all(|n| n.len() <= 32), "{all:?}");
        assert_eq!(all[1], format!("{}-2", "x".repeat(30)));
        assert_eq!(names("claude-branch").nth(2).unwrap(), "claude-branch-3");
    }
}
