//! Is Claude Desktop running, and which account has it loaded?
//!
//! Desktop writes only the loaded account's index and reads it only when that account
//! loads, so this decides what we may touch. Signals, most exact first:
//! * `main.log`: `[LocalSessionManager] Initialization succeeded — accountId=…, orgId=…`
//! * `config.json`: `lastKnownAccountUuid`
//!
//! When Desktop runs but neither signal is readable, every index counts as loaded.
//!
//! What we tell people is open is narrower: the account Desktop signed in to last, with the
//! organization from the log only when the newest line is about that account.

use crate::ctx::{Ctx, LogScan};
use crate::fsx;
use crate::model::{is_uuid, AppState};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const INIT_MARKER: &str = "[LocalSessionManager] Initialization succeeded";
/// How much of a log to read the first time: all of it, as Desktop starts a new `main.log` at
/// about 10 MB. A shorter tail can miss the line of the account in use after a few days, and
/// fall back to an older file's line about another account.
const TAIL_BYTES: u64 = 12 * 1024 * 1024;

#[cfg(windows)]
mod windows;

/// A launch target resolved before any sign-in files change.
pub struct Launcher {
    fake: bool,
    #[cfg(windows)]
    target: Option<windows::LaunchTarget>,
}

pub fn prepare_launch(ctx: &Ctx) -> anyhow::Result<Launcher> {
    let fake = ctx.fake.running.is_some();
    Ok(Launcher {
        fake,
        #[cfg(windows)]
        target: if fake { None } else { Some(windows::resolve_launch(&ctx.paths.user_data)?) },
    })
}

impl Launcher {
    pub fn launch(&self) -> anyhow::Result<()> {
        if self.fake {
            return Ok(());
        }
        #[cfg(windows)]
        {
            self.target.as_ref().expect("real launcher was preflighted").launch()
        }
        #[cfg(not(windows))]
        {
            process::launch()
        }
    }
}

pub fn detect(ctx: &Ctx) -> AppState {
    let running = is_running(ctx);
    let mut accounts = BTreeSet::new();
    let mut pairs = BTreeSet::new();
    let mut sources = Vec::new();
    // What Desktop shows, for telling people: an account, and its organization when known.
    let mut open: Option<(String, Option<String>)> = None;
    if let Some(active) = &ctx.fake.active {
        for item in active {
            let (acct, org) = match item.split_once('/') {
                Some((a, o)) => (a.to_string(), Some(o.to_string())),
                None => (item.clone(), None),
            };
            if let Some(org) = &org {
                pairs.insert((acct.clone(), org.clone()));
            }
            accounts.insert(acct.clone());
            open.get_or_insert((acct, org));
        }
        sources.push("override");
    }
    let mut init_marker = None;
    if ctx.fake.active.is_none() {
        if let Some(acct) = last_known_account(ctx) {
            accounts.insert(acct.clone());
            open = Some((acct, None));
            sources.push("config.json");
        }
    }
    if let Some(init) = last_init(ctx) {
        init_marker = Some(init.marker);
        if ctx.fake.active.is_none() {
            // The newest line can predate the last sign-in (it may sit in an older log file), so
            // it only names the organization of the account signed in to.
            match &mut open {
                Some((acct, org)) if *acct == init.acct => *org = Some(init.org.clone()),
                Some(_) => {}
                None => open = Some((init.acct.clone(), Some(init.org.clone()))),
            }
            accounts.insert(init.acct.clone());
            pairs.insert((init.acct, init.org));
            sources.push("Desktop log");
        }
    }
    let (open_account, open_org) = match open.filter(|_| running) {
        Some((acct, org)) => (Some(acct), org),
        None => (None, None),
    };
    // Keep a just-left account read-only for a moment: Desktop flushes it after switching.
    let grace = Duration::from_secs_f64(ctx.config().switch_grace_seconds.max(0.0));
    let mut recent = ctx.caches.recent_accounts.lock().unwrap();
    if running {
        for a in &accounts {
            recent.insert(a.clone(), Instant::now());
        }
    }
    recent.retain(|_, t| t.elapsed() <= grace);
    accounts.extend(recent.keys().cloned());
    AppState {
        running,
        uncertain: accounts.is_empty(),
        accounts,
        pairs,
        source: if sources.is_empty() { "unknown".into() } else { sources.join(" + ") },
        init_marker,
        open_account,
        open_org,
    }
}

pub fn is_running(ctx: &Ctx) -> bool {
    if let Some(r) = ctx.fake.running {
        return r;
    }
    // If the process table cannot be read, assume Desktop runs: the safe side.
    process::desktop_running().unwrap_or(true)
}

/// Why Claude could not be made ready for a change to its sign-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotReady {
    /// Claude did not quit: it may be asking something, or a task keeps it open.
    StillOpen,
    /// Claude's updater is replacing the app.
    Updating,
    /// Whoever asked stopped waiting for Claude to quit.
    Stopped,
}

impl std::fmt::Display for NotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            NotReady::StillOpen => "Claude did not quit, so nothing was changed; choose File > Quit in Claude (closing the window may leave it in the tray), then try again",
            NotReady::Updating => "Claude is updating; try again in a minute",
            NotReady::Stopped => "stopped waiting for Claude to quit, so nothing was changed",
        })
    }
}

impl std::error::Error for NotReady {}

/// How a request to quit is going, as Claude's log tells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quitting {
    /// Asked; Claude is closing, or has said nothing yet.
    Asked,
    /// Claude is asking whether to stop its work in progress ("Claude is still working").
    Confirming,
    /// The answer was "Wait for Claude": it quits by itself once that work is done.
    AfterWork,
}

/// How whoever asked Claude to quit follows along, and stops waiting.
#[derive(Default)]
pub struct Watch<'a> {
    pub progress: Option<&'a (dyn Fn(Quitting) + Sync)>,
    pub stop: Option<&'a AtomicBool>,
}

impl Watch<'_> {
    fn tell(&self, quitting: Quitting) {
        if let Some(progress) = self.progress {
            progress(quitting);
        }
    }

    fn stopped(&self) -> bool {
        self.stop.is_some_and(|s| s.load(Ordering::Relaxed))
    }
}

/// How long Claude may keep a "Claude is still working" question open: someone who chose Cancel
/// there may never stop waiting here.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Ask Claude to quit, the way the Dock's Quit does, and wait for it and its helpers to be gone:
/// up to `timeout` while it says nothing, up to ten minutes while it asks whether to stop its
/// work in progress, and for as long as that work takes when the answer was to wait. `watch`
/// hears how it goes, and can stop waiting.
pub fn quit(ctx: &Ctx, timeout: Duration, watch: &Watch) -> anyhow::Result<()> {
    let launcher = prepare_launch(ctx)?;
    quit_prepared(ctx, timeout, watch, &launcher)
}

pub(crate) fn quit_prepared(ctx: &Ctx, timeout: Duration, watch: &Watch, launcher: &Launcher) -> anyhow::Result<()> {
    if ctx.fake.running.is_some() {
        return Ok(());
    }
    let mut log = QuitLog::start(&ctx.paths.desktop_logs.join("main.log"));
    #[cfg(windows)]
    windows::ask_to_quit(launcher.target.as_ref().expect("real launcher was preflighted"), |message| ctx.log(message))?;
    #[cfg(not(windows))]
    let _ = launcher;
    #[cfg(not(windows))]
    process::ask_to_quit();
    wait_for_quit(process::any_running, &mut log, (timeout, CONFIRM_TIMEOUT), watch)
}

/// Wait while `running`, following the quit in `log`: `limits` is how long Claude may say nothing,
/// then how long it may keep its question open.
fn wait_for_quit(
    running: impl Fn() -> bool,
    log: &mut QuitLog,
    limits: (Duration, Duration),
    watch: &Watch,
) -> anyhow::Result<()> {
    let mut quitting = Quitting::Asked;
    watch.tell(quitting);
    let mut deadline = Some(Instant::now() + limits.0);
    while running() {
        if let Some(news) = log.read().filter(|n| *n != quitting) {
            quitting = news;
            deadline = match quitting {
                Quitting::Asked => deadline,
                Quitting::Confirming => Some(Instant::now() + limits.1),
                Quitting::AfterWork => None,
            };
            watch.tell(quitting);
        }
        if watch.stopped() {
            return Err(NotReady::Stopped.into());
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(NotReady::StillOpen.into());
        }
        // Waiting for Claude's work can take hours: look less often then.
        let pause = if quitting == Quitting::AfterWork { 1000 } else { 250 };
        std::thread::sleep(Duration::from_millis(pause));
    }
    Ok(())
}

/// What Claude writes to `main.log` while it quits, read as it comes.
struct QuitLog {
    path: PathBuf,
    offset: u64,
}

impl QuitLog {
    fn start(path: &Path) -> QuitLog {
        let offset = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        QuitLog { path: path.to_path_buf(), offset }
    }

    /// The latest word on the quit in what was written since the last look.
    fn read(&mut self) -> Option<Quitting> {
        let len = fs::metadata(&self.path).ok()?.len();
        if len < self.offset {
            self.offset = 0; // a new log file
        }
        if len == self.offset {
            return None;
        }
        let mut f = fs::File::open(&self.path).ok()?;
        f.seek(SeekFrom::Start(self.offset)).ok()?;
        let mut text = Vec::new();
        f.take(len - self.offset).read_to_end(&mut text).ok()?;
        // Only whole lines; a line still being written is read next time.
        let end = text.iter().rposition(|b| *b == b'\n').map_or(0, |n| n + 1);
        self.offset += end as u64;
        quit_news(&text[..end])
    }
}

/// The latest word on a quit in some of Claude's log: its quit guard vetoing the quit to ask,
/// then deferring it when the answer is to wait.
fn quit_news(text: &[u8]) -> Option<Quitting> {
    let has = |line: &[u8], needle: &[u8]| line.windows(needle.len()).any(|w| w == needle);
    text.split(|b| *b == b'\n').fold(None, |news, line| {
        if has(line, b"vetoed by before-quit interceptor") {
            Some(Quitting::Confirming)
        } else if has(line, b"[updater-guard] restart deferred") {
            Some(Quitting::AfterWork)
        } else {
            news
        }
    })
}

/// Wait up to `timeout` while Claude's updater replaces the app: starting Claude, or changing
/// its data, in the middle of that leaves it broken.
pub fn wait_for_update(ctx: &Ctx, timeout: Duration) -> anyhow::Result<()> {
    if ctx.fake.running.is_some() {
        return Ok(());
    }
    let deadline = Instant::now() + timeout;
    while process::updating() {
        if Instant::now() >= deadline {
            return Err(NotReady::Updating.into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

/// Start Claude.
pub fn launch(ctx: &Ctx) -> anyhow::Result<()> {
    prepare_launch(ctx)?.launch()
}

pub fn last_known_account(ctx: &Ctx) -> Option<String> {
    let Ok(Value::Object(cfg)) = fsx::read_json(&ctx.paths.desktop_config(), 16 * 1024 * 1024) else {
        return None;
    };
    cfg.get("lastKnownAccountUuid").and_then(Value::as_str).filter(|s| is_uuid(s)).map(str::to_string)
}

/// The newest "Initialization succeeded" line in Desktop's log.
pub struct InitLine {
    pub acct: String,
    pub org: String,
    /// `<log file name>@<byte offset>`: changes whenever Desktop loads a session list again.
    pub marker: String,
    /// When Desktop wrote it (Unix seconds), when its timestamp can be read.
    pub at: Option<f64>,
}

pub fn last_init(ctx: &Ctx) -> Option<InitLine> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(&ctx.paths.desktop_logs)
        .ok()?
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("main") && n.ends_with(".log")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    let mut scans = ctx.caches.log_scan.lock().unwrap();
    for (_, path) in files.into_iter().take(4) {
        let scan = scans.entry(path.clone()).or_default();
        if let Some((acct, org)) = scan_log(&path, scan) {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            return Some(InitLine { acct, org, marker: format!("{name}@{}", scan.found_at), at: scan.found_time });
        }
    }
    None
}

/// Account and org from the newest "Initialization succeeded" line in Desktop's log.
pub fn last_init_pair(ctx: &Ctx) -> Option<(String, String)> {
    last_init(ctx).map(|i| (i.acct, i.org))
}

fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i] == needle[0] && &hay[i..i + needle.len()] == needle {
            out.push(i);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Re-read only what was appended since the last call (with some overlap).
fn scan_log(path: &Path, scan: &mut LogScan) -> Option<(String, String)> {
    let len = fs::metadata(path).ok()?.len();
    if len < scan.offset {
        *scan = LogScan::default(); // rotated or truncated
    }
    if scan.offset == 0 {
        scan.offset = len.saturating_sub(TAIL_BYTES);
    }
    let start = scan.offset.saturating_sub(512);
    let mut f = fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(len - start).read_to_end(&mut buf).ok()?;
    let needle = INIT_MARKER.as_bytes();
    for idx in find_all(&buf, needle) {
        let from = idx + needle.len();
        let tail = String::from_utf8_lossy(&buf[from..buf.len().min(from + 200)]);
        if let (Some(acct), Some(org)) = (value_after(&tail, "accountId="), value_after(&tail, "orgId=")) {
            scan.found = Some((acct, org));
            scan.found_at = start + idx as u64;
            let line = buf[..idx].iter().rposition(|b| *b == b'\n').map_or(0, |n| n + 1);
            scan.found_time = line_time(&buf[line..idx]);
        }
    }
    scan.offset = start + buf.len() as u64;
    scan.found.clone()
}

/// The local time a log line starts with, as electron-log writes it (`2026-09-30 11:57:01 …`,
/// or `[2026-09-30 11:57:01.123] …`), in Unix seconds.
fn line_time(line: &[u8]) -> Option<f64> {
    use chrono::{Local, NaiveDateTime, TimeZone as _};
    let text = String::from_utf8_lossy(line.get(..24.min(line.len()))?);
    let text = text.trim_start_matches('[');
    let stamp = NaiveDateTime::parse_from_str(text.get(..19)?, "%Y-%m-%d %H:%M:%S").ok()?;
    Local.from_local_datetime(&stamp).earliest().map(|t| t.timestamp() as f64)
}

fn value_after(text: &str, key: &str) -> Option<String> {
    let at = text.find(key)? + key.len();
    let v = text.get(at..at + 36)?;
    is_uuid(v).then(|| v.to_string())
}

/// Desktop's version, where it can be read cheaply.
pub fn desktop_version() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let plist = fs::read_to_string("/Applications/Claude.app/Contents/Info.plist").ok()?;
        let at = plist.find("<key>CFBundleShortVersionString</key>")?;
        let rest = &plist[at..];
        let start = rest.find("<string>")? + "<string>".len();
        let end = rest[start..].find("</string>")?;
        Some(rest[start..start + end].trim().to_string())
    }
    #[cfg(windows)]
    {
        // Squirrel installs live in %LOCALAPPDATA%\AnthropicClaude\app-<version>.
        let dir = dirs::data_local_dir()?.join("AnthropicClaude");
        let mut versions: Vec<String> = fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter_map(|e| e.file_name().to_string_lossy().strip_prefix("app-").map(str::to_string))
            .collect();
        versions.sort_by_key(|v| v.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>());
        versions.pop()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

mod process {
    //! A tiny process-table reader per platform, just enough to spot Claude Desktop.

    /// Claude Desktop's main executable.
    #[cfg(target_os = "macos")]
    const MAIN: &[u8] = b"Claude.app/Contents/MacOS/Claude";
    /// Where everything Claude Desktop runs lives: the app and its helpers.
    #[cfg(target_os = "macos")]
    const BUNDLE: &[u8] = b"Claude.app/Contents/";
    /// Squirrel's installer, which replaces the app during an update.
    #[cfg(target_os = "macos")]
    const UPDATER: &[u8] = b"Claude.app/Contents/Frameworks/Squirrel.framework/Resources/ShipIt";

    /// Process ids whose executable path `matches`; `None` when the table cannot be read.
    #[cfg(target_os = "macos")]
    fn pids(matches: impl Fn(&[u8]) -> bool) -> Option<Vec<i32>> {
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 {
            return None;
        }
        let mut pids = vec![0i32; count as usize + 256];
        let size = (pids.len() * std::mem::size_of::<i32>()) as i32;
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
        if count <= 0 {
            return None;
        }
        let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let mut found = Vec::new();
        for &pid in pids.iter().take(count as usize) {
            if pid <= 0 {
                continue;
            }
            let n = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
            if n > 0 && matches(&path[..n as usize]) {
                found.push(pid);
            }
        }
        Some(found)
    }

    #[cfg(target_os = "macos")]
    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[cfg(target_os = "macos")]
    pub fn desktop_running() -> Option<bool> {
        pids(|p| p.ends_with(MAIN)).map(|found| !found.is_empty())
    }

    /// Claude or any of its helpers, but not its updater.
    #[cfg(target_os = "macos")]
    pub fn any_running() -> bool {
        pids(|p| contains(p, BUNDLE) && !p.ends_with(UPDATER)).is_none_or(|found| !found.is_empty())
    }

    #[cfg(target_os = "macos")]
    pub fn updating() -> bool {
        pids(|p| p.ends_with(UPDATER)).is_some_and(|found| !found.is_empty())
    }

    /// SIGTERM: Electron quits the way it does from the Dock or the menu, without the
    /// "control Claude" permission prompt an Apple Event would bring.
    #[cfg(target_os = "macos")]
    pub fn ask_to_quit() {
        for pid in pids(|p| p.ends_with(MAIN)).unwrap_or_default() {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }

    #[cfg(target_os = "macos")]
    pub fn launch() -> anyhow::Result<()> {
        use anyhow::Context as _;
        let status = std::process::Command::new("/usr/bin/open")
            .args(["-b", "com.anthropic.claudefordesktop"])
            .stdin(std::process::Stdio::null())
            .status()
            .context("starting Claude")?;
        anyhow::ensure!(status.success(), "could not start Claude ({status})");
        Ok(())
    }

    #[cfg(not(target_os = "macos"))]
    pub fn any_running() -> bool {
        desktop_running().unwrap_or(true)
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    pub fn updating() -> bool {
        false
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    pub fn ask_to_quit() {}

    #[cfg(not(any(target_os = "macos", windows)))]
    pub fn launch() -> anyhow::Result<()> {
        anyhow::bail!("starting Claude is not supported on this system yet")
    }

    #[cfg(target_os = "linux")]
    pub fn desktop_running() -> Option<bool> {
        // Linux builds are unofficial (e.g. claude-desktop-debian): an Electron process whose
        // executable or command line names claude-desktop. The Claude Code CLI ("claude") is
        // deliberately not matched.
        let entries = std::fs::read_dir("/proc").ok()?;
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(pid) = name.to_str().filter(|s| s.bytes().all(|b| b.is_ascii_digit())) else { continue };
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            let comm = comm.trim();
            if comm == "claude-desktop" || comm == "Claude" {
                return Some(true);
            }
            if comm == "electron" || comm.starts_with("claude") {
                let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                if String::from_utf8_lossy(&cmd).contains("claude-desktop") {
                    return Some(true);
                }
            }
        }
        Some(false)
    }

    #[cfg(windows)]
    pub(super) use super::windows::{desktop_running, updating};

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    pub fn desktop_running() -> Option<bool> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_asking_and_waiting_shows_in_its_log() {
        let asked = b"2026-10-01 20:00:00 [info] beforeQuit: handler fired, going down\n\
2026-10-01 20:00:00 [info] beforeQuit: vetoed by before-quit interceptor\n";
        assert_eq!(quit_news(asked), Some(Quitting::Confirming));
        let waiting =
            b"2026-10-01 20:00:09 [info] [updater-guard] restart deferred; 2 local session(s) still running\n";
        assert_eq!(quit_news(waiting), Some(Quitting::AfterWork));
        assert_eq!(quit_news(b"2026-10-01 20:00:00 [info] willQuit: handler is ready for quit, so quitting\n"), None);
    }

    /// A stand-in for Claude while it quits: whether it runs, and its log.
    struct Claude {
        _dir: tempfile::TempDir,
        log: PathBuf,
        running: std::sync::Arc<AtomicBool>,
    }

    impl Claude {
        fn new() -> Claude {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("main.log");
            fs::write(&log, "2026-10-01 20:00:00 [info] started\n").unwrap();
            Claude { _dir: dir, log, running: std::sync::Arc::new(AtomicBool::new(true)) }
        }

        fn says(&self, line: &str) {
            use std::io::Write as _;
            let mut f = fs::OpenOptions::new().append(true).open(&self.log).unwrap();
            writeln!(f, "2026-10-01 20:00:01 [info] {line}").unwrap();
        }

        /// Its log from now on, as `quit` reads it right before asking Claude to quit.
        fn log(&self) -> QuitLog {
            QuitLog::start(&self.log)
        }

        /// Wait for it to quit as `quit` does, with `limits` in milliseconds; what was heard, and how
        /// it ended.
        fn wait(
            &self,
            mut log: QuitLog,
            stop: &AtomicBool,
            limits: (u64, u64),
        ) -> (Vec<Quitting>, Result<(), NotReady>) {
            let heard = std::sync::Mutex::new(Vec::new());
            let tell = |q: Quitting| heard.lock().unwrap().push(q);
            let watch = Watch { progress: Some(&tell), stop: Some(stop) };
            let running = self.running.clone();
            let limits = (Duration::from_millis(limits.0), Duration::from_millis(limits.1));
            let result = wait_for_quit(|| running.load(Ordering::Relaxed), &mut log, limits, &watch)
                .map_err(|e| *e.downcast_ref::<NotReady>().unwrap());
            (heard.into_inner().unwrap(), result)
        }
    }

    #[test]
    fn cancelling_in_claude_then_stopping_here_changes_nothing() {
        let claude = Claude::new();
        let stop = AtomicBool::new(false);
        let log = claude.log();
        std::thread::scope(|s| {
            s.spawn(|| {
                claude.says("beforeQuit: vetoed by before-quit interceptor");
                // Cancel in Claude says nothing; the user stops waiting here instead.
                std::thread::sleep(Duration::from_millis(200));
                stop.store(true, Ordering::Relaxed);
            });
            let (heard, result) = claude.wait(log, &stop, (5000, 5000));
            assert_eq!(heard, [Quitting::Asked, Quitting::Confirming]);
            assert_eq!(result, Err(NotReady::Stopped));
        });
    }

    #[test]
    fn waiting_for_claudes_work_waits_as_long_as_it_takes() {
        let claude = Claude::new();
        let stop = AtomicBool::new(false);
        let log = claude.log();
        std::thread::scope(|s| {
            s.spawn(|| {
                claude.says("beforeQuit: vetoed by before-quit interceptor");
                // The user reads the question for a moment (longer than one look at the log).
                std::thread::sleep(Duration::from_millis(400));
                claude.says("[updater-guard] restart deferred; 1 local session(s) still running");
                // Longer than either limit: the user chose to wait.
                std::thread::sleep(Duration::from_millis(1800));
                claude.says("[updater-guard] all local sessions idle; restarting");
                claude.running.store(false, Ordering::Relaxed);
            });
            let (heard, result) = claude.wait(log, &stop, (300, 1500));
            assert_eq!(heard, [Quitting::Asked, Quitting::Confirming, Quitting::AfterWork]);
            assert_eq!(result, Ok(()));
        });
    }

    #[test]
    fn claude_gets_a_limit_for_saying_nothing_and_for_asking() {
        let claude = Claude::new();
        let stop = AtomicBool::new(false);
        assert_eq!(claude.wait(claude.log(), &stop, (300, 5000)), (vec![Quitting::Asked], Err(NotReady::StillOpen)));
        // Asked, and nobody answers: the longer limit, then give up.
        claude.says("beforeQuit: vetoed by before-quit interceptor");
        let started = Instant::now();
        let mut log = QuitLog::start(&claude.log);
        log.offset = 0;
        let running = claude.running.clone();
        let result = wait_for_quit(
            || running.load(Ordering::Relaxed),
            &mut log,
            (Duration::from_millis(100), Duration::from_millis(400)),
            &Watch::default(),
        );
        assert_eq!(result.unwrap_err().downcast_ref::<NotReady>(), Some(&NotReady::StillOpen));
        assert!(started.elapsed() >= Duration::from_millis(400), "{:?}", started.elapsed());
    }

    #[test]
    fn the_quit_log_is_read_as_it_grows() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.log");
        fs::write(&path, "an old veto: beforeQuit: vetoed by before-quit interceptor\n").unwrap();
        let mut log = QuitLog::start(&path);
        assert_eq!(log.read(), None, "only what comes after the request counts");
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        // Half a line is left for the next look.
        write!(f, "beforeQuit: vetoed by before-quit").unwrap();
        assert_eq!(log.read(), None);
        writeln!(f, " interceptor").unwrap();
        assert_eq!(log.read(), Some(Quitting::Confirming));
        writeln!(f, "[updater-guard] restart deferred; 1 local session(s) still running").unwrap();
        assert_eq!(log.read(), Some(Quitting::AfterWork));
        // Claude started a new log.
        fs::write(&path, "x\n").unwrap();
        assert_eq!(log.read(), None);
    }
}
