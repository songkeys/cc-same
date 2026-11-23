// The transcript format notes, prompt rules and token counting follow claude-switcher
// (MIT, Copyright (c) 2026 NeezerGu and Nyxon); its license is in `LICENSES/MIT-claude-switcher.txt`.
//! Claude Code transcripts: `<projects>/<project>/<session>.jsonl`, one JSON object per line.
//!
//! Reading only. A transcript is the chat's history; Desktop's "card" for it (the `local_*.json`
//! file in an account's index) is what makes the chat appear in the sidebar. This module tells
//! what a transcript is about (for chats that lost their card), what it cost in tokens, and
//! turns it into a page or a Markdown document.

use crate::ctx::Ctx;
use chrono::DateTime;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// How much of the start a quick read looks at (the folder and the first prompt are there), and
/// of the end (the titles and the last model are there).
const HEAD_BYTES: u64 = 256 * 1024;
const TAIL_BYTES: u64 = 64 * 1024;
const TITLE_MAX: usize = 80;
const PROMPT_MAX: usize = 300;
/// The model name of API-error placeholders, which are not real replies.
const SYNTHETIC: &str = "<synthetic>";
/// What every assistant line holds, for skipping the others without parsing them.
const ASSISTANT_TAG: &[u8] = br#""type":"assistant""#;

/// What a transcript is about.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    /// The file name without `.jsonl`: the transcript's id.
    pub session: String,
    pub path: PathBuf,
    pub size: u64,
    /// The folder the chat worked in.
    pub cwd: String,
    /// The last custom title, else the agent's name, else an AI title, else a prompt.
    pub title: String,
    pub first_prompt: String,
    pub last_prompt: String,
    /// The model of the last reply (`claude-…`), when there was one.
    pub model: String,
    pub branch: String,
    /// The earliest and latest message time, in Unix milliseconds.
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    /// Counted by [`read_meta`] only; a [`quick_meta`] leaves them at 0.
    pub prompts: usize,
    pub replies: usize,
}

impl Meta {
    /// Whether it holds a conversation and not only bookkeeping lines.
    pub fn is_conversation(&self) -> bool {
        !self.cwd.is_empty()
            && (self.prompts > 0 || self.replies > 0 || !self.first_prompt.is_empty() || !self.model.is_empty())
    }
}

/// Every transcript on disk, by id, across all the roots Claude Code may keep them in.
pub fn transcript_paths(ctx: &Ctx) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    for root in &ctx.paths.projects {
        for proj in fs::read_dir(root).into_iter().flatten().flatten() {
            if !proj.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            for f in fs::read_dir(proj.path()).into_iter().flatten().flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                if let Some(id) = name.strip_suffix(".jsonl") {
                    out.entry(id.to_string()).or_insert_with(|| f.path());
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------------------------ lines

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Collapse whitespace to single spaces and cut to `n` characters.
pub fn one_line(s: &str, n: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() > n {
        let cut: String = joined.chars().take(n - 1).collect();
        format!("{cut}…")
    } else {
        joined
    }
}

/// The text of a user message's content (a string or blocks), with images and documents as
/// placeholders and tool results left out; `None` when nothing is left.
fn human_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|b| match text(b, "type") {
                    "text" => Some(text(b, "text").to_string()),
                    kind @ ("image" | "document") => Some(format!("[{kind}]")),
                    _ => None,
                })
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        _ => None,
    }
}

fn tag_text(s: &str, tag: &str) -> String {
    let Some((_, rest)) = s.split_once(&format!("<{tag}>")) else { return String::new() };
    rest.split_once(&format!("</{tag}>")).map_or(rest, |(inner, _)| inner).trim().to_string()
}

/// A prompt a person wrote: for a slash command `("/name", arguments)`, otherwise `("", text)`.
fn split_prompt(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    // Machine-written user text is not a prompt.
    const NOT_PROMPTS: [&str; 4] =
        ["<local-command-caveat>", "<local-command-stdout>", "<task-notification>", "[Request interrupted"];
    if NOT_PROMPTS.iter().any(|p| text.starts_with(p)) {
        return None;
    }
    if text.starts_with("<command-name>") || text.starts_with("<command-message>") {
        let mut name = tag_text(text, "command-name").trim_start_matches('/').to_string();
        if name.is_empty() {
            name = tag_text(text, "command-message");
        }
        let args = tag_text(text, "command-args");
        return (!name.is_empty() || !args.is_empty()).then(|| (format!("/{name}"), args));
    }
    (!text.is_empty()).then(|| (String::new(), text.to_string()))
}

/// The prompt on a user line or a queued-prompt attachment, as [`split_prompt`] reads it.
fn human_prompt(line: &Value) -> Option<(String, String)> {
    let content = match text(line, "type") {
        "user"
            if !flag(line, "isMeta") && !flag(line, "isCompactSummary") && !flag(line, "isVisibleInTranscriptOnly") =>
        {
            line.get("message")?.get("content")?
        }
        "attachment" => {
            let a = line.get("attachment")?;
            if text(a, "type") == "queued_command" && text(a, "commandMode") == "prompt" {
                a.get("prompt")?
            } else {
                return None;
            }
        }
        _ => return None,
    };
    split_prompt(&human_text(content)?)
}

fn millis(ts: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(ts).ok().map(|t| t.timestamp_millis())
}

/// Calls `f` with every JSON line of `bytes`; a line that is not JSON (a torn first or last
/// line of a partial read, a damaged one) is skipped.
fn each_line(bytes: &[u8], mut f: impl FnMut(&Value)) {
    // A byte order mark (an editor or a script added one) would make the first line unreadable.
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    for raw in bytes.split(|b| *b == b'\n') {
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if let Ok(v) = serde_json::from_slice::<Value>(raw) {
            f(&v);
        }
    }
}

// -------------------------------------------------------------------------------- meta

struct Builder {
    m: Meta,
    custom_title: String,
    agent_name: String,
    ai_title: String,
    last_prompt_line: String,
    last_human: String,
    replies: HashSet<String>,
}

impl Builder {
    fn new(path: &Path, size: u64) -> Builder {
        let session = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Builder {
            m: Meta { session, path: path.to_path_buf(), size, ..Meta::default() },
            custom_title: String::new(),
            agent_name: String::new(),
            ai_title: String::new(),
            last_prompt_line: String::new(),
            last_human: String::new(),
            replies: HashSet::new(),
        }
    }

    fn add(&mut self, l: &Value) {
        let set = |dst: &mut String, v: &str| {
            if !v.is_empty() {
                *dst = v.to_string();
            }
        };
        let ty = text(l, "type");
        match ty {
            "custom-title" => set(&mut self.custom_title, text(l, "customTitle")),
            "agent-name" => set(&mut self.agent_name, text(l, "agentName")),
            "ai-title" => set(&mut self.ai_title, text(l, "aiTitle")),
            "last-prompt" => set(&mut self.last_prompt_line, text(l, "lastPrompt")),
            "assistant" => {
                let msg = l.get("message").unwrap_or(&Value::Null);
                if text(msg, "model") != SYNTHETIC {
                    set(&mut self.m.model, text(msg, "model"));
                    let id = match text(msg, "id") {
                        "" => text(l, "uuid"),
                        id => id,
                    };
                    self.replies.insert(id.to_string());
                }
            }
            _ => {}
        }
        if let Some((cmd, args)) = human_prompt(l) {
            self.m.prompts += 1;
            self.last_human = if args.is_empty() { cmd } else { args };
            if self.m.first_prompt.is_empty() {
                self.m.first_prompt = one_line(&self.last_human, PROMPT_MAX);
            }
        }
        if matches!(ty, "user" | "assistant" | "system" | "attachment") {
            if self.m.cwd.is_empty() {
                self.m.cwd = text(l, "cwd").to_string();
            }
            set(&mut self.m.branch, text(l, "gitBranch"));
            if let Some(t) = millis(text(l, "timestamp")) {
                self.m.start_ms = Some(self.m.start_ms.map_or(t, |s| s.min(t)));
                self.m.end_ms = Some(self.m.end_ms.map_or(t, |e| e.max(t)));
            }
        }
    }

    fn finish(mut self) -> Meta {
        self.m.replies = self.replies.len();
        let last = if self.last_prompt_line.is_empty() { &self.last_human } else { &self.last_prompt_line };
        self.m.last_prompt = one_line(last, PROMPT_MAX);
        let title = [&self.custom_title, &self.agent_name, &self.ai_title, &self.m.first_prompt, &self.m.last_prompt]
            .into_iter()
            .find(|s| !s.is_empty())
            .cloned()
            .unwrap_or_default();
        self.m.title = one_line(&title, TITLE_MAX);
        self.m
    }
}

fn read_range(f: &mut fs::File, from: u64, len: u64) -> std::io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    f.take(len).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Read the start and the end of a transcript only (fast, for lists and for finding lost
/// chats). Prompt and reply counts stay 0. A transcript with neither a folder nor a message in
/// those parts is read whole.
pub fn quick_meta(path: &Path) -> std::io::Result<Meta> {
    let mut f = fs::File::open(path)?;
    let size = f.metadata()?.len();
    let mut b = Builder::new(path, size);
    if size > HEAD_BYTES + TAIL_BYTES {
        let head = read_range(&mut f, 0, HEAD_BYTES)?;
        each_line(&head, |l| b.add(l));
        // Start one byte early so a line beginning exactly at the cut is not lost with the torn one.
        let tail = read_range(&mut f, size - TAIL_BYTES - 1, TAIL_BYTES + 1)?;
        let from = tail.iter().position(|c| *c == b'\n').map_or(tail.len(), |n| n + 1);
        each_line(&tail[from..], |l| b.add(l));
    }
    if b.m.cwd.is_empty() {
        b = Builder::new(path, size);
        let all = read_range(&mut f, 0, size)?;
        each_line(&all, |l| b.add(l));
    }
    let mut m = b.finish();
    m.prompts = 0;
    m.replies = 0;
    Ok(m)
}

/// Read a whole transcript: exact counts and time span.
pub fn read_meta(path: &Path) -> std::io::Result<Meta> {
    let all = fs::read(path)?;
    let mut b = Builder::new(path, all.len() as u64);
    each_line(&all, |l| b.add(l));
    Ok(b.finish())
}

// -------------------------------------------------------------------------------- usage

/// Tokens counted the way the API bills them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub messages: u64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_write += o.cache_write;
        self.cache_read += o.cache_read;
        self.messages += o.messages;
    }

    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }
}

/// What the transcripts under some roots add up to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub by_model: BTreeMap<String, Usage>,
    /// `YYYY-MM-DD`, in the local time zone.
    pub by_day: BTreeMap<String, Usage>,
    pub by_project: BTreeMap<String, Usage>,
    pub sessions: usize,
    pub files: usize,
    pub first_ms: Option<i64>,
    pub last_ms: Option<i64>,
}

impl Stats {
    pub fn total(&self) -> Usage {
        let mut t = Usage::default();
        for u in self.by_model.values() {
            t.add(u);
        }
        t
    }
}

fn tokens(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Count the replies in every transcript under `roots` once each, however many files a streamed
/// reply was written to.
pub fn usage_stats(roots: &[PathBuf]) -> Stats {
    use chrono::{Local, TimeZone as _};
    let mut st = Stats::default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut paths: Vec<PathBuf> = Vec::new();
    for root in roots {
        for proj in fs::read_dir(root).into_iter().flatten().flatten() {
            if !proj.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            for f in fs::read_dir(proj.path()).into_iter().flatten().flatten() {
                if f.file_name().to_string_lossy().ends_with(".jsonl") {
                    paths.push(f.path());
                }
            }
        }
    }
    paths.sort();
    for path in paths {
        let Ok(bytes) = fs::read(&path) else { continue };
        st.files += 1;
        let mut any = false;
        for raw in bytes.split(|b| *b == b'\n') {
            // Cheap skip of tool results and bookkeeping before parsing.
            if !raw.windows(ASSISTANT_TAG.len()).any(|w| w == ASSISTANT_TAG) {
                continue;
            }
            let Ok(l) = serde_json::from_slice::<Value>(raw) else { continue };
            let msg = l.get("message").unwrap_or(&Value::Null);
            if text(&l, "type") != "assistant" || text(msg, "model") == SYNTHETIC {
                continue;
            }
            let id = match text(msg, "id") {
                "" => text(&l, "requestId"),
                id => id,
            };
            if !id.is_empty() && !seen.insert(id.to_string()) {
                continue;
            }
            any = true;
            let u = msg.get("usage").unwrap_or(&Value::Null);
            let usage = Usage {
                input: tokens(u, "input_tokens"),
                output: tokens(u, "output_tokens"),
                cache_write: tokens(u, "cache_creation_input_tokens"),
                cache_read: tokens(u, "cache_read_input_tokens"),
                messages: 1,
            };
            st.by_model.entry(text(msg, "model").to_string()).or_default().add(&usage);
            st.by_project.entry(text(&l, "cwd").to_string()).or_default().add(&usage);
            if let Some(t) = millis(text(&l, "timestamp")) {
                if let Some(local) = Local.timestamp_millis_opt(t).single() {
                    st.by_day.entry(local.format("%Y-%m-%d").to_string()).or_default().add(&usage);
                }
                st.first_ms = Some(st.first_ms.map_or(t, |f| f.min(t)));
                st.last_ms = Some(st.last_ms.map_or(t, |f| f.max(t)));
            }
        }
        if any {
            st.sessions += 1;
        }
    }
    st
}

// ------------------------------------------------------------------------------- export

/// One turn of a conversation, for export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Turn {
    You(String),
    Claude(String),
}

/// The conversation in a transcript: what the person wrote and what Claude said, with each tool
/// use as a one-line note. Tool results, thinking and bookkeeping are left out. Consecutive lines
/// from Claude (a reply is written as one line per block) make one turn.
pub fn conversation(bytes: &[u8]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    each_line(bytes, |l| match text(l, "type") {
        "assistant" => {
            let msg = l.get("message").unwrap_or(&Value::Null);
            let Some(blocks) = msg.get("content").and_then(Value::as_array) else { return };
            if text(msg, "model") == SYNTHETIC {
                return;
            }
            for b in blocks {
                let piece = match text(b, "type") {
                    "text" => text(b, "text").trim().to_string(),
                    "tool_use" => format!("> Used tool: {}", text(b, "name")),
                    _ => continue,
                };
                if piece.is_empty() {
                    continue;
                }
                match turns.last_mut() {
                    Some(Turn::Claude(t)) => {
                        t.push_str("\n\n");
                        t.push_str(&piece);
                    }
                    _ => turns.push(Turn::Claude(piece)),
                }
            }
        }
        _ => {
            if let Some((cmd, args)) = human_prompt(l) {
                turns.push(Turn::You(if args.is_empty() {
                    cmd
                } else if cmd.is_empty() {
                    args
                } else {
                    format!("{cmd} {args}")
                }));
            }
        }
    });
    turns
}

fn stamp(ms: Option<i64>) -> String {
    use chrono::{Local, TimeZone as _};
    ms.and_then(|t| Local.timestamp_millis_opt(t).single())
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// The transcript as a Markdown document.
pub fn export_markdown(path: &Path) -> std::io::Result<String> {
    let bytes = fs::read(path)?;
    let meta = read_meta(path)?;
    let title = if meta.title.is_empty() { &meta.session } else { &meta.title };
    let mut out = format!("# {title}\n\n");
    let mut facts = vec![format!("- Session: `{}`", meta.session)];
    if !meta.cwd.is_empty() {
        facts.push(format!("- Folder: `{}`", meta.cwd));
    }
    if !meta.model.is_empty() {
        facts.push(format!("- Model: {}", meta.model));
    }
    if meta.start_ms.is_some() {
        facts.push(format!("- From {} to {}", stamp(meta.start_ms), stamp(meta.end_ms)));
    }
    out.push_str(&facts.join("\n"));
    out.push('\n');
    for turn in conversation(&bytes) {
        match turn {
            Turn::You(t) => out.push_str(&format!("\n## You\n\n{t}\n")),
            Turn::Claude(t) => out.push_str(&format!("\n## Claude\n\n{t}\n")),
        }
    }
    Ok(out)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The transcript as one self-contained web page (no scripts, no network).
pub fn export_html(path: &Path) -> std::io::Result<String> {
    let bytes = fs::read(path)?;
    let meta = read_meta(path)?;
    let title = escape(if meta.title.is_empty() { &meta.session } else { &meta.title });
    let mut body = String::new();
    for turn in conversation(&bytes) {
        let (who, t) = match &turn {
            Turn::You(t) => ("you", t),
            Turn::Claude(t) => ("claude", t),
        };
        let label = if who == "you" { "You" } else { "Claude" };
        body.push_str(&format!(
            "<section class=\"{who}\"><h2>{label}</h2><p>{}</p></section>\n",
            escape(t).replace('\n', "<br>")
        ));
    }
    let when = if meta.start_ms.is_some() {
        format!("{} – {}", stamp(meta.start_ms), stamp(meta.end_ms))
    } else {
        String::new()
    };
    Ok(format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\n\
         <title>{title}</title>\n<style>\n\
         :root{{color-scheme:light dark;--bg:#fff;--fg:#1c1c1e;--you:#eef2ff;--claude:#f5f5f4}}\n\
         @media(prefers-color-scheme:dark){{:root{{--bg:#161618;--fg:#ececec;--you:#232a45;--claude:#222224}}}}\n\
         body{{margin:0 auto;max-width:46rem;padding:1.5rem 1rem;background:var(--bg);color:var(--fg);font:16px/1.55 system-ui,sans-serif}}\n\
         h1{{font-size:1.4rem}}.meta{{opacity:.7;font-size:.9rem}}section{{margin:1rem 0;padding:.75rem 1rem;border-radius:10px}}\n\
         section.you{{background:var(--you)}}section.claude{{background:var(--claude)}}h2{{font-size:.85rem;margin:0 0 .25rem;opacity:.7}}p{{margin:0;overflow-wrap:anywhere}}\n\
         </style></head><body>\n<h1>{title}</h1>\n<p class=\"meta\">{} {}</p>\n{body}</body></html>\n",
        escape(&meta.cwd),
        escape(&when)
    ))
}

// -------------------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "5a1d3c07-8f2e-4b6a-9c1d-2e3f4a5b6c7d";

    fn line(v: serde_json::Value) -> String {
        format!("{v}\n")
    }

    fn sample() -> String {
        [
            line(serde_json::json!({"type":"custom-title","customTitle":"Fix the build"})),
            line(serde_json::json!({"type":"user","cwd":"/work/app","gitBranch":"main","timestamp":"2026-10-01T10:00:00.000Z",
                "message":{"role":"user","content":"please fix   the build\nthanks"}})),
            line(serde_json::json!({"type":"user","isMeta":true,"timestamp":"2026-10-01T10:00:01.000Z",
                "message":{"content":"<local-command-caveat>ignore</local-command-caveat>"}})),
            line(serde_json::json!({"type":"assistant","cwd":"/work/app","timestamp":"2026-10-01T10:00:05.000Z","requestId":"r1",
                "message":{"id":"m1","model":"claude-opus-5-5","content":[{"type":"text","text":"On it."}],
                "usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000}}})),
            line(serde_json::json!({"type":"assistant","cwd":"/work/app","timestamp":"2026-10-01T10:00:06.000Z","requestId":"r1",
                "message":{"id":"m1","model":"claude-opus-5-5","content":[{"type":"tool_use","name":"Bash","input":{}}],
                "usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000}}})),
            line(serde_json::json!({"type":"assistant","timestamp":"2026-10-01T10:00:07.000Z",
                "message":{"id":"m2","model":"<synthetic>","content":[{"type":"text","text":"API error"}]}})),
            line(serde_json::json!({"type":"user","timestamp":"2026-10-01T10:01:00.000Z","cwd":"/work/app",
                "message":{"content":[{"type":"tool_result","content":"ok"},{"type":"text","text":"/compact keep it short"}]}})),
        ]
        .concat()
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn a_transcript_says_what_it_is_about() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write(&tmp.path().join("proj"), &format!("{SESSION}.jsonl"), &sample());
        let m = read_meta(&path).unwrap();
        assert_eq!(m.session, SESSION);
        assert_eq!((m.cwd.as_str(), m.branch.as_str(), m.model.as_str()), ("/work/app", "main", "claude-opus-5-5"));
        assert_eq!(m.title, "Fix the build", "a custom title beats the prompt");
        assert_eq!(m.first_prompt, "please fix the build thanks");
        assert_eq!(m.prompts, 2, "the caveat is not a prompt; the tool result is not text");
        assert_eq!(m.replies, 1, "two lines of one streamed reply, and a synthetic one that is not a reply");
        assert!(m.start_ms < m.end_ms && m.is_conversation());
        let quick = quick_meta(&path).unwrap();
        assert_eq!((quick.prompts, quick.replies), (0, 0));
        assert_eq!((quick.cwd, quick.title), (m.cwd, m.title));
    }

    #[test]
    fn without_a_title_the_first_prompt_names_the_chat_and_long_ones_are_cut() {
        let tmp = tempfile::tempdir().unwrap();
        let long = "word ".repeat(60);
        let body = line(
            serde_json::json!({"type":"user","cwd":"/w","timestamp":"2026-10-01T10:00:00Z","message":{"content":long}}),
        );
        let m = read_meta(&write(tmp.path(), "a.jsonl", &body)).unwrap();
        assert_eq!(m.title.chars().count(), TITLE_MAX);
        assert!(m.title.ends_with('…'));
        let nothing = read_meta(&write(tmp.path(), "b.jsonl", "{\"type\":\"summary\"}\n")).unwrap();
        assert!(!nothing.is_conversation());
    }

    #[test]
    fn a_big_transcript_is_read_at_both_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let filler = line(serde_json::json!({"type":"progress","data":"x".repeat(1000)}));
        let mut body = sample().lines().take(2).map(|l| format!("{l}\n")).collect::<String>();
        body.push_str(&filler.repeat(400)); // > head + tail
        body.push_str(&line(serde_json::json!({"type":"ai-title","aiTitle":"Late title"})));
        body.push_str(&line(serde_json::json!({"type":"assistant","timestamp":"2026-10-02T09:00:00Z",
            "message":{"id":"m9","model":"claude-sonnet-5-5","content":[]}})));
        assert!(body.len() as u64 > HEAD_BYTES + TAIL_BYTES);
        let path = write(tmp.path(), "big.jsonl", &body);
        let m = quick_meta(&path).unwrap();
        assert_eq!(m.cwd, "/work/app");
        assert_eq!(m.model, "claude-sonnet-5-5", "the last model is at the end");
        assert_eq!(m.title, "Fix the build", "{m:?}");
    }

    #[test]
    fn tokens_are_counted_once_per_reply_across_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("a"), "1.jsonl", &sample());
        write(&tmp.path().join("b"), "2.jsonl", &sample()); // a resumed chat repeats earlier replies
        let st = usage_stats(&[tmp.path().to_path_buf()]);
        assert_eq!((st.files, st.sessions), (2, 1));
        let opus = st.by_model["claude-opus-5-5"];
        assert_eq!(opus, Usage { input: 10, output: 5, cache_write: 100, cache_read: 1000, messages: 1 });
        assert!(!st.by_model.contains_key("<synthetic>"));
        assert_eq!(st.total().total(), 1115);
        assert_eq!(st.by_project["/work/app"].messages, 1);
        assert_eq!(st.by_day.len(), 1);
    }

    #[test]
    fn the_conversation_keeps_words_and_tool_uses_only() {
        let turns = conversation(sample().as_bytes());
        assert_eq!(
            turns,
            [
                Turn::You("please fix   the build\nthanks".into()),
                Turn::Claude("On it.\n\n> Used tool: Bash".into()),
                Turn::You("/compact keep it short".into()),
            ]
        );
    }

    #[test]
    fn a_byte_order_mark_does_not_hide_the_first_line() {
        let tmp = tempfile::tempdir().unwrap();
        let body =
            format!("\u{feff}{}", line(serde_json::json!({"type":"user","cwd":"/w","message":{"content":"hello"}})));
        let m = read_meta(&write(tmp.path(), "bom.jsonl", &body)).unwrap();
        assert_eq!((m.cwd.as_str(), m.first_prompt.as_str()), ("/w", "hello"));
    }

    #[test]
    fn exports_are_readable_and_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let body = line(serde_json::json!({"type":"user","cwd":"/w","timestamp":"2026-10-01T10:00:00Z",
            "message":{"content":"<script>alert(1)</script> & more"}}));
        let path = write(tmp.path(), "x.jsonl", &body);
        let md = export_markdown(&path).unwrap();
        assert!(md.starts_with("# ") && md.contains("## You") && md.contains("- Folder: `/w`"));
        let html = export_html(&path).unwrap();
        assert!(!html.contains("<script>alert") && html.contains("&lt;script&gt;") && html.contains("&amp; more"));
        assert!(!html.contains("src=") && !html.contains("http"), "self-contained: no network");
    }
}
