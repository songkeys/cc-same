//! Deciding what a sync would do. Reads only.

use crate::config::State;
use crate::ctx::Ctx;
use crate::desktop;
use crate::merge::{merge_collection, trivially_empty};
use crate::model::*;
use crate::scan;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

/// Scan every synced surface and plan a sync against the current Desktop state.
pub fn build_plan(
    ctx: &Ctx,
    app: Option<AppState>,
    state: Option<&State>,
) -> (Plan, AppState, BTreeMap<Surface, Vec<PartState>>) {
    let app = app.unwrap_or_else(|| desktop::detect(ctx));
    let loaded_state;
    let state = match state {
        Some(s) => s,
        None => {
            loaded_state = State::load(&ctx.paths);
            &loaded_state
        }
    };
    let cfg = ctx.config();
    let mut plan = Plan::default();
    let mut scanned = BTreeMap::new();
    for surface in Surface::ALL {
        let known: BTreeSet<String> = state.known(surface).into_iter().collect();
        let parts: Vec<Partition> = scan::discover(ctx, surface)
            .into_iter()
            .filter(|p| !cfg.is_excluded(&p.acct, &p.org))
            .filter(|p| cfg.auto_join_new || known.is_empty() || known.contains(&p.key()))
            .collect();
        let states: Vec<PartState> = parts.iter().map(|p| scan::scan_partition(ctx, p)).collect();
        let bases = state.bases.get(surface.as_str()).cloned().unwrap_or_default();
        plan_surface(ctx, surface, &states, &app, &known, &bases, &mut plan);
        scanned.insert(surface, states);
    }
    (plan, app, scanned)
}

/// The newest copy wins; ties go to the greatest partition key so every run agrees. If the
/// newest copy lost its transcript link while an older one still points at a transcript on
/// disk, the older one wins, unless the newest copy left that transcript behind on purpose.
fn choose_winner<'a>(
    members: &[&'a PartState],
    cands: &[(usize, &'a Record)],
    transcripts: Option<&HashSet<String>>,
) -> (usize, &'a Record, Option<String>) {
    let mut ordered: Vec<(usize, &Record)> = cands.to_vec();
    ordered.sort_by(|a, b| (b.1.mtime_ns, members[b.0].part.key()).cmp(&(a.1.mtime_ns, members[a.0].part.key())));
    let healthy = |r: &Record| {
        let d = r.data.as_deref().unwrap();
        let cid = d.get("cliSessionId").and_then(Value::as_str).unwrap_or("");
        if cid.is_empty() || d.get("transcriptUnavailable") == Some(&Value::Bool(true)) {
            return false;
        }
        transcripts.is_none_or(|t| t.contains(cid))
    };
    let top = ordered[0];
    if transcripts.is_none() || healthy(top.1) {
        return (top.0, top.1, None);
    }
    let Some(&(i, healthy_copy)) = ordered[1..].iter().find(|c| healthy(c.1)) else {
        return (top.0, top.1, None);
    };
    let cid = healthy_copy.data.as_deref().unwrap().get("cliSessionId").and_then(Value::as_str);
    if cid.is_some_and(|cid| left_behind(top.1.data.as_deref().unwrap()).contains(cid)) {
        return (top.0, top.1, None);
    }
    let why = format!(
        "newest copy of {} lost its transcript link; kept the newest copy whose transcript is on disk",
        top.1.uuid
    );
    (i, healthy_copy, Some(why))
}

/// Transcripts a session moved off on purpose: Desktop lists the old one in `priorCliSessionIds`
/// when a conversation is cleared or rewound (clearing can also keep it in
/// `preClearCliSessionId`). A transcript it could not resume is dropped without a trace.
fn left_behind(d: &Json) -> BTreeSet<&str> {
    let prior = d.get("priorCliSessionIds").and_then(Value::as_array).into_iter().flatten();
    prior.chain(d.get("preClearCliSessionId")).filter_map(Value::as_str).collect()
}

struct Planner<'a> {
    surface: Surface,
    app: &'a AppState,
    known: &'a BTreeSet<String>,
    plan: &'a mut Plan,
}

impl Planner<'_> {
    /// Desktop has this index loaded: only brand-new files, and only into an index we have
    /// never seen (first join). Everything else waits for an account switch or a quit.
    fn can_write(&self, p: &Partition, additive: bool) -> bool {
        !self.app.loaded(p) || (additive && !self.known.contains(&p.key()))
    }

    fn defer(&mut self, p: &Partition) {
        *self.plan.deferred.entry(p.key()).or_default() += 1;
    }

    fn add(&mut self, a: Action) {
        if a.kind == ActionKind::WriteRecord && self.app.loaded(&a.part) {
            *self.plan.seeded_while_loaded.entry(a.part.key()).or_default() += 1;
        }
        self.plan.actions.push(a);
    }

    fn note(&mut self, msg: String) {
        self.plan.notes.push(msg);
    }
}

pub(crate) fn plan_surface(
    ctx: &Ctx,
    surface: Surface,
    states: &[PartState],
    app: &AppState,
    known: &BTreeSet<String>,
    bases: &BTreeMap<String, BTreeMap<String, Value>>,
    plan: &mut Plan,
) {
    let members: Vec<&PartState> = states.iter().filter(|s| s.error.is_none()).collect();
    plan.members.insert(surface, members.iter().map(|s| s.part.clone()).collect());
    for s in states {
        if let Some(err) = &s.error {
            plan.notes.push(format!("{surface} {}: skipped ({err})", s.part.label()));
            plan.skipped.push(format!("{surface} {}: {err}", s.part.label()));
        }
    }
    if members.len() < 2 {
        return;
    }
    let transcripts = scan::transcript_index(ctx);
    let mut pl = Planner { surface, app, known, plan };

    let mut recs: BTreeMap<&str, Vec<(usize, &Record)>> = BTreeMap::new();
    let mut tombs: BTreeMap<&str, Vec<(usize, &Tomb)>> = BTreeMap::new();
    for (i, s) in members.iter().enumerate() {
        for (u, r) in &s.records {
            recs.entry(u.as_str()).or_default().push((i, r));
        }
        for (t, tomb) in &s.tombs {
            tombs.entry(t.as_str()).or_default().push((i, tomb));
        }
    }

    let mut cleared: BTreeSet<&str> = BTreeSet::new();
    let mut finals: Vec<BTreeMap<String, Arc<Json>>> = members
        .iter()
        .map(|s| s.records.iter().filter_map(|(u, r)| r.data.clone().map(|d| (u.clone(), d))).collect())
        .collect();
    let mut alive = 0;

    for (&u, entries) in &recs {
        for (i, r) in entries {
            let label = members[*i].part.label();
            let file = r.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
            if let Some(err) = &r.error {
                pl.note(format!("{surface} {label}: {file} is unreadable ({err}); left untouched"));
            } else if r.nlink > 1 {
                pl.note(format!(
                    "{surface} {label}: {file} has {} hard links; Claude refuses to read such files",
                    r.nlink
                ));
            }
        }
        let valid: Vec<(usize, &Record)> = entries.iter().filter(|(_, r)| r.data.is_some()).copied().collect();
        if valid.is_empty() {
            continue;
        }
        let (wi, wr, why) = choose_winner(&members, &valid, transcripts.as_deref());
        if let Some(why) = why {
            pl.note(why);
        }
        let winner = wr.data.clone().unwrap();

        let dead_ts = tombs.get(u).and_then(|l| l.iter().map(|(_, t)| t.ts_ms).max());
        if dead_ts.is_some_and(|ts| ts > wr.mtime_ms()) {
            // Deleted in some account after its last change: remove everywhere (to trash).
            for &(i, r) in &valid {
                let p = &members[i].part;
                if pl.can_write(p, false) {
                    let name = r.path.file_name().unwrap().to_string_lossy().into_owned();
                    pl.add(Action::new(ActionKind::TrashRecord, p, name));
                    finals[i].remove(u);
                } else {
                    pl.defer(p);
                }
            }
            continue;
        }

        alive += 1;
        for (i, s) in members.iter().enumerate() {
            let existing = s.records.get(u);
            if let Some(e) = existing {
                if e.data.is_none() {
                    continue; // unreadable: never overwrite what we cannot parse
                }
                if e.data.as_deref() == Some(&*winner) {
                    continue; // identical apart from account-bound fields
                }
            }
            let additive = existing.is_none();
            if !pl.can_write(&s.part, additive) {
                pl.defer(&s.part);
                continue;
            }
            let mut a = Action::new(ActionKind::WriteRecord, &s.part, format!("local_{u}.json"));
            a.mtime_ns = Some(wr.mtime_ns);
            a.src = Some(members[wi].part.clone());
            a.additive = additive;
            a.extra = ActionExtra::Record { portable: winner.clone(), expect_mtime_ns: existing.map(|e| e.mtime_ns) };
            pl.add(a);
            finals[i].insert(u.to_string(), winner.clone());
        }

        // The session is live: drop older "deleted" markers that refer to it (Desktop does the
        // same when a session comes back).
        let mut ids = transcript_ids(&winner);
        ids.insert(u.to_string());
        for id in &ids {
            let Some((&tid, lst)) = tombs.get_key_value(id.as_str()) else { continue };
            if lst.iter().map(|(_, t)| t.ts_ms).max().unwrap() > wr.mtime_ms() {
                continue;
            }
            cleared.insert(tid);
            for &(i, _) in lst {
                let p = &members[i].part;
                if pl.can_write(p, false) {
                    pl.add(Action::new(ActionKind::TrashTomb, p, format!("deleted_{tid}")));
                } else {
                    pl.defer(p);
                }
            }
        }
    }
    pl.plan.alive.insert(surface, alive);

    // Propagate "deleted" markers (they also stop Desktop's importer offering the session).
    for (&tid, lst) in &tombs {
        if cleared.contains(tid) {
            continue;
        }
        let best = lst
            .iter()
            .max_by(|a, b| (a.1.ts_ms, members[a.0].part.key()).cmp(&(b.1.ts_ms, members[b.0].part.key())))
            .unwrap()
            .1;
        let holders: HashSet<usize> = lst.iter().map(|(i, _)| *i).collect();
        for (i, s) in members.iter().enumerate() {
            if holders.contains(&i) {
                continue;
            }
            if pl.can_write(&s.part, true) {
                let mut a = Action::new(ActionKind::WriteTomb, &s.part, format!("deleted_{tid}"));
                a.payload = Some(best.raw.clone());
                a.mtime_ns = Some(best.mtime_ns);
                a.additive = true;
                pl.add(a);
            } else {
                pl.defer(&s.part);
            }
        }
    }

    plan_collections(&mut pl, &members, bases);

    // Desktop's archived-sessions.idx is a load-order hint; keep it matching the records.
    for (i, s) in members.iter().enumerate() {
        if !pl.can_write(&s.part, false) {
            continue;
        }
        let records = &finals[i];
        let any_archived = records.values().any(|d| d.get("isArchived") == Some(&Value::Bool(true)));
        if s.archive_idx.is_none() && !any_archived {
            continue;
        }
        let payload = archive_idx_payload(records);
        if s.archive_idx.as_deref() != Some(payload.as_slice()) {
            let mut a = Action::new(ActionKind::WriteArchiveIdx, &s.part, ARCHIVE_IDX);
            a.payload = Some(Arc::new(payload));
            pl.add(a);
        }
    }
}

fn plan_collections(pl: &mut Planner<'_>, members: &[&PartState], bases: &BTreeMap<String, BTreeMap<String, Value>>) {
    let surface = pl.surface;
    let mut out_bases: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    for &rel in surface.collections() {
        let mut have: Vec<(usize, &Collection)> =
            members.iter().enumerate().filter_map(|(i, s)| s.collections.get(rel).map(|c| (i, c))).collect();
        for (i, c) in &have {
            if let Some(err) = &c.error {
                let label = members[*i].part.label();
                pl.note(format!("{surface} {label}: {rel} unreadable ({err}); left untouched"));
            }
        }
        have.retain(|(_, c)| c.data.is_some());
        if have.is_empty() {
            continue;
        }
        let rb = bases.get(rel).cloned().unwrap_or_default();
        have.sort_by(|a, b| (b.1.mtime_ns, members[b.0].part.key()).cmp(&(a.1.mtime_ns, members[a.0].part.key())));
        let inputs: Vec<(&Map<String, Value>, Option<&Value>)> =
            have.iter().map(|(i, c)| (c.data.as_ref().unwrap(), rb.get(&members[*i].part.key()))).collect();
        let result = merge_collection(&inputs, rb.get("__group__"));
        let payload = Arc::new(dumps_like(&result, have[0].1.indent));
        let mut new_rb = rb.clone();
        new_rb.insert("__group__".into(), Value::Object(result.clone()));
        for s in members {
            let key = s.part.key();
            match s.collections.get(rel) {
                Some(c) if c.data.is_none() => continue,
                Some(c) if c.data.as_ref() == Some(&result) => {
                    new_rb.insert(key, Value::Object(result.clone()));
                    continue;
                }
                None if trivially_empty(&result) => continue,
                _ => {}
            }
            if !pl.can_write(&s.part, false) {
                pl.defer(&s.part);
                continue;
            }
            let mut a = Action::new(ActionKind::WriteCollection, &s.part, rel);
            a.payload = Some(payload.clone());
            a.extra = ActionExtra::Collection { rel: rel.to_string(), prev_base: rb.get(&key).cloned() };
            pl.add(a);
            new_rb.insert(key, Value::Object(result.clone()));
        }
        out_bases.insert(rel.to_string(), new_rb);
    }
    pl.plan.bases.insert(surface, out_bases);
}

/// Same bytes Desktop writes: `{"v":1,"archived":[sorted "local_<uuid>" ids]}`.
pub fn archive_idx_payload(records: &BTreeMap<String, Arc<Json>>) -> Vec<u8> {
    let mut ids: Vec<String> = records
        .iter()
        .filter(|(_, d)| d.get("isArchived") == Some(&Value::Bool(true)))
        .map(|(u, _)| format!("local_{u}"))
        .filter(|id| id[6..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
        .collect();
    ids.sort();
    let mut m = Map::new();
    m.insert("v".into(), Value::from(1));
    m.insert("archived".into(), Value::from(ids));
    serde_json::to_vec(&Value::Object(m)).unwrap()
}

/// Compact like `JSON.stringify(x)`, or indented like `JSON.stringify(x, null, n)`.
pub fn dumps_like(v: &Map<String, Value>, indent: Option<usize>) -> Vec<u8> {
    let value = Value::Object(v.clone());
    match indent {
        None | Some(0) => serde_json::to_vec(&value).unwrap(),
        Some(n) => {
            let pad = " ".repeat(n);
            let fmt = serde_json::ser::PrettyFormatter::with_indent(pad.as_bytes());
            let mut out = Vec::new();
            let mut ser = serde_json::Serializer::with_formatter(&mut out, fmt);
            serde::Serialize::serialize(&value, &mut ser).unwrap();
            out
        }
    }
}
