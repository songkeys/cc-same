//! The accounts CC Same switches Claude between: a numbered list, the way claude-swap keeps one.
//!
//! An account joins the list by itself the first time Claude is seen signed in to it, or when a
//! sign-in is kept for it, and keeps its number until it is removed. It can have a short alias,
//! sit out of the rotation, or move to another number. Switching without naming an account goes
//! to the next one in the list; with a strategy, to the one with the most of its plan left, as
//! Claude last saw it ([`crate::usage`]). The switching itself is [`crate::logins`]'s.

use crate::apply::Lock;
use crate::ctx::Ctx;
use crate::paths::Paths;
use crate::{desktop, fsx, logins, scan, usage};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::time::Duration;

/// One account in the list.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Slot {
    /// Its place in the list, from 1.
    pub number: u32,
    /// The account's UUID, as Claude names it.
    pub account: String,
    pub email: Option<String>,
    /// A short name to use instead of the number or the email.
    pub alias: Option<String>,
    /// Sits out of the rotation; switching to it by name still works.
    pub disabled: bool,
    /// When it joined (Unix seconds, whole ones: they read back from JSON exactly as written).
    pub added_at: f64,
    /// Organizations Claude was seen using it with, to tell its plan usage apart.
    pub orgs: Vec<String>,
}

impl Slot {
    /// The email, or `Account 1a2b3c4d` when it is not known.
    pub fn label(&self) -> String {
        self.email.clone().unwrap_or_else(|| format!("Account {}", crate::short(&self.account)))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Roster {
    pub version: u32,
    /// In number order.
    pub slots: Vec<Slot>,
}

impl Roster {
    /// The list as saved; empty when there is none (or it cannot be read).
    pub fn load(paths: &Paths) -> Roster {
        read(paths).unwrap_or_default()
    }

    pub fn slot(&self, account: &str) -> Option<&Slot> {
        self.slots.iter().find(|s| s.account == account)
    }

    fn slot_mut(&mut self, account: &str) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|s| s.account == account)
    }

    fn next_number(&self) -> u32 {
        self.slots.iter().map(|s| s.number).max().unwrap_or(0) + 1
    }

    /// Add `account` at the end, unless it is in the list already.
    fn join(&mut self, account: &str, email: Option<String>, now: f64) {
        if self.slot(account).is_none() && crate::is_uuid(account) {
            let number = self.next_number();
            let slot = Slot { number, account: account.into(), email, added_at: now.trunc(), ..Slot::default() };
            self.slots.push(slot);
        }
    }

    fn sort(&mut self) {
        self.slots.sort_by_key(|s| s.number);
    }

    /// The one account `query` names: its number, alias, email, or the start of its ID.
    pub fn find(&self, query: &str) -> Result<&Slot, NotFound> {
        let query = query.trim();
        if let Ok(number) = query.parse::<u32>() {
            return self.slots.iter().find(|s| s.number == number).ok_or_else(|| NotFound::None(query.into()));
        }
        let lower = query.to_lowercase();
        let hits: Vec<&Slot> = self
            .slots
            .iter()
            .filter(|s| {
                s.alias.as_deref() == Some(lower.as_str())
                    || s.email.as_ref().is_some_and(|e| e.eq_ignore_ascii_case(query))
                    || (lower.len() >= 4 && s.account.starts_with(&lower))
            })
            .collect();
        match hits.as_slice() {
            [one] => Ok(one),
            [] => Err(NotFound::None(query.into())),
            many => Err(NotFound::Several(many.iter().map(|s| s.label()).collect())),
        }
    }
}

fn read(paths: &Paths) -> Result<Roster, ReadError> {
    let raw = match fs::read(paths.roster_file()) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Roster::default()),
        Err(_) => return Err(ReadError::Io),
    };
    let mut roster: Roster = serde_json::from_slice(&raw).map_err(|_| ReadError::Unreadable)?;
    roster.sort();
    Ok(roster)
}

enum ReadError {
    Io,
    /// Not JSON we understand: someone's edit gone wrong (ours are written whole or not at all).
    Unreadable,
}

/// Change the list under its lock, starting from what is saved now, and save it if it changed.
fn edit<T>(paths: &Paths, change: impl FnOnce(&mut Roster) -> Result<T>) -> Result<(T, Roster)> {
    let _lock = Lock::acquire_at(&paths.state_dir.join("accounts.lock"), Duration::from_secs(5))?;
    let mut roster = match read(paths) {
        Ok(roster) => roster,
        Err(ReadError::Io) => anyhow::bail!("cannot read {}", paths.roster_file().display()),
        Err(ReadError::Unreadable) => {
            // Keep it, out of the way, rather than overwrite it.
            let aside = fsx::unique_path(paths.roster_file().with_extension("json.unreadable"));
            fs::rename(paths.roster_file(), &aside).context("setting an unreadable accounts.json aside")?;
            Roster::default()
        }
    };
    let before = roster.clone();
    let out = change(&mut roster)?;
    roster.sort();
    if roster.slots != before.slots {
        roster.version = 1;
        fsx::create_private_dir_all(&paths.state_dir)?;
        let mut body = serde_json::to_vec_pretty(&roster)?;
        body.push(b'\n');
        fsx::atomic_write(&paths.roster_file(), &body, None, false)?;
    }
    Ok((out, roster))
}

/// Bring the list up to date with what Claude shows, and return it. The account Claude is signed
/// in to and every account with a sign-in kept join, emails are filled in, and the organizations
/// the signed-in account was seen using are noted. Writes only when something changed.
pub fn observe(ctx: &Ctx) -> Roster {
    let paths = &ctx.paths;
    let now = fsx::now_secs();
    let signed_in = desktop::last_known_account(ctx);
    let kept = logins::list(ctx).saved;
    let labels = scan::account_labels(ctx);
    // Samples Claude took after the signed-in account loaded can only be about its organizations.
    let seen: BTreeSet<String> = match (&signed_in, desktop::last_init(ctx)) {
        (Some(account), Some(init)) if init.acct == *account => {
            init.at.map(|at| usage::orgs_since(&usage::read(paths), at)).unwrap_or_default()
        }
        _ => BTreeSet::new(),
    };
    let update = |roster: &mut Roster| {
        if let Some(account) = &signed_in {
            roster.join(account, labels.get(account).cloned(), now);
        }
        for saved in &kept {
            roster.join(&saved.account, saved.email.clone().or_else(|| labels.get(&saved.account).cloned()), now);
        }
        for slot in &mut roster.slots {
            if let Some(email) = labels.get(&slot.account) {
                if slot.email.as_ref() != Some(email) {
                    slot.email = Some(email.clone());
                }
            }
        }
        if let Some(slot) = signed_in.as_deref().and_then(|a| roster.slot_mut(a)) {
            for org in &seen {
                if !slot.orgs.contains(org) {
                    slot.orgs.push(org.clone());
                }
            }
        }
    };
    let saved = Roster::load(paths);
    let mut wanted = saved.clone();
    update(&mut wanted);
    if wanted == saved {
        return saved;
    }
    match edit(paths, |roster| {
        update(roster);
        Ok(())
    }) {
        Ok(((), roster)) => roster,
        Err(e) => {
            ctx.debug(format!("updating the account list: {e:#}"));
            wanted
        }
    }
}

/// What each account last used of its plan, as far as Claude's record tells: the ones in the
/// list, and the ones only seen in Claude's session folders.
pub fn usage(ctx: &Ctx, roster: &Roster) -> BTreeMap<String, usage::Usage> {
    let folders: Vec<(String, String)> =
        crate::Surface::ALL.into_iter().flat_map(|s| scan::discover(ctx, s)).map(|p| (p.acct, p.org)).collect();
    let owners = usage::owners(
        roster.slots.iter().map(|s| (s.account.as_str(), s.orgs.as_slice())),
        folders.iter().map(|(a, o)| (a.as_str(), o.as_str())),
    );
    usage::latest(&usage::read(&ctx.paths), &owners)
}

/// Why an account could not be found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotFound {
    None(String),
    /// These accounts all match.
    Several(Vec<String>),
}

impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotFound::None(query) => write!(f, "no account called {query} (see `cc-same accounts`)"),
            NotFound::Several(names) => write!(f, "that could be {}", names.join(" or ")),
        }
    }
}

impl std::error::Error for NotFound {}

/// Why a change to the list was not made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Claude is signed in to it, so it would join again right away.
    SignedIn,
    /// No other account in the rotation has a sign-in kept.
    NoOther,
    /// Letters and digits of any script, `-`, `_` and `.`, at most [`ALIAS_MAX`] of them, not only
    /// digits, not starting with `-`.
    BadAlias(String),
    AliasTaken {
        alias: String,
        by: String,
    },
    BadNumber,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::SignedIn => f.write_str("Claude is signed in to that account; switch to another one first"),
            Refused::NoOther => {
                f.write_str("no other account to switch to: sign in to one in Claude first (`cc-same add`)")
            }
            Refused::BadAlias(alias) => write!(
                f,
                "{alias} cannot be an alias: use up to {ALIAS_MAX} letters, digits, `-`, `_` or `.`, not only digits, not starting with `-`"
            ),
            Refused::AliasTaken { alias, by } => write!(f, "{by} is already called {alias}"),
            Refused::BadNumber => f.write_str("numbers start at 1"),
        }
    }
}

impl std::error::Error for Refused {}

/// The longest alias, in characters.
pub const ALIAS_MAX: usize = 24;

/// An alias as it is kept: lowercase, checked.
pub fn normalize_alias(alias: &str) -> Result<String, Refused> {
    let alias = alias.trim().to_lowercase();
    let fine = !alias.is_empty()
        && alias.chars().count() <= ALIAS_MAX
        && !alias.starts_with('-')
        && !alias.chars().all(|c| c.is_ascii_digit())
        && alias.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if fine {
        Ok(alias)
    } else {
        Err(Refused::BadAlias(alias))
    }
}

/// Give an account an alias, or take it away (`None`).
pub fn set_alias(ctx: &Ctx, account: &str, alias: Option<&str>) -> Result<Roster> {
    let alias = alias.map(normalize_alias).transpose()?;
    let ((), roster) = edit(&ctx.paths, |roster| {
        if let Some(alias) = &alias {
            if let Some(other) = roster.slots.iter().find(|s| s.alias.as_ref() == Some(alias) && s.account != account) {
                return Err(Refused::AliasTaken { alias: alias.clone(), by: other.label() }.into());
            }
        }
        if let Some(slot) = roster.slot_mut(account) {
            slot.alias = alias.clone();
        }
        Ok(())
    })?;
    Ok(roster)
}

/// Take an account out of the rotation, or back in.
pub fn set_disabled(ctx: &Ctx, account: &str, disabled: bool) -> Result<Roster> {
    let ((), roster) = edit(&ctx.paths, |roster| {
        if let Some(slot) = roster.slot_mut(account) {
            slot.disabled = disabled;
        }
        Ok(())
    })?;
    Ok(roster)
}

/// Give an account another number; the account that had it takes the old one. Returns that
/// account.
pub fn move_to(ctx: &Ctx, account: &str, number: u32) -> Result<Option<String>> {
    if number == 0 {
        return Err(Refused::BadNumber.into());
    }
    let (swapped, _) = edit(&ctx.paths, |roster| {
        let Some(old) = roster.slot(account).map(|s| s.number) else { return Ok(None) };
        let mut swapped = None;
        for slot in &mut roster.slots {
            if slot.account == account {
                slot.number = number;
            } else if slot.number == number {
                slot.number = old;
                swapped = Some(slot.account.clone());
            }
        }
        Ok(swapped)
    })?;
    Ok(swapped)
}

/// Take an account off the list and forget the sign-in kept for it. Its sessions stay; it joins
/// again the next time Claude is signed in to it.
pub fn remove(ctx: &Ctx, account: &str) -> Result<()> {
    if desktop::last_known_account(ctx).as_deref() == Some(account) {
        return Err(Refused::SignedIn.into());
    }
    logins::forget(ctx, account)?;
    edit(&ctx.paths, |roster| {
        roster.slots.retain(|s| s.account != account);
        Ok(())
    })?;
    Ok(())
}

/// How to pick the account when none is named.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Strategy {
    /// The next one in the list.
    #[default]
    Next,
    /// The next one whose plan was not used up when Claude last saw it.
    NextAvailable,
    /// The one with the most of its plan left when Claude last saw it.
    Best,
}

/// The account to switch to when none is named. `current` is the account Claude is signed in
/// to; `can_switch` says whether Claude can switch to an account (a sign-in is kept for it).
/// Accounts sitting out of the rotation are skipped.
pub fn pick<'a>(
    roster: &'a Roster,
    current: Option<&str>,
    strategy: Strategy,
    can_switch: impl Fn(&Slot) -> bool,
    usage: &BTreeMap<String, usage::Usage>,
    now: f64,
) -> Option<&'a Slot> {
    let after = current.and_then(|c| roster.slot(c)).map_or(0, |s| s.number);
    let mut order: Vec<&Slot> =
        roster.slots.iter().filter(|s| !s.disabled && Some(s.account.as_str()) != current && can_switch(s)).collect();
    // The rotation: the numbers after the current one, then from the start.
    order.sort_by_key(|s| (s.number <= after, s.number));
    let load = |s: &Slot| usage.get(&s.account).and_then(|u| u.load_at(now));
    match strategy {
        Strategy::Next => order.first().copied(),
        Strategy::NextAvailable => {
            order.iter().copied().find(|s| !usage.get(&s.account).is_some_and(|u| u.exhausted_at(now)))
        }
        // Known numbers first, least used; ties and unknowns keep the rotation's order.
        Strategy::Best => order
            .iter()
            .copied()
            .enumerate()
            .min_by(|(i, a), (j, b)| match (load(a), load(b)) {
                (Some(x), Some(y)) => x.total_cmp(&y).then(i.cmp(j)),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => i.cmp(j),
            })
            .map(|(_, s)| s),
    }
}

/// Switch Claude to `account`, keeping the one in use, and note what changed in the list. `watch`
/// follows Claude's quit ([`desktop::quit`]).
pub fn switch(ctx: &Ctx, account: &str, watch: &desktop::Watch) -> Result<logins::Switched> {
    let done = logins::switch(ctx, logins::Target::Account(account), watch)?;
    observe(ctx);
    crate::hooks::run(ctx, &crate::hooks::Event::Switch { from: done.from.clone(), to: done.to.clone() });
    Ok(done)
}

/// Restart Claude on its sign-in page to add an account; the one in use is kept and stays in
/// the list.
pub fn add(ctx: &Ctx, watch: &desktop::Watch) -> Result<logins::Switched> {
    let done = logins::switch(ctx, logins::Target::SignedOut, watch)?;
    observe(ctx);
    crate::hooks::run(ctx, &crate::hooks::Event::Switch { from: done.from.clone(), to: None });
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, FakeDesktop, LogSink};
    use std::sync::Arc;

    const ADA: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const BOB: &str = "bbbbbbbb-0000-4000-8000-000000000002";
    const CY: &str = "cccccccc-0000-4000-8000-000000000003";
    const ADA_ORG: &str = "0a0a0a0a-0000-4000-8000-000000000001";
    const BOB_ORG: &str = "0b0b0b0b-0000-4000-8000-000000000002";

    struct Mac {
        _tmp: tempfile::TempDir,
        ctx: Arc<Ctx>,
    }

    impl Mac {
        fn new() -> Mac {
            let tmp = tempfile::tempdir().unwrap();
            let mut paths = Paths::new(tmp.path().join("Claude"), tmp.path().join("state"));
            paths.claude_json = tmp.path().join(".claude.json");
            paths.claude_settings = tmp.path().join(".claude/settings.json");
            paths.projects = vec![tmp.path().join(".claude/projects")];
            paths.desktop_logs = tmp.path().join("Logs");
            fs::create_dir_all(&paths.user_data).unwrap();
            fs::create_dir_all(&paths.desktop_logs).unwrap();
            let fake = FakeDesktop { running: Some(false), active: None };
            Mac { _tmp: tmp, ctx: Arc::new(Ctx::new(paths, Config::default(), fake, LogSink::Silent)) }
        }

        fn signed_in(&self, account: &str) {
            let config = serde_json::json!({ "lastKnownAccountUuid": account });
            fs::write(self.ctx.paths.desktop_config(), config.to_string()).unwrap();
        }

        fn kept(&self, account: &str, email: &str) {
            let dir = self.ctx.paths.state_dir.join("logins").join(account);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("config.json"), "{}").unwrap();
            let about = serde_json::json!({ "account": account, "email": email, "setAsideAt": fsx::now_secs() });
            fs::write(dir.join("about.json"), about.to_string()).unwrap();
        }
    }

    fn slot(number: u32, account: &str) -> Slot {
        Slot {
            number,
            account: account.into(),
            email: Some(format!("{}@example.com", &account[..1])),
            ..Slot::default()
        }
    }

    #[test]
    fn accounts_join_by_themselves_and_keep_their_numbers() {
        let mac = Mac::new();
        assert!(observe(&mac.ctx).slots.is_empty());
        mac.signed_in(BOB);
        mac.kept(ADA, "ada@lovelace.dev");
        let roster = observe(&mac.ctx);
        let listed: Vec<_> = roster.slots.iter().map(|s| (s.number, s.account.as_str(), s.email.clone())).collect();
        assert_eq!(listed, [(1, BOB, None), (2, ADA, Some("ada@lovelace.dev".into()))]);
        // Saved, and the same on the next look.
        assert_eq!(Roster::load(&mac.ctx.paths), roster);
        assert_eq!(observe(&mac.ctx), roster);
        // An email learned later fills in; a third account takes the next number.
        fs::write(
            &mac.ctx.paths.claude_json,
            serde_json::json!({ "oauthAccount": { "accountUuid": BOB, "emailAddress": "bob@example.com" } })
                .to_string(),
        )
        .unwrap();
        mac.signed_in(CY);
        let roster = observe(&mac.ctx);
        assert_eq!(roster.slot(BOB).unwrap().email.as_deref(), Some("bob@example.com"));
        assert_eq!(roster.slot(CY).unwrap().number, 3);
    }

    #[test]
    fn an_unreadable_list_is_set_aside_not_overwritten() {
        let mac = Mac::new();
        fs::create_dir_all(&mac.ctx.paths.state_dir).unwrap();
        fs::write(mac.ctx.paths.roster_file(), "{ not json").unwrap();
        mac.signed_in(ADA);
        assert_eq!(observe(&mac.ctx).slots.len(), 1);
        let aside = mac.ctx.paths.state_dir.join("accounts.json.unreadable");
        assert_eq!(fs::read_to_string(aside).unwrap(), "{ not json");
    }

    #[test]
    fn organizations_seen_after_an_account_loads_are_its_own() {
        let mac = Mac::new();
        mac.signed_in(BOB);
        // Claude loaded Bob at 12:00:00; usage samples before that were Ada's.
        let line = "2026-09-30 12:00:00 [info] [LocalSessionManager] Initialization succeeded — accountId=";
        fs::write(mac.ctx.paths.desktop_logs.join("main.log"), format!("{line}{BOB}, orgId={BOB_ORG}\n")).unwrap();
        let loaded = desktop::last_init(&mac.ctx).unwrap().at.unwrap();
        let history = serde_json::json!({ "version": 2, "samples": [
            { "t": (loaded - 600.0) * 1000.0, "org": ADA_ORG, "u": { "fh": 90, "sd": 40 } },
            { "t": (loaded + 300.0) * 1000.0, "org": BOB_ORG, "u": { "fh": 10, "sd": 20 } },
        ]});
        fs::write(mac.ctx.paths.usage_history(), history.to_string()).unwrap();
        let roster = observe(&mac.ctx);
        assert_eq!(roster.slot(BOB).unwrap().orgs, [BOB_ORG]);
        let used = usage(&mac.ctx, &roster);
        assert_eq!(used[BOB].weekly, Some(20.0));
        assert!(!used.contains_key(ADA));
    }

    #[test]
    fn accounts_are_found_by_number_alias_email_or_id() {
        let mut ada = slot(1, ADA);
        ada.alias = Some("work".into());
        let roster = Roster { version: 1, slots: vec![ada, slot(2, BOB)] };
        assert_eq!(roster.find("2").unwrap().account, BOB);
        assert_eq!(roster.find("WORK").unwrap().account, ADA);
        assert_eq!(roster.find("B@Example.com").unwrap().account, BOB);
        assert_eq!(roster.find("bbbb").unwrap().account, BOB);
        assert_eq!(roster.find("3"), Err(NotFound::None("3".into())));
        assert!(matches!(roster.find("abc"), Err(NotFound::None(_))));
    }

    #[test]
    fn aliases_are_checked_and_unique() {
        let mac = Mac::new();
        mac.signed_in(ADA);
        mac.kept(BOB, "bob@example.com");
        observe(&mac.ctx);
        assert_eq!(normalize_alias(" Work "), Ok("work".into()));
        assert_eq!(normalize_alias("工作"), Ok("工作".into()));
        assert_eq!(normalize_alias("Ärbeit"), Ok("ärbeit".into()));
        let long = "x".repeat(ALIAS_MAX + 1);
        for bad in ["", "12", "-x", "a b", "a/b", long.as_str()] {
            assert!(normalize_alias(bad).is_err(), "{bad}");
        }
        let roster = set_alias(&mac.ctx, BOB, Some("Home")).unwrap();
        assert_eq!(roster.slot(BOB).unwrap().alias.as_deref(), Some("home"));
        let taken = set_alias(&mac.ctx, ADA, Some("home")).unwrap_err();
        assert!(matches!(taken.downcast_ref::<Refused>(), Some(Refused::AliasTaken { .. })));
        assert_eq!(set_alias(&mac.ctx, BOB, None).unwrap().slot(BOB).unwrap().alias, None);
    }

    #[test]
    fn moving_to_a_taken_number_trades_places() {
        let mac = Mac::new();
        mac.signed_in(ADA);
        mac.kept(BOB, "bob@example.com");
        observe(&mac.ctx);
        assert_eq!(move_to(&mac.ctx, BOB, 1).unwrap().as_deref(), Some(ADA));
        let roster = Roster::load(&mac.ctx.paths);
        assert_eq!(roster.slots.iter().map(|s| s.account.as_str()).collect::<Vec<_>>(), [BOB, ADA]);
        assert_eq!(move_to(&mac.ctx, ADA, 7).unwrap(), None);
        assert_eq!(Roster::load(&mac.ctx.paths).slot(ADA).unwrap().number, 7);
        assert!(move_to(&mac.ctx, ADA, 0).is_err());
    }

    #[test]
    fn removing_an_account_forgets_its_sign_in() {
        let mac = Mac::new();
        mac.signed_in(ADA);
        mac.kept(BOB, "bob@example.com");
        observe(&mac.ctx);
        let refused = remove(&mac.ctx, ADA).unwrap_err();
        assert_eq!(refused.downcast_ref::<Refused>(), Some(&Refused::SignedIn));
        remove(&mac.ctx, BOB).unwrap();
        assert!(logins::list(&mac.ctx).saved.is_empty());
        assert_eq!(observe(&mac.ctx).slots.iter().map(|s| s.account.as_str()).collect::<Vec<_>>(), [ADA]);
    }

    #[test]
    fn the_rotation_skips_accounts_that_sit_out_or_cannot_be_switched_to() {
        let mut roster = Roster { version: 1, slots: vec![slot(1, ADA), slot(2, BOB), slot(3, CY)] };
        let none = BTreeMap::new();
        let all = |_: &Slot| true;
        let next = |r: &Roster, current, can: &dyn Fn(&Slot) -> bool| {
            pick(r, current, Strategy::Next, can, &none, 0.0).map(|s| s.account.clone())
        };
        assert_eq!(next(&roster, Some(ADA), &all).as_deref(), Some(BOB));
        assert_eq!(next(&roster, Some(CY), &all).as_deref(), Some(ADA), "wraps around");
        assert_eq!(next(&roster, None, &all).as_deref(), Some(ADA));
        roster.slots[1].disabled = true;
        assert_eq!(next(&roster, Some(ADA), &all).as_deref(), Some(CY));
        let only_ada = |s: &Slot| s.account == ADA;
        assert_eq!(next(&roster, Some(ADA), &only_ada), None);
    }

    #[test]
    fn strategies_use_what_claude_last_saw() {
        let roster = Roster { version: 1, slots: vec![slot(1, ADA), slot(2, BOB), slot(3, CY)] };
        let now = 1_000_000.0;
        let used = |five_hour: f64, weekly: f64| usage::Usage {
            at: now - 600.0,
            five_hour: Some(five_hour),
            weekly: Some(weekly),
        };
        let usage = BTreeMap::from([(BOB.to_string(), used(100.0, 30.0)), (CY.to_string(), used(5.0, 20.0))]);
        let pick = |strategy| pick(&roster, Some(ADA), strategy, |_| true, &usage, now).map(|s| s.account.clone());
        assert_eq!(pick(Strategy::Next).as_deref(), Some(BOB));
        assert_eq!(pick(Strategy::NextAvailable).as_deref(), Some(CY), "Bob's 5-hour window is full");
        assert_eq!(pick(Strategy::Best).as_deref(), Some(CY));
        // Six hours on, Bob's 5-hour window has started over: he is next again.
        let later = super::pick(&roster, Some(ADA), Strategy::NextAvailable, |_| true, &usage, now + 6.0 * 3600.0);
        assert_eq!(later.map(|s| s.account.as_str()), Some(BOB));
    }
}
