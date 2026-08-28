use std::convert::TryFrom;
use std::thread;
use std::time::Duration;
use windows::core::{IUnknown, Result};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCondition, IUIAutomationElement,
    IUIAutomationElementArray, IUIAutomationSelectionItemPattern, IUIAutomationValuePattern,
    TreeScope_Children, TreeScope_Descendants, UIA_EditControlTypeId, UIA_SelectionItemPatternId,
    UIA_TabItemControlTypeId, UIA_ValuePatternId,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
    VK_CONTROL, VK_SHIFT, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowTextW, IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

const WEB_UI_TITLE: &str = "DeepSeek Harness";
const VK_R_KEY: VIRTUAL_KEY = VIRTUAL_KEY(0x52);

pub fn try_activate(web_ui_url: &str, refresh_matching_tab: bool) -> bool {
    unsafe {
        let initialization = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if initialization.is_err() {
            return false;
        }

        let result = activate_with_automation(web_ui_url, refresh_matching_tab);
        CoUninitialize();
        result.unwrap_or(false)
    }
}

unsafe fn activate_with_automation(web_ui_url: &str, refresh_matching_tab: bool) -> Result<bool> {
    let automation: IUIAutomation = CoCreateInstance::<_, IUIAutomation>(
        &CUIAutomation,
        None::<&IUnknown>,
        CLSCTX_INPROC_SERVER,
    )?;
    let root = automation.GetRootElement()?;
    let condition = automation.CreateTrueCondition()?;
    let windows = root.FindAll(TreeScope_Children, &condition)?;

    let windows = elements(&windows)?;

    for window in &windows {
        let class_name = current_string(&window.CurrentClassName()?).unwrap_or_default();
        if !is_browser_window(&class_name) {
            continue;
        }

        let window_title = current_string(&window.CurrentName()?).unwrap_or_default();
        if is_web_ui_title(&window_title) {
            activate_window(&window)?;
            refresh_if_matching(&window, &condition, web_ui_url, refresh_matching_tab);
            return Ok(true);
        }

        let tabs = window.FindAll(TreeScope_Descendants, &condition)?;
        let tabs = elements(&tabs)?
            .into_iter()
            .filter(|tab| {
                tab.CurrentControlType()
                    .map(|control_type| control_type == UIA_TabItemControlTypeId)
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        let previously_selected = find_selected_tab(&tabs);

        for tab in &tabs {
            let title = current_string(&tab.CurrentName()?).unwrap_or_default();
            if !is_web_ui_title(&title) {
                continue;
            }

            if !select_tab(Some(tab))? || !has_matching_address(&window, &condition, web_ui_url)? {
                let _ = select_tab(previously_selected.as_ref());
                continue;
            }

            activate_window(&window)?;
            refresh_activated_tab(&window, refresh_matching_tab, true);
            return Ok(true);
        }
    }

    for window in &windows {
        let class_name = current_string(&window.CurrentClassName()?).unwrap_or_default();
        if class_name != "Chrome_WidgetWin_1" {
            continue;
        }

        let handle = window.CurrentNativeWindowHandle()?;
        if handle.0.is_null() || activate_window(window).is_err() {
            continue;
        }
        thread::sleep(Duration::from_millis(50));
        if unsafe { GetForegroundWindow().0 != handle.0 } {
            continue;
        }

        if try_activate_with_tab_cycle(handle) {
            refresh_if_matching(window, &condition, web_ui_url, refresh_matching_tab);
            return Ok(true);
        }
    }

    Ok(false)
}

unsafe fn refresh_if_matching(
    window: &IUIAutomationElement,
    condition: &IUIAutomationCondition,
    web_ui_url: &str,
    refresh_requested: bool,
) {
    if !refresh_requested {
        return;
    }
    let address_matches = has_matching_address(window, condition, web_ui_url).unwrap_or(false);
    refresh_activated_tab(window, refresh_requested, address_matches);
}

unsafe fn refresh_activated_tab(
    window: &IUIAutomationElement,
    refresh_requested: bool,
    address_matches: bool,
) {
    if !should_refresh(refresh_requested, address_matches) {
        return;
    }
    let Ok(handle) = window.CurrentNativeWindowHandle() else {
        return;
    };
    thread::sleep(Duration::from_millis(50));
    if !handle.0.is_null() && GetForegroundWindow().0 == handle.0 {
        let _ = send_key_chord(&[VK_CONTROL, VK_R_KEY]);
    }
}

fn should_refresh(refresh_requested: bool, address_matches: bool) -> bool {
    refresh_requested && address_matches
}

fn try_activate_with_tab_cycle(window: windows::Win32::Foundation::HWND) -> bool {
    const MAX_TAB_CYCLE_STEPS: usize = 32;
    let initial_title = window_title(window);
    if initial_title.is_empty() {
        return false;
    }

    for _ in 0..MAX_TAB_CYCLE_STEPS {
        if !send_key_chord(&[VK_CONTROL, VK_TAB]) {
            return false;
        }
        thread::sleep(Duration::from_millis(50));

        let title = window_title(window);
        if is_web_ui_title(&title) {
            return true;
        }
        if title == initial_title {
            return false;
        }
    }

    for _ in 0..MAX_TAB_CYCLE_STEPS {
        if !send_key_chord(&[VK_CONTROL, VK_SHIFT, VK_TAB]) {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

fn send_key_chord(keys: &[VIRTUAL_KEY]) -> bool {
    let mut inputs = Vec::with_capacity(keys.len() * 2);
    for &key in keys {
        inputs.push(keyboard_input(key, Default::default()));
    }
    for &key in keys.iter().rev() {
        inputs.push(keyboard_input(key, KEYEVENTF_KEYUP));
    }
    send_inputs(&inputs)
}

fn keyboard_input(
    key: VIRTUAL_KEY,
    flags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS,
) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: key,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send_inputs(inputs: &[INPUT]) -> bool {
    unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) == inputs.len() as u32 }
}

fn window_title(window: windows::Win32::Foundation::HWND) -> String {
    let mut title = [0u16; 512];
    let length = unsafe { GetWindowTextW(window, &mut title) };
    String::from_utf16_lossy(&title[..length.max(0) as usize])
}

unsafe fn elements(array: &IUIAutomationElementArray) -> Result<Vec<IUIAutomationElement>> {
    let count = array.Length()?.max(0);
    (0..count).map(|index| array.GetElement(index)).collect()
}

unsafe fn find_selected_tab(tabs: &[IUIAutomationElement]) -> Option<IUIAutomationElement> {
    tabs.iter().find_map(|tab| {
        let pattern = tab
            .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
            .ok()?;
        pattern
            .CurrentIsSelected()
            .ok()?
            .as_bool()
            .then(|| tab.clone())
    })
}

unsafe fn select_tab(tab: Option<&IUIAutomationElement>) -> Result<bool> {
    let Some(tab) = tab else {
        return Ok(false);
    };
    let pattern =
        tab.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)?;
    pattern.Select()?;
    Ok(true)
}

unsafe fn has_matching_address(
    window: &IUIAutomationElement,
    condition: &IUIAutomationCondition,
    target_url: &str,
) -> Result<bool> {
    for _ in 0..3 {
        let edit_controls = window.FindAll(TreeScope_Descendants, condition)?;
        for edit_control in elements(&edit_controls)? {
            if edit_control
                .CurrentControlType()
                .map(|control_type| control_type != UIA_EditControlTypeId)
                .unwrap_or(true)
            {
                continue;
            }

            let Ok(pattern) =
                edit_control.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            else {
                continue;
            };
            let address = current_string(&pattern.CurrentValue()?).unwrap_or_default();
            if same_origin(&address, target_url) {
                return Ok(true);
            }
        }
        thread::sleep(Duration::from_millis(50));
    }

    Ok(false)
}

unsafe fn activate_window(window: &IUIAutomationElement) -> Result<()> {
    let handle = window.CurrentNativeWindowHandle()?;
    if handle.0.is_null() {
        return Ok(());
    }
    if IsIconic(handle).as_bool() {
        let _ = ShowWindow(handle, SW_RESTORE);
    }
    let _ = window.SetFocus();
    let _ = SetForegroundWindow(handle);
    Ok(())
}

fn current_string(value: &windows::core::BSTR) -> Option<String> {
    String::try_from(value).ok()
}

fn is_browser_window(class_name: &str) -> bool {
    class_name == "Chrome_WidgetWin_1" || class_name == "MozillaWindowClass"
}

fn is_web_ui_title(title: &str) -> bool {
    title
        .to_ascii_lowercase()
        .contains(&WEB_UI_TITLE.to_ascii_lowercase())
}

fn same_origin(address: &str, target: &str) -> bool {
    let Some(target) = parse_origin(target, None) else {
        return false;
    };
    let Some(address) = parse_origin(address, Some(&target.0)) else {
        return false;
    };
    address == target
}

fn parse_origin(value: &str, default_scheme: Option<&str>) -> Option<(String, String, u16)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let normalized;
    let value = if value.contains("://") {
        value
    } else {
        normalized = format!("{}://{value}", default_scheme.unwrap_or("http"));
        &normalized
    };

    let (scheme, remainder) = value.split_once("://")?;
    let authority = remainder.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    let (host, port) = if let Some((host, port)) = authority.rsplit_once(':') {
        (host.trim_matches(['[', ']']), port.parse().ok()?)
    } else {
        (authority.trim_matches(['[', ']']), default_port(scheme)?)
    };
    if host.is_empty() {
        return None;
    }
    Some((scheme.to_ascii_lowercase(), host.to_ascii_lowercase(), port))
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme.to_ascii_lowercase().as_str() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{is_web_ui_title, same_origin, should_refresh};

    #[test]
    fn browser_address_matches_web_ui_origin() {
        assert!(same_origin(
            "http://127.0.0.1:3080/chat",
            "http://127.0.0.1:3080/"
        ));
        assert!(same_origin("127.0.0.1:3080", "http://127.0.0.1:3080/"));
        assert!(!same_origin(
            "http://127.0.0.1:3081/",
            "http://127.0.0.1:3080/"
        ));
    }

    #[test]
    fn browser_window_title_matches_web_ui() {
        assert!(is_web_ui_title("DeepSeek Harness - Cent Browser"));
        assert!(!is_web_ui_title("New Tab - Cent Browser"));
    }

    #[test]
    fn refresh_requires_restart_request_and_matching_address() {
        assert!(should_refresh(true, true));
        assert!(!should_refresh(true, false));
        assert!(!should_refresh(false, true));
    }
}
