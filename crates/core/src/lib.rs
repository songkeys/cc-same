//! Keep Claude Desktop's local sessions identical across all your accounts.
//!
//! Claude Desktop keeps one session index per account and organization, e.g. on macOS
//! `~/Library/Application Support/Claude/claude-code-sessions/<account>/<org>/local_<uuid>.json`.
//! Transcripts live in `~/.claude/projects` and are shared, so after an account switch the
//! history is still on disk; only the new account's index is empty.
//!
//! How every index is kept identical:
//! * Every linked index stays a real folder. Desktop 2.x opens its storage folder with
//!   `O_DIRECTORY|O_NOFOLLOW` and silently stops saving sessions through a symlink.
//! * One writer at a time. Desktop writes only the loaded account's index and reads it only
//!   when that account loads. We never modify an index Desktop has loaded; we mirror it to
//!   the others and catch it up after an account switch or a quit.
//! * Per session the newest file wins; mtimes travel with copies, so a copy never looks newer
//!   than its source. Deletions travel as Desktop's own `deleted_<id>` markers.
//! * Account-bound fields (org connectors, Remote Control mirrors, published artifacts, pins)
//!   stay with the account that owns them.
//! * Nothing is hard-deleted: copy-on-write snapshots before changes, a trash folder for
//!   anything removed or replaced.

pub mod accounts;
pub mod apply;
pub mod config;
pub mod desktop;
pub mod fsx;
pub mod hooks;
pub mod logins;
pub mod merge;
pub mod model;
pub mod notify;
pub mod paths;
pub mod plan;
pub mod report;
pub mod retention;
pub mod scan;
pub mod service;
pub mod snapshot;
pub mod usage;
pub mod watch;

mod ctx;

pub use config::{Config, State};
pub use ctx::{Ctx, FakeDesktop, LogSink, Logger};
pub use model::*;
pub use paths::Paths;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
