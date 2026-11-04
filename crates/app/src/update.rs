//! Updates: the newest release on GitHub, and putting it in place of this copy.
//!
//! Once a day, and whenever asked, the app asks GitHub for the latest release. Installing one
//! downloads this system's archive and checks it against the size and SHA-256 digest GitHub
//! recorded when it was uploaded. Then it unpacks the archive and makes sure the new program starts
//! and reports the expected version; on macOS it must also carry a valid signature from the same
//! team as this copy. Only then is the new copy swapped in and started, and this one quits. The new
//! copy waits for the old one to be gone, then tidies up.

use anyhow::{Context as _, anyhow, bail};
use cc_same_core::{Paths, fsx};
use semver::Version;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// This copy's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const LATEST: &str = "https://api.github.com/repos/Arcaneology/cc-same/releases/latest";
/// Where people can download the latest release themselves.
pub const DOWNLOADS: &str = "https://github.com/Arcaneology/cc-same/releases/latest";
/// What changed in each version, as of this build.
pub const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");
/// Given to a freshly installed copy, followed by the process id of the copy it replaces.
pub const AFTER_UPDATE: &str = "--after-update";
/// Makes the program print its name and version, and exit.
pub const PRINT_VERSION: &str = "--version";

/// A release on GitHub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// The release notes, in Markdown.
    pub notes: String,
    /// The release page.
    pub page: String,
    /// The archive for this system; none when the release has no build for it.
    pub asset: Option<Asset>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// The SHA-256 GitHub recorded at upload, in lowercase hex.
    pub sha256: Option<String>,
}

/// What stopped a check or an update, in terms people can act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// GitHub could not be reached, or the connection broke off.
    Offline,
    /// GitHub turned the request down (it limits how often it is asked) or answered nonsense.
    Refused,
    /// The release has no build for this system.
    NoBuild,
    /// This copy is not an installed app: a development build, or a program outside its bundle.
    NotInstalled,
    /// This copy runs from a disk image, or from the temporary read-only place macOS uses until
    /// the app is moved to Applications.
    Translocated,
    /// This copy's folder cannot be written to.
    ReadOnly,
    /// The download failed a check: its size, its checksum, its signature, or it would not start.
    Unverified,
    Other,
}

impl Problem {
    /// The message for people, in `locales/app.yml`.
    pub fn message_key(self) -> &'static str {
        match self {
            Problem::Offline => "update.problem.offline",
            Problem::Refused => "update.problem.refused",
            Problem::NoBuild => "update.problem.no_build",
            Problem::NotInstalled => "update.problem.not_installed",
            Problem::Translocated => "update.problem.translocated",
            Problem::ReadOnly => "update.problem.read_only",
            Problem::Unverified => "update.problem.unverified",
            Problem::Other => "update.problem.other",
        }
    }
}

/// A check or an update that did not work: what to tell people, and the error behind it.
#[derive(Debug)]
pub struct Failure {
    pub problem: Problem,
    pub error: anyhow::Error,
}

impl Failure {
    fn new(problem: Problem, error: impl Into<anyhow::Error>) -> Failure {
        Failure { problem, error: error.into() }
    }
}

pub type Outcome<T> = Result<T, Failure>;

/// Says which [`Problem`] an error means for people.
trait Because<T> {
    fn because(self, problem: Problem) -> Outcome<T>;
}

impl<T, E: Into<anyhow::Error>> Because<T> for Result<T, E> {
    fn because(self, problem: Problem) -> Outcome<T> {
        self.map_err(|e| Failure::new(problem, e))
    }
}

pub fn current() -> Version {
    Version::parse(VERSION).unwrap_or_else(|_| Version::new(0, 0, 0))
}

/// Whether this copy looks after itself: looks for updates, installs them, and keeps the
/// background agent in step with it. A development build does not (set `CC_SAME_DEV_UPDATE=1`
/// to try).
pub fn self_managing() -> bool {
    !cfg!(debug_assertions) || std::env::var_os("CC_SAME_DEV_UPDATE").is_some()
}

/// Whether `path` is somewhere an app cannot stay: a disk image, or the temporary read-only
/// place macOS runs a downloaded app from until it is moved (App Translocation).
#[cfg(target_os = "macos")]
pub fn temporary_place(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;
    if path.components().any(|c| c.as_os_str() == "AppTranslocation") {
        return true;
    }
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    let mut volume: libc::statfs = unsafe { std::mem::zeroed() };
    let found = unsafe { libc::statfs(c_path.as_ptr(), &mut volume) } == 0;
    found && volume.f_flags & libc::MNT_RDONLY as u32 != 0
}

#[cfg(not(target_os = "macos"))]
pub fn temporary_place(_path: &Path) -> bool {
    false
}

// ---------------------------------------------------------------------- asking GitHub

fn client(timeout: Duration) -> anyhow::Result<reqwest::blocking::Client> {
    // ring does the cryptography; installing it a second time changes nothing.
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(reqwest::blocking::Client::builder()
        .user_agent(format!("CC-Same/{VERSION} (+https://github.com/Arcaneology/cc-same)"))
        .connect_timeout(Duration::from_secs(20))
        .timeout(timeout)
        .build()?)
}

/// Where the latest release is described: GitHub, unless a development build is pointed at a
/// test feed (`CC_SAME_UPDATE_FEED`) to try updates.
fn feed() -> String {
    match std::env::var("CC_SAME_UPDATE_FEED") {
        Ok(url) if cfg!(debug_assertions) => url,
        _ => LATEST.to_string(),
    }
}

/// The latest release when it is newer than this copy; `None` when this copy is the latest.
pub fn check() -> Outcome<Option<Release>> {
    let response = client(Duration::from_secs(30))
        .and_then(|client| {
            Ok(client
                .get(feed())
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .send()?)
        })
        .because(Problem::Offline)?;
    if !response.status().is_success() {
        return Err(Failure::new(Problem::Refused, anyhow!("GitHub answered {}", response.status())));
    }
    let body = response.bytes().because(Problem::Offline)?;
    let release = serde_json::from_slice(&body).map_err(anyhow::Error::from).and_then(|json| parse(&json));
    let release = release.because(Problem::Refused)?;
    Ok((release.version > current()).then_some(release))
}

/// A release from GitHub's REST API.
fn parse(json: &Value) -> anyhow::Result<Release> {
    let tag = json["tag_name"].as_str().context("the release has no tag")?;
    let version = Version::parse(tag.trim_start_matches('v'))
        .with_context(|| format!("the release tag {tag} is not a version"))?;
    let wanted = archive_name(&version);
    let asset = json["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| wanted.as_deref().is_some_and(|name| a["name"] == name))
        .and_then(|a| {
            Some(Asset {
                name: a["name"].as_str()?.to_string(),
                url: a["browser_download_url"].as_str()?.to_string(),
                size: a["size"].as_u64()?,
                sha256: a["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(str::to_ascii_lowercase),
            })
        });
    Ok(Release {
        version,
        notes: json["body"].as_str().unwrap_or_default().trim().to_string(),
        page: json["html_url"].as_str().unwrap_or(DOWNLOADS).to_string(),
        asset,
    })
}

/// This system's archive in a release, like `CC-Same-0.2.0-macos-arm64.zip`.
fn archive_name(version: &Version) -> Option<String> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "macos-arm64",
        ("macos", "x86_64") => "macos-x64",
        ("windows", "x86_64") => "windows-x64",
        ("linux", "x86_64") => "linux-x64",
        ("linux", "aarch64") => "linux-arm64",
        _ => return None,
    };
    let extension = if cfg!(target_os = "linux") { "tar.gz" } else { "zip" };
    Some(format!("CC-Same-{version}-{platform}.{extension}"))
}

// ---------------------------------------------------------------------- downloading

/// Download `asset` into `dir` (emptied first), counting the bytes into `progress`, and check its
/// size and digest.
pub fn download(asset: &Asset, dir: &Path, progress: &AtomicU64) -> Outcome<PathBuf> {
    let _ = fs::remove_dir_all(dir);
    fsx::create_private_dir_all(dir).because(Problem::Other)?;
    let path = dir.join(&asset.name);
    let mut response = client(Duration::from_secs(30 * 60))
        .and_then(|client| Ok(client.get(&asset.url).send()?.error_for_status()?))
        .because(Problem::Offline)?;
    let mut file = fs::File::create(&path).because(Problem::Other)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = response.read(&mut buffer).because(Problem::Offline)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > asset.size {
            let error = anyhow!("{} is larger than the {} bytes GitHub lists", asset.name, asset.size);
            return Err(Failure::new(Problem::Unverified, error));
        }
        hasher.update(&buffer[..n]);
        file.write_all(&buffer[..n]).because(Problem::Other)?;
        progress.store(total, Ordering::Relaxed);
    }
    file.sync_all().because(Problem::Other)?;
    if total != asset.size {
        let error = anyhow!("the download of {} stopped at {total} of {} bytes", asset.name, asset.size);
        return Err(Failure::new(Problem::Offline, error));
    }
    if let Some(expected) = &asset.sha256 {
        let actual = format!("{:x}", hasher.finalize());
        if actual != *expected {
            let error = anyhow!("{} has the SHA-256 {actual}, not {expected}", asset.name);
            return Err(Failure::new(Problem::Unverified, error));
        }
    }
    Ok(path)
}

/// Unpack a downloaded archive next to it and check the new copy: it has to start and report
/// `version`, and on macOS be signed like `target`, the copy it is to replace. Returns what
/// [`install`] swaps in: the app bundle on macOS, the program on Windows, the unpacked folder
/// (`bin/`, `share/`) on Linux.
pub fn unpack(archive: &Path, version: &Version, target: &Path) -> Outcome<PathBuf> {
    let dir = archive.with_file_name("unpacked");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).because(Problem::Other)?;
    platform::extract(archive, &dir).because(Problem::Unverified)?;
    let (staged, program) = platform::find(&dir).because(Problem::Unverified)?;
    check_version(&program, version).because(Problem::Unverified)?;
    platform::check_signature(&staged, target).because(Problem::Unverified)?;
    Ok(staged)
}

/// Start the new program with `--version` and compare what it says.
fn check_version(program: &Path, version: &Version) -> anyhow::Result<()> {
    let mut child = Command::new(program)
        .arg(PRINT_VERSION)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {}", program.display()))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} did not answer in time", program.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut said = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut said);
    }
    let expected = format!("{} {version}", crate::APP_NAME);
    if !status.success() || said.trim() != expected {
        bail!("{} says {:?} ({status}), not {expected:?}", program.display(), said.trim());
    }
    Ok(())
}

// ---------------------------------------------------------------------- installing

/// What an update replaces: the app bundle on macOS, the program elsewhere.
pub fn target() -> Outcome<PathBuf> {
    if !self_managing() {
        let error = anyhow!("a development build does not update itself (set CC_SAME_DEV_UPDATE=1)");
        return Err(Failure::new(Problem::NotInstalled, error));
    }
    let exe = std::env::current_exe().because(Problem::Other)?;
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    platform::installed(exe)
}

/// Put the unpacked copy in place of `target` and start it; the caller then quits. `hidden`
/// starts it without a window, in the menu bar or notification area.
pub fn install(staged: &Path, target: &Path, hidden: bool) -> Outcome<()> {
    platform::swap(staged, target)?;
    relaunch(target, hidden).because(Problem::Other)
}

/// Put the unpacked copy in place of `target` without starting it: the app is quitting, and the
/// next start is the new version.
pub fn install_on_quit(staged: &Path, target: &Path) -> Outcome<()> {
    platform::swap(staged, target)
}

fn relaunch(target: &Path, hidden: bool) -> anyhow::Result<()> {
    let pid = std::process::id().to_string();
    let mut args = vec![AFTER_UPDATE, pid.as_str()];
    if hidden {
        args.push(crate::login::HIDDEN);
    }
    if cfg!(target_os = "macos") {
        // Through Launch Services, as if opened in the Finder; `-n` because this copy still runs.
        let mut open = Command::new("/usr/bin/open");
        open.arg("-n");
        // Launch Services starts apps with a fresh environment: pass on this copy's own settings.
        for (name, value) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("CC_SAME_") {
                let mut pair = name;
                pair.push("=");
                pair.push(value);
                open.arg("--env").arg(pair);
            }
        }
        let status = open
            .arg(target)
            .arg("--args")
            .args(&args)
            .stdin(Stdio::null())
            .status()
            .context("starting the new copy")?;
        if !status.success() {
            bail!("open could not start {} ({status})", target.display());
        }
    } else {
        Command::new(target)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {}", target.display()))?;
    }
    Ok(())
}

/// At startup: when an update started this copy, wait for the copy it replaced to quit. Then
/// clear away what updates leave behind.
pub fn finish(paths: &Paths) {
    let mut args = std::env::args().skip_while(|a| a != AFTER_UPDATE).skip(1);
    if let Some(pid) = args.next().and_then(|p| p.parse::<u32>().ok()) {
        platform::wait_for_exit(pid, Duration::from_secs(10));
    }
    let _ = fs::remove_dir_all(updates_dir(paths));
    if let Ok(target) = target() {
        platform::remove_leftovers(&target);
    }
}

/// Where updates are downloaded and unpacked.
pub fn updates_dir(paths: &Paths) -> PathBuf {
    paths.state_dir.join("updates")
}

/// The version that ran before this one, when this is its first run since an update. Remembers
/// this version for the next start.
pub fn updated_from(paths: &Paths) -> Option<Version> {
    let file = paths.state_dir.join("app.json");
    let last = match fsx::read_json(&file, 64 * 1024) {
        Ok(json) => json["lastVersion"].as_str().and_then(|v| Version::parse(v).ok()),
        // 0.1.0, the only version from before this file, leaves settings or state behind.
        Err(_) if paths.config_file().exists() || paths.state_file().exists() => Some(Version::new(0, 1, 0)),
        Err(_) => None,
    };
    if last.as_ref() != Some(&current()) {
        let body = serde_json::json!({ "lastVersion": VERSION }).to_string();
        let _ = fsx::create_private_dir_all(&paths.state_dir);
        let _ = fsx::atomic_write(&file, body.as_bytes(), None, false);
    }
    last.filter(|v| *v < current())
}

/// The changelog's sections for the versions after `after` up to this one (every version for
/// `None`), newest first.
pub fn changes_since(after: Option<&Version>) -> String {
    let mut out = String::new();
    let mut keep = false;
    for line in CHANGELOG.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            let version = heading.split_whitespace().next().and_then(|v| Version::parse(v).ok());
            keep = version.is_some_and(|v| v <= current() && after.is_none_or(|after| v > *after));
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

fn run(command: &mut Command) -> anyhow::Result<()> {
    let program = command.get_program().to_string_lossy().into_owned();
    let out = command.stdin(Stdio::null()).output().with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!("{program} failed ({}): {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// The first entry of `dir` that `wanted` accepts.
fn find_entry(dir: &Path, wanted: impl Fn(&Path) -> bool) -> anyhow::Result<PathBuf> {
    fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| wanted(p))
        .with_context(|| format!("the archive does not have what was expected in {}", dir.display()))
}

/// The file name of `path`, for building names next to it.
fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(unix)]
fn is_running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    // Signal 0 only asks whether the process exists.
    let exists = unsafe { libc::kill(pid, 0) } == 0;
    exists || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub fn installed(exe: PathBuf) -> Outcome<PathBuf> {
        // …/CC Same.app/Contents/MacOS/CC Same
        let bundle = exe
            .parent()
            .filter(|dir| dir.ends_with("Contents/MacOS"))
            .and_then(Path::parent)
            .and_then(Path::parent)
            .filter(|bundle| bundle.extension().is_some_and(|e| e == "app"))
            .map(Path::to_path_buf);
        let Some(bundle) = bundle else {
            let error = anyhow!("{} is not inside an app bundle", exe.display());
            return Err(Failure::new(Problem::NotInstalled, error));
        };
        if temporary_place(&bundle) {
            let error = anyhow!("this copy runs from {}, which it cannot replace", bundle.display());
            return Err(Failure::new(Problem::Translocated, error));
        }
        Ok(bundle)
    }

    pub fn extract(archive: &Path, dir: &Path) -> anyhow::Result<()> {
        run(Command::new("/usr/bin/ditto").args(["-x", "-k"]).arg(archive).arg(dir))
    }

    pub fn find(dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
        let app = find_entry(dir, |p| p.extension().is_some_and(|e| e == "app"))?;
        let program = find_entry(&app.join("Contents/MacOS"), Path::is_file)?;
        Ok((app, program))
    }

    /// A valid signature, from the same team as `target`. When `target` has none (built
    /// locally), Gatekeeper has to accept the new copy instead.
    pub fn check_signature(app: &Path, target: &Path) -> anyhow::Result<()> {
        run(Command::new("/usr/bin/codesign").args(["--verify", "--deep", "--strict"]).arg(app))?;
        match (team(target), team(app)) {
            (Some(ours), theirs) => {
                if theirs.as_deref() != Some(ours.as_str()) {
                    bail!("the new copy is signed by team {theirs:?}, not {ours}");
                }
            }
            // A development build trying updates takes another local build as it is.
            (None, None) if self_managing() && cfg!(debug_assertions) => {}
            (None, _) => run(Command::new("/usr/sbin/spctl").args(["--assess", "--type", "execute"]).arg(app))?,
        }
        Ok(())
    }

    /// The team a bundle is signed by, if any.
    fn team(path: &Path) -> Option<String> {
        let out = Command::new("/usr/bin/codesign").args(["-dv", "--verbose=2"]).arg(path).output().ok()?;
        // codesign describes the signature on stderr.
        let text = String::from_utf8_lossy(&out.stderr).into_owned();
        let team = text.lines().find_map(|l| l.strip_prefix("TeamIdentifier="))?.trim();
        (team != "not set").then(|| team.to_string())
    }

    pub fn swap(staged: &Path, target: &Path) -> Outcome<()> {
        let (incoming, outgoing) = (sibling(target, "incoming"), sibling(target, "outgoing"));
        let _ = fs::remove_dir_all(&incoming);
        let _ = fs::remove_dir_all(&outgoing);
        // A rename on the same volume; a copy otherwise, which ditto makes without breaking the
        // signature.
        if fs::rename(staged, &incoming).is_err() {
            run(Command::new("/usr/bin/ditto").arg(staged).arg(&incoming)).because(Problem::ReadOnly)?;
        }
        if let Err(e) = fs::rename(target, &outgoing) {
            let _ = fs::remove_dir_all(&incoming);
            return Err(Failure::new(Problem::ReadOnly, e));
        }
        if let Err(e) = fs::rename(&incoming, target) {
            let _ = fs::rename(&outgoing, target);
            return Err(Failure::new(Problem::ReadOnly, e));
        }
        // The old copy is still running; the new one removes it (`remove_leftovers`).
        Ok(())
    }

    pub fn remove_leftovers(target: &Path) {
        for what in ["incoming", "outgoing"] {
            let _ = fs::remove_dir_all(sibling(target, what));
        }
    }

    /// `.<name>.<what>` next to `target`: hidden, and not an app bundle to Launch Services.
    fn sibling(target: &Path, what: &str) -> PathBuf {
        target.with_file_name(format!(".{}.{what}", file_name(target)))
    }

    pub fn wait_for_exit(pid: u32, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while is_running(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;

    pub fn installed(exe: PathBuf) -> Outcome<PathBuf> {
        Ok(exe)
    }

    pub fn extract(archive: &Path, dir: &Path) -> anyhow::Result<()> {
        // The tar that comes with Windows reads zip archives; the one on PATH may be another.
        let windows = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let tar = Path::new(&windows).join("System32").join("tar.exe");
        run(Command::new(tar).arg("-xf").arg(archive).arg("-C").arg(dir))
    }

    pub fn find(dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
        let exe = find_entry(dir, |p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")))?;
        Ok((exe.clone(), exe))
    }

    pub fn check_signature(_program: &Path, _target: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn swap(staged: &Path, target: &Path) -> Outcome<()> {
        let name = file_name(target);
        let incoming = target.with_file_name(format!("{name}.incoming"));
        let _ = fs::remove_file(&incoming);
        fs::copy(staged, &incoming).because(Problem::ReadOnly)?;
        // Windows cannot overwrite a running program, but it can rename one.
        let mut outgoing = target.with_file_name(format!("{name}.old"));
        if fs::remove_file(&outgoing).is_err() && outgoing.exists() {
            outgoing = target.with_file_name(format!("{name}.old-{}", std::process::id()));
        }
        if let Err(e) = fs::rename(target, &outgoing) {
            let _ = fs::remove_file(&incoming);
            return Err(Failure::new(Problem::ReadOnly, e));
        }
        if let Err(e) = fs::rename(&incoming, target) {
            let _ = fs::rename(&outgoing, target);
            let _ = fs::remove_file(&incoming);
            return Err(Failure::new(Problem::ReadOnly, e));
        }
        Ok(())
    }

    pub fn remove_leftovers(target: &Path) {
        let name = file_name(target);
        let Some(dir) = target.parent() else { return };
        let Ok(entries) = fs::read_dir(dir) else { return };
        for path in entries.filter_map(|e| e.ok().map(|e| e.path())) {
            let entry = file_name(&path);
            if entry == format!("{name}.incoming") || entry.starts_with(&format!("{name}.old")) {
                let _ = fs::remove_file(&path);
            }
        }
    }

    pub fn wait_for_exit(pid: u32, timeout: Duration) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
        unsafe {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if handle.is_null() {
                return;
            }
            WaitForSingleObject(handle, timeout.as_millis() as u32);
            CloseHandle(handle);
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    pub fn installed(exe: PathBuf) -> Outcome<PathBuf> {
        Ok(exe)
    }

    pub fn extract(archive: &Path, dir: &Path) -> anyhow::Result<()> {
        run(Command::new("tar").arg("-xzf").arg(archive).arg("-C").arg(dir))
    }

    /// The archive holds `cc-same-<version>/` with `bin/cc-same-app` and `bin/cc-same`.
    pub fn find(dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
        let root = find_entry(dir, |p| p.join("bin").join("cc-same-app").is_file())?;
        let program = root.join("bin").join("cc-same-app");
        Ok((root, program))
    }

    pub fn check_signature(_root: &Path, _target: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn swap(root: &Path, target: &Path) -> Outcome<()> {
        replace(&root.join("bin").join("cc-same-app"), target).because(Problem::ReadOnly)?;
        // The command-line tool comes in the same archive; keep it in step when it sits alongside.
        let cli = target.with_file_name("cc-same");
        if cli.is_file() {
            let _ = replace(&root.join("bin").join("cc-same"), &cli);
        }
        Ok(())
    }

    /// Put `new` in place of `old`. A program that is running keeps the file it started from.
    fn replace(new: &Path, old: &Path) -> std::io::Result<()> {
        let incoming = incoming(old);
        let _ = fs::remove_file(&incoming);
        fs::copy(new, &incoming)?;
        fs::set_permissions(&incoming, fs::Permissions::from_mode(0o755))?;
        fs::rename(&incoming, old).inspect_err(|_| {
            let _ = fs::remove_file(&incoming);
        })
    }

    fn incoming(path: &Path) -> PathBuf {
        path.with_file_name(format!(".{}.incoming", file_name(path)))
    }

    pub fn remove_leftovers(target: &Path) {
        let _ = fs::remove_file(incoming(target));
        let _ = fs::remove_file(incoming(&target.with_file_name("cc-same")));
    }

    pub fn wait_for_exit(pid: u32, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while is_running(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
mod platform {
    use super::*;

    pub fn installed(exe: PathBuf) -> Outcome<PathBuf> {
        Err(Failure::new(Problem::NoBuild, anyhow!("no updates for {}", exe.display())))
    }

    pub fn extract(_archive: &Path, _dir: &Path) -> anyhow::Result<()> {
        bail!("no updates on this system")
    }

    pub fn find(_dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
        bail!("no updates on this system")
    }

    pub fn check_signature(_staged: &Path, _target: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn swap(_staged: &Path, _target: &Path) -> Outcome<()> {
        Err(Failure::new(Problem::NoBuild, anyhow!("no updates on this system")))
    }

    pub fn remove_leftovers(_target: &Path) {}

    pub fn wait_for_exit(_pid: u32, _timeout: Duration) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_json(tag: &str) -> Value {
        let name = archive_name(&Version::parse(tag.trim_start_matches('v')).unwrap()).unwrap_or_default();
        serde_json::json!({
            "tag_name": tag,
            "html_url": format!("https://github.com/Arcaneology/cc-same/releases/tag/{tag}"),
            "body": "## What's new\n\n- Updates\n",
            "assets": [
                { "name": "cc-same-cli-9.9.9-linux-x64.tar.gz", "size": 1, "browser_download_url": "https://x/cli",
                  "digest": "sha256:00" },
                { "name": name, "size": 1234, "browser_download_url": "https://x/app",
                  "digest": "sha256:ABCDEF" },
            ],
        })
    }

    #[test]
    fn reads_a_release_and_picks_this_systems_archive() {
        let release = parse(&release_json("v9.9.9")).unwrap();
        assert_eq!(release.version, Version::new(9, 9, 9));
        assert_eq!(release.notes, "## What's new\n\n- Updates");
        assert!(release.page.ends_with("/tag/v9.9.9"));
        if archive_name(&release.version).is_some() {
            let asset = release.asset.unwrap();
            assert!(asset.name.starts_with("CC-Same-9.9.9-"), "{}", asset.name);
            assert_eq!((asset.url.as_str(), asset.size), ("https://x/app", 1234));
            assert_eq!(asset.sha256.as_deref(), Some("abcdef"));
        }
    }

    #[test]
    fn a_release_without_a_version_tag_is_refused() {
        assert!(parse(&serde_json::json!({ "tag_name": "nightly" })).is_err());
        assert!(parse(&serde_json::json!({})).is_err());
    }

    #[test]
    fn archive_names_match_the_release_workflow() {
        let name = archive_name(&Version::new(0, 2, 0));
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            assert_eq!(name.as_deref(), Some("CC-Same-0.2.0-macos-arm64.zip"));
        }
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(name.as_deref(), Some("CC-Same-0.2.0-linux-x64.tar.gz"));
        }
        if cfg!(all(windows, target_arch = "x86_64")) {
            assert_eq!(name.as_deref(), Some("CC-Same-0.2.0-windows-x64.zip"));
        }
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(Version::parse("0.1.10").unwrap() > Version::parse("0.1.9").unwrap());
        assert!(Version::parse("1.0.0").unwrap() > Version::parse("1.0.0-rc.1").unwrap());
    }

    #[test]
    fn the_changelog_covers_this_version() {
        let all = changes_since(None);
        assert!(all.starts_with(&format!("## {VERSION} ")), "CHANGELOG.md needs a section for {VERSION}");
        assert_eq!(changes_since(Some(&current())), "");
        let since_first = changes_since(Some(&Version::new(0, 1, 0)));
        if current() > Version::new(0, 1, 0) {
            assert!(since_first.starts_with(&format!("## {VERSION} ")));
            assert!(!since_first.contains("## 0.1.0 "));
        }
    }

    /// Answers every request on 127.0.0.1 with `body`; returns its URL.
    fn serve(body: Vec<u8>) -> String {
        use std::io::BufRead as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/archive", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = std::io::BufReader::new(&stream);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let mut stream = &stream;
                let _ = stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(&body));
            }
        });
        url
    }

    #[test]
    fn downloads_are_checked_for_size_and_digest() {
        let body = b"the new version".to_vec();
        let digest = format!("{:x}", Sha256::digest(&body));
        let dir = tempfile::tempdir().unwrap();
        let fetch = |sha256: &str, size: u64| {
            let asset =
                Asset { name: "archive.zip".into(), url: serve(body.clone()), size, sha256: Some(sha256.into()) };
            let progress = AtomicU64::new(0);
            download(&asset, &dir.path().join("updates"), &progress).map(|path| (path, progress.into_inner()))
        };
        let (path, progress) = fetch(&digest, body.len() as u64).unwrap();
        assert_eq!((fs::read(path).unwrap(), progress), (body.clone(), body.len() as u64));
        let problem = |result: Outcome<(PathBuf, u64)>| result.unwrap_err().problem;
        assert_eq!(problem(fetch(&"0".repeat(64), body.len() as u64)), Problem::Unverified);
        assert_eq!(problem(fetch(&digest, body.len() as u64 - 1)), Problem::Unverified);
        assert_eq!(problem(fetch(&digest, body.len() as u64 + 1)), Problem::Offline);
    }

    /// Tests that write a program and run it take turns: a process started by another test at
    /// the wrong moment could still hold the file open for writing ("text file busy").
    #[cfg(unix)]
    static SCRIPTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A program that says it is CC Same `version`.
    #[cfg(unix)]
    fn fake_program(path: &Path, version: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, format!("#!/bin/sh\necho \"CC Same {version}\"\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_new_copy_has_to_start_and_say_its_version() {
        let _turn = SCRIPTS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("cc-same-app");
        fake_program(&program, "9.9.9");
        check_version(&program, &Version::new(9, 9, 9)).unwrap();
        assert!(check_version(&program, &Version::new(9, 9, 8)).is_err());
        assert!(check_version(&dir.path().join("missing"), &Version::new(9, 9, 9)).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_linux_archive_is_unpacked_checked_and_swapped_in() {
        let _turn = SCRIPTS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fake_program(&source.join("cc-same-9.9.9/bin/cc-same-app"), "9.9.9");
        fs::write(source.join("cc-same-9.9.9/bin/cc-same"), "new command line").unwrap();
        let archive = dir.path().join("updates/CC-Same-9.9.9-linux-x64.tar.gz");
        fs::create_dir_all(archive.parent().unwrap()).unwrap();
        run(Command::new("tar").arg("-C").arg(&source).arg("-czf").arg(&archive).arg("cc-same-9.9.9")).unwrap();
        let target = dir.path().join("prefix/bin/cc-same-app");
        fake_program(&target, "0.0.1");
        fs::write(target.with_file_name("cc-same"), "old command line").unwrap();

        let staged = unpack(&archive, &Version::new(9, 9, 9), &target).unwrap();
        platform::swap(&staged, &target).unwrap();
        check_version(&target, &Version::new(9, 9, 9)).unwrap();
        assert_eq!(fs::read_to_string(target.with_file_name("cc-same")).unwrap(), "new command line");
        platform::remove_leftovers(&target);
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 2);
        assert!(unpack(&archive, &Version::new(9, 9, 8), &target).is_err_and(|f| f.problem == Problem::Unverified));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_mac_bundle_is_swapped_in_and_the_old_one_cleared_away_later() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Applications/CC Same.app");
        let staged = dir.path().join("updates/unpacked/CC Same.app");
        for (bundle, what) in [(&target, "old"), (&staged, "new")] {
            fs::create_dir_all(bundle.join("Contents")).unwrap();
            fs::write(bundle.join("Contents/what"), what).unwrap();
        }
        platform::swap(&staged, &target).unwrap();
        assert_eq!(fs::read_to_string(target.join("Contents/what")).unwrap(), "new");
        let outgoing = dir.path().join("Applications/.CC Same.app.outgoing");
        assert_eq!(fs::read_to_string(outgoing.join("Contents/what")).unwrap(), "old");
        platform::remove_leftovers(&target);
        assert!(!outgoing.exists() && !staged.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_mac_update_without_a_trusted_signature_is_refused() {
        let _turn = SCRIPTS.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let bundle = |root: &Path, version: &str| {
            let app = root.join("CC Same.app");
            fake_program(&app.join("Contents/MacOS/CC Same"), version);
            run(Command::new("/usr/bin/codesign").args(["--force", "--sign", "-"]).arg(&app)).unwrap();
            app
        };
        let target = bundle(&dir.path().join("Applications"), "0.0.1");
        let new = bundle(&dir.path().join("build"), "9.9.9");
        let archive = dir.path().join("updates/CC-Same-9.9.9-macos-arm64.zip");
        fs::create_dir_all(archive.parent().unwrap()).unwrap();
        run(Command::new("/usr/bin/ditto").args(["-c", "-k", "--keepParent"]).arg(&new).arg(&archive)).unwrap();
        // Neither copy is signed by a team, and Gatekeeper does not accept ad-hoc code.
        let refused = unpack(&archive, &Version::new(9, 9, 9), &target).unwrap_err();
        assert_eq!(refused.problem, Problem::Unverified, "{:#}", refused.error);
    }

    #[cfg(windows)]
    #[test]
    fn a_running_program_is_swapped_in_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        let system = Path::new(&std::env::var_os("SystemRoot").unwrap()).join("System32");
        let target = dir.path().join("CC Same.exe");
        let staged = dir.path().join("unpacked.exe");
        fs::copy(system.join("cmd.exe"), &target).unwrap();
        fs::copy(system.join("hostname.exe"), &staged).unwrap();
        // The copy of cmd.exe keeps running while it is replaced.
        let mut running =
            Command::new(&target).args(["/c", "ping -n 6 127.0.0.1 >nul"]).stdout(Stdio::null()).spawn().unwrap();
        platform::swap(&staged, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), fs::read(&staged).unwrap());
        assert!(dir.path().join("CC Same.exe.old").exists());
        let _ = running.kill();
        let _ = running.wait();
        platform::remove_leftovers(&target);
        assert!(!dir.path().join("CC Same.exe.old").exists());
    }

    /// Asks the real GitHub (`cargo test -- --ignored`).
    #[test]
    #[ignore = "uses the network"]
    fn the_latest_release_on_github_reads_back() {
        let body = client(Duration::from_secs(30)).unwrap().get(LATEST).send().unwrap().bytes().unwrap();
        let release = parse(&serde_json::from_slice(&body).unwrap()).unwrap();
        let asset = release.asset.expect("an archive for this system");
        assert!(asset.sha256.is_some_and(|d| d.len() == 64), "GitHub lists a SHA-256 digest");
        assert!(check().is_ok());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_disk_image_or_a_translocated_app_is_a_temporary_place() {
        let translocated = "/private/var/folders/x/T/AppTranslocation/0A1B/d/CC Same.app/Contents/MacOS/CC Same";
        assert!(temporary_place(Path::new(translocated)));
        let dir = tempfile::tempdir().unwrap();
        assert!(!temporary_place(dir.path()));
    }

    #[test]
    fn after_an_update_the_previous_version_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("user"), dir.path().join("state"));
        // A first start: nothing to report.
        assert_eq!(updated_from(&paths), None);
        assert_eq!(updated_from(&paths), None);
        // An older version ran last.
        fs::write(paths.state_dir.join("app.json"), r#"{"lastVersion":"0.0.1"}"#).unwrap();
        assert_eq!(updated_from(&paths), Some(Version::new(0, 0, 1)));
        assert_eq!(updated_from(&paths), None);
    }
}
