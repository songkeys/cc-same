//! How much of each account's plan is used, from Claude Desktop's own record.
//!
//! While it runs, Desktop asks claude.ai how much of the open organization's plan is used, and
//! keeps the answers for 30 days in `plan-usage-history.json`: one sample per answer, labelled
//! with the organization (`fh` is the 5-hour window and `sd` the weekly one, in percent). It asks
//! when it starts, when the account changes, when its menu bar menu opens (a right-click on its
//! icon), and every 15 minutes in between. Since October 2026 (Desktop 2.19675) a switch on
//! Anthropic's side can pause those in-between checks until the menu has been opened in the last
//! 24 hours (`[plan-usage] background poll paused: tray not opened recently` in `main.log`), so
//! even the open account's numbers can be a day old. CC Same only reads that file. It never asks
//! Anthropic itself, so it needs no sign-in and no token, and an account's numbers are as fresh as
//! the last time Claude asked.
//!
//! A sample names an organization, not an account. Desktop only gets an answer for an
//! organization the signed-in account belongs to, so the samples taken after an account loads
//! show which organizations it uses; [`crate::accounts`] notes them. The rest of the record is
//! attributed by organization: one that a single account uses belongs to it.

use crate::fsx;
use crate::model::is_uuid;
use crate::paths::Paths;
use chrono::{Datelike as _, Duration, TimeZone, Weekday};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The 5-hour window reopens five hours after it starts at the latest.
pub const FIVE_HOURS: f64 = 5.0 * 3600.0;
pub const WEEK: f64 = 7.0 * 86_400.0;
/// Desktop checks every 15 minutes while it checks at all, so an older reading of the open
/// account means it has stopped.
pub const FRESH: f64 = 30.0 * 60.0;

/// One answer Desktop recorded.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// When (Unix seconds).
    pub at: f64,
    /// The organization it is about; none in records from before Desktop noted it.
    pub org: Option<String>,
    /// Percent of the 5-hour window used.
    pub five_hour: Option<f64>,
    /// Percent of the weekly window used.
    pub weekly: Option<f64>,
}

/// Desktop's record, oldest first. Empty when there is none or it cannot be read.
pub fn read(paths: &Paths) -> Vec<Sample> {
    let Ok(Value::Object(file)) = fsx::read_json(&paths.usage_history(), 32 * 1024 * 1024) else {
        return Vec::new();
    };
    let Some(Value::Array(samples)) = file.get("samples") else { return Vec::new() };
    let number = |v: Option<&Value>| v.and_then(Value::as_f64).filter(|n| n.is_finite());
    let mut out: Vec<Sample> = samples
        .iter()
        .filter_map(|s| {
            let s = s.as_object()?;
            let at = number(s.get("t"))? / 1000.0;
            // Version 2 keeps the windows under `u`; version 1 kept them beside the time.
            let u = s.get("u").and_then(Value::as_object).unwrap_or(s);
            Some(Sample {
                at,
                org: s.get("org").and_then(Value::as_str).filter(|o| is_uuid(o)).map(str::to_string),
                five_hour: number(u.get("fh")),
                weekly: number(u.get("sd")),
            })
        })
        .collect();
    out.sort_by(|a, b| a.at.total_cmp(&b.at));
    out
}

/// An account's last known usage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Usage {
    /// When Desktop read it (Unix seconds).
    pub at: f64,
    pub five_hour: Option<f64>,
    pub weekly: Option<f64>,
}

impl Usage {
    /// The 5-hour window as it stands at `now`: a reading older than five hours no longer holds,
    /// and the window has started over (0).
    pub fn five_hour_at(&self, now: f64) -> Option<f64> {
        if now - self.at > FIVE_HOURS {
            self.five_hour.map(|_| 0.0)
        } else {
            self.five_hour
        }
    }

    /// The weekly window as it stands at `now`, when the reading is recent enough to say.
    pub fn weekly_at(&self, now: f64) -> Option<f64> {
        if now - self.at > WEEK {
            None
        } else {
            self.weekly
        }
    }

    /// The fuller of the two windows at `now`, when anything is known.
    pub fn load_at(&self, now: f64) -> Option<f64> {
        match (self.five_hour_at(now), self.weekly_at(now)) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    /// A window was full when last read and has not started over since.
    pub fn exhausted_at(&self, now: f64) -> bool {
        self.load_at(now).is_some_and(|load| load >= 100.0)
    }
}

/// Which account each organization belongs to, for attributing samples. `seen` holds the
/// organizations each account was seen using; `folders` the (account, organization) session
/// folders on disk, for organizations not seen yet. An organization claimed by more than one
/// account (one the accounts share, or a folder Claude left behind when switching) is left out.
pub fn owners<'a>(
    seen: impl IntoIterator<Item = (&'a str, &'a [String])>,
    folders: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> BTreeMap<String, String> {
    fn single(claims: BTreeMap<&str, BTreeSet<&str>>) -> BTreeMap<String, String> {
        claims
            .into_iter()
            .filter_map(|(org, accounts)| {
                let mut accounts = accounts.into_iter();
                match (accounts.next(), accounts.next()) {
                    (Some(account), None) => Some((org.to_string(), account.to_string())),
                    _ => None,
                }
            })
            .collect()
    }
    let mut learned: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (account, orgs) in seen {
        for org in orgs {
            learned.entry(org.as_str()).or_default().insert(account);
        }
    }
    let mut on_disk: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (account, org) in folders {
        if !learned.contains_key(org) {
            on_disk.entry(org).or_default().insert(account);
        }
    }
    let mut out = single(on_disk);
    out.extend(single(learned));
    out
}

/// The newest sample of every account that `owners` can attribute one to.
pub fn latest(samples: &[Sample], owners: &BTreeMap<String, String>) -> BTreeMap<String, Usage> {
    let mut out: BTreeMap<String, Usage> = BTreeMap::new();
    for s in samples {
        let Some(account) = s.org.as_ref().and_then(|org| owners.get(org)) else { continue };
        if out.get(account).is_none_or(|u| s.at >= u.at) {
            out.insert(account.clone(), Usage { at: s.at, five_hour: s.five_hour, weekly: s.weekly });
        }
    }
    out
}

/// Organizations in samples taken after `since`.
pub fn orgs_since(samples: &[Sample], since: f64) -> BTreeSet<String> {
    samples.iter().filter(|s| s.at > since).filter_map(|s| s.org.clone()).collect()
}

/// The first renewal after `now` of a weekly window that renews at `anchor` and every week from
/// there, both in Unix seconds. Both are moments, not clock times, so a change of time zone moves
/// only how the renewal reads.
pub fn next_weekly_reset(anchor: i64, now: i64) -> i64 {
    let week = WEEK as i64;
    let ahead = (anchor - now).rem_euclid(week);
    now + if ahead == 0 { week } else { ahead }
}

/// The renewal on `weekday` at `hour`:`minute` in `tz` (the computer's own time zone, in the
/// app), as an anchor for [`next_weekly_reset`]. A time the clocks skip reads an hour later.
pub fn weekly_anchor<Tz: TimeZone>(tz: &Tz, weekday: Weekday, hour: u32, minute: u32, now: i64) -> Option<i64> {
    let today = tz.timestamp_opt(now, 0).single()?.date_naive();
    let days = (weekday.num_days_from_monday() + 7 - today.weekday().num_days_from_monday()) % 7;
    let at = (today + Duration::days(days.into())).and_hms_opt(hour, minute, 0)?;
    let local = tz
        .from_local_datetime(&at)
        .earliest()
        .or_else(|| tz.from_local_datetime(&(at + Duration::hours(1))).earliest())?;
    Some(local.timestamp())
}

/// [`weekly_anchor`] in this computer's time zone, with the weekday from 0 for Monday.
pub fn weekly_anchor_here(weekday: usize, hour: u32, minute: u32, now: i64) -> Option<i64> {
    let weekday = Weekday::try_from(u8::try_from(weekday).ok()?).ok()?;
    weekly_anchor(&chrono::Local, weekday, hour, minute, now)
}

/// The next renewal of the window that renews at `anchor`, as this computer's time zone reads it
/// now: the weekday (0 for Monday), the hour and the minute.
pub fn next_weekly_reset_here(anchor: i64, now: i64) -> Option<(usize, u32, u32)> {
    use chrono::Timelike as _;
    let at = chrono::Local.timestamp_opt(next_weekly_reset(anchor, now), 0).single()?;
    Some((at.weekday().num_days_from_monday() as usize, at.hour(), at.minute()))
}

/// When a weekly window last renewed, given its anchor: a reading older than that is spent.
pub fn last_weekly_reset(anchor: i64, now: i64) -> i64 {
    next_weekly_reset(anchor, now) - WEEK as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADA: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const BOB: &str = "bbbbbbbb-0000-4000-8000-000000000002";
    const ADA_ORG: &str = "0a0a0a0a-0000-4000-8000-000000000001";
    const BOB_ORG: &str = "0b0b0b0b-0000-4000-8000-000000000002";
    const TEAM: &str = "0c0c0c0c-0000-4000-8000-000000000003";

    fn paths() -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("Claude"), tmp.path().join("state"));
        std::fs::create_dir_all(&paths.user_data).unwrap();
        (tmp, paths)
    }

    #[test]
    fn reads_both_versions_of_desktops_record() {
        let (_tmp, paths) = paths();
        let v2 = serde_json::json!({ "version": 2, "samples": [
            { "t": 2_000_000.0, "org": BOB_ORG, "u": { "fh": 12, "sd": 58, "so": 3 } },
            { "t": 1_000_000.0, "org": ADA_ORG, "u": { "fh": 5 } },
            { "t": "garbage" },
        ]});
        std::fs::write(paths.usage_history(), v2.to_string()).unwrap();
        let samples = read(&paths);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0], Sample { at: 1000.0, org: Some(ADA_ORG.into()), five_hour: Some(5.0), weekly: None });
        assert_eq!((samples[1].five_hour, samples[1].weekly), (Some(12.0), Some(58.0)));

        let v1 = serde_json::json!({ "version": 1, "samples": [{ "t": 3_000_000, "fh": 40, "sd": null }] });
        std::fs::write(paths.usage_history(), v1.to_string()).unwrap();
        assert_eq!(read(&paths), [Sample { at: 3000.0, org: None, five_hour: Some(40.0), weekly: None }]);

        std::fs::write(paths.usage_history(), "{").unwrap();
        assert!(read(&paths).is_empty());
    }

    #[test]
    fn an_old_reading_of_the_five_hour_window_has_started_over() {
        let u = Usage { at: 0.0, five_hour: Some(97.0), weekly: Some(40.0) };
        assert_eq!(u.five_hour_at(3600.0), Some(97.0));
        assert!(!u.exhausted_at(3600.0) && u.load_at(3600.0) == Some(97.0));
        assert_eq!(u.five_hour_at(6.0 * 3600.0), Some(0.0));
        assert_eq!(u.load_at(6.0 * 3600.0), Some(40.0));
        // A week on, nothing is known any more.
        assert_eq!(u.load_at(8.0 * 86_400.0), Some(0.0));
        assert_eq!(u.weekly_at(8.0 * 86_400.0), None);
        let full = Usage { at: 0.0, five_hour: Some(30.0), weekly: Some(100.0) };
        assert!(full.exhausted_at(86_400.0));
    }

    #[test]
    fn organizations_belong_to_the_one_account_that_uses_them() {
        let ada_seen = vec![ADA_ORG.to_string()];
        let bob_seen: Vec<String> = Vec::new();
        // Claude left Bob a folder in Ada's organization (it reopens the last one after a
        // switch), and both are in a team.
        let folders = [(ADA, ADA_ORG), (BOB, ADA_ORG), (BOB, BOB_ORG), (ADA, TEAM), (BOB, TEAM)];
        let owners = owners([(ADA, ada_seen.as_slice()), (BOB, bob_seen.as_slice())], folders);
        assert_eq!(owners.get(ADA_ORG).map(String::as_str), Some(ADA), "seen in use beats a folder");
        assert_eq!(owners.get(BOB_ORG).map(String::as_str), Some(BOB));
        assert_eq!(owners.get(TEAM), None, "shared: no way to tell whose samples they are");
        let samples = [
            Sample { at: 10.0, org: Some(ADA_ORG.into()), five_hour: Some(1.0), weekly: Some(2.0) },
            Sample { at: 30.0, org: Some(ADA_ORG.into()), five_hour: Some(3.0), weekly: Some(4.0) },
            Sample { at: 20.0, org: Some(BOB_ORG.into()), five_hour: Some(5.0), weekly: Some(6.0) },
            Sample { at: 40.0, org: Some(TEAM.into()), five_hour: Some(99.0), weekly: Some(99.0) },
            Sample { at: 50.0, org: None, five_hour: Some(99.0), weekly: Some(99.0) },
        ];
        let latest = latest(&samples, &owners);
        assert_eq!(latest[ADA], Usage { at: 30.0, five_hour: Some(3.0), weekly: Some(4.0) });
        assert_eq!(latest[BOB].at, 20.0);
        assert_eq!(orgs_since(&samples, 15.0), BTreeSet::from([ADA_ORG.into(), BOB_ORG.into(), TEAM.into()]));
    }

    #[test]
    fn a_weekly_reset_rolls_on_a_week_at_a_time() {
        let week = WEEK as i64;
        let anchor = 1_000_000;
        assert_eq!(next_weekly_reset(anchor, anchor - 10), anchor);
        assert_eq!(next_weekly_reset(anchor, anchor), anchor + week);
        assert_eq!(next_weekly_reset(anchor, anchor + 3 * week + 5), anchor + 4 * week);
        // An anchor weeks ahead still names this week's renewal.
        assert_eq!(next_weekly_reset(anchor + 5 * week, anchor + 1), anchor + week);
        assert_eq!(last_weekly_reset(anchor, anchor + week + 1), anchor + week);
    }

    #[test]
    fn a_weekly_reset_is_given_in_local_time_and_kept_as_a_moment() {
        use chrono::FixedOffset;
        // Wednesday 2026-10-28 12:00 UTC.
        let now = 1_793_188_800;
        let shanghai = FixedOffset::east_opt(8 * 3600).unwrap();
        let la = FixedOffset::west_opt(7 * 3600).unwrap();
        // Friday 06:00 in Los Angeles is Friday 13:00 UTC.
        let anchor = weekly_anchor(&la, Weekday::Fri, 6, 0, now).unwrap();
        assert_eq!(anchor, now + 2 * 86_400 + 3600);
        // Read on a computer moved to Shanghai, the same renewal is Friday 21:00 there.
        let there = shanghai.timestamp_opt(next_weekly_reset(anchor, now), 0).unwrap();
        assert_eq!((there.weekday(), there.format("%H:%M").to_string()), (Weekday::Fri, "21:00".into()));
        // Today's weekday at an hour already past is this week's renewal, gone by: the next is a week on.
        let gone = weekly_anchor(&chrono::Utc, Weekday::Wed, 6, 0, now).unwrap();
        assert_eq!(next_weekly_reset(gone, now), gone + WEEK as i64);
        assert_eq!(weekly_anchor(&chrono::Utc, Weekday::Wed, 24, 0, now), None);
    }
}
