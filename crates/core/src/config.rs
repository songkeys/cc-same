//! `config.json` (what the user chose) and `state.json` (what we remember between runs).
//! Both keep the key names of the original Python prototype, so an existing install upgrades
//! in place.

use crate::fsx;
use crate::model::Surface;
use crate::paths::Paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// `<account>` or `<account>/<org>` entries that stay separate.
    pub exclude: Vec<String>,
    /// Accounts and orgs that appear later join automatically.
    pub auto_join_new: bool,
    /// Rolling snapshots kept (the baseline is kept forever).
    #[serde(deserialize_with = "de_count")]
    pub keep_snapshots: usize,
    #[serde(deserialize_with = "de_number")]
    pub snapshot_every_seconds: f64,
    #[serde(deserialize_with = "de_number")]
    pub trash_days: f64,
    /// Keep a just-left account read-only while Desktop flushes it.
    #[serde(deserialize_with = "de_number")]
    pub switch_grace_seconds: f64,
    pub notify: bool,
    /// Desktop app: `system`, `light` or `dark`.
    pub appearance: String,
    /// Desktop app: `system` or a language code such as `zh-CN`.
    pub language: String,
    /// Desktop app: show the menu bar / notification area icon.
    pub tray: bool,
    /// Desktop app: look for a new version on GitHub once a day.
    pub check_updates: bool,
    /// Desktop app: download new versions in the background and install them while the window
    /// is closed.
    pub auto_update: bool,
    /// A shell command to run after Claude switches accounts ([`crate::hooks`]).
    pub on_switch: Option<String>,
    /// A shell command to run when the background sync starts failing ([`crate::hooks`]).
    pub on_sync_error: Option<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            exclude: Vec::new(),
            auto_join_new: true,
            keep_snapshots: 20,
            snapshot_every_seconds: 3600.0,
            trash_days: 30.0,
            switch_grace_seconds: 30.0,
            notify: true,
            appearance: "system".into(),
            language: "system".into(),
            tray: true,
            check_updates: true,
            auto_update: true,
            on_switch: None,
            on_sync_error: None,
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Config> {
        let path = paths.config_file();
        let raw = match fs::read(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        serde_json::from_slice(&raw).with_context(|| format!("cannot parse {}", path.display()))
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        fsx::create_private_dir_all(&paths.state_dir)?;
        let mut body = serde_json::to_vec_pretty(self)?;
        body.push(b'\n');
        fsx::atomic_write(&paths.config_file(), &body, None, false)?;
        Ok(())
    }

    pub fn is_excluded(&self, acct: &str, org: &str) -> bool {
        let key = format!("{acct}/{org}");
        self.exclude.iter().any(|e| e == acct || *e == key)
    }
}

fn de_number<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    Ok(Value::deserialize(d)?.as_f64().unwrap_or(0.0))
}

fn de_count<'de, D: Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    Ok(Value::deserialize(d)?.as_f64().map(|n| n.max(0.0) as usize).unwrap_or(0))
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LastSync {
    pub at: f64,
    pub reason: String,
    pub applied: u64,
    pub errors: u64,
    pub deferred: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct State {
    pub version: u32,
    /// Snapshot taken before the very first change. Never pruned.
    pub baseline: Option<String>,
    /// surface -> partition keys that have been synced at least once.
    pub known: BTreeMap<String, Vec<String>>,
    /// surface -> collection file -> member key (or `__group__`) -> value at the last sync.
    pub bases: BTreeMap<String, BTreeMap<String, BTreeMap<String, Value>>>,
    pub last_snapshot_at: f64,
    pub last_sync: Option<LastSync>,
    /// Partitions we already told the user to restart Claude for.
    pub notified: Vec<String>,
    /// Sessions were copied into the index Claude has open; they show after it reloads.
    pub pending_restart: Option<PendingRestart>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PendingRestart {
    /// Partition keys that received sessions while loaded.
    pub keys: Vec<String>,
    pub sessions: u64,
    /// Desktop's `init_marker` at the time; a different marker means it reloaded.
    pub init_marker: Option<String>,
    pub at: f64,
}

impl State {
    pub fn load(paths: &Paths) -> State {
        let mut state: State = match fs::read(paths.state_file()) {
            Ok(raw) => serde_json::from_slice(&raw).unwrap_or_default(),
            Err(_) => State::default(),
        };
        // Up to 0.1.9, local Cowork sessions could be synced too.
        state.known.retain(|surface, _| Surface::parse(surface).is_some());
        state.bases.retain(|surface, _| Surface::parse(surface).is_some());
        state
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        fsx::create_private_dir_all(&paths.state_dir)?;
        let mut state = self.clone();
        state.version = 1;
        let mut body = serde_json::to_vec_pretty(&state)?;
        body.push(b'\n');
        fsx::atomic_write(&paths.state_file(), &body, None, false)?;
        Ok(())
    }

    pub fn known(&self, surface: Surface) -> Vec<String> {
        self.known.get(surface.as_str()).cloned().unwrap_or_default()
    }
}
