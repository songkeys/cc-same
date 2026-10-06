//! Plain data types shared by the scanner, the planner and the front-ends.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

pub type Json = Map<String, Value>;

/// Desktop's own name for its archive hint file.
pub const ARCHIVE_IDX: &str = "archived-sessions.idx";
/// Desktop refuses records larger than this.
pub const MAX_RECORD_BYTES: u64 = 10 * 1024 * 1024;
pub const MAX_COLLECTION_BYTES: u64 = 8 * 1024 * 1024;

/// Session fields that belong to one account/org and must not travel with a session.
pub const ACCOUNT_LOCAL_KEYS: &[&str] = &[
    "remoteMcpServersConfig", // the org's connectors (uuids + urls)
    "enabledMcpTools",        // tool toggles keyed by connector uuid
    "withheldConnectorHosts",
    "bridgeSessionIds", // Remote Control mirrors are server sessions owned by the account
    "bridgeSessionId",
    "steeredByRemoteClient",
    "publishedArtifacts", // artifacts belong to the account that published them
    "isStarred",          // pins are star-synced with each account's server settings
];
pub const ACCOUNT_LOCAL_PREFIXES: &[&str] = &["remoteControl"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    Code,
}

impl Surface {
    pub const ALL: [Surface; 1] = [Surface::Code];

    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Code => "code",
        }
    }

    /// Folder under Desktop's user data directory.
    pub fn dir_name(self) -> &'static str {
        match self {
            Surface::Code => "claude-code-sessions",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Surface::Code => "Code",
        }
    }

    /// JSON collection files kept in every partition of this surface.
    pub fn collections(self) -> &'static [&'static str] {
        match self {
            Surface::Code => &["scheduled-tasks.json", "backlog/tasks.json"],
        }
    }

    pub fn parse(s: &str) -> Option<Surface> {
        match s.trim() {
            "code" => Some(Surface::Code),
            _ => None,
        }
    }
}

impl fmt::Display for Surface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `<surface>/<account>/<org>` index folder.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Partition {
    pub surface: Surface,
    pub acct: String,
    pub org: String,
    pub path: PathBuf,
    /// The folder (or its account folder) is a symlink. Desktop cannot save through it.
    pub is_link: bool,
}

impl Partition {
    pub fn key(&self) -> String {
        format!("{}/{}", self.acct, self.org)
    }

    pub fn label(&self) -> String {
        format!("{}/{}", short(&self.acct), short(&self.org))
    }
}

pub fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// A `local_<uuid>.json` session record. Only the portable part is kept in memory:
/// account-bound fields can be ~97% of a record and are read again at write time.
#[derive(Clone, Debug)]
pub struct Record {
    pub uuid: String,
    pub path: PathBuf,
    pub mtime_ns: i128,
    pub size: u64,
    pub nlink: u64,
    pub data: Option<Arc<Json>>,
    pub error: Option<String>,
}

impl Record {
    pub fn mtime_ms(&self) -> i128 {
        self.mtime_ns.div_euclid(1_000_000)
    }
}

/// A `deleted_<id>` marker. Desktop writes one for the session id and one per transcript id.
#[derive(Clone, Debug)]
pub struct Tomb {
    pub id: String,
    pub path: PathBuf,
    pub ts_ms: i128,
    pub raw: Arc<Vec<u8>>,
    pub mtime_ns: i128,
}

/// A JSON collection file such as `scheduled-tasks.json`.
#[derive(Clone, Debug)]
pub struct Collection {
    pub rel: &'static str,
    pub path: PathBuf,
    pub mtime_ns: i128,
    pub data: Option<Json>,
    pub error: Option<String>,
    /// Indentation Desktop used, so a rewrite keeps its formatting.
    pub indent: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct PartState {
    pub part: Partition,
    pub records: BTreeMap<String, Record>,
    pub tombs: BTreeMap<String, Tomb>,
    pub collections: BTreeMap<&'static str, Collection>,
    pub archive_idx: Option<Vec<u8>>,
    pub unmanaged: Vec<String>,
    pub stale_tmp: Vec<String>,
    pub error: Option<String>,
}

impl PartState {
    pub fn new(part: Partition) -> PartState {
        PartState {
            part,
            records: BTreeMap::new(),
            tombs: BTreeMap::new(),
            collections: BTreeMap::new(),
            archive_idx: None,
            unmanaged: Vec::new(),
            stale_tmp: Vec::new(),
            error: None,
        }
    }

    pub fn live_sessions(&self) -> usize {
        self.records.values().filter(|r| r.data.is_some()).count()
    }

    pub fn archived_sessions(&self) -> usize {
        self.records
            .values()
            .filter(|r| r.data.as_deref().is_some_and(|d| d.get("isArchived") == Some(&Value::Bool(true))))
            .count()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ActionKind {
    WriteRecord,
    TrashRecord,
    WriteTomb,
    TrashTomb,
    WriteCollection,
    WriteArchiveIdx,
}

#[derive(Clone, Debug)]
pub enum ActionExtra {
    None,
    /// The winning session's portable fields; the target's own account-bound fields are
    /// merged in at write time. `expect_mtime_ns` guards against the file changing meanwhile.
    Record {
        portable: Arc<Json>,
        expect_mtime_ns: Option<i128>,
    },
    /// The member's base before this merge (`None`: it had none), restored if the write
    /// does not happen.
    Collection {
        rel: String,
        prev_base: Option<Value>,
    },
}

#[derive(Clone, Debug)]
pub struct Action {
    pub kind: ActionKind,
    pub part: Partition,
    /// Path relative to the partition folder.
    pub name: String,
    pub payload: Option<Arc<Vec<u8>>>,
    pub mtime_ns: Option<i128>,
    pub src: Option<Partition>,
    /// Creates a new file only (allowed into a freshly seen index Desktop has loaded).
    pub additive: bool,
    pub extra: ActionExtra,
}

impl Action {
    pub(crate) fn new(kind: ActionKind, part: &Partition, name: impl Into<String>) -> Action {
        Action {
            kind,
            part: part.clone(),
            name: name.into(),
            payload: None,
            mtime_ns: None,
            src: None,
            additive: false,
            extra: ActionExtra::None,
        }
    }
}

/// What a sync would do, plus the bookkeeping to commit once it is done.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    pub notes: Vec<String>,
    /// Index folders left out because they could not be read, with why.
    pub skipped: Vec<String>,
    /// Changes waiting because Desktop has that index loaded, per partition key.
    pub deferred: BTreeMap<String, usize>,
    /// surface -> collection file -> member key (or `__group__`) -> merged value.
    pub bases: BTreeMap<Surface, BTreeMap<String, BTreeMap<String, Value>>>,
    pub members: BTreeMap<Surface, Vec<Partition>>,
    /// Sessions copied into an index Desktop has loaded (visible after a restart).
    pub seeded_while_loaded: BTreeMap<String, usize>,
    /// Live sessions per surface after the sync.
    pub alive: BTreeMap<Surface, usize>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    pub fn deferred_total(&self) -> usize {
        self.deferred.values().sum()
    }
}

/// What Claude Desktop is doing right now, as far as we can tell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppState {
    pub running: bool,
    /// Accounts Desktop may have loaded (conservative: includes a just-left account).
    pub accounts: BTreeSet<String>,
    /// Exact (account, org) pairs from Desktop's log, when known.
    pub pairs: BTreeSet<(String, String)>,
    /// Running, but we cannot tell which account: treat every index as loaded.
    pub uncertain: bool,
    pub source: String,
    /// Identifies Desktop's latest session-list load (log file + offset of its init line).
    /// Changes whenever Desktop reloads the list, e.g. after a restart.
    pub init_marker: Option<String>,
    /// The account Desktop shows while it runs: the one it signed in to last. For telling
    /// people; what may be written is [`AppState::loaded`]'s call, which also counts a
    /// just-left account, or every account when unsure.
    pub open_account: Option<String>,
    /// That account's organization, when Desktop's log names it.
    pub open_org: Option<String>,
}

impl AppState {
    /// Desktop has this index loaded, so we must not modify it.
    pub fn loaded(&self, p: &Partition) -> bool {
        if !self.running {
            return false;
        }
        if self.uncertain {
            return true;
        }
        self.accounts.contains(&p.acct) || self.pairs.contains(&(p.acct.clone(), p.org.clone()))
    }

    /// Desktop shows this index now, as far as we know (every index of the open account when
    /// its organization is unknown). For telling people, not for deciding what may be written.
    pub fn showing(&self, p: &Partition) -> bool {
        self.open_account.as_deref() == Some(p.acct.as_str()) && self.open_org.as_ref().is_none_or(|o| *o == p.org)
    }
}

pub fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

/// Ids Desktop uses in `deleted_<id>` markers and transcript names.
pub fn is_marker_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn is_account_local(key: &str) -> bool {
    ACCOUNT_LOCAL_KEYS.contains(&key) || ACCOUNT_LOCAL_PREFIXES.iter().any(|p| key.starts_with(p))
}

pub fn portable(d: &Json) -> Json {
    d.iter().filter(|(k, _)| !is_account_local(k)).map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// The session as it should look in a target partition: the source's portable fields plus
/// the target's own account-bound fields (none for a new copy).
pub fn desired_record(src_portable: &Json, existing: Option<&Json>) -> Json {
    let mut out = src_portable.clone();
    if let Some(existing) = existing {
        for (k, v) in existing {
            if is_account_local(k) {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

/// Transcript ids a session refers to (its CLI session plus earlier ones).
pub fn transcript_ids(d: &Json) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for k in ["cliSessionId", "unarchivedCliSessionId", "preClearCliSessionId"] {
        if let Some(Value::String(v)) = d.get(k) {
            if is_marker_id(v) {
                ids.insert(v.clone());
            }
        }
    }
    if let Some(Value::Array(prior)) = d.get("priorCliSessionIds") {
        for v in prior {
            if let Value::String(v) = v {
                if is_marker_id(v) {
                    ids.insert(v.clone());
                }
            }
        }
    }
    ids
}
