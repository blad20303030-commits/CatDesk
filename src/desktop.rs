use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug)]
pub struct ScreenPlan {
    pub origin_x: i32,
    pub origin_y: i32,
    pub physical_width: i32,
    pub physical_height: i32,
    pub model_width: u32,
    pub model_height: u32,
}

pub struct Screenshot {
    pub png: Vec<u8>,
    pub plan: ScreenPlan,
}

static LAST_SCREEN_PLAN: OnceLock<Mutex<Option<ScreenPlan>>> = OnceLock::new();

fn screen_plan_slot() -> &'static Mutex<Option<ScreenPlan>> {
    LAST_SCREEN_PLAN.get_or_init(|| Mutex::new(None))
}

pub fn last_screen_plan() -> Option<ScreenPlan> {
    screen_plan_slot().lock().ok().and_then(|guard| *guard)
}

#[cfg(windows)]
pub fn capture_screenshot(max_width: u32, max_height: u32) -> Result<Screenshot, String> {
    use image::{ColorType, ImageEncoder, RgbaImage, imageops::FilterType};
    use image::codecs::png::PngEncoder;
    use std::mem::{size_of, zeroed};
    use std::ptr::null_mut;
    use windows_sys::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap,
        CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits,
        ReleaseDC, SelectObject, CAPTUREBLT, SRCCOPY,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
        SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    let max_width = max_width.clamp(320, 1920);
    let max_height = max_height.clamp(240, 1080);

    unsafe {
        let origin_x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let origin_y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let physical_width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let physical_height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        if physical_width <= 0 || physical_height <= 0 {
            return Err("Windows returned invalid virtual-screen dimensions".into());
        }

        let screen_dc = GetDC(null_mut());
        if screen_dc.is_null() {
            return Err("GetDC failed".into());
        }

        let memory_dc = CreateCompatibleDC(screen_dc);
        if memory_dc.is_null() {
            ReleaseDC(null_mut(), screen_dc);
            return Err("CreateCompatibleDC failed".into());
        }

        let bitmap = CreateCompatibleBitmap(screen_dc, physical_width, physical_height);
        if bitmap.is_null() {
            DeleteDC(memory_dc);
            ReleaseDC(null_mut(), screen_dc);
            return Err("CreateCompatibleBitmap failed".into());
        }

        let old = SelectObject(memory_dc, bitmap as _);
        let copied = BitBlt(
            memory_dc,
            0,
            0,
            physical_width,
            physical_height,
            screen_dc,
            origin_x,
            origin_y,
            SRCCOPY | CAPTUREBLT,
        );
        if copied == 0 {
            SelectObject(memory_dc, old);
            DeleteObject(bitmap as _);
            DeleteDC(memory_dc);
            ReleaseDC(null_mut(), screen_dc);
            return Err("BitBlt failed".into());
        }

        let mut info: BITMAPINFO = zeroed();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: physical_width,
            biHeight: -physical_height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..zeroed()
        };

        let byte_len = physical_width as usize * physical_height as usize * 4;
        let mut bgra = vec![0u8; byte_len];
        let rows = GetDIBits(
            memory_dc,
            bitmap,
            0,
            physical_height as u32,
            bgra.as_mut_ptr().cast(),
            &mut info,
            DIB_RGB_COLORS,
        );

        SelectObject(memory_dc, old);
        DeleteObject(bitmap as _);
        DeleteDC(memory_dc);
        ReleaseDC(null_mut(), screen_dc);

        if rows == 0 {
            return Err("GetDIBits failed".into());
        }

        for pixel in bgra.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }

        let image = RgbaImage::from_raw(
            physical_width as u32,
            physical_height as u32,
            bgra,
        )
        .ok_or_else(|| "Failed to construct screenshot image".to_string())?;

        let scale = f64::min(
            1.0,
            f64::min(
                max_width as f64 / physical_width as f64,
                max_height as f64 / physical_height as f64,
            ),
        );
        let model_width = ((physical_width as f64 * scale).round() as u32).max(1);
        let model_height = ((physical_height as f64 * scale).round() as u32).max(1);

        let resized = if model_width == physical_width as u32
            && model_height == physical_height as u32
        {
            image
        } else {
            image::imageops::resize(&image, model_width, model_height, FilterType::Triangle)
        };

        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(
                resized.as_raw(),
                model_width,
                model_height,
                ColorType::Rgba8.into(),
            )
            .map_err(|error| format!("PNG encode failed: {error}"))?;

        let plan = ScreenPlan {
            origin_x,
            origin_y,
            physical_width,
            physical_height,
            model_width,
            model_height,
        };
        if let Ok(mut guard) = screen_plan_slot().lock() {
            *guard = Some(plan);
        }

        Ok(Screenshot { png, plan })
    }
}

#[cfg(not(windows))]
pub fn capture_screenshot(_max_width: u32, _max_height: u32) -> Result<Screenshot, String> {
    Err("Desktop computer-use is supported on Windows only".into())
}

fn map_model_point(x: i32, y: i32) -> Result<(i32, i32), String> {
    let plan = last_screen_plan()
        .ok_or_else(|| "Call screenshot before using mouse coordinates".to_string())?;
    if x < 0 || y < 0 || x >= plan.model_width as i32 || y >= plan.model_height as i32 {
        return Err(format!(
            "Point ({x},{y}) is outside last screenshot {}x{}",
            plan.model_width, plan.model_height
        ));
    }
    let px = plan.origin_x
        + ((x as f64 + 0.5) * plan.physical_width as f64 / plan.model_width as f64).floor() as i32;
    let py = plan.origin_y
        + ((y as f64 + 0.5) * plan.physical_height as f64 / plan.model_height as f64).floor() as i32;
    Ok((px, py))
}

#[cfg(windows)]
pub fn mouse_move(x: i32, y: i32) -> Result<(i32, i32), String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos;

    let (screen_x, screen_y) = map_model_point(x, y)?;
    let ok = unsafe { SetCursorPos(screen_x, screen_y) };
    if ok == 0 {
        return Err("SetCursorPos failed".into());
    }
    Ok((screen_x, screen_y))
}

#[cfg(not(windows))]
pub fn mouse_move(_x: i32, _y: i32) -> Result<(i32, i32), String> {
    Err("Desktop computer-use is supported on Windows only".into())
}

#[cfg(windows)]
pub fn mouse_click(x: i32, y: i32, button: &str, clicks: u32) -> Result<(i32, i32), String> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN,
        MOUSEEVENTF_RIGHTUP, MOUSEINPUT, SendInput,
    };

    let (screen_x, screen_y) = mouse_move(x, y)?;
    let (down, up) = match button {
        "left" => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        "right" => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        "middle" => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        other => return Err(format!("Unsupported mouse button: {other}")),
    };
    let clicks = clicks.clamp(1, 3);

    unsafe {
        for _ in 0..clicks {
            let mut inputs: [INPUT; 2] = [zeroed(), zeroed()];
            inputs[0].r#type = INPUT_MOUSE;
            inputs[0].Anonymous.mi = MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: down,
                time: 0,
                dwExtraInfo: 0,
            };
            inputs[1].r#type = INPUT_MOUSE;
            inputs[1].Anonymous.mi = MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: up,
                time: 0,
                dwExtraInfo: 0,
            };
            let sent = SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32);
            if sent != inputs.len() as u32 {
                return Err("SendInput mouse click failed".into());
            }
            if clicks > 1 {
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
        }
    }

    Ok((screen_x, screen_y))
}

#[cfg(not(windows))]
pub fn mouse_click(_x: i32, _y: i32, _button: &str, _clicks: u32) -> Result<(i32, i32), String> {
    Err("Desktop computer-use is supported on Windows only".into())
}

#[cfg(windows)]
pub fn mouse_scroll(delta: i32) -> Result<(), String> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_MOUSE, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
    };

    unsafe {
        let mut input: INPUT = zeroed();
        input.r#type = INPUT_MOUSE;
        input.Anonymous.mi = MOUSEINPUT {
            dx: 0,
            dy: 0,
            mouseData: delta as u32,
            dwFlags: MOUSEEVENTF_WHEEL,
            time: 0,
            dwExtraInfo: 0,
        };
        let sent = SendInput(1, &input, size_of::<INPUT>() as i32);
        if sent != 1 {
            return Err("SendInput mouse scroll failed".into());
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn mouse_scroll(_delta: i32) -> Result<(), String> {
    Err("Desktop computer-use is supported on Windows only".into())
}

#[cfg(windows)]
pub fn type_text(text: &str) -> Result<(), String> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput,
    };

    for unit in text.encode_utf16() {
        unsafe {
            let mut inputs: [INPUT; 2] = [zeroed(), zeroed()];
            inputs[0].r#type = INPUT_KEYBOARD;
            inputs[0].Anonymous.ki = KEYBDINPUT {
                wVk: 0,
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE,
                time: 0,
                dwExtraInfo: 0,
            };
            inputs[1].r#type = INPUT_KEYBOARD;
            inputs[1].Anonymous.ki = KEYBDINPUT {
                wVk: 0,
                wScan: unit,
                dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                time: 0,
                dwExtraInfo: 0,
            };
            let sent = SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32);
            if sent != inputs.len() as u32 {
                return Err("SendInput text failed".into());
            }
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn type_text(_text: &str) -> Result<(), String> {
    Err("Desktop computer-use is supported on Windows only".into())
}

#[cfg(windows)]
fn virtual_key(name: &str) -> Option<u16> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    let lower = name.trim().to_ascii_lowercase();
    let vk = match lower.as_str() {
        "enter" | "return" => VK_RETURN,
        "tab" => VK_TAB,
        "escape" | "esc" => VK_ESCAPE,
        "backspace" => VK_BACK,
        "delete" | "del" => VK_DELETE,
        "space" => VK_SPACE,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        "up" => VK_UP,
        "down" => VK_DOWN,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" => VK_PRIOR,
        "pagedown" | "pgdn" => VK_NEXT,
        "ctrl" | "control" => VK_CONTROL,
        "shift" => VK_SHIFT,
        "alt" => VK_MENU,
        "win" | "windows" | "meta" => VK_LWIN,
        "f1" => VK_F1,
        "f2" => VK_F2,
        "f3" => VK_F3,
        "f4" => VK_F4,
        "f5" => VK_F5,
        "f6" => VK_F6,
        "f7" => VK_F7,
        "f8" => VK_F8,
        "f9" => VK_F9,
        "f10" => VK_F10,
        "f11" => VK_F11,
        "f12" => VK_F12,
        _ if lower.len() == 1 => {
            let byte = lower.as_bytes()[0];
            if byte.is_ascii_alphabetic() {
                (byte.to_ascii_uppercase()) as u16
            } else if byte.is_ascii_digit() {
                byte as u16
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(vk as u16)
}

#[cfg(windows)]
pub fn key_press(key: &str, modifiers: &[String]) -> Result<(), String> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    };

    fn send_vk(vk: u16, key_up: bool) -> Result<(), String> {
        unsafe {
            let mut input: INPUT = zeroed();
            input.r#type = INPUT_KEYBOARD;
            input.Anonymous.ki = KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if key_up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            };
            let sent = SendInput(1, &input, size_of::<INPUT>() as i32);
            if sent != 1 {
                return Err("SendInput key press failed".into());
            }
        }
        Ok(())
    }

    let mut modifier_keys = Vec::new();
    for modifier in modifiers {
        modifier_keys.push(
            virtual_key(modifier)
                .ok_or_else(|| format!("Unsupported modifier key: {modifier}"))?,
        );
    }
    let key_vk = virtual_key(key).ok_or_else(|| format!("Unsupported key: {key}"))?;

    for &modifier in &modifier_keys {
        send_vk(modifier, false)?;
    }
    send_vk(key_vk, false)?;
    send_vk(key_vk, true)?;
    for &modifier in modifier_keys.iter().rev() {
        send_vk(modifier, true)?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn key_press(_key: &str, _modifiers: &[String]) -> Result<(), String> {
    Err("Desktop computer-use is supported on Windows only".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_model_coordinates_requires_screenshot() {
        if let Ok(mut guard) = screen_plan_slot().lock() {
            *guard = None;
        }
        assert!(map_model_point(0, 0).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn captures_real_windows_desktop_as_png() {
        let shot = capture_screenshot(960, 540).expect("capture Windows desktop");
        assert!(shot.png.starts_with(&[0x89, b'P', b'N', b'G']));
        assert!(shot.plan.model_width > 0);
        assert!(shot.plan.model_height > 0);
        assert!(shot.plan.physical_width > 0);
        assert!(shot.plan.physical_height > 0);
    }

    #[test]
    fn maps_model_coordinates_to_physical_space() {
        if let Ok(mut guard) = screen_plan_slot().lock() {
            *guard = Some(ScreenPlan {
                origin_x: -1920,
                origin_y: 0,
                physical_width: 3840,
                physical_height: 1080,
                model_width: 1366,
                model_height: 384,
            });
        }
        let (x, y) = map_model_point(683, 192).expect("map point");
        assert!(x >= -5 && x <= 5);
        assert!(y >= 539 && y <= 542);
    }
}
