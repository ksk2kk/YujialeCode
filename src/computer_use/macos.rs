//! macOS 原生 Computer Use 后端。
//!
//! 鼠标和键盘直接走 CoreGraphics；显示器与窗口清单直接读 WindowServer。
//! 截图调用系统自带的 `screencapture`，因为它负责统一处理 Retina、多显示器和
//! Screen Recording 隐私授权，YujialeCode 不需要长期驻留一个 Objective-C 运行时。

use super::*;
use std::ffi::{c_char, c_void, CStr};
use std::ptr;

type CFTypeRef = *const c_void;
type CFArrayRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFStringRef = *const c_void;
type CGEventRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct ProcessSerialNumber {
    high_long_of_psn: u32,
    low_long_of_psn: u32,
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGGetActiveDisplayList(max_displays: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGDisplayPixelsHigh(display: u32) -> usize;
    fn CGDisplayCopyDisplayMode(display: u32) -> CFTypeRef;
    fn CGDisplayModeGetPixelWidth(mode: CFTypeRef) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: CFTypeRef) -> usize;

    fn CGEventCreateMouseEvent(
        source: *const c_void,
        event_type: u32,
        position: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(
        source: *const c_void,
        keycode: u16,
        key_down: bool,
    ) -> CGEventRef;
    fn CGEventKeyboardSetUnicodeString(event: CGEventRef, length: usize, text: *const u16);
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventCreateScrollWheelEvent2(
        source: *const c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGPreflightPostEventAccess() -> bool;
    fn CGRequestPostEventAccess() -> bool;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn GetProcessForPID(pid: i32, psn: *mut ProcessSerialNumber) -> i32;
    fn SetFrontProcessWithOptions(psn: *const ProcessSerialNumber, options: u32) -> i32;

    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFArrayRef;
    fn CGRectMakeWithDictionaryRepresentation(dict: CFDictionaryRef, rect: *mut CGRect) -> bool;
    static kCGWindowNumber: CFStringRef;
    static kCGWindowOwnerPID: CFStringRef;
    static kCGWindowOwnerName: CFStringRef;
    static kCGWindowName: CFStringRef;
    static kCGWindowLayer: CFStringRef;
    static kCGWindowBounds: CFStringRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(value: CFTypeRef);
    fn CFArrayGetCount(array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> CFTypeRef;
    fn CFDictionaryGetValueIfPresent(
        dict: CFDictionaryRef,
        key: CFTypeRef,
        value: *mut CFTypeRef,
    ) -> bool;
    fn CFNumberGetValue(number: CFTypeRef, number_type: i32, value: *mut c_void) -> bool;
    fn CFStringGetCString(
        string: CFStringRef,
        buffer: *mut c_char,
        size: isize,
        encoding: u32,
    ) -> bool;
}

const K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1;
const K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 16;
const K_CF_NUMBER_SINT64_TYPE: i32 = 4;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_CG_HID_EVENT_TAP: u32 = 0;
const K_CG_SCROLL_EVENT_UNIT_LINE: u32 = 1;

#[derive(Debug, Clone, Serialize)]
struct MacWindow {
    window_id: u64,
    pid: i64,
    owner: String,
    title: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

pub(super) fn capabilities(vision_available: bool) -> Result<String, String> {
    let capture_program = capture_program();
    let input_authorized = unsafe { CGPreflightPostEventAccess() };
    let capture_authorized = unsafe { CGPreflightScreenCaptureAccess() };
    Ok(serde_json::to_string_pretty(&json!({
        "available": Path::new(capture_program).is_file() || command_exists(capture_program),
        "session": "macos",
        "capture": {
            "backend": "Apple screencapture / Screen Recording privacy control",
            "authorized": capture_authorized,
            "targets": ["focused_output", "output", "all", "region", "window"]
        },
        "input": {
            "backend": "CoreGraphics CGEvent",
            "authorized": input_authorized,
            "requires_accessibility_permission": true
        },
        "windows": "CoreGraphics WindowServer",
        "vision": vision_available,
        "note": "首次使用截图或控制输入时，macOS 可能要求在隐私与安全性中授权"
    }))
    .map_err(|e| e.to_string())?)
}

pub(super) fn list_outputs() -> Result<String, String> {
    let main = unsafe { CGMainDisplayID() };
    let displays = displays()?;
    let values: Vec<Value> = displays
        .iter()
        .map(|(id, rect, pixels)| {
            json!({
                "id": id,
                "name": format!("display-{id}"),
                "main": *id == main,
                "logical": {"x":rect.x,"y":rect.y,"width":rect.width,"height":rect.height},
                "pixels": {"width":pixels.0,"height":pixels.1},
                "scale": if rect.width > 0.0 { pixels.0 as f64 / rect.width } else { 1.0 }
            })
        })
        .collect();
    serde_json::to_string_pretty(&values).map_err(|e| e.to_string())
}

pub(super) fn list_windows(_cancel: &AtomicBool) -> Result<String, String> {
    serde_json::to_string_pretty(&window_list()?).map_err(|e| e.to_string())
}

pub(super) fn focus_window(id: u64, cancel: &AtomicBool) -> Result<(), String> {
    let _ = cancel;
    let window = window_list()?
        .into_iter()
        .find(|window| window.window_id == id)
        .ok_or_else(|| format!("没有找到 macOS 窗口 {id}"))?;
    // 直接使用系统进程服务，避免 AppleScript/System Events 额外弹出“自动化”授权。
    // 这些兼容 API 虽已 deprecated，但仍由 ApplicationServices 提供，且从 PID 激活
    // 正是 NSRunningApplication 出现前的稳定做法。
    let mut psn = ProcessSerialNumber::default();
    let lookup = unsafe { GetProcessForPID(window.pid as i32, &mut psn) };
    if lookup != 0 {
        return Err(format!(
            "把窗口 PID 转成 macOS 进程标识失败: OSStatus {lookup}"
        ));
    }
    let status = unsafe { SetFrontProcessWithOptions(&psn, 1) };
    if status == 0 {
        Ok(())
    } else {
        Err(format!("聚焦 macOS 窗口所属应用失败: OSStatus {status}"))
    }
}

pub(super) fn capture(
    cfg: &Config,
    session: &str,
    args: &Value,
    cancel: &AtomicBool,
    vision_available: bool,
) -> Result<String, String> {
    let all_displays = displays()?;
    let bounds = display_bounds(&all_displays).ok_or("没有找到可截图的显示器")?;
    let requested = args
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("focused_output")
        .trim()
        .to_ascii_lowercase();
    let frame_id = next_frame_id();
    let dir = frame_dir(cfg, session);
    fs::create_dir_all(&dir).map_err(|e| format!("创建截图目录失败: {e}"))?;
    let path = dir.join(format!("{frame_id}.png"));
    let mut command_args = vec!["-x".to_string(), "-t".into(), "png".into()];
    if args.get("cursor").and_then(Value::as_bool).unwrap_or(false) {
        command_args.push("-C".into());
    }
    let (target, output, logical, clickable) = match requested.as_str() {
        "focused" | "focused_output" | "focused-output" => {
            let main = unsafe { CGMainDisplayID() };
            let (_, rect, _) = all_displays
                .iter()
                .find(|(id, _, _)| *id == main)
                .ok_or("找不到主显示器")?;
            command_args.extend(["-R".into(), rect_arg(*rect)]);
            (
                "output".to_string(),
                Some(format!("display-{main}")),
                *rect,
                true,
            )
        }
        "output" => {
            let raw = args
                .get("output")
                .and_then(Value::as_str)
                .ok_or("target=output 时必须提供 output")?;
            let id: u32 = raw
                .trim_start_matches("display-")
                .parse()
                .map_err(|_| "macOS output 应为 display-<id> 或数字 id")?;
            let (_, rect, _) = all_displays
                .iter()
                .find(|(value, _, _)| *value == id)
                .ok_or("找不到指定显示器")?;
            command_args.extend(["-R".into(), rect_arg(*rect)]);
            (
                "output".to_string(),
                Some(format!("display-{id}")),
                *rect,
                true,
            )
        }
        "all" | "desktop" => {
            command_args.extend(["-R".into(), rect_arg(bounds)]);
            ("all".to_string(), None, bounds, true)
        }
        "region" => {
            let geometry = args
                .get("region")
                .and_then(Value::as_str)
                .ok_or("target=region 时必须提供 region")?;
            let rect = parse_geometry(geometry)?;
            command_args.extend(["-R".into(), rect_arg(rect)]);
            ("region".to_string(), None, rect, true)
        }
        "window" => {
            let id = args
                .get("window_id")
                .and_then(Value::as_u64)
                .ok_or("target=window 时必须提供 window_id")?;
            let window = window_list()?
                .into_iter()
                .find(|window| window.window_id == id)
                .ok_or("找不到指定窗口")?;
            command_args.extend(["-o".into(), "-l".into(), id.to_string()]);
            (
                "window".to_string(),
                Some(id.to_string()),
                Rect {
                    x: window.x,
                    y: window.y,
                    width: window.width,
                    height: window.height,
                },
                true,
            )
        }
        other => return Err(format!("未知截图 target: {other}")),
    };
    command_args.push(path.to_string_lossy().into_owned());
    let refs: Vec<&str> = command_args.iter().map(String::as_str).collect();
    let out = run_bounded(capture_program(), &refs, CAPTURE_TIMEOUT, cancel, true)?;
    if out.code != 0 {
        return Err(format!(
            "macOS 截图失败（exit {}）: {}。请检查“隐私与安全性 → 屏幕与系统录音”授权",
            out.code,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
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
        clickable,
        captured_unix_ms: unix_ms(),
        generation: current_ui_generation(cfg, session),
    };
    save_latest_meta(cfg, session, &meta)?;
    prune_old_frames(&dir, 8);
    Ok(frame_result(&meta, vision_available))
}

pub(super) fn pointer_move(x: f64, y: f64) -> Result<(), String> {
    ensure_input_access()?;
    post_mouse(5, x, y, 0)
}

pub(super) fn pointer_click(
    x: f64,
    y: f64,
    button: u32,
    count: usize,
    cancel: &AtomicBool,
) -> Result<(), String> {
    ensure_input_access()?;
    let (down, up, cg_button) = mouse_types(button, false)?;
    pointer_move(x, y)?;
    for index in 0..count {
        post_mouse(down, x, y, cg_button)?;
        cancellable_sleep(Duration::from_millis(24), cancel)?;
        post_mouse(up, x, y, cg_button)?;
        if index + 1 < count {
            cancellable_sleep(Duration::from_millis(70), cancel)?;
        }
    }
    Ok(())
}

pub(super) fn pointer_drag(
    from: (f64, f64),
    to: (f64, f64),
    button: u32,
    cancel: &AtomicBool,
) -> Result<(), String> {
    ensure_input_access()?;
    let (down, up, cg_button) = mouse_types(button, false)?;
    let (_, _, drag_button) = mouse_types(button, true)?;
    pointer_move(from.0, from.1)?;
    post_mouse(down, from.0, from.1, cg_button)?;
    for step in 1..=20 {
        if cancel.load(Ordering::Relaxed) {
            let _ = post_mouse(up, from.0, from.1, cg_button);
            return Err("拖拽被取消，已释放鼠标键".into());
        }
        let t = step as f64 / 20.0;
        let x = from.0 + (to.0 - from.0) * t;
        let y = from.1 + (to.1 - from.1) * t;
        post_mouse(drag_button, x, y, cg_button)?;
        cancellable_sleep(Duration::from_millis(8), cancel)?;
    }
    post_mouse(up, to.0, to.1, cg_button)
}

pub(super) fn pointer_scroll(
    point: Option<(f64, f64)>,
    steps: i32,
    cancel: &AtomicBool,
) -> Result<(), String> {
    ensure_input_access()?;
    if cancel.load(Ordering::Relaxed) {
        return Err("已取消".into());
    }
    if let Some((x, y)) = point {
        pointer_move(x, y)?;
    }
    let event = unsafe {
        CGEventCreateScrollWheelEvent2(ptr::null(), K_CG_SCROLL_EVENT_UNIT_LINE, 1, -steps, 0, 0)
    };
    post_event(event, "创建 macOS 滚轮事件失败")
}

pub(super) fn type_text(text: &str, cancel: &AtomicBool) -> Result<(), String> {
    ensure_input_access()?;
    let mut chunk = Vec::with_capacity(20);
    for character in text.chars() {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".into());
        }
        let mut encoded = [0u16; 2];
        let units = character.encode_utf16(&mut encoded);
        if chunk.len() + units.len() > 20 {
            post_unicode(&chunk)?;
            chunk.clear();
        }
        chunk.extend_from_slice(units);
    }
    post_unicode(&chunk)
}

pub(super) fn press_key(keys: &str, cancel: &AtomicBool) -> Result<(), String> {
    ensure_input_access()?;
    if cancel.load(Ordering::Relaxed) {
        return Err("已取消".into());
    }
    let parts: Vec<&str> = keys
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() || parts.len() > 8 {
        return Err("keys 格式错误，例如 CMD+L、CTRL+C 或 Return".into());
    }
    let mut flags = 0u64;
    for modifier in &parts[..parts.len() - 1] {
        flags |= match modifier.to_ascii_lowercase().as_str() {
            "shift" => 1 << 17,
            "ctrl" | "control" => 1 << 18,
            "alt" | "option" => 1 << 19,
            "cmd" | "command" | "meta" | "super" => 1 << 20,
            other => return Err(format!("不支持的 macOS 修饰键 {other}")),
        };
    }
    let keycode = mac_keycode(parts.last().copied().unwrap())?;
    for down in [true, false] {
        let event = unsafe { CGEventCreateKeyboardEvent(ptr::null(), keycode, down) };
        if event.is_null() {
            return Err("创建 macOS 键盘事件失败；请检查辅助功能授权".into());
        }
        unsafe {
            CGEventSetFlags(event, flags);
            CGEventPost(K_CG_HID_EVENT_TAP, event);
            CFRelease(event.cast());
        }
    }
    Ok(())
}

fn displays() -> Result<Vec<(u32, Rect, (usize, usize))>, String> {
    // CGGetActiveDisplayList 在 max_displays=0 时不会像常见的 C API 那样只返回数量；
    // 直接给一个合理上限的固定缓冲区，避免把“成功但 count=0”误报成无显示器。
    let mut ids = vec![0u32; 32];
    let mut count = 0u32;
    let status = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if status != 0 || count == 0 {
        return Err(format!("读取 macOS 显示器失败: CGError {status}"));
    }
    ids.truncate(count as usize);
    Ok(ids
        .into_iter()
        .map(|id| {
            let value = unsafe { CGDisplayBounds(id) };
            (
                id,
                Rect {
                    x: value.origin.x,
                    y: value.origin.y,
                    width: value.size.width,
                    height: value.size.height,
                },
                display_pixel_size(id),
            )
        })
        .collect())
}

fn display_pixel_size(id: u32) -> (usize, usize) {
    unsafe {
        let mode = CGDisplayCopyDisplayMode(id);
        if !mode.is_null() {
            let size = (
                CGDisplayModeGetPixelWidth(mode),
                CGDisplayModeGetPixelHeight(mode),
            );
            CFRelease(mode);
            if size.0 > 0 && size.1 > 0 {
                return size;
            }
        }
        (CGDisplayPixelsWide(id), CGDisplayPixelsHigh(id))
    }
}

fn display_bounds(displays: &[(u32, Rect, (usize, usize))]) -> Option<Rect> {
    layout_bounds(
        &displays
            .iter()
            .map(|(id, rect, _)| (id.to_string(), *rect))
            .collect(),
    )
}

fn window_list() -> Result<Vec<MacWindow>, String> {
    let options =
        K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY | K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS;
    let array = unsafe { CGWindowListCopyWindowInfo(options, 0) };
    if array.is_null() {
        return Err("WindowServer 没有返回窗口清单；当前进程可能不在图形会话中".into());
    }
    let mut windows = Vec::new();
    let count = unsafe { CFArrayGetCount(array) };
    for index in 0..count {
        let dict = unsafe { CFArrayGetValueAtIndex(array, index) } as CFDictionaryRef;
        let id = unsafe { dict_i64(dict, kCGWindowNumber) }.unwrap_or(0);
        let pid = unsafe { dict_i64(dict, kCGWindowOwnerPID) }.unwrap_or(0);
        let layer = unsafe { dict_i64(dict, kCGWindowLayer) }.unwrap_or(-1);
        if id <= 0 || pid <= 0 || layer != 0 {
            continue;
        }
        let mut bounds = CGRect::default();
        let bounds_value = unsafe { dict_value(dict, kCGWindowBounds) };
        if bounds_value.is_null()
            || !unsafe { CGRectMakeWithDictionaryRepresentation(bounds_value.cast(), &mut bounds) }
        {
            continue;
        }
        windows.push(MacWindow {
            window_id: id as u64,
            pid,
            owner: unsafe { dict_string(dict, kCGWindowOwnerName) }.unwrap_or_default(),
            title: unsafe { dict_string(dict, kCGWindowName) }.unwrap_or_default(),
            x: bounds.origin.x,
            y: bounds.origin.y,
            width: bounds.size.width,
            height: bounds.size.height,
        });
    }
    unsafe { CFRelease(array) };
    Ok(windows)
}

unsafe fn dict_value(dict: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef {
    let mut value = ptr::null();
    if CFDictionaryGetValueIfPresent(dict, key, &mut value) {
        value
    } else {
        ptr::null()
    }
}

unsafe fn dict_i64(dict: CFDictionaryRef, key: CFTypeRef) -> Option<i64> {
    let value = dict_value(dict, key);
    let mut number = 0i64;
    (!value.is_null()
        && CFNumberGetValue(
            value,
            K_CF_NUMBER_SINT64_TYPE,
            (&mut number as *mut i64).cast(),
        ))
    .then_some(number)
}

unsafe fn dict_string(dict: CFDictionaryRef, key: CFTypeRef) -> Option<String> {
    let value = dict_value(dict, key) as CFStringRef;
    if value.is_null() {
        return None;
    }
    let mut buffer = vec![0 as c_char; 4096];
    if !CFStringGetCString(
        value,
        buffer.as_mut_ptr(),
        buffer.len() as isize,
        K_CF_STRING_ENCODING_UTF8,
    ) {
        return None;
    }
    Some(
        CStr::from_ptr(buffer.as_ptr())
            .to_string_lossy()
            .into_owned(),
    )
}

fn capture_program() -> &'static str {
    if Path::new("/usr/sbin/screencapture").is_file() {
        "/usr/sbin/screencapture"
    } else {
        "screencapture"
    }
}

fn ensure_input_access() -> Result<(), String> {
    let authorized = unsafe { CGPreflightPostEventAccess() || CGRequestPostEventAccess() };
    if authorized {
        Ok(())
    } else {
        Err("macOS 尚未允许控制键盘和鼠标。请在“隐私与安全性 → 辅助功能”中启用 YujialeCode/启动它的终端，然后重试".into())
    }
}

fn rect_arg(rect: Rect) -> String {
    format!(
        "{},{},{},{}",
        rect.x.floor() as i64,
        rect.y.floor() as i64,
        rect.width.ceil().max(1.0) as u64,
        rect.height.ceil().max(1.0) as u64
    )
}

fn post_mouse(event_type: u32, x: f64, y: f64, button: u32) -> Result<(), String> {
    let event =
        unsafe { CGEventCreateMouseEvent(ptr::null(), event_type, CGPoint { x, y }, button) };
    post_event(event, "创建 macOS 鼠标事件失败；请检查辅助功能授权")
}

fn post_event(event: CGEventRef, error: &str) -> Result<(), String> {
    if event.is_null() {
        return Err(error.into());
    }
    unsafe {
        CGEventPost(K_CG_HID_EVENT_TAP, event);
        CFRelease(event.cast());
    }
    Ok(())
}

fn post_unicode(text: &[u16]) -> Result<(), String> {
    if text.is_empty() {
        return Ok(());
    }
    for key_down in [true, false] {
        let event = unsafe { CGEventCreateKeyboardEvent(ptr::null(), 0, key_down) };
        if event.is_null() {
            return Err("创建 macOS 文字输入事件失败；请检查辅助功能授权".into());
        }
        unsafe {
            CGEventKeyboardSetUnicodeString(event, text.len(), text.as_ptr());
            CGEventPost(K_CG_HID_EVENT_TAP, event);
            CFRelease(event.cast());
        }
    }
    Ok(())
}

fn mouse_types(button: u32, drag: bool) -> Result<(u32, u32, u32), String> {
    match button {
        0x110 => Ok(if drag { (1, 2, 6) } else { (1, 2, 0) }),
        0x111 => Ok(if drag { (3, 4, 7) } else { (3, 4, 1) }),
        0x112 => Ok(if drag { (25, 26, 27) } else { (25, 26, 2) }),
        _ => Err("macOS 不支持这个鼠标键".into()),
    }
}

fn mac_keycode(key: &str) -> Result<u16, String> {
    let normalized = key.to_ascii_lowercase();
    let code = match normalized.as_str() {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "=" | "equal" => 24,
        "9" => 25,
        "7" => 26,
        "-" | "minus" => 27,
        "8" => 28,
        "0" => 29,
        "]" => 30,
        "o" => 31,
        "u" => 32,
        "[" => 33,
        "i" => 34,
        "p" => 35,
        "return" | "enter" => 36,
        "l" => 37,
        "j" => 38,
        "'" => 39,
        "k" => 40,
        ";" => 41,
        "\\" => 42,
        "," => 43,
        "/" => 44,
        "n" => 45,
        "m" => 46,
        "." => 47,
        "tab" => 48,
        "space" => 49,
        "`" => 50,
        "backspace" => 51,
        "escape" | "esc" => 53,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        "home" => 115,
        "end" => 119,
        "pageup" | "pgup" => 116,
        "pagedown" | "pgdn" => 121,
        "left" | "arrowleft" => 123,
        "right" | "arrowright" => 124,
        "down" | "arrowdown" => 125,
        "up" | "arrowup" => 126,
        "delete" => 117,
        _ => return Err(format!("不支持的 macOS 按键 {key}")),
    };
    Ok(code)
}
