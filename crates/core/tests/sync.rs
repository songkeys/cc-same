//! End-to-end behaviour of the sync against fake Claude Desktop data folders.
//! Every test builds its own temporary tree; nothing touches real data.

use cc_same_core::apply::{apply_plan, run_sync};
use cc_same_core::plan::build_plan;
use cc_same_core::watch::{self, WatchOptions};
use cc_same_core::{
    desktop, fsx, scan, snapshot, Config, Ctx, FakeDesktop, LogSink, Partition, Paths, Plan, State, Surface,
};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const A: (&str, &str) = ("aaaaaaaa-0000-4000-8000-000000000001", "0a0a0a0a-0000-4000-8000-000000000001");
const B: (&str, &str) = ("bbbbbbbb-0000-4000-8000-000000000002", "0b0b0b0b-0000-4000-8000-000000000002");
const C: (&str, &str) = ("cccccccc-0000-4000-8000-000000000003", "0c0c0c0c-0000-4000-8000-000000000003");

fn key(p: (&str, &str)) -> String {
    format!("{}/{}", p.0, p.1)
}

fn uid() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (t >> 32) as u32,
        (t >> 16) as u16,
        n & 0xfff,
        (t & 0xfff) as u16,
        n
    )
}

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    user_data: PathBuf,
    state_dir: PathBuf,
    logs: PathBuf,
    projects: PathBuf,
}

impl Env {
    fn new() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let env = Env {
            user_data: root.join("Claude"),
            state_dir: root.join("state"),
            logs: root.join("logs"),
            projects: root.join("projects"),
            root,
            _tmp: tmp,
        };
        fs::create_dir_all(&env.user_data).unwrap();
        fs::create_dir_all(&env.logs).unwrap();
        fs::create_dir_all(env.projects.join("-tmp-proj")).unwrap();
        env
    }

    fn paths(&self) -> Paths {
        let mut p = Paths::new(self.user_data.clone(), self.state_dir.clone());
        p.desktop_logs = self.logs.clone();
        p.projects = vec![self.projects.clone()];
        p.claude_settings = self.root.join("claude-settings.json");
        p.claude_json = self.root.join("claude.json");
        p.synced_plugins = self.root.join("plugins-synced");
        p
    }

    /// `running`/`active` simulate Claude Desktop; `None` = read from the fake files.
    fn ctx_with(&self, running: bool, active: Option<Vec<String>>) -> Ctx {
        let paths = self.paths();
        let config = Config::load(&paths).unwrap();
        Ctx::new(paths, config, FakeDesktop { running: Some(running), active }, LogSink::Silent)
    }

    fn ctx(&self) -> Ctx {
        self.ctx_with(false, None)
    }

    fn code(&self, p: (&str, &str)) -> PathBuf {
        let d = self.user_data.join(Surface::Code.dir_name()).join(p.0).join(p.1);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn record(&self, p: (&str, &str), u: &str, mtime_ms: i64, fields: Value) -> PathBuf {
        let mut data = json!({
            "sessionId": format!("local_{u}"), "cliSessionId": format!("cli-{}", &u[..8]),
            "cwd": "/tmp/proj", "originCwd": "/tmp/proj", "title": "t", "isArchived": false,
            "createdAt": 1, "lastActivityAt": mtime_ms, "permissionMode": "default",
            "remoteMcpServersConfig": [], "bridgeSessionIds": [], "sessionPermissionUpdates": [],
        });
        for (k, v) in fields.as_object().unwrap() {
            data[k] = v.clone();
        }
        let path = self.code(p).join(format!("local_{u}.json"));
        fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        set_mtime(&path, mtime_ms);
        path
    }

    fn tomb(&self, p: (&str, &str), id: &str, ts_ms: i64) {
        fs::write(self.code(p).join(format!("deleted_{id}")), ts_ms.to_string()).unwrap();
    }

    fn read(&self, p: (&str, &str), u: &str) -> Value {
        let path = self.user_data.join(Surface::Code.dir_name()).join(p.0).join(p.1).join(format!("local_{u}.json"));
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn exists(&self, p: (&str, &str), name: &str) -> bool {
        self.user_data.join(Surface::Code.dir_name()).join(p.0).join(p.1).join(name).exists()
    }

    fn mtime_ms(&self, p: (&str, &str), name: &str) -> i128 {
        let md = fs::metadata(self.code(p).join(name)).unwrap();
        fsx::mtime_ns(&md).div_euclid(1_000_000)
    }

    fn sync(&self) {
        self.sync_with(&self.ctx());
    }

    fn sync_with(&self, ctx: &Ctx) -> cc_same_core::apply::Outcome {
        let (_, out) = run_sync(ctx, "test").unwrap();
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        out
    }

    fn plan(&self, ctx: &Ctx) -> Plan {
        build_plan(ctx, None, None).0
    }

    fn tasks(&self, p: (&str, &str), ids: &[&str], mtime_ms: i64) {
        let path = self.code(p).join("scheduled-tasks.json");
        let tasks: Vec<Value> = ids.iter().map(|i| json!({"id": i, "prompt": format!("p-{i}")})).collect();
        let body = serde_json::to_string_pretty(&json!({"scheduledTasks": tasks, "recordedSkips": {}})).unwrap();
        fs::write(&path, body).unwrap();
        set_mtime(&path, mtime_ms);
    }

    fn task_ids(&self, p: (&str, &str)) -> Vec<String> {
        let v: Value = serde_json::from_slice(&fs::read(self.code(p).join("scheduled-tasks.json")).unwrap()).unwrap();
        let mut ids: Vec<String> =
            v["scheduledTasks"].as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap().to_string()).collect();
        ids.sort();
        ids
    }

    fn trash_files(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![self.state_dir.join("trash")];
        while let Some(d) = stack.pop() {
            let Ok(rd) = fs::read_dir(d) else { continue };
            for e in rd.flatten() {
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                } else {
                    out.push(e.file_name().to_string_lossy().into_owned());
                }
            }
        }
        out
    }
}

fn set_mtime(path: &Path, ms: i64) {
    let f = fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::from_millis(ms as u64)).unwrap();
}

// ---------------------------------------------------------------------------- core merge

#[test]
fn three_accounts_newest_wins_regardless_of_order() {
    let e = Env::new();
    let (x, y) = (uid(), uid());
    e.code(A);
    e.record(B, &x, 100_000, json!({"title": "from B (older)"}));
    e.record(C, &x, 200_000, json!({"title": "from C (newest)"}));
    e.record(A, &y, 150_000, json!({"title": "only in A"}));
    e.sync();
    for p in [A, B, C] {
        assert_eq!(e.read(p, &x)["title"], "from C (newest)");
        assert_eq!(e.read(p, &y)["title"], "only in A");
    }
    // mtime travels with the copy, so a copy never looks newer than its source
    assert_eq!(e.mtime_ms(A, &format!("local_{x}.json")), 200_000);
    assert_eq!(e.mtime_ms(B, &format!("local_{y}.json")), 150_000);
    assert!(e.plan(&e.ctx()).is_empty(), "second run must be a no-op");
}

#[test]
fn written_files_are_private_single_link_regular_files() {
    let e = Env::new();
    let x = uid();
    e.record(A, &x, 100_000, json!({}));
    e.code(B);
    e.sync();
    let md = fs::symlink_metadata(e.code(B).join(format!("local_{x}.json"))).unwrap();
    assert!(md.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        assert_eq!(md.permissions().mode() & 0o777, 0o600);
        assert_eq!(md.nlink(), 1);
    }
    let leftovers: Vec<_> = fs::read_dir(e.code(B))
        .unwrap()
        .flatten()
        .filter(|d| d.file_name().to_string_lossy().starts_with(fsx::TMP_PREFIX))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn account_bound_fields_stay_with_their_account() {
    let e = Env::new();
    let x = uid();
    e.record(
        A,
        &x,
        200_000,
        json!({
            "title": "A title", "bridgeSessionIds": ["bridge-A"], "isStarred": true,
            "remoteMcpServersConfig": [{"uuid": "conn-A", "name": "a", "tools": []}],
            "enabledMcpTools": {"conn-A:tool": true}, "remoteControlAutoEligible": true,
            "publishedArtifacts": [{"url": "https://claude.ai/artifact/a"}],
            "sessionPermissionUpdates": [{"type": "addDirectories", "directories": ["/x"]}],
        }),
    );
    e.record(B, &x, 100_000, json!({"title": "B title", "bridgeSessionIds": ["bridge-B"], "isStarred": false}));
    e.code(C);
    e.sync();
    let b = e.read(B, &x);
    assert_eq!(b["title"], "A title");
    assert_eq!(b["bridgeSessionIds"], json!(["bridge-B"]));
    assert_eq!(b["isStarred"], json!(false));
    assert_eq!(b["remoteMcpServersConfig"], json!([])); // B keeps its own (empty) connector list
    assert!(b.get("publishedArtifacts").is_none());
    assert_eq!(b["sessionPermissionUpdates"], json!([{"type": "addDirectories", "directories": ["/x"]}]));
    let c = e.read(C, &x);
    for k in [
        "bridgeSessionIds",
        "isStarred",
        "remoteMcpServersConfig",
        "enabledMcpTools",
        "remoteControlAutoEligible",
        "publishedArtifacts",
    ] {
        assert!(c.get(k).is_none(), "{k} leaked into a new copy");
    }
    // B continues the session later: A gets the new content but keeps its own bindings
    e.record(B, &x, 300_000, json!({"title": "renamed in B", "bridgeSessionIds": ["bridge-B"], "isStarred": false}));
    e.sync();
    let a = e.read(A, &x);
    assert_eq!(a["title"], "renamed in B");
    assert_eq!(a["bridgeSessionIds"], json!(["bridge-A"]));
    assert_eq!(a["isStarred"], json!(true));
    assert_eq!(a["remoteMcpServersConfig"][0]["uuid"], "conn-A");
    assert!(e.plan(&e.ctx()).is_empty());
}

// ---------------------------------------------------------------------------- deletions

#[test]
fn deletion_travels_as_a_marker_and_goes_to_trash() {
    let e = Env::new();
    let x = uid();
    e.record(A, &x, 100_000, json!({"cliSessionId": "cli-x"}));
    e.code(B);
    e.sync();
    assert!(e.exists(B, &format!("local_{x}.json")));
    // Desktop deletes in A: record removed, markers for the session id and its transcript
    fs::remove_file(e.code(A).join(format!("local_{x}.json"))).unwrap();
    e.tomb(A, &x, 500_000);
    e.tomb(A, "cli-x", 500_000);
    e.sync();
    assert!(!e.exists(B, &format!("local_{x}.json")));
    assert!(e.exists(B, &format!("deleted_{x}")));
    assert!(e.exists(B, "deleted_cli-x"));
    assert!(e.trash_files().contains(&format!("local_{x}.json")));
}

#[test]
fn newer_session_beats_an_older_marker() {
    let e = Env::new();
    let x = uid();
    e.tomb(A, &x, 100_000);
    e.record(B, &x, 200_000, json!({"title": "alive again"}));
    e.sync();
    assert_eq!(e.read(A, &x)["title"], "alive again");
    assert!(!e.exists(A, &format!("deleted_{x}")));
}

#[test]
fn deleted_session_is_not_seeded_anywhere() {
    let e = Env::new();
    let x = uid();
    e.record(B, &x, 100_000, json!({}));
    e.tomb(A, &x, 200_000);
    e.code(C);
    e.sync();
    for p in [A, B, C] {
        assert!(!e.exists(p, &format!("local_{x}.json")));
        assert!(e.exists(p, &format!("deleted_{x}")));
    }
}

// ---------------------------------------------------------------------------- Desktop running

#[test]
fn loaded_index_is_never_modified_then_catches_up_after_quit() {
    let e = Env::new();
    let x = uid();
    e.record(A, &x, 100_000, json!({"title": "old"}));
    e.code(B);
    e.sync(); // both known now
    e.record(B, &x, 200_000, json!({"title": "newer in B"}));
    let path_a = e.code(A).join(format!("local_{x}.json"));
    let before = fs::read(&path_a).unwrap();
    let running = e.ctx_with(true, Some(vec![key(A)]));
    let plan = e.plan(&running);
    assert!(plan.actions.iter().all(|a| a.part.acct != A.0));
    assert!(plan.deferred_total() > 0);
    e.sync_with(&running);
    assert_eq!(fs::read(&path_a).unwrap(), before);
    e.sync(); // Desktop quit
    assert_eq!(e.read(A, &x)["title"], "newer in B");
}

#[test]
fn unknown_active_account_means_hands_off_everything() {
    let e = Env::new();
    let x = uid();
    e.record(A, &x, 100_000, json!({}));
    e.code(B);
    e.sync();
    e.record(A, &x, 200_000, json!({"title": "changed"}));
    let ctx = e.ctx_with(true, Some(vec![]));
    assert!(e.plan(&ctx).is_empty());
}

#[test]
fn first_join_seeds_a_loaded_new_account_additively() {
    let e = Env::new();
    let (x, y, z) = (uid(), uid(), uid());
    e.record(A, &x, 100_000, json!({"title": "x"}));
    e.record(B, &y, 100_000, json!({"title": "y"}));
    e.sync();
    // a brand-new account C signs in; Desktop has it loaded and already saved session z
    e.record(C, &z, 300_000, json!({"title": "z made in C"}));
    let running = e.ctx_with(true, Some(vec![key(C)]));
    let out = e.sync_with(&running);
    assert!(out.seeded_while_loaded.get(&key(C)).copied().unwrap_or(0) >= 2);
    assert_eq!(e.read(C, &x)["title"], "x");
    assert_eq!(e.read(C, &y)["title"], "y");
    assert_eq!(e.read(A, &z)["title"], "z made in C");
    assert!(State::load(&running.paths).pending_restart.is_some());
    // C is known now: later changes wait while it is loaded
    e.record(A, &x, 400_000, json!({"title": "x v2"}));
    e.sync_with(&running);
    assert_eq!(e.read(C, &x)["title"], "x");
    e.sync();
    assert_eq!(e.read(C, &x)["title"], "x v2");
    assert!(State::load(&running.paths).pending_restart.is_none(), "the hint clears once Claude quits");
}

#[test]
fn additive_write_never_clobbers_a_file_that_appeared() {
    let e = Env::new();
    let x = uid();
    e.record(A, &x, 100_000, json!({"title": "from A"}));
    e.code(B);
    let ctx = e.ctx();
    let mut state = State::load(&ctx.paths);
    let (plan, _, _) = build_plan(&ctx, None, Some(&state));
    e.record(B, &x, 150_000, json!({"title": "Desktop wrote this meanwhile"}));
    apply_plan(&ctx, plan, &mut state, "test").unwrap();
    assert_eq!(e.read(B, &x)["title"], "Desktop wrote this meanwhile");
}

// ---------------------------------------------------------------------------- robustness

#[test]
fn unreadable_record_is_left_alone() {
    let e = Env::new();
    let x = uid();
    let bad = e.code(A).join(format!("local_{x}.json"));
    fs::write(&bad, "{not json").unwrap();
    e.record(B, &x, 100_000, json!({"title": "good"}));
    e.code(C);
    let plan = e.plan(&e.ctx());
    assert!(plan.notes.iter().any(|n| n.contains("unreadable")));
    e.sync();
    assert_eq!(fs::read_to_string(&bad).unwrap(), "{not json");
    assert_eq!(e.read(C, &x)["title"], "good");
}

#[test]
fn mismatched_session_id_is_rejected() {
    let e = Env::new();
    let (x, other) = (uid(), uid());
    e.record(A, &x, 100_000, json!({"sessionId": format!("local_{other}")}));
    e.code(B);
    e.sync();
    assert!(!e.exists(B, &format!("local_{x}.json")));
}

#[cfg(unix)]
#[test]
fn symlinked_index_is_skipped_and_can_be_fixed() {
    let e = Env::new();
    let x = uid();
    e.record(A, &x, 100_000, json!({}));
    let link_parent = e.user_data.join(Surface::Code.dir_name()).join(B.0);
    fs::create_dir_all(&link_parent).unwrap();
    std::os::unix::fs::symlink(e.code(A), link_parent.join(B.1)).unwrap();
    let plan = e.plan(&e.ctx());
    assert!(plan.notes.iter().any(|n| n.contains("symlink")));
    assert!(plan.actions.iter().all(|a| a.part.acct != B.0));
    let fixed = snapshot::fix_symlinks(&e.ctx()).unwrap();
    assert_eq!(fixed.len(), 1);
    assert!(!fsx::is_symlink(&link_parent.join(B.1)));
    assert!(e.exists(B, &format!("local_{x}.json")));
    assert!(e.exists(A, &format!("local_{x}.json")));
}

#[test]
fn damaged_newest_copy_loses_to_a_healthy_copy_with_transcript() {
    let e = Env::new();
    let x = uid();
    fs::write(e.projects.join("-tmp-proj").join("cli-good.jsonl"), "{}\n").unwrap();
    // It remembers an earlier transcript, not the one the healthy copy points at.
    let damaged = json!({
        "cliSessionId": null, "transcriptUnavailable": true, "priorCliSessionIds": ["cli-earlier"], "title": "damaged",
    });
    e.record(A, &x, 300_000, damaged);
    e.record(B, &x, 200_000, json!({"cliSessionId": "cli-good", "title": "healthy"}));
    let plan = e.plan(&e.ctx());
    assert!(plan.notes.iter().any(|n| n.contains("transcript")));
    e.sync();
    assert_eq!(e.read(A, &x)["cliSessionId"], "cli-good");
    assert!(e.read(A, &x).get("transcriptUnavailable").is_none());
}

#[test]
fn a_cleared_conversation_is_not_a_lost_transcript() {
    let e = Env::new();
    let x = uid();
    fs::write(e.projects.join("-tmp-proj").join("cli-old.jsonl"), "{}\n").unwrap();
    e.record(A, &x, 100_000, json!({"cliSessionId": "cli-old"}));
    e.code(B);
    e.sync();
    // Cleared, then archived, in the account Claude has open. Desktop lists the transcript it
    // left in priorCliSessionIds before dropping cliSessionId.
    e.record(A, &x, 200_000, json!({"cliSessionId": null, "priorCliSessionIds": ["cli-old"], "isArchived": true}));
    let running = e.ctx_with(true, Some(vec![key(A)]));
    let plan = e.plan(&running);
    assert_eq!(plan.deferred_total(), 0);
    assert!(!plan.notes.iter().any(|n| n.contains("transcript")));
    e.sync_with(&running);
    assert!(e.read(B, &x)["cliSessionId"].is_null());
    assert_eq!(e.read(B, &x)["isArchived"], true);
    e.sync(); // Claude quit
    assert!(e.read(A, &x)["cliSessionId"].is_null());
    assert_eq!(e.read(A, &x)["isArchived"], true);
}

#[test]
fn excluded_partition_is_untouched() {
    let e = Env::new();
    let x = uid();
    e.record(A, &x, 100_000, json!({}));
    e.code(B);
    let ctx = e.ctx();
    let mut cfg = ctx.config();
    cfg.exclude.push(B.0.to_string());
    ctx.set_config(cfg);
    e.sync_with(&ctx);
    assert!(!e.exists(B, &format!("local_{x}.json")));
}

// ---------------------------------------------------------------------------- collections & hint

#[test]
fn collection_three_way_merge() {
    let e = Env::new();
    e.tasks(A, &["t1", "t2"], 200_000);
    e.tasks(B, &["t1", "t3"], 100_000);
    e.sync();
    assert_eq!(e.task_ids(A), ["t1", "t2", "t3"]);
    assert_eq!(e.task_ids(B), ["t1", "t2", "t3"]);
    // formatting follows Desktop's file
    assert!(fs::read_to_string(e.code(B).join("scheduled-tasks.json")).unwrap().starts_with("{\n  \""));
    // A deletes t2 -> gone everywhere
    e.tasks(A, &["t1", "t3"], 300_000);
    e.sync();
    assert_eq!(e.task_ids(B), ["t1", "t3"]);
    // a new account joining with its own task can add, not remove
    e.tasks(C, &["t9"], 400_000);
    e.sync();
    for p in [A, B, C] {
        assert_eq!(e.task_ids(p), ["t1", "t3", "t9"]);
    }
    assert!(e.plan(&e.ctx()).is_empty());
}

#[test]
fn collection_write_skipped_at_apply_keeps_the_old_base() {
    let e = Env::new();
    e.tasks(A, &["t1", "t2"], 100_000);
    e.tasks(B, &["t1", "t2"], 100_000);
    e.sync();
    e.tasks(A, &["t1"], 200_000); // A deletes t2
    let planner = e.ctx();
    let mut state = State::load(&planner.paths);
    let (plan, _, _) = build_plan(&planner, None, Some(&state));
    // Claude loads B between planning and applying: the write to B must be skipped
    let loaded_b = e.ctx_with(true, Some(vec![key(B)]));
    apply_plan(&loaded_b, plan, &mut state, "test").unwrap();
    assert_eq!(e.task_ids(B), ["t1", "t2"]);
    e.sync(); // Claude quit: B must now lose t2, not resurrect it in A
    assert_eq!(e.task_ids(A), ["t1"]);
    assert_eq!(e.task_ids(B), ["t1"]);
}

#[test]
fn archive_hint_matches_desktop_format() {
    let e = Env::new();
    let (x, y) = (uid(), uid());
    e.record(A, &x, 100_000, json!({"isArchived": true}));
    e.record(A, &y, 100_000, json!({"isArchived": false}));
    e.code(B);
    e.sync();
    let raw = fs::read(e.code(B).join("archived-sessions.idx")).unwrap();
    assert_eq!(raw, format!("{{\"v\":1,\"archived\":[\"local_{x}\"]}}").into_bytes());
}

// ---------------------------------------------------------------------------- snapshots

#[test]
fn baseline_snapshot_and_restore() {
    let e = Env::new();
    let x = uid();
    e.record(A, &x, 100_000, json!({}));
    e.code(B);
    e.sync();
    let base = State::load(&e.paths()).baseline.expect("baseline snapshot");
    let snap_b = e.state_dir.join("snapshots").join(&base).join("code").join(B.0).join(B.1);
    assert_eq!(fs::read_dir(&snap_b).unwrap().count(), 0, "B was empty before the first sync");
    snapshot::restore(&e.ctx(), &base).unwrap();
    assert!(!e.exists(B, &format!("local_{x}.json")));
    assert!(e.exists(A, &format!("local_{x}.json")));
}

#[test]
fn restore_refuses_while_claude_runs() {
    let e = Env::new();
    e.record(A, &uid(), 100_000, json!({}));
    e.code(B);
    e.sync();
    let base = State::load(&e.paths()).baseline.unwrap();
    assert!(snapshot::restore(&e.ctx_with(true, None), &base).is_err());
}

// ---------------------------------------------------------------------------- Cowork

#[test]
fn cowork_sessions_are_left_alone() {
    let e = Env::new();
    // Up to 0.1.9 a config could ask for Cowork too.
    fs::create_dir_all(&e.state_dir).unwrap();
    fs::write(e.paths().config_file(), r#"{"surfaces": ["code", "cowork"]}"#).unwrap();
    let cowork = |p: (&str, &str)| {
        let d = e.paths().cowork_sessions().join(p.0).join(p.1);
        fs::create_dir_all(&d).unwrap();
        d
    };
    let x = uid();
    let record = json!({"sessionId": format!("local_{x}"), "emailAddress": "a@example.com"});
    fs::write(cowork(A).join(format!("local_{x}.json")), record.to_string()).unwrap();
    fs::create_dir_all(cowork(A).join(format!("local_{x}")).join("outputs")).unwrap();
    let y = uid();
    e.record(A, &y, 100_000, json!({}));
    e.code(B);
    let b = cowork(B);
    let ctx = e.ctx();
    e.sync_with(&ctx);
    assert!(e.read(B, &y).is_object());
    assert_eq!(fs::read_dir(&b).unwrap().count(), 0);
    // Their records still tell which account is whose.
    assert_eq!(scan::account_labels(&ctx).get(A.0).map(String::as_str), Some("a@example.com"));
}

// ---------------------------------------------------------------------------- the agent

fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn activate(e: &Env, p: (&str, &str)) {
    use std::io::Write;
    let mut log = fs::OpenOptions::new().create(true).append(true).open(e.logs.join("main.log")).unwrap();
    writeln!(
        log,
        "2026-09-28 10:00:00 [info] [LocalSessionManager] Initialization succeeded \u{2014} accountId={}, orgId={}, existingSessions=0",
        p.0, p.1
    )
    .unwrap();
    fs::write(e.user_data.join("config.json"), json!({"lastKnownAccountUuid": p.0}).to_string()).unwrap();
}

/// What people are told is open: the account Claude signed in to last, with the organization
/// its log names for that account. Writing stays as careful as before.
#[test]
fn the_open_account_is_the_one_signed_in_to() {
    let e = Env::new();
    let list = |p: (&str, &str)| Partition {
        surface: Surface::Code,
        acct: p.0.into(),
        org: p.1.into(),
        path: e.code(p),
        is_link: false,
    };
    let running = |on: bool| {
        Ctx::new(e.paths(), Config::default(), FakeDesktop { running: Some(on), active: None }, LogSink::Silent)
    };
    activate(&e, A);
    let app = desktop::detect(&running(true));
    assert_eq!((app.open_account.as_deref(), app.open_org.as_deref()), (Some(A.0), Some(A.1)));
    assert!(app.showing(&list(A)) && !app.showing(&list((A.0, B.1))) && !app.showing(&list(B)));
    // Signed in to B since, and the newest log line is still about A (an older log file, say):
    // B is open, in an organization we cannot name; both wait to be written.
    fs::write(e.user_data.join("config.json"), json!({"lastKnownAccountUuid": B.0}).to_string()).unwrap();
    let app = desktop::detect(&running(true));
    assert_eq!((app.open_account.as_deref(), app.open_org.as_deref()), (Some(B.0), None));
    assert!(app.showing(&list(B)) && app.showing(&list((B.0, A.1))) && !app.showing(&list(A)));
    assert!(app.loaded(&list(A)) && app.loaded(&list(B)));
    // Closed: nothing is open.
    let app = desktop::detect(&running(false));
    assert_eq!((app.open_account, app.open_org), (None, None));
}

/// After days of running, the line about the account in use sits far back in `main.log`. It
/// still wins over an older log file's line about another account.
#[test]
fn a_line_far_back_in_the_log_still_counts() {
    use std::io::Write;
    let e = Env::new();
    let line = |p: (&str, &str)| {
        format!(
            "2026-09-28 10:00:00 [info] [LocalSessionManager] Initialization succeeded \u{2014} accountId={}, orgId={}, existingSessions=0\n",
            p.0, p.1
        )
    };
    let older = e.logs.join("main1.log");
    fs::write(&older, line(B)).unwrap();
    set_mtime(&older, 1_000_000);
    let mut log = fs::File::create(e.logs.join("main.log")).unwrap();
    log.write_all(line(A).as_bytes()).unwrap();
    let chatter = "2026-09-28 10:00:01 [info] [other] something else happened\n".repeat(100_000);
    log.write_all(chatter.as_bytes()).unwrap();
    assert!(fs::metadata(e.logs.join("main.log")).unwrap().len() > 5 * 1024 * 1024);
    let ctx =
        Ctx::new(e.paths(), Config::default(), FakeDesktop { running: Some(true), active: None }, LogSink::Silent);
    let app = desktop::detect(&ctx);
    assert_eq!(app.pairs.iter().collect::<Vec<_>>(), [&(A.0.to_string(), A.1.to_string())]);
    assert_eq!(app.open_account.as_deref(), Some(A.0));
}

/// Claude runs the whole time; the active account is read the real way, from its config.json
/// and main.log, while the agent loop runs on its own thread.
#[test]
fn switching_accounts_while_claude_runs() {
    let e = Env::new();
    let (x, y) = (uid(), uid());
    activate(&e, A);
    e.record(A, &x, 100_000, json!({"title": "v1"}));
    e.code(B);
    let paths = e.paths();
    let config = Config { switch_grace_seconds: 0.0, notify: false, ..Config::default() };
    let ctx =
        Arc::new(Ctx::new(paths, config.clone(), FakeDesktop { running: Some(true), active: None }, LogSink::Silent));
    config.save(&ctx.paths).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let agent = {
        let (ctx, stop) = (ctx.clone(), stop.clone());
        std::thread::spawn(move || {
            watch::run(&ctx, &WatchOptions { interval: Duration::from_millis(150), iterations: None }, &stop)
        })
    };
    let t = Duration::from_secs(20);
    // 1. A is in use: its changes flow out to B, and A itself is never written
    assert!(wait_for(t, || e.exists(B, &format!("local_{x}.json")) && e.read(B, &x)["title"] == "v1"));
    let path_a = e.record(A, &x, 200_000, json!({"title": "v2"}));
    assert!(wait_for(t, || e.read(B, &x)["title"] == "v2"));
    assert_eq!(fsx::mtime_ns(&fs::metadata(&path_a).unwrap()), 200_000 * 1_000_000);
    // 2. switch to B, continue the session there and start a new one
    activate(&e, B);
    std::thread::sleep(Duration::from_millis(600));
    e.record(B, &x, 300_000, json!({"title": "v3 in B"}));
    e.record(B, &y, 310_000, json!({"title": "new in B"}));
    assert!(wait_for(t, || e.read(A, &x)["title"] == "v3 in B"));
    assert!(wait_for(t, || e.exists(A, &format!("local_{y}.json")) && e.read(A, &y)["title"] == "new in B"));
    // 3. delete x in B -> it leaves A too
    fs::remove_file(e.code(B).join(format!("local_{x}.json"))).unwrap();
    e.tomb(B, &x, 400_000);
    assert!(wait_for(t, || !e.exists(A, &format!("local_{x}.json"))));
    // 4. sign in to a brand-new account C: its folder is created and seeded
    activate(&e, C);
    assert!(wait_for(t, || e.exists(C, &format!("local_{y}.json"))));
    assert!(!e.exists(C, &format!("local_{x}.json")));
    assert!(wait_for(t, || State::load(&ctx.paths).notified.contains(&key(C))));
    assert!(e.state_dir.join("agent.json").exists(), "the agent writes a heartbeat");
    stop.store(true, Ordering::Relaxed);
    agent.join().unwrap();
}

// ---------------------------------------------------------------------------- what stays home

/// Connectors, org plugins and published artifacts stay with their account; the inventory says
/// which account has which, and what each one lacks.
#[test]
fn the_inventory_tells_what_stays_with_each_account() {
    use cc_same_core::inventory;
    let e = Env::new();
    let linear = json!({"name": "Linear", "url": "https://mcp.linear.app/sse", "uuid": "u1", "tools": []});
    let slack = json!({"name": "Slack", "url": "https://mcp.slack.com", "uuid": "u2", "tools": []});
    let (x, y, z) = (uid(), uid(), uid());
    e.record(
        A,
        &x,
        100_000,
        json!({
            "remoteMcpServersConfig": [linear.clone(), slack],
            "publishedArtifacts": [{"url": "https://claude.ai/artifact/one", "title": "One", "updatedAt": 1_000_000}],
        }),
    );
    // Another session lists the same artifact, edited later.
    e.record(
        A,
        &y,
        100_000,
        json!({
            "publishedArtifacts": [
                {"url": "https://claude.ai/artifact/one", "title": "One, edited", "updatedAt": 2_000_000},
                {"url": "https://claude.ai/artifact/two", "title": "Two", "updatedAt": 1_500_000},
            ],
        }),
    );
    e.record(B, &z, 100_000, json!({"remoteMcpServersConfig": [linear]}));
    let synced = e.root.join("plugins-synced").join(format!("{}_{}", B.1, B.0));
    fs::create_dir_all(&synced).unwrap();
    let manifest = json!({"plugins": [{"name": "legal", "marketplaceName": "knowledge-work-plugins"}]});
    fs::write(synced.join("manifest.json"), manifest.to_string()).unwrap();

    let held = inventory::read(&e.ctx());
    let a = held.of(A.0).unwrap();
    assert_eq!(a.connectors.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Linear", "Slack"]);
    let titles: Vec<_> = a.artifacts.iter().map(|x| x.title.as_deref().unwrap()).collect();
    assert_eq!(titles, ["One, edited", "Two"]);
    assert!(a.plugins.is_empty());
    let b = held.of(B.0).unwrap();
    assert_eq!(b.plugins.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["legal"]);
    assert!(b.artifacts.is_empty());

    let missing = held.missing(A.0);
    assert!(missing.connectors.is_empty());
    assert_eq!(missing.plugins.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["legal"]);
    let missing = held.missing(B.0);
    assert_eq!(missing.connectors.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Slack"]);
    assert!(missing.plugins.is_empty());

    // A sync copies sessions, never what belongs to an account: the inventory stays as it was.
    e.sync();
    assert_eq!(inventory::read(&e.ctx()), held);
}

/// The inventory reads records with the scanner's care: never through a symlinked folder, never a
/// file that is not a session's own; and what it cannot read is counted, not taken for "none".
#[cfg(unix)]
#[test]
fn the_inventory_trusts_only_what_the_scanner_trusts() {
    use cc_same_core::inventory;
    let e = Env::new();
    let slack = json!({"name": "Slack", "url": "https://mcp.slack.com", "uuid": "u2", "tools": []});
    let (x, y) = (uid(), uid());
    e.record(A, &x, 100_000, json!({"remoteMcpServersConfig": [slack.clone()]}));
    // B's folder is a link to A's: A's connectors are not B's.
    let b = e.user_data.join(Surface::Code.dir_name()).join(B.0);
    fs::create_dir_all(&b).unwrap();
    std::os::unix::fs::symlink(e.code(A), b.join(B.1)).unwrap();
    // A record that names another session, a damaged one, and a FIFO: none of them count, and the
    // FIFO is never opened.
    let other = e.record(C, &y, 100_000, json!({"remoteMcpServersConfig": [slack]}));
    fs::rename(&other, e.code(C).join(format!("local_{}.json", uid()))).unwrap();
    fs::write(e.code(C).join(format!("local_{}.json", uid())), "{not json").unwrap();
    let fifo = std::ffi::CString::new(e.code(C).join(format!("local_{}.json", uid())).to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    // A plugin list that cannot be read.
    let synced = e.root.join("plugins-synced").join(format!("{}_{}", C.1, C.0));
    fs::create_dir_all(&synced).unwrap();
    fs::write(synced.join("manifest.json"), "[").unwrap();

    let held = inventory::read(&e.ctx());
    assert_eq!(held.of(A.0).unwrap().connectors.len(), 1);
    let b = held.of(B.0).unwrap();
    assert!(b.connectors.is_empty());
    assert_eq!(b.unreadable, 1);
    let c = held.of(C.0).unwrap();
    assert!(c.connectors.is_empty() && c.plugins.is_empty());
    assert_eq!(c.unreadable, 4);
}

/// Plugins of the same name from different marketplaces are different plugins.
#[test]
fn plugins_are_told_apart_by_marketplace() {
    use cc_same_core::inventory;
    let e = Env::new();
    for (p, market) in [(A, "company"), (B, "community")] {
        e.code(p);
        let synced = e.root.join("plugins-synced").join(format!("{}_{}", p.1, p.0));
        fs::create_dir_all(&synced).unwrap();
        let manifest = json!({"plugins": [{"name": "legal", "marketplaceName": market}]});
        fs::write(synced.join("manifest.json"), manifest.to_string()).unwrap();
    }
    let held = inventory::read(&e.ctx());
    let missing = held.missing(A.0);
    assert_eq!(missing.plugins.len(), 1);
    assert_eq!(missing.plugins[0].marketplace.as_deref(), Some("community"));
}
