//! What stays with each account: the connectors its sessions use, the plugins its organization
//! hands out, and the artifacts it published. None of it travels with a session (see
//! [`crate::model::ACCOUNT_LOCAL_KEYS`]), so after a switch it is set up again, or shared, by
//! hand. This says what that is.
//!
//! Everything is read from disk, as Claude left it; nothing is asked of Anthropic:
//! * connectors from `remoteMcpServersConfig` in the account's own session records. A session
//!   names only the connectors it ran with, so these are the ones seen, not every one connected;
//! * plugins from Claude Code's synced copy of the organization's,
//!   `~/.claude/plugins/synced/<org>_<account>/manifest.json`;
//! * artifacts from `publishedArtifacts` in the account's own session records.
//!
//! These are what was seen, not proof of what is set up now: a connector added since has not been
//! seen yet, and one removed since still was. What could not be read is counted, so a missing
//! list is not mistaken for an empty one. Records are read with the scanner's care: real files
//! named for their session only, never through a symlinked folder.

use crate::ctx::Ctx;
use crate::fsx;
use crate::model::{is_uuid, Surface, MAX_RECORD_BYTES};
use crate::scan;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Connector {
    pub name: String,
    pub url: Option<String>,
}

impl Connector {
    /// The same connector, by name: its address changes with the server's version.
    fn key(&self) -> String {
        self.name.to_lowercase()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Plugin {
    pub name: String,
    pub marketplace: Option<String>,
}

impl Plugin {
    /// Two plugins of the same name from different marketplaces are different plugins.
    fn key(&self) -> (Option<&str>, &str) {
        (self.marketplace.as_deref(), self.name.as_str())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    pub url: String,
    pub title: Option<String>,
    /// Unix seconds.
    pub updated_at: Option<f64>,
}

/// What one account has that others may not.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Holdings {
    pub account: String,
    pub connectors: Vec<Connector>,
    pub plugins: Vec<Plugin>,
    /// Newest first.
    pub artifacts: Vec<Artifact>,
    /// Records, folders or plugin lists that could not be read: what is listed may be incomplete.
    pub unreadable: usize,
}

/// What an account lacks that another account has.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Missing {
    pub connectors: Vec<Connector>,
    pub plugins: Vec<Plugin>,
}

impl Missing {
    pub fn is_empty(&self) -> bool {
        self.connectors.is_empty() && self.plugins.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    pub accounts: Vec<Holdings>,
    /// The synced plugins' folder exists but could not be listed: no account's plugins are known.
    pub plugins_unreadable: bool,
}

impl Inventory {
    pub fn of(&self, account: &str) -> Option<&Holdings> {
        self.accounts.iter().find(|h| h.account == account)
    }

    /// The connectors and plugins some other account has and `account` does not.
    pub fn missing(&self, account: &str) -> Missing {
        let empty = Holdings::default();
        let mine = self.of(account).unwrap_or(&empty);
        let have: BTreeSet<String> = mine.connectors.iter().map(Connector::key).collect();
        let have_plugins: BTreeSet<(Option<&str>, &str)> = mine.plugins.iter().map(Plugin::key).collect();
        let mut connectors: BTreeMap<String, Connector> = BTreeMap::new();
        let mut plugins: BTreeSet<Plugin> = BTreeSet::new();
        for other in self.accounts.iter().filter(|h| h.account != account) {
            for c in other.connectors.iter().filter(|c| !have.contains(&c.key())) {
                connectors.entry(c.key()).or_insert_with(|| c.clone());
            }
            plugins.extend(other.plugins.iter().filter(|p| !have_plugins.contains(&p.key())).cloned());
        }
        Missing { connectors: connectors.into_values().collect(), plugins: plugins.into_iter().collect() }
    }
}

/// Read every account's holdings. Accounts come from the session folders and the synced plugins.
pub fn read(ctx: &Ctx) -> Inventory {
    // Per account, each connector with the time of the newest record that names it.
    let mut connectors: BTreeMap<String, BTreeMap<String, (i128, Connector)>> = BTreeMap::new();
    let mut artifacts: BTreeMap<String, BTreeMap<String, Artifact>> = BTreeMap::new();
    let mut unreadable: BTreeMap<String, usize> = BTreeMap::new();
    for part in scan::discover(ctx, Surface::Code) {
        connectors.entry(part.acct.clone()).or_default();
        if part.is_link {
            // Another account's folder, seen through a link: not this account's.
            *unreadable.entry(part.acct.clone()).or_default() += 1;
            continue;
        }
        let Ok(files) = fs::read_dir(&part.path) else {
            *unreadable.entry(part.acct.clone()).or_default() += 1;
            continue;
        };
        for f in files.flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            let Some(uuid) = name.strip_prefix("local_").and_then(|n| n.strip_suffix(".json")).filter(|u| is_uuid(u))
            else {
                continue;
            };
            let Ok(md) = fs::symlink_metadata(f.path()) else { continue };
            let record = match md.is_file().then(|| fsx::read_json(&f.path(), MAX_RECORD_BYTES)) {
                Some(Ok(Value::Object(d)))
                    if d.get("sessionId").and_then(Value::as_str) == Some(&format!("local_{uuid}")) =>
                {
                    d
                }
                _ => {
                    *unreadable.entry(part.acct.clone()).or_default() += 1;
                    continue;
                }
            };
            let mtime = fsx::mtime_ns(&md);
            for c in record.get("remoteMcpServersConfig").and_then(Value::as_array).into_iter().flatten() {
                let name = c.get("name").and_then(Value::as_str).filter(|n| !n.is_empty());
                let url = c.get("url").and_then(Value::as_str).filter(|u| !u.is_empty()).map(str::to_string);
                if let Some(name) = name {
                    let c = Connector { name: name.to_string(), url };
                    let seen = connectors.entry(part.acct.clone()).or_default();
                    // The newest record's word on it (its address changes with the server's version).
                    match seen.get(&c.key()) {
                        Some((at, had)) if (*at, &had.url) >= (mtime, &c.url) => {}
                        _ => {
                            seen.insert(c.key(), (mtime, c));
                        }
                    }
                }
            }
            for a in record.get("publishedArtifacts").and_then(Value::as_array).into_iter().flatten() {
                let Some(url) = a.get("url").and_then(Value::as_str).filter(|u| !u.is_empty()) else { continue };
                let artifact = Artifact {
                    url: url.to_string(),
                    title: a.get("title").and_then(Value::as_str).map(str::to_string),
                    updated_at: a.get("updatedAt").and_then(Value::as_f64).map(|ms| ms / 1000.0),
                };
                let seen = artifacts.entry(part.acct.clone()).or_default();
                // The same artifact can be listed by several sessions: keep the newest word on it.
                match seen.get(url) {
                    Some(had) if had.updated_at >= artifact.updated_at => {}
                    _ => {
                        seen.insert(url.to_string(), artifact);
                    }
                }
            }
        }
    }
    let (plugins, plugins_unreadable) = synced_plugins(ctx, &mut unreadable);
    let accounts: BTreeSet<&String> = connectors.keys().chain(plugins.keys()).collect();
    let accounts = accounts
        .into_iter()
        .map(|account| {
            let mut artifacts: Vec<Artifact> =
                artifacts.get(account).map(|a| a.values().cloned().collect()).unwrap_or_default();
            artifacts.sort_by(|a, b| b.updated_at.unwrap_or(0.0).total_cmp(&a.updated_at.unwrap_or(0.0)));
            Holdings {
                account: account.clone(),
                connectors: connectors
                    .get(account)
                    .map(|c| c.values().map(|(_, c)| c.clone()).collect())
                    .unwrap_or_default(),
                plugins: plugins.get(account).map(|p| p.iter().cloned().collect()).unwrap_or_default(),
                artifacts,
                unreadable: unreadable.get(account).copied().unwrap_or(0),
            }
        })
        .collect();
    Inventory { accounts, plugins_unreadable }
}

/// Plugins per account, from every `<org>_<account>` folder of Claude Code's synced plugins.
fn synced_plugins(ctx: &Ctx, unreadable: &mut BTreeMap<String, usize>) -> (BTreeMap<String, BTreeSet<Plugin>>, bool) {
    let mut out: BTreeMap<String, BTreeSet<Plugin>> = BTreeMap::new();
    let dirs = match fs::read_dir(&ctx.paths.synced_plugins) {
        Ok(dirs) => dirs,
        // No synced plugins at all: none to know of.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (out, false),
        Err(_) => return (out, true),
    };
    for dir in dirs.flatten() {
        let name = dir.file_name().to_string_lossy().into_owned();
        let Some((org, account)) = name.split_once('_') else { continue };
        if !is_uuid(org) || !is_uuid(account) {
            continue;
        }
        let have = out.entry(account.to_string()).or_default();
        let manifest = dir.path().join("manifest.json");
        let manifest = match fs::symlink_metadata(&manifest) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Ok(md) if md.is_file() && !fsx::is_symlink(&dir.path()) => fsx::read_json(&manifest, 4 * 1024 * 1024).ok(),
            _ => None,
        };
        let Some(manifest) = manifest else {
            *unreadable.entry(account.to_string()).or_default() += 1;
            continue;
        };
        for p in manifest.get("plugins").and_then(Value::as_array).into_iter().flatten() {
            if let Some(name) = p.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) {
                let marketplace = p.get("marketplaceName").and_then(Value::as_str).map(str::to_string);
                have.insert(Plugin { name: name.to_string(), marketplace });
            }
        }
    }
    (out, false)
}
