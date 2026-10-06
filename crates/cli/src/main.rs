//! `cc-same`: keep Claude Desktop's local sessions identical across all your accounts.

use anyhow::{bail, Result};
use cc_same_core::accounts::{self, NotFound, Roster, Slot, Strategy};
use cc_same_core::desktop::{self, Quitting};
use cc_same_core::logins;
use cc_same_core::report::{self, Overview, Warning};
use cc_same_core::retention::{self, Kept, Limit};
use cc_same_core::service::{self, Heartbeat};
use cc_same_core::usage::Usage;
use cc_same_core::watch::{self, WatchOptions};
use cc_same_core::{apply, fsx, plan, scan, short, snapshot, Config, Ctx, FakeDesktop, LogSink, Paths, State, Surface};
use clap::{Parser, Subcommand, ValueEnum};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "cc-same",
    version,
    about = "Keep Claude Desktop's local sessions identical across all your accounts."
)]
struct Cli {
    /// Claude Desktop's data folder (default: this platform's location)
    #[arg(long, global = true, env = "CC_SAME_USER_DATA", value_name = "DIR")]
    user_data: Option<PathBuf>,
    /// Where cc-same keeps its config, snapshots and trash
    #[arg(long, global = true, env = "CC_SAME_STATE_DIR", value_name = "DIR")]
    state_dir: Option<PathBuf>,
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Subcommand)]
enum Cmd {
    /// Read-only check of every account's session index
    Doctor,
    /// Show what a sync would change (dry run)
    Plan,
    /// Sync once
    Sync {
        #[arg(short, long)]
        yes: bool,
    },
    /// Keep syncing (the background agent runs this)
    Watch {
        /// Seconds between checks
        #[arg(long, default_value_t = 2.0)]
        interval: f64,
        #[arg(long, hide = true)]
        iterations: Option<u64>,
        /// Log to the terminal instead of the log file
        #[arg(long)]
        foreground: bool,
    },
    /// Sync now, then keep syncing in the background from login
    Install {
        #[arg(short, long)]
        yes: bool,
    },
    /// Stop background syncing (every account keeps its full copy)
    Uninstall,
    /// Background sync and last sync
    Status,
    /// List snapshots
    Snapshots,
    /// Put every session index back as it was in a snapshot (quit Claude first)
    Restore {
        snapshot: String,
        #[arg(short, long)]
        yes: bool,
    },
    /// Turn symlinked index folders left by other tools into real folders (quit Claude first)
    FixSymlinks {
        #[arg(short, long)]
        yes: bool,
    },
    /// Show how long Claude Code keeps transcripts; keep them, undo that, or set cleanupPeriodDays
    Retention {
        /// Set cleanupPeriodDays (terminal sessions and other data)
        days: Option<u32>,
        /// Keep the transcripts of Desktop sessions (only needed when Claude Code would delete them)
        #[arg(long, conflicts_with_all = ["days", "undo"])]
        keep: bool,
        /// Put back the setting CC Same changed
        #[arg(long, conflicts_with = "days")]
        undo: bool,
    },
    /// Your accounts, numbered: which one Claude is signed in to, and what each has used
    #[command(visible_alias = "list")]
    Accounts {
        #[arg(long)]
        json: bool,
    },
    /// Switch Claude to another account, or the next one (Claude restarts if it is open)
    Switch {
        /// Its number, alias or email (default: the next account in the list)
        account: Option<String>,
        /// Without an account: the next one, the next one with plan left, or the one with the most left
        #[arg(long, value_enum, conflicts_with = "account")]
        strategy: Option<StrategyArg>,
        #[arg(long)]
        json: bool,
    },
    /// Restart Claude signed out, to add an account; the current one is kept
    #[command(alias = "sign-in")]
    Add,
    /// Take an account off the list and forget the sign-in kept for it (its sessions stay)
    #[command(alias = "forget")]
    Remove {
        /// Its number, alias or email
        account: String,
        #[arg(short, long)]
        yes: bool,
    },
    /// Give an account a short name, or list them
    Alias {
        /// Its number, alias or email
        account: Option<String>,
        /// The new alias
        name: Option<String>,
        /// Take the alias away
        #[arg(long, requires = "account", conflicts_with = "name")]
        unset: bool,
    },
    /// Leave an account out of `switch` without an account (it can still be named)
    Disable { account: String },
    /// Put an account back in the rotation
    Enable { account: String },
    /// Give an account another number (the account that has it takes the old one)
    Move { account: String, number: u32 },
    /// Show or change settings
    Config {
        /// Keep an account (or `<account>/<org>`) separate
        #[arg(long, value_name = "ID")]
        exclude: Vec<String>,
        /// Undo an --exclude
        #[arg(long, value_name = "ID")]
        include: Vec<String>,
        /// Let accounts that appear later join automatically
        #[arg(long)]
        auto_join: Option<OnOff>,
        /// Desktop notifications from the background agent
        #[arg(long)]
        notify: Option<OnOff>,
        /// A shell command to run after Claude switches accounts ("" to remove it)
        #[arg(long, value_name = "COMMAND")]
        on_switch: Option<String>,
        /// A shell command to run when the background sync starts failing ("" to remove it)
        #[arg(long, value_name = "COMMAND")]
        on_sync_error: Option<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum StrategyArg {
    Next,
    NextAvailable,
    Best,
}

impl From<StrategyArg> for Strategy {
    fn from(s: StrategyArg) -> Strategy {
        match s {
            StrategyArg::Next => Strategy::Next,
            StrategyArg::NextAvailable => Strategy::NextAvailable,
            StrategyArg::Best => Strategy::Best,
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let result = run(cli);
    // A hook started here (`switch`, `add`) is stopped if it hangs, which needs us still running.
    cc_same_core::hooks::wait();
    if let Err(e) = result {
        eprintln!("cc-same: {e:#}");
        std::process::exit(2);
    }
}

fn run(cli: Cli) -> Result<()> {
    let detected = Paths::detect();
    let paths = Paths::new(cli.user_data.unwrap_or(detected.user_data), cli.state_dir.unwrap_or(detected.state_dir));
    let config = Config::load(&paths)?;
    let mut ctx = Ctx::new(paths, config, FakeDesktop::from_env(), LogSink::Stderr);
    ctx.verbose = cli.verbose;
    let Some(cmd) = cli.cmd else {
        return doctor(&ctx);
    };
    match cmd {
        Cmd::Doctor => doctor(&ctx),
        Cmd::Plan => show_plan(&ctx, cli.verbose),
        Cmd::Sync { yes } => sync(&ctx, yes),
        Cmd::Watch { interval, iterations, foreground } => {
            if !foreground {
                ctx.logger.set_sink(LogSink::File(ctx.paths.log_file.clone()));
            }
            let opts = WatchOptions { interval: Duration::from_secs_f64(interval.max(0.2)), iterations };
            watch::run(&ctx, &opts, &AtomicBool::new(false));
            Ok(())
        }
        Cmd::Install { yes } => install(&ctx, yes),
        Cmd::Uninstall => {
            service::uninstall(&ctx)?;
            println!("Background sync removed. Every account keeps its full copy of your sessions.");
            println!("Snapshots and trash stay in {}", ctx.paths.state_dir.display());
            Ok(())
        }
        Cmd::Status => status(&ctx),
        Cmd::Snapshots => snapshots(&ctx),
        Cmd::Restore { snapshot, yes } => {
            let manifest = snapshot::list(&ctx).into_iter().find(|m| m.id == snapshot);
            let Some(m) = manifest else { bail!("no such snapshot: {snapshot} (see `cc-same snapshots`)") };
            println!(
                "Restore {} session folder(s) from {}. The current ones go to the trash first.",
                m.partitions.len(),
                m.id
            );
            if !yes && !confirm("Continue?") {
                return Ok(());
            }
            let pre = snapshot::restore(&ctx, &m.id)?;
            println!("Restored. The state before this restore is snapshot {pre}.");
            Ok(())
        }
        Cmd::FixSymlinks { yes } => {
            let todo = snapshot::symlinked_folders(&ctx);
            if todo.is_empty() {
                println!("No symlinked session folders.");
                return Ok(());
            }
            for t in &todo {
                let target = std::fs::read_link(t).map(|p| p.display().to_string()).unwrap_or_default();
                println!("  {} -> {target}", t.strip_prefix(&ctx.paths.user_data).unwrap_or(t).display());
            }
            println!("Each link becomes a real folder holding a copy of what it points to.");
            if !yes && !confirm("Continue?") {
                return Ok(());
            }
            let fixed = snapshot::fix_symlinks(&ctx)?;
            println!("Fixed {} folder(s). Run `cc-same sync` to merge them.", fixed.len());
            Ok(())
        }
        Cmd::Retention { days, keep, undo } => retention(&ctx, days, keep, undo),
        Cmd::Accounts { json } => accounts(&ctx, json),
        Cmd::Switch { account, strategy, json } => switch(&ctx, account.as_deref(), strategy.map(Into::into), json),
        Cmd::Add => add(&ctx),
        Cmd::Remove { account, yes } => remove(&ctx, &account, yes),
        Cmd::Alias { account, name, unset } => alias(&ctx, account.as_deref(), name.as_deref(), unset),
        Cmd::Disable { account } => sit_out(&ctx, &account, true),
        Cmd::Enable { account } => sit_out(&ctx, &account, false),
        Cmd::Move { account, number } => move_account(&ctx, &account, number),
        Cmd::Config { exclude, include, auto_join, notify, on_switch, on_sync_error } => {
            let mut cfg = ctx.config();
            let before = cfg.clone();
            for id in exclude {
                if !cfg.exclude.contains(&id) {
                    cfg.exclude.push(id);
                }
            }
            cfg.exclude.retain(|e| !include.contains(e));
            if let Some(v) = auto_join {
                cfg.auto_join_new = matches!(v, OnOff::On);
            }
            if let Some(v) = notify {
                cfg.notify = matches!(v, OnOff::On);
            }
            let hook = |c: String| Some(c).filter(|c| !c.trim().is_empty());
            if let Some(c) = on_switch {
                cfg.on_switch = hook(c);
            }
            if let Some(c) = on_sync_error {
                cfg.on_sync_error = hook(c);
            }
            if cfg != before {
                cfg.save(&ctx.paths)?;
            }
            println!("{}", serde_json::to_string_pretty(&cfg)?);
            Ok(())
        }
    }
}

fn confirm(prompt: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        println!("No terminal to ask in. Review the plan above, then run again with --yes.");
        return false;
    }
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).is_ok() && matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

fn email_or_id(ov: &Overview, acct: &str) -> String {
    ov.labels.get(acct).cloned().unwrap_or_else(|| format!("account {}", short(acct)))
}

fn describe_plan(ov_plan: &report::PlanView, labels: &std::collections::BTreeMap<String, String>) -> Vec<String> {
    let mut lines = Vec::new();
    if ov_plan.total == 0 {
        lines.push("Nothing to do: every linked account already has the same sessions.".to_string());
    }
    for (p, kinds) in &ov_plan.per_partition {
        let who = labels.get(&p.acct).cloned().unwrap_or_else(|| p.label());
        let what: Vec<String> = kinds.iter().map(|(k, n)| format!("{n} {k}")).collect();
        lines.push(format!("  {:<6} {:<32} {}", p.surface.as_str(), who, what.join(", ")));
    }
    if !ov_plan.deferred.is_empty() {
        lines.push("Waiting for Claude (it has these open; applied after you switch accounts or quit):".into());
        for (key, n) in &ov_plan.deferred {
            let acct = key.split('/').next().unwrap_or(key);
            let who = labels.get(acct).cloned().unwrap_or_else(|| short(acct).to_string());
            lines.push(format!("  {who}: {n} change(s)"));
        }
    }
    for n in &ov_plan.notes {
        lines.push(format!("note: {n}"));
    }
    lines
}

fn doctor(ctx: &Ctx) -> Result<()> {
    let ov = report::overview(ctx);
    println!("cc-same {}", cc_same_core::VERSION);
    let desktop = match (&ov.desktop_version, ov.app.running) {
        (Some(v), true) => format!("Claude {v}, running"),
        (Some(v), false) => format!("Claude {v}, not running"),
        (None, true) => "Claude is running".to_string(),
        (None, false) => "Claude is not running".to_string(),
    };
    let open = ov.app.open_account.as_deref().map(|a| format!("; open account: {}", email_or_id(&ov, a)));
    println!("{desktop}{}", open.unwrap_or_default());
    println!("Background sync: {}", service_line(&ov));
    for sv in &ov.surfaces {
        println!();
        println!("{} sessions ({})", sv.surface.title(), sv.surface.dir_name());
        for pv in &sv.partitions {
            let mut flags = Vec::new();
            if ov.app.showing(&pv.part) {
                flags.push("OPEN IN CLAUDE");
            }
            if pv.excluded {
                flags.push("excluded");
            }
            if pv.part.is_link {
                flags.push("SYMLINK");
            }
            let who = pv.email.clone().unwrap_or_default();
            if let Some(err) = &pv.error {
                println!("  {}  {:<30} {}  {err}", pv.part.label(), who, flags.join(" "));
                continue;
            }
            let mut facts = vec![format!("{} sessions", pv.sessions)];
            if pv.archived > 0 {
                facts.push(format!("{} archived", pv.archived));
            }
            if pv.missing > 0 && !pv.excluded {
                facts.push(format!("missing {}", pv.missing));
            }
            if pv.unreadable > 0 {
                facts.push(format!("{} unreadable", pv.unreadable));
            }
            println!("  {}  {:<30} {}  {}", pv.part.label(), who, facts.join(" · "), flags.join(" "));
            if ctx.verbose && !pv.unmanaged.is_empty() {
                println!("      left untouched: {}", pv.unmanaged.join(", "));
            }
        }
    }
    let retention = match ov.retention.desktop_days {
        Some(d) => format!("deleted after {d} days"),
        None => "kept at any age".into(),
    };
    println!();
    println!("Transcripts of Desktop sessions: {retention}");
    if let Some(ls) = &ov.last_sync {
        println!("Last sync: {} ({}), {} change(s), {} error(s)", report::ago(ls.at), ls.reason, ls.applied, ls.errors);
    }
    if let Some(hint) = ov.restart_hint() {
        println!("\nQuit and reopen Claude to see {} session(s) copied into the account it has open.", hint.sessions);
    }
    if !ov.warnings.is_empty() {
        println!();
        for w in &ov.warnings {
            // Several organizations is the normal case, so it reads as a tip, not a problem.
            let mark = if matches!(w, Warning::SpansOrgs { .. }) { "·" } else { "!" };
            println!("{mark} {}", warning_text(w));
        }
    }
    println!("\nNot synced by design: sidebar groups and order (a server setting of each account), pins, Cowork tasks, claude.ai chats, projects and memory, cloud sessions, connectors and Remote Control links.");
    println!("\nIf you sync now:");
    for l in describe_plan(&ov.plan, &ov.labels) {
        println!("{l}");
    }
    Ok(())
}

fn service_line(ov: &Overview) -> String {
    let beat = ov.heartbeat.as_ref().filter(|h| h.is_fresh());
    match (ov.service.installed, beat) {
        (true, Some(h)) => format!("on (agent v{}, last check {})", h.version, report::ago(h.heartbeat_at)),
        (true, None) => format!("on ({})", ov.service.detail),
        (false, _) if ov.service.legacy => {
            "the original Python agent is still installed; `cc-same install` replaces it".into()
        }
        (false, _) => "off (turn on with `cc-same install`)".into(),
    }
}

fn warning_text(w: &Warning) -> String {
    match w {
        Warning::SymlinkedFolders { surface, count } => format!(
            "{count} {surface} session folder(s) are symlinks. Claude silently stops saving sessions there. Quit Claude, then run: cc-same fix-symlinks"
        ),
        Warning::LeftoverFolders { dirs, .. } => format!("Leftover backup folders from other tools: {} (safe to archive by hand)", dirs.join(", ")),
        Warning::SpansOrgs { orgs, .. } => format!(
            "Your accounts span {orgs} organizations, and synced sessions appear in all of them. To keep one separate: cc-same config --exclude <account-id>"
        ),
        Warning::ShortRetention { days, limited_by, path } => {
            let why = match limited_by {
                Limit::DesktopSetting => format!("desktopSessionCleanupPeriodDays in {}", path.display()),
                Limit::Organization => "your organization's managed settings".into(),
                Limit::OlderClaudeCode => "a Claude Code before 2.1.248; updating Claude keeps them at any age".into(),
            };
            let fix = match limited_by {
                Limit::Organization => "",
                _ => " Keep them: cc-same retention --keep",
            };
            format!("Claude Code deletes the transcripts of Desktop sessions after {days} days ({why}), and a session opens empty without one.{fix}")
        }
        Warning::MissingTranscripts { missing, total } => {
            format!("{missing} of {total} sessions no longer have a transcript on disk; they open empty or not at all")
        }
        Warning::DesktopDataMissing { path } => format!("Claude Desktop's data folder was not found at {}", path.display()),
    }
}

fn show_plan(ctx: &Ctx, verbose: bool) -> Result<()> {
    let (plan, app, _) = plan::build_plan(ctx, None, None);
    if app.running {
        println!("Claude is running; changes to the account it has open wait until you switch accounts or quit.");
    }
    let labels = cc_same_core::scan::account_labels(ctx);
    for l in describe_plan(&report::summarize(&plan), &labels) {
        println!("{l}");
    }
    if verbose {
        for a in &plan.actions {
            let from = a.src.as_ref().map(|s| format!("  <- {}", s.label())).unwrap_or_default();
            println!("  {:<16} {}/{}{from}", format!("{:?}", a.kind), a.part.label(), a.name);
        }
    }
    Ok(())
}

fn sync(ctx: &Ctx, yes: bool) -> Result<()> {
    let (plan, _, _) = plan::build_plan(ctx, None, None);
    let labels = cc_same_core::scan::account_labels(ctx);
    for l in describe_plan(&report::summarize(&plan), &labels) {
        println!("{l}");
    }
    if !plan.is_empty() && !yes && !confirm("Apply these changes?") {
        return Ok(());
    }
    // Run even without changes: it records which accounts are linked and what each task list
    // looked like, which later merges depend on.
    let (_, out) = apply::run_sync(ctx, "manual")?;
    if !plan.is_empty() {
        println!(
            "Done: {} change(s){}.",
            out.applied_total(),
            if out.errors.is_empty() { String::new() } else { format!(", {} error(s)", out.errors.len()) }
        );
    }
    if !out.seeded_while_loaded.is_empty() {
        println!("Quit and reopen Claude to see the sessions copied into the account it has open.");
    }
    if !out.errors.is_empty() {
        bail!("{} change(s) failed; see above", out.errors.len());
    }
    Ok(())
}

fn install(ctx: &Ctx, yes: bool) -> Result<()> {
    let (plan, _, _) = plan::build_plan(ctx, None, None);
    let labels = cc_same_core::scan::account_labels(ctx);
    println!("First sync:");
    for l in describe_plan(&report::summarize(&plan), &labels) {
        println!("{l}");
    }
    if !yes && !confirm("Sync now and keep syncing in the background?") {
        return Ok(());
    }
    if !ctx.paths.config_file().exists() {
        ctx.config().save(&ctx.paths)?;
    }
    let (_, out) = apply::run_sync(ctx, "install")?;
    let exe = std::env::current_exe()?;
    // On Windows the windowless agent build runs in the background.
    let agent = if cfg!(windows) { exe.with_file_name("cc-same-agent.exe") } else { exe.clone() };
    let (source, name) = if agent.exists() {
        (agent, if cfg!(windows) { "cc-same-agent.exe" } else { "cc-same" })
    } else {
        (exe, if cfg!(windows) { "cc-same.exe" } else { "cc-same" })
    };
    let installed = service::install(ctx, &source, name)?;
    println!("Background sync is on ({}).", installed.display());
    println!("Log: {}", ctx.paths.log_file.display());
    if !out.seeded_while_loaded.is_empty() {
        println!("Quit and reopen Claude once to see the sessions copied into the account it has open.");
    }
    Ok(())
}

/// Every account CC Same knows of, by ID, with its name: the list, then the ones only seen in
/// Claude's session folders.
fn known_accounts(ctx: &Ctx, roster: &Roster) -> BTreeMap<String, String> {
    let labels = scan::account_labels(ctx);
    let mut known: BTreeMap<String, String> = roster.slots.iter().map(|s| (s.account.clone(), s.label())).collect();
    for part in scan::discover(ctx, Surface::Code) {
        let name = labels.get(&part.acct).cloned().unwrap_or_else(|| format!("Account {}", short(&part.acct)));
        known.entry(part.acct).or_insert(name);
    }
    known
}

/// The account `query` names in the list; an account only seen in Claude's session folders is
/// named in the error, with how to add it.
fn named(ctx: &Ctx, roster: &Roster, query: &str) -> Result<(String, String)> {
    match roster.find(query) {
        Ok(slot) => Ok((slot.account.clone(), slot.label())),
        Err(NotFound::Several(names)) => bail!("{query} could be {}", names.join(" or ")),
        Err(NotFound::None(_)) => {
            let query = query.trim();
            let others: Vec<(String, String)> = known_accounts(ctx, roster)
                .into_iter()
                .filter(|(id, name)| name.eq_ignore_ascii_case(query) || (query.len() >= 4 && id.starts_with(query)))
                .collect();
            match others.as_slice() {
                [(_, name)] => bail!("{name} is not in the list yet: sign in to it once in Claude (`cc-same add`)"),
                [] => bail!("no account called {query} (see `cc-same accounts`)"),
                _ => bail!(
                    "{query} could be {}",
                    others.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(" or ")
                ),
            }
        }
    }
}

fn name_in(roster: &Roster, account: &str) -> String {
    roster.slot(account).map(Slot::label).unwrap_or_else(|| format!("Account {}", short(account)))
}

/// "5h 12% · 7d 58%, 2 min ago", or nothing when Claude has no reading.
fn usage_text(usage: Option<&Usage>, now: f64) -> String {
    let Some(u) = usage else { return String::new() };
    let mut parts = Vec::new();
    if let Some(p) = u.five_hour_at(now) {
        parts.push(format!("5h {p:.0}%"));
    }
    if let Some(p) = u.weekly_at(now) {
        parts.push(format!("7d {p:.0}%"));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("{}, {}", parts.join(" · "), report::ago(u.at))
}

fn accounts(ctx: &Ctx, json: bool) -> Result<()> {
    let roster = accounts::observe(ctx);
    let found = logins::list(ctx);
    let usage = accounts::usage(ctx, &roster);
    let now = fsx::now_secs();
    let signed_in = found.signed_in.as_deref();
    if json {
        let rows: Vec<serde_json::Value> = roster
            .slots
            .iter()
            .map(|s| {
                let kept = found.saved(&s.account);
                let u = usage.get(&s.account);
                serde_json::json!({
                    "number": s.number,
                    "account": s.account,
                    "email": s.email,
                    "alias": s.alias,
                    "disabled": s.disabled,
                    "signedIn": signed_in == Some(s.account.as_str()),
                    "switchable": kept.is_some(),
                    "keptAt": kept.map(|k| k.set_aside_at),
                    "stale": kept.is_some_and(|k| k.stale()),
                    "usage": u.map(|u| serde_json::json!({
                        "at": u.at,
                        "fiveHour": u.five_hour_at(now),
                        "weekly": u.weekly_at(now),
                    })),
                })
            })
            .collect();
        let out = serde_json::json!({ "schemaVersion": 1, "signedIn": signed_in, "accounts": rows });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if roster.slots.is_empty() {
        println!("No accounts yet: sign in to Claude.");
        return Ok(());
    }
    if !found.supported {
        println!("Switching accounts is not supported on this system yet.\n");
    }
    let name_width = roster.slots.iter().map(|s| s.label().chars().count()).max().unwrap_or(0);
    let alias_width =
        roster.slots.iter().filter_map(|s| s.alias.as_ref()).map(|a| a.chars().count()).max().unwrap_or(0);
    for s in &roster.slots {
        let state = if signed_in == Some(s.account.as_str()) {
            "signed in".to_string()
        } else if let Some(kept) = found.saved(&s.account) {
            let stale = if kept.stale() { " (may ask to sign in again)" } else { "" };
            format!("kept, used {}{stale}", report::ago(kept.set_aside_at))
        } else {
            "sign in once to switch here (`cc-same add`)".to_string()
        };
        let state = if s.disabled { format!("{state}, sits out") } else { state };
        let used = usage_text(usage.get(&s.account), now);
        let alias = s.alias.clone().unwrap_or_default();
        let line = format!("  {:>2}  {:name_width$}  {alias:alias_width$}  {state:<30}  {used}", s.number, s.label());
        println!("{}", line.trim_end());
    }
    let others: Vec<String> = known_accounts(ctx, &roster)
        .into_iter()
        .filter(|(id, _)| roster.slot(id).is_none())
        .map(|(_, name)| name)
        .collect();
    if !others.is_empty() {
        println!("\nAlso in your sessions: {} (sign in to them once in Claude to add them).", others.join(", "));
    }
    Ok(())
}

fn switch(ctx: &Ctx, account: Option<&str>, strategy: Option<Strategy>, json: bool) -> Result<()> {
    let roster = accounts::observe(ctx);
    let found = logins::list(ctx);
    let target = match account {
        Some(query) => named(ctx, &roster, query)?.0,
        None => {
            let usage = accounts::usage(ctx, &roster);
            let strategy = strategy.unwrap_or_default();
            let can_switch = |s: &Slot| found.saved(&s.account).is_some();
            let current = found.signed_in.as_deref();
            match accounts::pick(&roster, current, strategy, can_switch, &usage, fsx::now_secs()) {
                Some(slot) => slot.account.clone(),
                None => return Err(accounts::Refused::NoOther.into()),
            }
        }
    };
    let name = name_in(&roster, &target);
    if found.signed_in.as_deref() == Some(target.as_str()) {
        if json {
            let out = serde_json::json!({ "schemaVersion": 1, "switched": false, "to": target, "reason": "already-signed-in" });
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            println!("Claude is already signed in to {name}.");
        }
        return Ok(());
    }
    let done = accounts::switch(ctx, &target, &quit_watch())?;
    if json {
        let out = serde_json::json!({
            "schemaVersion": 1, "switched": true, "from": done.from, "to": done.to, "launched": done.launched,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    let restarted = if done.launched { " (restarted)" } else { "" };
    println!("Claude is signed in to {name}{restarted}.");
    Ok(())
}

/// Claude asks before it quits when it has work in progress: say so, and what Ctrl-C does then.
fn quit_watch() -> desktop::Watch<'static> {
    fn tell(quitting: Quitting) {
        match quitting {
            Quitting::Asked => {}
            Quitting::Confirming => eprintln!(
                "Claude is asking whether to stop its work in progress: answer it in Claude. (Ctrl-C stops waiting; nothing changes.)"
            ),
            Quitting::AfterWork => eprintln!(
                "Claude quits once its work in progress is done, and CC Same carries on then. (Ctrl-C stops waiting; Claude still quits then.)"
            ),
        }
    }
    desktop::Watch { progress: Some(&tell), stop: None }
}

fn add(ctx: &Ctx) -> Result<()> {
    let switched = accounts::add(ctx, &quit_watch())?;
    let roster = Roster::load(&ctx.paths);
    if let Some(from) = &switched.from {
        let name = name_in(&roster, from);
        let number = roster.slot(from).map(|s| s.number.to_string()).unwrap_or_else(|| name.clone());
        println!("Kept {name}: `cc-same switch {number}` brings it back.");
    }
    println!("Claude is open on its sign-in page: sign in to the other account there.");
    println!("To switch later, use `cc-same switch`: Log out in Claude ends a sign-in for good.");
    Ok(())
}

fn remove(ctx: &Ctx, query: &str, yes: bool) -> Result<()> {
    let roster = accounts::observe(ctx);
    let (id, name) = named(ctx, &roster, query)?;
    println!("Remove {name} from the list? The sign-in kept for it is forgotten; its sessions stay.");
    if !yes && !confirm("Remove it?") {
        return Ok(());
    }
    accounts::remove(ctx, &id)?;
    println!("Removed {name}. Sign in to it in Claude to add it again.");
    Ok(())
}

fn alias(ctx: &Ctx, account: Option<&str>, name: Option<&str>, unset: bool) -> Result<()> {
    let roster = accounts::observe(ctx);
    let Some(query) = account else {
        let aliased: Vec<&Slot> = roster.slots.iter().filter(|s| s.alias.is_some()).collect();
        if aliased.is_empty() {
            println!("No aliases yet: `cc-same alias <account> <name>`.");
        }
        for s in aliased {
            println!("  {:<12}  {}  {}", s.alias.as_deref().unwrap_or_default(), s.number, s.label());
        }
        return Ok(());
    };
    let (id, label) = named(ctx, &roster, query)?;
    if unset {
        accounts::set_alias(ctx, &id, None)?;
        println!("{label} has no alias now.");
    } else if let Some(name) = name {
        let roster = accounts::set_alias(ctx, &id, Some(name))?;
        let alias = roster.slot(&id).and_then(|s| s.alias.clone()).unwrap_or_default();
        println!("{label} is also {alias}: `cc-same switch {alias}`.");
    } else {
        match roster.slot(&id).and_then(|s| s.alias.as_deref()) {
            Some(alias) => println!("{alias}"),
            None => println!("{label} has no alias."),
        }
    }
    Ok(())
}

fn sit_out(ctx: &Ctx, query: &str, disabled: bool) -> Result<()> {
    let roster = accounts::observe(ctx);
    let (id, name) = named(ctx, &roster, query)?;
    accounts::set_disabled(ctx, &id, disabled)?;
    if disabled {
        println!("{name} sits out of the rotation; `cc-same switch {query}` still switches to it.");
    } else {
        println!("{name} is back in the rotation.");
    }
    Ok(())
}

fn move_account(ctx: &Ctx, query: &str, number: u32) -> Result<()> {
    let roster = accounts::observe(ctx);
    let (id, name) = named(ctx, &roster, query)?;
    let old = roster.slot(&id).map(|s| s.number);
    let swapped = accounts::move_to(ctx, &id, number)?;
    println!("{name} is number {number} now.");
    if let (Some(other), Some(old)) = (swapped, old) {
        println!("{} took number {old}.", name_in(&roster, &other));
    }
    Ok(())
}

fn status(ctx: &Ctx) -> Result<()> {
    let st = service::status(ctx);
    println!("Background sync: {}{}", if st.installed { "on, " } else { "off, " }, st.detail);
    if let Some(h) = Heartbeat::read(&ctx.paths) {
        println!("Agent: v{} (pid {}), last check {}", h.version, h.pid, report::ago(h.heartbeat_at));
    }
    let state = State::load(&ctx.paths);
    match &state.last_sync {
        Some(ls) => println!(
            "Last sync: {} ({}): {} change(s), {} error(s), {} waiting for Claude",
            report::ago(ls.at),
            ls.reason,
            ls.applied,
            ls.errors,
            ls.deferred
        ),
        None => println!("Never synced."),
    }
    println!("Baseline snapshot: {}", state.baseline.as_deref().unwrap_or("none"));
    for (surface, keys) in &state.known {
        let list: Vec<String> = keys.iter().map(|k| k.split('/').map(short).collect::<Vec<_>>().join("/")).collect();
        println!("Linked {surface}: {}", list.join(", "));
    }
    println!("Log: {}", ctx.paths.log_file.display());
    Ok(())
}

fn snapshots(ctx: &Ctx) -> Result<()> {
    let state = State::load(&ctx.paths);
    let list = snapshot::list(ctx);
    if list.is_empty() {
        println!("No snapshots yet. The first sync takes one.");
    }
    for m in &list {
        let baseline = if Some(&m.id) == state.baseline.as_ref() { "  (before the first sync)" } else { "" };
        println!("{}  {} folder(s){baseline}", m.id, m.partitions.len());
    }
    println!("Location: {}", ctx.paths.snapshots_dir().display());
    Ok(())
}

fn retention(ctx: &Ctx, days: Option<u32>, keep: bool, undo: bool) -> Result<()> {
    if let Some(days) = days {
        let backup = retention::set_cleanup_days(&ctx.paths, days)?;
        println!("cleanupPeriodDays = {days} in {}", ctx.paths.claude_settings.display());
        if let Some(b) = backup {
            println!("Previous file saved as {}", b.display());
        }
        return Ok(());
    }
    if keep {
        match retention::keep(&ctx.paths)? {
            Kept::Already => println!("Claude Code already keeps the transcripts of Desktop sessions at any age."),
            Kept::AnyAge => {
                println!("Removed desktopSessionCleanupPeriodDays: Desktop sessions keep their transcripts at any age.")
            }
            Kept::TenYears => {
                println!("cleanupPeriodDays = 3650. Updating Claude keeps Desktop transcripts at any age.")
            }
        }
        println!("Undo with: cc-same retention --undo");
        return Ok(());
    }
    if undo {
        match retention::undo(&ctx.paths)? {
            true => println!("Put back the setting CC Same changed in {}.", ctx.paths.claude_settings.display()),
            false => println!("Nothing to undo."),
        }
        return Ok(());
    }
    let r = retention::read(&ctx.paths);
    let version = r.claude_code.as_deref().map(|v| format!("Claude Code {v}")).unwrap_or_else(|| "Claude Code".into());
    match (r.desktop_days, r.limited_by) {
        (Some(d), Some(Limit::DesktopSetting)) => {
            println!("Desktop sessions: transcripts deleted after {d} days (desktopSessionCleanupPeriodDays).")
        }
        (Some(d), Some(Limit::Organization)) => {
            println!("Desktop sessions: transcripts deleted after {d} days (set by your organization).")
        }
        (Some(d), _) => println!("Desktop sessions: transcripts deleted after {d} days ({version} predates 2.1.248)."),
        (None, _) => println!("Desktop sessions: transcripts kept at any age ({version})."),
    }
    println!("Terminal sessions and other data: deleted after {} days (cleanupPeriodDays).", r.cleanup_days);
    if r.is_short() && r.limited_by != Some(Limit::Organization) {
        println!("Keep them: cc-same retention --keep");
    }
    if r.undo.is_some() {
        println!("Put back what CC Same changed: cc-same retention --undo");
    }
    Ok(())
}
