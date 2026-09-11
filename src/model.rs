use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub port: u16,
    pub state_dir: PathBuf,
    pub safety_margin_bytes: u64,
    pub services: BTreeMap<String, Service>,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub mode: Mode,
    #[serde(default)]
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub match_executable: Option<PathBuf>,
    #[serde(default)]
    pub match_args: Vec<String>,
    pub port: Option<u16>,
    #[serde(default)]
    pub memory_bytes: u64,
    #[serde(default)]
    pub disposable: bool,
    #[serde(default)]
    pub allowed_actions: BTreeSet<String>,
    #[serde(default = "timeout")]
    pub startup_timeout_ms: u64,
}
fn timeout() -> u64 {
    5000
}
#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Managed,
    Discovered,
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.port != 0, "port cannot be zero");
        let mut ports = BTreeSet::from([self.port]);
        for (id, s) in &self.services {
            ensure!(
                !id.is_empty()
                    && id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c)),
                "invalid service id"
            );
            if let Some(p) = s.port {
                ensure!(p != 0 && ports.insert(p), "duplicate/invalid port: {p}");
            }
            ensure!(
                (100..=60000).contains(&s.startup_timeout_ms),
                "startup timeout out of range"
            );
            ensure!(
                s.allowed_actions
                    .iter()
                    .all(|a| ["start", "stop", "restart"].contains(&a.as_str())),
                "unknown action"
            );
            if s.mode == Mode::Managed {
                ensure!(
                    !s.command.is_empty(),
                    "managed service {id} requires command"
                );
                ensure!(
                    s.command[0] == "@self" || PathBuf::from(&s.command[0]).is_absolute(),
                    "command must be absolute or @self"
                );
            } else if !s.command.is_empty() || !s.allowed_actions.is_empty() || s.disposable {
                bail!("discovered service {id} is read-only");
            }
            if let Some(p) = &s.cwd {
                ensure!(
                    p.is_absolute() && p.is_dir(),
                    "cwd must be an existing absolute directory"
                );
            }
            if let Some(p) = &s.match_executable {
                ensure!(p.is_absolute(), "match_executable must be absolute");
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub job_id: String,
    pub agent: String,
    pub requires: Vec<String>,
    /// Additional job memory, beyond the incremental service startup estimates.
    #[serde(default)]
    pub memory_bytes: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Job {
    pub runtime_id: String,
    pub request: Request,
    pub status: String,
    pub result: serde_json::Value,
}
#[derive(Default, Serialize, Deserialize)]
pub struct State {
    pub jobs: BTreeMap<String, Job>,
}
#[derive(Clone, Serialize)]
pub struct Process {
    pub pid: u32,
    pub parent: Option<u32>,
    pub start_time: u64,
    pub name: String,
    pub executable: Option<PathBuf>,
    #[serde(skip)]
    pub args: Vec<String>,
    pub ram_bytes: u64,
    pub cpu_percent: f32,
    pub service: Option<String>,
    pub class: String,
}
#[derive(Serialize)]
pub struct MemoryGate {
    pub status: &'static str,
    pub required: u64,
    pub available: u64,
    pub shortfall: u64,
}
pub fn memory_gate(
    required: u64,
    observed_available: u64,
    margin: u64,
    outstanding: u64,
) -> MemoryGate {
    let available = observed_available
        .saturating_sub(margin)
        .saturating_sub(outstanding);
    MemoryGate {
        status: if required <= available {
            "READY"
        } else {
            "BLOCKED_RESOURCE"
        },
        required,
        available,
        shortfall: required.saturating_sub(available),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deterministic_gate_boundaries() {
        assert_eq!(memory_gate(60, 100, 20, 20).status, "READY");
        let g = memory_gate(61, 100, 20, 20);
        assert_eq!(
            (g.status, g.available, g.shortfall),
            ("BLOCKED_RESOURCE", 60, 1)
        );
        assert_eq!(memory_gate(1, 1, 20, u64::MAX).shortfall, 1);
    }
    #[test]
    fn reject_discovered_control() {
        let c: Config = serde_saphyr::from_str("port: 47831\nstate_dir: .runtime\nsafety_margin_bytes: 0\nservices:\n  external:\n    mode: discovered\n    command: [/bin/sleep, '10']\n").unwrap();
        assert!(c.validate().is_err());
    }
}
