//! 独立 Chromium/CDP 后端：所有输入、截图和焦点都留在独立无头浏览器中。

use super::*;
use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::{Mutex, OnceLock};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message, WebSocket};

const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 800;

struct BrowserRuntime {
    child: std::process::Child,
    port: u16,
    width: u32,
    height: u32,
    next_id: u64,
}

impl Drop for BrowserRuntime {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn runtimes() -> &'static Mutex<HashMap<String, BrowserRuntime>> {
    static RUNTIMES: OnceLock<Mutex<HashMap<String, BrowserRuntime>>> = OnceLock::new();
    RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn execute(
    cfg: &Config,
    session: &str,
    args: &Value,
    cancel: &AtomicBool,
    vision_available: bool,
) -> Result<String, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err("已取消".into());
    }
    let action = action_name(args);
    if matches!(action.as_str(), "stop" | "close") {
        let removed = runtimes()
            .lock()
            .map_err(|_| "浏览器运行时锁已损坏")?
            .remove(&runtime_key(cfg, session))
            .is_some();
        return Ok(json!({"backend":"browser","stopped":removed}).to_string());
    }
    let key = runtime_key(cfg, session);
    let mut guard = runtimes().lock().map_err(|_| "浏览器运行时锁已损坏")?;
    if !guard.contains_key(&key) {
        guard.insert(key.clone(), start_browser(cfg, session, args, cancel)?);
    }
    let runtime = guard.get_mut(&key).ok_or("浏览器运行时启动失败")?;
    if runtime
        .child
        .try_wait()
        .map_err(|e| e.to_string())?
        .is_some()
    {
        *runtime = start_browser(cfg, session, args, cancel)?;
    }
    if let Some(actions) = args.get("actions") {
        let actions = actions.as_array().ok_or("actions 必须是数组")?;
        if actions.is_empty() || actions.len() > MAX_RAW_ACTIONS_PER_BATCH {
            return Err("actions 数量必须为 1..=200".into());
        }
        for (index, action) in compact_batch_actions(actions).into_iter().enumerate() {
            run_action(
                cfg,
                session,
                runtime,
                &action,
                cancel,
                false,
                vision_available,
            )
            .map_err(|e| format!("actions[{index}] 失败: {e}"))?;
        }
        return capture(cfg, session, runtime, vision_available);
    }
    run_action(cfg, session, runtime, args, cancel, true, vision_available)
}

fn run_action(
    cfg: &Config,
    session: &str,
    runtime: &mut BrowserRuntime,
    args: &Value,
    cancel: &AtomicBool,
    capture_after: bool,
    vision_available: bool,
) -> Result<String, String> {
    let action = action_name(args);
    match action.as_str() {
        "capabilities" | "status" => Ok(json!({
            "available":true,
            "backend":"isolated_chromium_cdp",
            "host_pointer":false,
            "host_keyboard":false,
            "requires_focus":false,
            "viewport":{"width":runtime.width,"height":runtime.height},
            "actions":["observe","open_url","click","double_click","move","drag","scroll","type_text","press_key","wait","stop"]
        }).to_string()),
        "observe" | "screenshot" => capture(cfg, session, runtime, vision_available),
        "open_url" => {
            let url = args.get("url").and_then(Value::as_str).ok_or("open_url 缺少 url")?;
            validate_web_url(url)?;
            cdp(runtime, "Page.navigate", json!({"url":url}))?;
            wait_after(args, cancel, 800)?;
            advance_ui_generation(cfg, session)?;
            result_or_capture(capture_after, "已在隔离浏览器打开网址", || capture(cfg, session, runtime, vision_available))
        }
        "click" | "double_click" | "double-click" => {
            let meta = browser_frame(cfg, session, args)?;
            let (x, y) = required_point(args)?;
            validate_browser_point(&meta, x, y)?;
            let button = cdp_button(args.get("button").and_then(Value::as_str).unwrap_or("left"))?;
            let count = if action == "click" { 1 } else { 2 };
            for click_count in 1..=count {
                cdp(runtime, "Input.dispatchMouseEvent", json!({"type":"mousePressed","x":x,"y":y,"button":button,"clickCount":click_count}))?;
                cdp(runtime, "Input.dispatchMouseEvent", json!({"type":"mouseReleased","x":x,"y":y,"button":button,"clickCount":click_count}))?;
            }
            advance_ui_generation(cfg, session)?;
            result_or_capture(capture_after, "隔离浏览器点击完成", || capture(cfg, session, runtime, vision_available))
        }
        "move" | "move_pointer" | "move-pointer" => {
            let meta = browser_frame(cfg, session, args)?;
            let (x, y) = required_point(args)?;
            validate_browser_point(&meta, x, y)?;
            cdp(runtime, "Input.dispatchMouseEvent", json!({"type":"mouseMoved","x":x,"y":y}))?;
            Ok("隔离浏览器指针移动完成（宿主鼠标未移动）".into())
        }
        "drag" => {
            let meta = browser_frame(cfg, session, args)?;
            let from = (required_f64(args, "from_x")?, required_f64(args, "from_y")?);
            let to = (required_f64(args, "to_x")?, required_f64(args, "to_y")?);
            validate_browser_point(&meta, from.0, from.1)?;
            validate_browser_point(&meta, to.0, to.1)?;
            let button = cdp_button(args.get("button").and_then(Value::as_str).unwrap_or("left"))?;
            cdp(runtime, "Input.dispatchMouseEvent", json!({"type":"mouseMoved","x":from.0,"y":from.1}))?;
            cdp(runtime, "Input.dispatchMouseEvent", json!({"type":"mousePressed","x":from.0,"y":from.1,"button":button,"clickCount":1}))?;
            for step in 1..=12 {
                let t = step as f64 / 12.0;
                let x = from.0 + (to.0 - from.0) * t;
                let y = from.1 + (to.1 - from.1) * t;
                cdp(runtime, "Input.dispatchMouseEvent", json!({"type":"mouseMoved","x":x,"y":y,"button":button,"buttons":1}))?;
            }
            cdp(runtime, "Input.dispatchMouseEvent", json!({"type":"mouseReleased","x":to.0,"y":to.1,"button":button,"clickCount":1}))?;
            advance_ui_generation(cfg, session)?;
            result_or_capture(capture_after, "隔离浏览器拖拽完成", || capture(cfg, session, runtime, vision_available))
        }
        "scroll" => {
            let meta = browser_frame(cfg, session, args)?;
            let x = args.get("x").and_then(Value::as_f64).unwrap_or(meta.pixel_width as f64 / 2.0);
            let y = args.get("y").and_then(Value::as_f64).unwrap_or(meta.pixel_height as f64 / 2.0);
            validate_browser_point(&meta, x, y)?;
            let steps = args.get("steps").and_then(Value::as_i64).unwrap_or(3).clamp(-50, 50);
            if steps == 0 { return Err("steps 不能为 0".into()); }
            cdp(runtime, "Input.dispatchMouseEvent", json!({"type":"mouseWheel","x":x,"y":y,"deltaX":0,"deltaY":steps * 120}))?;
            advance_ui_generation(cfg, session)?;
            result_or_capture(capture_after, "隔离浏览器滚动完成", || capture(cfg, session, runtime, vision_available))
        }
        "type" | "type_text" | "type-text" => {
            let text = args.get("text").and_then(Value::as_str).ok_or("type_text 缺少 text")?;
            if text.len() > 20_000 { return Err("单次输入最多 20000 字节".into()); }
            cdp(runtime, "Input.insertText", json!({"text":text}))?;
            advance_ui_generation(cfg, session)?;
            result_or_capture(capture_after, "隔离浏览器文字输入完成", || capture(cfg, session, runtime, vision_available))
        }
        "key" | "press_key" | "press-key" => {
            let keys = args.get("keys").or_else(|| args.get("key")).and_then(Value::as_str).ok_or("press_key 缺少 keys")?;
            dispatch_key(runtime, keys)?;
            advance_ui_generation(cfg, session)?;
            result_or_capture(capture_after, "隔离浏览器按键完成", || capture(cfg, session, runtime, vision_available))
        }
        "wait" => {
            wait_after(args, cancel, 500)?;
            result_or_capture(capture_after, "等待完成", || capture(cfg, session, runtime, vision_available))
        }
        "list_windows" | "windows" => Ok(http_json(runtime.port, "/json/list")?.to_string()),
        other => Err(format!("隔离浏览器不支持 action={other}；任意桌面 GUI 请使用 backend=isolated")),
    }
}

fn start_browser(
    cfg: &Config,
    session: &str,
    args: &Value,
    cancel: &AtomicBool,
) -> Result<BrowserRuntime, String> {
    let program = chromium_program().ok_or(
        "没有找到 Chromium/Chrome。Linux 可安装 chromium；网页之外的 GUI 请使用 backend=isolated",
    )?;
    let width = args
        .get("width")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_WIDTH as u64)
        .clamp(320, 3840) as u32;
    let height = args
        .get("height")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_HEIGHT as u64)
        .clamp(240, 2160) as u32;
    let root = cfg
        .data_dir()
        .join("computer-use")
        .join("browser")
        .join(safe_session(session));
    let profile = root.join("profile");
    fs::create_dir_all(&profile).map_err(|e| format!("创建隔离浏览器目录失败: {e}"))?;
    let active_port = profile.join("DevToolsActivePort");
    let _ = fs::remove_file(&active_port);
    let mut child = Command::new(program)
        .arg("--headless=new")
        .arg("--remote-debugging-address=127.0.0.1")
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(format!("--window-size={width},{height}"))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-sync",
            "--disable-extensions",
            "--disable-component-update",
            "about:blank",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("启动隔离 Chromium 失败: {e}"))?;
    let started = std::time::Instant::now();
    let port = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("启动隔离浏览器时被取消".into());
        }
        if let Ok(text) = fs::read_to_string(&active_port) {
            if let Some(port) = text
                .lines()
                .next()
                .and_then(|line| line.parse::<u16>().ok())
            {
                break port;
            }
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Chromium 未在 10 秒内建立 CDP 端口".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    Ok(BrowserRuntime {
        child,
        port,
        width,
        height,
        next_id: 1,
    })
}

fn capture(
    cfg: &Config,
    session: &str,
    runtime: &mut BrowserRuntime,
    vision_available: bool,
) -> Result<String, String> {
    let result = cdp(
        runtime,
        "Page.captureScreenshot",
        json!({"format":"png","fromSurface":true,"captureBeyondViewport":false}),
    )?;
    let encoded = result
        .get("data")
        .and_then(Value::as_str)
        .ok_or("CDP 截图缺少 data")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| format!("解码 CDP 截图失败: {e}"))?;
    let frame_id = next_frame_id();
    let dir = frame_dir(cfg, session);
    fs::create_dir_all(&dir).map_err(|e| format!("创建截图目录失败: {e}"))?;
    let path = dir.join(format!("{frame_id}.png"));
    fs::write(&path, bytes).map_err(|e| format!("保存 CDP 截图失败: {e}"))?;
    let (pixel_width, pixel_height) = png_size(&path)?;
    let meta = FrameMeta {
        frame_id,
        image_path: path.to_string_lossy().into_owned(),
        target: "browser".into(),
        output: Some("isolated-chromium".into()),
        pixel_width,
        pixel_height,
        logical_x: 0.0,
        logical_y: 0.0,
        logical_width: pixel_width as f64,
        logical_height: pixel_height as f64,
        layout_min_x: 0.0,
        layout_min_y: 0.0,
        layout_width: pixel_width as f64,
        layout_height: pixel_height as f64,
        clickable: true,
        captured_unix_ms: unix_ms(),
        generation: current_ui_generation(cfg, session),
    };
    save_latest_meta(cfg, session, &meta)?;
    prune_old_frames(&dir, 8);
    Ok(frame_result(&meta, vision_available))
}

fn cdp(runtime: &mut BrowserRuntime, method: &str, params: Value) -> Result<Value, String> {
    let targets = http_json(runtime.port, "/json/list")?;
    let ws_url = targets
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.get("type").and_then(Value::as_str) == Some("page"))
        })
        .and_then(|item| item.get("webSocketDebuggerUrl"))
        .and_then(Value::as_str)
        .ok_or("Chromium 没有可操作的 page target")?;
    let (mut socket, _) = connect(ws_url).map_err(|e| format!("连接 Chromium CDP 失败: {e}"))?;
    if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| format!("设置 CDP 读取超时失败: {e}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| format!("设置 CDP 写入超时失败: {e}"))?;
    }
    let id = runtime.next_id;
    runtime.next_id = runtime.next_id.saturating_add(1);
    socket
        .send(Message::Text(
            json!({"id":id,"method":method,"params":params}).to_string(),
        ))
        .map_err(|e| format!("发送 CDP {method} 失败: {e}"))?;
    read_cdp_response(&mut socket, id, method)
}

fn read_cdp_response(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    id: u64,
    method: &str,
) -> Result<Value, String> {
    for _ in 0..200 {
        let message = socket
            .read()
            .map_err(|e| format!("读取 CDP {method} 响应失败: {e}"))?;
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value =
            serde_json::from_str(&text).map_err(|e| format!("解析 CDP 响应失败: {e}"))?;
        if value.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(format!("CDP {method} 失败: {error}"));
        }
        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
    }
    Err(format!("CDP {method} 响应事件过多"))
}

fn http_json(port: u16, path: &str) -> Result<Value, String> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let response = ureq::get(&url)
        .timeout(Duration::from_secs(3))
        .call()
        .map_err(|e| format!("访问 Chromium CDP 失败: {e}"))?;
    let text = response.into_string().map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("解析 Chromium CDP 列表失败: {e}"))
}

fn browser_frame(cfg: &Config, session: &str, args: &Value) -> Result<FrameMeta, String> {
    let latest = load_latest_meta(cfg, session)?;
    let meta = match args.get("frame_id").and_then(Value::as_str) {
        Some(id) if id != latest.frame_id => {
            load_frame_meta(cfg, session, id).map_err(|_| format!("找不到 frame_id {id}"))?
        }
        _ => latest,
    };
    if meta.target != "browser" {
        return Err("frame_id 不是隔离浏览器截图，请重新 observe".into());
    }
    if meta.generation != current_ui_generation(cfg, session) {
        return Err("frame_id 已过期，请重新 observe".into());
    }
    Ok(meta)
}

fn validate_browser_point(meta: &FrameMeta, x: f64, y: f64) -> Result<(), String> {
    if x.is_finite()
        && y.is_finite()
        && x >= 0.0
        && y >= 0.0
        && x < meta.pixel_width as f64
        && y < meta.pixel_height as f64
    {
        Ok(())
    } else {
        Err(format!(
            "坐标 ({x},{y}) 超出隔离浏览器截图 {}x{}",
            meta.pixel_width, meta.pixel_height
        ))
    }
}

fn dispatch_key(runtime: &mut BrowserRuntime, keys: &str) -> Result<(), String> {
    let parts: Vec<_> = keys
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let key = parts.last().ok_or("keys 不能为空")?.to_string();
    let mut modifiers = 0;
    for value in &parts[..parts.len().saturating_sub(1)] {
        modifiers |= match value.to_ascii_lowercase().as_str() {
            "alt" | "option" => 1,
            "ctrl" | "control" => 2,
            "meta" | "super" | "win" | "command" | "cmd" => 4,
            "shift" => 8,
            other => return Err(format!("不支持的修饰键 {other}")),
        };
    }
    let code = match key.to_ascii_lowercase().as_str() {
        "return" | "enter" => "Enter",
        "escape" | "esc" => "Escape",
        "space" => "Space",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "up" | "arrowup" => "ArrowUp",
        "down" | "arrowdown" => "ArrowDown",
        "left" | "arrowleft" => "ArrowLeft",
        "right" | "arrowright" => "ArrowRight",
        _ => &key,
    };
    cdp(
        runtime,
        "Input.dispatchKeyEvent",
        json!({"type":"keyDown","key":code,"code":code,"modifiers":modifiers}),
    )?;
    cdp(
        runtime,
        "Input.dispatchKeyEvent",
        json!({"type":"keyUp","key":code,"code":code,"modifiers":modifiers}),
    )?;
    Ok(())
}

fn cdp_button(value: &str) -> Result<&'static str, String> {
    match value.to_ascii_lowercase().as_str() {
        "left" | "primary" | "1" => Ok("left"),
        "middle" | "2" => Ok("middle"),
        "right" | "secondary" | "3" => Ok("right"),
        _ => Err("button 支持 left/middle/right".into()),
    }
}

fn result_or_capture<F: FnOnce() -> Result<String, String>>(
    capture_after: bool,
    text: &str,
    capture: F,
) -> Result<String, String> {
    if capture_after {
        capture()
    } else {
        Ok(text.into())
    }
}

fn wait_after(args: &Value, cancel: &AtomicBool, default_ms: u64) -> Result<(), String> {
    let ms = args
        .get("wait_ms")
        .or_else(|| args.get("ms"))
        .and_then(Value::as_u64)
        .unwrap_or(default_ms)
        .min(10_000);
    cancellable_sleep(Duration::from_millis(ms), cancel)
}

fn action_name(args: &Value) -> String {
    args.get("action")
        .and_then(Value::as_str)
        .unwrap_or("observe")
        .trim()
        .to_ascii_lowercase()
}

fn chromium_program() -> Option<&'static str> {
    [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "chrome",
    ]
    .into_iter()
    .find(|name| command_exists(name))
}

fn runtime_key(cfg: &Config, session: &str) -> String {
    format!("{}:{}", cfg.data_dir().display(), safe_session(session))
}

fn safe_session(session: &str) -> String {
    let value: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
    if value.is_empty() {
        "default".into()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_name_cannot_escape_runtime_directory() {
        assert_eq!(safe_session("../../hello world"), "______hello_world");
    }
    #[test]
    fn browser_points_are_strictly_bounded() {
        let meta = FrameMeta {
            frame_id: "f".into(),
            image_path: "x".into(),
            target: "browser".into(),
            output: None,
            pixel_width: 100,
            pixel_height: 50,
            logical_x: 0.0,
            logical_y: 0.0,
            logical_width: 100.0,
            logical_height: 50.0,
            layout_min_x: 0.0,
            layout_min_y: 0.0,
            layout_width: 100.0,
            layout_height: 50.0,
            clickable: true,
            captured_unix_ms: 0,
            generation: 0,
        };
        assert!(validate_browser_point(&meta, 99.0, 49.0).is_ok());
        assert!(validate_browser_point(&meta, 100.0, 49.0).is_err());
    }
}
