// Adapted from zitont/computer-use-win (MIT), src/uia.rs.
// See THIRD_PARTY_NOTICES.md for attribution.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiElement {
    pub element_index: i32,
    pub control_type: String,
    pub name: String,
    pub automation_id: String,
    pub class_name: String,
    pub is_enabled: bool,
    pub bounding_rect: Rect,
    pub has_keyboard_focus: bool,
    pub is_offscreen: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone)]
pub struct UiTreeSnapshot {
    pub window_name: String,
    pub elements: Vec<UiElement>,
}

#[cfg(windows)]
mod windows_impl {
    use super::{Rect, UiElement, UiTreeSnapshot};
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTreeWalker,
        UIA_CONTROLTYPE_ID,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    pub fn capture_foreground(max_depth: i32) -> Result<UiTreeSnapshot, String> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        let foreground_hwnd = unsafe { GetForegroundWindow() };
        if foreground_hwnd.is_invalid() {
            return Err("No foreground window".into());
        }

        let automation: IUIAutomation = unsafe {
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                .map_err(|error| format!("UI Automation init failed: {error}"))?
        };
        let root: IUIAutomationElement = unsafe {
            automation
                .ElementFromHandle(foreground_hwnd)
                .map_err(|error| format!("UI Automation foreground lookup failed: {error}"))?
        };
        let window_name = unsafe { root.CurrentName().unwrap_or_default().to_string() };
        let walker: IUIAutomationTreeWalker = unsafe {
            automation
                .RawViewWalker()
                .or_else(|_| automation.ControlViewWalker())
                .map_err(|error| format!("UI Automation tree walker failed: {error}"))?
        };

        let mut elements = Vec::new();
        let mut index = 0;
        walk_element(
            &walker,
            &root,
            &mut elements,
            &mut index,
            0,
            max_depth.clamp(1, 12),
        )?;

        Ok(UiTreeSnapshot {
            window_name,
            elements,
        })
    }

    fn walk_element(
        walker: &IUIAutomationTreeWalker,
        element: &IUIAutomationElement,
        elements: &mut Vec<UiElement>,
        index: &mut i32,
        depth: i32,
        max_depth: i32,
    ) -> Result<(), String> {
        if depth > max_depth {
            return Ok(());
        }

        let name = unsafe { element.CurrentName().unwrap_or_default().to_string() };
        let control_type_id = unsafe {
            element
                .CurrentControlType()
                .unwrap_or(UIA_CONTROLTYPE_ID(0))
                .0
        };
        let automation_id = unsafe {
            element
                .CurrentAutomationId()
                .unwrap_or_default()
                .to_string()
        };
        let class_name = unsafe { element.CurrentClassName().unwrap_or_default().to_string() };
        let is_enabled = unsafe {
            element
                .CurrentIsEnabled()
                .map(|value| value.as_bool())
                .unwrap_or(false)
        };
        let has_keyboard_focus = unsafe {
            element
                .CurrentHasKeyboardFocus()
                .map(|value| value.as_bool())
                .unwrap_or(false)
        };
        let is_offscreen = unsafe {
            element
                .CurrentIsOffscreen()
                .map(|value| value.as_bool())
                .unwrap_or(false)
        };
        let rect = unsafe { element.CurrentBoundingRectangle().unwrap_or_default() };
        let width = rect.right - rect.left;
        let height = rect.bottom - rect.top;

        if (width > 0 && height > 0) || depth == 0 {
            let current_index = *index;
            *index += 1;
            elements.push(UiElement {
                element_index: current_index,
                control_type: control_type_to_string(control_type_id),
                name,
                automation_id,
                class_name,
                is_enabled,
                bounding_rect: Rect {
                    x: rect.left,
                    y: rect.top,
                    width,
                    height,
                },
                has_keyboard_focus,
                is_offscreen,
            });
        }

        if let Ok(child) = unsafe { walker.GetFirstChildElement(element) } {
            let mut current = child;
            loop {
                walk_element(walker, &current, elements, index, depth + 1, max_depth)?;
                match unsafe { walker.GetNextSiblingElement(&current) } {
                    Ok(next) => current = next,
                    Err(_) => break,
                }
            }
        }

        Ok(())
    }

    fn control_type_to_string(id: i32) -> String {
        match id {
            50000 => "Button",
            50001 => "Calendar",
            50002 => "CheckBox",
            50003 => "ComboBox",
            50004 => "Edit",
            50005 => "Hyperlink",
            50006 => "Image",
            50007 => "ListItem",
            50008 => "List",
            50009 => "Menu",
            50010 => "MenuBar",
            50011 => "MenuItem",
            50012 => "ProgressBar",
            50013 => "RadioButton",
            50014 => "ScrollBar",
            50015 => "Slider",
            50016 => "Spinner",
            50017 => "StatusBar",
            50018 => "Tab",
            50019 => "TabItem",
            50020 => "Text",
            50021 => "ToolBar",
            50022 => "ToolTip",
            50023 => "Tree",
            50024 => "TreeItem",
            50025 => "DataGrid",
            50026 => "DataItem",
            50027 => "Document",
            50028 => "SplitButton",
            50029 => "Window",
            50030 => "Pane",
            50031 => "Header",
            50032 => "HeaderItem",
            50033 => "Table",
            50034 => "Thumb",
            50035 => "DataColumn",
            50036 => "DataRow",
            50039 => "IPAddress",
            50040 => "Document",
            50042 => "Group",
            50044 => "DataGrid",
            50045 => "DataItem",
            _ => "Unknown",
        }
        .to_string()
    }
}

#[cfg(windows)]
pub use windows_impl::capture_foreground;

#[cfg(not(windows))]
pub fn capture_foreground(_max_depth: i32) -> Result<UiTreeSnapshot, String> {
    Err("UI Automation is supported on Windows only".into())
}

pub fn compact_tree(snapshot: &UiTreeSnapshot) -> String {
    snapshot
        .elements
        .iter()
        .map(|element| {
            let name: String = element.name.chars().take(80).collect();
            let mut flags = String::new();
            if !element.is_enabled {
                flags.push('!');
            }
            if element.is_offscreen {
                flags.push('O');
            }
            if element.has_keyboard_focus {
                flags.push('*');
            }
            format!(
                "{}|{}|{}|{}|{}|{},{},{},{}|{}",
                element.element_index,
                element.control_type,
                name.replace(['\r', '\n'], " "),
                element.automation_id.replace(['\r', '\n'], " "),
                element.class_name.replace(['\r', '\n'], " "),
                element.bounding_rect.x,
                element.bounding_rect.y,
                element.bounding_rect.width,
                element.bounding_rect.height,
                flags,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn resolve_element_center(
    snapshot: &UiTreeSnapshot,
    index: i32,
) -> Result<(i32, i32, String), String> {
    let element = snapshot
        .elements
        .iter()
        .find(|element| element.element_index == index)
        .ok_or_else(|| format!("UI element {index} no longer exists; refresh ui_tree"))?;

    if element.bounding_rect.width <= 0 || element.bounding_rect.height <= 0 {
        return Err(format!("UI element {index} has no clickable bounds"));
    }
    if element.is_offscreen {
        return Err(format!(
            "UI element {index} is offscreen; refresh or scroll first"
        ));
    }
    if !element.is_enabled {
        return Err(format!("UI element {index} is disabled"));
    }

    Ok((
        element.bounding_rect.x + element.bounding_rect.width / 2,
        element.bounding_rect.y + element.bounding_rect.height / 2,
        element.name.clone(),
    ))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn captures_foreground_window_tree() {
        let snapshot = capture_foreground(3).expect("capture foreground UI Automation tree");
        assert!(!snapshot.elements.is_empty());
        assert!(snapshot.elements[0].element_index == 0);
    }
}
