//! Herdr: terminals that survive a reboot (they come back as bare shells), driven through
//! its `hc` CLI. Ported from agent-resume-plus's `main.js`.

use super::{Detail, Host, Listed};
use crate::model::text_at;
use serde_json::Value;
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
}
