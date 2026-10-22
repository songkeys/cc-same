//! Windows Desktop lifecycle. Profile selection and launch selection are one decision.
//! Discovery reads installation metadata only; it never opens authentication data.

use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::fs;
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod uia;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LaunchTarget {
    Store(String),
    Conventional(PathBuf),
}

#[derive(Deserialize)]
struct StartApp {
    #[serde(rename = "AppID")]
    app_id: String,
}

fn key(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_ascii_lowercase()
}

fn family(value: &str) -> bool {
    value.to_ascii_lowercase().starts_with("claude_")
        && value.len() > "Claude_".len()
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn app_family(app_id: &str) -> Option<&str> {
    let (f, app) = app_id.split_once('!')?;
    (family(f) && !app.is_empty() && app.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))).then_some(f)
}

/// Reject arbitrary overrides, traversal and ambiguous AppIDs before touching sign-in files.
fn select_launch(
    data: &Path,
    roaming: &Path,
    local: &Path,
    apps: &[String],
    conventional: Option<PathBuf>,
) -> Result<LaunchTarget> {
    anyhow::ensure!(
        !data.components().any(|c| matches!(c, std::path::Component::ParentDir)),
        "Claude data override cannot contain parent traversal"
    );
    let data_key = key(data);
    if data_key == key(&roaming.join("Claude")) {
        return conventional
            .map(LaunchTarget::Conventional)
            .context("the selected conventional Claude profile has no conventional Desktop installation");
    }
    let modern_prefix = format!("{}\\", key(&local.join("Claude-Data")));
    let legacy_prefix = format!("{}\\", key(&local.join("Packages")));
    let selected_family = data_key.strip_prefix(&modern_prefix).filter(|s| !s.contains('\\')).or_else(|| {
        data_key.strip_prefix(&legacy_prefix).and_then(|p| {
            let (first, rest) = p.split_once('\\')?;
            (rest == "localcache\\roaming\\claude").then_some(first)
        })
    });
    let selected_family = selected_family.filter(|s| family(s))
        .context("cannot bind the selected Claude data folder to an installed Desktop; use its conventional or Store data folder")?;
    let mut matching = apps
        .iter()
        .filter(|a| app_family(a).is_some_and(|f| f.eq_ignore_ascii_case(selected_family)))
        .collect::<Vec<_>>();
    matching.sort();
    matching.dedup();
    anyhow::ensure!(matching.len() == 1,
        "selected Store profile needs exactly one registered Claude app in the same package family; installation missing or ambiguous");
    Ok(LaunchTarget::Store(matching[0].clone()))
}

fn version(name: &str) -> Option<Vec<u64>> {
    let suffix = name.strip_prefix("app-")?;
    let parts = suffix.split('.').map(str::parse).collect::<std::result::Result<Vec<u64>, _>>().ok()?;
    (parts.len() >= 2).then_some(parts)
}

fn conventional_install(local: &Path) -> Option<PathBuf> {
    let mut installs = fs::read_dir(local.join("AnthropicClaude"))
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let ver = version(&entry.file_name().to_string_lossy())?;
            let exe = entry.path().join("Claude.exe");
            exe.is_file().then_some((ver, exe))
        })
        .collect::<Vec<_>>();
    installs.sort_by(|a, b| a.0.cmp(&b.0));
    installs.pop().map(|(_, exe)| exe)
}

pub(super) fn resolve_launch(data: &Path) -> Result<LaunchTarget> {
    let roaming = dirs::config_dir().context("Windows roaming application-data directory unavailable")?;
    let local = dirs::data_local_dir().context("Windows local application-data directory unavailable")?;
    // A conventional profile never needs Store enumeration (including machines without PowerShell).
    if key(data) == key(&roaming.join("Claude")) {
        return select_launch(data, &roaming, &local, &[], conventional_install(&local));
    }
    let output = Command::new("powershell.exe")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command",
            "@(Get-StartApps | Where-Object { $_.AppID -like 'Claude_*!*' } | Select-Object AppID) | ConvertTo-Json -Compress"])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .stdin(Stdio::null()).output().context("discovering registered Claude Desktop installations")?;
    anyhow::ensure!(output.status.success(), "could not read registered Claude Desktop installations");
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim_start_matches('\u{feff}').trim();
    let apps = if text.is_empty() {
        Vec::new()
    } else if text.starts_with('[') {
        serde_json::from_str::<Vec<StartApp>>(text)?
    } else {
        vec![serde_json::from_str::<StartApp>(text)?]
    };
    select_launch(
        data,
        &roaming,
        &local,
        &apps.into_iter().map(|a| a.app_id).collect::<Vec<_>>(),
        conventional_install(&local),
    )
}

impl LaunchTarget {
    pub(super) fn launch(&self) -> Result<()> {
        match self {
            Self::Conventional(exe) => {
                Command::new(exe)
                    .current_dir(exe.parent().context("Claude installation has no directory")?)
                    .stdin(Stdio::null())
                    .spawn()
                    .context("starting the selected conventional Claude installation")?;
                Ok(())
            }
            Self::Store(app_id) => activate_store(app_id),
        }
    }
}

/// IApplicationActivationManager returns the activation HRESULT, unlike launching explorer.exe,
/// whose successful spawn says nothing about whether the registered app activated.
fn activate_store(app_id: &str) -> Result<()> {
    use std::ffi::c_void;
    use windows_sys::core::GUID;
    use windows_sys::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED,
    };
    #[repr(C)]
    struct ActivationVtable {
        query: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        activate: unsafe extern "system" fn(*mut c_void, *const u16, *const u16, u32, *mut u32) -> i32,
    }
    const CLASS: GUID = GUID::from_u128(0x45ba127d_10a8_46ea_8ab7_56ea9078943c);
    const INTERFACE: GUID = GUID::from_u128(0x2e941141_7f97_4756_ba1d_9decde894a3d);
    let id = app_id.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    unsafe {
        let initialized = CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
        // RPC_E_CHANGED_MODE: the caller already owns a usable COM apartment.
        anyhow::ensure!(
            initialized >= 0 || initialized == 0x80010106u32 as i32,
            "initializing Windows app activation failed ({initialized:#x})"
        );
        let mut object = std::ptr::null_mut();
        let created = CoCreateInstance(&CLASS, std::ptr::null_mut(), CLSCTX_LOCAL_SERVER, &INTERFACE, &mut object);
        if created < 0 {
            if initialized >= 0 {
                CoUninitialize();
            }
            anyhow::bail!("creating Windows app activation manager failed ({created:#x})");
        }
        let vtable = &**(object as *const *const ActivationVtable);
        let mut pid = 0;
        let result = (vtable.activate)(object, id.as_ptr(), std::ptr::null(), 0, &mut pid);
        (vtable.release)(object);
        if initialized >= 0 {
            CoUninitialize();
        }
        anyhow::ensure!(result >= 0, "activating the selected Claude Store installation failed ({result:#x})");
    }
    Ok(())
}

/// Trust installation roots, never a bare executable name or an arbitrary `app-*` folder.
fn desktop_path(path: &Path, local: &Path, program_files: &Path) -> bool {
    let text = key(path);
    if !text.ends_with(".exe") || text.contains("\\claude-code\\") || text.contains("\\cli\\") {
        return false;
    }
    let conventional = format!("{}\\", key(&local.join("AnthropicClaude")));
    if let Some(rest) = text.strip_prefix(&conventional) {
        return rest.split_once('\\').is_some_and(|(dir, _)| version(dir).is_some());
    }
    let store = format!("{}\\", key(&program_files.join("WindowsApps")));
    text.strip_prefix(&store)
        .and_then(|rest| rest.split_once('\\'))
        .is_some_and(|(package, rest)| family(package) && rest.starts_with("app\\"))
}

struct Process {
    pid: u32,
    path: PathBuf,
}

fn processes() -> Option<Vec<Process>> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let local = dirs::data_local_dir()?;
    let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from)?;
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut result = Vec::new();
        let mut uncertain = false;
        let mut ok = Process32FirstW(snapshot, &mut entry) != 0;
        if !ok {
            CloseHandle(snapshot);
            return None;
        }
        while ok {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID);
            let mut identified = false;
            if !handle.is_null() {
                let mut buffer = [0u16; 32768];
                let mut size = buffer.len() as u32;
                if QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) != 0 {
                    identified = true;
                    let path = PathBuf::from(String::from_utf16_lossy(&buffer[..size as usize]));
                    let updater = key(&path) == key(&local.join("AnthropicClaude/Update.exe"));
                    if desktop_path(&path, &local, &program_files) || updater {
                        result.push(Process { pid: entry.th32ProcessID, path });
                    }
                }
                CloseHandle(handle);
            }
            let len = entry.szExeFile.iter().position(|c| *c == 0).unwrap_or(entry.szExeFile.len());
            // An unreadable Claude process must never be treated as safely exited.
            if !identified && String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case("Claude.exe") {
                uncertain = true;
            }
            ok = Process32NextW(snapshot, &mut entry) != 0;
        }
        let complete = GetLastError() == ERROR_NO_MORE_FILES;
        CloseHandle(snapshot);
        (!uncertain && complete).then_some(result)
    }
}

fn updater(process: &Process) -> bool {
    process.path.file_name().is_some_and(|n| n.eq_ignore_ascii_case("Update.exe"))
}

pub(super) fn desktop_running() -> Option<bool> {
    processes().map(|p| p.iter().any(|p| !updater(p)))
}

pub(super) fn updating() -> bool {
    processes().is_none_or(|p| p.iter().any(updater))
}

fn is_quit_label(text: &str) -> bool {
    let label = text.split('\t').next().unwrap_or("").replace('&', "").trim().to_lowercase();
    matches!(label.as_str(), "quit" | "exit" | "quit claude" | "退出" | "退出 claude")
}

/// Request a native Quit menu command or Electron's accessible application menu. A tray-only close
/// is deliberately not promoted to success: the caller waits for every Desktop helper to exit.
pub(super) fn ask_to_quit(launcher: &LaunchTarget, log: impl Fn(String)) -> Result<()> {
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    unsafe fn quit_item(menu: HMENU) -> Option<u32> {
        for i in 0..GetMenuItemCount(menu) {
            let submenu = GetSubMenu(menu, i);
            if !submenu.is_null() {
                if let Some(id) = quit_item(submenu) {
                    return Some(id);
                }
            }
            let mut buffer = [0u16; 256];
            let len = GetMenuStringW(menu, i as u32, buffer.as_mut_ptr(), buffer.len() as i32, MF_BYPOSITION);
            let text = String::from_utf16_lossy(&buffer[..len.max(0) as usize]);
            if is_quit_label(&text) {
                let id = GetMenuItemID(menu, i);
                if id != u32::MAX {
                    return Some(id);
                }
            }
        }
        None
    }
    let mut pids = trusted_pids().context("cannot inspect Claude processes safely before requesting Quit")?;
    let mut windows = visible_windows(&pids);
    if !pids.is_empty() && windows.is_empty() {
        // Surface this exact installation when Claude is already hidden in its tray.
        launcher.launch()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while windows.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
            pids = trusted_pids().context("cannot inspect Claude processes after surfacing its window")?;
            windows = visible_windows(&pids);
        }
    }
    if pids.is_empty() {
        return Ok(());
    }
    for &window in &windows {
        unsafe {
            let menu = GetMenu(window);
            if let Some(command) = (!menu.is_null()).then(|| quit_item(menu)).flatten() {
                if PostMessageW(window, WM_COMMAND, command as usize, 0) != 0 {
                    return Ok(());
                }
            }
        }
    }
    match uia::request_quit(pids) {
        Ok(()) => return Ok(()),
        Err(error) => log(format!(
            "Claude's accessible Quit request failed; using native close and waiting for process exit: {error:#}"
        )),
    }
    // Unsupported accessibility providers retain the previous safe native-close fallback.
    // The process-exit wait, not this request, decides whether changing sign-ins is safe.
    for window in visible_windows(&trusted_pids().unwrap_or_default()) {
        unsafe {
            PostMessageW(window, WM_CLOSE, 0, 0);
        }
    }
    Ok(())
}

fn trusted_pids() -> Option<Vec<u32>> {
    processes().map(|p| p.into_iter().filter(|p| !updater(p)).map(|p| p.pid).collect())
}

fn visible_windows(pids: &[u32]) -> Vec<windows_sys::Win32::Foundation::HWND> {
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    struct Search<'a> {
        pids: &'a [u32],
        windows: Vec<HWND>,
    }
    unsafe extern "system" fn collect(window: HWND, parameter: LPARAM) -> BOOL {
        let search = &mut *(parameter as *mut Search<'_>);
        let mut pid = 0;
        GetWindowThreadProcessId(window, &mut pid);
        if search.pids.contains(&pid) && IsWindowVisible(window) != 0 && GetWindow(window, GW_OWNER).is_null() {
            search.windows.push(window);
        }
        1
    }
    let mut search = Search { pids, windows: Vec::new() };
    unsafe {
        EnumWindows(Some(collect), &mut search as *mut Search<'_> as isize);
    }
    search.windows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_quit_labels_cannot_select_sign_out() {
        for label in ["&Quit\tCtrl+Q", "E&xit", "退出", "Quit Claude"] {
            assert!(is_quit_label(label));
        }
        for label in ["退出登录", "退出账户", "Sign out", "Log out", "Quit and sign out"] {
            assert!(!is_quit_label(label));
        }
    }

    #[test]
    fn process_paths_trust_only_desktop_installations_and_helpers() {
        let local = Path::new(r"C:\Users\test\AppData\Local");
        let pf = Path::new(r"C:\Program Files");
        for path in [
            r"C:\Users\test\AppData\Local\AnthropicClaude\app-1.2.3\Claude.exe",
            r"C:\Users\test\AppData\Local\AnthropicClaude\app-1.2.3\resources\helper.exe",
            r"C:\Program Files\WindowsApps\Claude_1.2.3_x64__publisher\app\Claude.exe",
        ] {
            assert!(desktop_path(Path::new(path), local, pf), "{path}");
        }
        for path in [
            r"C:\Tools\app-1.2.3\claude.exe",
            r"C:\Users\test\.local\bin\claude.exe",
            r"C:\Program Files\WindowsApps\Unrelated_1.2\app\Claude.exe",
            r"C:\Users\test\AppData\Local\AnthropicClaudeFake\app-1.2\Claude.exe",
            r"C:\Users\test\AppData\Local\AnthropicClaude\app-1.2\resources\claude-code\claude.exe",
        ] {
            assert!(!desktop_path(Path::new(path), local, pf), "{path}");
        }
    }

    #[test]
    fn mixed_installations_launch_the_selected_profiles_family() {
        let local = Path::new(r"C:\Local");
        let roaming = Path::new(r"C:\Roaming");
        let exe = PathBuf::from(r"C:\Local\AnthropicClaude\app-1.2\Claude.exe");
        let apps = vec!["Claude_familyA!Claude".into(), "Claude_familyB!Claude".into()];
        for data in [
            r"C:\Local\Claude-Data\Claude_familyA",
            r"C:\Local\Packages\Claude_familyA\LocalCache\Roaming\Claude",
            r"c:\LOCAL\claude-data\claude_familyA",
        ] {
            assert_eq!(
                select_launch(Path::new(data), roaming, local, &apps, Some(exe.clone())).unwrap(),
                LaunchTarget::Store(apps[0].clone())
            );
        }
        assert_eq!(
            select_launch(&roaming.join("Claude"), roaming, local, &apps, Some(exe.clone())).unwrap(),
            LaunchTarget::Conventional(exe)
        );
        assert!(select_launch(&roaming.join("Claude"), roaming, local, &apps, None).is_err());
    }

    #[test]
    fn missing_ambiguous_and_arbitrary_launch_bindings_are_refused() {
        let local = Path::new(r"C:\Local");
        let roaming = Path::new(r"C:\Roaming");
        let data = local.join("Claude-Data/Claude_familyA");
        for apps in [
            vec![],
            vec!["Claude_other!Claude".into()],
            vec!["Claude_familyA!Claude".into(), "Claude_familyA!Other".into()],
        ] {
            assert!(select_launch(&data, roaming, local, &apps, None).is_err());
        }
        assert!(select_launch(
            Path::new(r"C:\arbitrary\Claude"),
            roaming,
            local,
            &["Claude_familyA!Claude".into()],
            None
        )
        .is_err());
        assert!(app_family("Claude_familyA!Claude!Injected").is_none());
        assert!(app_family("Other_family!Claude").is_none());
    }

    #[test]
    fn conventional_install_uses_numeric_versions_and_requires_desktop_executable() {
        let temp = tempfile::tempdir().unwrap();
        for version in ["app-1.9.0", "app-1.10.0", "app-2.0.0", "app-malformed"] {
            let dir = temp.path().join("AnthropicClaude").join(version);
            fs::create_dir_all(&dir).unwrap();
            if version != "app-2.0.0" {
                fs::write(dir.join("Claude.exe"), "synthetic executable").unwrap();
            }
        }
        assert_eq!(conventional_install(temp.path()), Some(temp.path().join("AnthropicClaude/app-1.10.0/Claude.exe")));
    }
}
