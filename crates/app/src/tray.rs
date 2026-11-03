//! The menu bar (macOS) and notification area (Windows, Linux) icon: the headline at a glance,
//! the accounts to switch between, Sync now, the background switch, and the way back to the
//! window.

use crate::i18n::{t, tf};
use crate::model::Busy;
use crate::store::{Store, UpdatePhase};
use gpui_kit::App;
use std::rc::Rc;

/// What a click in the tray asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Open,
    /// Open the panel under the icon, or close it (a left click on macOS).
    Panel(Option<crate::panel::Anchor>),
    SyncNow,
    ToggleBackground,
    Settings,
    /// Install the new version, or look for one.
    Update,
    /// Switch Claude to this account.
    SwitchTo(String),
    /// Switch Claude to the next account in the list.
    SwitchNext,
    /// Restart Claude on its sign-in page, to sign in to another account.
    SignInAnother,
    Quit,
}

/// An account in the tray's menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayAccount {
    pub id: String,
    /// Its number and name, and the fuller window of its plan: `1  work · 7d 58%`.
    pub label: String,
    /// Claude is signed in to it.
    pub current: bool,
}

pub type OnCommand = Rc<dyn Fn(Command, &mut App)>;

/// Everything the tray shows, in the interface language.
#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub title: String,
    pub detail: String,
    pub can_sync: bool,
    pub background: bool,
    pub background_enabled: bool,
    pub attention: bool,
    pub sync_now: String,
    pub background_label: String,
    pub open: String,
    pub settings: String,
    /// "Check for Updates…", or "Update to 0.2.0…" once there is one.
    pub update: String,
    pub update_enabled: bool,
    /// Switching accounts is available on this system.
    pub can_switch: bool,
    /// Every account CC Same knows: the list by number, then the others.
    pub accounts: Vec<TrayAccount>,
    /// "Next Account", and whether there is one.
    pub next_label: String,
    pub has_next: bool,
    /// "Add Account…": Claude restarts on its sign-in page.
    pub sign_in_label: String,
    pub quit: String,
}

impl Status {
    pub fn of(store: &Store) -> Status {
        let mood = store.mood();
        let idle = store.busy.is_none();
        let (update, update_enabled) = match (&store.updates.phase, &store.updates.available) {
            (UpdatePhase::Checking, _) => (t("app.checking_updates"), false),
            (UpdatePhase::Downloading(_) | UpdatePhase::Installing, _) => (t("app.updating"), false),
            (_, Some(release)) => (tf("app.update_to", &[("version", &release.version)]), true),
            (_, None) => (t("app.check_updates"), true),
        };
        let logins = store.overview.as_ref().map(|o| o.logins.clone()).unwrap_or_default();
        // Every account, not only the ones with a sign-in kept: choosing one without asks to sign in.
        let rows = store.accounts();
        let saved = logins.saved.iter().map(|s| (s.account.clone(), store.account_name(&s.account)));
        let accounts = tray_accounts(&rows, logins.signed_in.as_deref(), saved);
        let next = store.next_account();
        Status {
            title: mood.title(),
            detail: mood.detail(),
            can_sync: idle && store.overview.is_some(),
            background: store.background_on(),
            background_enabled: idle && store.busy != Some(Busy::Service) && store.overview.is_some(),
            attention: mood.needs_attention(),
            sync_now: t("action.sync_now"),
            background_label: t("background.title"),
            open: t("app.open"),
            settings: t("app.settings"),
            update,
            update_enabled,
            // Decided by the system, not by the overview: the menu is built at launch, before the
            // first overview arrives.
            can_switch: cc_same_core::logins::supported(),
            accounts,
            // Which account is next shows in the list above it; naming it here made the menu wide.
            next_label: t("tray.next_none"),
            has_next: next.is_some(),
            sign_in_label: t("tray.sign_in_another"),
            quit: t("app.quit"),
        }
    }

    fn tooltip(&self) -> String {
        format!("{} — {}", crate::APP_NAME, self.title)
    }
}

/// The longest name the menu shows; a longer one ends in "…", so the menu stays narrow.
const NAME_MAX: usize = 22;

fn clipped(name: &str) -> String {
    match name.chars().count() > NAME_MAX {
        true => format!("{}…", name.chars().take(NAME_MAX - 1).collect::<String>()),
        false => name.to_string(),
    }
}

/// The accounts in the menu: the window's rows (the list by number, then the others), and any
/// account with a sign-in kept that has no row, each once.
fn tray_accounts(
    rows: &[crate::model::Account],
    signed_in: Option<&str>,
    saved: impl IntoIterator<Item = (String, String)>,
) -> Vec<TrayAccount> {
    let mut list: Vec<TrayAccount> = rows
        .iter()
        .map(|a| {
            let mut label = match a.number {
                Some(n) => format!("{n}  {}", clipped(&a.name)),
                None => clipped(&a.name),
            };
            if let Some(usage) = a.usage {
                label = format!("{label} · {}", usage.short());
            }
            TrayAccount { id: a.id.clone(), label, current: signed_in == Some(a.id.as_str()) }
        })
        .collect();
    for (id, name) in saved {
        if !list.iter().any(|a| a.id == id) {
            let current = signed_in == Some(id.as_str());
            list.push(TrayAccount { id, label: clipped(&name), current });
        }
    }
    list
}

/// The icon; dropping it removes it.
pub struct Tray(platform::Tray);

impl Tray {
    pub fn install(status: &Status, on_command: OnCommand, cx: &mut App) -> anyhow::Result<Tray> {
        platform::Tray::new(status, on_command, cx).map(Tray)
    }

    pub fn update(&self, status: &Status) {
        self.0.update(status);
    }

    /// Where the panel hangs from under the icon, when the system says where the icon is.
    pub fn anchor(&self) -> Option<crate::panel::Anchor> {
        self.0.anchor()
    }
}

fn decode(png: &[u8]) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let image = image::load_from_memory(png)?.into_rgba8();
    let (width, height) = image.dimensions();
    Ok((image.into_raw(), width, height))
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod platform {
    use super::{Command, OnCommand, Status, decode};
    use anyhow::Context as _;
    use gpui_kit::{App, AsyncApp, Task};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;
    use tray_icon::menu::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    // macOS draws a template image in the menu bar's own color; Windows wants the colored icon.
    #[cfg(target_os = "macos")]
    const NORMAL: &[u8] = include_bytes!("../resources/tray/template.png");
    #[cfg(target_os = "macos")]
    const ATTENTION: &[u8] = include_bytes!("../resources/tray/template-attention.png");
    #[cfg(target_os = "windows")]
    const NORMAL: &[u8] = include_bytes!("../resources/tray/color.png");
    #[cfg(target_os = "windows")]
    const ATTENTION: &[u8] = include_bytes!("../resources/tray/color.png");

    fn icon(attention: bool) -> anyhow::Result<Icon> {
        let (rgba, width, height) = decode(if attention { ATTENTION } else { NORMAL })?;
        Icon::from_rgba(rgba, width, height).context("tray icon")
    }

    fn anchor_of(rect: tray_icon::Rect) -> Option<crate::panel::Anchor> {
        let (at, size) = (rect.position, rect.size);
        crate::panel::below(at.x, at.y, size.width.into(), size.height.into())
    }

    pub struct Tray {
        icon: TrayIcon,
        title: MenuItem,
        detail: MenuItem,
        sync: MenuItem,
        background: CheckMenuItem,
        open: MenuItem,
        settings: MenuItem,
        update: MenuItem,
        quit: MenuItem,
        menu: Menu,
        /// How many items the accounts take in the menu, after the headline; none when switching
        /// is not available.
        account_items: std::cell::Cell<usize>,
        /// What each of those items does; they change with the accounts.
        account_commands: Rc<RefCell<Vec<(MenuId, Command)>>>,
        last: RefCell<Status>,
        _events: Task<()>,
    }

    /// Where the accounts start in the menu: after the headline, its detail and a separator.
    const ACCOUNTS_AT: usize = 3;

    /// Put the accounts in the menu in place of the `previous` items there: every account (the one
    /// in use checked), the next account, signing in to another, and a separator. Returns how
    /// many items they take.
    fn fill_accounts(
        menu: &Menu,
        previous: usize,
        status: &Status,
        commands: &RefCell<Vec<(MenuId, Command)>>,
    ) -> usize {
        for _ in 0..previous {
            menu.remove_at(ACCOUNTS_AT);
        }
        let mut found = Vec::new();
        let mut items: Vec<Box<dyn IsMenuItem>> = Vec::new();
        for account in &status.accounts {
            let item = CheckMenuItem::new(&account.label, !account.current && status.can_sync, account.current, None);
            if !account.current {
                found.push((item.id().clone(), Command::SwitchTo(account.id.clone())));
            }
            items.push(Box::new(item));
        }
        let next = MenuItem::new(&status.next_label, status.has_next && status.can_sync, None);
        found.push((next.id().clone(), Command::SwitchNext));
        let sign_in = MenuItem::new(&status.sign_in_label, status.can_sync, None);
        found.push((sign_in.id().clone(), Command::SignInAnother));
        items.extend([
            Box::new(next) as Box<dyn IsMenuItem>,
            Box::new(sign_in),
            Box::new(PredefinedMenuItem::separator()),
        ]);
        for (i, item) in items.iter().enumerate() {
            let _ = menu.insert(item.as_ref(), ACCOUNTS_AT + i);
        }
        *commands.borrow_mut() = found;
        items.len()
    }

    impl Tray {
        pub fn new(status: &Status, on_command: OnCommand, cx: &mut App) -> anyhow::Result<Tray> {
            let title = MenuItem::new(&status.title, false, None);
            let detail = MenuItem::new(&status.detail, false, None);
            let sync = MenuItem::new(&status.sync_now, status.can_sync, None);
            let background =
                CheckMenuItem::new(&status.background_label, status.background_enabled, status.background, None);
            let open = MenuItem::new(&status.open, true, None);
            let settings = MenuItem::new(&status.settings, true, None);
            let update = MenuItem::new(&status.update, status.update_enabled, None);
            let quit = MenuItem::new(&status.quit, true, None);
            let account_commands = Rc::new(RefCell::new(Vec::new()));
            let menu = Menu::new();
            menu.append_items(&[&title, &detail, &PredefinedMenuItem::separator(), &sync, &background])?;
            menu.append_items(&[
                &PredefinedMenuItem::separator(),
                &open,
                &settings,
                &update,
                &PredefinedMenuItem::separator(),
                &quit,
            ])?;
            let account_items = match status.can_switch {
                true => fill_accounts(&menu, 0, status, &account_commands),
                false => 0,
            };
            let icon = TrayIconBuilder::new()
                .with_tooltip(status.tooltip())
                .with_icon(icon(status.attention)?)
                .with_icon_as_template(cfg!(target_os = "macos"))
                .with_menu(Box::new(menu.clone()))
                // A click opens the app (on macOS, its panel under the icon); the menu is a right-click.
                .with_menu_on_left_click(false)
                .build()
                .context("creating the tray icon")?;

            let commands: Vec<(MenuId, Command)> = vec![
                (sync.id().clone(), Command::SyncNow),
                (background.id().clone(), Command::ToggleBackground),
                (open.id().clone(), Command::Open),
                (settings.id().clone(), Command::Settings),
                (update.id().clone(), Command::Update),
                (quit.id().clone(), Command::Quit),
            ];
            let tray_id = icon.id().clone();
            let dynamic = account_commands.clone();
            let events = cx.spawn(async move |cx: &mut AsyncApp| {
                loop {
                    let mut asked = Vec::new();
                    while let Ok(event) = MenuEvent::receiver().try_recv() {
                        let known = commands.iter().find(|(id, _)| *id == event.id).map(|(_, c)| c.clone());
                        let known = known.or_else(|| {
                            dynamic.borrow().iter().find(|(id, _)| *id == event.id).map(|(_, c)| c.clone())
                        });
                        asked.extend(known);
                    }
                    while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                        if let TrayIconEvent::Click {
                            id,
                            rect,
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        } = event
                            && id == tray_id
                        {
                            asked.push(match cfg!(target_os = "macos") {
                                true => Command::Panel(anchor_of(rect)),
                                false => Command::Open,
                            });
                        }
                    }
                    for command in asked {
                        let on_command = on_command.clone();
                        cx.update(|cx| on_command(command, cx));
                    }
                    cx.background_executor().timer(Duration::from_millis(120)).await;
                }
            });
            Ok(Tray {
                icon,
                title,
                detail,
                sync,
                background,
                open,
                settings,
                update,
                quit,
                menu,
                account_items: std::cell::Cell::new(account_items),
                account_commands,
                last: RefCell::new(status.clone()),
                _events: events,
            })
        }

        pub fn anchor(&self) -> Option<crate::panel::Anchor> {
            self.icon.rect().and_then(anchor_of)
        }

        pub fn update(&self, status: &Status) {
            let mut last = self.last.borrow_mut();
            if *last == *status {
                return;
            }
            self.title.set_text(&status.title);
            self.detail.set_text(&status.detail);
            self.sync.set_text(&status.sync_now);
            self.sync.set_enabled(status.can_sync);
            self.background.set_text(&status.background_label);
            self.background.set_enabled(status.background_enabled);
            self.background.set_checked(status.background);
            self.open.set_text(&status.open);
            self.settings.set_text(&status.settings);
            self.update.set_text(&status.update);
            self.update.set_enabled(status.update_enabled);
            self.quit.set_text(&status.quit);
            let changed = (&last.accounts, &last.next_label, last.has_next, &last.sign_in_label, last.can_sync)
                != (&status.accounts, &status.next_label, status.has_next, &status.sign_in_label, status.can_sync);
            if status.can_switch && changed {
                let count = fill_accounts(&self.menu, self.account_items.get(), status, &self.account_commands);
                self.account_items.set(count);
            }
            let _ = self.icon.set_tooltip(Some(status.tooltip()));
            if last.attention != status.attention
                && let Ok(image) = icon(status.attention)
            {
                let _ = self.icon.set_icon_with_as_template(Some(image), cfg!(target_os = "macos"));
            }
            *last = status.clone();
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{Command, OnCommand, Status, decode};
    use gpui_kit::{App, AsyncApp, Task};
    use ksni::blocking::{Handle, TrayMethods as _};
    use ksni::menu::{CheckmarkItem, StandardItem};
    use std::sync::mpsc::{self, Sender};
    use std::time::Duration;

    struct Item {
        status: Status,
        icon: Vec<ksni::Icon>,
        commands: Sender<Command>,
    }

    impl Item {
        fn send(&self, command: Command) {
            let _ = self.commands.send(command);
        }

        /// Every account (the one in use checked), the next account, signing in to another, and a
        /// separator.
        fn account_items(&self) -> Vec<ksni::MenuItem<Self>> {
            let s = &self.status;
            let mut items: Vec<ksni::MenuItem<Self>> = s
                .accounts
                .iter()
                .map(|account| -> ksni::MenuItem<Self> {
                    let id = account.id.clone();
                    CheckmarkItem {
                        label: account.label.clone(),
                        enabled: !account.current && s.can_sync,
                        checked: account.current,
                        activate: Box::new(move |this: &mut Self| this.send(Command::SwitchTo(id.clone()))),
                        ..Default::default()
                    }
                    .into()
                })
                .collect();
            items.push(
                StandardItem {
                    label: s.next_label.clone(),
                    enabled: s.has_next && s.can_sync,
                    activate: Box::new(|this: &mut Self| this.send(Command::SwitchNext)),
                    ..Default::default()
                }
                .into(),
            );
            items.push(
                StandardItem {
                    label: s.sign_in_label.clone(),
                    enabled: s.can_sync,
                    activate: Box::new(|this: &mut Self| this.send(Command::SignInAnother)),
                    ..Default::default()
                }
                .into(),
            );
            items.push(ksni::MenuItem::Separator);
            items
        }
    }

    impl ksni::Tray for Item {
        fn id(&self) -> String {
            crate::APP_ID.into()
        }

        fn title(&self) -> String {
            crate::APP_NAME.into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            self.icon.clone()
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip {
                title: self.status.tooltip(),
                description: self.status.detail.clone(),
                ..Default::default()
            }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            self.send(Command::Open);
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            let s = &self.status;
            let mut items: Vec<ksni::MenuItem<Self>> = vec![
                StandardItem { label: s.title.clone(), enabled: false, ..Default::default() }.into(),
                StandardItem { label: s.detail.clone(), enabled: false, ..Default::default() }.into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: s.sync_now.clone(),
                    enabled: s.can_sync,
                    activate: Box::new(|this: &mut Self| this.send(Command::SyncNow)),
                    ..Default::default()
                }
                .into(),
                CheckmarkItem {
                    label: s.background_label.clone(),
                    enabled: s.background_enabled,
                    checked: s.background,
                    activate: Box::new(|this: &mut Self| this.send(Command::ToggleBackground)),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: s.open.clone(),
                    activate: Box::new(|this: &mut Self| this.send(Command::Open)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: s.settings.clone(),
                    activate: Box::new(|this: &mut Self| this.send(Command::Settings)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: s.update.clone(),
                    enabled: s.update_enabled,
                    activate: Box::new(|this: &mut Self| this.send(Command::Update)),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: s.quit.clone(),
                    activate: Box::new(|this: &mut Self| this.send(Command::Quit)),
                    ..Default::default()
                }
                .into(),
            ];
            // After the headline: title, detail, separator.
            if s.can_switch {
                items.splice(3..3, self.account_items());
            }
            items
        }
    }

    pub struct Tray {
        handle: Handle<Item>,
        _events: Task<()>,
    }

    impl Tray {
        pub fn new(status: &Status, on_command: OnCommand, cx: &mut App) -> anyhow::Result<Tray> {
            let (rgba, width, height) = decode(include_bytes!("../resources/tray/color.png"))?;
            // StatusNotifierItem wants ARGB32 in network byte order.
            let data = rgba.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
            let icon = vec![ksni::Icon { width: width as i32, height: height as i32, data }];
            let (commands, received) = mpsc::channel();
            let handle = Item { status: status.clone(), icon, commands }.spawn()?;
            let events = cx.spawn(async move |cx: &mut AsyncApp| {
                loop {
                    while let Ok(command) = received.try_recv() {
                        let on_command = on_command.clone();
                        cx.update(|cx| on_command(command, cx));
                    }
                    cx.background_executor().timer(Duration::from_millis(120)).await;
                }
            });
            Ok(Tray { handle, _events: events })
        }

        pub fn anchor(&self) -> Option<crate::panel::Anchor> {
            None
        }

        pub fn update(&self, status: &Status) {
            let status = status.clone();
            self.handle.update(move |item| {
                if item.status != status {
                    item.status = status;
                }
            });
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            let _ = self.handle.shutdown();
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
mod platform {
    use super::{OnCommand, Status};
    use gpui_kit::App;

    pub struct Tray;

    impl Tray {
        pub fn new(_: &Status, _: OnCommand, _: &mut App) -> anyhow::Result<Tray> {
            anyhow::bail!("no system tray on this platform")
        }

        pub fn update(&self, _: &Status) {}

        pub fn anchor(&self) -> Option<crate::panel::Anchor> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, name: &str, number: Option<u32>) -> crate::model::Account {
        crate::model::Account {
            id: id.into(),
            name: name.into(),
            email: Some(name.into()),
            sessions: 0,
            missing: 0,
            open: false,
            excluded: false,
            broken: false,
            login: crate::model::Login::Unknown,
            number,
            alias: None,
            sits_out: false,
            usage: None,
            weekly_reset: None,
            has_lists: true,
        }
    }

    #[test]
    fn every_account_is_listed_once_by_number() {
        let _locale = crate::i18n::TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        let use_of = |weekly, age: f64| {
            let usage = cc_same_core::usage::Usage {
                at: cc_same_core::fsx::now_secs() - age,
                five_hour: Some(30.0),
                weekly: Some(weekly),
            };
            crate::model::PlanUse::of(&usage, cc_same_core::fsx::now_secs(), false, None)
        };
        let mut ada = row("a", "ada@example.com", Some(1));
        ada.usage = use_of(40.0, 2.5 * 86_400.0);
        let mut bob = row("b", "bob@example.com", Some(2));
        bob.usage = use_of(99.0, 60.0);
        let rows = [ada, bob, row("c", "Account 0c0c0c0c", None)];
        let saved = [("b".to_string(), "bob@example.com".to_string()), ("d".into(), "dan@example.com".into())];
        let list = tray_accounts(&rows, Some("b"), saved);
        let shown: Vec<_> = list.iter().map(|a| (a.label.as_str(), a.current)).collect();
        assert_eq!(
            shown,
            [
                ("1  ada@example.com · 7d 40%", false),
                ("2  bob@example.com · 7d 99%", true),
                ("Account 0c0c0c0c", false),
                ("dan@example.com", false),
            ]
        );
        // A long name ends in "…".
        let long = row("e", "a-very-long-name@somewhere.example", Some(3));
        assert_eq!(tray_accounts(&[long], None, [])[0].label, "3  a-very-long-name@some…");
        // Signed out: nobody is current.
        assert!(tray_accounts(&rows, None, []).iter().all(|a| !a.current));
    }

    /// The menu is built once, at launch, before the accounts have been read: switching must be
    /// offered from the start.
    #[test]
    fn switch_account_is_offered_before_the_accounts_load() {
        use cc_same_core::{Config, Ctx, FakeDesktop, LogSink, Paths};
        use std::sync::Arc;
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("Claude"), tmp.path().join("state"));
        let ctx = Arc::new(Ctx::new(paths, Config::default(), FakeDesktop::default(), LogSink::Silent));
        let mut cx = gpui_kit::HeadlessAppContext::new(Arc::new(gpui_kit::NoopTextSystem::new()));
        let status = cx.update(|cx| {
            let store = Store::preview(ctx, None, Vec::new(), None, cx);
            Status::of(store.read(cx))
        });
        assert_eq!(status.can_switch, cc_same_core::logins::supported(), "decided before the first overview");
        assert!(status.accounts.is_empty() && !status.can_sync);
    }
}
