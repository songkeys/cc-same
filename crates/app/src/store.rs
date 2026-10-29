//! What the window and the tray show, and what they can do. There is one per app, alive whether
//! or not the window is open, so the tray stays current with the window closed.

use crate::i18n::{t, tf, tn};
use crate::model::{self, Account, Busy, Mood};
use crate::theme::ThemeChoice;
use crate::update::{self, Failure, Problem, Release};
use cc_same_core::accounts::{self, Refused as ListRefused};
use cc_same_core::config::Config;
use cc_same_core::desktop::{NotReady, Quitting, Watch};
use cc_same_core::logins::{self, Refused};
use cc_same_core::report::{self, Overview};
use cc_same_core::retention::{self, Kept};
use cc_same_core::snapshot::{self, Manifest};
use cc_same_core::{ActionKind, Ctx, apply, desktop, fsx, service, watch};
use gpui_kit::{App, AppContext as _, Context, Entity, EventEmitter, Global};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(windows)]
const AGENT_NAME: &str = "cc-same-agent.exe";
#[cfg(not(windows))]
const AGENT_NAME: &str = "cc-same-agent";

/// How often the app looks for a new version, and how soon it looks again when it could not.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

/// Something the user should hear about: the outcome of a task they started.
pub enum StoreEvent {
    Done(Result<String, String>),
}

/// A switch waiting for Claude to quit: how that goes, and the way to stop waiting.
#[derive(Debug, Default)]
pub struct Waiting {
    quitting: AtomicU8,
    stop: AtomicBool,
}

impl Waiting {
    pub fn new(quitting: Quitting) -> Waiting {
        let waiting = Waiting::default();
        waiting.set(quitting);
        waiting
    }

    fn set(&self, quitting: Quitting) {
        let n = match quitting {
            Quitting::Asked => 0,
            Quitting::Confirming => 1,
            Quitting::AfterWork => 2,
        };
        self.quitting.store(n, Ordering::Relaxed);
    }

    pub fn quitting(&self) -> Quitting {
        match self.quitting.load(Ordering::Relaxed) {
            1 => Quitting::Confirming,
            2 => Quitting::AfterWork,
            _ => Quitting::Asked,
        }
    }
}

/// Claude was restarted on its sign-in page, to sign in to another account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigningIn {
    /// The account set aside to make room, to switch back to.
    pub previous: Option<String>,
    /// The account the user meant to sign in to, by name, when they picked one.
    pub expected: Option<String>,
}

/// Where an update stands.
#[derive(Clone, Debug, Default)]
pub enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    /// The bytes downloaded so far.
    Downloading(Arc<AtomicU64>),
    /// Downloaded and checked: the new copy waits to be swapped in.
    Ready(PathBuf),
    Installing,
}

/// New versions: what GitHub said last, and what is under way.
#[derive(Clone, Debug, Default)]
pub struct Updates {
    /// A release newer than this copy.
    pub available: Option<Release>,
    pub phase: UpdatePhase,
    /// When GitHub last answered (Unix seconds).
    pub checked_at: Option<f64>,
    /// What went wrong last, and the error behind it.
    pub problem: Option<(Problem, String)>,
    /// The version this copy replaced, until the note about it is put away.
    pub updated_from: Option<String>,
    next_check: Option<Instant>,
}

impl Updates {
    /// Checking, downloading or installing.
    pub fn working(&self) -> bool {
        matches!(self.phase, UpdatePhase::Checking | UpdatePhase::Downloading(_) | UpdatePhase::Installing)
    }

    /// How much of the download is done, from 0 to 1.
    pub fn progress(&self) -> Option<f32> {
        let UpdatePhase::Downloading(bytes) = &self.phase else { return None };
        let total = self.available.as_ref()?.asset.as_ref()?.size.max(1);
        Some((bytes.load(Ordering::Relaxed) as f64 / total as f64).clamp(0., 1.) as f32)
    }
}

pub struct Store {
    pub ctx: Arc<Ctx>,
    pub overview: Option<Arc<Overview>>,
    pub snapshots: Vec<Manifest>,
    pub busy: Option<Busy>,
    /// The app opens (hidden) at login.
    pub open_at_login: bool,
    /// When the background switch was last turned on: the agent gets a moment to start.
    pub switched_on_at: Option<Instant>,
    pub updates: Updates,
    /// The account a switch is heading for.
    pub switching_to: Option<String>,
    /// Waiting for a sign-in to another account in Claude.
    pub signing_in: Option<SigningIn>,
    /// A switch is waiting for Claude to quit.
    pub waiting: Option<Arc<Waiting>>,
    refreshing: bool,
    /// The background agent was compared with this version once.
    agent_checked: bool,
    fingerprint: u64,
    pending_since: Option<Instant>,
}

impl EventEmitter<StoreEvent> for Store {}

struct GlobalStore(Entity<Store>);

impl Global for GlobalStore {}

impl Store {
    fn blank(ctx: Arc<Ctx>) -> Store {
        Store {
            ctx,
            overview: None,
            snapshots: Vec::new(),
            busy: None,
            open_at_login: false,
            switched_on_at: None,
            updates: Updates::default(),
            switching_to: None,
            signing_in: None,
            waiting: None,
            refreshing: false,
            agent_checked: false,
            fingerprint: 0,
            pending_since: None,
        }
    }

    /// The live store: reads now, then again whenever anything it depends on changes.
    pub fn init(ctx: Arc<Ctx>, cx: &mut App) -> Entity<Store> {
        let store = cx.new(|cx| {
            let mut store = Store::blank(ctx);
            store.open_at_login = crate::login::enabled();
            store.updates.updated_from = update::updated_from(&store.ctx.paths).map(|v| v.to_string());
            // Finish an account switch a crash cut short, before Claude is next opened.
            let ctx = store.ctx.clone();
            cx.background_executor().spawn(async move { logins::recover_if_idle(&ctx) }).detach();
            store.refresh(cx);
            store.watch_for_changes(cx);
            store.watch_for_updates(cx);
            store
        });
        cx.set_global(GlobalStore(store.clone()));
        store
    }

    /// A still picture of a given state (screenshots): no polling and no disk access.
    pub fn preview(
        ctx: Arc<Ctx>,
        overview: Option<Overview>,
        snapshots: Vec<Manifest>,
        busy: Option<Busy>,
        cx: &mut App,
    ) -> Entity<Store> {
        let store = cx.new(|_| Store { overview: overview.map(Arc::new), snapshots, busy, ..Store::blank(ctx) });
        cx.set_global(GlobalStore(store.clone()));
        store
    }

    pub fn global(cx: &App) -> Entity<Store> {
        cx.global::<GlobalStore>().0.clone()
    }

    pub fn config(&self) -> Config {
        self.ctx.config()
    }

    pub fn theme(&self) -> ThemeChoice {
        ThemeChoice::from_config(&self.ctx.config().appearance)
    }

    pub fn mood(&self) -> Mood {
        model::mood(self.overview.as_deref(), self.busy == Some(Busy::Syncing), self.pending_since.map(|t| t.elapsed()))
    }

    pub fn accounts(&self) -> Vec<Account> {
        self.overview.as_deref().map(model::accounts).unwrap_or_default()
    }

    /// The background agent is registered.
    pub fn background_on(&self) -> bool {
        self.overview.as_ref().is_some_and(|o| o.service.installed)
    }

    // ------------------------------------------------------------------ reading

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        let ctx = self.ctx.clone();
        cx.spawn(async move |this, cx| {
            let (overview, snapshots) =
                cx.background_executor().spawn(async move { (report::overview(&ctx), snapshot::list(&ctx)) }).await;
            let _ = this.update(cx, |this, cx| {
                this.refreshing = false;
                this.pending_since = match (overview.plan.total > 0, this.pending_since) {
                    (true, None) => Some(Instant::now()),
                    (true, since) => since,
                    (false, _) => None,
                };
                let signed_in = overview.logins.signed_in.clone();
                this.overview = Some(Arc::new(overview));
                this.snapshots = snapshots;
                this.renew_agent(cx);
                // Claude was waiting on its sign-in page, and now someone signed in.
                if this.busy.is_none()
                    && let (Some(_), Some(account)) = (&this.signing_in, signed_in)
                {
                    this.signing_in = None;
                    let name = this.account_name(&account);
                    cx.emit(StoreEvent::Done(Ok(tf("toast.added", &[("account", &name)]))));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Re-read when Claude's session folders, Claude itself, our state or the agent change, and,
    /// while the window is open, Claude's record of plan use; and at least every 12 seconds.
    fn watch_for_changes(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut ticks = 0u32;
            loop {
                cx.background_executor().timer(Duration::from_millis(1500)).await;
                ticks += 1;
                let Ok((ctx, last, looking)) =
                    this.update(cx, |this, cx| (this.ctx.clone(), this.fingerprint, !cx.windows().is_empty()))
                else {
                    break;
                };
                let fp = cx.background_executor().spawn(async move { fingerprint(&ctx, looking) }).await;
                if fp != last || ticks.is_multiple_of(8) {
                    let alive = this.update(cx, |this, cx| {
                        this.fingerprint = fp;
                        this.refresh(cx);
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            }
        })
        .detach();
    }

    // ------------------------------------------------------------------ doing

    /// Run blocking work off the UI thread, then report and re-read.
    fn run_task(
        &mut self,
        busy: Busy,
        cx: &mut Context<Self>,
        work: impl FnOnce(&Ctx) -> anyhow::Result<String> + Send + 'static,
    ) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(busy);
        cx.notify();
        let ctx = self.ctx.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { work(&ctx) }).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                cx.emit(StoreEvent::Done(result.map_err(|e| format!("{e:#}"))));
                this.refresh(cx);
            });
        })
        .detach();
    }

    pub fn sync_now(&mut self, cx: &mut Context<Self>) {
        self.run_task(Busy::Syncing, cx, |ctx| {
            let (_, out) = apply::run_sync(ctx, "app")?;
            if !out.errors.is_empty() {
                anyhow::bail!("{}", tn("toast.write_errors", out.errors.len(), &[]));
            }
            let copied = out.applied.get(&ActionKind::WriteRecord).copied().unwrap_or(0);
            Ok(match (copied, out.applied_total()) {
                (0, 0) => t("toast.in_sync"),
                (0, n) => tn("toast.applied", n, &[]),
                (n, _) => tn("toast.copied", n, &[]),
            })
        });
    }

    pub fn set_background(&mut self, on: bool, cx: &mut Context<Self>) {
        self.switched_on_at = on.then(Instant::now);
        self.run_task(Busy::Service, cx, move |ctx| {
            if on {
                install_agent(ctx)?;
            } else {
                service::uninstall(ctx)?;
            }
            // The switch shows the result.
            Ok(String::new())
        });
    }

    pub fn set_open_at_login(&mut self, on: bool, cx: &mut Context<Self>) {
        self.open_at_login = on;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { crate::login::set(on) }).await;
            let _ = this.update(cx, |this, cx| {
                this.open_at_login = crate::login::enabled();
                if let Err(e) = result {
                    cx.emit(StoreEvent::Done(Err(format!("{e:#}"))));
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn restore(&mut self, id: String, cx: &mut Context<Self>) {
        self.run_task(Busy::Restoring, cx, move |ctx| {
            snapshot::restore(ctx, &id)?;
            Ok(t("toast.restored"))
        });
    }

    pub fn fix_links(&mut self, cx: &mut Context<Self>) {
        self.run_task(Busy::Fixing, cx, |ctx| {
            let fixed = snapshot::fix_symlinks(ctx)?;
            apply::run_sync(ctx, "app")?;
            Ok(tn("toast.fixed", fixed.len(), &[]))
        });
    }

    /// Keep the transcripts behind Desktop sessions, whichever setting it takes.
    pub fn keep_transcripts(&mut self, cx: &mut Context<Self>) {
        self.run_task(Busy::Saving, cx, |ctx| {
            Ok(match retention::keep(&ctx.paths)? {
                Kept::TenYears => t("toast.kept"),
                Kept::AnyAge | Kept::Already => t("toast.kept_unlimited"),
            })
        });
    }

    /// Put back what CC Same changed in Claude Code's settings.
    pub fn undo_retention(&mut self, cx: &mut Context<Self>) {
        self.run_task(Busy::Saving, cx, |ctx| {
            retention::undo(&ctx.paths)?;
            Ok(t("toast.undone"))
        });
    }

    /// Set the background agent up again when it no longer matches this app (once, at the first
    /// reading): after an update it still runs the old version; it is installed but not loaded
    /// (a restart that failed); or, on macOS, 0.1.0 and 0.1.1 registered a copy of the app that the
    /// system refuses to run. Quietly: the background switch shows how that went.
    fn renew_agent(&mut self, cx: &mut Context<Self>) {
        let Some(overview) = self.overview.clone() else { return };
        if std::mem::replace(&mut self.agent_checked, true) || !overview.service.installed || !update::self_managing() {
            return;
        }
        let behind = overview
            .heartbeat
            .as_ref()
            .and_then(|h| semver::Version::parse(&h.version).ok())
            .is_some_and(|v| v < update::current());
        let unloaded = overview.service.loaded == Some(false);
        let outdated = agent_outdated(&overview.service.program, &self.ctx.paths.bin_dir());
        if !(behind || unloaded || outdated) || self.busy.is_some() {
            return;
        }
        self.busy = Some(Busy::Service);
        self.switched_on_at = Some(Instant::now());
        cx.notify();
        let ctx = self.ctx.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { install_agent(&ctx) }).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                if let Err(e) = result {
                    eprintln!("setting the background agent up again: {e:#}");
                }
                this.refresh(cx);
            });
        })
        .detach();
    }

    // ------------------------------------------------------------------ updates

    /// Look for a new version a few seconds after starting and then once a day, and install a
    /// downloaded one while nobody is looking.
    fn watch_for_updates(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(8)).await;
            loop {
                let alive = this.update(cx, |this, cx| {
                    let due = this.updates.next_check.is_none_or(|at| Instant::now() >= at);
                    if due && this.config().check_updates && update::self_managing() {
                        this.check_for_updates(false, cx);
                    }
                    this.install_when_idle(cx);
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(10 * 60)).await;
            }
        })
        .detach();
    }

    /// Ask GitHub for the latest version. `manual` reports the answer either way.
    pub fn check_for_updates(&mut self, manual: bool, cx: &mut Context<Self>) {
        if !matches!(self.updates.phase, UpdatePhase::Idle) {
            return;
        }
        self.updates.phase = UpdatePhase::Checking;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async { update::check() }).await;
            let _ = this.update(cx, |this, cx| this.checked(result, manual, cx));
        })
        .detach();
    }

    fn checked(&mut self, result: update::Outcome<Option<Release>>, manual: bool, cx: &mut Context<Self>) {
        self.updates.phase = UpdatePhase::Idle;
        match result {
            Ok(release) => {
                self.updates.checked_at = Some(fsx::now_secs());
                self.updates.next_check = Some(Instant::now() + CHECK_EVERY);
                self.updates.problem = None;
                self.updates.available = release;
                match &self.updates.available {
                    Some(release) => {
                        if self.config().auto_update && release.asset.is_some() && update::target().is_ok() {
                            self.download_update(false, cx);
                        }
                    }
                    None if manual => {
                        cx.emit(StoreEvent::Done(Ok(tf("update.up_to_date", &[("version", &update::VERSION)]))));
                    }
                    None => {}
                }
            }
            Err(failure) => {
                self.updates.next_check = Some(Instant::now() + RETRY_AFTER);
                eprintln!("checking for updates: {:#}", failure.error);
                if manual {
                    cx.emit(StoreEvent::Done(Err(t(failure.problem.message_key()))));
                }
                // With a release already found, its note stays as it is.
                if self.updates.available.is_none() {
                    self.updates.problem = Some((failure.problem, failure.error.root_cause().to_string()));
                }
            }
        }
        cx.notify();
    }

    /// Update now: install what is downloaded, download what is available (then install it), or
    /// look for something new.
    pub fn update_or_check(&mut self, cx: &mut Context<Self>) {
        match (&self.updates.phase, &self.updates.available) {
            (UpdatePhase::Ready(staged), _) => {
                let staged = staged.clone();
                self.install_update(staged, false, cx);
            }
            (UpdatePhase::Idle, Some(_)) => self.download_update(true, cx),
            (UpdatePhase::Idle, None) => self.check_for_updates(true, cx),
            _ => {}
        }
    }

    /// Download the available release and check it; `then_install` swaps it in right after.
    fn download_update(&mut self, then_install: bool, cx: &mut Context<Self>) {
        let Some(release) = self.updates.available.clone() else { return };
        if !matches!(self.updates.phase, UpdatePhase::Idle) {
            return;
        }
        let Some(asset) = release.asset.clone() else {
            let error = anyhow::anyhow!("release {} has no archive for this system", release.version);
            return self.update_failed(Failure { problem: Problem::NoBuild, error }, cx);
        };
        let target = match update::target() {
            Ok(target) => target,
            Err(failure) => return self.update_failed(failure, cx),
        };
        let bytes = Arc::new(AtomicU64::new(0));
        self.updates.phase = UpdatePhase::Downloading(bytes.clone());
        self.updates.problem = None;
        cx.notify();
        let dir = update::updates_dir(&self.ctx.paths);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let archive = update::download(&asset, &dir, &bytes)?;
                    update::unpack(&archive, &release.version, &target)
                })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(staged) => {
                    this.updates.phase = UpdatePhase::Ready(staged.clone());
                    if then_install {
                        this.install_update(staged, false, cx);
                    } else {
                        this.install_when_idle(cx);
                    }
                    cx.notify();
                }
                Err(failure) => this.update_failed(failure, cx),
            });
        })
        .detach();
        // Redraw the progress while it downloads.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let downloading = this.update(cx, |this, cx| {
                    let downloading = matches!(this.updates.phase, UpdatePhase::Downloading(_));
                    if downloading {
                        cx.notify();
                    }
                    downloading
                });
                if !matches!(downloading, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Swap the downloaded copy in and restart into it; `hidden` restarts without the window.
    fn install_update(&mut self, staged: PathBuf, hidden: bool, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        let target = match update::target() {
            Ok(target) => target,
            Err(failure) => return self.update_failed(failure, cx),
        };
        self.updates.phase = UpdatePhase::Installing;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { update::install(&staged, &target, hidden) }).await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => cx.quit(),
                Err(failure) => this.update_failed(failure, cx),
            });
        })
        .detach();
    }

    /// With automatic updates on, install a downloaded one once the window is closed and nothing
    /// is running.
    pub fn install_when_idle(&mut self, cx: &mut Context<Self>) {
        if let UpdatePhase::Ready(staged) = &self.updates.phase
            && self.config().auto_update
            && self.busy.is_none()
            && cx.windows().is_empty()
        {
            let staged = staged.clone();
            self.install_update(staged, true, cx);
        }
    }

    /// With automatic updates on, put a downloaded update in place on the way out; the next start
    /// is the new version.
    pub fn install_on_quit(&mut self) {
        if let UpdatePhase::Ready(staged) = &self.updates.phase
            && self.config().auto_update
            && self.busy.is_none()
        {
            let installed = update::target().and_then(|target| update::install_on_quit(staged, &target));
            if let Err(failure) = installed {
                eprintln!("updating on quit: {:#}", failure.error);
            }
        }
    }

    fn update_failed(&mut self, failure: Failure, cx: &mut Context<Self>) {
        eprintln!("updating: {:#}", failure.error);
        self.updates.phase = UpdatePhase::Idle;
        self.updates.problem = Some((failure.problem, failure.error.root_cause().to_string()));
        cx.notify();
    }

    /// Put away the note that this copy was just updated.
    pub fn dismiss_updated(&mut self, cx: &mut Context<Self>) {
        if self.updates.updated_from.take().is_some() {
            cx.notify();
        }
    }

    // ------------------------------------------------------------------ accounts

    /// Switch Claude to `account`: Claude quits and starts again if it is open.
    pub fn switch_account(&mut self, account: String, cx: &mut Context<Self>) {
        self.run_switch(Some(account), None, cx);
    }

    /// Restart Claude on its sign-in page, to sign in to another account: the account in use is
    /// set aside, ready to switch back to. `expected` names the account the user means to add.
    pub fn sign_in_another(&mut self, expected: Option<String>, cx: &mut Context<Self>) {
        self.run_switch(None, expected, cx);
    }

    fn run_switch(&mut self, to: Option<String>, expected: Option<String>, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(Busy::Switching);
        self.switching_to = to.clone();
        let waiting = Arc::new(Waiting::default());
        self.waiting = Some(waiting.clone());
        cx.notify();
        let ctx = self.ctx.clone();
        cx.spawn(async move |this, cx| {
            let target = to.clone();
            let result = cx
                .background_executor()
                .spawn({
                    let waiting = waiting.clone();
                    async move {
                        let tell = |quitting: Quitting| waiting.set(quitting);
                        let watch = Watch { progress: Some(&tell), stop: Some(&waiting.stop) };
                        match &target {
                            Some(account) => accounts::switch(&ctx, account, &watch),
                            None => accounts::add(&ctx, &watch),
                        }
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = None;
                this.switching_to = None;
                this.waiting = None;
                match result {
                    Ok(switched) => match &switched.to {
                        Some(account) => {
                            this.signing_in = None;
                            let name = this.account_name(account);
                            cx.emit(StoreEvent::Done(Ok(tf("toast.switched", &[("account", &name)]))));
                        }
                        None => this.signing_in = Some(SigningIn { previous: switched.from, expected }),
                    },
                    // Stopped by the user: nothing to apologize for.
                    Err(error) if error.downcast_ref::<NotReady>() == Some(&NotReady::Stopped) => {
                        let after_work = waiting.quitting() == Quitting::AfterWork;
                        cx.emit(StoreEvent::Done(Ok(t(if after_work {
                            "switch.stopped_after_work"
                        } else {
                            "switch.stopped"
                        }))));
                    }
                    Err(error) => cx.emit(StoreEvent::Done(Err(explain_switch(&error)))),
                }
                this.refresh(cx);
            });
        })
        .detach();
        // Show how Claude's quit goes as it changes.
        cx.spawn(async move |this, cx| {
            let mut shown = None;
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let going = this.update(cx, |this, cx| {
                    let quitting = this.waiting.as_ref().map(|w| w.quitting());
                    if quitting != shown {
                        shown = quitting;
                        cx.notify();
                    }
                    quitting.is_some()
                });
                if !matches!(going, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// How Claude's quit goes while a switch waits for it.
    pub fn quitting(&self) -> Option<Quitting> {
        self.waiting.as_ref().map(|w| w.quitting())
    }

    /// Stop waiting for Claude to quit: the switch ends there, with nothing changed.
    pub fn stop_waiting(&mut self, cx: &mut Context<Self>) {
        if let Some(waiting) = &self.waiting {
            waiting.stop.store(true, Ordering::Relaxed);
            cx.notify();
        }
    }

    /// The account "Next Account" switches to: the next one in the list that Claude can switch to
    /// and that takes part.
    pub fn next_account(&self) -> Option<String> {
        let ov = self.overview.as_deref()?;
        let can_switch = |s: &accounts::Slot| ov.logins.saved(&s.account).is_some();
        let current = ov.logins.signed_in.as_deref();
        let next =
            accounts::pick(&ov.roster, current, accounts::Strategy::Next, can_switch, &ov.usage, fsx::now_secs());
        next.map(|s| s.account.clone())
    }

    /// Take an account off the list, forgetting the sign-in kept for it.
    pub fn remove_account(&mut self, account: String, cx: &mut Context<Self>) {
        let name = self.account_name(&account);
        self.run_task(Busy::Saving, cx, move |ctx| {
            accounts::remove(ctx, &account)?;
            Ok(tf("toast.removed", &[("account", &name)]))
        });
    }

    /// Put an account in "Next Account", or leave it out.
    pub fn set_in_rotation(&mut self, account: String, on: bool, cx: &mut Context<Self>) {
        self.run_task(Busy::Saving, cx, move |ctx| {
            accounts::set_disabled(ctx, &account, !on)?;
            Ok(String::new())
        });
    }

    /// Give an account a name (its alias in the list), or take it away. It only edits the list,
    /// so unlike the tasks above it does not wait for a switch or a sync to finish.
    pub fn set_alias(&mut self, account: String, alias: Option<String>, cx: &mut Context<Self>) {
        let ctx = self.ctx.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { accounts::set_alias(&ctx, &account, alias.as_deref()) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    cx.emit(StoreEvent::Done(Err(format!("{e:#}"))));
                }
                this.refresh(cx);
            });
        })
        .detach();
    }

    /// Note when an account's weekly window renews (a moment it renews, in Unix seconds), or forget
    /// it. Like the name, it only edits the list.
    pub fn set_weekly_reset(&mut self, account: String, at: Option<i64>, cx: &mut Context<Self>) {
        let ctx = self.ctx.clone();
        cx.spawn(async move |this, cx| {
            let result =
                cx.background_executor().spawn(async move { accounts::set_weekly_reset(&ctx, &account, at) }).await;
            let _ = this.update(cx, |this, cx| {
                if let Err(e) = result {
                    cx.emit(StoreEvent::Done(Err(format!("{e:#}"))));
                }
                this.refresh(cx);
            });
        })
        .detach();
    }

    /// An account's name as the window shows it.
    pub fn account_name(&self, id: &str) -> String {
        if let Some(account) = self.accounts().into_iter().find(|a| a.id == id) {
            return account.name;
        }
        let saved = self.overview.as_ref().and_then(|o| o.logins.saved(id).and_then(|s| s.email.clone()));
        saved.unwrap_or_else(|| tf("account.unnamed", &[("id", &cc_same_core::short(id))]))
    }

    /// Change a setting now and save it in the background.
    pub fn update_config(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Config)) {
        let mut cfg = self.ctx.config();
        change(&mut cfg);
        self.ctx.set_config(cfg.clone());
        cx.notify();
        let ctx = self.ctx.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = cfg.save(&ctx.paths) {
                    ctx.log(format!("saving settings: {e:#}"));
                }
            })
            .detach();
    }
}

/// Register this app as the background agent and start it.
fn install_agent(ctx: &Ctx) -> anyhow::Result<()> {
    if !ctx.paths.config_file().exists() {
        ctx.config().save(&ctx.paths)?;
    }
    let exe = std::env::current_exe()?;
    if cfg!(target_os = "macos") {
        // The app's signature covers its executable only inside the app: macOS refuses to run a
        // copy, so the agent runs this very file, which has to stay.
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        if update::temporary_place(&exe) {
            anyhow::bail!("{}", t("background.move_first"));
        }
        service::install_in_place(ctx, &exe)?;
        // The copy an earlier version ran is of no more use.
        let _ = std::fs::remove_file(ctx.paths.bin_dir().join(AGENT_NAME));
    } else {
        service::install(ctx, &exe, AGENT_NAME)?;
    }
    Ok(())
}

/// Why a switch did not happen, in words people can act on.
fn explain_switch(error: &anyhow::Error) -> String {
    if let Some(ListRefused::NoOther) = error.downcast_ref::<ListRefused>() {
        return t("switch.no_other");
    }
    if let Some(refused) = error.downcast_ref::<Refused>() {
        return t(match refused {
            Refused::Unsupported => "switch.unsupported",
            Refused::NothingSaved => "switch.nothing_saved",
        });
    }
    if let Some(not_ready) = error.downcast_ref::<NotReady>() {
        return t(match not_ready {
            NotReady::StillOpen => "switch.still_open",
            NotReady::Updating => "switch.updating",
            NotReady::Stopped => "switch.stopped",
        });
    }
    format!("{error:#}")
}

/// Whether the registered agent has to be set up again from this app, on macOS: it is the copy
/// in `bin` an earlier version made, or the app it ran from is gone (moved or deleted).
fn agent_outdated(program: &[String], bin: &Path) -> bool {
    let Some(registered) = program.first().map(Path::new) else { return false };
    cfg!(target_os = "macos")
        && (registered == bin.join(AGENT_NAME)
            || (registered.to_string_lossy().contains(".app/Contents/MacOS/") && !registered.exists()))
}

/// Changes worth re-reading for: Claude's session folders and state, and ours (the account list
/// included).
/// What the window shows changes with; `usage` adds Claude's record of plan use, so a new reading
/// shows as soon as Claude writes it while someone is looking.
fn fingerprint(ctx: &Ctx, usage: bool) -> u64 {
    let mut h = DefaultHasher::new();
    watch::fingerprint(ctx, desktop::is_running(ctx)).hash(&mut h);
    let paths = &ctx.paths;
    let mut files = vec![
        paths.state_file(),
        paths.heartbeat_file(),
        paths.config_file(),
        paths.claude_settings.clone(),
        paths.roster_file(),
    ];
    if usage {
        files.push(paths.usage_history());
    }
    for p in files {
        if let Ok(md) = std::fs::metadata(&p) {
            fsx::mtime_ns(&md).hash(&mut h);
        }
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_copied_by_an_earlier_version_or_left_behind_is_set_up_again() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let program = |path: &Path| vec![path.to_string_lossy().into_owned(), "watch".to_string()];
        let expected = cfg!(target_os = "macos");
        assert_eq!(agent_outdated(&program(&bin.join(AGENT_NAME)), &bin), expected);
        assert_eq!(agent_outdated(&program(&dir.path().join("Gone.app/Contents/MacOS/CC Same")), &bin), expected);
        // The app it runs from is still there, or it is the command-line tool's own copy.
        let app = dir.path().join("CC Same.app/Contents/MacOS/CC Same");
        std::fs::create_dir_all(app.parent().unwrap()).unwrap();
        std::fs::write(&app, "").unwrap();
        assert!(!agent_outdated(&program(&app), &bin));
        assert!(!agent_outdated(&program(&bin.join("cc-same")), &bin));
        assert!(!agent_outdated(&[], &bin));
    }
}
