//! tmux, through its CLI on the default server. A pane is addressed `session:window.pane`,
//! which tmux-resurrect restores after a reboot; without it the panes are gone, and
//! `resume` prints the commands instead.
//!
//! ponytail: the default socket only; add `-L`/`-S` sockets if someone runs several servers.

use super::{Detail, Host, Listed};
use std::fs;
use std::process::Command;

pub struct Tmux;

fn tmux(args: &[&str]) -> Result<String, String> {
    let out = Command::new("tmux")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

const FORMAT: &str = "#{session_name}:#{window_index}.#{pane_index}\t#{window_name}\t#{pane_current_command}\t#{pane_dead}";

/// One `list-panes` line; dead panes (their program exited, kept open) are skipped.
fn parse_pane(line: &str) -> Option<Listed> {
    let mut f = line.split('\t');
    let (address, window, program, dead) = (f.next()?, f.next()?, f.next()?, f.next()?);
    (dead != "1" && !address.is_empty()).then(|| Listed {
        address: address.to_string(),
        location: Some(format!("(tmux) {window}")),
        status: None,
        program: Some(program.to_string()),
    })
}

/// The foreground process group of `pid`'s terminal (`tpgid` in /proc/<pid>/stat): the
/// shell itself at a prompt, else the job it runs, whose leader is the program.
fn foreground(stat: &str) -> Option<u32> {
    // The command name is in parentheses and may contain spaces; fields follow the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    let tpgid: i64 = rest.split_whitespace().nth(5)?.parse().ok()?;
    u32::try_from(tpgid).ok().filter(|p| *p > 0)
}

impl Host for Tmux {
    fn name(&self) -> &'static str {
        "tmux"
    }

    /// None when no tmux server is running.
    fn list(&self) -> Option<Vec<Listed>> {
        let out = tmux(&["list-panes", "-a", "-F", FORMAT]).ok()?;
        Some(out.lines().filter_map(parse_pane).collect())
    }

    fn get(&self, address: &str) -> Option<Detail> {
        let shell: u32 = tmux(&["display-message", "-p", "-t", address, "#{pane_pid}"])
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let stat = fs::read_to_string(format!("/proc/{shell}/stat")).ok();
        Some(Detail {
            shell_pid: Some(shell),
            foreground_pid: stat.as_deref().and_then(foreground),
            ..Default::default()
        })
    }

    /// Typed literally (`-l`, so words like "Enter" aren't key names), then Enter.
    fn send(&self, address: &str, text: &str) -> Result<(), String> {
        tmux(&["send-keys", "-t", address, "-l", "--", text])?;
        tmux(&["send-keys", "-t", address, "Enter"]).map(|_| ())
    }

    fn read(&self, address: &str) -> Option<String> {
        tmux(&["capture-pane", "-p", "-t", address]).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_panes_and_the_foreground_group() {
        let p = parse_pane("work:2.1\teditor\tclaude\t0").unwrap();
        assert_eq!(
            (
                p.address.as_str(),
                p.location.as_deref(),
                p.program.as_deref()
            ),
            ("work:2.1", Some("(tmux) editor"), Some("claude"))
        );
        assert!(parse_pane("work:3.0\tlogs\tbash\t1").is_none(), "dead pane");
        assert!(parse_pane("garbage").is_none());

        // pid (comm) state ppid pgrp session tty_nr tpgid ...
        let stat = "4242 (zsh) S 4200 4242 4242 34817 5151 4194560 0 0";
        assert_eq!(foreground(stat), Some(5151));
        assert_eq!(
            foreground("7 (my (odd) cmd) S 1 7 7 0 -1 0"),
            None,
            "no terminal"
        );
    }
}
