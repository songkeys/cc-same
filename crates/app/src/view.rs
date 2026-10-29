//! The main window: a picture of your accounts and one sentence about them, the switch that keeps
//! them in step, the accounts themselves, and a note only when something needs you. Settings,
//! snapshots and files live in a sheet behind the gear.

use crate::hero;
use crate::i18n::{t, tf, tn};
use crate::model::{self, Account, Busy, Fullness, Login, Mood, PlanUse};
use crate::settings;
use crate::store::{Store, StoreEvent, UpdatePhase};
use crate::theme;
use crate::update::{self, Release};
use cc_same_core::desktop::Quitting;
use cc_same_core::report::Warning;
use cc_same_core::retention::Limit;
use cc_same_core::snapshot::Manifest;
use gpui_kit::base::{Easing, Transition, transition};
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::text::{TextView, TextViewStyle};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Theme, TitleBar, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    Anchor, Animation, AnimationExt as _, AnyElement, App, AppContext as _, ClickEvent, Context, Div, ElementId,
    Entity, FocusHandle, FontWeight, HighlightStyle, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, StyledText, Subscription, Window, actions, div,
    ease_out_quint, linear_color_stop, linear_gradient, prelude::FluentBuilder as _, px, rems,
};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

actions!(cc_same, [Refresh, SyncNow, OpenSettings]);

pub struct UniApp {
    store: Entity<Store>,
    /// The account under the pointer, in the list or in the picture.
    hovered: Option<SharedString>,
    /// False until the first frame with data has been drawn, so the rings fly in and the
    /// numbers count up.
    gathered: bool,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl UniApp {
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> UniApp {
        let subscriptions = vec![
            cx.observe_window_appearance(window, |_, _, cx| cx.notify()),
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe_in(&store, window, |_, _, event: &StoreEvent, window, cx| {
                let StoreEvent::Done(result) = event;
                show_outcome(result, window, cx);
            }),
        ];
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        UniApp { store, hovered: None, gathered: false, focus, _subscriptions: subscriptions }
    }

    /// A still picture (screenshots): everything already in place.
    pub fn still(store: Entity<Store>, cx: &mut Context<Self>) -> UniApp {
        UniApp { store, hovered: None, gathered: true, focus: cx.focus_handle(), _subscriptions: Vec::new() }
    }

    pub fn store(&self) -> &Entity<Store> {
        &self.store
    }

    // ------------------------------------------------------------------ actions

    fn sync_now(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| store.sync_now(cx));
    }

    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        settings::open(self.store.clone(), window, cx);
    }

    pub fn confirm_restore(&mut self, snapshot: &Manifest, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let id = snapshot.id.clone();
        let when = model::snapshot_when(&snapshot.id);
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (store, id) = (store.clone(), id.clone());
            alert
                .title(tf("restore.title", &[("when", &when)]))
                .description(t("restore.body"))
                .confirm()
                .ok_text(t("restore.ok"))
                .cancel_text(t("dialog.cancel"))
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let id = id.clone();
                    store.update(cx, |store, cx| store.restore(id, cx));
                    true
                })
        });
    }

    fn confirm_fix_links(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let store = store.clone();
            alert
                .title(t("fix.title"))
                .description(t("fix.body"))
                .confirm()
                .ok_text(t("fix.ok"))
                .cancel_text(t("dialog.cancel"))
                .on_ok(move |_, _, cx| {
                    store.update(cx, |store, cx| store.fix_links(cx));
                    true
                })
        });
    }

    /// What sits at the end of an account's row: where Claude stands with it, or what one click
    /// does.
    fn account_action(&self, a: &Account, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (busy, switching_to, signing_in) = {
            let store = self.store.read(cx);
            (store.busy, store.switching_to.clone(), store.signing_in.is_some())
        };
        let theme = cx.theme();
        // The account in use: in the accent color, like the window's buttons, while Claude runs on
        // it; grey while Claude is closed.
        let current = |tag: Tag| tag.small().rounded_full().child(t("account.current")).into_any_element();
        if a.open {
            return Some(current(Tag::primary()));
        }
        if a.login == Login::SignedIn {
            return Some(current(Tag::secondary()));
        }
        if switching_to.as_deref() == Some(a.id.as_str()) {
            let muted = theme.muted_foreground;
            return Some(
                h_flex()
                    .gap_1p5()
                    .text_size(rems(0.923))
                    .text_color(muted)
                    .child(Spinner::new().xsmall())
                    .child(t("account.switching"))
                    .into_any_element(),
            );
        }
        let id = a.id.clone();
        match a.login {
            Login::Saved { stale } => Some(
                Button::new(SharedString::from(format!("switch-{}", a.id)))
                    .small()
                    .outline()
                    .label(t("account.switch"))
                    .disabled(busy.is_some())
                    .when(stale, |b| b.tooltip(t("account.stale")))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let id = id.clone();
                        this.store.update(cx, |store, cx| store.switch_account(id, cx));
                    }))
                    .into_any_element(),
            ),
            // Waiting for a sign-in already: one restart is enough.
            Login::Unknown if signing_in => None,
            Login::Unknown => {
                // Named in the dialog only by an email address, something people can type.
                let name = a.email.clone();
                Some(
                    Button::new(SharedString::from(format!("sign-in-{}", a.id)))
                        .small()
                        .ghost()
                        .label(t("account.sign_in"))
                        .disabled(busy.is_some())
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.confirm_sign_in(name.clone(), window, cx)),
                        )
                        .into_any_element(),
                )
            }
            Login::SignedIn | Login::Unsupported => None,
        }
    }

    /// Explain, then restart Claude on its sign-in page. `expected` names the account the user
    /// picked, when they did.
    pub fn confirm_sign_in(&mut self, expected: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let current = {
            let store = store.read(cx);
            let signed_in = store.overview.as_ref().and_then(|o| o.logins.signed_in.clone());
            signed_in.map(|id| store.account_name(&id))
        };
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let muted = cx.theme().muted_foreground;
            let (title, body) = match &expected {
                Some(name) => (tf("signin.title_as", &[("account", name)]), tf("signin.body_as", &[("account", name)])),
                None => (t("signin.title"), t("signin.body")),
            };
            let description = v_flex()
                .gap_2()
                .child(body)
                .when_some(current.as_ref(), |el, name| el.child(tf("signin.keeps", &[("account", name)])))
                .child(div().pt_1().text_size(rems(0.846)).text_color(muted).child(t("signin.tip")));
            let (store, expected) = (store.clone(), expected.clone());
            alert
                .title(title)
                .description(description)
                .confirm()
                .ok_text(t("signin.ok"))
                .cancel_text(t("dialog.cancel"))
                .on_ok(move |_, _, cx| {
                    let expected = expected.clone();
                    store.update(cx, |store, cx| store.sign_in_another(expected, cx));
                    true
                })
        });
    }

    /// Ask before switching Claude to `account`, as the menu bar does: a menu item is easy to hit
    /// by mistake, and switching restarts Claude. An account whose sign-in is kept switches; one
    /// without restarts Claude on its sign-in page instead, as its "Sign in" button does.
    pub fn confirm_switch(&mut self, account: String, window: &mut Window, cx: &mut Context<Self>) {
        let (saved, name, email, current) = {
            let store = self.store.read(cx);
            let Some(overview) = store.overview.as_ref() else { return };
            if store.busy.is_some() || overview.logins.signed_in.as_deref() == Some(account.as_str()) {
                return;
            }
            let saved = overview.logins.saved(&account).cloned();
            let email = store.accounts().into_iter().find(|a| a.id == account).and_then(|a| a.email);
            let email = email.or_else(|| saved.as_ref().and_then(|s| s.email.clone()));
            let current = overview.logins.signed_in.as_ref().map(|id| store.account_name(id));
            (saved, store.account_name(&account), email, current)
        };
        let Some(saved) = saved else {
            return self.confirm_sign_in(email, window, cx);
        };
        let store = self.store.clone();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let muted = cx.theme().muted_foreground;
            let description = v_flex()
                .gap_2()
                .child(tf("switch.confirm_body", &[("account", &name)]))
                .when_some(current.as_ref(), |el, current| el.child(tf("signin.keeps", &[("account", current)])))
                .when(saved.stale(), |el| {
                    el.child(div().pt_1().text_size(rems(0.846)).text_color(muted).child(t("account.stale")))
                });
            let (store, account) = (store.clone(), account.clone());
            alert
                .title(tf("switch.confirm_title", &[("account", &name)]))
                .description(description)
                .confirm()
                .ok_text(t("account.switch"))
                .cancel_text(t("dialog.cancel"))
                .on_ok(move |_, _, cx| {
                    let account = account.clone();
                    store.update(cx, |store, cx| store.switch_account(account, cx));
                    true
                })
        });
    }

    /// Ask, then take an account off the list (forgetting the sign-in kept for it).
    pub fn confirm_remove(&mut self, account: String, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (store, account) = (store.clone(), account.clone());
            alert
                .title(tf("remove.title", &[("account", &name)]))
                .description(t("remove.body"))
                .confirm()
                .ok_text(t("remove.ok"))
                .cancel_text(t("dialog.cancel"))
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let account = account.clone();
                    store.update(cx, |store, cx| store.remove_account(account, cx));
                    true
                })
        });
    }

    /// Ask for the name an account goes by in the list (its alias), and when its weekly window
    /// renews. Empty takes either away.
    pub fn open_name(&mut self, account: String, window: &mut Window, cx: &mut Context<Self>) {
        let now = cc_same_core::fsx::now_secs() as i64;
        let (title, current, others, reset) = {
            let rows = self.store.read(cx).accounts();
            let Some(row) = rows.iter().find(|a| a.id == account) else { return };
            // An account as people know it apart from its name: its email, else its ID.
            let known_as = |a: &Account| {
                a.email.clone().unwrap_or_else(|| tf("account.unnamed", &[("id", &cc_same_core::short(&a.id))]))
            };
            let others: Vec<(String, String)> =
                rows.iter().filter(|a| a.id != account).filter_map(|a| Some((a.alias.clone()?, known_as(a)))).collect();
            let title = tf("name.title", &[("account", &known_as(row))]);
            (title, row.alias.clone(), Rc::new(others), row.weekly_reset)
        };
        // The renewal as this computer's clock reads it now.
        let shown = reset.and_then(|anchor| model::reset_local(anchor, now));
        let day = cx.new(|_| shown.as_ref().map(|(day, _)| *day));
        let clock = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t("reset.placeholder"))
                .default_value(shown.map(|(_, time)| time).unwrap_or_default())
        });
        let reset_problem = cx.new(|_| None::<String>);
        cx.subscribe(&clock, {
            let problem = reset_problem.clone();
            move |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    problem.update(cx, |problem, cx| {
                        if problem.take().is_some() {
                            cx.notify();
                        }
                    });
                }
            }
        })
        .detach();
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t("name.placeholder")).default_value(current.unwrap_or_default())
        });
        let problem = cx.new(|_| None::<String>);
        // Editing clears the complaint about what was there before.
        cx.subscribe(&input, {
            let problem = problem.clone();
            move |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    problem.update(cx, |problem, cx| {
                        if problem.take().is_some() {
                            cx.notify();
                        }
                    });
                }
            }
        })
        .detach();
        let (store, field) = (self.store.clone(), input.clone());
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let theme = cx.theme();
            let picked = *day.read(cx);
            // The day picked stands out in the accent color; the others stay quiet.
            let days = h_flex().gap_1().children((0..7).map(|i| {
                let (day, problem) = (day.clone(), reset_problem.clone());
                let button = Button::new(SharedString::from(format!("reset-day-{i}")))
                    .xsmall()
                    .label(model::weekday_name(i))
                    .on_click(move |_, window, cx| {
                        day.update(cx, |day, cx| {
                            *day = Some(i);
                            cx.notify();
                        });
                        problem.update(cx, |problem, _| *problem = None);
                        window.refresh();
                    });
                match picked == Some(i) {
                    true => button.primary(),
                    false => button.outline(),
                }
            }));
            let reset_note = match reset_problem.read(cx) {
                Some(problem) => div().text_color(theme.danger).child(problem.clone()),
                None => div().text_color(theme.muted_foreground).child(t("reset.hint")),
            };
            let reset_fields = v_flex()
                .pt_2()
                .gap_2()
                .child(div().font_weight(FontWeight::MEDIUM).child(t("reset.label")))
                .child(days)
                .child(Input::new(&clock).cleanable(true).py_1())
                .child(reset_note.text_size(rems(0.846)).line_height(rems(1.2)));
            let note = match problem.read(cx) {
                Some(problem) => div().text_color(theme.danger).child(problem.clone()),
                None => div()
                    .text_color(theme.muted_foreground)
                    .child(tf("name.hint", &[("max", &cc_same_core::accounts::ALIAS_MAX)])),
            };
            // Below the description, not in it: the field keeps its own color and line height. The
            // dialog clips its body, so the field's focus ring needs room above it.
            let field_and_note = v_flex()
                .pt_1()
                .gap_2()
                // Its default 8 px padding leaves a 20 px line 16 px and cuts off descenders.
                .child(Input::new(&field).cleanable(true).py_1())
                .child(note.text_size(rems(0.846)).line_height(rems(1.2)))
                .child(reset_fields);
            let (input, problem, store, account, others) =
                (field.clone(), problem.clone(), store.clone(), account.clone(), others.clone());
            let (day, clock, reset_problem) = (day.clone(), clock.clone(), reset_problem.clone());
            alert
                .title(title.clone())
                .description(t("name.body"))
                .child(field_and_note)
                .confirm()
                .ok_text(t("name.ok"))
                .cancel_text(t("dialog.cancel"))
                .on_ok(move |_, window, cx| {
                    let text = input.read(cx).value();
                    if let Some(found) = model::name_problem(&text, &others) {
                        problem.update(cx, |problem, cx| {
                            *problem = Some(found);
                            cx.notify();
                        });
                        window.refresh();
                        return false;
                    }
                    let renews = model::reset_from(*day.read(cx), &clock.read(cx).value(), now);
                    let renews = match renews {
                        Ok(renews) => renews,
                        Err(found) => {
                            reset_problem.update(cx, |problem, cx| {
                                *problem = Some(found);
                                cx.notify();
                            });
                            window.refresh();
                            return false;
                        }
                    };
                    let name = Some(text.trim().to_string()).filter(|name| !name.is_empty());
                    let account = account.clone();
                    store.update(cx, |store, cx| store.set_alias(account.clone(), name, cx));
                    // The same renewal given again keeps the moment noted before.
                    let same = |a: Option<i64>, b: Option<i64>| {
                        let next = |at: Option<i64>| at.map(|at| cc_same_core::usage::next_weekly_reset(at, now));
                        next(a) == next(b)
                    };
                    if !same(reset, renews) {
                        store.update(cx, |store, cx| store.set_weekly_reset(account, renews, cx));
                    }
                    true
                })
        });
        input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// Ask, then switch Claude to the next account in the list.
    pub fn confirm_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.store.read(cx).next_account() {
            Some(next) => self.confirm_switch(next, window, cx),
            None => show_outcome(&Err(t("switch.no_other")), window, cx),
        }
    }

    /// The account under the pointer, by ID (screenshots set it as well).
    pub fn set_hovered(&mut self, key: Option<SharedString>, cx: &mut Context<Self>) {
        if self.hovered != key {
            self.hovered = key;
            cx.notify();
        }
    }

    // ------------------------------------------------------------------ rendering

    fn render_title_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        TitleBar::new()
            .bg(gpui_kit::transparent_black())
            .border_color(gpui_kit::transparent_black())
            .child(
                h_flex().w_full().justify_end().pr_2().child(
                    Button::new("open-settings")
                        .ghost()
                        .small()
                        .icon(IconName::Settings2)
                        .tooltip(t("settings.title"))
                        // Keep Windows' native caption hit-test off this button.
                        .when(cfg!(windows), |button| button.occlude())
                        .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
                ),
            )
            .into_any_element()
    }

    fn render_headline(
        &mut self,
        mood: Mood,
        accounts: &[Account],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let view = cx.entity().downgrade();
        let on_hover: hero::OnHover = Rc::new(move |key, _, cx| {
            let _ = view.update(cx, |this, cx| this.set_hovered(key, cx));
        });
        let gathered = self.gathered || mood == Mood::Loading;
        let syncing: Vec<Account> = accounts.iter().filter(|a| a.has_lists).cloned().collect();
        let picture = hero::rings(&syncing, mood, self.hovered.as_deref(), gathered, on_hover, window, cx);

        // The count rolls up to its value when the window opens, and on to a new one.
        let counted = |n: usize, window: &mut Window, cx: &mut Context<Self>| -> usize {
            let target = if gathered { n as f32 } else { 0. };
            let value = transition(
                "headline-count",
                target,
                Transition::new(Duration::from_millis(900)).easing(Easing::EaseOut),
                window,
                cx,
            );
            value.round() as usize
        };
        let shown = match mood {
            Mood::InSync { sessions, accounts } => Mood::InSync { sessions: counted(sessions, window, cx), accounts },
            Mood::Pending { sessions, other } if sessions > 0 => {
                Mood::Pending { sessions: counted(sessions, window, cx), other }
            }
            other => other,
        };

        let (overview, busy) = {
            let store = self.store.read(cx);
            (store.overview.clone(), store.busy)
        };
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let foreground = theme.foreground;
        let synced = overview
            .as_ref()
            .and_then(|o| o.last_sync.as_ref())
            .filter(|_| matches!(mood, Mood::InSync { .. } | Mood::Waiting(_)))
            .map(|ls| tf("synced_ago", &[("ago", &crate::i18n::ago(ls.at))]));
        let running = overview.as_ref().is_some_and(|o| o.app.running);
        let action: Option<AnyElement> = match mood {
            Mood::Pending { .. } => Some(
                Button::new("sync-now")
                    .primary()
                    .label(t("action.sync_now"))
                    .icon(IconName::RefreshCw)
                    .disabled(busy.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.sync_now(cx)))
                    .into_any_element(),
            ),
            Mood::Linked(_) => Some(
                v_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("fix-links")
                            .primary()
                            .label(t("action.fix_folders"))
                            .disabled(running || busy.is_some())
                            .on_click(cx.listener(|this, _, window, cx| this.confirm_fix_links(window, cx))),
                    )
                    .when(running, |el| {
                        el.child(div().text_size(rems(0.923)).text_color(muted).child(t("hint.quit_first")))
                    })
                    .into_any_element(),
            ),
            _ => None,
        };

        let detail = shown.detail();
        let words = v_flex()
            .relative()
            .items_center()
            .gap_1()
            .child(
                div()
                    .text_size(rems(1.54))
                    .line_height(rems(1.9))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_center()
                    .child(shown.title()),
            )
            .when(!detail.is_empty(), |el| {
                el.child(div().text_center().text_color(muted).child(emphasize_numbers(detail, foreground)))
            })
            // A new sentence rises into place; a number that changes does not replay it.
            .with_animation(
                ElementId::Name(format!("headline-{}", mood_kind(mood)).into()),
                Animation::new(Duration::from_millis(360)).with_easing(ease_out_quint()),
                |el, t| el.opacity(t).top(px(5. * (1. - t))),
            );

        v_flex()
            .items_center()
            .px_8()
            .pb_6()
            .child(picture)
            .child(words)
            .when_some(synced, |el, synced| {
                el.child(div().mt_1p5().text_size(rems(0.923)).text_color(muted.opacity(0.8)).child(synced))
            })
            .when_some(action, |el, action| el.child(div().mt_4().child(action)))
    }

    fn render_background(&self, cx: &mut Context<Self>) -> AnyElement {
        let (overview, busy, switched_on_at) = {
            let store = self.store.read(cx);
            (store.overview.clone(), store.busy, store.switched_on_at)
        };
        let theme = cx.theme();
        let st = overview.as_ref().map(|o| o.service.clone()).unwrap_or_default();
        let alive = overview.as_deref().is_some_and(model::agent_alive);
        // Just switched on: the agent is starting and has not checked in yet.
        let starting = switched_on_at.is_some_and(|t| t.elapsed() < Duration::from_secs(90));
        let (line, tone): (String, Hsla) = if st.installed && alive {
            let checked =
                overview.as_ref().and_then(|o| o.heartbeat.as_ref()).map(|h| crate::i18n::ago(h.heartbeat_at));
            (tf("background.on", &[("ago", &checked.unwrap_or_else(|| t("time.just_now")))]), theme.muted_foreground)
        } else if st.installed && starting {
            (t("background.starting"), theme.muted_foreground)
        } else if st.installed {
            let mut line = t("background.stalled");
            if cfg!(target_os = "macos") {
                line = format!("{line} {}", t("background.stalled_mac"));
            }
            (line, theme.warning)
        } else if st.legacy {
            (t("background.legacy"), theme.warning)
        } else {
            (t("background.off"), theme.muted_foreground)
        };
        let switching = busy == Some(Busy::Service);
        let store = self.store.clone();
        surface(cx)
            .child(
                h_flex()
                    .gap_4()
                    .px_4()
                    .py_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(div().font_weight(FontWeight::MEDIUM).child(t("background.title")))
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .text_size(rems(0.923))
                                    .text_color(tone)
                                    .when(st.installed && alive, |el| el.child(dot(theme.success)))
                                    .child(div().min_w_0().child(line)),
                            ),
                    )
                    .child(if switching {
                        Spinner::new().small().into_any_element()
                    } else {
                        Switch::new("background")
                            .checked(st.installed)
                            .disabled(busy.is_some() || overview.is_none())
                            .on_change(move |on: &bool, _, cx| {
                                let on = *on;
                                store.update(cx, |store, cx| store.set_background(on, cx));
                            })
                            .into_any_element()
                    }),
            )
            .into_any_element()
    }

    fn render_accounts(&self, accounts: &[Account], cx: &mut Context<Self>) -> AnyElement {
        let (loading, switching, settled) = {
            let store = self.store.read(cx);
            let switching = store.overview.as_ref().is_some_and(|o| o.logins.supported);
            (store.overview.is_none(), switching, store.mood().settled())
        };
        let actions: Vec<Option<AnyElement>> = accounts.iter().map(|a| self.account_action(a, cx)).collect();
        let add = switching.then(|| {
            Button::new("add-account")
                .ghost()
                .xsmall()
                .icon(IconName::Plus)
                .tooltip(t("accounts.add"))
                .disabled(self.store.read(cx).busy.is_some() || self.store.read(cx).signing_in.is_some())
                .on_click(cx.listener(|this, _, window, cx| this.confirm_sign_in(None, window, cx)))
        });
        let theme = cx.theme();
        let rows: Vec<AnyElement> = if loading {
            (0..3).map(|i| skeleton_row(i, cx)).collect()
        } else {
            accounts
                .iter()
                .zip(actions)
                .map(|(a, action)| {
                    let key: SharedString = a.id.clone().into();
                    let hovered = self.hovered.as_ref() == Some(&key);
                    // In sync, the headline already counts every account's sessions: the plan is
                    // all the row needs to say.
                    let plan_leads = settled && a.missing == 0 && a.plan_known();
                    let hover_key = key.clone();
                    let row = h_flex()
                        .id(ElementId::Name(format!("account-{}", a.id).into()))
                        .gap_3()
                        .px_3()
                        .py_2()
                        .rounded(theme.radius)
                        .when(hovered, |el| el.bg(theme.list_hover))
                        .on_hover(cx.listener(move |this, inside: &bool, _, cx| {
                            let key = inside.then(|| hover_key.clone());
                            if key.is_some() || this.hovered.as_ref() == Some(&hover_key) {
                                this.set_hovered(key, cx);
                            }
                        }))
                        .when(a.excluded, |el| el.opacity(0.55))
                        .child(Avatar::new().name(a.initials()).small())
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .min_w_0()
                                        .child(div().truncate().font_weight(FontWeight::MEDIUM).child(a.name.clone()))
                                        // Naming is in the row's menu too, which people may not
                                        // find; only accounts in the list have a name to give.
                                        .when(hovered && a.number.is_some(), |el| {
                                            let id = a.id.clone();
                                            el.child(
                                                Button::new(SharedString::from(format!("name-{}", a.id)))
                                                    .ghost()
                                                    .xsmall()
                                                    .icon(gpui_kit::assets::IconName::Pencil)
                                                    .tooltip(t("account.name_tip"))
                                                    .on_click(cx.listener(move |this, _, window, cx| {
                                                        this.open_name(id.clone(), window, cx)
                                                    })),
                                            )
                                        }),
                                )
                                .child(
                                    h_flex()
                                        .min_w_0()
                                        .text_size(rems(0.923))
                                        .text_color(if a.broken { theme.danger } else { theme.muted_foreground })
                                        .when(!plan_leads, |el| {
                                            el.child(div().when(!a.broken, |el| el.truncate()).child(a.detail()))
                                        })
                                        .when_some(a.usage.filter(|_| !a.broken && !a.excluded), |el, usage| {
                                            el.child(plan_use(&a.id, usage, plan_leads, cx))
                                        })
                                        .when_some(a.weekly_reset.filter(|_| !a.broken && !a.excluded), |el, at| {
                                            let after = match (a.usage.is_some(), plan_leads) {
                                                (true, _) => After::Usage,
                                                (false, false) => After::Sessions,
                                                (false, true) => After::Nothing,
                                            };
                                            el.child(reset_left(&a.id, at, after, cx))
                                        }),
                                ),
                        )
                        .children(action);
                    // An account in the list can be named, sit out of "Next Account", or leave the list.
                    let Some(_) = a.number else { return row.into_any_element() };
                    let view = cx.entity().downgrade();
                    let store = self.store.clone();
                    let (id, name, in_rotation) = (a.id.clone(), a.name.clone(), !a.sits_out);
                    let removable = a.login != Login::SignedIn;
                    row.context_menu(move |menu, _, _| {
                        let (view, store, id, name) = (view.clone(), store.clone(), id.clone(), name.clone());
                        let (naming, namer, toggle) = (id.clone(), view.clone(), id.clone());
                        let menu = menu
                            .item(PopupMenuItem::new(t("account.name")).on_click(move |_, window, cx| {
                                let account = naming.clone();
                                let _ = namer.update(cx, |this, cx| this.open_name(account, window, cx));
                            }))
                            .item(PopupMenuItem::new(t("account.rotation")).checked(in_rotation).on_click(
                                move |_, _, cx| {
                                    let account = toggle.clone();
                                    store.update(cx, |store, cx| store.set_in_rotation(account, !in_rotation, cx));
                                },
                            ));
                        if !removable {
                            return menu;
                        }
                        menu.separator().item(PopupMenuItem::new(t("account.remove")).on_click(move |_, window, cx| {
                            let (id, name) = (id.clone(), name.clone());
                            let _ = view.update(cx, |this, cx| this.confirm_remove(id, name, window, cx));
                        }))
                    })
                    .into_any_element()
                })
                .collect()
        };
        let empty = rows.is_empty();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .px_1()
                    .h(px(20.))
                    .justify_between()
                    .child(section_label(t("accounts.title"), cx))
                    .child(h_flex().gap_2().children(add)),
            )
            .child(surface(cx).p_1().gap_px().children(rows).when(empty, |el| {
                el.child(
                    div()
                        .px_3()
                        .py_3()
                        .text_size(rems(0.923))
                        .text_color(cx.theme().muted_foreground)
                        .child(t("accounts.empty")),
                )
            }))
            .into_any_element()
    }

    fn render_notices(&self, accounts: &[Account], cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut notices = Vec::new();
        let (overview, busy, log) = {
            let store = self.store.read(cx);
            (store.overview.clone(), store.busy, store.ctx.paths.log_file.clone())
        };
        let Some(ov) = overview else { return notices };
        notices.extend(self.render_sign_in_notice(cx));
        if let Some(hint) = ov.restart_hint() {
            let open =
                accounts.iter().find(|a| a.open).map(|a| a.name.clone()).unwrap_or_else(|| t("notice.open_account"));
            notices.push(notice(
                "notice-restart",
                Tone::Info,
                tn("notice.restart", hint.sessions as usize, &[("account", &open)]),
                Vec::new(),
                None,
                cx,
            ));
        }
        if let Some(ls) = &ov.last_sync
            && ls.errors > 0
        {
            notices.push(notice(
                "notice-errors",
                Tone::Danger,
                tn("notice.errors", ls.errors as usize, &[]),
                vec![
                    Button::new("open-log")
                        .small()
                        .outline()
                        .label(t("notice.show_log"))
                        .on_click(move |_, _, cx| cx.open_with_system(&log))
                        .into_any_element(),
                ],
                None,
                cx,
            ));
        }
        // Only when Claude Code will actually delete the transcripts behind Desktop sessions.
        if let Some((days, limited_by)) = ov.warnings.iter().find_map(|w| match w {
            Warning::ShortRetention { days, limited_by, .. } => Some((*days, *limited_by)),
            _ => None,
        }) {
            let mut text = tn("notice.retention", days.round() as usize, &[]);
            match limited_by {
                Limit::Organization => text = format!("{text} {}", t("notice.retention_org")),
                Limit::OlderClaudeCode => text = format!("{text} {}", t("notice.retention_old")),
                Limit::DesktopSetting => {}
            }
            let action = (limited_by != Limit::Organization).then(|| {
                let store = self.store.clone();
                let label = match limited_by {
                    Limit::OlderClaudeCode => t("notice.keep_10_years"),
                    _ => t("notice.keep_them"),
                };
                Button::new("keep-transcripts")
                    .small()
                    .outline()
                    .label(label)
                    .disabled(busy.is_some())
                    .on_click(move |_, _, cx| store.update(cx, |store, cx| store.keep_transcripts(cx)))
                    .into_any_element()
            });
            notices.push(notice("notice-retention", Tone::Warning, text, action.into_iter().collect(), None, cx));
        }
        notices.extend(self.render_update_notice(cx));
        notices
    }

    /// Claude is asking before it quits for a switch, restarting on its sign-in page, or waiting
    /// there for the user to sign in.
    fn render_sign_in_notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (busy, switching_to, signing_in, quitting) = {
            let store = self.store.read(cx);
            (store.busy, store.switching_to.clone(), store.signing_in.clone(), store.quitting())
        };
        // Claude has work in progress: it asks whether to stop it, or waits for it. Either way the
        // user decides in Claude, and can stop waiting here (say after choosing Cancel there).
        if let Some(quitting @ (Quitting::Confirming | Quitting::AfterWork)) = quitting {
            let (tone, text) = match quitting {
                Quitting::Confirming => (Tone::Warning, t("quit.confirming")),
                _ => (Tone::Info, t("quit.after_work")),
            };
            let stop = Button::new("stop-waiting")
                .small()
                .outline()
                .label(t("quit.stop"))
                .on_click(cx.listener(|this, _, _, cx| this.store.update(cx, |store, cx| store.stop_waiting(cx))))
                .into_any_element();
            return Some(notice("notice-quitting", tone, text, vec![stop], None, cx));
        }
        if busy == Some(Busy::Switching) && switching_to.is_none() {
            let spinner = Spinner::new().small().into_any_element();
            return Some(notice("notice-sign-in", Tone::Info, t("signin.restarting"), vec![spinner], None, cx));
        }
        let pending = signing_in?;
        let text = match &pending.expected {
            Some(name) => tf("signin.pending_as", &[("account", name)]),
            None => t("signin.pending"),
        };
        let back = pending.previous.map(|previous| {
            Button::new("sign-in-back")
                .small()
                .outline()
                .label(t("signin.back"))
                .disabled(busy.is_some())
                .on_click(cx.listener(move |this, _, _, cx| {
                    let previous = previous.clone();
                    this.store.update(cx, |store, cx| store.switch_account(previous, cx));
                }))
                .into_any_element()
        });
        Some(notice("notice-sign-in", Tone::Info, text, back.into_iter().collect(), None, cx))
    }

    /// A new version: available, on its way, ready, or just installed.
    fn render_update_notice(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (updates, busy) = {
            let store = self.store.read(cx);
            (store.updates.clone(), store.busy)
        };
        let store = self.store.clone();
        let update_now = |id: &'static str, label: String, enabled: bool| -> AnyElement {
            let store = store.clone();
            Button::new(id)
                .small()
                .primary()
                .label(label)
                .disabled(!enabled)
                .on_click(move |_, _, cx| store.update(cx, |store, cx| store.update_or_check(cx)))
                .into_any_element()
        };
        let Some(release) = updates.available.clone() else {
            // Just updated: what changed since the version before.
            let from = updates.updated_from.clone()?;
            let notes = update::changes_since(semver::Version::parse(&from).ok().as_ref());
            let title = tf("update.notes_title", &[("version", &update::VERSION)]);
            let dismiss = {
                let store = store.clone();
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| store.update(cx, |s, cx| s.dismiss_updated(cx))
            };
            let read = Button::new("updated-notes").small().outline().label(t("update.whats_new")).on_click({
                let dismiss = dismiss.clone();
                move |event, window, cx| {
                    open_notes(title.clone(), notes.clone(), window, cx);
                    dismiss(event, window, cx);
                }
            });
            let close = Button::new("updated-close").ghost().xsmall().icon(IconName::Close).on_click(dismiss);
            let text = tf("update.updated", &[("version", &update::VERSION)]);
            return Some(notice(
                "notice-updated",
                Tone::New,
                text,
                vec![read.into_any_element()],
                Some(close.into_any_element()),
                cx,
            ));
        };
        let version = release.version.to_string();
        let whats_new = release_notes_button(&release);
        let (tone, text, actions) = match &updates.phase {
            UpdatePhase::Downloading(_) => {
                let percent = updates.progress().unwrap_or(0.) * 100.;
                let bar = div().w_full().pt_0p5().child(Progress::new("update-progress").value(percent));
                (Tone::Update, tf("update.downloading", &[("version", &version)]), vec![bar.into_any_element()])
            }
            UpdatePhase::Installing => (Tone::Update, tf("update.installing", &[("version", &version)]), Vec::new()),
            UpdatePhase::Ready(_) => (
                Tone::Update,
                tf("update.ready", &[("version", &version)]),
                vec![update_now("update-restart", t("update.restart"), busy.is_none()), whats_new],
            ),
            UpdatePhase::Idle | UpdatePhase::Checking => match &updates.problem {
                Some((problem, _)) => {
                    let page = release.page.clone();
                    let download = Button::new("update-download")
                        .small()
                        .outline()
                        .icon(IconName::ExternalLink)
                        .label(t("update.download"))
                        .on_click(move |_, _, cx| cx.open_url(&page));
                    let retry = Button::new("update-retry").small().outline().label(t("update.retry")).on_click({
                        let store = store.clone();
                        move |_, _, cx| store.update(cx, |store, cx| store.update_or_check(cx))
                    });
                    (
                        Tone::Warning,
                        t(problem.message_key()),
                        vec![retry.disabled(updates.working()).into_any_element(), download.into_any_element()],
                    )
                }
                None => (
                    Tone::Update,
                    tf("update.available", &[("version", &version)]),
                    vec![update_now("update-now", t("update.install"), !updates.working()), whats_new],
                ),
            },
        };
        Some(notice("notice-update", tone, text, actions, None, cx))
    }
}

impl Render for UniApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (theme_choice, mood, accounts, loaded) = {
            let store = self.store.read(cx);
            (store.theme(), store.mood(), store.accounts(), store.overview.is_some())
        };
        theme::follow(theme_choice, window, cx);
        if loaded && !self.gathered {
            cx.on_next_frame(window, |this, _, cx| {
                this.gathered = true;
                cx.notify();
            });
        }
        let headline = self.render_headline(mood, &accounts, window, cx);
        let background = self.render_background(cx);
        let list = self.render_accounts(&accounts, cx);
        let notices = self.render_notices(&accounts, cx);
        let (background_color, text_color, wash) = {
            let theme = cx.theme();
            (theme.background, theme.foreground, theme::wash(cx))
        };
        v_flex()
            .relative()
            .key_context("UniApp")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.store.update(cx, |s, cx| s.refresh(cx))))
            .on_action(cx.listener(|this, _: &SyncNow, _, cx| this.sync_now(cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| this.open_settings(window, cx)))
            .on_action(cx.listener(|_, _: &crate::CloseWindow, window, _| window.remove_window()))
            .size_full()
            .bg(background_color)
            .text_color(text_color)
            // A warm light falls on the top of the window.
            .child(div().absolute().top_0().left_0().right_0().h(px(300.)).bg(linear_gradient(
                180.,
                linear_color_stop(wash, 0.),
                linear_color_stop(wash.opacity(0.), 1.),
            )))
            .child(self.render_title_bar(cx))
            .child(
                div().id("content").flex_1().min_h_0().overflow_y_scrollbar().child(
                    v_flex()
                        .pb_6()
                        .child(headline)
                        .child(v_flex().px_5().gap_5().children(notices).child(background).child(list)),
                ),
            )
    }
}

/// The outcome of a task, as a notification: an error stays until it is dismissed.
pub fn show_outcome(result: &Result<String, String>, window: &mut Window, cx: &mut App) {
    let note = match result {
        Ok(message) if message.is_empty() => return,
        Ok(message) => Notification::success(message.clone()),
        Err(error) => Notification::error(error.clone()).autohide(false),
    };
    let note = centered(note, window, cx);
    window.push_notification(note, cx);
}

/// Notifications sit at the bottom center of the window. With a sheet open, GPUI Kit keeps them to
/// the part of the window the sheet leaves free, a sliver in a window this narrow, so they are
/// placed from the window's left edge instead, still centered, over the sheet.
fn centered(note: Notification, window: &mut Window, cx: &mut App) -> Notification {
    if !window.has_active_sheet(cx) {
        return note;
    }
    let free = window.viewport_size().width - cx.theme().notification.width;
    Theme::global_mut(cx).notification.margins.left = (free / 2.).max(px(0.));
    note.placement(Anchor::BottomLeft)
}

/// Which headline is showing, ignoring its numbers.
fn mood_kind(mood: Mood) -> &'static str {
    match mood {
        Mood::Loading => "loading",
        Mood::Missing => "missing",
        Mood::Single => "single",
        Mood::Linked(_) => "linked",
        Mood::Syncing { .. } => "syncing",
        Mood::Pending { .. } => "pending",
        Mood::Waiting(_) => "waiting",
        Mood::InSync { .. } => "in-sync",
    }
}

/// A placeholder row while the accounts load.
fn skeleton_row(i: usize, cx: &App) -> AnyElement {
    h_flex()
        .gap_3()
        .px_3()
        .py_2()
        .child(Skeleton::new().size(px(24.)).rounded_full())
        .child(
            v_flex()
                .flex_1()
                .gap_1p5()
                .child(Skeleton::new().h(rems(0.8)).w(rems([9., 11., 7.5][i % 3])).rounded(cx.theme().radius))
                .child(Skeleton::new().secondary().h(rems(0.7)).w(rems(5.5)).rounded(cx.theme().radius)),
        )
        .into_any_element()
}

/// "· 84% used (2 h ago)" after an account's sessions (or first on its line, without the dot): the
/// week, tinted as it fills up, and how old the reading is once Claude has not read
/// it lately; both windows, and when Claude read them, on hover, with how to have Claude read the
/// open account's again.
fn plan_use(id: &str, usage: PlanUse, first: bool, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let color = match usage.used_fullness() {
        Fullness::Roomy => theme.muted_foreground,
        Fullness::Nearly => theme.warning,
        Fullness::Full => theme.danger,
    };
    let hint = usage.stale && usage.in_use;
    let key = match (hint, cfg!(target_os = "macos")) {
        (false, _) => "usage.tip",
        (true, true) => "usage.stale.mac",
        (true, false) => "usage.stale.other",
    };
    let tip = tf(key, &[("usage", &usage.text()), ("ago", &crate::i18n::ago(usage.at as f64))]);
    h_flex()
        .id(ElementId::Name(format!("usage-{id}").into()))
        .flex_none()
        .when(!first, |el| el.child(div().px_1().child("·")))
        .child(div().text_color(color).child(usage.used()))
        .tooltip(move |window, cx| {
            let tip = tip.clone();
            // A tooltip is laid out as narrow as its content allows, which for text is one line:
            // the hint, too long for that, gets a width of its own to wrap in.
            match hint {
                true => Tooltip::element(move |_, _| div().max_w(px(280.)).child(tip.clone())),
                false => Tooltip::new(tip),
            }
            .build(window, cx)
        })
        .into_any_element()
}

/// What comes before the time left to the weekly reset on an account's line.
#[derive(Clone, Copy, PartialEq)]
enum After {
    /// The plan's use: a bar sets them apart.
    Usage,
    /// The sessions: a dot, as between the others.
    Sessions,
    /// Nothing: it starts the line.
    Nothing,
}

/// "｜ resets in 2d 3h" after the plan's use: how long until the weekly window renews, when it was
/// given; the day and time it renews on, in this computer's time zone, on hover.
fn reset_left(id: &str, anchor: i64, after: After, cx: &App) -> AnyElement {
    let now = cc_same_core::fsx::now_secs() as i64;
    let tip = model::reset_tip(anchor, now).unwrap_or_default();
    let bar = cx.theme().muted_foreground.opacity(0.45);
    h_flex()
        .id(ElementId::Name(format!("reset-{id}").into()))
        .flex_none()
        .when(after == After::Usage, |el| el.child(div().mx_2().w(px(1.)).h(rems(0.8)).bg(bar)))
        .when(after == After::Sessions, |el| el.child(div().px_1().child("·")))
        .child(model::reset_in(anchor, now))
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        .into_any_element()
}

/// The raised surface every group of controls sits on.
pub(crate) fn surface(cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .bg(theme.group_box)
        .border_1()
        .border_color(theme.border)
        .rounded(theme.radius_lg)
        .shadow(theme::card_shadow(cx))
}

fn section_label(text: String, cx: &App) -> impl IntoElement {
    div().text_size(rems(0.846)).font_weight(FontWeight::MEDIUM).text_color(cx.theme().muted_foreground).child(text)
}

fn dot(color: Hsla) -> impl IntoElement {
    div().size(px(6.)).flex_none().rounded_full().bg(color)
}

/// Numbers in a sentence carry the news, so they get the full text color.
fn emphasize_numbers(text: String, color: Hsla) -> StyledText {
    let mut runs = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
        match (c.is_ascii_digit(), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let style =
                    HighlightStyle { color: Some(color), font_weight: Some(FontWeight::MEDIUM), ..Default::default() };
                runs.push((s..i, style));
                start = None;
            }
            _ => {}
        }
    }
    StyledText::new(text).with_highlights(runs)
}

// Two Lucide icons (ISC license) that GPUI Kit's default icon set leaves out.
const CIRCLE_ARROW_UP: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><path d="m16 12-4-4-4 4"/><path d="M12 16V8"/></svg>"#;
const SPARKLES: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M11.017 2.814a1 1 0 0 1 1.966 0l1.051 5.558a2 2 0 0 0 1.594 1.594l5.558 1.051a1 1 0 0 1 0 1.966l-5.558 1.051a2 2 0 0 0-1.594 1.594l-1.051 5.558a1 1 0 0 1-1.966 0l-1.051-5.558a2 2 0 0 0-1.594-1.594l-5.558-1.051a1 1 0 0 1 0-1.966l5.558-1.051a2 2 0 0 0 1.594-1.594z"/><path d="M20 2v4"/><path d="M22 4h-4"/><circle cx="4" cy="20" r="2"/></svg>"#;

#[derive(Clone, Copy)]
enum Tone {
    Info,
    Warning,
    Danger,
    /// A new version is available or on its way.
    Update,
    /// This version is new.
    New,
}

/// A short note with its actions, tinted by what it is about, and a close button when it can be
/// put away. It slides in once.
fn notice(
    id: &'static str,
    tone: Tone,
    text: String,
    actions: Vec<AnyElement>,
    close: Option<AnyElement>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let (icon, color) = match tone {
        Tone::Info => (Icon::new(IconName::Info), theme.info),
        Tone::Warning => (Icon::new(IconName::TriangleAlert), theme.warning),
        Tone::Danger => (Icon::new(IconName::CircleAlert), theme.danger),
        Tone::Update => (Icon::default().data(CIRCLE_ARROW_UP), theme.primary),
        Tone::New => (Icon::default().data(SPARKLES), theme.primary),
    };
    h_flex()
        .id(id)
        .relative()
        .items_start()
        .gap_3()
        .px_3p5()
        .py_3()
        .rounded(theme.radius_lg)
        .bg(color.opacity(if theme.is_dark() { 0.09 } else { 0.06 }))
        .border_1()
        .border_color(color.opacity(0.22))
        .child(div().pt(px(2.)).child(icon.small().text_color(color)))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_2p5()
                .child(div().text_size(rems(0.923)).line_height(rems(1.3)).child(text))
                .when(!actions.is_empty(), |el| el.child(h_flex().gap_2().children(actions))),
        )
        .when_some(close, |el, close| el.child(div().mt(px(-3.)).mr(px(-6.)).child(close)))
        .with_animation(
            ElementId::Name(format!("{id}-enter").into()),
            Animation::new(Duration::from_millis(320)).with_easing(ease_out_quint()),
            |el, t| el.opacity(t).top(px(-6. * (1. - t))),
        )
        .into_any_element()
}

/// "What's new" for a release: its notes from GitHub.
fn release_notes_button(release: &Release) -> AnyElement {
    let title = tf("update.notes_title", &[("version", &release.version)]);
    let notes = release.notes.clone();
    Button::new("update-notes")
        .small()
        .ghost()
        .label(t("update.whats_new"))
        .on_click(move |_, window, cx| open_notes(title.clone(), notes.clone(), window, cx))
        .into_any_element()
}

/// Release notes, in Markdown, in a dialog.
pub fn open_notes(title: String, notes: String, window: &mut Window, cx: &mut App) {
    let notes: SharedString = if notes.trim().is_empty() { t("update.no_notes").into() } else { notes.into() };
    // Headings stay below the dialog's title: a version, then its sections.
    let style = TextViewStyle {
        paragraph_gap: rems(0.75),
        heading_font_size: Some(Arc::new(|level, _| px(if level <= 2 { 15. } else { 13. }))),
        ..TextViewStyle::default()
    };
    window.open_dialog(cx, move |dialog, _, _| {
        dialog.title(title.clone()).w(px(380.)).child(
            TextView::markdown("release-notes", notes.clone())
                .style(style.clone())
                .text_size(rems(0.923))
                .selectable(true),
        )
    });
}
