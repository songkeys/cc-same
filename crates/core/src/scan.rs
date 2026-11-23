//! Reading Desktop's index folders. Nothing here writes.

use crate::ctx::Ctx;
use crate::fsx;
use crate::model::*;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Every `<account>/<org>` folder of a surface, symlinked ones included (flagged).
pub fn discover(ctx: &Ctx, surface: Surface) -> Vec<Partition> {
    org_folders(&ctx.paths.surface_root(surface))
        .into_iter()
        .map(|(acct, org, path, is_link)| Partition { surface, acct, org, path, is_link })
        .collect()
}

/// `<account>/<org>` folders under `root`: account, org, path, and whether a symlink is involved.
fn org_folders(root: &Path) -> Vec<(String, String, PathBuf, bool)> {
    let mut out = Vec::new();
    for acct in sorted_entries(root) {
        let acct_name = acct.file_name().to_string_lossy().into_owned();
        if !is_uuid(&acct_name) || !acct.path().is_dir() {
            continue;
        }
        let acct_link = fsx::is_symlink(&acct.path());
        for org in sorted_entries(&acct.path()) {
            let org_name = org.file_name().to_string_lossy().into_owned();
            if !is_uuid(&org_name) || !org.path().is_dir() {
                continue;
            }
            let path = root.join(&acct_name).join(&org_name);
            out.push((acct_name.clone(), org_name, path, acct_link || fsx::is_symlink(&org.path())));
        }
    }
    out
}

fn sorted_entries(dir: &Path) -> Vec<fs::DirEntry> {
    let mut v: Vec<fs::DirEntry> = fs::read_dir(dir).map(|rd| rd.flatten().collect()).unwrap_or_default();
    v.sort_by_key(|e| e.file_name());
    v
}

/// Backup/rename leftovers from other tools (e.g. `<org>.bak-<time>`).
pub fn leftover_dirs(ctx: &Ctx, surface: Surface) -> Vec<String> {
    let mut out = Vec::new();
    for acct in sorted_entries(&ctx.paths.surface_root(surface)) {
        let name = acct.file_name().to_string_lossy().into_owned();
        let looks_like = |n: &str| n.len() > 36 && n.get(..36).is_some_and(is_uuid);
        if is_uuid(&name) && acct.path().is_dir() {
            for org in sorted_entries(&acct.path()) {
                let o = org.file_name().to_string_lossy().into_owned();
                if looks_like(&o) {
                    out.push(format!("{name}/{o}"));
                }
            }
        } else if looks_like(&name) {
            out.push(name);
        }
    }
    out
}

/// The record in `path`, a file of the index folder `dir`. It is parsed once and kept until the
/// file changes or goes away ([`forget_missing`]): Desktop rewrites the record of a session in
/// use every few seconds, and each rewrite must take the place of the one before.
fn load_record(ctx: &Ctx, dir: &Path, path: &Path, uuid: &str, md: &fs::Metadata) -> Record {
    let (mtime_ns, size) = (fsx::mtime_ns(md), md.len());
    if let Some(hit) = ctx.caches.records.lock().unwrap().get(dir).and_then(|recs| recs.get(uuid)) {
        if hit.mtime_ns == mtime_ns && hit.size == size {
            return hit.clone();
        }
    }
    let mut rec = Record {
        uuid: uuid.to_string(),
        path: path.to_path_buf(),
        mtime_ns,
        size,
        nlink: fsx::nlink(md),
        data: None,
        error: None,
    };
    match fsx::read_json(path, MAX_RECORD_BYTES) {
        Ok(Value::Object(d)) => {
            if d.get("sessionId").and_then(Value::as_str) == Some(&format!("local_{uuid}")) {
                rec.data = Some(Arc::new(portable(&d)));
            } else {
                rec.error = Some("sessionId does not match the file name".into());
            }
        }
        Ok(_) => rec.error = Some("not a JSON object".into()),
        Err(e) => rec.error = Some(e.to_string()),
    }
    let mut cache = ctx.caches.records.lock().unwrap();
    // A session's copies in the other accounts hold the same portable fields once they are in
    // sync, and so does a file rewritten with nothing portable changed: keep one for all.
    if let Some(data) = &mut rec.data {
        let same = cache.values().filter_map(|recs| recs.get(uuid)?.data.as_ref()).find(|d| ***d == **data);
        if let Some(same) = same {
            *data = same.clone();
        }
    }
    cache.entry(dir.to_path_buf()).or_default().insert(uuid.to_string(), rec.clone());
    rec
}

/// Forget the parsed records of files that are no longer in the folder `ps` was read from, and
/// those of folders that are gone.
fn forget_missing(ctx: &Ctx, ps: &PartState) {
    ctx.caches.records.lock().unwrap().retain(|dir, recs| {
        if *dir != ps.part.path {
            return dir.is_dir();
        }
        recs.retain(|uuid, _| ps.records.contains_key(uuid));
        !recs.is_empty()
    });
}

fn load_tomb(path: &Path, id: &str, md: &fs::Metadata) -> Tomb {
    let raw = fsx::read_limited(path, 64).unwrap_or_default();
    let text = String::from_utf8_lossy(&raw).trim().to_string();
    let ts_ms = match text.parse::<i128>() {
        Ok(ts) if !text.is_empty() && text.len() <= 16 && text.bytes().all(|b| b.is_ascii_digit()) => ts,
        _ => fsx::mtime_ns(md).div_euclid(1_000_000),
    };
    let raw = if raw.is_empty() { ts_ms.to_string().into_bytes() } else { raw };
    Tomb { id: id.to_string(), path: path.to_path_buf(), ts_ms, raw: Arc::new(raw), mtime_ns: fsx::mtime_ns(md) }
}

fn detect_indent(raw: &[u8]) -> Option<usize> {
    let rest = raw.strip_prefix(b"{\n").or_else(|| raw.strip_prefix(b"{\r\n"))?;
    let spaces = rest.iter().take_while(|b| **b == b' ').count();
    (spaces > 0 && rest.get(spaces) == Some(&b'"')).then_some(spaces)
}

fn load_collection(path: &Path, rel: &'static str) -> Option<Collection> {
    let md = fs::symlink_metadata(path).ok()?;
    let mut col = Collection {
        rel,
        path: path.to_path_buf(),
        mtime_ns: fsx::mtime_ns(&md),
        data: None,
        error: None,
        indent: None,
    };
    if !md.is_file() {
        col.error = Some("not a regular file".into());
        return Some(col);
    }
    match fsx::read_limited(path, MAX_COLLECTION_BYTES) {
        Ok(raw) => match serde_json::from_slice::<Value>(&raw) {
            Ok(Value::Object(d)) => {
                col.data = Some(d);
                col.indent = detect_indent(&raw);
            }
            Ok(_) => col.error = Some("not a JSON object".into()),
            Err(e) => col.error = Some(e.to_string()),
        },
        Err(e) => col.error = Some(e.to_string()),
    }
    Some(col)
}

const KNOWN_NAMES: &[&str] = &[".DS_Store", ARCHIVE_IDX, "scheduled-tasks.json", "backlog", "desktop.ini"];

pub fn scan_partition(ctx: &Ctx, p: &Partition) -> PartState {
    let ps = read_partition(ctx, p);
    forget_missing(ctx, &ps);
    ps
}

fn read_partition(ctx: &Ctx, p: &Partition) -> PartState {
    let mut ps = PartState::new(p.clone());
    if p.is_link {
        ps.error = Some("symlinked folder: Claude cannot save sessions through it (run fix-symlinks)".into());
        return ps;
    }
    let entries = match fs::read_dir(&p.path) {
        Ok(rd) => rd.flatten().collect::<Vec<_>>(),
        Err(e) => {
            ps.error = Some(e.to_string());
            return ps;
        }
    };
    let now = std::time::SystemTime::now();
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(md) = fs::symlink_metadata(e.path()) else { continue };
        if name.starts_with(fsx::TMP_PREFIX) {
            if md
                .modified()
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|d| d > Duration::from_secs(3600))
            {
                ps.stale_tmp.push(name);
            }
            continue;
        }
        if let Some(uuid) = name.strip_prefix("local_").and_then(|n| n.strip_suffix(".json")).filter(|u| is_uuid(u)) {
            if md.is_file() {
                ps.records.insert(uuid.to_string(), load_record(ctx, &p.path, &e.path(), uuid, &md));
            } else {
                ps.unmanaged.push(format!("{name} (not a regular file)"));
            }
            continue;
        }
        if let Some(id) = name.strip_prefix("deleted_").filter(|i| is_marker_id(i)) {
            if md.is_file() {
                ps.tombs.insert(id.to_string(), load_tomb(&e.path(), id, &md));
                continue;
            }
        }
        if name == ARCHIVE_IDX && md.is_file() {
            ps.archive_idx = fsx::read_limited(&e.path(), 8 * 1024 * 1024).ok();
            continue;
        }
        if KNOWN_NAMES.contains(&name.as_str()) {
            continue;
        }
        ps.unmanaged.push(name);
    }
    for rel in p.surface.collections() {
        if let Some(parent) = Path::new(rel).parent().filter(|p| !p.as_os_str().is_empty()) {
            match fs::symlink_metadata(p.path.join(parent)) {
                Ok(md) if md.is_dir() => {}
                _ => continue,
            }
        }
        if let Some(col) = load_collection(&p.path.join(rel), rel) {
            ps.collections.insert(rel, col);
        }
    }
    ps
}

/// Session ids (`local_<uuid>` without the prefix) that have a readable record anywhere.
pub fn union_of(states: &[PartState]) -> HashSet<String> {
    states.iter().flat_map(|s| s.records.iter().filter(|(_, r)| r.data.is_some()).map(|(u, _)| u.clone())).collect()
}

/// Ids of transcripts on disk (`<projects>/<project>/<id>.jsonl`). Cached for a minute.
pub fn transcript_index(ctx: &Ctx) -> Option<Arc<HashSet<String>>> {
    let mut cache = ctx.caches.transcripts.lock().unwrap();
    if let Some((at, ids)) = cache.as_ref() {
        if at.elapsed() < Duration::from_secs(60) {
            return ids.clone();
        }
    }
    let mut ids = HashSet::new();
    let mut seen_root = false;
    for root in &ctx.paths.projects {
        let Ok(projects) = fs::read_dir(root) else { continue };
        seen_root = true;
        for proj in projects.flatten() {
            if !proj.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Ok(files) = fs::read_dir(proj.path()) else { continue };
            for f in files.flatten() {
                let n = f.file_name().to_string_lossy().into_owned();
                if let Some(stem) = n.strip_suffix(".jsonl") {
                    ids.insert(stem.to_string());
                }
            }
        }
    }
    let result = seen_root.then(|| Arc::new(ids));
    *cache = Some((Instant::now(), result.clone()));
    result
}

/// Best-effort email per account id, from Claude Code's CLI login and local Cowork records.
pub fn account_labels(ctx: &Ctx) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    if let Ok(Value::Object(root)) = fsx::read_json(&ctx.paths.claude_json, 64 * 1024 * 1024) {
        if let Some(Value::Object(oa)) = root.get("oauthAccount") {
            if let (Some(Value::String(id)), Some(Value::String(email))) =
                (oa.get("accountUuid"), oa.get("emailAddress"))
            {
                if is_uuid(id) {
                    labels.insert(id.clone(), email.clone());
                }
            }
        }
    }
    for (acct, _, path, is_link) in org_folders(&ctx.paths.cowork_sessions()) {
        if labels.contains_key(&acct) || is_link {
            continue;
        }
        let names: Vec<_> = sorted_entries(&path)
            .into_iter()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.starts_with("local_") && n.ends_with(".json")
            })
            .take(25)
            .collect();
        for e in names {
            if let Ok(Value::Object(d)) = fsx::read_json(&e.path(), MAX_RECORD_BYTES) {
                if let Some(Value::String(email)) = d.get("emailAddress") {
                    if email.contains('@') {
                        labels.insert(acct.clone(), email.clone());
                        break;
                    }
                }
            }
        }
    }
    labels
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, FakeDesktop, LogSink, Paths};
    use std::time::UNIX_EPOCH;

    const ADA: (&str, &str) = ("aaaaaaaa-0000-4000-8000-000000000001", "0a0a0a0a-0000-4000-8000-000000000001");
    const BOB: (&str, &str) = ("bbbbbbbb-0000-4000-8000-000000000002", "0b0b0b0b-0000-4000-8000-000000000002");
    const CHAT: &str = "11111111-0000-4000-8000-000000000001";
    const NOTES: &str = "22222222-0000-4000-8000-000000000002";

    /// Claude Desktop's index folders, and what a long-running agent keeps of them.
    struct Desk {
        _tmp: tempfile::TempDir,
        ctx: Ctx,
    }

    impl Desk {
        fn new() -> Desk {
            let tmp = tempfile::tempdir().unwrap();
            let mut paths = Paths::new(tmp.path().join("Claude"), tmp.path().join("state"));
            paths.projects = vec![tmp.path().join("projects")];
            paths.desktop_logs = tmp.path().join("logs");
            let fake = FakeDesktop { running: Some(false), active: None };
            Desk { _tmp: tmp, ctx: Ctx::new(paths, Config::default(), fake, LogSink::Silent) }
        }

        fn folder(&self, who: (&str, &str)) -> PathBuf {
            self.ctx.paths.surface_root(Surface::Code).join(who.0).join(who.1)
        }

        /// Desktop saves `session` for an account at second `at`: the title travels, the Remote
        /// Control mirror is the account's own.
        fn save(&self, who: (&str, &str), session: &str, title: &str, at: u64) {
            let record = serde_json::json!({
                "sessionId": format!("local_{session}"), "title": title, "bridgeSessionId": who.0,
            });
            fs::create_dir_all(self.folder(who)).unwrap();
            let path = self.folder(who).join(format!("local_{session}.json"));
            fs::write(&path, record.to_string()).unwrap();
            let file = fs::OpenOptions::new().write(true).open(path).unwrap();
            file.set_modified(UNIX_EPOCH + Duration::from_secs(1_800_000_000 + at)).unwrap();
        }

        /// One pass over every folder, as the agent and the app make them: each session's
        /// parsed record, per account.
        fn scan(&self) -> Vec<BTreeMap<String, Arc<Json>>> {
            discover(&self.ctx, Surface::Code)
                .iter()
                .map(|p| scan_partition(&self.ctx, p).records.into_iter().map(|(u, r)| (u, r.data.unwrap())).collect())
                .collect()
        }

        /// The folders records are kept of, and how many records in all.
        fn kept(&self) -> (Vec<PathBuf>, usize) {
            let cache = self.ctx.caches.records.lock().unwrap();
            let mut folders: Vec<PathBuf> = cache.keys().cloned().collect();
            folders.sort();
            (folders, cache.values().map(|recs| recs.len()).sum())
        }
    }

    #[test]
    fn a_session_saved_again_and_again_is_kept_once() {
        let desk = Desk::new();
        desk.save(ADA, NOTES, "notes", 0);
        // A session in use: Desktop rewrites its record after every turn, for weeks.
        for turn in 0..200 {
            desk.save(ADA, CHAT, &format!("turn {turn}"), turn);
            let seen = desk.scan();
            assert_eq!(seen[0][CHAT]["title"], format!("turn {turn}"));
            assert_eq!(desk.kept(), (vec![desk.folder(ADA)], 2));
        }
    }

    #[test]
    fn what_is_gone_from_disk_is_forgotten() {
        let desk = Desk::new();
        desk.save(ADA, CHAT, "chat", 0);
        desk.save(ADA, NOTES, "notes", 0);
        desk.save(BOB, CHAT, "chat", 0);
        desk.scan();
        assert_eq!(desk.kept(), (vec![desk.folder(ADA), desk.folder(BOB)], 3));
        // A session is deleted.
        fs::remove_file(desk.folder(ADA).join(format!("local_{NOTES}.json"))).unwrap();
        desk.scan();
        assert_eq!(desk.kept(), (vec![desk.folder(ADA), desk.folder(BOB)], 2));
        // An account's folder is removed: nothing scans it again, the next pass still drops it.
        fs::remove_dir_all(desk.folder(ADA)).unwrap();
        desk.scan();
        assert_eq!(desk.kept(), (vec![desk.folder(BOB)], 1));
    }

    #[test]
    fn copies_in_sync_share_one_parsed_record() {
        let desk = Desk::new();
        desk.save(ADA, CHAT, "chat", 0);
        desk.save(BOB, CHAT, "chat", 0);
        let seen = desk.scan();
        assert!(Arc::ptr_eq(&seen[0][CHAT], &seen[1][CHAT]), "the files differ only in what is the account's own");
        // One account moves on: the other still has what it had.
        desk.save(ADA, CHAT, "chat, continued", 1);
        let seen = desk.scan();
        assert_eq!(seen[0][CHAT]["title"], "chat, continued");
        assert_eq!(seen[1][CHAT]["title"], "chat");
        // The newer copy arrives there, and they are one again.
        desk.save(BOB, CHAT, "chat, continued", 1);
        let seen = desk.scan();
        assert!(Arc::ptr_eq(&seen[0][CHAT], &seen[1][CHAT]));
        assert_eq!(seen[1][CHAT]["title"], "chat, continued");
    }
}
