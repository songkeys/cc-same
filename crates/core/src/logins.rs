//! Switching the account Claude Desktop is signed in to.
//!
//! Desktop keeps one sign-in: the claude.ai web session (its cookies and the site's storage) and
//! the tokens it caches in `config.json`, all encrypted with a key in this Mac's login keychain.
//! To switch, Claude quits, the sign-in in use is set aside in `<state>/logins/<account>`, the
//! target's is put back where it was, and Claude starts again. The files move as they are:
//! nothing is decrypted, read, refreshed or sent anywhere. Nobody signs out either, since that
//! ends a sign-in on the server for good.
//!
//! Tokens rotate while Claude runs, so a sign-in is set aside afresh every time it is left, and
//! a copy is never put back twice. A sign-in left unused for about four weeks expires; Claude
//! then opens on its sign-in page, and signing in there again makes the account switchable again.
//!
//! Every move is written to a journal first: a switch that fails is undone, and one that was
//! interrupted (a crash, a power cut) is undone or finished the next time CC Same looks.

use crate::ctx::Ctx;
use crate::desktop;
use crate::fsx;
use crate::model::is_uuid;
use crate::paths::Paths;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Desktop's sign-in in its data folder: the web session and the site's storage.
const ITEMS: &[&str] = &["Cookies", "Cookies-journal", "Local Storage", "Session Storage", "IndexedDB", "WebStorage"];
/// The keys of Desktop's `config.json` that belong to the sign-in.
const KEYS: &[&str] = &["oauth:tokenCache", "oauth:tokenCacheV2", "lastKnownAccountUuid"];
/// State Desktop keeps for the signed-in account and makes again: dropped on a switch.
const DROP: &[&str] = &["bridge-state.json"];

/// In a saved sign-in's folder: its keys from Desktop's `config.json`, and what we know of it.
const KEYS_FILE: &str = "config.json";
const ABOUT_FILE: &str = "about.json";
const JOURNAL: &str = "switch.json";

/// How long Claude gets to quit when it asks nothing ([`desktop::quit`] waits longer while it asks),
/// and its updater to finish.
const QUIT_TIMEOUT: Duration = Duration::from_secs(60);
const UPDATE_TIMEOUT: Duration = Duration::from_secs(180);

/// A sign-in unused for this long has probably expired on the server.
pub const EXPIRES_AFTER_DAYS: f64 = 28.0;

/// Whether switching is available on this system: macOS for now.
pub fn supported() -> bool {
    cfg!(target_os = "macos")
}

/// A sign-in set aside, ready to switch to.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Saved {
    /// The account's UUID.
    pub account: String,
    pub email: Option<String>,
    /// When it was set aside (Unix seconds): when it was last in use.
    pub set_aside_at: f64,
}

impl Saved {
    /// Whether it has probably expired: set aside for about four weeks.
    pub fn stale(&self) -> bool {
        fsx::now_secs() - self.set_aside_at > EXPIRES_AFTER_DAYS * 86_400.0
    }
}

/// Who Claude is signed in to, and who it can switch to.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Logins {
    pub supported: bool,
    /// The account Claude is signed in to.
    pub signed_in: Option<String>,
    /// Sign-ins set aside, other than the one in use.
    pub saved: Vec<Saved>,
}

impl Logins {
    pub fn saved(&self, account: &str) -> Option<&Saved> {
        self.saved.iter().find(|s| s.account == account)
    }
}

pub fn list(ctx: &Ctx) -> Logins {
    let signed_in = desktop::last_known_account(ctx);
    let mut saved: Vec<Saved> = fs::read_dir(root(&ctx.paths))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let account = e.file_name().to_string_lossy().into_owned();
            (is_uuid(&account) && e.path().join(KEYS_FILE).is_file()).then(|| about(&e.path(), account))
        })
        .filter(|s| Some(&s.account) != signed_in.as_ref())
        .collect();
    saved.sort_by(|a, b| b.set_aside_at.total_cmp(&a.set_aside_at));
    Logins { supported: supported(), signed_in, saved }
}

/// Who Claude should be signed in to after a switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target<'a> {
    Account(&'a str),
    /// Nobody: Claude opens on its sign-in page, to sign in to another account.
    SignedOut,
}

/// Why a switch was not even tried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    Unsupported,
    /// There is no sign-in saved for that account.
    NothingSaved,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refused::Unsupported => "switching accounts is not supported on this system yet",
            Refused::NothingSaved => "CC Same has no sign-in saved for that account; sign in to it once in Claude",
        })
    }
}

impl std::error::Error for Refused {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Switched {
    /// The account set aside, when Claude was signed in.
    pub from: Option<String>,
    pub to: Option<String>,
    /// Claude was started again: it was running, or it is needed to sign in.
    pub launched: bool,
}

/// Quit Claude, set its sign-in aside, put `target`'s back, and start Claude again if it was
/// running (or, to sign in, in any case). `watch` follows Claude's quit, and can stop waiting for
/// it; once Claude has quit, the switch goes through. Should it fail after Claude quit, Claude
/// starts again as it was.
pub fn switch(ctx: &Ctx, target: Target, watch: &desktop::Watch) -> Result<Switched> {
    // A stand-in Desktop (tests, scripts) works everywhere.
    if !supported() && ctx.fake.running.is_none() {
        return Err(Refused::Unsupported.into());
    }
    let to = match target {
        Target::Account(account) => Some(account.to_string()),
        Target::SignedOut => None,
    };
    let _lock = crate::apply::Lock::acquire(ctx, Duration::from_secs(30))?;
    if let Some(account) = &to {
        if !is_saved(&ctx.paths, account) && desktop::last_known_account(ctx).as_ref() != Some(account) {
            return Err(Refused::NothingSaved.into());
        }
    }
    let was_running = desktop::is_running(ctx);
    let from = quit_and_swap(ctx, to.as_deref(), was_running, watch).inspect_err(|error| {
        // Not while its updater replaces the app: starting Claude then breaks it.
        let updating = error.downcast_ref::<desktop::NotReady>() == Some(&desktop::NotReady::Updating);
        if was_running && !updating && !desktop::is_running(ctx) {
            if let Err(e) = desktop::launch(ctx) {
                ctx.log(format!("starting Claude again after a failed switch: {e:#}"));
            }
        }
    })?;
    let launched = was_running || to.is_none();
    if launched {
        desktop::launch(ctx)?;
    }
    Ok(Switched { from, to, launched })
}

/// Make sure Claude and its updater are done, then trade sign-ins; who was signed in.
fn quit_and_swap(ctx: &Ctx, to: Option<&str>, was_running: bool, watch: &desktop::Watch) -> Result<Option<String>> {
    if was_running {
        desktop::quit(ctx, QUIT_TIMEOUT, watch)?;
    }
    desktop::wait_for_update(ctx, UPDATE_TIMEOUT)?;
    // The updater starts Claude again once it is done.
    if desktop::is_running(ctx) {
        desktop::quit(ctx, QUIT_TIMEOUT, watch)?;
    }
    recover(&ctx.paths)?;
    let from = desktop::last_known_account(ctx);
    if from.is_some() && from.as_deref() == to {
        // Already signed in to it; a copy set aside earlier is out of date.
        if let Some(account) = to {
            let _ = fs::remove_dir_all(saved_dir(&ctx.paths, account));
        }
    } else {
        let email = from.as_ref().and_then(|a| crate::scan::account_labels(ctx).remove(a));
        swap(&ctx.paths, from.as_deref(), to, email)?;
    }
    Ok(from)
}

/// Delete the sign-in saved for `account`.
pub fn forget(ctx: &Ctx, account: &str) -> Result<()> {
    let _lock = crate::apply::Lock::acquire(ctx, Duration::from_secs(30))?;
    let dir = saved_dir(&ctx.paths, account);
    if dir.exists() {
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}

fn root(paths: &Paths) -> PathBuf {
    paths.state_dir.join("logins")
}

fn saved_dir(paths: &Paths, account: &str) -> PathBuf {
    root(paths).join(account)
}

fn is_saved(paths: &Paths, account: &str) -> bool {
    is_uuid(account) && saved_dir(paths, account).join(KEYS_FILE).is_file()
}

fn about(dir: &Path, account: String) -> Saved {
    let saved = fsx::read_json(&dir.join(ABOUT_FILE), 64 * 1024).ok().and_then(|v| serde_json::from_value(v).ok());
    Saved { account, ..saved.unwrap_or_default() }
}

// ------------------------------------------------------------------------ the swap

/// A switch in progress, kept on disk so it can be undone or finished after an interruption.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Journal {
    from: Option<String>,
    to: Option<String>,
    /// Where the outgoing sign-in gathers before it is filed.
    stage: PathBuf,
    /// Moves done so far, in order.
    moves: Vec<(PathBuf, PathBuf)>,
    /// Desktop's `config.json` as it was, kept to put back (none when there was none).
    config_before: Option<PathBuf>,
    /// Everything is in place; only filing the outgoing sign-in and tidying up are left.
    committed: bool,
    #[serde(skip)]
    path: PathBuf,
}

impl Journal {
    fn save(&self) -> Result<()> {
        let body = serde_json::to_vec_pretty(self)?;
        fsx::atomic_write(&self.path, &body, None, false)?;
        Ok(())
    }

    fn moved(&mut self, from: PathBuf, to: PathBuf) -> Result<()> {
        fsx::move_path(&from, &to).with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        self.moves.push((from, to));
        self.save()
    }

    /// Put every move back, newest first, and Desktop's `config.json` as it was.
    ///
    /// A piece whose place was taken meanwhile (Claude ran after an interrupted switch and made a
    /// new one) is never deleted: what was set aside is filed as the account's saved sign-in.
    fn undo(&self, paths: &Paths) -> Result<()> {
        let mut stranded = false;
        for (from, to) in self.moves.iter().rev() {
            if fs::symlink_metadata(to).is_err() {
                continue;
            }
            if fs::symlink_metadata(from).is_err() {
                fsx::move_path(to, from).with_context(|| format!("moving {} back", to.display()))?;
            } else {
                stranded = true;
            }
        }
        let before = self.config_before.as_ref().filter(|b| b.is_file());
        if stranded {
            self.keep_stranded(paths, before)?;
        } else if let Some(before) = before {
            fs::copy(before, paths.desktop_config()).context("restoring Claude's config.json")?;
        }
        self.tidy();
        Ok(())
    }

    /// File a sign-in that could not be put back under its account, keys included, or keep it
    /// apart when that account has one saved already.
    fn keep_stranded(&self, paths: &Paths, config_before: Option<&PathBuf>) -> Result<()> {
        let keys = self.stage.join(KEYS_FILE);
        if !keys.exists() {
            if let Some(Value::Object(config)) = config_before.and_then(|b| fsx::read_json(b, 16 * 1024 * 1024).ok()) {
                let saved: Map<String, Value> =
                    KEYS.iter().filter_map(|k| config.get(*k).map(|v| (k.to_string(), v.clone()))).collect();
                fsx::atomic_write(&keys, &serde_json::to_vec_pretty(&saved)?, None, false)?;
            }
        }
        let home = match &self.from {
            Some(account) if !saved_dir(paths, account).exists() => saved_dir(paths, account),
            _ => fsx::unique_path(root(paths).join(".stranded")),
        };
        fs::rename(&self.stage, &home).with_context(|| format!("keeping {}", self.stage.display()))?;
        if let Some(account) = self.from.as_ref().filter(|a| home.file_name() == Some(a.as_ref())) {
            let about = Saved { account: account.clone(), email: None, set_aside_at: fsx::now_secs() };
            fsx::atomic_write(&home.join(ABOUT_FILE), &serde_json::to_vec_pretty(&about)?, None, false)?;
        }
        Ok(())
    }

    /// File the outgoing sign-in under its account and forget the one now in use.
    fn finish(&self, paths: &Paths, email: Option<String>) -> Result<()> {
        for name in DROP {
            let _ = fs::remove_file(paths.user_data.join(name));
        }
        match &self.from {
            Some(account) if self.stage.exists() => {
                let dir = saved_dir(paths, account);
                if dir.exists() {
                    fs::remove_dir_all(&dir).with_context(|| format!("replacing {}", dir.display()))?;
                }
                fs::rename(&self.stage, &dir).with_context(|| format!("filing {}", dir.display()))?;
                let about = Saved { account: account.clone(), email, set_aside_at: fsx::now_secs() };
                fsx::atomic_write(&dir.join(ABOUT_FILE), &serde_json::to_vec_pretty(&about)?, None, false)?;
            }
            _ => {
                let _ = fs::remove_dir_all(&self.stage);
            }
        }
        if let Some(account) = &self.to {
            let _ = fs::remove_dir_all(saved_dir(paths, account));
        }
        self.tidy();
        Ok(())
    }

    fn tidy(&self) {
        if let Some(before) = &self.config_before {
            let _ = fs::remove_file(before);
        }
        if !self.committed {
            let _ = fs::remove_dir_all(&self.stage);
        }
        let _ = fs::remove_file(&self.path);
    }
}

/// Set `from`'s sign-in aside and put `to`'s in its place (nobody's, for `None`), all or nothing.
fn swap(paths: &Paths, from: Option<&str>, to: Option<&str>, email: Option<String>) -> Result<()> {
    let root = root(paths);
    fsx::create_private_dir_all(&root)?;
    let stage = root.join(format!(".{}.incoming", from.unwrap_or("unfinished")));
    // Something left by an older run: keep it, out of the way.
    if stage.exists() {
        fs::rename(&stage, fsx::unique_path(root.join(".stranded")))?;
    }
    fsx::create_private_dir_all(&stage)?;
    let mut journal = Journal {
        from: from.map(str::to_string),
        to: to.map(str::to_string),
        stage: stage.clone(),
        path: root.join(JOURNAL),
        ..Journal::default()
    };
    journal.save()?;
    let result = (|| -> Result<()> {
        let config_path = paths.desktop_config();
        let before = match fs::read(&config_path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).context("reading Claude's config.json"),
        };
        let mut config: Map<String, Value> = match &before {
            Some(bytes) => serde_json::from_slice(bytes).context("reading Claude's config.json")?,
            None => Map::new(),
        };
        if let Some(bytes) = &before {
            let copy = root.join(".config-before.json");
            fsx::atomic_write(&copy, bytes, None, false)?;
            journal.config_before = Some(copy);
            journal.save()?;
        }
        // Set the sign-in in use aside.
        for name in ITEMS {
            let live = paths.user_data.join(name);
            if fs::symlink_metadata(&live).is_ok() {
                journal.moved(live, stage.join(name))?;
            }
        }
        let keys: Map<String, Value> =
            KEYS.iter().filter_map(|k| config.get(*k).map(|v| (k.to_string(), v.clone()))).collect();
        fsx::atomic_write(&stage.join(KEYS_FILE), &serde_json::to_vec_pretty(&keys)?, None, false)?;
        // Put the target's back.
        for key in KEYS {
            config.remove(*key);
        }
        if let Some(account) = to {
            let saved = saved_dir(paths, account);
            for name in ITEMS {
                let parked = saved.join(name);
                if fs::symlink_metadata(&parked).is_ok() {
                    journal.moved(parked, paths.user_data.join(name))?;
                }
            }
            let keys = fsx::read_json(&saved.join(KEYS_FILE), 16 * 1024 * 1024)?;
            if let Value::Object(keys) = keys {
                config.extend(keys);
            }
        }
        fsx::atomic_write(&config_path, &desktop_json(&config)?, None, false)?;
        Ok(())
    })();
    if let Err(e) = result {
        journal.undo(paths).context("undoing the switch")?;
        return Err(e);
    }
    journal.committed = true;
    journal.save()?;
    journal.finish(paths, email)
}

/// JSON the way Desktop writes its `config.json`: tab-indented, without a final newline.
fn desktop_json(config: &Map<String, Value>) -> Result<Vec<u8>> {
    use serde::Serialize as _;
    let mut out = Vec::new();
    let mut ser =
        serde_json::Serializer::with_formatter(&mut out, serde_json::ser::PrettyFormatter::with_indent(b"\t"));
    config.serialize(&mut ser)?;
    Ok(out)
}

/// `recover`, if Claude is not running and no other CC Same run is busy: for startup.
pub fn recover_if_idle(ctx: &Ctx) {
    if desktop::is_running(ctx) {
        return;
    }
    if let Ok(_lock) = crate::apply::Lock::acquire(ctx, Duration::ZERO) {
        if let Err(e) = recover(&ctx.paths) {
            ctx.log(format!("finishing an interrupted account switch: {e:#}"));
        }
    }
}

/// Undo a switch that was interrupted before everything was in place, or finish one that was
/// interrupted afterwards. Only while Claude is not running, which a switch makes sure of.
pub fn recover(paths: &Paths) -> Result<()> {
    let path = root(paths).join(JOURNAL);
    if !path.exists() {
        return Ok(());
    }
    let journal = fsx::read_json(&path, 1024 * 1024).ok().and_then(|v| serde_json::from_value::<Journal>(v).ok());
    let Some(mut journal) = journal else {
        // Unreadable: it is written whole or not at all, so this cannot happen mid-switch.
        let _ = fs::remove_file(&path);
        return Ok(());
    };
    journal.path = path;
    if journal.committed {
        let email = journal.from.as_deref().and_then(|a| about(&saved_dir(paths, a), a.to_string()).email);
        journal.finish(paths, email)
    } else {
        journal.undo(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, FakeDesktop, LogSink};
    use std::sync::Arc;

    const ADA: &str = "5a1d3c07-8f2e-4b6a-9c1d-2e3f4a5b6c7d";
    const GRACE: &str = "9e31ea7e-1c2d-4e5f-8a9b-0c1d2e3f4a5b";

    struct Mac {
        _tmp: tempfile::TempDir,
        ctx: Arc<Ctx>,
    }

    impl Mac {
        /// Claude's data folder signed in to `account`, with every piece marked by its owner.
        fn new(account: &str) -> Mac {
            let tmp = tempfile::tempdir().unwrap();
            let mut paths = Paths::new(tmp.path().join("Claude"), tmp.path().join("state"));
            // Never the real ones.
            paths.claude_json = tmp.path().join(".claude.json");
            paths.claude_settings = tmp.path().join(".claude/settings.json");
            paths.projects = vec![tmp.path().join(".claude/projects")];
            paths.desktop_logs = tmp.path().join("Logs");
            fs::create_dir_all(&paths.user_data).unwrap();
            let fake = FakeDesktop { running: Some(false), active: None };
            let ctx = Arc::new(Ctx::new(paths, Config::default(), fake, LogSink::Silent));
            let mac = Mac { _tmp: tmp, ctx };
            mac.sign_in(account);
            fs::write(mac.data().join("bridge-state.json"), "{}").unwrap();
            mac
        }

        fn data(&self) -> PathBuf {
            self.ctx.paths.user_data.clone()
        }

        /// What Claude does when someone signs in: a fresh session and config keys.
        fn sign_in(&self, account: &str) {
            let data = self.data();
            fs::write(data.join("Cookies"), format!("cookies of {account}")).unwrap();
            fs::write(data.join("Cookies-journal"), "").unwrap();
            for dir in ["Local Storage/leveldb", "IndexedDB", "Session Storage", "WebStorage"] {
                fs::create_dir_all(data.join(dir)).unwrap();
            }
            fs::write(data.join("Local Storage/leveldb/000003.log"), format!("storage of {account}")).unwrap();
            let mut config: Map<String, Value> = fsx::read_json(&data.join("config.json"), 1 << 20)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_else(|| {
                    serde_json::json!({ "darkMode": "dark", "locale": "en-US" }).as_object().unwrap().clone()
                });
            config.insert("oauth:tokenCacheV2".into(), format!("tokens of {account}").into());
            config.insert("lastKnownAccountUuid".into(), account.into());
            fs::write(data.join("config.json"), desktop_json(&config).unwrap()).unwrap();
        }

        fn owner(&self) -> String {
            fs::read_to_string(self.data().join("Cookies")).unwrap_or_default()
        }

        fn config(&self) -> Map<String, Value> {
            fsx::read_json(&self.data().join("config.json"), 1 << 20).unwrap().as_object().unwrap().clone()
        }
    }

    #[test]
    fn signing_in_to_another_account_sets_the_first_aside() {
        let mac = Mac::new(ADA);
        let switched = switch(&mac.ctx, Target::SignedOut, &desktop::Watch::default()).unwrap();
        assert_eq!(switched, Switched { from: Some(ADA.into()), to: None, launched: true });
        // Claude has nobody's sign-in, and keeps every other setting.
        assert!(!mac.data().join("Cookies").exists() && !mac.data().join("Local Storage").exists());
        let config = mac.config();
        assert_eq!(config.get("darkMode"), Some(&Value::from("dark")));
        assert!(KEYS.iter().all(|k| !config.contains_key(*k)));
        assert!(!mac.data().join("bridge-state.json").exists());
        // Ada's is set aside as it was.
        let logins = list(&mac.ctx);
        assert_eq!((logins.signed_in.as_deref(), logins.saved.len()), (None, 1));
        assert_eq!(logins.saved[0].account, ADA);
        let parked = saved_dir(&mac.ctx.paths, ADA);
        assert_eq!(fs::read_to_string(parked.join("Cookies")).unwrap(), format!("cookies of {ADA}"));
        assert!(parked.join("Local Storage/leveldb/000003.log").is_file());
        let keys = fsx::read_json(&parked.join(KEYS_FILE), 1 << 20).unwrap();
        assert_eq!(keys["oauth:tokenCacheV2"], format!("tokens of {ADA}"));
    }

    #[test]
    fn switching_back_and_forth_keeps_each_sign_in_whole() {
        let mac = Mac::new(ADA);
        switch(&mac.ctx, Target::SignedOut, &desktop::Watch::default()).unwrap();
        mac.sign_in(GRACE);
        assert_eq!(list(&mac.ctx).signed_in.as_deref(), Some(GRACE));

        let switched = switch(&mac.ctx, Target::Account(ADA), &desktop::Watch::default()).unwrap();
        assert_eq!(switched, Switched { from: Some(GRACE.into()), to: Some(ADA.into()), launched: false });
        assert_eq!(mac.owner(), format!("cookies of {ADA}"));
        assert_eq!(mac.config()["oauth:tokenCacheV2"], format!("tokens of {ADA}"));
        assert_eq!(mac.config()["lastKnownAccountUuid"], ADA);
        assert_eq!(mac.config()["darkMode"], "dark");
        let logins = list(&mac.ctx);
        assert_eq!(logins.signed_in.as_deref(), Some(ADA));
        assert_eq!(logins.saved.iter().map(|s| s.account.as_str()).collect::<Vec<_>>(), [GRACE]);
        // A copy is never put back twice: Ada's is live now, so it is no longer saved.
        assert!(!saved_dir(&mac.ctx.paths, ADA).exists());

        // Tokens rotate while Claude runs; switching away saves the newest.
        let mut config = mac.config();
        config.insert("oauth:tokenCacheV2".into(), "rotated tokens of ada".into());
        fs::write(mac.data().join("config.json"), desktop_json(&config).unwrap()).unwrap();
        switch(&mac.ctx, Target::Account(GRACE), &desktop::Watch::default()).unwrap();
        assert_eq!(mac.owner(), format!("cookies of {GRACE}"));
        let keys = fsx::read_json(&saved_dir(&mac.ctx.paths, ADA).join(KEYS_FILE), 1 << 20).unwrap();
        assert_eq!(keys["oauth:tokenCacheV2"], "rotated tokens of ada");
    }

    #[test]
    fn claude_config_keeps_its_format() {
        let mac = Mac::new(ADA);
        switch(&mac.ctx, Target::SignedOut, &desktop::Watch::default()).unwrap();
        let text = fs::read_to_string(mac.data().join("config.json")).unwrap();
        assert!(text.starts_with("{\n\t\"darkMode\""), "{text}");
        assert!(text.ends_with('}'));
    }

    #[test]
    fn nothing_saved_means_nothing_changes() {
        let mac = Mac::new(ADA);
        let refused = switch(&mac.ctx, Target::Account(GRACE), &desktop::Watch::default()).unwrap_err();
        assert_eq!(refused.downcast_ref::<Refused>(), Some(&Refused::NothingSaved));
        assert_eq!(mac.owner(), format!("cookies of {ADA}"));
        assert!(mac.data().join("bridge-state.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_switch_puts_everything_back() {
        use std::os::unix::fs::PermissionsExt as _;
        let mac = Mac::new(ADA);
        switch(&mac.ctx, Target::SignedOut, &desktop::Watch::default()).unwrap();
        mac.sign_in(GRACE);
        let before = mac.config();
        // Ada's sign-in cannot leave its folder, so the switch fails halfway: Grace's sign-in is
        // already set aside by then.
        let ada = saved_dir(&mac.ctx.paths, ADA);
        fs::set_permissions(&ada, fs::Permissions::from_mode(0o555)).unwrap();
        let result = switch(&mac.ctx, Target::Account(ADA), &desktop::Watch::default());
        fs::set_permissions(&ada, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert_eq!(mac.owner(), format!("cookies of {GRACE}"));
        assert!(mac.data().join("Local Storage/leveldb/000003.log").is_file());
        assert_eq!(mac.config(), before);
        assert!(ada.join("Cookies").is_file());
        let root = root(&mac.ctx.paths);
        assert!(!root.join(format!(".{GRACE}.incoming")).exists());
        assert!(!root.join(JOURNAL).exists());
    }

    #[test]
    fn an_interrupted_switch_is_undone() {
        let mac = Mac::new(ADA);
        let root = root(&mac.ctx.paths);
        fsx::create_private_dir_all(&root).unwrap();
        // A switch that set Ada's cookies aside, then stopped.
        let stage = root.join(format!(".{ADA}.incoming"));
        fsx::create_private_dir_all(&stage).unwrap();
        fs::rename(mac.data().join("Cookies"), stage.join("Cookies")).unwrap();
        let journal = Journal {
            from: Some(ADA.into()),
            stage: stage.clone(),
            moves: vec![(mac.data().join("Cookies"), stage.join("Cookies"))],
            path: root.join(JOURNAL),
            ..Journal::default()
        };
        journal.save().unwrap();
        recover(&mac.ctx.paths).unwrap();
        assert_eq!(mac.owner(), format!("cookies of {ADA}"));
        assert!(!stage.exists() && !root.join(JOURNAL).exists());
    }

    #[test]
    fn a_sign_in_that_cannot_go_back_is_kept_not_deleted() {
        let mac = Mac::new(ADA);
        let root = root(&mac.ctx.paths);
        fsx::create_private_dir_all(&root).unwrap();
        // A switch set Ada's cookies aside and stopped; then Claude ran and made new ones.
        let stage = root.join(format!(".{ADA}.incoming"));
        fsx::create_private_dir_all(&stage).unwrap();
        fs::rename(mac.data().join("Cookies"), stage.join("Cookies")).unwrap();
        let before = root.join(".config-before.json");
        fs::copy(mac.data().join("config.json"), &before).unwrap();
        let journal = Journal {
            from: Some(ADA.into()),
            stage: stage.clone(),
            moves: vec![(mac.data().join("Cookies"), stage.join("Cookies"))],
            config_before: Some(before),
            path: root.join(JOURNAL),
            ..Journal::default()
        };
        journal.save().unwrap();
        fs::write(mac.data().join("Cookies"), "a new session").unwrap();
        recover(&mac.ctx.paths).unwrap();
        // Claude's new session stays; Ada's is filed as her saved sign-in, keys and all.
        assert_eq!(mac.owner(), "a new session");
        let ada = saved_dir(&mac.ctx.paths, ADA);
        assert_eq!(fs::read_to_string(ada.join("Cookies")).unwrap(), format!("cookies of {ADA}"));
        let keys = fsx::read_json(&ada.join(KEYS_FILE), 1 << 20).unwrap();
        assert_eq!(keys["oauth:tokenCacheV2"], format!("tokens of {ADA}"));
        assert!(!root.join(JOURNAL).exists());
    }

    #[test]
    fn forgetting_deletes_a_saved_sign_in() {
        let mac = Mac::new(ADA);
        switch(&mac.ctx, Target::SignedOut, &desktop::Watch::default()).unwrap();
        forget(&mac.ctx, ADA).unwrap();
        assert!(list(&mac.ctx).saved.is_empty());
        assert!(!saved_dir(&mac.ctx.paths, ADA).exists());
    }

    #[test]
    fn a_sign_in_set_aside_four_weeks_ago_is_stale() {
        let fresh = Saved { set_aside_at: fsx::now_secs() - 86_400.0, ..Saved::default() };
        let old = Saved { set_aside_at: fsx::now_secs() - 30.0 * 86_400.0, ..Saved::default() };
        assert!(!fresh.stale() && old.stale());
    }

    #[test]
    fn labels_are_kept_for_the_accounts_set_aside() {
        let mac = Mac::new(ADA);
        let claude_json = mac.ctx.paths.claude_json.clone();
        fs::create_dir_all(claude_json.parent().unwrap()).unwrap();
        let oauth = serde_json::json!({ "oauthAccount": { "accountUuid": ADA, "emailAddress": "ada@lovelace.dev" } });
        fs::write(&claude_json, oauth.to_string()).unwrap();
        switch(&mac.ctx, Target::SignedOut, &desktop::Watch::default()).unwrap();
        assert_eq!(list(&mac.ctx).saved[0].email.as_deref(), Some("ada@lovelace.dev"));
    }
}
