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
    if args.get("actions").is_some() {
        return execute_batch(cfg, session, args, cancel, vision_available);
    }
    execute_single(cfg, session, args, cancel, vision_available, true, None)
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
            action_result_or_capture(capture_after, "点击完成", || {
                recapture(cfg, session, &meta, cancel, vision_available)
            })
        }
        "move" | "move_pointer" | "move-pointer" => {
            let meta = frame_for_action(cfg, session, args, batch_frame)?;
            let (x, y) = required_point(args)?;
            let (desktop_x, desktop_y) = map_frame_point(&meta, x, y)?;
            pointer_move(&meta, desktop_x, desktop_y, cancel)?;
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
            action_result_or_capture(capture_after, "按键完成", || {
                recapture_latest_or_focused(cfg, session, cancel, vision_available)
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
            "未知 computer_use action: {action}。可用：capabilities, observe, list_outputs, list_windows, focus_window, click, double_click, move, drag, scroll, type_text, press_key, wait；也可传 actions 数组批量执行"
        )),
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
    let actions = args
        .get("actions")
        .and_then(Value::as_array)
        .ok_or("actions 必须是 JSON 数组")?;
    if actions.is_empty() || actions.len() > MAX_ACTIONS_PER_BATCH {
        return Err(format!(
            "actions 数量必须在 1..={MAX_ACTIONS_PER_BATCH} 之间"
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
        let object = action
            .as_object()
            .ok_or_else(|| format!("actions[{}] 必须是对象", index))?;
        if object.contains_key("actions") {
            return Err("禁止嵌套 actions 批次".into());
        }
        let name = action
            .get("action")
            .or_else(|| action.get("type"))
            .and_then(Value::as_str)
            .ok_or_else(|| format!("actions[{}] 缺少 action/type", index))?;
        let normalized_name = name.trim().to_ascii_lowercase();
        if matches!(normalized_name.as_str(), "screenshot" | "observe") {
            // OpenAI 协议允许显式 screenshot 动作；我们的批次结尾必定生成一张完整新图，
            // 因此这里合并掉重复截图，避免白白消耗本地视觉模型上下文。
            names.push("screenshot(final)".into());
            continue;
        }
        if matches!(
            normalized_name.as_str(),
            "capabilities" | "status" | "list_outputs" | "outputs" | "list_windows" | "windows"
        ) {
            return Err(format!("批量动作中不能混入只读查询 {name}；请单独调用"));
        }
        let mut normalized = action.clone();
        if normalized.get("action").is_none() {
            normalized["action"] = Value::String(name.to_string());
        }
        execute_single(
            cfg,
            session,
            &normalized,
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
        Some(meta) if meta.clickable => recapture(cfg, session, meta, cancel, vision_available),
        _ => capture(
            cfg,
            session,
            &json!({"target":"focused_output"}),
            cancel,
            vision_available,
        ),
    }?;
    Ok(format!(
        "批量动作成功: {}（{} 项，{}ms）\n{}",
        names.join(" -> "),
        names.len(),
        started.elapsed().as_millis(),
        result
    ))
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
            "window" | "foreign_toplevel" | "foreign-toplevel" => {
                let identifier = args
                    .get("window_identifier")
                    .or_else(|| args.get("identifier"))
                    .and_then(Value::as_str)
                    .ok_or("窗口直截需要 Wayland foreign-toplevel identifier；Niri 数字 window_id 不能代替。点击前请改用 focused_output 截图")?;
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
        Ok(meta) if meta.clickable => recapture(cfg, session, &meta, cancel, vision_available),
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
        "截图已作为视觉输入附加到下一次模型请求"
    } else {
        "本地模型未声明 vision，截图已保存但不会强塞给文本模型"
    };
    format!(
        "截图成功\nframe_id: {}\nimage: {}\nimage_size_px: {}x{}\ntarget: {}\noutput: {}\nlogical_rect: {},{} {}x{}\nclickable: {}\n坐标规则: click/move/drag 直接使用这张截图里的像素坐标，并携带 frame_id；工具会处理分数缩放和多屏偏移。\n{}\n{}{}{}",
        meta.frame_id,
        meta.image_path,
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
    let meta = load_latest_meta(cfg, session)?;
    if let Some(requested) = args.get("frame_id").and_then(Value::as_str) {
        if requested != meta.frame_id {
            return Err(format!(
                "frame_id 已过期：请求 {requested}，最新是 {}。请依据最新截图重新定位",
                meta.frame_id
            ));
        }
    }
    if !meta.clickable {
        return Err("这张窗口直截图没有可靠桌面原点，禁止猜坐标点击；请 observe target=focused_output 后再点击".into());
    }
    if !Path::new(&meta.image_path).exists() {
        return Err("最新截图文件已不存在，请重新 observe".into());
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

fn platform_list_outputs(cancel: &AtomicBool) -> Result<String, String> {
    let _ = cancel;
    #[cfg(target_os = "linux")]
    return niri_query("outputs", cancel);
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
    return niri_query("windows", cancel);
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
    return run_niri_action(&["focus-window", "--id", &id.to_string()], cancel);
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

#[cfg(target_os = "linux")]
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

fn save_latest_meta(cfg: &Config, session: &str, meta: &FrameMeta) -> Result<(), String> {
    let path = latest_meta_path(cfg, session);
    let temp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(meta).map_err(|e| format!("序列化截图坐标失败: {e}"))?;
    fs::write(&temp, data).map_err(|e| format!("保存截图坐标失败: {e}"))?;
    fs::rename(&temp, &path).map_err(|e| format!("替换截图坐标失败: {e}"))
}

fn load_latest_meta(cfg: &Config, session: &str) -> Result<FrameMeta, String> {
    let path = latest_meta_path(cfg, session);
    let data = fs::read(&path)
        .map_err(|_| "还没有截图；先调用 computer_use action=observe".to_string())?;
    serde_json::from_slice(&data).map_err(|e| format!("截图坐标记录损坏: {e}"))
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
    fn batch_rejects_empty_and_read_only_queries_before_touching_desktop() {
        let cfg = Config::default();
        let cancel = AtomicBool::new(false);
        let empty =
            execute(&cfg, "batch-test", &json!({"actions":[]}), &cancel, false).unwrap_err();
        assert!(empty.contains("1..=50"));
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
