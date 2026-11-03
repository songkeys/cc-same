//! The settings sheet: look and language, what syncs, which accounts take part, how long
//! transcripts last, snapshots to go back to, where the files are, and updates.

use crate::i18n::{self, t, tf, tn};
use crate::model;
use crate::store::{Store, UpdatePhase};
use crate::theme::ThemeChoice;
use crate::update;
use crate::view::{UniApp, open_notes};
use cc_same_core::config::Config;
use cc_same_core::report::Warning;
use cc_same_core::retention::Limit;
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::button::{Button, ButtonGroup};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    Anchor, AnyElement, App, Entity, FontWeight, IntoElement, ParentElement as _, SharedString, Styled as _, Window,
    div, prelude::FluentBuilder as _, px, rems,
};

pub fn open(store: Entity<Store>, window: &mut Window, cx: &mut App) {
    // A window of its own keeps its title bar clear and shows the list beside the sheet; the
    // panel, with no title bar and no room to spare, gives the sheet all of itself.
    let panel = crate::uses_panel(cx);
    gpui_kit::component::Theme::global_mut(cx).sheet.margin_top =
        if panel { px(0.) } else { gpui_kit::component::TITLE_BAR_HEIGHT };
    window.open_sheet(cx, move |sheet, _, cx| {
        let sheet = sheet.title(t("settings.title"));
        let sheet = if panel { sheet.size(gpui_kit::relative(1.)) } else { sheet.size(px(360.)) };
        sheet.child(body(&store, cx))
    });
}

fn body(store: &Entity<Store>, cx: &mut App) -> impl IntoElement {
    let this = store.read(cx);
    let cfg = this.config();
    let idle = this.busy.is_none();
    let running = this.overview.as_ref().is_some_and(|o| o.app.running);
    let retention = this.overview.as_ref().map(|o| o.retention.clone()).unwrap_or_default();
    let missing = this.overview.as_ref().and_then(|o| {
        o.warnings.iter().find_map(|w| match w {
            Warning::MissingTranscripts { missing, .. } => Some(*missing),
            _ => None,
        })
    });
    let baseline = this.overview.as_ref().and_then(|o| o.baseline.clone());
    let accounts = this.accounts();
    let current_theme = this.theme();
    let open_at_login = this.open_at_login;
    let mut snapshots = this.snapshots.clone();
    snapshots.reverse();
    let state_dir = this.ctx.paths.state_dir.clone();
    let log = this.ctx.paths.log_file.clone();
    let updates = this.updates.clone();
    let muted = cx.theme().muted_foreground;

    let mut general = vec![
        row(
            t("settings.theme"),
            None,
            ButtonGroup::new("theme")
                .small()
                .outline()
                .children(
                    ThemeChoice::ALL
                        .iter()
                        .map(|c| Button::new(c.config_value()).label(c.label()).selected(*c == current_theme)),
                )
                .on_click({
                    let store = store.clone();
                    move |picked: &Vec<usize>, _, cx| {
                        if let Some(choice) = picked.first().and_then(|i| ThemeChoice::ALL.get(*i)).copied() {
                            store.update(cx, |s, cx| {
                                s.update_config(cx, |c| c.appearance = choice.config_value().into())
                            });
                        }
                    }
                })
                .into_any_element(),
            cx,
        ),
        row(t("settings.language"), None, language_picker(store, &cfg.language), cx),
        row(
            t(if cfg!(target_os = "macos") { "settings.tray.mac" } else { "settings.tray.other" }),
            Some(t("settings.tray.detail")),
            toggle(store, "tray", cfg.tray, true, |c, on| c.tray = on),
            cx,
        ),
    ];
    if cfg!(target_os = "macos") {
        general.push(row(
            t("settings.dock"),
            Some(t("settings.dock.detail")),
            toggle(store, "dock", cfg.dock, true, |c, on| c.dock = on),
            cx,
        ));
    }
    general.push(row(
        t("settings.login"),
        Some(t(if cfg!(target_os = "macos") { "settings.login.detail.mac" } else { "settings.login.detail.other" })),
        Switch::new("open-at-login")
            .small()
            .checked(open_at_login)
            .on_change({
                let store = store.clone();
                move |on: &bool, _, cx| {
                    let on = *on;
                    store.update(cx, |s, cx| s.set_open_at_login(on, cx));
                }
            })
            .into_any_element(),
        cx,
    ));

    let sync = vec![
        row(
            t("settings.notifications"),
            Some(t("settings.notifications.detail")),
            toggle(store, "notify", cfg.notify, idle, |c, on| c.notify = on),
            cx,
        ),
        row(
            t("settings.auto_join"),
            Some(t("settings.auto_join.detail")),
            toggle(store, "auto-join", cfg.auto_join_new, idle, |c, on| c.auto_join_new = on),
            cx,
        ),
    ];

    let people: Vec<AnyElement> = if accounts.is_empty() {
        vec![note(t("settings.no_accounts"), cx)]
    } else {
        accounts
            .iter()
            .map(|a| {
                let id = a.id.clone();
                let included = !a.excluded;
                h_flex()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .child(Avatar::new().name(a.initials()).xsmall())
                    .child(div().flex_1().min_w_0().truncate().child(a.name.clone()))
                    .child(
                        Switch::new(SharedString::from(format!("include-{}", a.id)))
                            .small()
                            .checked(included)
                            .disabled(!idle)
                            .tooltip(if included { t("settings.included") } else { t("account.excluded") })
                            .on_change({
                                let store = store.clone();
                                move |on: &bool, _, cx| {
                                    let (id, on) = (id.clone(), *on);
                                    store.update(cx, |s, cx| {
                                        s.update_config(cx, move |c| {
                                            c.exclude.retain(|e| e != &id && !e.starts_with(&format!("{id}/")));
                                            if !on {
                                                c.exclude.push(id);
                                            }
                                        })
                                    });
                                }
                            }),
                    )
                    .into_any_element()
            })
            .collect()
    };

    // How long the transcripts behind Desktop sessions stay, a way to keep them when they would
    // go, and a way back from anything CC Same changed.
    let (title, mut detail) = match retention.desktop_days {
        None => (t("settings.kept_unlimited"), t("settings.kept.desktop")),
        Some(days) => (
            tf("settings.kept_for", &[("period", &model::retention_label(Some(days)))]),
            t(if days >= 365.0 { "settings.kept.long" } else { "settings.kept.short" }),
        ),
    };
    let control = if retention.undo.is_some() {
        detail = format!("{detail} {}", t("settings.changed"));
        Button::new("undo-retention")
            .small()
            .outline()
            .label(t("settings.undo"))
            .disabled(!idle)
            .on_click({
                let store = store.clone();
                move |_, _, cx| store.update(cx, |s, cx| s.undo_retention(cx))
            })
            .into_any_element()
    } else if retention.is_short() && retention.limited_by != Some(Limit::Organization) {
        Button::new("keep-transcripts")
            .small()
            .outline()
            .label(match retention.limited_by {
                Some(Limit::OlderClaudeCode) => t("notice.keep_10_years"),
                _ => t("notice.keep_them"),
            })
            .disabled(!idle)
            .on_click({
                let store = store.clone();
                move |_, _, cx| store.update(cx, |s, cx| s.keep_transcripts(cx))
            })
            .into_any_element()
    } else if retention.is_short() {
        detail = format!("{detail} {}", t("notice.retention_org"));
        div().into_any_element()
    } else {
        Icon::new(IconName::CircleCheck).small().text_color(cx.theme().success).into_any_element()
    };
    let mut transcripts = vec![row(title, Some(detail), control, cx)];
    if let Some(n) = missing.filter(|n| *n > 0) {
        transcripts.push(note(tn("settings.missing", n, &[]), cx));
    }

    let mut restore: Vec<AnyElement> = Vec::new();
    if running && !snapshots.is_empty() {
        restore.push(note(t("settings.quit_to_restore"), cx));
    }
    for snap in &snapshots {
        let partitions = snap.partitions.iter().filter(|p| p.surface == "code").count();
        let why = model::snapshot_reason(&snap.reason, baseline.as_deref() == Some(snap.id.as_str()));
        let picked = snap.clone();
        restore.push(row(
            model::snapshot_when(&snap.id),
            Some(format!("{why} · {}", tn("snapshot.accounts", partitions, &[]))),
            Button::new(SharedString::from(format!("restore-{}", snap.id)))
                .small()
                .outline()
                .label(t("settings.restore"))
                .disabled(running || !idle)
                .on_click(move |_, window, cx| {
                    let picked = picked.clone();
                    if let Some(view) = main_view(window, cx) {
                        view.update(cx, |this, cx| this.confirm_restore(&picked, window, cx));
                    }
                })
                .into_any_element(),
            cx,
        ));
    }
    if snapshots.is_empty() {
        restore.push(note(t("settings.no_snapshots"), cx));
    }

    let files = vec![
        row(
            t("settings.state"),
            Some(t("settings.state.detail")),
            Button::new("reveal-state")
                .small()
                .outline()
                .icon(IconName::FolderOpen)
                .label(t("settings.show"))
                .on_click(move |_, _, cx| cx.reveal_path(&state_dir))
                .into_any_element(),
            cx,
        ),
        row(
            t("settings.log"),
            Some(t("settings.log.detail")),
            Button::new("open-log")
                .small()
                .outline()
                .icon(IconName::FileText)
                .label(t("settings.open"))
                .disabled(!log.exists())
                .on_click(move |_, _, cx| cx.open_with_system(&log))
                .into_any_element(),
            cx,
        ),
    ];

    // This version and the latest one, and whether to look for new ones and install them.
    let status = match (&updates.phase, &updates.available, &updates.problem) {
        (UpdatePhase::Checking, _, _) => t("update.status.checking"),
        (UpdatePhase::Downloading(_), Some(release), _) => {
            let percent = (updates.progress().unwrap_or(0.) * 100.).round() as u32;
            tf("update.status.downloading", &[("version", &release.version), ("percent", &percent)])
        }
        (UpdatePhase::Ready(_), Some(release), _) => tf("update.status.ready", &[("version", &release.version)]),
        (UpdatePhase::Installing, Some(release), _) => tf("update.installing", &[("version", &release.version)]),
        (_, _, Some((problem, why))) => format!("{} ({why})", t(problem.message_key())),
        (_, Some(release), None) => tf("update.status.available", &[("version", &release.version)]),
        (_, None, None) => match updates.checked_at {
            Some(at) => tf("update.status.latest", &[("ago", &i18n::ago(at))]),
            None => t("update.status.unchecked"),
        },
    };
    let action = if updates.working() {
        Spinner::new().small().into_any_element()
    } else {
        let label = match (&updates.phase, &updates.available) {
            (UpdatePhase::Ready(_), _) => t("update.restart"),
            (_, Some(_)) => t("update.install"),
            (_, None) => t("settings.check_now"),
        };
        Button::new("update-or-check")
            .small()
            .outline()
            .label(label)
            .disabled(!idle && updates.available.is_some())
            .on_click({
                let store = store.clone();
                move |_, _, cx| store.update(cx, |s, cx| s.update_or_check(cx))
            })
            .into_any_element()
    };
    let changelog = Button::new("changelog")
        .small()
        .outline()
        .label(t("settings.show"))
        .on_click(|_, window, cx| open_notes(t("update.changelog_title"), update::changes_since(None), window, cx));
    let updating = vec![
        row(tf("settings.version", &[("version", &update::VERSION)]), Some(status), action, cx),
        row(
            t("settings.check_updates"),
            Some(t("settings.check_updates.detail")),
            toggle(store, "check-updates", cfg.check_updates, true, |c, on| c.check_updates = on),
            cx,
        ),
        row(
            t("settings.auto_update"),
            Some(t("settings.auto_update.detail")),
            toggle(store, "auto-update", cfg.auto_update && cfg.check_updates, cfg.check_updates, |c, on| {
                c.auto_update = on
            }),
            cx,
        ),
        row(t("settings.whats_new"), Some(t("settings.whats_new.detail")), changelog.into_any_element(), cx),
    ];

    v_flex()
        .gap_6()
        .pb_6()
        .child(group(t("settings.general"), general, cx))
        .child(group(t("settings.sync"), sync, cx))
        .child(group(t("accounts.title"), people, cx))
        .child(group(t("settings.transcripts"), transcripts, cx))
        .child(group(t("settings.snapshots"), restore, cx))
        .child(group(t("settings.files"), files, cx))
        .child(group(t("settings.updates"), updating, cx))
        .child(
            div()
                .text_center()
                .text_size(rems(0.846))
                .text_color(muted)
                .child(tf("settings.footer", &[("version", &cc_same_core::VERSION)])),
        )
}

/// The window's own view, for actions that open a dialog in it.
fn main_view(window: &mut Window, cx: &mut App) -> Option<Entity<UniApp>> {
    window
        .root::<gpui_kit::component::Root>()
        .flatten()
        .and_then(|root| root.read(cx).view().clone().downcast::<UniApp>().ok())
}

/// The language button: the current choice, and a menu of every language in its own name.
fn language_picker(store: &Entity<Store>, preference: &str) -> AnyElement {
    let system = || tf("language.system", &[("language", &i18n::language_name(i18n::system_language()))]);
    let label = if i18n::LANGUAGES.iter().any(|l| l.code == preference) {
        i18n::language_name(preference).to_string()
    } else {
        system()
    };
    let preference = preference.to_string();
    let store = store.clone();
    Button::new("language")
        .small()
        .outline()
        .label(label)
        .dropdown_caret(true)
        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
            let pick = |code: &'static str| {
                let store = store.clone();
                move |_: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
                    store.update(cx, |s, cx| s.update_config(cx, |c| c.language = code.into()));
                }
            };
            let chosen = |code: &str| preference == code;
            let mut menu = menu
                .item(
                    PopupMenuItem::new(system())
                        .checked(!i18n::LANGUAGES.iter().any(|l| chosen(l.code)))
                        .on_click(pick(i18n::SYSTEM)),
                )
                .separator();
            for language in i18n::LANGUAGES {
                menu = menu.item(
                    PopupMenuItem::new(language.name).checked(chosen(language.code)).on_click(pick(language.code)),
                );
            }
            menu
        })
        .into_any_element()
}

/// A switch that edits one setting.
fn toggle(
    store: &Entity<Store>,
    id: &'static str,
    on: bool,
    enabled: bool,
    set: impl Fn(&mut Config, bool) + Clone + 'static,
) -> AnyElement {
    let store = store.clone();
    Switch::new(id)
        .small()
        .checked(on)
        .disabled(!enabled)
        .on_change(move |on: &bool, _, cx| {
            let (on, set) = (*on, set.clone());
            store.update(cx, |s, cx| s.update_config(cx, move |c| set(c, on)));
        })
        .into_any_element()
}

/// A titled group: the rows share one surface, with hairlines between them.
fn group(title: String, rows: Vec<AnyElement>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let mut surface =
        v_flex().bg(theme.group_box).border_1().border_color(theme.border).rounded(theme.radius_lg).overflow_hidden();
    for (i, row) in rows.into_iter().enumerate() {
        if i > 0 {
            surface = surface.child(div().ml_3().h_px().bg(theme.border));
        }
        surface = surface.child(row);
    }
    v_flex()
        .gap_2()
        .child(
            div()
                .px_1()
                .text_size(rems(0.846))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.muted_foreground)
                .child(title),
        )
        .child(surface)
        .into_any_element()
}

fn row(title: String, detail: Option<String>, control: AnyElement, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    h_flex()
        .gap_3()
        .px_3()
        .py_2p5()
        .child(v_flex().flex_1().min_w_0().gap_0p5().child(div().child(title)).when_some(detail, |el, d| {
            el.child(div().text_size(rems(0.846)).line_height(rems(1.2)).text_color(muted).child(d))
        }))
        .child(control)
        .into_any_element()
}

fn note(text: String, cx: &App) -> AnyElement {
    div().px_3().py_2p5().text_size(rems(0.923)).text_color(cx.theme().muted_foreground).child(text).into_any_element()
}
