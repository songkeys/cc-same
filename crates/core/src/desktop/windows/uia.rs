//! Exact, process-scoped UI Automation for Electron's custom application menu.
//! No coordinates, keystrokes, global focus changes or sign-out actions are used.

use anyhow::{Context as _, Result};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use windows::Win32::{
    System::{Com::*, Variant::VARIANT},
    UI::Accessibility::*,
};

const LIMIT: Duration = Duration::from_secs(8);

#[derive(Clone, Copy)]
enum Action {
    Menu,
    File,
    Quit,
}

impl Action {
    fn labels(self) -> &'static [&'static str] {
        match self {
            Self::Menu => &["Menu", "Application menu", "菜单", "選單"],
            Self::File => &["File", "&File", "文件", "檔案"],
            Self::Quit => &["Quit", "&Quit", "Exit", "E&xit", "Quit Claude", "退出", "退出 Claude"],
        }
    }

    fn control(self) -> UIA_CONTROLTYPE_ID {
        match self {
            Self::Menu => UIA_ButtonControlTypeId,
            _ => UIA_MenuItemControlTypeId,
        }
    }
}

fn permits(action: Action, pid: u32, control: UIA_CONTROLTYPE_ID, label: &str, trusted: &[u32]) -> bool {
    if !trusted.contains(&pid) || control != action.control() {
        return false;
    }
    match action {
        Action::Quit => super::is_quit_label(label),
        _ => action.labels().iter().any(|s| s.eq_ignore_ascii_case(label)),
    }
}

/// Run UIA in its own MTA. Provider calls and the overall request are bounded; cancellation is
/// checked before every invocation so a late query cannot act after the caller gives up.
pub(super) fn request_quit(pids: Vec<u32>) -> Result<()> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new().name("claude-graceful-quit".into()).spawn(move || {
        let result = automate(&pids, &worker_cancelled);
        let _ = send.send(result);
    })?;
    let result = receive.recv_timeout(LIMIT + Duration::from_secs(2));
    cancelled.store(true, Ordering::Relaxed);
    result.context("Claude's accessible Quit menu did not respond in time")?
}

fn automate(pids: &[u32], cancelled: &AtomicBool) -> Result<()> {
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe {
                CoUninitialize();
            }
        }
    }
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let _apartment = Apartment;
        let automation: IUIAutomation2 = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
        automation.SetAutoSetFocus(false)?;
        automation.SetConnectionTimeout(500)?;
        automation.SetTransactionTimeout(750)?;
        let deadline = Instant::now() + LIMIT;
        if find(&automation, Action::File, pids)?.is_none() {
            let menu = wait_for(&automation, Action::Menu, pids, deadline, cancelled)
                .context("finding Claude's application Menu button")?;
            perform(&menu, Action::Menu, pids, cancelled).context("opening Claude's application Menu")?;
        }
        let file =
            wait_for(&automation, Action::File, pids, deadline, cancelled).context("finding Claude's File menu")?;
        perform(&file, Action::File, pids, cancelled).context("opening Claude's File menu")?;
        let quit = wait_for(&automation, Action::Quit, pids, deadline, cancelled)
            .context("finding Claude's exact Quit menu item")?;
        perform(&quit, Action::Quit, pids, cancelled).context("invoking Claude's exact Quit menu item")
    }
}

unsafe fn find(automation: &IUIAutomation2, action: Action, pids: &[u32]) -> Result<Option<IUIAutomationElement>> {
    anyhow::ensure!(!pids.is_empty(), "no trusted Claude Desktop process");
    let process_conditions = pids
        .iter()
        .map(|pid| automation.CreatePropertyCondition(UIA_ProcessIdPropertyId, &VARIANT::from(*pid as i32)).map(Some))
        .collect::<windows::core::Result<Vec<_>>>()?;
    let processes = if process_conditions.len() == 1 {
        process_conditions[0].as_ref().expect("one process condition").clone()
    } else {
        automation.CreateOrConditionFromNativeArray(&process_conditions)?
    };
    let labels = action
        .labels()
        .iter()
        .map(|label| {
            automation
                .CreatePropertyConditionEx(
                    UIA_NamePropertyId,
                    &VARIANT::from(*label),
                    PropertyConditionFlags_IgnoreCase,
                )
                .map(Some)
        })
        .collect::<windows::core::Result<Vec<_>>>()?;
    let names = automation.CreateOrConditionFromNativeArray(&labels)?;
    let control = automation.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(action.control().0))?;
    let action_condition = automation.CreateAndCondition(&names, &control)?;
    let condition = automation.CreateAndCondition(&processes, &action_condition)?;
    let root = automation.GetRootElement()?;
    // Search only top-level regions belonging to known Desktop executables. This includes the
    // separate native popup panes Electron creates, and excludes other applications' menus.
    let regions = root.FindAll(TreeScope_Children, &processes)?;
    let mut found = None;
    for index in 0..regions.Length()? {
        let region = regions.GetElement(index)?;
        let candidates = region.FindAll(TreeScope_Subtree, &condition)?;
        for index in 0..candidates.Length()? {
            let element = candidates.GetElement(index)?;
            if element.CurrentIsOffscreen()?.as_bool() {
                continue;
            }
            let pid = element.CurrentProcessId()? as u32;
            let control = element.CurrentControlType()?;
            let label = element.CurrentName()?.to_string();
            if permits(action, pid, control, &label, pids) {
                anyhow::ensure!(found.is_none(), "Claude's accessible menu target is ambiguous");
                found = Some(element);
            }
        }
    }
    Ok(found)
}

unsafe fn wait_for(
    automation: &IUIAutomation2,
    action: Action,
    pids: &[u32],
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<IUIAutomationElement> {
    while Instant::now() < deadline && !cancelled.load(Ordering::Relaxed) {
        if let Some(element) = find(automation, action, pids)? {
            return Ok(element);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("Claude's accessible application menu is unavailable")
}

unsafe fn perform(element: &IUIAutomationElement, action: Action, pids: &[u32], cancelled: &AtomicBool) -> Result<()> {
    anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "accessible Quit request cancelled");
    let current = super::trusted_pids().context("cannot revalidate Claude process identity")?;
    let pid = element.CurrentProcessId()? as u32;
    anyhow::ensure!(
        pids.contains(&pid)
            && permits(action, pid, element.CurrentControlType()?, &element.CurrentName()?.to_string(), &current),
        "accessible menu element does not belong to the trusted Claude process and exact menu action"
    );
    if matches!(action, Action::File) {
        if let Ok(pattern) =
            element.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId)
        {
            anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "accessible Quit request cancelled");
            if pattern.Expand().is_ok() {
                return Ok(());
            }
        }
    }
    let pattern = element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)?;
    anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "accessible Quit request cancelled");
    pattern.Invoke()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessible_menu_actions_require_exact_labels_control_types_and_trusted_processes() {
        assert!(permits(Action::Menu, 42, UIA_ButtonControlTypeId, "菜单", &[42]));
        assert!(permits(Action::File, 42, UIA_MenuItemControlTypeId, "文件", &[42]));
        assert!(permits(Action::Quit, 42, UIA_MenuItemControlTypeId, "退出", &[42]));
        for label in ["退出登录", "Sign out", "关闭窗口", "Close Window", "Quit and sign out"] {
            assert!(!permits(Action::Quit, 42, UIA_MenuItemControlTypeId, label, &[42]));
        }
        assert!(!permits(Action::Quit, 43, UIA_MenuItemControlTypeId, "Quit", &[42]));
        assert!(!permits(Action::Quit, 42, UIA_ButtonControlTypeId, "Quit", &[42]));
    }
}
