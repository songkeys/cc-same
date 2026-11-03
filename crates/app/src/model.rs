//! What the window says, worked out from the engine's [`Overview`]: the headline, the accounts,
//! and how each ring in the headline sits. No drawing here; the words come from `locales/`.

use crate::i18n::{t, tf, tn};
use cc_same_core::report::{Overview, PartitionView, Warning};
use cc_same_core::{Surface, short};
use std::cmp::Reverse;
use std::time::Duration;

/// How long a pending change may wait for the background agent before we offer "Sync now".
pub const AGENT_PATIENCE: Duration = Duration::from_secs(20);
/// With the agent running, this few sessions in flight is routine mirroring (the session you
/// are using right now, say), not worth a headline.
const ROUTINE_SESSIONS: usize = 3;

/// The state the headline reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mood {
    Loading,
    /// No Claude data on this machine.
    Missing,
    /// Fewer than two accounts take part.
    Single,
    /// Another tool linked this many index folders; Claude stops saving there.
    Linked(usize),
    Syncing {
        sessions: usize,
    },
    Pending {
        sessions: usize,
        other: usize,
    },
    /// Updates that wait until Claude lets go of the account it has open.
    Waiting(usize),
    InSync {
        sessions: usize,
        accounts: usize,
    },
}

impl Mood {
    pub fn title(self) -> String {
        match self {
            Mood::Loading => t("headline.loading"),
            Mood::Missing => t("headline.missing.title"),
            Mood::Single => t("headline.single.title"),
            Mood::Linked(_) => t("headline.linked.title"),
            Mood::Syncing { .. } => t("headline.syncing.title"),
            Mood::Pending { sessions: 0, other } => tn("headline.pending.updates", other, &[]),
            Mood::Pending { sessions, .. } => tn("headline.pending.sessions", sessions, &[]),
            Mood::Waiting(_) => t("headline.waiting.title"),
            Mood::InSync { .. } => t("headline.in_sync.title"),
        }
    }

    pub fn detail(self) -> String {
        match self {
            Mood::Loading => String::new(),
            Mood::Missing => t("headline.missing.detail"),
            Mood::Single => t("headline.single.detail"),
            Mood::Linked(n) => tn("headline.linked.detail", n, &[]),
            Mood::Syncing { sessions: 0 } => t("headline.syncing.any"),
            Mood::Syncing { sessions } => tn("headline.syncing.detail", sessions, &[]),
            Mood::Pending { .. } => t("headline.pending.detail"),
            Mood::Waiting(n) => tn("headline.waiting.detail", n, &[]),
            Mood::InSync { sessions, accounts } => tn("headline.in_sync.detail", sessions, &[("accounts", &accounts)]),
        }
    }

    /// Whether every account has everything.
    pub fn settled(self) -> bool {
        matches!(self, Mood::InSync { .. })
    }

    /// Whether the user has something to do.
    pub fn needs_attention(self) -> bool {
        matches!(self, Mood::Linked(_) | Mood::Pending { .. } | Mood::Missing)
    }
}

/// Work in progress.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Busy {
    Syncing,
    Service,
    Restoring,
    Fixing,
    Saving,
    /// Claude is switching accounts: quitting, trading sign-ins, starting again.
    Switching,
}

/// An account, the way people know it. Claude keeps a session list for every organization an
/// account has been used in (signing in to another account can even leave an empty one in the
/// previous account's organization), and CC Same keeps them all the same, so an account is one
/// row that reports the list Claude shows for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    /// What the window calls it: the name it was given, else its email, else `Account 1a2b3c4d`.
    pub name: String,
    /// Its email, when CC Same could find one: what to type on Claude's sign-in page.
    pub email: Option<String>,
    pub sessions: usize,
    /// Sessions the others have and this one does not, yet.
    pub missing: usize,
    /// Claude is signed in to this account and running.
    pub open: bool,
    pub excluded: bool,
    /// One of its folders is a link; Claude cannot save there.
    pub broken: bool,
    /// Whether Claude can switch to this account.
    pub login: Login,
    /// Its number in the list of accounts to switch between; none until Claude has been seen
    /// signed in to it.
    pub number: Option<u32>,
    /// The name it was given in the list (`cc-same alias`, or the pencil beside its name).
    pub alias: Option<String>,
    /// Left out of "Next Account".
    pub sits_out: bool,
    /// What it last used of its plan, as Claude saw it.
    pub usage: Option<PlanUse>,
    /// A moment its weekly window renews, as given in the list (Unix seconds).
    pub weekly_reset: Option<i64>,
    /// Claude keeps session lists for it (an account that just joined may have none yet).
    pub has_lists: bool,
}

/// What an account last used of its plan, when Claude last read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanUse {
    /// Percent of the 5-hour window; 0 once the reading is older than the window, unless the
    /// account is in use (then the window it is filling now is unknown).
    pub five_hour: Option<u32>,
    /// Percent of the weekly window, while the reading is less than a week old.
    pub weekly: Option<u32>,
    /// When Claude read it (Unix seconds).
    pub at: i64,
    /// Older than Claude's checks of the open account: shown with its age.
    pub stale: bool,
    /// Claude is signed in to the account, so it goes on using the plan while the reading ages.
    pub in_use: bool,
}

/// How full a plan is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fullness {
    Roomy,
    /// 80% or more of a window.
    Nearly,
    /// A window is used up.
    Full,
}

impl PlanUse {
    /// `weekly_reset` is when the account's weekly window renews, when it was given: a reading
    /// from before the last renewal has started over.
    pub fn of(
        usage: &cc_same_core::usage::Usage,
        now: f64,
        in_use: bool,
        weekly_reset: Option<i64>,
    ) -> Option<PlanUse> {
        use cc_same_core::usage::{FIVE_HOURS, FRESH, last_weekly_reset};
        let percent = |p: f64| p.clamp(0., 999.).round() as u32;
        let age = now - usage.at;
        let five_hour = if in_use && age > FIVE_HOURS { None } else { usage.five_hour_at(now).map(percent) };
        let renewed = weekly_reset.is_some_and(|anchor| usage.at < last_weekly_reset(anchor, now as i64) as f64);
        let weekly = match renewed {
            true => usage.weekly.map(|_| 0),
            false => usage.weekly_at(now).map(percent),
        };
        (five_hour.is_some() || weekly.is_some()).then_some(PlanUse {
            five_hour,
            weekly,
            at: usage.at as i64,
            stale: age > FRESH,
            in_use,
        })
    }

    /// How long ago Claude read it, once that is older than its checks.
    pub fn age(&self) -> Option<String> {
        self.stale.then(|| crate::i18n::ago(self.at as f64))
    }

    /// Both windows and, once the reading is old, its age: `5h 12% · 7d 58% · 3 h ago`.
    pub fn dated(&self) -> String {
        match self.age() {
            Some(age) => format!("{} · {age}", self.text()),
            None => self.text(),
        }
    }

    /// Both windows, `5h 12% · 7d 58%`, in the interface language.
    pub fn text(&self) -> String {
        let mut parts = Vec::new();
        if let Some(p) = self.five_hour {
            parts.push(tf("usage.five_hour", &[("percent", &p)]));
        }
        if let Some(p) = self.weekly {
            parts.push(tf("usage.weekly", &[("percent", &p)]));
        }
        parts.join(" · ")
    }

    /// The week as used, and how old the reading is once it is old: `84% used (2 h ago)`. Without
    /// a reading of the week, the 5-hour window, named: `5h 12% (2 h ago)`.
    pub fn used(&self) -> String {
        let used = match (self.weekly, self.five_hour) {
            (Some(w), _) => tf("usage.used", &[("percent", &w)]),
            (None, h) => tf("usage.five_hour", &[("percent", &h.unwrap_or(0))]),
        };
        match self.age() {
            Some(ago) => tf("usage.dated", &[("usage", &used), ("ago", &ago)]),
            None => used,
        }
    }

    /// How full the window [`PlanUse::used`] shows is.
    pub fn used_fullness(&self) -> Fullness {
        match self.weekly.or(self.five_hour).unwrap_or(0) {
            100.. => Fullness::Full,
            80.. => Fullness::Nearly,
            _ => Fullness::Roomy,
        }
    }

    /// The fuller window only, `7d 58%`: the one that would stop Claude first.
    pub fn short(&self) -> String {
        match (self.five_hour, self.weekly) {
            (Some(h), Some(w)) if h > w => tf("usage.five_hour", &[("percent", &h)]),
            (_, Some(w)) => tf("usage.weekly", &[("percent", &w)]),
            (Some(h), None) => tf("usage.five_hour", &[("percent", &h)]),
            (None, None) => String::new(),
        }
    }

    pub fn fullness(&self) -> Fullness {
        match self.five_hour.max(self.weekly).unwrap_or(0) {
            100.. => Fullness::Full,
            80.. => Fullness::Nearly,
            _ => Fullness::Roomy,
        }
    }
}

/// Where an account stands for switching Claude to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Login {
    /// Switching is not available on this system.
    Unsupported,
    /// Claude is signed in to it.
    SignedIn,
    /// Its sign-in is set aside: one click switches to it. `stale` once it has not been used for
    /// about four weeks, when Claude may ask to sign in again.
    Saved { stale: bool },
    /// Signed in to before CC Same kept sign-ins: sign in to it once more to switch to it later.
    Unknown,
}

impl Account {
    /// Known by more than its ID: an email or a name.
    fn named(&self) -> bool {
        self.email.is_some() || self.alias.is_some()
    }

    /// One or two characters for the ring and the avatar.
    pub fn initials(&self) -> String {
        if self.named() {
            self.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()
        } else {
            self.id.chars().take(2).collect::<String>().to_uppercase()
        }
    }

    pub fn detail(&self) -> String {
        if self.broken {
            return t("account.broken");
        }
        if self.excluded {
            return t("account.excluded");
        }
        if !self.has_lists {
            return t("account.no_lists");
        }
        let mut parts = vec![tn("account.sessions", self.sessions, &[])];
        if self.missing > 0 {
            parts.push(tf("account.to_copy", &[("count", &self.missing)]));
        }
        parts.join(" · ")
    }

    /// Something of its plan to show: what it used, or when its week renews.
    pub fn plan_known(&self) -> bool {
        !self.broken && !self.excluded && self.has_lists && (self.usage.is_some() || self.weekly_reset.is_some())
    }

    pub fn ring(&self) -> Placement {
        if self.broken {
            Placement::Broken
        } else if self.excluded {
            Placement::Apart
        } else if self.missing > 0 {
            Placement::Behind
        } else {
            Placement::Joined
        }
    }
}

/// Where a ring sits relative to the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Has everything: overlaps the others fully.
    Joined,
    /// Still missing sessions: drifts out a little until it catches up.
    Behind,
    /// Excluded from the sync: set apart and faded.
    Apart,
    /// Claude cannot save to it.
    Broken,
}

/// One row per account, labelled with an email when we know it: the accounts in the list by
/// number, then the ones only seen in Claude's session folders.
pub fn accounts(ov: &Overview) -> Vec<Account> {
    let logins = &ov.logins;
    let login = |acct: &str| match logins.saved(acct) {
        _ if !logins.supported => Login::Unsupported,
        _ if logins.signed_in.as_deref() == Some(acct) => Login::SignedIn,
        Some(saved) => Login::Saved { stale: saved.stale() },
        None => Login::Unknown,
    };
    let now = cc_same_core::fsx::now_secs();
    let mut groups: Vec<Vec<&PartitionView>> = Vec::new();
    for p in ov.surface(Surface::Code).map(|c| c.partitions.as_slice()).unwrap_or_default() {
        match groups.iter_mut().find(|g| g[0].part.acct == p.part.acct) {
            Some(group) => group.push(p),
            None => groups.push(vec![p]),
        }
    }
    let row = |acct: &str, lists: &[&PartitionView]| {
        let slot = ov.roster.slot(acct);
        // The list Claude shows for the account: the one it has open, else the fullest.
        let shown = lists.iter().copied().max_by_key(|p| {
            let open = ov.app.showing(&p.part);
            (!p.excluded, open, p.sessions, Reverse(p.missing), Reverse(p.part.org.as_str()))
        });
        let email = lists
            .iter()
            .find_map(|p| p.email.clone())
            .or_else(|| slot.and_then(|s| s.email.clone()))
            .or_else(|| logins.saved(acct).and_then(|s| s.email.clone()));
        let alias = slot.and_then(|s| s.alias.clone());
        let weekly_reset = slot.and_then(|s| s.weekly_reset);
        let open = ov.app.open_account.as_deref() == Some(acct);
        Account {
            name: alias
                .clone()
                .or_else(|| email.clone())
                .unwrap_or_else(|| tf("account.unnamed", &[("id", &short(acct))])),
            email,
            sessions: shown.map_or(0, |p| p.sessions),
            missing: shown.filter(|p| !p.excluded).map_or(0, |p| p.missing),
            open,
            excluded: !lists.is_empty() && lists.iter().all(|p| p.excluded),
            broken: lists.iter().any(|p| p.error.is_some() || p.part.is_link),
            login: login(acct),
            number: slot.map(|s| s.number),
            alias,
            sits_out: slot.is_some_and(|s| s.disabled),
            usage: ov.usage.get(acct).and_then(|u| PlanUse::of(u, now, open, weekly_reset)),
            weekly_reset,
            has_lists: !lists.is_empty(),
            id: acct.to_string(),
        }
    };
    let mut rows: Vec<Account> = groups.iter().map(|lists| row(&lists[0].part.acct, lists)).collect();
    for slot in &ov.roster.slots {
        if !rows.iter().any(|r| r.id == slot.account) {
            rows.push(row(&slot.account, &[]));
        }
    }
    // The list by number; then named accounts, the ones we only know by id, and excluded ones.
    rows.sort_by(|a, b| {
        let key =
            |r: &Account| (r.number.is_none(), r.number, r.excluded, !r.named(), r.name.to_lowercase(), r.id.clone());
        key(a).cmp(&key(b))
    });
    rows
}

/// What keeps `text` from being an account's name, given the names the other accounts go by
/// (`(name, account)` pairs); `None` when it can be saved. Empty text takes the name away.
pub fn name_problem(text: &str, others: &[(String, String)]) -> Option<String> {
    use cc_same_core::accounts::{ALIAS_MAX, normalize_alias};
    if text.trim().is_empty() {
        return None;
    }
    match normalize_alias(text) {
        Err(_) => Some(tf("name.invalid", &[("max", &ALIAS_MAX)])),
        Ok(name) => {
            others.iter().find(|(n, _)| *n == name).map(|(_, account)| tf("name.taken", &[("account", account)]))
        }
    }
}

/// "resets in 3d 4h": how long until the weekly window that renews at `anchor` renews, to the hour.
pub fn reset_in(anchor: i64, now: i64) -> String {
    let hours = (cc_same_core::usage::next_weekly_reset(anchor, now) - now) / 3600;
    match hours {
        0 => t("reset.in.soon"),
        1..=23 => tf("reset.in.hours", &[("hours", &hours)]),
        _ => tf("reset.in.days", &[("days", &(hours / 24)), ("hours", &(hours % 24))]),
    }
}

/// The weekday (0 for Monday) and time, `06:00`, the weekly window renews on in this computer's
/// time zone as it is now.
pub fn reset_local(anchor: i64, now: i64) -> Option<(usize, String)> {
    let (day, hour, minute) = cc_same_core::usage::next_weekly_reset_here(anchor, now)?;
    Some((day, format!("{hour:02}:{minute:02}")))
}

/// A weekday's short name, from 0 for Monday.
pub fn weekday_name(day: usize) -> String {
    t("date.weekdays").split(',').nth(day).unwrap_or_default().to_string()
}

/// "Resets every Fri at 06:00".
pub fn reset_tip(anchor: i64, now: i64) -> Option<String> {
    let (day, time) = reset_local(anchor, now)?;
    Some(tf("reset.tip", &[("day", &weekday_name(day)), ("time", &time)]))
}

/// The renewal the dialog gives, as a moment to keep: none when the time is left empty, else the
/// complaint to show.
pub fn reset_from(day: Option<usize>, time: &str, now: i64) -> Result<Option<i64>, String> {
    if time.trim().is_empty() {
        return Ok(None);
    }
    let Some((hour, minute)) = parse_clock(time) else { return Err(t("reset.bad_time")) };
    let Some(day) = day else { return Err(t("reset.no_day")) };
    cc_same_core::usage::weekly_anchor_here(day, hour, minute, now).map(Some).ok_or_else(|| t("reset.bad_time"))
}

/// A time of day as people type it: `6`, `06:00`, `18:30`, `6.30`, `6pm`, `6:30 am`.
pub fn parse_clock(text: &str) -> Option<(u32, u32)> {
    let text = text.trim().to_lowercase().replace('：', ":").replace(' ', "");
    let (text, half) = match (text.strip_suffix("am"), text.strip_suffix("pm")) {
        (Some(t), _) => (t.to_string(), Some(false)),
        (_, Some(t)) => (t.to_string(), Some(true)),
        _ => (text, None),
    };
    let (h, m) = text.split_once([':', '.']).unwrap_or((&text, "0"));
    let all_digits = |s: &str| !s.is_empty() && s.len() <= 2 && s.bytes().all(|b| b.is_ascii_digit());
    if !all_digits(h) || !all_digits(m) {
        return None;
    }
    let (mut hour, minute): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    if let Some(pm) = half {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour = hour % 12 + if pm { 12 } else { 0 };
    }
    (hour < 24 && minute < 60).then_some((hour, minute))
}

pub fn agent_alive(ov: &Overview) -> bool {
    ov.service.installed && (ov.heartbeat.as_ref().is_some_and(|h| h.is_fresh()) || ov.service.running == Some(true))
}

/// The headline. `pending_for` is how long the current plan has been waiting.
pub fn mood(ov: Option<&Overview>, syncing: bool, pending_for: Option<Duration>) -> Mood {
    let Some(ov) = ov else { return Mood::Loading };
    if syncing {
        return Mood::Syncing { sessions: ov.plan.sessions };
    }
    if ov.warnings.iter().any(|w| matches!(w, Warning::DesktopDataMissing { .. })) {
        return Mood::Missing;
    }
    if let Some(n) = ov.warnings.iter().find_map(|w| match w {
        Warning::SymlinkedFolders { count, .. } => Some(*count),
        _ => None,
    }) {
        return Mood::Linked(n);
    }
    let accounts = accounts(ov);
    let taking_part = accounts.iter().filter(|a| a.has_lists && !a.excluded && !a.broken).count();
    // Two lists are enough to keep in step, even two organizations of one account.
    let lists = ov.surface(Surface::Code).map_or(0, |code| {
        code.partitions.iter().filter(|p| !p.excluded && p.error.is_none() && !p.part.is_link).count()
    });
    if lists < 2 {
        return Mood::Single;
    }
    let union = ov.surface(Surface::Code).map(|s| s.union).unwrap_or(0);
    if ov.plan.total > 0 {
        let sessions = ov.plan.sessions;
        let fresh = pending_for.is_none_or(|t| t < AGENT_PATIENCE);
        if agent_alive(ov) && fresh {
            // The agent is on it; a handful in flight is just mirroring.
            return if sessions <= ROUTINE_SESSIONS {
                Mood::InSync { sessions: union, accounts: taking_part }
            } else {
                Mood::Syncing { sessions }
            };
        }
        let writes: usize = ov
            .plan
            .per_partition
            .iter()
            .flat_map(|(_, kinds)| kinds.iter())
            .filter(|(k, _)| k.ends_with("sessions"))
            .map(|(_, n)| *n)
            .sum();
        return Mood::Pending { sessions, other: ov.plan.total.saturating_sub(writes) };
    }
    let deferred: usize = ov.plan.deferred.values().sum();
    if deferred > 0 { Mood::Waiting(deferred) } else { Mood::InSync { sessions: union, accounts: taking_part } }
}

/// `20260928-225026-baseline` → `Sep 28, 22:50` in the interface language.
pub fn snapshot_when(id: &str) -> String {
    let (Some(m), Some(d), Some(hh), Some(mm)) = (id.get(4..6), id.get(6..8), id.get(9..11), id.get(11..13)) else {
        return id.to_string();
    };
    match (m.parse::<usize>(), d.parse::<u32>()) {
        (Ok(m), Ok(d)) if (1..=12).contains(&m) => crate::i18n::short_date(m, d, &format!("{hh}:{mm}")),
        _ => id.to_string(),
    }
}

pub fn snapshot_reason(reason: &str, baseline: bool) -> String {
    t(if baseline {
        "snapshot.first"
    } else {
        match reason {
            "baseline" => "snapshot.first",
            "pre-restore" => "snapshot.pre_restore",
            "watch" => "snapshot.automatic",
            _ => "snapshot.manual",
        }
    })
}

/// How long transcripts are kept, for display.
pub fn retention_label(days: Option<f64>) -> String {
    match days {
        Some(d) if d >= 3650.0 => tn("period.years", (d / 365.0).round() as usize, &[]),
        Some(d) => tn("period.days", d as usize, &[]),
        None => tn("period.days", 30, &[]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(name: &str, has_email: bool) -> Account {
        Account {
            id: "9e31ea7e-0000-0000-0000-000000000000".into(),
            name: name.into(),
            email: has_email.then(|| name.into()),
            sessions: 209,
            missing: 0,
            open: false,
            excluded: false,
            broken: false,
            login: Login::Unknown,
            number: None,
            alias: None,
            sits_out: false,
            usage: None,
            weekly_reset: None,
            has_lists: true,
        }
    }

    const ADA: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const BOB: &str = "bbbbbbbb-0000-4000-8000-000000000002";
    const ADA_ORG: &str = "0a0a0a0a-0000-4000-8000-000000000001";
    const BOB_ORG: &str = "0b0b0b0b-0000-4000-8000-000000000002";

    /// As seen on a Windows machine: each account has a list in both organizations, the one in
    /// the other's organization empty (signing in there reused the organization Claude still
    /// remembered). Claude runs, signed in to Bob; `open_org` is what its log says.
    fn two_accounts_in_two_orgs(open_org: Option<&str>) -> Overview {
        use cc_same_core::report::{PlanView, SurfaceView};
        use cc_same_core::{AppState, Config, Partition};
        let list = |acct: &str, org: &str, sessions: usize, missing: usize| PartitionView {
            part: Partition {
                surface: Surface::Code,
                acct: acct.into(),
                org: org.into(),
                path: Default::default(),
                is_link: false,
            },
            email: Some(format!("{}@example.com", if acct == ADA { "ada" } else { "bob" })),
            sessions,
            archived: 0,
            markers: 0,
            missing,
            unreadable: 0,
            loaded: true,
            excluded: false,
            error: None,
            collections: Vec::new(),
            unmanaged: Vec::new(),
        };
        let partitions = vec![
            list(ADA, ADA_ORG, 95, 106),
            list(ADA, BOB_ORG, 0, 201),
            list(BOB, ADA_ORG, 0, 201),
            list(BOB, BOB_ORG, 106, 95),
        ];
        Overview {
            desktop_version: None,
            app: AppState {
                running: true,
                open_account: Some(BOB.into()),
                open_org: open_org.map(str::to_string),
                ..AppState::default()
            },
            labels: Default::default(),
            surfaces: vec![SurfaceView { surface: Surface::Code, partitions, union: 201 }],
            plan: PlanView::default(),
            warnings: Vec::new(),
            retention: Default::default(),
            logins: cc_same_core::logins::Logins { supported: false, signed_in: Some(BOB.into()), saved: Vec::new() },
            roster: Default::default(),
            usage: Default::default(),
            service: Default::default(),
            heartbeat: None,
            last_sync: None,
            baseline: None,
            pending_restart: None,
            config: Config::default(),
        }
    }

    #[test]
    fn one_row_per_account() {
        let _locale = crate::i18n::TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        // Whether or not Claude's log names the organization, only the signed-in account is open,
        // and each row reports the list that account uses.
        for open_org in [Some(BOB_ORG), None] {
            let ov = two_accounts_in_two_orgs(open_org);
            let rows: Vec<_> = accounts(&ov).into_iter().map(|a| (a.name, a.sessions, a.missing, a.open)).collect();
            assert_eq!(rows, [("ada@example.com".into(), 95, 106, false), ("bob@example.com".into(), 106, 95, true)]);
            assert_eq!(mood(Some(&ov), false, None), Mood::InSync { sessions: 201, accounts: 2 });
        }
        // The organization Claude has open wins over a fuller list.
        let mut ov = two_accounts_in_two_orgs(Some(ADA_ORG));
        ov.app.open_account = Some(BOB.into());
        assert_eq!(accounts(&ov)[1].sessions, 0);
        // Claude closed: nothing is open.
        ov.app.running = false;
        ov.app.open_account = None;
        assert!(accounts(&ov).iter().all(|a| !a.open));
        // One account in two organizations still has two lists to keep in step.
        let mut ov = two_accounts_in_two_orgs(None);
        ov.surfaces[0].partitions.retain(|p| p.part.acct == BOB);
        assert_eq!(mood(Some(&ov), false, None), Mood::InSync { sessions: 201, accounts: 1 });
    }

    #[test]
    fn a_name_takes_the_place_of_the_email() {
        use cc_same_core::accounts::{Roster, Slot};
        let _locale = crate::i18n::TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        const CY: &str = "cccccccc-0000-4000-8000-000000000003";
        let mut ov = two_accounts_in_two_orgs(None);
        let slot = |number, account: &str, email: Option<&str>, alias: &str| Slot {
            number,
            account: account.into(),
            email: email.map(str::to_string),
            alias: Some(alias.into()),
            ..Slot::default()
        };
        ov.roster = Roster {
            version: 1,
            slots: vec![slot(1, BOB, Some("bob@example.com"), "home"), slot(2, CY, None, "工作")],
        };
        let rows = accounts(&ov);
        let named: Vec<_> = rows.iter().map(|a| (a.name.as_str(), a.initials(), a.email.as_deref())).collect();
        assert_eq!(named[..2], [("home", "H".into(), Some("bob@example.com")), ("工作", "工".into(), None)]);
    }

    #[test]
    fn a_weekly_reset_is_typed_as_a_day_and_a_time() {
        let _locale = crate::i18n::TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        for (typed, read) in
            [("6", (6, 0)), ("06:00", (6, 0)), ("18:30", (18, 30)), ("6.30", (6, 30)), ("6pm", (18, 0))]
        {
            assert_eq!(parse_clock(typed), Some(read), "{typed}");
        }
        assert_eq!(parse_clock("12 am"), Some((0, 0)));
        assert_eq!(parse_clock("9：05"), Some((9, 5)));
        for bad in ["", "24:00", "6:60", "13pm", "six", "6:0:0"] {
            assert_eq!(parse_clock(bad), None, "{bad}");
        }
        let now = 1_793_188_800;
        assert_eq!(reset_from(None, "  ", now), Ok(None), "no time, no reset");
        assert_eq!(reset_from(None, "6:00", now), Err("Pick the day it resets.".into()));
        assert!(reset_from(Some(4), "25:00", now).is_err());
        let anchor = reset_from(Some(4), "6:00", now).unwrap().unwrap();
        // Read back on this computer as it was given.
        assert_eq!(reset_local(anchor, now), Some((4, "06:00".into())));
        assert_eq!(reset_tip(anchor, now).as_deref(), Some("The weekly limit resets every Fri at 06:00"));
    }

    #[test]
    fn the_time_left_reads_to_the_hour() {
        let _locale = crate::i18n::TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        let now = 1_000_000;
        assert_eq!(reset_in(now + 3 * 86_400 + 4 * 3600 + 59 * 60, now), "resets in 3d 4h");
        assert_eq!(reset_in(now + 5 * 3600 + 10, now), "resets in 5h");
        assert_eq!(reset_in(now + 59 * 60, now), "resets within the hour");
        // Weeks on, it is the next renewal that counts.
        assert_eq!(reset_in(now - 14 * 86_400 + 3600, now), "resets in 1h");
        rust_i18n::set_locale("zh-CN");
        assert_eq!(reset_in(now + 2 * 86_400 + 3600, now), "2天1小时后重置");
    }

    #[test]
    fn the_plan_reads_as_the_share_used_and_how_old_that_is() {
        let _locale = crate::i18n::TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        let now = cc_same_core::fsx::now_secs();
        let read = |age: f64, h, w| cc_same_core::usage::Usage { at: now - age, five_hour: h, weekly: w };
        let fresh = PlanUse::of(&read(60.0, Some(12.0), Some(84.0)), now, true, None).unwrap();
        assert_eq!(fresh.used(), "84% used");
        // It is the week, even with the 5-hour window fuller, and tinted by the week.
        let old = PlanUse::of(&read(2.0 * 3600.0, Some(97.0), Some(40.0)), now, false, None).unwrap();
        assert_eq!((old.used().as_str(), old.used_fullness()), ("40% used (2 h ago)", Fullness::Roomy));
        // Without a reading of the week, the 5-hour window says which it is.
        let hours = PlanUse::of(&read(60.0, Some(30.0), None), now, true, None).unwrap();
        assert_eq!(hours.used(), "5h 30%");
        rust_i18n::set_locale("zh-CN");
        assert_eq!(old.used(), "40%已用（2 小时前）");
    }

    #[test]
    fn a_reading_from_before_the_weekly_reset_has_started_over() {
        let now = 1_000_000.0;
        let read = |age: f64| cc_same_core::usage::Usage { at: now - age, five_hour: Some(0.0), weekly: Some(84.0) };
        // The week renewed an hour ago.
        let anchor = Some(now as i64 - 3600);
        assert_eq!(PlanUse::of(&read(2.0 * 3600.0), now, false, anchor).unwrap().weekly, Some(0));
        assert_eq!(PlanUse::of(&read(1800.0), now, false, anchor).unwrap().weekly, Some(84));
        assert_eq!(PlanUse::of(&read(2.0 * 3600.0), now, false, None).unwrap().weekly, Some(84));
    }

    #[test]
    fn a_name_is_checked_before_it_is_saved() {
        let _locale = crate::i18n::TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        let others = [("home".to_string(), "bob@example.com".to_string())];
        assert_eq!(name_problem("  ", &others), None, "empty takes the name away");
        assert_eq!(name_problem("Studio", &others), None);
        assert_eq!(name_problem("Home", &others).as_deref(), Some("bob@example.com already goes by this name."));
        assert!(name_problem("my work", &others).is_some_and(|p| p.contains("no spaces")));
    }

    #[test]
    fn the_list_comes_first_in_number_order() {
        use cc_same_core::accounts::{Roster, Slot};
        let _locale = crate::i18n::TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        const CY: &str = "cccccccc-0000-4000-8000-000000000003";
        let mut ov = two_accounts_in_two_orgs(None);
        let slot = |number, account: &str, alias: Option<&str>| Slot {
            number,
            account: account.into(),
            email: Some(format!("{}@example.com", &account[..1])),
            alias: alias.map(str::to_string),
            ..Slot::default()
        };
        // Bob and Cy are in the list (Cy has no session list yet); Ada only has her sessions.
        ov.roster = Roster { version: 1, slots: vec![slot(1, BOB, Some("home")), slot(2, CY, None)] };
        let now = cc_same_core::fsx::now_secs();
        let read = cc_same_core::usage::Usage { at: now - 60.0, five_hour: Some(97.0), weekly: Some(40.0) };
        ov.usage = [(BOB.to_string(), read)].into();
        let rows = accounts(&ov);
        let shown: Vec<_> = rows.iter().map(|a| (a.name.as_str(), a.number, a.has_lists)).collect();
        assert_eq!(
            shown,
            [("home", Some(1), true), ("c@example.com", Some(2), false), ("ada@example.com", None, true)]
        );
        assert_eq!(rows[0].alias.as_deref(), Some("home"));
        assert_eq!(rows[1].detail(), "No sessions yet");
        let usage = rows[0].usage.unwrap();
        assert_eq!((usage.short().as_str(), usage.text().as_str()), ("5h 97%", "5h 97% · 7d 40%"));
        assert_eq!(usage.fullness(), Fullness::Nearly);
        // The headline still counts the accounts whose sessions are kept in step.
        assert_eq!(mood(Some(&ov), false, None), Mood::InSync { sessions: 201, accounts: 2 });
    }

    #[test]
    fn plan_use_follows_the_windows() {
        let _locale = crate::i18n::TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        let now = 1_000_000.0;
        let read = |age: f64, five_hour, weekly| cc_same_core::usage::Usage { at: now - age, five_hour, weekly };
        let fresh = PlanUse::of(&read(60.0, Some(12.4), Some(58.0)), now, true, None).unwrap();
        assert_eq!((fresh.short().as_str(), fresh.fullness()), ("7d 58%", Fullness::Roomy));
        assert!(!fresh.stale && fresh.age().is_none());
        // A day on, the 5-hour window has started over; a full week shows as full.
        let old = PlanUse::of(&read(86_400.0, Some(80.0), Some(100.0)), now, false, None).unwrap();
        assert_eq!((old.text().as_str(), old.fullness()), ("5h 0% · 7d 100%", Fullness::Full));
        assert!(old.stale && old.age().is_some());
        // Unless Claude is signed in to it: then the window it is filling now is unknown.
        let in_use = PlanUse::of(&read(35.0 * 3600.0, Some(0.0), Some(69.0)), now, true, None).unwrap();
        assert_eq!((in_use.text().as_str(), in_use.stale), ("7d 69%", true));
        assert_eq!(PlanUse::of(&read(4.0 * 3600.0, Some(20.0), None), now, true, None).unwrap().text(), "5h 20%");
        // Claude checks every 15 minutes while it checks at all.
        assert!(!PlanUse::of(&read(20.0 * 60.0, Some(5.0), None), now, true, None).unwrap().stale);
        // Older than a week, only the 5-hour window is known (to have started over).
        assert_eq!(PlanUse::of(&read(8.0 * 86_400.0, None, Some(90.0)), now, false, None), None);
        rust_i18n::set_locale("zh-CN");
        assert_eq!(fresh.text(), "5 小时 12% · 7 天 58%");
        rust_i18n::set_locale("en");
    }

    #[test]
    fn headline_copy() {
        let _locale = crate::i18n::TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        assert_eq!(Mood::InSync { sessions: 209, accounts: 3 }.detail(), "209 sessions in each of your 3 accounts");
        assert_eq!(Mood::Pending { sessions: 1, other: 0 }.title(), "1 session to copy");
        assert_eq!(Mood::Pending { sessions: 0, other: 2 }.title(), "2 updates to apply");
        assert_eq!(Mood::Waiting(1).detail(), "1 update lands when you switch accounts or quit Claude.");
        assert_eq!(Mood::Waiting(4).detail(), "4 updates land when you switch accounts or quit Claude.");
    }

    #[test]
    fn rows_describe_themselves() {
        let _locale = crate::i18n::TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        let mut a = account("i@example.com", true);
        assert_eq!(a.initials(), "I");
        assert_eq!(a.detail(), "209 sessions");
        assert_eq!(a.ring(), Placement::Joined);
        a.missing = 3;
        assert_eq!(a.detail(), "209 sessions · 3 to copy");
        assert_eq!(a.ring(), Placement::Behind);
        a.excluded = true;
        assert_eq!((a.detail().as_str(), a.ring()), ("Kept separate", Placement::Apart));
        let b = account("Account 9e31ea7e", false);
        assert_eq!(b.initials(), "9E");
    }

    #[test]
    fn snapshot_labels() {
        let _locale = crate::i18n::TEST_LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        assert_eq!(snapshot_when("20260928-225026-baseline"), "Sep 28, 22:50");
        assert_eq!(snapshot_when("garbage"), "garbage");
        assert_eq!(snapshot_reason("watch", false), "Automatic");
        assert_eq!(snapshot_reason("watch", true), "Before the first sync");
        assert_eq!(retention_label(Some(3650.0)), "10 years");
        assert_eq!(retention_label(None), "30 days");
    }
}
