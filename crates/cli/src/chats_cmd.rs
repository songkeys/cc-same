// The commands follow the ideas of claude-switcher (MIT, Copyright (c) 2026 NeezerGu and Nyxon);
// its license is in `LICENSES/MIT-claude-switcher.txt`.
//! `cc-same chats …` and `cc-same stats`.

use super::{confirm, ExportFormat};
use anyhow::{bail, Context as _, Result};
use cc_same_core::chats::{self, Done};
use cc_same_core::report;
use cc_same_core::transcript::{self, Usage};
use cc_same_core::{short, Ctx, Partition};
use std::path::PathBuf;

fn when(ms: i64) -> String {
    if ms <= 0 {
        return "-".into();
    }
    report::ago(ms as f64 / 1000.0)
}

/// The one list `query` names; an account with several organizations needs `<account>/<org>`.
fn one_list(ctx: &Ctx, query: &str) -> Result<Partition> {
    let mut lists = chats::resolve(ctx, query)?;
    if lists.len() > 1 {
        let names: Vec<String> = lists.iter().map(Partition::key).collect();
        bail!("{query} has several organizations; name one: {}", names.join(" or "));
    }
    Ok(lists.remove(0))
}

pub fn list(ctx: &Ctx, account: Option<&str>, json: bool) -> Result<()> {
    let only: Option<Vec<String>> = match account {
        Some(q) => Some(chats::resolve(ctx, q)?.iter().map(Partition::key).collect()),
        None => None,
    };
    let all: Vec<_> =
        chats::list(ctx).into_iter().filter(|c| only.as_ref().is_none_or(|o| o.contains(&c.list))).collect();
    if json {
        let rows: Vec<_> = all
            .iter()
            .map(|c| {
                serde_json::json!({
                    "list": c.list, "id": c.id, "title": c.title, "folder": c.cwd,
                    "archived": c.archived, "lastActivityMs": c.last_activity_ms, "transcript": c.transcript,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "schemaVersion": 1, "chats": rows }))?);
        return Ok(());
    }
    if all.is_empty() {
        println!("No chats yet.");
        return Ok(());
    }
    for c in &all {
        let archived = if c.archived { " [archived]" } else { "" };
        let list = c.list.split_once('/').map_or(c.list.as_str(), |(a, _)| short(a));
        println!(
            "  {:<10} {:<9} {:<14} {}{archived}",
            short(c.id.trim_start_matches("local_")),
            list,
            when(c.last_activity_ms),
            c.title
        );
    }
    println!(
        "\n{} chat(s). Cards of the accounts that sync are the same; `chats lost` finds chats no account shows.",
        all.len()
    );
    Ok(())
}

pub fn lost(ctx: &Ctx, json: bool) -> Result<()> {
    let (found, left_out) = chats::lost(ctx);
    if json {
        let rows: Vec<_> = found
            .iter()
            .map(|l| {
                serde_json::json!({
                    "transcript": l.meta.session, "title": l.title, "folder": l.meta.cwd,
                    "model": l.meta.model, "endMs": l.meta.end_ms,
                })
            })
            .collect();
        let out = serde_json::json!({ "schemaVersion": 1, "lost": rows, "leftOut": left_out });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if found.is_empty() {
        println!("No lost chats: every conversation on disk is in a list.");
        return Ok(());
    }
    for l in &found {
        println!(
            "  {:<10} {:<14} {:<16} {}",
            short(&l.meta.session),
            when(l.meta.end_ms.unwrap_or(0)),
            l.project,
            l.title
        );
    }
    if left_out > 0 {
        println!("\n{left_out} older part(s) of these chats or copies of listed ones were left out.");
    }
    println!("\nGive them a card again: `cc-same chats rescue --to <account> [<id>…]` (all of them without ids).");
    Ok(())
}

fn report_done(done: &Done, verb: &str) {
    for (id, title) in &done.created {
        println!("  {verb} {title}  ({})", short(id.trim_start_matches("local_")));
    }
    if done.updated > 0 {
        println!("  {} older card(s) replaced by a newer one.", done.updated);
    }
    if done.already > 0 {
        println!("  {} chat(s) were there already.", done.already);
    }
    if done.deleted_there > 0 {
        println!("  {} chat(s) left out because they were deleted there.", done.deleted_there);
    }
    if let Some(id) = &done.snapshot {
        println!("\nTo undo: quit Claude, then `cc-same restore {id}`.");
    }
}

pub fn rescue(ctx: &Ctx, ids: &[String], to: &str, yes: bool) -> Result<()> {
    let target = one_list(ctx, to)?;
    let (found, _) = chats::lost(ctx);
    if found.is_empty() {
        println!("No lost chats.");
        return Ok(());
    }
    let count = if ids.is_empty() { found.len() } else { ids.len() };
    println!("Give {count} lost chat(s) a card in {}.", target.label());
    if !yes && !confirm("Continue?") {
        return Ok(());
    }
    let done = chats::rescue(ctx, &target, ids)?;
    report_done(&done, "rescued");
    println!("\nClaude shows them after it restarts. Accounts that sync get them too.");
    Ok(())
}

pub fn copy(ctx: &Ctx, ids: &[String], from: &str, to: &str, yes: bool) -> Result<()> {
    let (src, dst) = (one_list(ctx, from)?, one_list(ctx, to)?);
    let what = if ids.is_empty() { "every chat".to_string() } else { format!("{} chat(s)", ids.len()) };
    println!("Put {what} of {} into {}. Newer cards win; nothing is deleted.", src.label(), dst.label());
    if !yes && !confirm("Continue?") {
        return Ok(());
    }
    let done = chats::copy(ctx, &src, &dst, ids)?;
    if done.created.is_empty() && done.updated == 0 {
        println!("Nothing to copy: {} has them all.", dst.label());
    }
    report_done(&done, "copied");
    println!("\nIf both accounts take part in sync, they match already; this is for accounts kept out of it.");
    Ok(())
}

pub fn export(ctx: &Ctx, chat: &str, format: ExportFormat, out: Option<PathBuf>) -> Result<()> {
    let path = chats::find_transcript(ctx, chat)?;
    let (body, ext) = match format {
        ExportFormat::Md => (transcript::export_markdown(&path)?, "md"),
        ExportFormat::Html => (transcript::export_html(&path)?, "html"),
    };
    match out {
        Some(dest) => {
            std::fs::write(&dest, body).with_context(|| format!("writing {}", dest.display()))?;
            println!("Wrote {}", dest.display());
        }
        None => {
            // To a file next to where you are by default would surprise; print so it can be piped.
            let _ = ext;
            print!("{body}");
        }
    }
    Ok(())
}

fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000_000 => format!("{:.1}B", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

fn usage_row(label: &str, u: &Usage, width: usize) {
    println!(
        "  {label:<width$}  {:>8}  in {:>7}  out {:>7}  cache w {:>7}  cache r {:>7}  {:>6} replies",
        tokens(u.total()),
        tokens(u.input),
        tokens(u.output),
        tokens(u.cache_write),
        tokens(u.cache_read),
        u.messages
    );
}

pub fn stats(ctx: &Ctx, json: bool) -> Result<()> {
    let st = transcript::usage_stats(&ctx.paths.projects);
    if json {
        let table = |m: &std::collections::BTreeMap<String, Usage>| -> serde_json::Value {
            m.iter()
                .map(|(k, u)| {
                    let row = serde_json::json!({
                        "input": u.input, "output": u.output, "cacheWrite": u.cache_write,
                        "cacheRead": u.cache_read, "replies": u.messages,
                    });
                    (k.clone(), row)
                })
                .collect::<serde_json::Map<_, _>>()
                .into()
        };
        let out = serde_json::json!({
            "schemaVersion": 1, "sessions": st.sessions, "files": st.files,
            "firstMs": st.first_ms, "lastMs": st.last_ms,
            "byModel": table(&st.by_model), "byDay": table(&st.by_day), "byProject": table(&st.by_project),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if st.files == 0 {
        println!(
            "No transcripts found in {}.",
            ctx.paths.projects.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        );
        return Ok(());
    }
    println!("{} chat(s) in {} transcript file(s), counted once per reply.\n", st.sessions, st.files);
    println!("By model");
    let width = st.by_model.keys().map(|k| k.chars().count()).max().unwrap_or(0).max(5);
    let mut models: Vec<_> = st.by_model.iter().collect();
    models.sort_by_key(|(_, u)| std::cmp::Reverse(u.total()));
    for (m, u) in models {
        usage_row(m, u, width);
    }
    println!("\nLast 14 days");
    for (day, u) in st.by_day.iter().rev().take(14).collect::<Vec<_>>().into_iter().rev() {
        usage_row(day, u, 10);
    }
    println!("\nBusiest folders");
    let mut projects: Vec<_> = st.by_project.iter().filter(|(k, _)| !k.is_empty()).collect();
    projects.sort_by_key(|(_, u)| std::cmp::Reverse(u.total()));
    for (p, u) in projects.into_iter().take(8) {
        usage_row(&p.chars().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<String>(), u, 40);
    }
    usage_row("all", &st.total(), 3);
    Ok(())
}
