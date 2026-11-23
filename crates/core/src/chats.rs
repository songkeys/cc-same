// The idea and rules of lost-chat rescue and of copying chats between accounts follow
// claude-switcher (MIT, Copyright (c) 2026 NeezerGu and Nyxon); its license is in `LICENSES/MIT-claude-switcher.txt`.
//! Chats across accounts: list them, find the ones Desktop forgot, and copy them between lists.
//!
//! A chat is a `local_<uuid>.json` "card" in an account's index (what the sidebar shows) plus a
//! transcript under `~/.claude/projects` (what was said). Sync keeps the cards of the accounts that
//! take part identical, so these operations matter for the rest:
//!
//! * **Lost chats**: a transcript no card in any account names, which Desktop therefore never
//!   shows (a cleared index, an account that was removed, a crash). Rescue gives them a card.
//! * **Copy / merge**: put chats into an account's list, for accounts kept out of sync. The newer
//!   card wins.
//!
//! Everything goes through the sync engine's writer ([`apply::apply_plan`]): new files only unless
//! a newer card replaces an older one, never into a list Claude has loaded, behind the lock, and
//! after a snapshot that `restore` can put back. A move is deliberately not offered: a deleted
//! card travels to every account that takes part in sync, so it would delete the chat everywhere.

use crate::accounts::{self, NotFound};
use crate::apply::{self, Lock};
use crate::ctx::Ctx;
use crate::model::*;
use crate::scan;
use crate::snapshot;
use crate::transcript::{self, Meta};
use crate::State;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// A chat as one account's list has it.
#[derive(Clone, Debug, PartialEq)]
pub struct Chat {
    /// `<account>/<organization>`.
    pub list: String,
    /// The card's id, `local_<uuid>`.
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub archived: bool,
    /// Unix milliseconds; 0 when the card has none.
    pub last_activity_ms: i64,
    /// The current transcript's id.
    pub transcript: Option<String>,
}

/// A chat whose history is on disk but that no account's list shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lost {
    pub meta: Meta,
    /// The title its card would get.
    pub title: String,
    /// The last folder of its working directory.
    pub project: String,
}

/// The index folders of every account, read.
fn states(ctx: &Ctx) -> Vec<PartState> {
    scan::discover(ctx, Surface::Code)
        .iter()
        .map(|p| scan::scan_partition(ctx, p))
        .filter(|s| s.error.is_none())
        .collect()
}

fn text(d: &Json, key: &str) -> String {
    d.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// Every chat in every account's list, newest first.
pub fn list(ctx: &Ctx) -> Vec<Chat> {
    let mut out = Vec::new();
    for s in states(ctx) {
        for (uuid, r) in &s.records {
            let Some(d) = r.data.as_deref() else { continue };
            out.push(Chat {
                list: s.part.key(),
                id: format!("local_{uuid}"),
                title: text(d, "title"),
                cwd: text(d, "cwd"),
                archived: d.get("isArchived") == Some(&Value::Bool(true)),
                last_activity_ms: d.get("lastActivityAt").and_then(Value::as_f64).map_or(0, |n| n as i64),
                transcript: d.get("cliSessionId").and_then(Value::as_str).map(str::to_string),
            });
        }
    }
    out.sort_by(|a, b| b.last_activity_ms.cmp(&a.last_activity_ms).then_with(|| a.id.cmp(&b.id)));
    out
}

/// Which lists `query` names: an account's number, alias or email in the account list, an account
/// id (or the start of one), or `<account>/<organization>`. One list per organization.
pub fn resolve(ctx: &Ctx, query: &str) -> Result<Vec<Partition>> {
    let all = scan::discover(ctx, Surface::Code);
    let query = query.trim();
    let roster = accounts::observe(ctx);
    let account = match roster.find(query) {
        Ok(slot) => Some(slot.account.clone()),
        Err(NotFound::Several(names)) => bail!("{query} could be {}", names.join(" or ")),
        Err(NotFound::None(_)) => None,
    };
    let hits: Vec<Partition> = all
        .into_iter()
        .filter(|p| match &account {
            Some(a) => p.acct == *a,
            None => query.len() >= 4 && (p.key() == query || p.acct.starts_with(query) || p.key().starts_with(query)),
        })
        .collect();
    let accounts_hit: BTreeSet<&str> = hits.iter().map(|p| p.acct.as_str()).collect();
    match (hits.len(), accounts_hit.len()) {
        (0, _) => bail!("no account called {query} has chats here (see `cc-same accounts`)"),
        (_, 1) => Ok(hits),
        _ => bail!("{query} could be several accounts: use more of its id"),
    }
}

fn chat_key(title: &str, cwd: &str) -> String {
    format!("{}|{cwd}", transcript::one_line(title, 80)).to_lowercase()
}

/// The title a rescued chat gets: the transcript's title, else its first prompt.
fn title_for(m: &Meta) -> String {
    [&m.title, &m.first_prompt]
        .into_iter()
        .map(|s| transcript::one_line(s, 80))
        .find(|t| !t.is_empty())
        .unwrap_or_else(|| "Recovered chat".to_string())
}

fn last_folder(cwd: &str) -> String {
    cwd.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or_default().to_string()
}

/// Lost chats, newest first, and how many other transcripts were left out as older parts or
/// copies of chats that are listed already.
///
/// A chat spans several transcripts (a `/clear`, a restart or a resume starts a new one) and its
/// card names them all. So a transcript is lost when no card in any list names it, it was not
/// deleted in the app, it holds a conversation, and no card carries its title in its folder.
/// Transcripts of one lost chat share a title and a folder and are offered once, at the newest.
pub fn lost(ctx: &Ctx) -> (Vec<Lost>, usize) {
    let states = states(ctx);
    let mut known: HashSet<String> = HashSet::new();
    let mut titled: HashSet<String> = HashSet::new();
    for s in &states {
        for (uuid, r) in &s.records {
            known.insert(format!("local_{uuid}"));
            if let Some(d) = r.data.as_deref() {
                known.extend(transcript_ids(d));
                let title = text(d, "title");
                if !title.is_empty() {
                    titled.insert(chat_key(&title, &text(d, "cwd")));
                }
            }
        }
        known.extend(s.tombs.keys().cloned());
    }
    let mut newest: BTreeMap<String, Lost> = BTreeMap::new();
    let mut candidates = 0;
    for (session, path) in transcript::transcript_paths(ctx) {
        if known.contains(&session) {
            continue;
        }
        let Ok(meta) = transcript::quick_meta(&path) else { continue };
        if !meta.is_conversation() {
            continue;
        }
        candidates += 1;
        let title = title_for(&meta);
        let key = chat_key(&title, &meta.cwd);
        if titled.contains(&key) {
            continue;
        }
        let project = last_folder(&meta.cwd);
        let found = Lost { meta, title, project };
        let newer = newest
            .get(&key)
            .is_none_or(|cur| (found.meta.end_ms, &found.meta.session) > (cur.meta.end_ms, &cur.meta.session));
        if newer {
            newest.insert(key, found);
        }
    }
    let mut out: Vec<Lost> = newest.into_values().collect();
    out.sort_by(|a, b| b.meta.end_ms.cmp(&a.meta.end_ms).then_with(|| a.meta.session.cmp(&b.meta.session)));
    let left_out = candidates - out.len();
    (out, left_out)
}

/// The card Desktop shows for a rescued chat: the fields Desktop itself writes for a new one.
fn new_card(m: &Meta) -> (String, Json) {
    let uuid = uuid::Uuid::new_v4().to_string();
    let end = m.end_ms.unwrap_or_else(|| (crate::fsx::now_secs() * 1000.0) as i64);
    let start = m.start_ms.unwrap_or(end);
    let mut card = Json::new();
    let mut put = |k: &str, v: Value| {
        card.insert(k.to_string(), v);
    };
    put("sessionId", json!(format!("local_{uuid}")));
    put("cliSessionId", json!(m.session));
    put("cwd", json!(m.cwd));
    put("originCwd", json!(m.cwd));
    put("lastFocusedAt", json!(end));
    put("createdAt", json!(start));
    put("lastActivityAt", json!(end));
    put("isArchived", json!(false));
    put("title", json!(title_for(m)));
    put("titleSource", json!("auto"));
    put("permissionMode", json!("default"));
    put("remoteMcpServersConfig", json!([]));
    put("alwaysAllowedReasons", json!([]));
    put("sessionPermissionUpdates", json!([]));
    put("spawnSeed", json!({}));
    if m.model.starts_with("claude-") {
        put("model", json!(m.model));
    }
    (uuid, card)
}

/// What a rescue, copy or merge did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Done {
    /// Cards created, as `(card id, title)`.
    pub created: Vec<(String, String)>,
    /// Older cards replaced by a newer one.
    pub updated: usize,
    /// Chats the list had already, in the same or a newer state.
    pub already: usize,
    /// Chats left alone because the list's owner deleted them.
    pub deleted_there: usize,
    /// The snapshot taken first; `restore` puts every list back as it was.
    pub snapshot: Option<String>,
}

fn ensure_closed(ctx: &Ctx, part: &Partition) -> Result<()> {
    if crate::desktop::detect(ctx).loaded(part) {
        bail!(
            "Claude has {} open, so its chats cannot be changed now: quit Claude, or choose another account",
            part.label()
        );
    }
    Ok(())
}

fn write_all(ctx: &Ctx, actions: Vec<Action>, reason: &str, done: &mut Done) -> Result<()> {
    if actions.is_empty() {
        return Ok(());
    }
    done.snapshot = Some(snapshot::take(ctx, reason)?);
    let plan = Plan { actions, ..Plan::default() };
    let mut state = State::load(&ctx.paths);
    let out = apply::apply_plan(ctx, plan, &mut state, reason)?;
    if let Some(first) = out.errors.first() {
        bail!("{first}");
    }
    if out.skipped_loaded > 0 {
        bail!("Claude opened the list meanwhile; nothing more was written. Quit Claude and try again");
    }
    Ok(())
}

fn write_action(part: &Partition, uuid: &str, portable: Arc<Json>, additive: bool, expect: Option<i128>) -> Action {
    let mut a = Action::new(ActionKind::WriteRecord, part, format!("local_{uuid}.json"));
    a.additive = additive;
    a.extra = ActionExtra::Record { portable, expect_mtime_ns: expect };
    a
}

/// Pick lost chats by the start of a transcript id.
fn pick(lost: &[Lost], refs: &[String]) -> Result<Vec<Lost>> {
    let mut out: Vec<Lost> = Vec::new();
    for r in refs {
        let hits: Vec<&Lost> =
            lost.iter().filter(|l| !r.is_empty() && l.meta.session.starts_with(r.as_str())).collect();
        match hits.as_slice() {
            [] => bail!("no lost chat starts with {r} (see `cc-same chats lost`)"),
            [one] => {
                if !out.iter().any(|o| o.meta.session == one.meta.session) {
                    out.push((*one).clone());
                }
            }
            many => bail!("{r} starts {} lost chats: use more of the id", many.len()),
        }
    }
    Ok(out)
}

/// Give lost chats a card in `to`'s list so Claude shows them again. `refs` start transcript ids
/// from [`lost`]; none means all of them.
pub fn rescue(ctx: &Ctx, to: &Partition, refs: &[String]) -> Result<Done> {
    let _lock = Lock::acquire(ctx, Duration::from_secs(30))?;
    ensure_closed(ctx, to)?;
    let (found, _) = lost(ctx);
    let picked = if refs.is_empty() { found } else { pick(&found, refs)? };
    let mut done = Done::default();
    let mut actions = Vec::new();
    for l in &picked {
        let (uuid, card) = new_card(&l.meta);
        done.created.push((format!("local_{uuid}"), l.title.clone()));
        actions.push(write_action(to, &uuid, Arc::new(card), true, None));
    }
    write_all(ctx, actions, "rescue lost chats", &mut done)?;
    Ok(done)
}

fn activity(d: &Json) -> f64 {
    d.get("lastActivityAt").and_then(Value::as_f64).unwrap_or(0.0)
}

/// Put chats of `from`'s list into `to`'s. `refs` start card ids (`local_…` or the uuid) or
/// transcript ids; none means every chat, which is a merge. A chat `to` has already is replaced
/// only by a newer card, one its owner deleted is left deleted.
pub fn copy(ctx: &Ctx, from: &Partition, to: &Partition, refs: &[String]) -> Result<Done> {
    if from.key() == to.key() {
        bail!("that is the same list");
    }
    let _lock = Lock::acquire(ctx, Duration::from_secs(30))?;
    ensure_closed(ctx, to)?;
    let src = scan::scan_partition(ctx, from);
    let dst = scan::scan_partition(ctx, to);
    if let Some(e) = src.error.as_ref().or(dst.error.as_ref()) {
        bail!("{e}");
    }
    let wanted = |uuid: &str, d: &Json| {
        refs.is_empty()
            || refs.iter().any(|r| {
                let r = r.trim_start_matches("local_");
                !r.is_empty()
                    && (uuid.starts_with(r)
                        || d.get("cliSessionId").and_then(Value::as_str).is_some_and(|c| c.starts_with(r)))
            })
    };
    let mut done = Done::default();
    let mut actions = Vec::new();
    let mut matched = 0;
    for (uuid, r) in &src.records {
        let Some(d) = r.data.as_deref() else { continue };
        if !wanted(uuid, d) {
            continue;
        }
        matched += 1;
        let deleted = dst.tombs.contains_key(&format!("local_{uuid}"))
            || transcript_ids(d).iter().any(|t| dst.tombs.contains_key(t));
        if deleted {
            done.deleted_there += 1;
            continue;
        }
        let portable = Arc::new(d.clone());
        let title = text(d, "title");
        match dst.records.get(uuid) {
            None => {
                let mut a = write_action(to, uuid, portable, true, None);
                a.mtime_ns = Some(r.mtime_ns);
                actions.push(a);
                done.created.push((format!("local_{uuid}"), title));
            }
            Some(existing) => match existing.data.as_deref() {
                Some(e) if activity(d) > activity(e) => {
                    let mut a = write_action(to, uuid, portable, false, Some(existing.mtime_ns));
                    a.mtime_ns = Some(r.mtime_ns);
                    actions.push(a);
                    done.updated += 1;
                }
                _ => done.already += 1,
            },
        }
    }
    if matched == 0 && !refs.is_empty() {
        bail!("no chat in {} starts with {}", from.label(), refs.join(" or "));
    }
    write_all(ctx, actions, "copy chats", &mut done)?;
    Ok(done)
}

/// The transcript a reference names: the start of a transcript id, or of a chat's card id
/// (`local_…` or the uuid) or of the transcript its card points at.
pub fn find_transcript(ctx: &Ctx, reference: &str) -> Result<std::path::PathBuf> {
    let reference = reference.trim();
    if reference.is_empty() {
        bail!("name a chat by the start of its id");
    }
    let paths = transcript::transcript_paths(ctx);
    let mut ids: BTreeSet<String> = paths.keys().filter(|id| id.starts_with(reference)).cloned().collect();
    let card = reference.trim_start_matches("local_");
    for chat in list(ctx) {
        if !card.is_empty() && chat.id.trim_start_matches("local_").starts_with(card) {
            ids.extend(chat.transcript.into_iter().filter(|t| paths.contains_key(t)));
        }
    }
    match ids.len() {
        0 => bail!("no chat with a transcript on disk starts with {reference}"),
        1 => Ok(paths[ids.first().expect("one id")].clone()),
        n => bail!("{reference} starts {n} chats: use more of the id"),
    }
}

// -------------------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, FakeDesktop, LogSink, Paths};
    use std::fs;
    use std::path::{Path, PathBuf};

    const A: (&str, &str) = ("aaaaaaaa-0000-4000-8000-000000000001", "0a0a0a0a-0000-4000-8000-000000000001");
    const B: (&str, &str) = ("bbbbbbbb-0000-4000-8000-000000000002", "0b0b0b0b-0000-4000-8000-000000000002");
    const U1: &str = "11111111-1111-4111-8111-111111111111";
    const U2: &str = "22222222-2222-4222-8222-222222222222";

    struct Env {
        _tmp: tempfile::TempDir,
        ctx: Ctx,
        root: PathBuf,
    }

    impl Env {
        fn new(running: bool) -> Env {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().to_path_buf();
            let mut paths = Paths::new(root.join("Claude"), root.join("state"));
            paths.desktop_logs = root.join("logs");
            paths.projects = vec![root.join("projects")];
            paths.claude_settings = root.join("settings.json");
            paths.claude_json = root.join("claude.json");
            fs::create_dir_all(&paths.user_data).unwrap();
            fs::create_dir_all(&paths.desktop_logs).unwrap();
            let ctx = Ctx::new(
                paths,
                Config::default(),
                FakeDesktop { running: Some(running), active: running.then(|| vec![A.0.to_string()]) },
                LogSink::Silent,
            );
            Env { _tmp: tmp, ctx, root }
        }

        fn list_dir(&self, p: (&str, &str)) -> PathBuf {
            let d = self.ctx.paths.user_data.join(Surface::Code.dir_name()).join(p.0).join(p.1);
            fs::create_dir_all(&d).unwrap();
            d
        }

        fn card(&self, p: (&str, &str), u: &str, title: &str, cli: &str, activity: i64) {
            let d = json!({"sessionId": format!("local_{u}"), "cliSessionId": cli, "cwd": "/work/app", "title": title,
                "isArchived": false, "createdAt": 1, "lastActivityAt": activity});
            fs::write(self.list_dir(p).join(format!("local_{u}.json")), serde_json::to_vec(&d).unwrap()).unwrap();
        }

        fn transcript(&self, id: &str, title: &str, ts: &str) -> PathBuf {
            let dir = self.root.join("projects/-work-app");
            fs::create_dir_all(&dir).unwrap();
            let body = format!(
                "{}\n{}\n",
                json!({"type":"user","cwd":"/work/app","timestamp":ts,"message":{"content":title}}),
                json!({"type":"assistant","cwd":"/work/app","timestamp":ts,"message":{"id":"m","model":"claude-opus-5-5","content":[]}})
            );
            let path = dir.join(format!("{id}.jsonl"));
            fs::write(&path, body).unwrap();
            path
        }

        fn part(&self, p: (&str, &str)) -> Partition {
            scan::discover(&self.ctx, Surface::Code).into_iter().find(|x| x.acct == p.0).unwrap()
        }

        fn names(&self, p: (&str, &str)) -> BTreeSet<String> {
            fs::read_dir(self.list_dir(p))
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("local_"))
                .collect()
        }
    }

    fn exists(p: &Path) -> bool {
        p.exists()
    }

    #[test]
    fn a_transcript_no_card_names_is_lost_and_rescue_gives_it_a_card() {
        let env = Env::new(false);
        env.card(A, U1, "Listed chat", "known-session", 100);
        env.list_dir(B);
        env.transcript("known-session", "Listed chat", "2026-10-01T10:00:00Z");
        env.transcript("lost-session-1", "Where did my chat go", "2026-10-02T10:00:00Z");
        // A cleared conversation is a second transcript of the same lost chat: offered once, at the newest.
        env.transcript("lost-session-0", "Where did my chat go", "2026-10-01T10:00:00Z");
        // An empty transcript is not a chat.
        fs::write(env.root.join("projects/-work-app/empty.jsonl"), "{\"type\":\"summary\"}\n").unwrap();

        let (found, left_out) = lost(&env.ctx);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            (found[0].meta.session.as_str(), found[0].title.as_str()),
            ("lost-session-1", "Where did my chat go")
        );
        assert_eq!((found[0].project.as_str(), left_out), ("app", 1));

        let done = rescue(&env.ctx, &env.part(B), &[]).unwrap();
        assert_eq!(done.created.len(), 1);
        assert!(done.snapshot.is_some());
        let name = format!("{}.json", done.created[0].0);
        let card: Value = serde_json::from_slice(&fs::read(env.list_dir(B).join(&name)).unwrap()).unwrap();
        assert_eq!(card["cliSessionId"], "lost-session-1");
        assert_eq!(card["title"], "Where did my chat go");
        assert_eq!(card["cwd"], "/work/app");
        assert_eq!(card["model"], "claude-opus-5-5");
        assert_eq!(card["sessionId"], done.created[0].0.as_str());
        // Nothing is lost any more, and rescuing again changes nothing.
        assert!(lost(&env.ctx).0.is_empty());
        assert!(rescue(&env.ctx, &env.part(B), &[]).unwrap().created.is_empty());
    }

    #[test]
    fn a_chat_deleted_in_the_app_is_not_lost() {
        let env = Env::new(false);
        env.list_dir(A);
        env.transcript("gone-session", "I deleted this", "2026-10-02T10:00:00Z");
        fs::write(env.list_dir(A).join("deleted_gone-session"), "1760000000000").unwrap();
        assert!(lost(&env.ctx).0.is_empty());
    }

    #[test]
    fn rescue_picks_by_the_start_of_an_id_and_refuses_what_it_cannot_find() {
        let env = Env::new(false);
        env.list_dir(A);
        env.transcript("abc-111", "One", "2026-10-02T10:00:00Z");
        env.transcript("abd-222", "Two", "2026-10-03T10:00:00Z");
        assert!(rescue(&env.ctx, &env.part(A), &["zzz".into()]).is_err());
        assert!(rescue(&env.ctx, &env.part(A), &["ab".into()]).unwrap_err().to_string().contains("starts 2"));
        let done = rescue(&env.ctx, &env.part(A), &["abc".into()]).unwrap();
        assert_eq!(done.created.len(), 1);
        assert_eq!(lost(&env.ctx).0.len(), 1);
    }

    #[test]
    fn nothing_is_written_into_a_list_claude_has_open() {
        let env = Env::new(true);
        env.list_dir(A);
        env.transcript("lost-1", "Lost", "2026-10-02T10:00:00Z");
        let err = rescue(&env.ctx, &env.part(A), &[]).unwrap_err().to_string();
        assert!(err.contains("quit Claude"), "{err}");
        assert!(env.names(A).is_empty());
    }

    #[test]
    fn copying_adds_missing_chats_replaces_older_ones_and_respects_deletions() {
        let env = Env::new(false);
        env.card(A, U1, "New in A", "c1", 100);
        env.card(A, U2, "Newer in A", "c2", 500);
        // A different length from A's card: the scan cache keys on size and modification time, and
        // some systems (Linux) stamp files written within a few milliseconds with the same time.
        env.card(B, U2, "Older card in B", "c2", 200);
        let other = "33333333-3333-4333-8333-333333333333";
        env.card(A, other, "Deleted in B", "c3", 100);
        fs::write(env.list_dir(B).join(format!("deleted_local_{other}")), "1760000000000").unwrap();
        let done = copy(&env.ctx, &env.part(A), &env.part(B), &[]).unwrap();
        assert_eq!((done.created.len(), done.updated, done.already, done.deleted_there), (1, 1, 0, 1));
        assert_eq!(env.names(B), BTreeSet::from([format!("local_{U1}.json"), format!("local_{U2}.json")]));
        let b2: Value =
            serde_json::from_slice(&fs::read(env.list_dir(B).join(format!("local_{U2}.json"))).unwrap()).unwrap();
        assert_eq!(b2["title"], "Newer in A");
        // Again: nothing to do, and no snapshot taken for it.
        let again = copy(&env.ctx, &env.part(A), &env.part(B), &[]).unwrap();
        assert_eq!((again.created.len(), again.updated, again.already, again.snapshot), (0, 0, 2, None));
    }

    #[test]
    fn copying_one_chat_by_the_start_of_its_id_leaves_the_rest() {
        let env = Env::new(false);
        env.card(A, U1, "One", "cli-one", 100);
        env.card(A, U2, "Two", "cli-two", 100);
        env.list_dir(B);
        let done = copy(&env.ctx, &env.part(A), &env.part(B), &["cli-t".into()]).unwrap();
        assert_eq!(done.created.len(), 1);
        assert_eq!(env.names(B), BTreeSet::from([format!("local_{U2}.json")]));
        assert!(copy(&env.ctx, &env.part(A), &env.part(B), &["nope".into()]).is_err());
        assert!(copy(&env.ctx, &env.part(A), &env.part(A), &[]).is_err(), "a list into itself");
        assert!(exists(&env.list_dir(A).join(format!("local_{U1}.json"))), "the source is never touched");
    }

    #[test]
    fn accounts_are_found_by_id_start_and_listing_shows_every_chat() {
        let env = Env::new(false);
        env.card(A, U1, "One", "c1", 100);
        env.card(B, U2, "Two", "c2", 300);
        assert_eq!(resolve(&env.ctx, "aaaaaaaa").unwrap()[0].acct, A.0);
        assert_eq!(resolve(&env.ctx, &format!("{}/{}", B.0, B.1)).unwrap()[0].acct, B.0);
        assert!(resolve(&env.ctx, "nobody").is_err());
        let chats = list(&env.ctx);
        assert_eq!(chats.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(), ["Two", "One"], "newest first");
        assert_eq!(chats[1].list, format!("{}/{}", A.0, A.1));
    }
}
