//! Render the app in every state to PNG files, from sample data (macOS, Metal headless).
//!
//!     cargo app --example screenshot -- <out-dir> [scene ...]
//!
//! Nothing here reads or writes real Claude data: each scene builds its own temporary Claude
//! data folder and computes the overview with the real sync engine. The scene `live` renders
//! this machine's data instead, read-only.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("the screenshot example needs macOS (Metal headless rendering)");
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    mac::main()
}

#[cfg(target_os = "macos")]
mod mac {
    use cc_same_app::i18n;
    use cc_same_app::model::Busy;
    use cc_same_app::store::{SigningIn, Store, UpdatePhase, Updates, Waiting};
    use cc_same_app::theme::{self, ThemeChoice};
    use cc_same_app::update::{self, Asset, Problem, Release};
    use cc_same_app::view::UniApp;
    use cc_same_core::accounts::{Roster, Slot};
    use cc_same_core::config::{LastSync, PendingRestart};
    use cc_same_core::desktop::Quitting;
    use cc_same_core::logins::{Logins, Saved};
    use cc_same_core::report::{self, Overview};
    use cc_same_core::service::{Heartbeat, ServiceStatus};
    use cc_same_core::snapshot::{Manifest, SnapshotPart};
    use cc_same_core::usage::Usage;
    use cc_same_core::{Config, Ctx, FakeDesktop, LogSink, Paths, Surface, apply, fsx};
    use gpui_kit::base::Root;
    use gpui_kit::{AppContext as _, Entity, HeadlessAppContext, px, size};
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    const ADA: (&str, &str) = ("5a1d3c07-8f2e-4b6a-9c1d-2e3f4a5b6c7d", "0f1e2d3c-4b5a-4968-8776-655443322110");
    const GRACE: (&str, &str) = ("9e31ea7e-1c2d-4e5f-8a9b-0c1d2e3f4a5b", "58e6a9d0-2b3c-4d5e-8f60-718293a4b5c6");
    const THIRD: (&str, &str) = ("c47b90e2-6d5c-4b3a-9281-7f6e5d4c3b2a", "e1d2c3b4-a596-4877-9665-544332211009");

    const WIDTH: f32 = 420.;
    const HEIGHT: f32 = 600.;

    #[derive(Clone, Copy)]
    enum Overlay {
        None,
        Settings,
        ConfirmRestore,
        Notes,
        /// Settings, with the answer to "Check now" on top.
        SettingsToast,
        /// About to restart Claude to sign in to another account.
        SignIn,
        /// Asked from the menu bar: switch Claude to Grace?
        ConfirmSwitch,
        /// Asked from the menu bar for the account with no sign-in kept yet.
        ConfirmSwitchUnsaved,
        /// About to take Grace off the list.
        ConfirmRemove,
        /// "Next Account" from the menu bar.
        ConfirmNext,
        /// Name… from an account's menu.
        Name(&'static str),
        /// Name… for Grace, after typing a name with a space in it and pressing Return.
        NameRefused,
        /// The pointer rests on an account's row.
        Hover(&'static str),
    }

    /// Where switching accounts stands in a scene.
    #[derive(Clone, Copy)]
    enum Switching {
        Idle,
        /// On the way to Grace.
        ToGrace,
        /// Restarting Claude on its sign-in page.
        Restarting,
        /// Claude waits for a sign-in to the third account.
        Pending,
        /// On the way to Grace, Claude asks whether to stop its work in progress.
        Confirming,
        /// On the way to Grace, Claude waits for its work to finish before it quits.
        AfterWork,
    }

    /// Where an update stands in a scene.
    #[derive(Clone, Copy)]
    enum Update {
        None,
        Available,
        Downloading,
        Ready,
        Failed,
        Updated,
    }

    struct Scene {
        name: &'static str,
        theme: ThemeChoice,
        language: &'static str,
        overlay: Overlay,
        busy: Option<Busy>,
        update: Update,
        switching: Switching,
        /// Tall enough to show the whole settings sheet.
        tall: bool,
        build: fn(&Sample) -> Option<Overview>,
    }

    fn scene(name: &'static str, theme: ThemeChoice, build: fn(&Sample) -> Option<Overview>) -> Scene {
        Scene {
            name,
            theme,
            language: "en",
            overlay: Overlay::None,
            busy: None,
            update: Update::None,
            switching: Switching::Idle,
            tall: false,
            build,
        }
    }

    pub fn main() -> anyhow::Result<()> {
        let mut args = std::env::args().skip(1);
        let out = PathBuf::from(args.next().unwrap_or_else(|| "screenshots".into()));
        let only: Vec<String> = args.collect();
        fs::create_dir_all(&out)?;
        use ThemeChoice::{Dark, Light};
        let scenes = [
            scene("in-sync-light", Light, in_sync),
            scene("in-sync-dark", Dark, in_sync),
            scene("pending-light", Light, pending),
            scene("pending-dark", Dark, pending),
            Scene { busy: Some(Busy::Syncing), ..scene("syncing-light", Light, pending) },
            scene("waiting-light", Light, waiting),
            scene("two-orgs-light", Light, two_orgs),
            Scene { language: "zh-CN", ..scene("zh-CN-two-orgs-dark", Dark, two_orgs) },
            scene("restart-light", Light, restart),
            scene("restart-dark", Dark, restart),
            scene("linked-light", Light, linked),
            scene("excluded-light", Light, excluded),
            scene("single-dark", Dark, single),
            scene("single-light", Light, single),
            scene("loading-light", Light, |_| None),
            Scene { overlay: Overlay::Settings, ..scene("settings-light", Light, quit) },
            Scene { overlay: Overlay::Settings, ..scene("settings-dark", Dark, in_sync) },
            Scene { overlay: Overlay::ConfirmRestore, ..scene("confirm-dark", Dark, quit) },
            Scene { language: "zh-CN", ..scene("zh-CN-restart-light", Light, restart) },
            Scene { language: "zh-CN", overlay: Overlay::Settings, ..scene("zh-CN-settings-dark", Dark, in_sync) },
            Scene { language: "ja", ..scene("ja-pending-light", Light, pending) },
            Scene { language: "ko", ..scene("ko-in-sync-dark", Dark, in_sync) },
            Scene { language: "de", ..scene("de-restart-light", Light, restart) },
            Scene { language: "de", overlay: Overlay::Settings, ..scene("de-settings-light", Light, quit) },
            Scene { language: "fr", ..scene("fr-waiting-light", Light, waiting) },
            Scene { language: "es", ..scene("es-pending-dark", Dark, pending) },
            Scene { language: "pt-BR", ..scene("pt-BR-linked-light", Light, linked) },
            Scene { language: "ru", ..scene("ru-restart-light", Light, restart) },
            Scene { language: "zh-TW", overlay: Overlay::ConfirmRestore, ..scene("zh-TW-confirm-light", Light, quit) },
            Scene { update: Update::Available, ..scene("update-available-light", Light, in_sync) },
            Scene { update: Update::Downloading, ..scene("update-downloading-dark", Dark, in_sync) },
            Scene { update: Update::Ready, ..scene("update-ready-light", Light, in_sync) },
            Scene { update: Update::Failed, ..scene("update-failed-light", Light, in_sync) },
            Scene { update: Update::Updated, ..scene("update-done-dark", Dark, in_sync) },
            Scene { update: Update::Available, overlay: Overlay::Notes, ..scene("update-notes-light", Light, in_sync) },
            Scene { overlay: Overlay::SettingsToast, ..scene("toast-settings-dark", Dark, in_sync) },
            Scene {
                busy: Some(Busy::Switching),
                switching: Switching::ToGrace,
                ..scene("switch-to-grace-light", Light, in_sync)
            },
            Scene {
                busy: Some(Busy::Switching),
                switching: Switching::Restarting,
                ..scene("switch-restarting-dark", Dark, in_sync)
            },
            Scene { switching: Switching::Pending, ..scene("switch-pending-light", Light, in_sync) },
            Scene {
                busy: Some(Busy::Switching),
                switching: Switching::Confirming,
                ..scene("switch-confirming-light", Light, in_sync)
            },
            Scene {
                busy: Some(Busy::Switching),
                switching: Switching::AfterWork,
                ..scene("switch-after-work-dark", Dark, in_sync)
            },
            Scene {
                language: "zh-CN",
                busy: Some(Busy::Switching),
                switching: Switching::Confirming,
                ..scene("zh-CN-switch-confirming-dark", Dark, in_sync)
            },
            Scene { overlay: Overlay::SignIn, ..scene("switch-sign-in-light", Light, in_sync) },
            Scene { language: "zh-CN", overlay: Overlay::SignIn, ..scene("zh-CN-switch-sign-in-dark", Dark, in_sync) },
            Scene { overlay: Overlay::ConfirmSwitch, ..scene("switch-confirm-light", Light, in_sync) },
            Scene {
                language: "zh-CN",
                overlay: Overlay::ConfirmSwitch,
                ..scene("zh-CN-switch-confirm-dark", Dark, in_sync)
            },
            Scene { overlay: Overlay::ConfirmSwitchUnsaved, ..scene("switch-confirm-unsaved-light", Light, in_sync) },
            Scene { overlay: Overlay::ConfirmRemove, ..scene("remove-confirm-light", Light, in_sync) },
            Scene { overlay: Overlay::ConfirmNext, ..scene("next-confirm-dark", Dark, in_sync) },
            Scene { overlay: Overlay::Name(GRACE.0), ..scene("name-light", Light, in_sync) },
            Scene { language: "zh-CN", overlay: Overlay::Name(ADA.0), ..scene("zh-CN-name-dark", Dark, named) },
            Scene { overlay: Overlay::Name(ADA.0), ..scene("name-reset-light", Light, named) },
            Scene { overlay: Overlay::NameRefused, ..scene("name-refused-light", Light, in_sync) },
            scene("named-light", Light, named),
            Scene { overlay: Overlay::Hover(GRACE.0), ..scene("hover-light", Light, in_sync) },
            Scene {
                language: "zh-CN",
                overlay: Overlay::Hover(THIRD.0),
                ..scene("zh-CN-hover-named-dark", Dark, named)
            },
            Scene { overlay: Overlay::Settings, tall: true, ..scene("settings-tall-light", Light, in_sync) },
            Scene {
                language: "zh-CN",
                overlay: Overlay::Settings,
                tall: true,
                ..scene("zh-CN-settings-tall-dark", Dark, in_sync)
            },
            Scene { language: "zh-CN", ..scene("zh-CN-in-sync-light", Light, in_sync) },
            Scene {
                language: "zh-CN",
                switching: Switching::Pending,
                ..scene("zh-CN-switch-pending-light", Light, in_sync)
            },
            Scene {
                update: Update::Available,
                overlay: Overlay::Settings,
                tall: true,
                ..scene("update-settings-light", Light, in_sync)
            },
            Scene {
                language: "zh-CN",
                update: Update::Available,
                ..scene("zh-CN-update-available-light", Light, in_sync)
            },
            Scene { language: "zh-CN", update: Update::Updated, ..scene("zh-CN-update-done-light", Light, in_sync) },
            Scene {
                language: "zh-CN",
                update: Update::Downloading,
                overlay: Overlay::Settings,
                tall: true,
                ..scene("zh-CN-update-settings-dark", Dark, in_sync)
            },
        ];

        let text_system = gpui_kit::platform::current_platform(true).text_system();
        let mut cx = HeadlessAppContext::with_platform(text_system, Arc::new(cc_same_app::assets::Assets), || {
            gpui_kit::platform::current_headless_renderer()
        });
        cx.update(|cx| {
            gpui_kit::init(cx);
            theme::install(cx);
            // Stills: every transition shows its end state.
            cx.set_reduce_motion(true);
        });

        if only.iter().any(|o| o == "live") {
            let ctx = cc_same_app::context();
            let overview = Some(report::overview(&ctx));
            let snapshots = cc_same_core::snapshot::list(&ctx);
            i18n::apply(&ctx.config().language);
            let (update, switching, overlay) = (Update::None, Switching::Idle, Overlay::None);
            let shot = Shot { busy: None, update, switching, overlay, height: HEIGHT };
            capture(&mut cx, &out.join("live.png"), ctx, overview, snapshots, shot)?;
            return Ok(());
        }
        for scene in scenes.iter().filter(|s| only.is_empty() || only.iter().any(|o| s.name.contains(o.as_str()))) {
            let sample = Sample::new()?;
            let mut config = sample.ctx.config();
            config.appearance = scene.theme.config_value().into();
            sample.ctx.set_config(config);
            i18n::apply(scene.language);
            let overview = (scene.build)(&sample);
            let path = out.join(format!("{}.png", scene.name));
            let height = if scene.tall { 1560. } else { HEIGHT };
            let (update, switching, overlay) = (scene.update, scene.switching, scene.overlay);
            let shot = Shot { busy: scene.busy, update, switching, overlay, height };
            capture(&mut cx, &path, sample.ctx.clone(), overview, sample_snapshots(), shot)?;
        }
        Ok(())
    }

    /// How a scene is shown: what is running, what is open on top, and how tall the window is.
    struct Shot {
        busy: Option<Busy>,
        update: Update,
        switching: Switching,
        overlay: Overlay,
        height: f32,
    }

    fn capture(
        cx: &mut HeadlessAppContext,
        path: &PathBuf,
        ctx: Arc<Ctx>,
        overview: Option<Overview>,
        snapshots: Vec<Manifest>,
        shot: Shot,
    ) -> anyhow::Result<()> {
        let Shot { busy, update, switching, overlay, height } = shot;
        let store = cx.update(|cx| Store::preview(ctx, overview, snapshots, busy, cx));
        cx.update(|cx| {
            store.update(cx, |store, _| {
                store.updates = sample_updates(update);
                match switching {
                    Switching::Idle | Switching::Restarting => {}
                    Switching::ToGrace => store.switching_to = Some(GRACE.0.into()),
                    Switching::Confirming | Switching::AfterWork => {
                        store.switching_to = Some(GRACE.0.into());
                        let quitting = match switching {
                            Switching::Confirming => Quitting::Confirming,
                            _ => Quitting::AfterWork,
                        };
                        store.waiting = Some(Arc::new(Waiting::new(quitting)));
                    }
                    Switching::Pending => {
                        let expected = None;
                        store.signing_in = Some(SigningIn { previous: Some(ADA.0.into()), expected });
                    }
                }
            })
        });
        let mut view: Option<Entity<UniApp>> = None;
        let window = cx.open_window(size(px(WIDTH), px(height)), |window, cx| {
            let app = cx.new(|cx| UniApp::still(store, cx));
            view = Some(app.clone());
            cx.new(|cx| Root::new(app, window, cx))
        })?;
        let view = view.expect("the window built its view");
        settle(cx, window.into())?;
        cx.update_window(window.into(), |_, window, cx| {
            view.update(cx, |this, cx| match overlay {
                Overlay::None => {}
                Overlay::Settings => this.open_settings(window, cx),
                Overlay::ConfirmRestore => {
                    let snapshot = sample_snapshots().remove(0);
                    this.confirm_restore(&snapshot, window, cx);
                }
                Overlay::SignIn => this.confirm_sign_in(Some("grace@hopper.work".into()), window, cx),
                Overlay::ConfirmSwitch => this.confirm_switch(GRACE.0.into(), window, cx),
                Overlay::ConfirmSwitchUnsaved => this.confirm_switch(THIRD.0.into(), window, cx),
                Overlay::ConfirmRemove => this.confirm_remove(GRACE.0.into(), "grace@hopper.work".into(), window, cx),
                Overlay::ConfirmNext => this.confirm_next(window, cx),
                Overlay::Name(account) => this.open_name(account.into(), window, cx),
                Overlay::NameRefused => this.open_name(GRACE.0.into(), window, cx),
                Overlay::Hover(account) => this.set_hovered(Some(account.into()), cx),
                Overlay::SettingsToast => {
                    this.open_settings(window, cx);
                    let message = i18n::tf("update.up_to_date", &[("version", &update::VERSION)]);
                    cc_same_app::view::show_outcome(&Ok(message), window, cx);
                }
                Overlay::Notes => {
                    let release = sample_release();
                    let title = i18n::tf("update.notes_title", &[("version", &release.version)]);
                    cc_same_app::view::open_notes(title, release.notes, window, cx);
                }
            })
        })?;
        settle(cx, window.into())?;
        if let Overlay::NameRefused = overlay {
            // One key at a time, as typing does, so each edit is seen before the next key.
            for key in ["m", "y", "space", "w", "o", "r", "k", "enter"] {
                cx.update_window(window.into(), |_, window, cx| {
                    window.dispatch_keystroke(gpui_kit::Keystroke::parse(key).expect("a key"), cx);
                })?;
                cx.run_until_parked();
            }
            settle(cx, window.into())?;
        }
        cx.capture_screenshot(window.into())?.save(path)?;
        println!("{}", path.display());
        cx.update_window(window.into(), |_, window, _| window.remove_window())?;
        Ok(())
    }

    /// Let layout, fonts and icons settle before taking the picture.
    fn settle(cx: &mut HeadlessAppContext, window: gpui_kit::AnyWindowHandle) -> anyhow::Result<()> {
        cx.allow_parking();
        for _ in 0..6 {
            cx.run_until_parked();
            cx.advance_clock(Duration::from_millis(250));
            std::thread::sleep(Duration::from_millis(40));
            cx.update_window(window, |_, window, _| window.refresh())?;
        }
        cx.run_until_parked();
        Ok(())
    }

    /// A temporary Claude data folder with three signed-in accounts.
    struct Sample {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        ctx: Arc<Ctx>,
    }

    impl Sample {
        fn new() -> anyhow::Result<Sample> {
            let tmp = tempfile::tempdir()?;
            let root = tmp.path().to_path_buf();
            let mut paths = Paths::new(root.join("Claude"), root.join("state"));
            paths.desktop_logs = root.join("logs");
            paths.projects = vec![root.join("projects")];
            paths.claude_settings = root.join("settings.json");
            paths.claude_json = root.join("claude.json");
            fs::create_dir_all(&paths.user_data)?;
            fs::create_dir_all(&paths.desktop_logs)?;
            fs::create_dir_all(root.join("projects").join("-Users-ada-code-app"))?;
            fs::write(&paths.claude_settings, json!({"cleanupPeriodDays": 3650}).to_string())?;
            fs::write(
                &paths.claude_json,
                json!({"oauthAccount": {"accountUuid": ADA.0, "emailAddress": "ada@lovelace.dev"}}).to_string(),
            )?;
            // Grace's email comes from a Cowork record, like on a real machine.
            let cowork = paths.cowork_sessions().join(GRACE.0).join(GRACE.1);
            fs::create_dir_all(&cowork)?;
            let cw = "7b0c1d2e-3f4a-4b5c-8d6e-7f8091a2b3c4";
            fs::write(
                cowork.join(format!("local_{cw}.json")),
                json!({"sessionId": format!("local_{cw}"), "emailAddress": "grace@hopper.work"}).to_string(),
            )?;
            let ctx = Arc::new(Ctx::new(
                paths,
                Config::default(),
                FakeDesktop { running: Some(true), active: Some(vec![key(ADA)]) },
                LogSink::Silent,
            ));
            Ok(Sample { _tmp: tmp, root, ctx })
        }

        fn part(&self, p: (&str, &str)) -> PathBuf {
            let d = self.ctx.paths.user_data.join(Surface::Code.dir_name()).join(p.0).join(p.1);
            fs::create_dir_all(&d).unwrap();
            d
        }

        fn sessions(&self, p: (&str, &str), count: usize, mtime_ms: u64) {
            self.sessions_from(p, 0, count, mtime_ms)
        }

        /// Sessions numbered `from..from + count`, so two lists can hold different ones.
        fn sessions_from(&self, p: (&str, &str), from: usize, count: usize, mtime_ms: u64) {
            let dir = self.part(p);
            for i in from..from + count {
                let u = format!("{:08x}-1111-4222-8333-{:012x}", i, i);
                let cli = format!("cli-{i:04}");
                fs::write(self.root.join("projects").join("-Users-ada-code-app").join(format!("{cli}.jsonl")), "{}\n")
                    .unwrap();
                let path = dir.join(format!("local_{u}.json"));
                let body = json!({
                    "sessionId": format!("local_{u}"), "cliSessionId": cli, "title": format!("Session {i}"),
                    "cwd": "/Users/ada/code/app", "isArchived": i % 9 == 0, "lastActivityAt": mtime_ms,
                });
                fs::write(&path, body.to_string()).unwrap();
                let f = fs::OpenOptions::new().write(true).open(&path).unwrap();
                f.set_modified(fsx::from_ns(mtime_ms as i128 * 1_000_000)).unwrap();
            }
        }

        /// Bring every account in step, as a real first sync would (Claude not running).
        fn settle(&self) {
            let quiet = Ctx::new(
                self.ctx.paths.clone(),
                self.ctx.config(),
                FakeDesktop { running: Some(false), active: None },
                LogSink::Silent,
            );
            apply::run_sync(&quiet, "manual").unwrap();
        }

        fn overview(&self) -> Overview {
            let mut ov = report::overview(&self.ctx);
            let now = fsx::now_secs();
            // Ada is in use; Grace was set aside two days ago; the third account never was.
            ov.logins = Logins {
                supported: true,
                signed_in: Some(ADA.0.into()),
                saved: vec![Saved {
                    account: GRACE.0.into(),
                    email: Some("grace@hopper.work".into()),
                    set_aside_at: now - 2.0 * 86_400.0,
                }],
            };
            // Ada and Grace are in the list. Claude last read Grace's plan the day before yesterday,
            // near the end of her week.
            let slot = |number: u32, p: (&str, &str), email: &str| Slot {
                number,
                account: p.0.into(),
                email: Some(email.into()),
                orgs: vec![p.1.into()],
                ..Slot::default()
            };
            ov.roster = Roster {
                version: 1,
                slots: vec![slot(1, ADA, "ada@lovelace.dev"), slot(2, GRACE, "grace@hopper.work")],
            };
            ov.usage = [
                (ADA.0.to_string(), Usage { at: now - 120.0, five_hour: Some(12.0), weekly: Some(58.0) }),
                (GRACE.0.to_string(), Usage { at: now - 31.0 * 3600.0, five_hour: Some(40.0), weekly: Some(92.0) }),
            ]
            .into();
            ov.service =
                ServiceStatus { installed: true, running: Some(true), detail: "running".into(), ..Default::default() };
            ov.heartbeat = Some(Heartbeat {
                pid: 1,
                version: cc_same_core::VERSION.into(),
                exe: String::new(),
                started_at: now - 3600.0,
                heartbeat_at: now - 4.0,
            });
            ov.last_sync =
                Some(LastSync { at: now - 130.0, reason: "watch".into(), applied: 3, errors: 0, deferred: 0 });
            ov.desktop_version = Some("2.9939.2".into());
            ov
        }
    }

    fn key(p: (&str, &str)) -> String {
        format!("{}/{}", p.0, p.1)
    }

    fn in_sync(s: &Sample) -> Option<Overview> {
        for p in [ADA, GRACE, THIRD] {
            s.sessions(p, 212, 1_790_000_000_000);
        }
        s.settle();
        Some(s.overview())
    }

    fn pending(s: &Sample) -> Option<Overview> {
        s.sessions(ADA, 212, 1_790_000_000_000);
        s.sessions(GRACE, 212, 1_790_000_000_000);
        s.sessions(THIRD, 7, 1_790_000_000_000);
        let mut ov = s.overview();
        ov.service = ServiceStatus { detail: "not installed".into(), running: Some(false), ..Default::default() };
        ov.heartbeat = None;
        Some(ov)
    }

    /// Two accounts, each with a list in both organizations, the one in the other's organization
    /// empty: what signing in to one account after the other can leave behind.
    fn two_orgs(s: &Sample) -> Option<Overview> {
        s.sessions_from(ADA, 0, 95, 1_790_000_000_000);
        s.sessions_from(GRACE, 95, 106, 1_790_000_000_000);
        s.part((ADA.0, GRACE.1));
        s.part((GRACE.0, ADA.1));
        let mut ov = s.overview();
        ov.service = ServiceStatus { detail: "not installed".into(), running: Some(false), ..Default::default() };
        ov.heartbeat = None;
        Some(ov)
    }

    fn waiting(s: &Sample) -> Option<Overview> {
        for p in [ADA, GRACE, THIRD] {
            s.sessions(p, 212, 1_790_000_000_000);
        }
        s.settle();
        // Three sessions changed while Grace's account was open; Ada's account is open now.
        for i in 0..3 {
            let u = format!("{:08x}-1111-4222-8333-{:012x}", i, i);
            for p in [GRACE, THIRD] {
                let path = s.part(p).join(format!("local_{u}.json"));
                let body = json!({"sessionId": format!("local_{u}"), "cliSessionId": format!("cli-{i:04}"), "title": "Renamed", "cwd": "/Users/ada/code/app", "isArchived": i % 9 == 0, "lastActivityAt": 1_790_000_500_000u64});
                fs::write(&path, body.to_string()).unwrap();
                let f = fs::OpenOptions::new().write(true).open(&path).unwrap();
                f.set_modified(fsx::from_ns(1_790_000_500_000i128 * 1_000_000)).unwrap();
            }
        }
        Some(s.overview())
    }

    fn restart(s: &Sample) -> Option<Overview> {
        let mut ov = in_sync(s)?;
        ov.pending_restart = Some(PendingRestart {
            keys: vec![key(ADA)],
            sessions: 4,
            init_marker: ov.app.init_marker.clone(),
            at: fsx::now_secs(),
        });
        Some(ov)
    }

    fn linked(s: &Sample) -> Option<Overview> {
        s.sessions(ADA, 212, 1_790_000_000_000);
        s.sessions(GRACE, 212, 1_790_000_000_000);
        s.settle();
        let parent = s.ctx.paths.user_data.join(Surface::Code.dir_name()).join(THIRD.0);
        fs::create_dir_all(&parent).unwrap();
        std::os::unix::fs::symlink(s.part(ADA), parent.join(THIRD.1)).unwrap();
        Some(s.overview())
    }

    /// Ada goes by "work", and her week renews in 3 days and 4 hours; the third account joined the
    /// list without an email CC Same could find, and goes by "studio".
    fn named(s: &Sample) -> Option<Overview> {
        let mut ov = in_sync(s)?;
        if let Some(ada) = ov.roster.slots.iter_mut().find(|slot| slot.account == ADA.0) {
            ada.alias = Some("work".into());
            ada.weekly_reset = Some(fsx::now_secs() as i64 + 3 * 86_400 + 4 * 3600 + 1800);
        }
        ov.roster.slots.push(Slot {
            number: 3,
            account: THIRD.0.into(),
            alias: Some("studio".into()),
            orgs: vec![THIRD.1.into()],
            ..Slot::default()
        });
        Some(ov)
    }

    fn excluded(s: &Sample) -> Option<Overview> {
        let mut cfg = s.ctx.config();
        cfg.exclude.push(THIRD.0.into());
        s.ctx.set_config(cfg);
        for p in [ADA, GRACE] {
            s.sessions(p, 212, 1_790_000_000_000);
        }
        s.sessions(THIRD, 40, 1_790_000_000_000);
        s.settle();
        Some(s.overview())
    }

    fn single(s: &Sample) -> Option<Overview> {
        s.sessions(ADA, 212, 1_790_000_000_000);
        Some(s.overview())
    }

    fn quit(s: &Sample) -> Option<Overview> {
        let mut ov = in_sync(s)?;
        ov.app.running = false;
        Some(ov)
    }

    fn sample_snapshots() -> Vec<Manifest> {
        let parts = |n: usize| -> Vec<SnapshotPart> {
            [ADA, GRACE, THIRD]
                .iter()
                .take(n)
                .map(|p| SnapshotPart { surface: "code".into(), acct: p.0.into(), org: p.1.into() })
                .collect()
        };
        let snap = |id: &str, reason: &str, n: usize| Manifest {
            id: id.into(),
            reason: reason.into(),
            created: 0.0,
            version: String::new(),
            partitions: parts(n),
        };
        vec![
            snap("20260929-091502-baseline", "baseline", 2),
            snap("20260929-141208-watch", "watch", 3),
            snap("20260929-171933-watch", "watch", 3),
            snap("20260929-190455-app", "app", 3),
        ]
    }

    /// A release after this one, with this version's notes.
    fn sample_release() -> Release {
        let notes = update::changes_since(Some(&semver::Version::new(0, 1, 0)));
        let notes = notes.lines().skip(1).collect::<Vec<_>>().join("\n").trim().to_string();
        Release {
            version: semver::Version::new(0, 2, 0),
            notes,
            page: "https://github.com/songkeys/cc-same/releases/tag/v0.2.0".into(),
            asset: Some(Asset {
                name: "CC-Same-0.2.0-macos-arm64.zip".into(),
                url: "https://github.com/songkeys/cc-same/releases/download/v0.2.0/CC-Same-0.2.0-macos-arm64.zip"
                    .into(),
                size: 1000,
                sha256: None,
            }),
        }
    }

    fn sample_updates(update: Update) -> Updates {
        let mut updates = Updates::default();
        updates.checked_at = Some(fsx::now_secs() - 300.);
        match update {
            Update::None => {}
            Update::Updated => updates.updated_from = Some("0.1.0".into()),
            _ => updates.available = Some(sample_release()),
        }
        updates.phase = match update {
            Update::Downloading => UpdatePhase::Downloading(Arc::new(AtomicU64::new(420))),
            Update::Ready => UpdatePhase::Ready(PathBuf::from("/tmp/CC Same.app")),
            _ => UpdatePhase::Idle,
        };
        if let Update::Failed = update {
            updates.problem = Some((Problem::ReadOnly, "Permission denied (os error 13)".into()));
        }
        updates
    }
}
