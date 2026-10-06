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

fn load_record(ctx: &Ctx, path: &Path, uuid: &str, md: &fs::Metadata) -> Record {
    let key = (path.to_path_buf(), fsx::mtime_ns(md), md.len());
    if let Some(hit) = ctx.caches.records.lock().unwrap().get(&key) {
        return hit.clone();
    }
    let mut rec = Record {
        uuid: uuid.to_string(),
        path: path.to_path_buf(),
        mtime_ns: fsx::mtime_ns(md),
        size: md.len(),
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
    if cache.len() > 20_000 {
        cache.clear();
    }
    cache.insert(key, rec.clone());
    rec
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
                ps.records.insert(uuid.to_string(), load_record(ctx, &e.path(), uuid, &md));
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

/// The account Claude Code's command line is signed in to, and its email when known: its
/// `oauthAccount` in `~/.claude.json`. Only who it is; the sign-in itself is in the keychain.
pub fn cli_account(ctx: &Ctx) -> Option<(String, Option<String>)> {
    let Ok(Value::Object(root)) = fsx::read_json(&ctx.paths.claude_json, 64 * 1024 * 1024) else {
        return None;
    };
    let Some(Value::Object(oa)) = root.get("oauthAccount") else { return None };
    let id = oa.get("accountUuid").and_then(Value::as_str).filter(|id| is_uuid(id))?;
    let email = oa.get("emailAddress").and_then(Value::as_str).map(str::to_string);
    Some((id.to_string(), email))
}

/// Best-effort email per account id, from Claude Code's CLI login and local Cowork records.
pub fn account_labels(ctx: &Ctx) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    if let Some((id, Some(email))) = cli_account(ctx) {
        labels.insert(id, email);
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
