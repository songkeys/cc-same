//! Where Claude Desktop keeps its data on each platform, and where we keep ours.
//!
//! | | Claude Desktop data | Desktop logs | cc-same state |
//! |---|---|---|---|
//! | macOS | `~/Library/Application Support/Claude` | `~/Library/Logs/Claude` | `~/Library/Application Support/cc-same` |
//! | Windows | `%APPDATA%\Claude` (Store build: `%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude`) | `<data>\logs` | `%APPDATA%\cc-same` |
//! | Linux | `~/.config/Claude` | `<data>/logs` | `~/.local/share/cc-same` |
//!
//! Every location can be overridden with an environment variable (`CC_SAME_USER_DATA`,
//! `CC_SAME_STATE_DIR`, `CC_SAME_DESKTOP_LOG_DIR`, `CC_SAME_PROJECTS`).

use crate::model::Surface;
use std::env;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Paths {
    /// Claude Desktop's user data directory.
    pub user_data: PathBuf,
    /// Claude Desktop's log directory (holds `main.log`).
    pub desktop_logs: PathBuf,
    /// Our state: config, snapshots, trash, lock.
    pub state_dir: PathBuf,
    /// Our log file (used by the background agent).
    pub log_file: PathBuf,
    /// Claude Code transcript roots (`~/.claude/projects`, `$CLAUDE_CONFIG_DIR/projects`).
    pub projects: Vec<PathBuf>,
    /// Claude Code user settings (`cleanupPeriodDays` lives here).
    pub claude_settings: PathBuf,
    /// Where Claude Code reads file-based managed settings (`managed-settings.json` and
    /// `managed-settings.d/`), which an organization uses to override user settings.
    pub managed_settings: PathBuf,
    /// Claude Code CLI state (its `oauthAccount` labels an account with an email).
    pub claude_json: PathBuf,
    /// Claude Code's copies of each organization's plugins, one `<org>_<account>` folder each.
    pub synced_plugins: PathBuf,
}

impl Paths {
    /// Default locations for this platform, honouring the `CC_SAME_*` overrides.
    pub fn detect() -> Paths {
        let user_data = env_path("CC_SAME_USER_DATA").unwrap_or_else(default_user_data);
        let state_dir = env_path("CC_SAME_STATE_DIR").unwrap_or_else(|| {
            let dir = default_state_dir();
            adopt_legacy_state(&dir);
            dir
        });
        Paths::new(user_data, state_dir)
    }

    pub fn new(user_data: PathBuf, state_dir: PathBuf) -> Paths {
        let desktop_logs = env_path("CC_SAME_DESKTOP_LOG_DIR").unwrap_or_else(|| default_desktop_logs(&user_data));
        let log_file = default_log_file(&state_dir);
        let claude_home = env_path("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home().join(".claude"));
        let projects = match env::var_os("CC_SAME_PROJECTS") {
            Some(v) if !v.is_empty() => env::split_paths(&v).collect(),
            _ => {
                let mut roots = vec![home().join(".claude").join("projects")];
                if let Some(dir) = env_path("CLAUDE_CONFIG_DIR") {
                    roots.push(dir.join("projects"));
                }
                roots
            }
        };
        Paths {
            user_data,
            desktop_logs,
            state_dir,
            log_file,
            projects,
            claude_settings: claude_home.join("settings.json"),
            managed_settings: default_managed_settings(),
            claude_json: home().join(".claude.json"),
            synced_plugins: claude_home.join("plugins").join("synced"),
        }
    }

    pub fn surface_root(&self, surface: Surface) -> PathBuf {
        self.user_data.join(surface.dir_name())
    }

    /// Desktop's local Cowork sessions. Not synced; their records name the account's email.
    pub fn cowork_sessions(&self) -> PathBuf {
        self.user_data.join("local-agent-mode-sessions")
    }

    pub fn desktop_config(&self) -> PathBuf {
        self.user_data.join("config.json")
    }

    pub fn config_file(&self) -> PathBuf {
        self.state_dir.join("config.json")
    }

    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }

    pub fn snapshots_dir(&self) -> PathBuf {
        self.state_dir.join("snapshots")
    }

    pub fn trash_dir(&self) -> PathBuf {
        self.state_dir.join("trash")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.state_dir.join("lock")
    }

    /// Written by the background agent so front-ends can tell it is alive.
    pub fn heartbeat_file(&self) -> PathBuf {
        self.state_dir.join("agent.json")
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.state_dir.join("bin")
    }

    /// What CC Same last changed in Claude Code's settings, so it can be put back.
    pub fn retention_undo_file(&self) -> PathBuf {
        self.state_dir.join("retention-undo.json")
    }

    /// The numbered list of accounts to switch between.
    pub fn roster_file(&self) -> PathBuf {
        self.state_dir.join("accounts.json")
    }

    /// Claude Desktop's record of plan usage, per organization.
    pub fn usage_history(&self) -> PathBuf {
        self.user_data.join("plan-usage-history.json")
    }
}

/// Claude Code's system directory for managed settings.
fn default_managed_settings() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode")
    } else if cfg!(windows) {
        PathBuf::from(r"C:\Program Files\ClaudeCode")
    } else {
        PathBuf::from("/etc/claude-code")
    }
}

pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Electron's `userData` for an app named "Claude".
pub fn default_user_data() -> PathBuf {
    #[cfg(windows)]
    if let Some(p) = windows_user_data() {
        return p;
    }
    dirs::config_dir().unwrap_or_else(home).join("Claude")
}

/// The Microsoft Store (MSIX) build virtualises `%APPDATA%` into its package folder. Use
/// whichever copy Desktop touched last.
#[cfg(windows)]
fn windows_user_data() -> Option<PathBuf> {
    use std::fs;
    use std::time::SystemTime;

    let mut best: Option<(SystemTime, PathBuf)> = None;
    let mut consider = |dir: PathBuf| {
        let modified = fs::metadata(dir.join("config.json")).and_then(|m| m.modified());
        if let Ok(t) = modified {
            if best.as_ref().is_none_or(|(b, _)| t > *b) {
                best = Some((t, dir));
            }
        }
    };
    if let Some(roaming) = dirs::config_dir() {
        consider(roaming.join("Claude"));
    }
    if let Some(local) = dirs::data_local_dir() {
        if let Ok(entries) = fs::read_dir(local.join("Packages")) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with("Claude_") {
                    consider(e.path().join("LocalCache").join("Roaming").join("Claude"));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

fn default_desktop_logs(user_data: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home().join("Library").join("Logs").join("Claude")
    } else {
        user_data.join("logs")
    }
}

/// What this tool was called before; its state moves over on first use.
const LEGACY_NAMES: &[&str] = &["uni-claude"];

pub fn default_state_dir() -> PathBuf {
    state_dir_named("cc-same")
}

fn state_dir_named(name: &str) -> PathBuf {
    if cfg!(target_os = "linux") {
        dirs::data_dir().unwrap_or_else(|| home().join(".local").join("share")).join(name)
    } else {
        dirs::config_dir().unwrap_or_else(home).join(name)
    }
}

/// The first time the new name is used, take over the snapshots, trash and settings kept under
/// an old one.
fn adopt_legacy_state(state_dir: &Path) {
    if state_dir.exists() {
        return;
    }
    for legacy in LEGACY_NAMES {
        let old = state_dir_named(legacy);
        if old.is_dir() && std::fs::rename(&old, state_dir).is_ok() {
            if cfg!(target_os = "macos") {
                let logs = home().join("Library").join("Logs");
                let _ = std::fs::rename(logs.join(format!("{legacy}.log")), logs.join("cc-same.log"));
            }
            return;
        }
    }
}

fn default_log_file(state_dir: &Path) -> PathBuf {
    if cfg!(target_os = "macos") && state_dir == default_state_dir() {
        home().join("Library").join("Logs").join("cc-same.log")
    } else {
        state_dir.join("cc-same.log")
    }
}
