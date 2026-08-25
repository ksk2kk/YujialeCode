use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::Config;

const IMAGE_MARKER_PREFIX: &str = "[[YJLCODER_IMAGE:";
const IMAGE_MARKER_SUFFIX: &str = "]]";
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(4);
#[cfg(target_os = "linux")]
const INPUT_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_CAPTURE_BYTES: u64 = 24 * 1024 * 1024;
const INTER_ACTION_DELAY: Duration = Duration::from_millis(120);
const BATCH_EXECUTION_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_ACTIONS_PER_BATCH: usize = 50;
const MAX_RAW_ACTIONS_PER_BATCH: usize = 200;
static FRAME_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct FrameMeta {
    frame_id: String,
    image_path: String,
    target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<String>,
    pixel_width: u32,
    pixel_height: u32,
    logical_x: f64,
    logical_y: f64,
    logical_width: f64,
    logical_height: f64,
    layout_min_x: f64,
    layout_min_y: f64,
    layout_width: f64,
    layout_height: f64,
    clickable: bool,
    captured_unix_ms: u128,
    /// 只有真正改变桌面状态的动作才递增。单纯多截一张图不会让旧图失效，
    /// 因此模型可以先看全屏、再看局部，最后仍用全屏 frame_id 点击。
    #[serde(default)]
    generation: u64,
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl Rect {
    fn right(self) -> f64 {
        self.x + self.width
    }

    fn bottom(self) -> f64 {
        self.y + self.height
    }
}

#[derive(Debug)]
struct ProcessOutput {
    #[allow(dead_code)]
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: i32,
}

pub fn execute(
    cfg: &Config,
    session: &str,
    args: &Value,
    cancel: &AtomicBool,
    vision_available: bool,
) -> Result<String, String> {
    let normalized = normalize_computer_args(args)?;
    if normalized.get("actions").is_some() {
        return execute_batch(cfg, session, &normalized, cancel, vision_available);
    }
    execute_single(
        cfg,
        session,
        &normalized,
        cancel,
        vision_available,
        true,
        None,
    )
}

fn execute_single(
    cfg: &Config,
    session: &str,
    args: &Value,
    cancel: &AtomicBool,
    vision_available: bool,
    capture_after: bool,
    batch_frame: Option<&FrameMeta>,
) -> Result<String, String> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("observe")
        .trim()
        .to_ascii_lowercase();

    match action.as_str() {
        "capabilities" | "status" => capabilities(vision_available, cancel),
        "observe" | "screenshot" => capture(cfg, session, args, cancel, vision_available),
        "list_outputs" | "outputs" => platform_list_outputs(cancel),
        "list_windows" | "windows" => platform_list_windows(cancel),
        "focus_window" | "focus" => {
            let id = required_u64(args, "window_id")?;
            platform_focus_window(id, cancel)?;
            advance_ui_generation(cfg, session)?;
            action_result_or_capture(capture_after, "已聚焦窗口", || {
                capture(cfg, session, &json!({"target":"focused_output"}), cancel, vision_available)
            })
        }
        "click" | "double_click" | "double-click" => {
            let meta = frame_for_action(cfg, session, args, batch_frame)?;
            let (x, y) = required_point(args)?;
            let (desktop_x, desktop_y) = map_frame_point(&meta, x, y)?;
            let button = pointer_button(args.get("button").and_then(Value::as_str).unwrap_or("left"))?;
            let count = if action.starts_with("double") { 2 } else { 1 };
            pointer_click(&meta, desktop_x, desktop_y, button, count, cancel)?;
            advance_ui_generation(cfg, session)?;
            action_result_or_capture(capture_after, "点击完成", || {
                recapture(cfg, session, &meta, cancel, vision_available)
            })
        }
        "move" | "move_pointer" | "move-pointer" => {
            let meta = frame_for_action(cfg, session, args, batch_frame)?;
            let (x, y) = required_point(args)?;
            let (desktop_x, desktop_y) = map_frame_point(&meta, x, y)?;
            pointer_move(&meta, desktop_x, desktop_y, cancel)?;
            advance_ui_generation(cfg, session)?;
            action_result_or_capture(capture_after, "移动完成", || {
                recapture(cfg, session, &meta, cancel, vision_available)
            })
        }
        "drag" => {
            let meta = frame_for_action(cfg, session, args, batch_frame)?;
            let from_x = required_f64(args, "from_x")?;
            let from_y = required_f64(args, "from_y")?;
            let to_x = required_f64(args, "to_x")?;
            let to_y = required_f64(args, "to_y")?;
            let from = map_frame_point(&meta, from_x, from_y)?;
            let to = map_frame_point(&meta, to_x, to_y)?;
            let button = pointer_button(args.get("button").and_then(Value::as_str).unwrap_or("left"))?;
            pointer_drag(&meta, from, to, button, cancel)?;
            advance_ui_generation(cfg, session)?;
            action_result_or_capture(capture_after, "拖拽完成", || {
                recapture(cfg, session, &meta, cancel, vision_available)
            })
        }
        "scroll" => {
            let meta = frame_for_action(cfg, session, args, batch_frame)?;
            let steps = args.get("steps").and_then(Value::as_i64).unwrap_or(3).clamp(-50, 50) as i32;
            if steps == 0 {
                return Err("steps 不能为 0；正数向下，负数向上".into());
            }
            let point = match (args.get("x").and_then(Value::as_f64), args.get("y").and_then(Value::as_f64)) {
                (Some(x), Some(y)) => Some(map_frame_point(&meta, x, y)?),
                (None, None) => None,
                _ => return Err("scroll 的 x 和 y 必须同时提供".into()),
            };
            pointer_scroll(&meta, point, steps, cancel)?;
            advance_ui_generation(cfg, session)?;
            action_result_or_capture(capture_after, "滚动完成", || {
                recapture(cfg, session, &meta, cancel, vision_available)
            })
        }
        "type" | "type_text" | "type-text" => {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .ok_or("type_text 缺少 text")?;
            if text.len() > 20_000 {
                return Err("单次输入最多 20000 字节".into());
            }
            platform_type_text(text, cancel)?;
            advance_ui_generation(cfg, session)?;
            action_result_or_capture(capture_after, "文字输入完成", || {
                recapture_latest_or_focused(cfg, session, cancel, vision_available)
            })
        }
        "key" | "press_key" | "press-key" => {
            let keys = args
                .get("keys")
                .or_else(|| args.get("key"))
                .and_then(Value::as_str)
                .ok_or("press_key 缺少 keys，例如 CTRL+L 或 Return")?;
            platform_press_key(keys, cancel)?;
            advance_ui_generation(cfg, session)?;
            action_result_or_capture(capture_after, "按键完成", || {
                recapture_latest_or_focused(cfg, session, cancel, vision_available)
            })
        }
        "open_url" => {
            let url = args
                .get("url")
                .and_then(Value::as_str)
                .ok_or("open_url 缺少 url")?;
            validate_web_url(url)?;
            platform_open_url(url, cancel)?;
            advance_ui_generation(cfg, session)?;
            let wait_ms = args
                .get("wait_ms")
                .and_then(Value::as_u64)
                .unwrap_or(800)
                .min(10_000);
            cancellable_sleep(Duration::from_millis(wait_ms), cancel)?;
            action_result_or_capture(capture_after, "网址已打开", || {
                capture(
                    cfg,
                    session,
                    &json!({"target":"focused_output"}),
                    cancel,
                    vision_available,
                )
            })
        }
        "wait" => {
            let ms = args.get("ms").and_then(Value::as_u64).unwrap_or(500).min(10_000);
            cancellable_sleep(Duration::from_millis(ms), cancel)?;
            action_result_or_capture(capture_after, "等待完成", || {
                recapture_latest_or_focused(cfg, session, cancel, vision_available)
            })
        }
        _ => Err(format!(
            "未知 computer_use action: {action}。可用：capabilities, observe, list_outputs, list_windows, focus_window, open_url, click, double_click, move, drag, scroll, type_text, press_key, wait；也可传 actions 数组批量执行"
        )),
    }
}

/// 把不同模型常见的 CUA 方言收敛为内部唯一格式。兼容发生在代码层，
/// 不需要把一大页“如果 A 失败再改成 B”的说明塞进系统提示词。
fn normalize_computer_args(args: &Value) -> Result<Value, String> {
    let mut normalized = args.clone();
    let object = normalized
        .as_object_mut()
        .ok_or("computer_use 参数必须是 JSON 对象")?;
    normalize_action_object(object)?;

    if let Some(actions) = object.get_mut("actions") {
        let actions = actions.as_array_mut().ok_or("actions 必须是 JSON 数组")?;
        for (index, action) in actions.iter_mut().enumerate() {
            let child = action
                .as_object_mut()
                .ok_or_else(|| format!("actions[{index}] 必须是对象"))?;
            if child.contains_key("actions") {
                return Err("禁止嵌套 actions 批次".into());
            }
            normalize_action_object(child)?;
        }
    }
    Ok(normalized)
}

fn normalize_action_object(object: &mut serde_json::Map<String, Value>) -> Result<(), String> {
    if !object.contains_key("action") {
        if let Some(value) = object.get("type").cloned() {
            object.insert("action".into(), value);
        }
    }
    if let Some(action) = object.get("action").and_then(Value::as_str) {
        object.insert("action".into(), Value::String(canonical_action(action)));
    }

    copy_alias(object, "window_id", &["id", "window", "target_id"]);
    copy_alias(object, "text", &["value", "input", "content"]);
    copy_alias(object, "keys", &["key", "hotkey"]);
    copy_alias(object, "url", &["uri", "href"]);
    copy_alias(object, "from_x", &["start_x"]);
    copy_alias(object, "from_y", &["start_y"]);
    copy_alias(object, "to_x", &["end_x"]);
    copy_alias(object, "to_y", &["end_y"]);

    if object.get("action").and_then(Value::as_str) == Some("drag") {
        copy_alias(object, "from_x", &["x"]);
        copy_alias(object, "from_y", &["y"]);
    }

    let target = object
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if target == "region" && !object.contains_key("region") {
        let x = compatible_f64(object.get("x"));
        let y = compatible_f64(object.get("y"));
        let width = compatible_f64(object.get("width").or_else(|| object.get("w")));
        let height = compatible_f64(object.get("height").or_else(|| object.get("h")));
        if let (Some(x), Some(y), Some(width), Some(height)) = (x, y, width, height) {
            object.insert(
                "region".into(),
                Value::String(format!(
                    "{},{} {}x{}",
                    compact_number(x),
                    compact_number(y),
                    compact_number(width),
                    compact_number(height)
                )),
            );
        }
    }

    if object.get("action").and_then(Value::as_str) == Some("scroll")
        && !object.contains_key("steps")
    {
        let delta = ["scroll_y", "delta_y", "dy", "amount"]
            .iter()
            .find_map(|key| compatible_f64(object.get(*key)));
        if let Some(delta) = delta {
            let steps = if delta == 0.0 {
                0
            } else {
                (delta.abs() / 100.0).ceil().clamp(1.0, 50.0) as i64 * delta.signum() as i64
            };
            object.insert("steps".into(), Value::from(steps));
        }
    }

    for key in [
        "x", "y", "from_x", "from_y", "to_x", "to_y", "width", "height",
    ] {
        if let Some(number) = compatible_f64(object.get(key)) {
            if object.get(key).is_some_and(Value::is_string) {
                if let Some(number) = serde_json::Number::from_f64(number) {
                    object.insert(key.into(), Value::Number(number));
                }
            }
        }
    }
    for key in ["window_id", "ms", "wait_ms", "steps"] {
        if let Some(value) = object.get(key).and_then(Value::as_str) {
            if let Ok(number) = value.parse::<i64>() {
                object.insert(key.into(), Value::from(number));
            }
        }
    }
    Ok(())
}

fn canonical_action(action: &str) -> String {
    let normalized = action.trim().to_ascii_lowercase().replace('-', "_");
    match normalized.as_str() {
        "screenshot" | "screen_shot" | "capture" => "observe".into(),
        "click_element" | "click_at" | "tap" => "click".into(),
        "doubleclick" | "double_click_element" | "double_click_at" => "double_click".into(),
        "mousemove" | "mouse_move" | "move_mouse" | "move_pointer" => "move".into(),
        "keypress" | "key_press" | "hotkey" => "press_key".into(),
        "type" | "input_text" | "write_text" => "type_text".into(),
        "sleep" | "pause" | "delay" => "wait".into(),
        "navigate" | "goto" | "open" | "open_url" => "open_url".into(),
        "batch" | "multi_action" => "actions".into(),
        _ => normalized,
    }
}

fn copy_alias(object: &mut serde_json::Map<String, Value>, canonical: &str, aliases: &[&str]) {
    if object.contains_key(canonical) {
        return;
    }
    if let Some(value) = aliases.iter().find_map(|key| object.get(*key).cloned()) {
        object.insert(canonical.into(), value);
    }
}

fn compatible_f64(value: Option<&Value>) -> Option<f64> {
    value.and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
    })
}

fn compact_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

/// OpenAI CUA 的成熟执行模型：同一张画面上规划的一组动作必须顺序执行，
/// 中间不生成新 frame_id，最后只回传一次完整新画面。这样既节省视觉 token，
/// 也不会让批次中的第二个动作被我们自己的“旧截图保护”误伤。
fn execute_batch(
    cfg: &Config,
    session: &str,
    args: &Value,
    cancel: &AtomicBool,
    vision_available: bool,
) -> Result<String, String> {
    let raw_actions = args
        .get("actions")
        .and_then(Value::as_array)
        .ok_or("actions 必须是 JSON 数组")?;
    if raw_actions.is_empty() || raw_actions.len() > MAX_RAW_ACTIONS_PER_BATCH {
        return Err(format!(
            "actions 原始数量必须在 1..={MAX_RAW_ACTIONS_PER_BATCH} 之间；当前 {} 项",
            raw_actions.len()
        ));
    }
    let actions = compact_batch_actions(raw_actions);
    if actions.len() > MAX_ACTIONS_PER_BATCH {
        return Err(format!(
            "actions 归一化后仍有 {} 项，执行上限 {MAX_ACTIONS_PER_BATCH}，超出 {} 项；连续 move/wait 会自动合并，其余请拆批",
            actions.len(),
            actions.len() - MAX_ACTIONS_PER_BATCH
        ));
    }
    let needs_frame = actions.iter().any(action_uses_frame);
    let batch_frame = if needs_frame {
        Some(checked_frame(cfg, session, args)?)
    } else {
        load_latest_meta(cfg, session).ok()
    };

    let started = std::time::Instant::now();
    let mut names = Vec::with_capacity(actions.len());
    for (index, action) in actions.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(format!("批量动作在第 {} 项前被 Esc 取消", index + 1));
        }
        if started.elapsed() >= BATCH_EXECUTION_TIMEOUT {
            return Err(format!(
                "批量动作超过 {} 秒，在第 {} 项前终止；请拆成更小批次",
                BATCH_EXECUTION_TIMEOUT.as_secs(),
                index + 1
            ));
        }
        let name = action
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("actions[{}] 缺少 action/type", index))?;
        let normalized_name = name.trim().to_ascii_lowercase();
        if matches!(
            normalized_name.as_str(),
            "capabilities" | "status" | "list_outputs" | "outputs" | "list_windows" | "windows"
        ) {
            return Err(format!("批量动作中不能混入只读查询 {name}；请单独调用"));
        }
        execute_single(
            cfg,
            session,
            action,
            cancel,
            vision_available,
            false,
            batch_frame.as_ref(),
        )
        .map_err(|e| format!("批量动作第 {} 项 {name} 失败: {e}", index + 1))?;
        names.push(name.to_string());
        if index + 1 < actions.len() && normalized_name != "wait" {
            cancellable_sleep(INTER_ACTION_DELAY, cancel)?;
        }
    }
    let result = match batch_frame.as_ref() {
        Some(meta)
            if meta.clickable
                && (needs_frame || matches!(meta.target.as_str(), "output" | "all")) =>
        {
            recapture(cfg, session, meta, cancel, vision_available)
        }
        _ => capture(
            cfg,
            session,
            &json!({"target":"focused_output"}),
            cancel,
            vision_available,
        ),
    }?;
    let summary = summarize_action_names(&names);
    let compacted = raw_actions.len().saturating_sub(actions.len());
    Ok(format!(
        "批量成功: {}（执行 {} 项，合并 {} 项，{}ms）\n{}",
        summary,
        names.len(),
        compacted,
        started.elapsed().as_millis(),
        result
    ))
}

/// 鼠标轨迹通常只是模型在“画动画”，真正有意义的是最后落点。
/// 相邻等待也可安全合并；批次内截图统一折叠成结尾那一张。
fn compact_batch_actions(actions: &[Value]) -> Vec<Value> {
    let mut result: Vec<Value> = Vec::with_capacity(actions.len());
    for action in actions {
        let name = action.get("action").and_then(Value::as_str).unwrap_or("");
        if name == "observe" {
            continue;
        }
        if name == "move"
            && result
                .last()
                .and_then(|last| last.get("action"))
                .and_then(Value::as_str)
                == Some("move")
        {
            *result.last_mut().expect("last 已检查") = action.clone();
            continue;
        }
        if name == "wait"
            && result
                .last()
                .and_then(|last| last.get("action"))
                .and_then(Value::as_str)
                == Some("wait")
        {
            let previous = result
                .last()
                .and_then(|last| last.get("ms"))
                .and_then(Value::as_u64)
                .unwrap_or(500);
            let current = action.get("ms").and_then(Value::as_u64).unwrap_or(500);
            if let Some(last) = result.last_mut() {
                last["ms"] = Value::from(previous.saturating_add(current).min(10_000));
            }
            continue;
        }
        result.push(action.clone());
    }
    result
}

fn summarize_action_names(names: &[String]) -> String {
    if names.is_empty() {
        return "observe(final)".into();
    }
    let mut runs = Vec::new();
    let mut start = 0;
    while start < names.len() {
        let mut end = start + 1;
        while end < names.len() && names[end] == names[start] {
            end += 1;
        }
        let count = end - start;
        runs.push(if count == 1 {
            names[start].clone()
        } else {
            format!("{}×{count}", names[start])
        });
        start = end;
    }
    runs.join(" → ")
}

fn action_uses_frame(action: &Value) -> bool {
    matches!(
        action
            .get("action")
            .or_else(|| action.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "click"
            | "double_click"
            | "double-click"
            | "move"
            | "move_pointer"
            | "move-pointer"
            | "drag"
            | "scroll"
    )
}

fn frame_for_action(
    cfg: &Config,
    session: &str,
    args: &Value,
    batch_frame: Option<&FrameMeta>,
) -> Result<FrameMeta, String> {
    if let Some(meta) = batch_frame {
        if let Some(requested) = args.get("frame_id").and_then(Value::as_str) {
            if requested != meta.frame_id {
                return Err(format!(
                    "动作引用 frame_id {requested}，批次画面是 {}",
                    meta.frame_id
                ));
            }
        }
        return Ok(meta.clone());
    }
    checked_frame(cfg, session, args)
}

fn action_result_or_capture<F>(
    capture_after: bool,
    message: &str,
    capture: F,
) -> Result<String, String>
where
    F: FnOnce() -> Result<String, String>,
{
    if capture_after {
        capture()
    } else {
        Ok(message.to_string())
    }
}

pub fn split_image_marker(text: &str) -> (String, Option<String>) {
    let mut clean = Vec::new();
    let mut image = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(path) = trimmed
            .strip_prefix(IMAGE_MARKER_PREFIX)
            .and_then(|s| s.strip_suffix(IMAGE_MARKER_SUFFIX))
        {
            if !path.trim().is_empty() {
                image = Some(path.trim().to_string());
            }
        } else {
            clean.push(line);
        }
    }
    (clean.join("\n").trim_end().to_string(), image)
}

pub fn image_data_url(path: &str) -> Result<String, String> {
    let path = Path::new(path);
    let meta = fs::metadata(path).map_err(|e| format!("读取截图元数据失败: {e}"))?;
    if meta.len() == 0 || meta.len() > MAX_CAPTURE_BYTES {
        return Err(format!("截图大小异常: {} bytes", meta.len()));
    }
    let bytes = fs::read(path).map_err(|e| format!("读取截图失败: {e}"))?;
    let mime = match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    };
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

fn capabilities(vision_available: bool, cancel: &AtomicBool) -> Result<String, String> {
    let _ = cancel;
    #[cfg(target_os = "macos")]
    {
        return macos::capabilities(vision_available);
    }
    #[cfg(target_os = "windows")]
    {
        return windows::capabilities(vision_available);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = cancel;
        return Ok(
            json!({"available":false,"reason":"当前操作系统不受支持","vision":vision_available})
                .to_string(),
        );
    }

    #[cfg(target_os = "linux")]
    {
        install_desktop_env(cancel);
        let wayland = std::env::var("WAYLAND_DISPLAY").ok();
        let desktop = std::env::var("XDG_CURRENT_DESKTOP")
            .or_else(|_| std::env::var("XDG_SESSION_DESKTOP"))
            .unwrap_or_default();
        let pointer = linux_pointer::available().unwrap_or(false);
        Ok(serde_json::to_string_pretty(&json!({
            "available": wayland.is_some(),
            "session": "wayland",
            "desktop": desktop,
            "wayland_display": wayland,
            "capture": {
                "grim": command_exists("grim"),
                "scale": 1,
                "targets": ["focused_output", "output", "all", "region", "foreign_toplevel"]
            },
            "input": {
                "virtual_pointer": pointer,
                "wtype": command_exists("wtype"),
                "niri_ipc": command_exists("niri") && std::env::var_os("NIRI_SOCKET").is_some()
            },
            "vision": vision_available,
            "note": if vision_available {
                "截图会作为 image_url 直接送入本地模型；坐标按截图像素传回即可"
            } else {
                "当前模型未声明 vision；仍可截图给用户、列窗口和执行明确坐标动作，但模型不能自行看图"
            }
        }))
        .map_err(|e| e.to_string())?)
    }
}

fn capture(
    cfg: &Config,
    session: &str,
    args: &Value,
    cancel: &AtomicBool,
    vision_available: bool,
) -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        return macos::capture(cfg, session, args, cancel, vision_available);
    }
    #[cfg(target_os = "windows")]
    {
        return windows::capture(cfg, session, args, cancel, vision_available);
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (cfg, session, args, cancel, vision_available);
        return Err("当前操作系统不受支持".into());
    }

    #[cfg(target_os = "linux")]
    {
        install_desktop_env(cancel);
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err("没有找到 WAYLAND_DISPLAY；请在图形会话中启动 YujialeCode，或把桌面环境导入 systemd --user".into());
        }
        if !command_exists("grim") {
            return Err("缺少 grim，无法使用 Wayland 原生截图。请安装 grim 后重试".into());
        }

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

        let outputs = niri_outputs(cancel).unwrap_or_default();
        let bounds = layout_bounds(&outputs).unwrap_or(Rect {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        });

        let mut grim_args = vec![
            "-s".to_string(),
            "1".to_string(),
            "-l".to_string(),
            "1".to_string(),
        ];
        let (target, output, logical, clickable) = match requested.as_str() {
            "focused" | "focused_output" | "focused-output" => {
                if let Some((name, rect)) = niri_focused_output(cancel)? {
                    grim_args.extend(["-o".into(), name.clone()]);
                    ("output".to_string(), Some(name), rect, true)
                } else {
                    ("all".to_string(), None, bounds, true)
                }
            }
            "output" => {
                let name = args
                    .get("output")
                    .and_then(Value::as_str)
                    .ok_or("target=output 时必须提供 output")?
                    .to_string();
                let rect = outputs
                    .get(&name)
                    .copied()
                    .ok_or_else(|| format!("没有找到输出 {name}；先调用 list_outputs"))?;
                grim_args.extend(["-o".into(), name.clone()]);
                ("output".to_string(), Some(name), rect, true)
            }
            "all" | "desktop" => ("all".to_string(), None, bounds, true),
            "region" => {
                let geometry = args
                    .get("region")
                    .and_then(Value::as_str)
                    .ok_or("target=region 时必须提供 region，例如 10,20 800x600")?;
                let rect = parse_geometry(geometry)?;
                grim_args.extend(["-g".into(), geometry.to_string()]);
                ("region".to_string(), None, rect, true)
            }
            "window" => {
                if let Some(id) = args.get("window_id").and_then(Value::as_u64) {
                    // Niri 的数字 window_id 不是 grim 的 foreign-toplevel identifier。
                    // 最稳妥的兼容方式是先精确聚焦，再截取它所在的输出；截图可直接点击。
                    platform_focus_window(id, cancel)?;
                    advance_ui_generation(cfg, session)?;
                    let (name, rect) = niri_focused_output(cancel)?
                        .ok_or("窗口已聚焦，但 Niri 没有返回 focused-output")?;
                    grim_args.extend(["-o".into(), name.clone()]);
                    ("output".to_string(), Some(name), rect, true)
                } else {
                    let identifier = args
                        .get("window_identifier")
                        .or_else(|| args.get("identifier"))
                        .and_then(Value::as_str)
                        .ok_or("target=window 需要 window_id（来自 list_windows）")?;
                    grim_args.extend(["-T".into(), identifier.to_string()]);
                    (
                        "foreign_toplevel".to_string(),
                        None,
                        Rect {
                            x: 0.0,
                            y: 0.0,
                            width: 1.0,
                            height: 1.0,
                        },
                        false,
                    )
                }
            }
            "foreign_toplevel" | "foreign-toplevel" => {
                let identifier = args
                    .get("window_identifier")
                    .or_else(|| args.get("identifier"))
                    .and_then(Value::as_str)
                    .ok_or("target=foreign_toplevel 需要 window_identifier")?;
                grim_args.extend(["-T".into(), identifier.to_string()]);
                (
                    "foreign_toplevel".to_string(),
                    None,
                    Rect {
                        x: 0.0,
                        y: 0.0,
                        width: 1.0,
                        height: 1.0,
                    },
                    false,
                )
            }
            other => return Err(format!("未知截图 target: {other}")),
        };
        if args.get("cursor").and_then(Value::as_bool).unwrap_or(false) {
            grim_args.push("-c".into());
        }
        grim_args.push(path.to_string_lossy().into_owned());
        let arg_refs: Vec<&str> = grim_args.iter().map(String::as_str).collect();
        let output_result = run_bounded("grim", &arg_refs, CAPTURE_TIMEOUT, cancel, true)?;
        if output_result.code != 0 {
            return Err(format!(
                "grim 截图失败（exit {}）: {}",
                output_result.code,
                String::from_utf8_lossy(&output_result.stderr).trim()
            ));
        }
        let (pixel_width, pixel_height) = png_size(&path)?;
        let logical = if target == "foreign_toplevel" {
            Rect {
                x: 0.0,
                y: 0.0,
                width: pixel_width as f64,
                height: pixel_height as f64,
            }
        } else {
            logical
        };
        let effective_bounds = if bounds.width <= 1.0 && bounds.height <= 1.0 {
            Rect {
                x: logical.x.min(0.0),
                y: logical.y.min(0.0),
                width: logical.right().max(pixel_width as f64),
                height: logical.bottom().max(pixel_height as f64),
            }
        } else {
            bounds
        };
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
            layout_min_x: effective_bounds.x,
            layout_min_y: effective_bounds.y,
            layout_width: effective_bounds.width,
            layout_height: effective_bounds.height,
            clickable,
            captured_unix_ms: unix_ms(),
            generation: current_ui_generation(cfg, session),
        };
        save_latest_meta(cfg, session, &meta)?;
        prune_old_frames(&dir, 8);
        Ok(frame_result(&meta, vision_available))
    }
}

fn recapture(
    cfg: &Config,
    session: &str,
    meta: &FrameMeta,
    cancel: &AtomicBool,
    vision_available: bool,
) -> Result<String, String> {
    cancellable_sleep(Duration::from_millis(120), cancel)?;
    let args = match meta.target.as_str() {
        "output" => json!({"target":"output", "output":meta.output}),
        "all" => json!({"target":"all"}),
        "region" => json!({
            "target":"region",
            "region":format!("{},{} {}x{}", meta.logical_x, meta.logical_y, meta.logical_width, meta.logical_height)
        }),
        _ => json!({"target":"focused_output"}),
    };
    capture(cfg, session, &args, cancel, vision_available)
}

fn recapture_latest_or_focused(
    cfg: &Config,
    session: &str,
    cancel: &AtomicBool,
    vision_available: bool,
) -> Result<String, String> {
    match load_latest_meta(cfg, session) {
        // 局部截图是一次性放大镜。打字、按键、等待后继续截同一小块会让模型误以为
        // 整个桌面只有这一块；窗口裁剪也可能在窗口移动后失去可靠原点。
        Ok(meta) if meta.clickable && matches!(meta.target.as_str(), "output" | "all") => {
            recapture(cfg, session, &meta, cancel, vision_available)
        }
        _ => capture(
            cfg,
            session,
            &json!({"target":"focused_output"}),
            cancel,
            vision_available,
        ),
    }
}

fn frame_result(meta: &FrameMeta, vision_available: bool) -> String {
    let image_note = if vision_available {
        "image=attached".to_string()
    } else {
        format!("image={}", meta.image_path)
    };
    format!(
        "frame_id={} size={}x{} target={} output={} rect={},{} {}x{} clickable={} {}\n坐标=本图像素；后续动作携带 frame_id\n{}{}{}",
        meta.frame_id,
        meta.pixel_width,
        meta.pixel_height,
        meta.target,
        meta.output.as_deref().unwrap_or("all"),
        meta.logical_x,
        meta.logical_y,
        meta.logical_width,
        meta.logical_height,
        meta.clickable,
        image_note,
        IMAGE_MARKER_PREFIX,
        meta.image_path,
        IMAGE_MARKER_SUFFIX,
    )
}

fn checked_frame(cfg: &Config, session: &str, args: &Value) -> Result<FrameMeta, String> {
    let latest = load_latest_meta(cfg, session)?;
    let meta = match args.get("frame_id").and_then(Value::as_str) {
        Some(requested) if requested != latest.frame_id => load_frame_meta(cfg, session, requested)
            .map_err(|_| format!("找不到 frame_id {requested}；请重新 observe"))?,
        _ => latest,
    };
    let generation = current_ui_generation(cfg, session);
    if meta.generation != generation {
        return Err(format!(
            "frame_id {} 已过期（画面代次 {}，当前 {}）；请依据最新截图重新定位",
            meta.frame_id, meta.generation, generation
        ));
    }
    if !meta.clickable {
        return Err("这张窗口直截图没有可靠桌面原点，禁止猜坐标点击；请 observe target=focused_output 后再点击".into());
    }
    if !Path::new(&meta.image_path).exists() {
        return Err("截图文件已不存在，请重新 observe".into());
    }
    Ok(meta)
}

fn map_frame_point(meta: &FrameMeta, x: f64, y: f64) -> Result<(f64, f64), String> {
    if !x.is_finite() || !y.is_finite() {
        return Err("坐标必须是有限数字".into());
    }
    if x < 0.0 || y < 0.0 || x >= meta.pixel_width as f64 || y >= meta.pixel_height as f64 {
        return Err(format!(
            "坐标 ({x},{y}) 超出截图 {}x{}",
            meta.pixel_width, meta.pixel_height
        ));
    }
    let logical_x = meta.logical_x + (x + 0.5) * meta.logical_width / meta.pixel_width as f64;
    let logical_y = meta.logical_y + (y + 0.5) * meta.logical_height / meta.pixel_height as f64;
    Ok((logical_x, logical_y))
}

fn pointer_move(meta: &FrameMeta, x: f64, y: f64, cancel: &AtomicBool) -> Result<(), String> {
    let _ = meta;
    if cancel.load(Ordering::Relaxed) {
        return Err("已取消".into());
    }
    #[cfg(target_os = "linux")]
    {
        install_desktop_env(cancel);
        linux_pointer::move_absolute(meta, x, y)
    }
    #[cfg(target_os = "macos")]
    {
        macos::pointer_move(x, y)
    }
    #[cfg(target_os = "windows")]
    {
        windows::pointer_move(meta, x, y)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (meta, x, y);
        Err("当前操作系统不受支持".into())
    }
}

fn pointer_click(
    meta: &FrameMeta,
    x: f64,
    y: f64,
    button: u32,
    count: usize,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let _ = meta;
    #[cfg(target_os = "linux")]
    {
        install_desktop_env(cancel);
        linux_pointer::click(meta, x, y, button, count, cancel)
    }
    #[cfg(target_os = "macos")]
    {
        macos::pointer_click(x, y, button, count, cancel)
    }
    #[cfg(target_os = "windows")]
    {
        windows::pointer_click(meta, x, y, button, count, cancel)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (meta, x, y, button, count, cancel);
        Err("当前操作系统不受支持".into())
    }
}

fn pointer_drag(
    meta: &FrameMeta,
    from: (f64, f64),
    to: (f64, f64),
    button: u32,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let _ = meta;
    #[cfg(target_os = "linux")]
    {
        install_desktop_env(cancel);
        linux_pointer::drag(meta, from, to, button, cancel)
    }
    #[cfg(target_os = "macos")]
    {
        macos::pointer_drag(from, to, button, cancel)
    }
    #[cfg(target_os = "windows")]
    {
        windows::pointer_drag(meta, from, to, button, cancel)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (meta, from, to, button, cancel);
        Err("当前操作系统不受支持".into())
    }
}

fn pointer_scroll(
    meta: &FrameMeta,
    point: Option<(f64, f64)>,
    steps: i32,
    cancel: &AtomicBool,
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        install_desktop_env(cancel);
        linux_pointer::scroll(meta, point, steps, cancel)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = meta;
        macos::pointer_scroll(point, steps, cancel)
    }
    #[cfg(target_os = "windows")]
    {
        windows::pointer_scroll(meta, point, steps, cancel)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (meta, point, steps, cancel);
        Err("当前操作系统不受支持".into())
    }
}

fn platform_type_text(text: &str, cancel: &AtomicBool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    return wtype_text(text, cancel);
    #[cfg(target_os = "macos")]
    return macos::type_text(text, cancel);
    #[cfg(target_os = "windows")]
    return windows::type_text(text, cancel);
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (text, cancel);
        Err("当前操作系统不受支持".into())
    }
}

fn platform_press_key(keys: &str, cancel: &AtomicBool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    return wtype_key(keys, cancel);
    #[cfg(target_os = "macos")]
    return macos::press_key(keys, cancel);
    #[cfg(target_os = "windows")]
    return windows::press_key(keys, cancel);
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (keys, cancel);
        Err("当前操作系统不受支持".into())
    }
}

fn validate_web_url(url: &str) -> Result<(), String> {
    if url.len() > 4096
        || url
            .chars()
            .any(|c| c.is_ascii_control() || c.is_whitespace())
        || !(url.starts_with("https://") || url.starts_with("http://"))
    {
        return Err("url 必须是长度不超过 4096 的 http:// 或 https:// 地址，且不能含空白".into());
    }
    Ok(())
}

fn platform_open_url(url: &str, cancel: &AtomicBool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        install_desktop_env(cancel);
        if !command_exists("xdg-open") {
            return Err("缺少 xdg-open，无法打开网址".into());
        }
        return command_success(
            "xdg-open",
            run_bounded("xdg-open", &[url], Duration::from_secs(5), cancel, false)?,
        );
    }
    #[cfg(target_os = "macos")]
    {
        return command_success(
            "open",
            run_bounded("open", &[url], Duration::from_secs(5), cancel, false)?,
        );
    }
    #[cfg(target_os = "windows")]
    {
        return command_success(
            "rundll32",
            run_bounded(
                "rundll32.exe",
                &["url.dll,FileProtocolHandler", url],
                Duration::from_secs(5),
                cancel,
                false,
            )?,
        );
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (url, cancel);
        Err("当前操作系统不受支持".into())
    }
}

fn platform_list_outputs(cancel: &AtomicBool) -> Result<String, String> {
    let _ = cancel;
    #[cfg(target_os = "linux")]
    return summarize_niri_outputs(&niri_query("outputs", cancel)?);
    #[cfg(target_os = "macos")]
    return macos::list_outputs();
    #[cfg(target_os = "windows")]
    return windows::list_outputs();
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = cancel;
        Err("当前操作系统不受支持".into())
    }
}

fn platform_list_windows(cancel: &AtomicBool) -> Result<String, String> {
    #[cfg(target_os = "linux")]
    return summarize_niri_windows(&niri_query("windows", cancel)?);
    #[cfg(target_os = "macos")]
    return macos::list_windows(cancel);
    #[cfg(target_os = "windows")]
    return windows::list_windows();
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = cancel;
        Err("当前操作系统不受支持".into())
    }
}

fn platform_focus_window(id: u64, cancel: &AtomicBool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        run_niri_action(&["focus-window", "--id", &id.to_string()], cancel)?;
        cancellable_sleep(Duration::from_millis(60), cancel)?;
        let raw = niri_query("focused-window", cancel)?;
        let focused: Value = serde_json::from_str(&raw)
            .map_err(|e| format!("解析 Niri focused-window 失败: {e}"))?;
        let actual = focused.get("id").and_then(Value::as_u64);
        return match actual {
            Some(actual) if actual == id => Ok(()),
            Some(actual) => Err(format!("Niri 未聚焦窗口 {id}，当前仍是 {actual}")),
            None => Err(format!("Niri 没有找到或无法聚焦窗口 {id}")),
        };
    }
    #[cfg(target_os = "macos")]
    return macos::focus_window(id, cancel);
    #[cfg(target_os = "windows")]
    return windows::focus_window(id);
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (id, cancel);
        Err("当前操作系统不受支持".into())
    }
}

#[cfg(target_os = "linux")]
fn wtype_text(text: &str, cancel: &AtomicBool) -> Result<(), String> {
    install_desktop_env(cancel);
    if !command_exists("wtype") {
        return Err("缺少 wtype，无法向 Wayland 窗口输入文字".into());
    }
    let out = run_bounded("wtype", &["--", text], INPUT_TIMEOUT, cancel, false)?;
    command_success("wtype", out)
}

#[cfg(target_os = "linux")]
fn wtype_key(keys: &str, cancel: &AtomicBool) -> Result<(), String> {
    install_desktop_env(cancel);
    if !command_exists("wtype") {
        return Err("缺少 wtype，无法发送 Wayland 键盘事件".into());
    }
    let parts: Vec<&str> = keys
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() || parts.len() > 8 {
        return Err("keys 格式错误，例如 CTRL+L、ALT+F4 或 Return".into());
    }
    let key = parts.last().copied().unwrap_or("");
    let mut owned = Vec::<String>::new();
    for modifier in &parts[..parts.len() - 1] {
        owned.push("-M".into());
        owned.push(normalize_modifier(modifier)?.into());
    }
    owned.push("-k".into());
    owned.push(key.into());
    for modifier in parts[..parts.len() - 1].iter().rev() {
        owned.push("-m".into());
        owned.push(normalize_modifier(modifier)?.into());
    }
    let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
    let out = run_bounded("wtype", &refs, INPUT_TIMEOUT, cancel, false)?;
    command_success("wtype", out)
}

#[cfg(target_os = "linux")]
fn normalize_modifier(value: &str) -> Result<&'static str, String> {
    match value.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Ok("ctrl"),
        "shift" => Ok("shift"),
        "alt" | "option" => Ok("alt"),
        "super" | "logo" | "meta" | "win" => Ok("logo"),
        _ => Err(format!("不支持的修饰键 {value}；支持 CTRL/SHIFT/ALT/SUPER")),
    }
}

#[cfg(target_os = "linux")]
fn niri_query(command: &str, cancel: &AtomicBool) -> Result<String, String> {
    install_desktop_env(cancel);
    if !command_exists("niri") || std::env::var_os("NIRI_SOCKET").is_none() {
        return Err("当前桌面没有可用的 Niri IPC".into());
    }
    let out = run_bounded(
        "niri",
        &["msg", "--json", command],
        INPUT_TIMEOUT,
        cancel,
        false,
    )?;
    if out.code != 0 {
        return Err(format!(
            "niri msg {command} 失败: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("Niri 返回非 UTF-8: {e}"))
}

#[cfg(target_os = "linux")]
fn run_niri_action(args: &[&str], cancel: &AtomicBool) -> Result<(), String> {
    install_desktop_env(cancel);
    if !command_exists("niri") || std::env::var_os("NIRI_SOCKET").is_none() {
        return Err("当前桌面没有可用的 Niri IPC".into());
    }
    let mut full = vec!["msg", "action"];
    full.extend_from_slice(args);
    let out = run_bounded("niri", &full, INPUT_TIMEOUT, cancel, false)?;
    command_success("niri", out)
}

fn command_success(name: &str, output: ProcessOutput) -> Result<(), String> {
    if output.code == 0 {
        Ok(())
    } else {
        Err(format!(
            "{name} 失败（exit {}）: {}{}",
            output.code,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn run_bounded(
    program: &str,
    args: &[&str],
    timeout: Duration,
    cancel: &AtomicBool,
    quiet_stdout: bool,
) -> Result<ProcessOutput, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(if quiet_stdout {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 {program} 失败: {e}"))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || read_pipe(stdout));
    let stderr_reader = std::thread::spawn(move || read_pipe(stderr));
    let started = std::time::Instant::now();
    let code = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(format!("{program} 已被 Esc 取消"));
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(format!(
                "{program} 超过 {}ms，已终止；不会阻塞 Agent",
                timeout.as_millis()
            ));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                let _ = child.kill();
                return Err(format!("等待 {program} 失败: {e}"));
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    Ok(ProcessOutput {
        stdout,
        stderr,
        code,
    })
}

fn read_pipe(pipe: Option<impl Read>) -> Vec<u8> {
    let mut data = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.by_ref().take(4 * 1024 * 1024).read_to_end(&mut data);
    }
    data
}

#[cfg(target_os = "linux")]
fn install_desktop_env(cancel: &AtomicBool) {
    let wanted = [
        "WAYLAND_DISPLAY",
        "XDG_RUNTIME_DIR",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_DESKTOP",
        "NIRI_SOCKET",
        "DISPLAY",
    ];
    if wanted.iter().all(|key| std::env::var_os(key).is_some()) {
        return;
    }
    let Ok(output) = run_bounded(
        "systemctl",
        &["--user", "show-environment"],
        Duration::from_secs(2),
        cancel,
        false,
    ) else {
        return;
    };
    if output.code != 0 {
        return;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if wanted.contains(&key) && std::env::var_os(key).is_none() && !value.is_empty() {
            std::env::set_var(key, value);
        }
    }
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn niri_outputs(cancel: &AtomicBool) -> Result<BTreeMap<String, Rect>, String> {
    let raw = niri_query("outputs", cancel)?;
    let value: Value =
        serde_json::from_str(&raw).map_err(|e| format!("解析 Niri outputs 失败: {e}"))?;
    let mut outputs = BTreeMap::new();
    if let Some(map) = value.as_object() {
        for (name, output) in map {
            if let Some(rect) = json_logical_rect(output) {
                outputs.insert(name.clone(), rect);
            }
        }
    }
    Ok(outputs)
}

fn summarize_niri_outputs(raw: &str) -> Result<String, String> {
    let value: Value =
        serde_json::from_str(raw).map_err(|e| format!("解析 Niri outputs 失败: {e}"))?;
    let mut concise = Vec::new();
    for (name, output) in value.as_object().into_iter().flatten() {
        let current_mode = output
            .get("current_mode")
            .and_then(Value::as_u64)
            .and_then(|index| output.get("modes")?.get(index as usize))
            .map(|mode| {
                json!({
                    "width": mode.get("width"),
                    "height": mode.get("height"),
                    "refresh_hz": mode.get("refresh_rate").and_then(Value::as_f64).map(|v| v / 1000.0)
                })
            });
        let logical = output.get("logical").map(|logical| {
            json!({
                "x": logical.get("x"),
                "y": logical.get("y"),
                "width": logical.get("width"),
                "height": logical.get("height"),
                "scale": logical.get("scale")
            })
        });
        concise.push(json!({
            "name": name,
            "make": output.get("make"),
            "model": output.get("model"),
            "physical_size_mm": output.get("physical_size"),
            "current_mode": current_mode,
            "logical": logical
        }));
    }
    serde_json::to_string(&concise).map_err(|e| e.to_string())
}

fn summarize_niri_windows(raw: &str) -> Result<String, String> {
    let value: Value =
        serde_json::from_str(raw).map_err(|e| format!("解析 Niri windows 失败: {e}"))?;
    let mut concise: Vec<Value> = value
        .as_array()
        .into_iter()
        .flatten()
        .map(|window| {
            json!({
                "window_id": window.get("id"),
                "title": window.get("title"),
                "app_id": window.get("app_id"),
                "pid": window.get("pid"),
                "workspace_id": window.get("workspace_id"),
                "focused": window.get("is_focused").and_then(Value::as_bool).unwrap_or(false),
                "floating": window.get("is_floating").and_then(Value::as_bool).unwrap_or(false)
            })
        })
        .collect();
    concise.sort_by_key(|window| {
        !window
            .get("focused")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    });
    serde_json::to_string(&concise).map_err(|e| e.to_string())
}

#[cfg(target_os = "linux")]
fn niri_focused_output(cancel: &AtomicBool) -> Result<Option<(String, Rect)>, String> {
    if !command_exists("niri") || std::env::var_os("NIRI_SOCKET").is_none() {
        return Ok(None);
    }
    let raw = niri_query("focused-output", cancel)?;
    let value: Value =
        serde_json::from_str(&raw).map_err(|e| format!("解析 Niri focused-output 失败: {e}"))?;
    if value.is_null() {
        return Ok(None);
    }
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .ok_or("Niri focused-output 缺少 name")?;
    let rect = json_logical_rect(&value).ok_or("Niri focused-output 缺少 logical 坐标")?;
    Ok(Some((name.to_string(), rect)))
}

#[cfg(target_os = "linux")]
fn json_logical_rect(value: &Value) -> Option<Rect> {
    let logical = value.get("logical")?;
    Some(Rect {
        x: logical.get("x")?.as_f64()?,
        y: logical.get("y")?.as_f64()?,
        width: logical.get("width")?.as_f64()?,
        height: logical.get("height")?.as_f64()?,
    })
}

fn layout_bounds(outputs: &BTreeMap<String, Rect>) -> Option<Rect> {
    let mut values = outputs.values().copied();
    let first = values.next()?;
    let mut min_x = first.x;
    let mut min_y = first.y;
    let mut max_x = first.right();
    let mut max_y = first.bottom();
    for rect in values {
        min_x = min_x.min(rect.x);
        min_y = min_y.min(rect.y);
        max_x = max_x.max(rect.right());
        max_y = max_y.max(rect.bottom());
    }
    Some(Rect {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    })
}

fn parse_geometry(value: &str) -> Result<Rect, String> {
    let mut pieces = value.split_whitespace();
    let origin = pieces.next().ok_or("region 缺少 x,y")?;
    let size = pieces.next().ok_or("region 缺少 widthxheight")?;
    if pieces.next().is_some() {
        return Err("region 格式应为 x,y widthxheight".into());
    }
    let (x, y) = origin.split_once(',').ok_or("region 原点格式应为 x,y")?;
    let (width, height) = size
        .split_once('x')
        .ok_or("region 尺寸格式应为 widthxheight")?;
    let rect = Rect {
        x: x.parse().map_err(|_| "region x 不是数字")?,
        y: y.parse().map_err(|_| "region y 不是数字")?,
        width: width.parse().map_err(|_| "region width 不是数字")?,
        height: height.parse().map_err(|_| "region height 不是数字")?,
    };
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return Err("region 宽高必须大于 0".into());
    }
    Ok(rect)
}

fn png_size(path: &Path) -> Result<(u32, u32), String> {
    let mut header = [0u8; 24];
    let mut file = fs::File::open(path).map_err(|e| format!("打开截图失败: {e}"))?;
    file.read_exact(&mut header)
        .map_err(|e| format!("读取 PNG 头失败: {e}"))?;
    if &header[..8] != b"\x89PNG\r\n\x1a\n" || &header[12..16] != b"IHDR" {
        return Err("grim 返回的文件不是有效 PNG".into());
    }
    let width = u32::from_be_bytes(header[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(header[20..24].try_into().unwrap());
    if width == 0 || height == 0 {
        return Err("PNG 尺寸为 0".into());
    }
    Ok((width, height))
}

fn frame_dir(cfg: &Config, session: &str) -> PathBuf {
    let safe: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    cfg.data_dir()
        .join("computer-use")
        .join(if safe.is_empty() { "main" } else { &safe })
}

fn latest_meta_path(cfg: &Config, session: &str) -> PathBuf {
    frame_dir(cfg, session).join("latest.json")
}

fn frame_meta_path(cfg: &Config, session: &str, frame_id: &str) -> PathBuf {
    frame_dir(cfg, session).join(format!("{frame_id}.json"))
}

fn generation_path(cfg: &Config, session: &str) -> PathBuf {
    frame_dir(cfg, session).join("ui-generation")
}

fn save_latest_meta(cfg: &Config, session: &str, meta: &FrameMeta) -> Result<(), String> {
    let path = latest_meta_path(cfg, session);
    let temp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(meta).map_err(|e| format!("序列化截图坐标失败: {e}"))?;
    fs::write(&temp, &data).map_err(|e| format!("保存截图坐标失败: {e}"))?;
    fs::rename(&temp, &path).map_err(|e| format!("替换截图坐标失败: {e}"))?;
    fs::write(frame_meta_path(cfg, session, &meta.frame_id), data)
        .map_err(|e| format!("保存 frame_id 索引失败: {e}"))
}

fn load_latest_meta(cfg: &Config, session: &str) -> Result<FrameMeta, String> {
    let path = latest_meta_path(cfg, session);
    let data = fs::read(&path)
        .map_err(|_| "还没有截图；先调用 computer_use action=observe".to_string())?;
    serde_json::from_slice(&data).map_err(|e| format!("截图坐标记录损坏: {e}"))
}

fn load_frame_meta(cfg: &Config, session: &str, frame_id: &str) -> Result<FrameMeta, String> {
    if frame_id.is_empty()
        || !frame_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("frame_id 格式错误".into());
    }
    let data = fs::read(frame_meta_path(cfg, session, frame_id))
        .map_err(|_| "frame_id 不存在".to_string())?;
    serde_json::from_slice(&data).map_err(|e| format!("frame_id 坐标记录损坏: {e}"))
}

fn current_ui_generation(cfg: &Config, session: &str) -> u64 {
    fs::read_to_string(generation_path(cfg, session))
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

fn advance_ui_generation(cfg: &Config, session: &str) -> Result<u64, String> {
    let dir = frame_dir(cfg, session);
    fs::create_dir_all(&dir).map_err(|e| format!("创建 Computer Use 状态目录失败: {e}"))?;
    let next = current_ui_generation(cfg, session).saturating_add(1);
    let path = generation_path(cfg, session);
    let temp = path.with_extension("tmp");
    fs::write(&temp, next.to_string()).map_err(|e| format!("保存画面代次失败: {e}"))?;
    fs::rename(&temp, &path).map_err(|e| format!("更新画面代次失败: {e}"))?;
    Ok(next)
}

fn prune_old_frames(dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut images: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("png"))
        .collect();
    images.sort();
    let remove_count = images.len().saturating_sub(keep);
    for path in images.into_iter().take(remove_count) {
        if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
            let _ = fs::remove_file(path.with_file_name(format!("{stem}.json")));
        }
        let _ = fs::remove_file(path);
    }
}

fn next_frame_id() -> String {
    format!(
        "f{}-{}",
        unix_ms(),
        FRAME_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn required_point(args: &Value) -> Result<(f64, f64), String> {
    Ok((required_f64(args, "x")?, required_f64(args, "y")?))
}

fn required_f64(args: &Value, key: &str) -> Result<f64, String> {
    args.get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("缺少数字参数 {key}"))
}

fn required_u64(args: &Value, key: &str) -> Result<u64, String> {
    args.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("缺少整数参数 {key}"))
}

fn pointer_button(value: &str) -> Result<u32, String> {
    match value.to_ascii_lowercase().as_str() {
        "left" | "primary" => Ok(0x110),
        "right" | "secondary" => Ok(0x111),
        "middle" => Ok(0x112),
        _ => Err(format!("未知鼠标键 {value}；支持 left/right/middle")),
    }
}

fn cancellable_sleep(duration: Duration, cancel: &AtomicBool) -> Result<(), String> {
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".into());
        }
        std::thread::sleep(
            Duration::from_millis(20)
                .min(deadline.saturating_duration_since(std::time::Instant::now())),
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux_pointer {
    use super::{cancellable_sleep, FrameMeta};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::{
        wl_pointer::{Axis, ButtonState},
        wl_registry,
    };
    use wayland_client::{Connection, Dispatch, QueueHandle};
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };

    #[derive(Default)]
    struct State;

    wayland_client::delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
    wayland_client::delegate_noop!(State: ignore ZwlrVirtualPointerV1);

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
        fn event(
            _state: &mut Self,
            _proxy: &wl_registry::WlRegistry,
            _event: wl_registry::Event,
            _data: &GlobalListContents,
            _connection: &Connection,
            _queue_handle: &QueueHandle<Self>,
        ) {
        }
    }

    struct PointerSession {
        connection: Connection,
        pointer: ZwlrVirtualPointerV1,
        queue: wayland_client::EventQueue<State>,
        state: State,
    }

    impl PointerSession {
        fn connect() -> Result<Self, String> {
            let connection =
                Connection::connect_to_env().map_err(|e| format!("连接 Wayland 失败: {e}"))?;
            let (globals, mut queue) = registry_queue_init::<State>(&connection)
                .map_err(|e| format!("读取 Wayland globals 失败: {e}"))?;
            let qh: QueueHandle<State> = queue.handle();
            let manager: ZwlrVirtualPointerManagerV1 = globals
                .bind(&qh, 1..=2, ())
                .map_err(|e| format!("合成器没有 zwlr_virtual_pointer_manager_v1: {e}"))?;
            let pointer = manager.create_virtual_pointer(None, &qh, ());
            let mut state = State;
            queue
                .roundtrip(&mut state)
                .map_err(|e| format!("初始化虚拟指针失败: {e}"))?;
            Ok(Self {
                connection,
                pointer,
                queue,
                state,
            })
        }

        fn sync(&mut self) -> Result<(), String> {
            self.connection
                .flush()
                .map_err(|e| format!("发送 Wayland 输入事件失败: {e}"))?;
            self.queue
                .roundtrip(&mut self.state)
                .map_err(|e| format!("等待 Wayland 合成器确认输入事件失败: {e}"))?;
            Ok(())
        }
    }

    pub fn available() -> Result<bool, String> {
        match PointerSession::connect() {
            Ok(_) => Ok(true),
            Err(e) if e.contains("zwlr_virtual_pointer") => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn move_absolute(meta: &FrameMeta, x: f64, y: f64) -> Result<(), String> {
        let mut session = PointerSession::connect()?;
        emit_absolute(&session.pointer, meta, x, y);
        session.pointer.frame();
        session.sync()
    }

    pub fn click(
        meta: &FrameMeta,
        x: f64,
        y: f64,
        button: u32,
        count: usize,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let mut session = PointerSession::connect()?;
        emit_absolute(&session.pointer, meta, x, y);
        session.pointer.frame();
        session.sync()?;
        for index in 0..count {
            if cancel.load(Ordering::Relaxed) {
                return Err("已取消".into());
            }
            session
                .pointer
                .button(event_time_ms(), button, ButtonState::Pressed);
            session.pointer.frame();
            session.sync()?;
            cancellable_sleep(Duration::from_millis(24), cancel)?;
            session
                .pointer
                .button(event_time_ms(), button, ButtonState::Released);
            session.pointer.frame();
            session.sync()?;
            if index + 1 < count {
                cancellable_sleep(Duration::from_millis(70), cancel)?;
            }
        }
        Ok(())
    }

    pub fn drag(
        meta: &FrameMeta,
        from: (f64, f64),
        to: (f64, f64),
        button: u32,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        let mut session = PointerSession::connect()?;
        emit_absolute(&session.pointer, meta, from.0, from.1);
        session.pointer.frame();
        session.sync()?;
        session
            .pointer
            .button(event_time_ms(), button, ButtonState::Pressed);
        session.pointer.frame();
        session.sync()?;
        for step in 1..=20 {
            if cancel.load(Ordering::Relaxed) {
                session
                    .pointer
                    .button(event_time_ms(), button, ButtonState::Released);
                session.pointer.frame();
                let _ = session.sync();
                return Err("拖拽被取消，已释放鼠标键".into());
            }
            let t = step as f64 / 20.0;
            emit_absolute(
                &session.pointer,
                meta,
                from.0 + (to.0 - from.0) * t,
                from.1 + (to.1 - from.1) * t,
            );
            session.pointer.frame();
            session.sync()?;
            cancellable_sleep(Duration::from_millis(8), cancel)?;
        }
        session
            .pointer
            .button(event_time_ms(), button, ButtonState::Released);
        session.pointer.frame();
        session.sync()
    }

    pub fn scroll(
        meta: &FrameMeta,
        point: Option<(f64, f64)>,
        steps: i32,
        cancel: &AtomicBool,
    ) -> Result<(), String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".into());
        }
        let mut session = PointerSession::connect()?;
        if let Some((x, y)) = point {
            emit_absolute(&session.pointer, meta, x, y);
            session.pointer.frame();
            session.sync()?;
        }
        session
            .pointer
            .axis(event_time_ms(), Axis::VerticalScroll, steps as f64 * 15.0);
        session.pointer.frame();
        session.sync()
    }

    fn emit_absolute(pointer: &ZwlrVirtualPointerV1, meta: &FrameMeta, x: f64, y: f64) {
        let width = meta.layout_width.max(1.0).ceil().min(u32::MAX as f64) as u32;
        let height = meta.layout_height.max(1.0).ceil().min(u32::MAX as f64) as u32;
        let local_x = (x - meta.layout_min_x)
            .clamp(0.0, width.saturating_sub(1) as f64)
            .round() as u32;
        let local_y = (y - meta.layout_min_y)
            .clamp(0.0, height.saturating_sub(1) as f64)
            .round() as u32;
        pointer.motion_absolute(event_time_ms(), local_x, local_y, width, height);
    }

    fn event_time_ms() -> u32 {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
        if result == 0 {
            (now.tv_sec as u64 * 1_000 + now.tv_nsec as u64 / 1_000_000) as u32
        } else {
            static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
            START.get_or_init(Instant::now).elapsed().as_millis() as u32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_is_removed_and_path_preserved() {
        let (clean, path) = split_image_marker("ok\n[[YJLCODER_IMAGE:/tmp/a.png]]\n");
        assert_eq!(clean, "ok");
        assert_eq!(path.as_deref(), Some("/tmp/a.png"));
    }

    #[test]
    fn geometry_and_layout_mapping_handle_fractional_scale() {
        let meta = FrameMeta {
            frame_id: "f1".into(),
            image_path: "/tmp/a.png".into(),
            target: "output".into(),
            output: Some("eDP-1".into()),
            pixel_width: 1462,
            pixel_height: 914,
            logical_x: 2560.0,
            logical_y: 0.0,
            logical_width: 1462.0,
            logical_height: 914.0,
            layout_min_x: 0.0,
            layout_min_y: 0.0,
            layout_width: 4022.0,
            layout_height: 1440.0,
            clickable: true,
            captured_unix_ms: 0,
            generation: 0,
        };
        let mapped = map_frame_point(&meta, 0.0, 0.0).unwrap();
        assert!((mapped.0 - 2560.5).abs() < 0.001);
        assert!((mapped.1 - 0.5).abs() < 0.001);
        assert!(map_frame_point(&meta, 1462.0, 0.0).is_err());
    }

    #[test]
    fn layout_bounds_support_negative_origins() {
        let outputs = BTreeMap::from([
            (
                "left".into(),
                Rect {
                    x: -1920.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                },
            ),
            (
                "main".into(),
                Rect {
                    x: 0.0,
                    y: -200.0,
                    width: 2560.0,
                    height: 1440.0,
                },
            ),
        ]);
        let bounds = layout_bounds(&outputs).unwrap();
        assert_eq!(bounds.x, -1920.0);
        assert_eq!(bounds.y, -200.0);
        assert_eq!(bounds.width, 4480.0);
        assert_eq!(bounds.height, 1440.0);
    }

    #[test]
    fn parse_region_rejects_broken_values() {
        assert_eq!(parse_geometry("-10,20 800x600").unwrap().x, -10.0);
        assert!(parse_geometry("10,20").is_err());
        assert!(parse_geometry("10,20 0x600").is_err());
    }

    #[test]
    fn batch_recognizes_only_coordinate_actions_as_frame_bound() {
        assert!(action_uses_frame(&json!({"type":"click"})));
        assert!(action_uses_frame(&json!({"action":"scroll"})));
        assert!(!action_uses_frame(&json!({"type":"type_text"})));
        assert!(!action_uses_frame(&json!({"type":"wait"})));
    }

    #[test]
    fn weak_model_aliases_are_normalized_without_retry() {
        let region = normalize_computer_args(&json!({
            "action":"screenshot", "target":"region",
            "x":"10", "y":20, "width":"300", "height":200
        }))
        .unwrap();
        assert_eq!(region["action"], "observe");
        assert_eq!(region["region"], "10,20 300x200");

        let drag = normalize_computer_args(&json!({
            "action":"drag", "x":"1", "y":"2", "to_x":"30", "to_y":40
        }))
        .unwrap();
        assert_eq!(drag["from_x"], 1.0);
        assert_eq!(drag["from_y"], 2.0);
        assert_eq!(drag["to_x"], 30.0);

        let scroll = normalize_computer_args(&json!({"action":"scroll", "delta_y":-600})).unwrap();
        assert_eq!(scroll["steps"], -6);

        let click = normalize_computer_args(&json!({"action":"click_element", "id":"5"})).unwrap();
        assert_eq!(click["action"], "click");
        assert_eq!(click["window_id"], 5);
    }

    #[test]
    fn batch_compacts_model_generated_mouse_tracks_and_waits() {
        let mut actions = Vec::new();
        for x in 0..51 {
            actions.push(json!({"action":"move", "x":x, "y":10}));
        }
        actions.push(json!({"action":"wait", "ms":400}));
        actions.push(json!({"action":"wait", "ms":700}));
        actions.push(json!({"action":"observe"}));
        let compacted = compact_batch_actions(&actions);
        assert_eq!(compacted.len(), 2);
        assert_eq!(compacted[0]["x"], 50);
        assert_eq!(compacted[1]["ms"], 1100);
        assert_eq!(summarize_action_names(&vec!["move".into(); 50]), "move×50");
    }

    #[test]
    fn observe_does_not_expire_another_frame_but_ui_change_does() {
        let root = std::env::temp_dir().join(format!("yjlcoder_generation_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut cfg = Config::default();
        cfg.set_test_data_dir(root.clone());
        fs::create_dir_all(frame_dir(&cfg, "generation-test")).unwrap();
        let image = frame_dir(&cfg, "generation-test").join("f-old.png");
        fs::write(&image, b"test").unwrap();
        let meta = FrameMeta {
            frame_id: "f-old".into(),
            image_path: image.to_string_lossy().into_owned(),
            target: "output".into(),
            output: Some("screen".into()),
            pixel_width: 100,
            pixel_height: 100,
            logical_x: 0.0,
            logical_y: 0.0,
            logical_width: 100.0,
            logical_height: 100.0,
            layout_min_x: 0.0,
            layout_min_y: 0.0,
            layout_width: 100.0,
            layout_height: 100.0,
            clickable: true,
            captured_unix_ms: 0,
            generation: 0,
        };
        save_latest_meta(&cfg, "generation-test", &meta).unwrap();
        let newer = FrameMeta {
            frame_id: "f-region".into(),
            target: "region".into(),
            ..meta.clone()
        };
        save_latest_meta(&cfg, "generation-test", &newer).unwrap();
        assert!(checked_frame(&cfg, "generation-test", &json!({"frame_id":"f-old"})).is_ok());
        advance_ui_generation(&cfg, "generation-test").unwrap();
        assert!(checked_frame(&cfg, "generation-test", &json!({"frame_id":"f-old"})).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn screenshot_result_is_concise_when_image_is_attached() {
        let meta = FrameMeta {
            frame_id: "f1".into(),
            image_path: "/a/very/long/private/path.png".into(),
            target: "output".into(),
            output: Some("eDP-1".into()),
            pixel_width: 100,
            pixel_height: 80,
            logical_x: 0.0,
            logical_y: 0.0,
            logical_width: 100.0,
            logical_height: 80.0,
            layout_min_x: 0.0,
            layout_min_y: 0.0,
            layout_width: 100.0,
            layout_height: 80.0,
            clickable: true,
            captured_unix_ms: 0,
            generation: 0,
        };
        let result = frame_result(&meta, true);
        let (clean, image) = split_image_marker(&result);
        assert!(clean.len() < 220, "{clean}");
        assert!(!clean.contains("private/path"));
        assert_eq!(image.as_deref(), Some("/a/very/long/private/path.png"));
    }

    #[test]
    fn niri_queries_drop_large_layout_and_mode_histories() {
        let outputs = r#"{"eDP-1":{"make":"Tianma","model":"Panel","physical_size":[290,180],"modes":[{"width":2560,"height":1600,"refresh_rate":180000},{"width":640,"height":480,"refresh_rate":60000}],"current_mode":0,"logical":{"x":0,"y":0,"width":1462,"height":914,"scale":1.75}}}"#;
        let concise = summarize_niri_outputs(outputs).unwrap();
        assert!(concise.contains("180.0"));
        assert!(!concise.contains("640"));

        let windows = r#"[{"id":5,"title":"Firefox","app_id":"firefox","pid":12,"workspace_id":1,"is_focused":true,"is_floating":false,"layout":{"tile_size":[700,800]},"focus_timestamp":{"secs":999}}]"#;
        let concise = summarize_niri_windows(windows).unwrap();
        assert!(concise.contains("\"window_id\":5"));
        assert!(!concise.contains("tile_size"));
        assert!(!concise.contains("focus_timestamp"));
    }

    #[test]
    fn batch_rejects_empty_and_read_only_queries_before_touching_desktop() {
        let cfg = Config::default();
        let cancel = AtomicBool::new(false);
        let empty =
            execute(&cfg, "batch-test", &json!({"actions":[]}), &cancel, false).unwrap_err();
        assert!(empty.contains("1..=200"));
        let query = execute(
            &cfg,
            "batch-test",
            &json!({"actions":[{"type":"capabilities"}]}),
            &cancel,
            false,
        )
        .unwrap_err();
        assert!(query.contains("只读查询"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "需要正在运行的 Wayland/Niri 图形会话"]
    fn live_niri_capture_and_virtual_pointer_move() {
        let root =
            std::env::temp_dir().join(format!("yjlcoder_computer_live_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut cfg = Config::default();
        cfg.set_test_data_dir(root.clone());
        let cancel = AtomicBool::new(false);

        let observed = execute(
            &cfg,
            "live",
            &json!({"action":"observe", "target":"focused_output"}),
            &cancel,
            true,
        )
        .unwrap();
        let (_, image) = split_image_marker(&observed);
        assert!(image
            .as_deref()
            .is_some_and(|path| Path::new(path).exists()));
        let meta = load_latest_meta(&cfg, "live").unwrap();
        assert!(meta.pixel_width > 0 && meta.pixel_height > 0);

        execute(
            &cfg,
            "live",
            &json!({
                "action":"move",
                "frame_id":meta.frame_id,
                "x":meta.pixel_width / 2,
                "y":meta.pixel_height / 2
            }),
            &cancel,
            true,
        )
        .unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "手动桌面实机验收：从 YJLCODER_COMPUTER_ACTION_B64 读取 JSON"]
    fn live_manual_action_from_env() {
        let encoded = std::env::var("YJLCODER_COMPUTER_ACTION_B64")
            .expect("请设置 YJLCODER_COMPUTER_ACTION_B64");
        let raw = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .expect("动作必须是 base64"),
        )
        .expect("动作必须是 UTF-8");
        let args: Value = serde_json::from_str(&raw).expect("动作必须是 JSON 对象");
        let cfg = Config::load();
        let cancel = AtomicBool::new(false);
        let output = execute(&cfg, "computer-use-smoke", &args, &cancel, true).unwrap();
        println!("{output}");
    }
}
