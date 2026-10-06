//! Commands to run when something happens, set in `config.json` (`onSwitch`, `onSyncError`): to
//! send a notification somewhere else, say, or to set something up for the account Claude
//! switched to. A hook runs through the shell, with what happened in its environment:
//!
//! | | |
//! | --- | --- |
//! | `CC_SAME_EVENT` | `switch` or `sync-error` |
//! | `CC_SAME_MESSAGE` | one sentence saying what happened |
//! | `CC_SAME_FROM`, `CC_SAME_TO` | the accounts switched from and to (empty when signed out) |
//! | `CC_SAME_FROM_EMAIL`, `CC_SAME_TO_EMAIL` | their emails, when known |
//! | `CC_SAME_ERRORS` | how many changes failed or folders were skipped (`sync-error`) |
//!
//! A hook never holds a switch or a sync up: it starts and CC Same carries on. It runs in a
//! process group of its own, and while the command it ran is still running after a minute, the
//! whole group is stopped. What the command leaves running in the background after it has ended
//! is its own business, as in any shell. The command line waits for its hooks (up to that minute)
//! before it exits, so nobody is left to stop them; the app and the background agent watch theirs
//! while they run (a hook still running when the app quits is left to finish). A sync error runs
//! its hook once, when syncing starts failing, not on every pass while it keeps failing.
//!
//! The configuration is read again from disk for every hook, so a hook set or removed from the
//! command line applies at once, in the app as well.

use crate::ctx::{Ctx, Logger};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(60);

/// The threads watching hooks that are still running.
static RUNNING: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Switch { from: Option<String>, to: Option<String> },
    SyncError { errors: usize, first: String },
}

impl Event {
    fn name(&self) -> &'static str {
        match self {
            Event::Switch { .. } => "switch",
            Event::SyncError { .. } => "sync-error",
        }
    }
}

/// Run the hook set for `event`, if there is one.
pub fn run(ctx: &Ctx, event: &Event) {
    let cfg = crate::Config::load(&ctx.paths).unwrap_or_else(|_| ctx.config());
    let command = match event {
        Event::Switch { .. } => cfg.on_switch,
        Event::SyncError { .. } => cfg.on_sync_error,
    };
    let Some(command) = command.filter(|c| !c.trim().is_empty()) else { return };
    let mut labels = crate::scan::account_labels(ctx);
    for slot in crate::accounts::Roster::load(&ctx.paths).slots {
        if let Some(email) = slot.email {
            labels.entry(slot.account).or_insert(email);
        }
    }
    let label = |a: &Option<String>| match a {
        Some(a) => labels.get(a).cloned().unwrap_or_else(|| format!("account {}", crate::short(a))),
        None => "nobody".to_string(),
    };
    let mut env: Vec<(&str, String)> = vec![("CC_SAME_EVENT", event.name().into())];
    match event {
        Event::Switch { from, to } => {
            env.push(("CC_SAME_MESSAGE", format!("Claude switched from {} to {}", label(from), label(to))));
            env.push(("CC_SAME_FROM", from.clone().unwrap_or_default()));
            env.push(("CC_SAME_TO", to.clone().unwrap_or_default()));
            let email = |a: &Option<String>| a.as_ref().and_then(|a| labels.get(a)).cloned().unwrap_or_default();
            env.push(("CC_SAME_FROM_EMAIL", email(from)));
            env.push(("CC_SAME_TO_EMAIL", email(to)));
        }
        Event::SyncError { errors, first } => {
            env.push(("CC_SAME_MESSAGE", format!("CC Same could not sync {errors} change(s) or folder(s): {first}")));
            env.push(("CC_SAME_ERRORS", errors.to_string()));
        }
    }
    let mut cmd = shell(&command);
    cmd.envs(env).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let name = event.name();
    match cmd.spawn() {
        Ok(child) => {
            let logger = Logger::new(ctx.logger.sink());
            let watcher = std::thread::spawn(move || {
                let outcome = supervise(child);
                logger.log(format!("hook {name}: {outcome}"));
            });
            let mut running = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
            running.retain(|h| !h.is_finished());
            running.push(watcher);
        }
        Err(e) => ctx.log(format!("hook {name}: could not start: {e}")),
    }
}

/// Wait for the hooks still running, each up to its time limit: before a short-lived process
/// exits, which would leave nobody to stop them.
pub fn wait() {
    let running = std::mem::take(&mut *RUNNING.lock().unwrap_or_else(|e| e.into_inner()));
    for h in running {
        let _ = h.join();
    }
}

/// Wait for the hook, stopping its whole process group after [`TIMEOUT`]; say how it ended.
fn supervise(mut child: Child) -> String {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return "done".into(),
            Ok(Some(status)) => return format!("failed ({status})"),
            Err(e) => return format!("lost track of it: {e}"),
            Ok(None) if Instant::now() >= deadline => {
                kill_tree(&mut child);
                let _ = child.wait();
                return format!("stopped after {} seconds", TIMEOUT.as_secs());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

#[cfg(unix)]
fn shell(command: &str) -> Command {
    use std::os::unix::process::CommandExt as _;
    let mut cmd = Command::new("/bin/sh");
    // Its own process group, so a timeout stops everything it started.
    cmd.arg("-c").arg(command).process_group(0);
    cmd
}

#[cfg(unix)]
fn kill_tree(child: &mut Child) {
    // SAFETY: a plain syscall, to the group made for this child alone.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
}

#[cfg(windows)]
fn shell(command: &str) -> Command {
    use std::os::windows::process::CommandExt as _;
    let mut cmd = Command::new("cmd");
    // /D: no AutoRun commands from the registry. /S with the whole command quoted once: cmd strips
    // exactly those outer quotes and keeps the command's own, such as around a program's path.
    cmd.raw_arg(format!("/D /S /C \"{command}\"")).creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
fn kill_tree(child: &mut Child) {
    use std::os::windows::process::CommandExt as _;
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    let _ = child.kill();
}

/// Whether syncing has been failing, so a run of failed passes runs the hook once.
#[derive(Debug, Default)]
pub struct Failing(bool);

impl Failing {
    /// Note a pass's errors (failed changes and skipped folders); runs the hook when syncing
    /// starts failing. A pass without any counts as working again only when it was `settled`:
    /// nothing left waiting for Claude, which might be what failed.
    pub fn pass(&mut self, ctx: &Ctx, errors: &[String], settled: bool) {
        let failing = !errors.is_empty();
        if failing && !self.0 {
            let first = errors.first().cloned().unwrap_or_default();
            run(ctx, &Event::SyncError { errors: errors.len(), first });
        }
        if failing || settled {
            self.0 = failing;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, FakeDesktop, LogSink, Paths};
    use std::path::Path;

    /// The hooks' watchers are one list for the whole process: tests that wait on it take turns.
    static TURN: Mutex<()> = Mutex::new(());

    fn ctx(dir: &Path, config: Config) -> Ctx {
        let mut paths = Paths::new(dir.join("Claude"), dir.join("state"));
        paths.claude_json = dir.join(".claude.json");
        config.save(&paths).unwrap();
        Ctx::new(paths, config, FakeDesktop { running: Some(false), active: None }, LogSink::Silent)
    }

    #[cfg(unix)]
    #[test]
    fn a_switch_runs_its_hook_with_what_happened() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        let hook = format!("echo \"$CC_SAME_EVENT|$CC_SAME_FROM|$CC_SAME_TO|$CC_SAME_MESSAGE\" > '{}'", out.display());
        let ctx = ctx(tmp.path(), Config { on_switch: Some(hook), ..Config::default() });
        let from = "aaaaaaaa-0000-4000-8000-000000000001".to_string();
        run(&ctx, &Event::Switch { from: Some(from.clone()), to: None });
        wait();
        let text = std::fs::read_to_string(&out).unwrap();
        assert_eq!(text, format!("switch|{from}||Claude switched from account aaaaaaaa to nobody\n"));
    }

    #[cfg(unix)]
    #[test]
    fn sync_errors_run_their_hook_once_until_syncing_works_again() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        let hook = format!("echo \"$CC_SAME_ERRORS\" >> '{}'", out.display());
        let ctx = ctx(tmp.path(), Config { on_sync_error: Some(hook), ..Config::default() });
        let mut failing = Failing::default();
        failing.pass(&ctx, &["disk full".into(), "disk full".into()], true);
        wait();
        failing.pass(&ctx, &["disk full".into()], true);
        // Nothing failed, but changes were left waiting for Claude: not known to work again yet.
        failing.pass(&ctx, &[], false);
        failing.pass(&ctx, &["disk full".into()], true);
        failing.pass(&ctx, &[], true);
        failing.pass(&ctx, &["locked".into()], true);
        wait();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "2\n1\n");
    }

    /// Set from the command line while the app runs with the config it read earlier: it runs.
    #[cfg(unix)]
    #[test]
    fn the_hook_set_now_is_the_one_that_runs() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        let ctx = ctx(tmp.path(), Config::default());
        let hook = format!("echo ran > '{}'", out.display());
        Config { on_switch: Some(hook), ..Config::default() }.save(&ctx.paths).unwrap();
        run(&ctx, &Event::Switch { from: None, to: None });
        wait();
        assert!(out.exists());
    }

    /// A hook that hangs, and has started something that would outlive it, is stopped whole.
    #[cfg(unix)]
    #[test]
    fn a_hook_that_hangs_is_stopped_with_everything_it_started() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("pid");
        let mut child = shell(&format!("sleep 300 & echo $! > '{}'; wait", pid_file.display())).spawn().unwrap();
        let started = Instant::now();
        while std::fs::read_to_string(&pid_file).map_or(true, |p| !p.ends_with('\n')) {
            assert!(started.elapsed() < Duration::from_secs(5), "the hook did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        let sleeper: libc::pid_t = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        kill_tree(&mut child);
        let _ = child.wait();
        let gone = Instant::now();
        // SAFETY: signal 0 only asks whether the process exists.
        while unsafe { libc::kill(sleeper, 0) } == 0 {
            assert!(gone.elapsed() < Duration::from_secs(5), "the hook's child outlived it");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn no_hook_no_command() {
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx(tmp.path(), Config::default());
        run(&ctx, &Event::Switch { from: None, to: None });
        Failing::default().pass(&ctx, &["x".into()], true);
        wait();
    }
}
