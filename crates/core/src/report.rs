//! A read-only picture of everything, shared by the CLI's `doctor` and the app.

use crate::config::{LastSync, PendingRestart, State};
use crate::ctx::Ctx;
use crate::desktop;
use crate::fsx;
use crate::model::*;
use crate::plan::build_plan;
use crate::scan;
use crate::service::{self, Heartbeat, ServiceStatus};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct PartitionView {
    pub part: Partition,
    pub email: Option<String>,
    pub sessions: usize,
    pub archived: usize,
    pub markers: usize,
    /// Live sessions (anywhere in the group) this index does not have yet.
    pub missing: usize,
    pub unreadable: usize,
    pub loaded: bool,
    pub excluded: bool,
    pub error: Option<String>,
    /// Collection file -> number of items (None: unreadable).
    pub collections: Vec<(String, Option<usize>)>,
    pub unmanaged: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct SurfaceView {
    pub surface: Surface,
    pub partitions: Vec<PartitionView>,
    /// Live sessions across all included partitions.
    pub union: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Warning {
    /// Index folders turned into symlinks by another tool: Claude silently stops saving there.
    SymlinkedFolders { surface: Surface, count: usize },
    /// `<org>.bak-*` style folders from other tools.
    LeftoverFolders { surface: Surface, dirs: Vec<String> },
    /// Linked partitions belong to more than one organization.
    SpansOrgs { surface: Surface, orgs: usize },
    /// Claude Code deletes the transcripts of Desktop sessions within a year; a session without
    /// one opens empty.
    ShortRetention { days: f64, limited_by: crate::retention::Limit, path: PathBuf },
    /// Sessions whose transcript is no longer on disk.
    MissingTranscripts { missing: usize, total: usize },
    /// Claude Desktop's data folder was not found.
    DesktopDataMissing { path: PathBuf },
    /// Claude Code in the terminal is signed in to another account than Claude Desktop, so what
    /// runs there counts against that account's plan when it uses that login (not an API key or a
    /// cloud provider).
    CliOnOtherAccount { cli: String, desktop: String },
}

#[derive(Clone, Debug, Default)]
pub struct PlanView {
    /// Pending actions per partition, grouped by a readable kind.
    pub per_partition: Vec<(Partition, BTreeMap<&'static str, usize>)>,
    pub total: usize,
    /// Distinct sessions that would be copied or updated somewhere.
    pub sessions: usize,
    pub deferred: BTreeMap<String, usize>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Overview {
    pub desktop_version: Option<String>,
    pub app: AppState,
    pub labels: BTreeMap<String, String>,
    pub surfaces: Vec<SurfaceView>,
    pub plan: PlanView,
    pub warnings: Vec<Warning>,
    /// How long Claude Code keeps the transcripts behind these sessions.
    pub retention: crate::retention::Retention,
    /// Who Claude is signed in to, and the sign-ins set aside for switching.
    pub logins: crate::logins::Logins,
    /// The numbered list of accounts to switch between.
    pub roster: crate::accounts::Roster,
    /// What each account last used of its plan, as Claude last saw it.
    pub usage: BTreeMap<String, crate::usage::Usage>,
    pub service: ServiceStatus,
    pub heartbeat: Option<Heartbeat>,
    pub last_sync: Option<LastSync>,
    pub baseline: Option<String>,
    pub pending_restart: Option<PendingRestart>,
    pub config: crate::config::Config,
}

impl Overview {
    pub fn surface(&self, s: Surface) -> Option<&SurfaceView> {
        self.surfaces.iter().find(|v| v.surface == s)
    }

    /// The restart hint, if it still applies (Desktop has not reloaded since).
    pub fn restart_hint(&self) -> Option<&PendingRestart> {
        let hint = self.pending_restart.as_ref()?;
        (self.app.running && self.app.init_marker == hint.init_marker).then_some(hint)
    }
}

pub fn kind_label(a: &Action) -> &'static str {
    match a.kind {
        ActionKind::WriteRecord if a.additive => "new sessions",
        ActionKind::WriteRecord => "updated sessions",
        ActionKind::TrashRecord => "deleted sessions",
        ActionKind::WriteTomb => "deletion markers",
        ActionKind::TrashTomb => "stale deletion markers",
        ActionKind::WriteCollection => "merged task lists",
        ActionKind::WriteArchiveIdx => "archive hint",
    }
}

pub fn summarize(plan: &Plan) -> PlanView {
    let mut per: Vec<(Partition, BTreeMap<&'static str, usize>)> = Vec::new();
    for a in &plan.actions {
        let idx = match per.iter().position(|(p, _)| *p == a.part) {
            Some(i) => i,
            None => {
                per.push((a.part.clone(), BTreeMap::new()));
                per.len() - 1
            }
        };
        *per[idx].1.entry(kind_label(a)).or_default() += 1;
    }
    let sessions: std::collections::BTreeSet<&str> =
        plan.actions.iter().filter(|a| a.kind == ActionKind::WriteRecord).map(|a| a.name.as_str()).collect();
    PlanView {
        per_partition: per,
        total: plan.actions.len(),
        sessions: sessions.len(),
        deferred: plan.deferred.clone(),
        notes: plan.notes.clone(),
    }
}

/// Everything at once. Nothing of Claude's is changed; CC Same's list of accounts is brought up
/// to date ([`crate::accounts::observe`]).
pub fn overview(ctx: &Ctx) -> Overview {
    let cfg = ctx.config();
    let roster = crate::accounts::observe(ctx);
    let usage = crate::accounts::usage(ctx, &roster);
    let app = desktop::detect(ctx);
    let state = State::load(&ctx.paths);
    let labels = scan::account_labels(ctx);
    let mut warnings = Vec::new();
    if !ctx.paths.user_data.is_dir() {
        warnings.push(Warning::DesktopDataMissing { path: ctx.paths.user_data.clone() });
    }
    let mut surfaces = Vec::new();
    for surface in Surface::ALL {
        let parts = scan::discover(ctx, surface);
        if parts.is_empty() {
            continue;
        }
        let states: Vec<PartState> = parts.iter().map(|p| scan::scan_partition(ctx, p)).collect();
        let included: Vec<PartState> =
            states.iter().filter(|s| !cfg.is_excluded(&s.part.acct, &s.part.org)).cloned().collect();
        let union = scan::union_of(&included);
        let mut views = Vec::new();
        for s in &states {
            views.push(PartitionView {
                email: labels.get(&s.part.acct).cloned(),
                sessions: s.live_sessions(),
                archived: s.archived_sessions(),
                markers: s.tombs.len(),
                missing: union.iter().filter(|u| !s.records.contains_key(*u)).count(),
                unreadable: s.records.values().filter(|r| r.data.is_none()).count(),
                loaded: app.loaded(&s.part),
                excluded: cfg.is_excluded(&s.part.acct, &s.part.org),
                error: s.error.clone(),
                collections: s
                    .collections
                    .iter()
                    .map(|(rel, c)| {
                        (
                            rel.to_string(),
                            c.data.as_ref().map(|d| d.values().find_map(|v| v.as_array().map(Vec::len)).unwrap_or(0)),
                        )
                    })
                    .collect(),
                unmanaged: s.unmanaged.clone(),
                part: s.part.clone(),
            });
        }
        let links = states.iter().filter(|s| s.part.is_link).count();
        if links > 0 {
            warnings.push(Warning::SymlinkedFolders { surface, count: links });
        }
        let left = scan::leftover_dirs(ctx, surface);
        if !left.is_empty() {
            warnings.push(Warning::LeftoverFolders { surface, dirs: left });
        }
        let orgs: std::collections::BTreeSet<&str> = included.iter().map(|s| s.part.org.as_str()).collect();
        if orgs.len() > 1 {
            warnings.push(Warning::SpansOrgs { surface, orgs: orgs.len() });
        }
        if let Some(tx) = scan::transcript_index(ctx) {
            let mut sessions: BTreeMap<&str, Option<&str>> = BTreeMap::new();
            for s in &included {
                for (u, r) in &s.records {
                    if let Some(d) = &r.data {
                        sessions.insert(u, d.get("cliSessionId").and_then(Value::as_str));
                    }
                }
            }
            let missing = sessions.values().filter(|c| !c.is_some_and(|c| tx.contains(c))).count();
            if missing > 0 {
                warnings.push(Warning::MissingTranscripts { missing, total: sessions.len() });
            }
        }
        surfaces.push(SurfaceView { surface, partitions: views, union: union.len() });
    }
    let retention = crate::retention::read(&ctx.paths);
    if let (true, Some(days), Some(limited_by)) = (retention.is_short(), retention.desktop_days, retention.limited_by) {
        warnings.push(Warning::ShortRetention { days, limited_by, path: ctx.paths.claude_settings.clone() });
    }
    let logins = crate::logins::list(ctx);
    if let (Some((cli, _)), Some(desktop)) = (scan::cli_account(ctx), &logins.signed_in) {
        if cli != *desktop {
            warnings.push(Warning::CliOnOtherAccount { cli, desktop: desktop.clone() });
        }
    }
    let (plan, _, _) = build_plan(ctx, Some(app.clone()), Some(&state));
    Overview {
        desktop_version: desktop::desktop_version(),
        labels,
        surfaces,
        plan: summarize(&plan),
        warnings,
        retention,
        logins,
        roster,
        usage,
        service: service::status(ctx),
        heartbeat: Heartbeat::read(&ctx.paths),
        last_sync: state.last_sync.clone(),
        baseline: state.baseline.clone(),
        pending_restart: state.pending_restart.clone(),
        config: cfg,
        app,
    }
}

/// Days since the Unix epoch seconds `t`, for "2 min ago" style labels.
pub fn ago(t: f64) -> String {
    let secs = (fsx::now_secs() - t).max(0.0);
    match secs {
        s if s < 45.0 => "just now".into(),
        s if s < 90.0 => "a minute ago".into(),
        s if s < 3600.0 => format!("{} min ago", (s / 60.0).round() as u64),
        s if s < 5400.0 => "an hour ago".into(),
        s if s < 86400.0 => format!("{} hours ago", (s / 3600.0).round() as u64),
        s if s < 172800.0 => "yesterday".into(),
        s => format!("{} days ago", (s / 86400.0).round() as u64),
    }
}
