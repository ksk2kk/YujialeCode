//! Windows 原生 Computer Use 后端。
//!
//! 截图使用 GDI 的 32 位顶向下 DIB，输入使用 `SendInput`，窗口和显示器来自
//! Win32 枚举接口。进程在每个入口都切到 Per-Monitor-V2 DPI 感知，避免 125% / 150%
//! 缩放下“截图坐标正确、点击却偏移”的经典问题。

use super::*;
use image::{ColorType, ImageFormat};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr;
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT, TRUE};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EnumDisplayMonitors,
    GetDC, GetMonitorInfoW, MonitorFromWindow, ReleaseDC, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HDC, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTOPRIMARY, RGBQUAD, SRCCOPY,
};
use windows_sys::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, VK_BACK, VK_CONTROL, VK_DELETE,
    VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_HOME, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR,
    VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetSystemMetrics, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    SetForegroundWindow, ShowWindow, MONITORINFOF_PRIMARY, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_RESTORE, WHEEL_DELTA,
};

#[derive(Debug, Clone, Serialize)]
struct WindowsWindow {
    window_id: u64,
    pid: u32,
    title: String,
    class_name: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct WindowsMonitor {
    id: u64,
    primary: bool,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    work_x: i32,
    work_y: i32,
    work_width: i32,
    work_height: i32,
}

pub(super) fn capabilities(vision_available: bool) -> Result<String, String> {
    enable_dpi_awareness();
    Ok(serde_json::to_string_pretty(&json!({
        "available": true,
        "session": "windows",
        "capture": {"backend":"Win32 GDI top-down DIB","targets":["focused_output","output","all","region","window"]},
        "input": {"backend":"Win32 SendInput","unicode":true,"per_monitor_dpi_v2":true},
        "windows": "EnumWindows / SetForegroundWindow",
        "vision": vision_available,
        "note": "UIPI 会阻止普通权限进程控制以管理员身份运行的窗口；此时请以相同权限启动 YujialeCode"
    })).map_err(|e| e.to_string())?)
}

pub(super) fn list_outputs() -> Result<String, String> {
    enable_dpi_awareness();
    serde_json::to_string_pretty(&monitor_list()?).map_err(|e| e.to_string())
}

pub(super) fn list_windows() -> Result<String, String> {
    enable_dpi_awareness();
    serde_json::to_string_pretty(&window_list()?).map_err(|e| e.to_string())
}

pub(super) fn focus_window(id: u64) -> Result<(), String> {
    enable_dpi_awareness();
    let hwnd = id as isize as HWND;
    let found = window_list()?.iter().any(|window| window.window_id == id);
    if !found {
        return Err(format!("没有找到 Windows 窗口 {id}"));
    }
    unsafe {
        ShowWindow(hwnd, SW_RESTORE);
        if SetForegroundWindow(hwnd) == 0 {
            return Err("Windows 拒绝切换前台窗口；请先手动与目标应用交互，或检查权限级别".into());
        }
    }
    Ok(())
}

pub(super) fn capture(
    cfg: &Config,
    session: &str,
    args: &Value,
    cancel: &AtomicBool,
    vision_available: bool,
) -> Result<String, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("已取消".into());
    }
    enable_dpi_awareness();
    let monitors = monitor_list()?;
    let bounds = virtual_screen_rect();
    let requested = args
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("focused_output")
        .trim()
        .to_ascii_lowercase();
    let (target, output, logical) = match requested.as_str() {
        "focused" | "focused_output" | "focused-output" => {
            let hwnd = unsafe { GetForegroundWindow() };
            let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY) };
            let value = monitors
                .iter()
                .find(|value| value.id == monitor as usize as u64)
                .or_else(|| monitors.iter().find(|value| value.primary))
                .ok_or("没有找到当前显示器")?;
            (
                "output".to_string(),
                Some(value.id.to_string()),
                monitor_rect(*value),
            )
        }
        "output" => {
            let id = args
                .get("output")
                .and_then(Value::as_str)
                .ok_or("target=output 时必须提供 output")?
                .parse::<u64>()
                .map_err(|_| "Windows output 必须是 list_outputs 返回的数字 id")?;
            let value = monitors
                .iter()
                .find(|value| value.id == id)
                .ok_or("找不到指定显示器")?;
            (
                "output".to_string(),
                Some(id.to_string()),
                monitor_rect(*value),
            )
        }
        "all" | "desktop" => ("all".to_string(), None, bounds),
        "region" => {
            let geometry = args
                .get("region")
                .and_then(Value::as_str)
                .ok_or("target=region 时必须提供 region")?;
            ("region".to_string(), None, parse_geometry(geometry)?)
        }
        "window" => {
            let id = args
                .get("window_id")
                .and_then(Value::as_u64)
                .ok_or("target=window 时必须提供 window_id")?;
            let value = window_list()?
                .into_iter()
                .find(|window| window.window_id == id)
                .ok_or("找不到指定窗口")?;
            (
                "window".to_string(),
                Some(id.to_string()),
                Rect {
                    x: value.x as f64,
                    y: value.y as f64,
                    width: value.width as f64,
                    height: value.height as f64,
                },
            )
        }
        other => return Err(format!("未知截图 target: {other}")),
    };
    if logical.width <= 0.0 || logical.height <= 0.0 {
        return Err("截图区域宽高必须大于 0".into());
    }
    let frame_id = next_frame_id();
    let dir = frame_dir(cfg, session);
    fs::create_dir_all(&dir).map_err(|e| format!("创建截图目录失败: {e}"))?;
    let path = dir.join(format!("{frame_id}.png"));
    capture_gdi(logical, &path)?;
    let (pixel_width, pixel_height) = png_size(&path)?;
    let meta = FrameMeta {
        frame_id,
        image_path: path.to_string_lossy().into_owned(),
        target,
        output,
        pixel_width,
        pixel_height,
        logical_x: logical.x,
        logical_y: logical.y,
        logical_width: logical.width,
        logical_height: logical.height,
        layout_min_x: bounds.x,
        layout_min_y: bounds.y,
        layout_width: bounds.width,
        layout_height: bounds.height,
        clickable: true,
        captured_unix_ms: unix_ms(),
    };
    save_latest_meta(cfg, session, &meta)?;
    prune_old_frames(&dir, 8);
    Ok(frame_result(&meta, vision_available))
}

pub(super) fn pointer_move(meta: &FrameMeta, x: f64, y: f64) -> Result<(), String> {
    enable_dpi_awareness();
    send_mouse_move(meta, x, y)
}

pub(super) fn pointer_click(
    meta: &FrameMeta,
    x: f64,
    y: f64,
    button: u32,
    count: usize,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let (down, up) = mouse_button_flags(button)?;
    send_mouse_move(meta, x, y)?;
    for index in 0..count {
        send_mouse_flags(down, 0)?;
        cancellable_sleep(Duration::from_millis(24), cancel)?;
        send_mouse_flags(up, 0)?;
        if index + 1 < count {
            cancellable_sleep(Duration::from_millis(70), cancel)?;
        }
    }
    Ok(())
}

pub(super) fn pointer_drag(
    meta: &FrameMeta,
    from: (f64, f64),
    to: (f64, f64),
    button: u32,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let (down, up) = mouse_button_flags(button)?;
    send_mouse_move(meta, from.0, from.1)?;
    send_mouse_flags(down, 0)?;
    for step in 1..=20 {
        if cancel.load(Ordering::Relaxed) {
            let _ = send_mouse_flags(up, 0);
            return Err("拖拽被取消，已释放鼠标键".into());
        }
        let t = step as f64 / 20.0;
        send_mouse_move(
            meta,
            from.0 + (to.0 - from.0) * t,
            from.1 + (to.1 - from.1) * t,
        )?;
        cancellable_sleep(Duration::from_millis(8), cancel)?;
    }
    send_mouse_flags(up, 0)
}

pub(super) fn pointer_scroll(
    meta: &FrameMeta,
    point: Option<(f64, f64)>,
    steps: i32,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("已取消".into());
    }
    if let Some((x, y)) = point {
        send_mouse_move(meta, x, y)?;
    }
    send_mouse_flags(MOUSEEVENTF_WHEEL, ((-steps) * WHEEL_DELTA as i32) as u32)
}

pub(super) fn type_text(text: &str, cancel: &AtomicBool) -> Result<(), String> {
    let mut inputs = Vec::new();
    for unit in text.encode_utf16() {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".into());
        }
        inputs.push(keyboard_input(0, unit, KEYEVENTF_UNICODE));
        inputs.push(keyboard_input(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
        if inputs.len() >= 256 {
            send_inputs(&inputs)?;
            inputs.clear();
        }
    }
    send_inputs(&inputs)
}

pub(super) fn press_key(keys: &str, cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("已取消".into());
    }
    let parts: Vec<&str> = keys
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() || parts.len() > 8 {
        return Err("keys 格式错误，例如 CTRL+L、WIN+D 或 Return".into());
    }
    let mut modifiers = Vec::new();
    for modifier in &parts[..parts.len() - 1] {
        modifiers.push(match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => VK_CONTROL,
            "shift" => VK_SHIFT,
            "alt" | "option" => VK_MENU,
            "win" | "super" | "meta" | "command" | "cmd" => VK_LWIN,
            other => return Err(format!("不支持的 Windows 修饰键 {other}")),
        } as u16);
    }
    let key = windows_vk(parts.last().copied().unwrap())?;
    let mut inputs = Vec::new();
    for vk in &modifiers {
        inputs.push(keyboard_input(*vk, 0, 0));
    }
    inputs.push(keyboard_input(key, 0, 0));
    inputs.push(keyboard_input(key, 0, KEYEVENTF_KEYUP));
    for vk in modifiers.iter().rev() {
        inputs.push(keyboard_input(*vk, 0, KEYEVENTF_KEYUP));
    }
    send_inputs(&inputs)
}

fn enable_dpi_awareness() {
    unsafe {
        // 进程可能已由宿主设置 DPI 模式；失败通常代表“已经设置”，无需中断操作。
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

fn virtual_screen_rect() -> Rect {
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN) as f64,
            y: GetSystemMetrics(SM_YVIRTUALSCREEN) as f64,
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as f64,
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as f64,
        }
    }
}

fn capture_gdi(rect: Rect, path: &Path) -> Result<(), String> {
    let x = rect.x.round() as i32;
    let y = rect.y.round() as i32;
    let width = rect.width.round() as i32;
    let height = rect.height.round() as i32;
    if width <= 0 || height <= 0 || (width as u64 * height as u64 * 4) > MAX_CAPTURE_BYTES * 4 {
        return Err(format!("Windows 截图尺寸异常: {width}x{height}"));
    }
    unsafe {
        let screen = GetDC(ptr::null_mut());
        if screen.is_null() {
            return Err("GetDC 获取桌面失败".into());
        }
        let memory = CreateCompatibleDC(screen);
        if memory.is_null() {
            ReleaseDC(ptr::null_mut(), screen);
            return Err("CreateCompatibleDC 失败".into());
        }
        let mut info: BITMAPINFO = zeroed();
        info.bmiHeader = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        info.bmiColors = [RGBQUAD {
            rgbBlue: 0,
            rgbGreen: 0,
            rgbRed: 0,
            rgbReserved: 0,
        }];
        let mut pixels: *mut c_void = ptr::null_mut();
        let bitmap = CreateDIBSection(
            screen,
            &info,
            DIB_RGB_COLORS,
            &mut pixels,
            ptr::null_mut(),
            0,
        );
        if bitmap.is_null() || pixels.is_null() {
            DeleteDC(memory);
            ReleaseDC(ptr::null_mut(), screen);
            return Err("CreateDIBSection 失败".into());
        }
        let previous = SelectObject(memory, bitmap);
        let copied = BitBlt(
            memory,
            0,
            0,
            width,
            height,
            screen,
            x,
            y,
            SRCCOPY | CAPTUREBLT,
        );
        let result = if copied == 0 {
            Err("BitBlt 截取桌面失败".into())
        } else {
            let size = width as usize * height as usize * 4;
            let bgra = std::slice::from_raw_parts(pixels.cast::<u8>(), size);
            let mut rgba = Vec::with_capacity(size);
            for value in bgra.chunks_exact(4) {
                rgba.extend_from_slice(&[value[2], value[1], value[0], 255]);
            }
            image::save_buffer_with_format(
                path,
                &rgba,
                width as u32,
                height as u32,
                ColorType::Rgba8,
                ImageFormat::Png,
            )
            .map_err(|e| format!("保存 Windows 截图失败: {e}"))
        };
        SelectObject(memory, previous);
        DeleteObject(bitmap);
        DeleteDC(memory);
        ReleaseDC(ptr::null_mut(), screen);
        result
    }
}

fn monitor_list() -> Result<Vec<WindowsMonitor>, String> {
    unsafe extern "system" fn callback(
        monitor: HMONITOR,
        _dc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let values = &mut *(data as *mut Vec<WindowsMonitor>);
        let mut info: MONITORINFO = zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) != 0 {
            values.push(WindowsMonitor {
                id: monitor as usize as u64,
                primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
                x: info.rcMonitor.left,
                y: info.rcMonitor.top,
                width: info.rcMonitor.right - info.rcMonitor.left,
                height: info.rcMonitor.bottom - info.rcMonitor.top,
                work_x: info.rcWork.left,
                work_y: info.rcWork.top,
                work_width: info.rcWork.right - info.rcWork.left,
                work_height: info.rcWork.bottom - info.rcWork.top,
            });
        }
        TRUE
    }
    let mut values = Vec::new();
    let success = unsafe {
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(callback),
            (&mut values as *mut Vec<WindowsMonitor>) as LPARAM,
        )
    };
    if success == 0 || values.is_empty() {
        Err("EnumDisplayMonitors 没有返回显示器".into())
    } else {
        Ok(values)
    }
}

fn window_list() -> Result<Vec<WindowsWindow>, String> {
    unsafe extern "system" fn callback(hwnd: HWND, data: LPARAM) -> BOOL {
        if IsWindowVisible(hwnd) == 0 {
            return TRUE;
        }
        let length = GetWindowTextLengthW(hwnd);
        if length <= 0 {
            return TRUE;
        }
        let mut title = vec![0u16; length as usize + 1];
        let read = GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32);
        if read <= 0 {
            return TRUE;
        }
        title.truncate(read as usize);
        let mut class_name = vec![0u16; 512];
        let class_read = GetClassNameW(hwnd, class_name.as_mut_ptr(), class_name.len() as i32);
        class_name.truncate(class_read.max(0) as usize);
        let mut rect: RECT = zeroed();
        if GetWindowRect(hwnd, &mut rect) == 0 || rect.right <= rect.left || rect.bottom <= rect.top
        {
            return TRUE;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let values = &mut *(data as *mut Vec<WindowsWindow>);
        values.push(WindowsWindow {
            window_id: hwnd as usize as u64,
            pid,
            title: String::from_utf16_lossy(&title),
            class_name: String::from_utf16_lossy(&class_name),
            x: rect.left,
            y: rect.top,
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
        });
        TRUE
    }
    let mut values = Vec::new();
    let success = unsafe {
        EnumWindows(
            Some(callback),
            (&mut values as *mut Vec<WindowsWindow>) as LPARAM,
        )
    };
    if success == 0 {
        Err("EnumWindows 失败".into())
    } else {
        Ok(values)
    }
}

fn monitor_rect(value: WindowsMonitor) -> Rect {
    Rect {
        x: value.x as f64,
        y: value.y as f64,
        width: value.width as f64,
        height: value.height as f64,
    }
}

fn send_mouse_move(meta: &FrameMeta, x: f64, y: f64) -> Result<(), String> {
    let width = meta.layout_width.max(1.0);
    let height = meta.layout_height.max(1.0);
    let normalized_x = (((x - meta.layout_min_x) * 65_535.0 / (width - 1.0).max(1.0)).round())
        .clamp(0.0, 65_535.0) as i32;
    let normalized_y = (((y - meta.layout_min_y) * 65_535.0 / (height - 1.0).max(1.0)).round())
        .clamp(0.0, 65_535.0) as i32;
    let mut input: INPUT = unsafe { zeroed() };
    input.r#type = INPUT_MOUSE;
    input.Anonymous.mi = MOUSEINPUT {
        dx: normalized_x,
        dy: normalized_y,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        time: 0,
        dwExtraInfo: 0,
    };
    send_inputs(&[input])
}

fn send_mouse_flags(flags: u32, data: u32) -> Result<(), String> {
    let mut input: INPUT = unsafe { zeroed() };
    input.r#type = INPUT_MOUSE;
    input.Anonymous.mi = MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: data,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    send_inputs(&[input])
}

fn keyboard_input(vk: u16, scan: u16, flags: u32) -> INPUT {
    let mut input: INPUT = unsafe { zeroed() };
    input.r#type = INPUT_KEYBOARD;
    input.Anonymous.ki = KEYBDINPUT {
        wVk: vk,
        wScan: scan,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    input
}

fn send_inputs(inputs: &[INPUT]) -> Result<(), String> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    };
    if sent == inputs.len() as u32 {
        Ok(())
    } else {
        Err(format!(
            "SendInput 只发送了 {sent}/{} 个事件；目标窗口可能处于更高权限级别",
            inputs.len()
        ))
    }
}

fn mouse_button_flags(button: u32) -> Result<(u32, u32), String> {
    match button {
        0x110 => Ok((MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP)),
        0x111 => Ok((MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)),
        0x112 => Ok((MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP)),
        _ => Err("Windows 不支持这个鼠标键".into()),
    }
}

fn windows_vk(key: &str) -> Result<u16, String> {
    let normalized = key.to_ascii_lowercase();
    if normalized.len() == 1 {
        let byte = normalized.as_bytes()[0];
        if byte.is_ascii_alphanumeric() {
            return Ok(byte.to_ascii_uppercase() as u16);
        }
    }
    let value = match normalized.as_str() {
        "return" | "enter" => VK_RETURN,
        "escape" | "esc" => VK_ESCAPE,
        "tab" => VK_TAB,
        "space" => VK_SPACE,
        "backspace" => VK_BACK,
        "delete" => VK_DELETE,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "pgup" => VK_PRIOR,
        "pagedown" | "pgdn" => VK_NEXT,
        "left" | "arrowleft" => VK_LEFT,
        "right" | "arrowright" => VK_RIGHT,
        "up" | "arrowup" => VK_UP,
        "down" | "arrowdown" => VK_DOWN,
        value if value.starts_with('f') => {
            let number: u16 = value[1..]
                .parse()
                .map_err(|_| format!("不支持的 Windows 按键 {key}"))?;
            if (1..=24).contains(&number) {
                VK_F1 as u16 + number - 1
            } else {
                return Err(format!("不支持的 Windows 按键 {key}"));
            }
        }
        _ => return Err(format!("不支持的 Windows 按键 {key}")),
    };
    Ok(value as u16)
}
