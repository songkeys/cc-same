use crate::config::Config;
use crate::model::Record;
use crate::paths::Paths;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

/// Overrides for tests and for driving the binaries from scripts
/// (`CC_SAME_FAKE_RUNNING=0|1`, `CC_SAME_FAKE_ACTIVE=<account>[/<org>],…`).
#[derive(Clone, Debug, Default)]
pub struct FakeDesktop {
    pub running: Option<bool>,
    pub active: Option<Vec<String>>,
}

impl FakeDesktop {
    pub fn from_env() -> FakeDesktop {
        let running = match std::env::var("CC_SAME_FAKE_RUNNING").as_deref() {
            Ok("1") => Some(true),
            Ok("0") => Some(false),
            _ => None,
        };
        let active = std::env::var("CC_SAME_FAKE_ACTIVE")
            .ok()
            .map(|v| v.split(',').filter(|s| !s.is_empty()).map(str::to_string).collect());
        FakeDesktop { running, active }
    }
}

#[derive(Clone, Debug)]
pub enum LogSink {
    Stderr,
    File(PathBuf),
    Silent,
}

pub struct Logger {
    sink: Mutex<LogSink>,
}

impl Logger {
    pub fn new(sink: LogSink) -> Logger {
        Logger { sink: Mutex::new(sink) }
    }

    pub fn sink(&self) -> LogSink {
        self.sink.lock().unwrap().clone()
    }

    pub fn set_sink(&self, sink: LogSink) {
        *self.sink.lock().unwrap() = sink;
    }

    pub fn log(&self, msg: impl AsRef<str>) {
        let msg = msg.as_ref();
        match &*self.sink.lock().unwrap() {
            LogSink::Stderr => eprintln!("{msg}"),
            LogSink::Silent => {}
            LogSink::File(path) => {
                if fs::metadata(path).map(|m| m.len() > 5 * 1024 * 1024).unwrap_or(false) {
                    let _ = fs::rename(path, path.with_extension("log.1"));
                }
                if let Some(parent) = path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
                    let _ = writeln!(f, "{now} {msg}");
                }
            }
        }
    }
}

pub(crate) type RecordKey = (PathBuf, i128, u64);
/// When the transcript index was built, and the ids it found (None: no projects folder).
pub(crate) type TranscriptCache = Option<(Instant, Option<Arc<HashSet<String>>>)>;

#[derive(Default)]
pub(crate) struct LogScan {
    pub offset: u64,
    pub found: Option<(String, String)>,
    /// Byte offset of the line `found` came from.
    pub found_at: u64,
    /// When that line was written (Unix seconds), from its timestamp.
    pub found_time: Option<f64>,
}

#[derive(Default)]
pub(crate) struct Caches {
    pub records: Mutex<HashMap<RecordKey, Record>>,
    pub recent_accounts: Mutex<HashMap<String, Instant>>,
    pub transcripts: Mutex<TranscriptCache>,
    pub log_scan: Mutex<HashMap<PathBuf, LogScan>>,
}

/// Everything a sync needs: locations, settings, test overrides, a logger and caches.
/// Shareable across threads.
pub struct Ctx {
    pub paths: Paths,
    config: RwLock<Config>,
    pub fake: FakeDesktop,
    pub logger: Logger,
    pub verbose: bool,
    pub(crate) caches: Caches,
}

impl Ctx {
    pub fn new(paths: Paths, config: Config, fake: FakeDesktop, sink: LogSink) -> Ctx {
        Ctx {
            paths,
            config: RwLock::new(config),
            fake,
            logger: Logger::new(sink),
            verbose: false,
            caches: Caches::default(),
        }
    }

    /// Default paths and the saved config, with test overrides from the environment.
    pub fn detect(sink: LogSink) -> anyhow::Result<Ctx> {
        let paths = Paths::detect();
        let config = Config::load(&paths)?;
        Ok(Ctx::new(paths, config, FakeDesktop::from_env(), sink))
    }

    pub fn config(&self) -> Config {
        self.config.read().unwrap().clone()
    }

    pub fn set_config(&self, config: Config) {
        *self.config.write().unwrap() = config;
    }

    /// Re-read `config.json` (the agent does this every pass).
    pub fn reload_config(&self) -> anyhow::Result<()> {
        let cfg = Config::load(&self.paths)?;
        self.set_config(cfg);
        Ok(())
    }

    pub fn log(&self, msg: impl AsRef<str>) {
        self.logger.log(msg);
    }

    pub fn debug(&self, msg: impl AsRef<str>) {
        if self.verbose {
            self.logger.log(msg);
        }
    }
}
