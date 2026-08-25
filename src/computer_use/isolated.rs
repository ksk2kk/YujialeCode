//! Linux 独立桌面后端。
//!
//! 每个 YujialeCode 会话拥有单独的 headless Sway compositor、Wayland socket、seat、
//! 焦点和虚拟输入设备。宿主 Niri/GNOME/KDE 只是提供 GPU/CPU 渲染能力，不会收到这里
//! 的鼠标或键盘事件；这和在真实桌面上伪造“第二个光标”有本质区别。

use super::*;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::sync::{mpsc, Mutex, OnceLock};
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

const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 800;

struct SandboxRuntime {
    compositor: std::process::Child,
    keyboard_keeper: std::process::Child,
    apps: Vec<std::process::Child>,
    pointer: PointerWorker,
    runtime_dir: PathBuf,
    socket: PathBuf,
    width: u32,
    height: u32,
}

impl Drop for SandboxRuntime {
    fn drop(&mut self) {
        for app in &mut self.apps {
            let _ = app.kill();
            let _ = app.wait();
        }
        let _ = self.keyboard_keeper.kill();
        let _ = self.keyboard_keeper.wait();
        self.pointer.stop();
        let _ = self.compositor.kill();
        let _ = self.compositor.wait();
    }
}

fn runtimes() -> &'static Mutex<HashMap<String, SandboxRuntime>> {
    static RUNTIMES: OnceLock<Mutex<HashMap<String, SandboxRuntime>>> = OnceLock::new();
    RUNTIMES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn execute(
    cfg: &Config,
    session: &str,
    args: &Value,
    cancel: &AtomicBool,
    vision_available: bool,
) -> Result<String, String> {
    let action = action_name(args);
    let key = runtime_key(cfg, session);
    if matches!(action.as_str(), "stop" | "close") {
        let stopped = runtimes()
            .lock()
            .map_err(|_| "隔离桌面运行时锁已损坏")?
            .remove(&key)
            .is_some();
        return Ok(json!({"backend":"isolated_wayland","stopped":stopped}).to_string());
    }
    if action == "capabilities" || action == "status" {
        let running = runtimes()
            .lock()
            .map_err(|_| "隔离桌面运行时锁已损坏")?
            .contains_key(&key);
        return capabilities(running, vision_available);
    }
    let mut guard = runtimes().lock().map_err(|_| "隔离桌面运行时锁已损坏")?;
    if !guard.contains_key(&key) {
        guard.insert(key.clone(), start_sandbox(cfg, session, args, cancel)?);
    }
    let runtime = guard.get_mut(&key).ok_or("隔离桌面运行时启动失败")?;
    if runtime
        .compositor
        .try_wait()
        .map_err(|e| e.to_string())?
        .is_some()
    {
        *runtime = start_sandbox(cfg, session, args, cancel)?;
    }
    runtime
        .apps
        .retain_mut(|child| child.try_wait().ok().flatten().is_none());

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
        return capture(cfg, session, runtime, cancel, vision_available);
    }
    run_action(cfg, session, runtime, args, cancel, true, vision_available)
}

fn capabilities(running: bool, vision_available: bool) -> Result<String, String> {
    Ok(serde_json::to_string_pretty(&json!({
        "available": command_exists("sway") && command_exists("grim") && command_exists("wtype"),
        "running":running,
        "backend":"isolated_headless_sway",
        "isolation":{"wayland_socket":true,"seat":true,"focus":true,"pointer":true,"keyboard":true,"clipboard":true},
        "host_pointer":false,
        "host_keyboard":false,
        "requires_host_focus":false,
        "vision":vision_available,
        "actions":["launch","open_url","observe","list_windows","focus_window","click","double_click","move","drag","scroll","type_text","send_text","press_key","wait","stop"],
        "missing": [if command_exists("sway") {Value::Null} else {json!("sway")}, if command_exists("grim") {Value::Null} else {json!("grim")}, if command_exists("wtype") {Value::Null} else {json!("wtype")}]
    })).map_err(|e| e.to_string())?)
}

fn run_action(
    cfg: &Config,
    session: &str,
    runtime: &mut SandboxRuntime,
    args: &Value,
    cancel: &AtomicBool,
    capture_after: bool,
    vision_available: bool,
) -> Result<String, String> {
    match action_name(args).as_str() {
        "observe" | "screenshot" => capture(cfg, session, runtime, cancel, vision_available),
        "launch" | "run_app" | "run-app" => {
            launch_app(runtime, args)?;
            wait_after(args, cancel, 700)?;
            advance_ui_generation(cfg, session)?;
            result_or_capture(capture_after, "应用已在隔离桌面启动", || capture(cfg, session, runtime, cancel, vision_available))
        }
        "open_url" => {
            let url = args.get("url").and_then(Value::as_str).ok_or("open_url 缺少 url")?;
            validate_web_url(url)?;
            launch_browser(runtime, url)?;
            wait_after(args, cancel, 1000)?;
            advance_ui_generation(cfg, session)?;
            result_or_capture(capture_after, "网址已在隔离桌面打开", || capture(cfg, session, runtime, cancel, vision_available))
        }
        "list_outputs" | "outputs" => Ok(json!([{"name":"HEADLESS-1","width":runtime.width,"height":runtime.height,"isolated":true}]).to_string()),
        "list_windows" | "windows" => list_windows(runtime, cancel),
        "focus_window" | "focus" => {
            let id = required_u64(args, "window_id")?;
            swaymsg(runtime, &[format!("[con_id={id}]"), "focus".into()], cancel)?;
            advance_ui_generation(cfg, session)?;
            result_or_capture(capture_after, "隔离窗口已聚焦", || capture(cfg, session, runtime, cancel, vision_available))
        }
        "click" | "double_click" | "double-click" => {
            let meta = isolated_frame(cfg, session, args)?;
            let (x, y) = required_point(args)?;
            validate_point(&meta, x, y)?;
            let button = pointer_button(args.get("button").and_then(Value::as_str).unwrap_or("left"))?;
            let count = if action_name(args) == "click" { 1 } else { 2 };
            runtime.pointer.request(PointerAction::Click { x:x as u32, y:y as u32, width:runtime.width, height:runtime.height, button, count })?;
            advance_ui_generation(cfg, session)?;
            result_or_capture(capture_after, "隔离桌面点击完成", || capture(cfg, session, runtime, cancel, vision_available))
        }
        "move" | "move_pointer" | "move-pointer" => {
            let meta = isolated_frame(cfg, session, args)?;
            let (x, y) = required_point(args)?;
            validate_point(&meta, x, y)?;
            runtime.pointer.request(PointerAction::Move { x:x as u32, y:y as u32, width:runtime.width, height:runtime.height })?;
            Ok("隔离桌面指针已移动（宿主鼠标未移动）".into())
        }
        "drag" => {
            let meta = isolated_frame(cfg, session, args)?;
            let from = (required_f64(args,"from_x")?, required_f64(args,"from_y")?);
            let to = (required_f64(args,"to_x")?, required_f64(args,"to_y")?);
            validate_point(&meta, from.0, from.1)?; validate_point(&meta, to.0, to.1)?;
            let button = pointer_button(args.get("button").and_then(Value::as_str).unwrap_or("left"))?;
            runtime.pointer.request(PointerAction::Drag { from:(from.0 as u32,from.1 as u32), to:(to.0 as u32,to.1 as u32), width:runtime.width, height:runtime.height, button })?;
            advance_ui_generation(cfg, session)?;
            result_or_capture(capture_after, "隔离桌面拖拽完成", || capture(cfg, session, runtime, cancel, vision_available))
        }
        "scroll" => {
            let meta = isolated_frame(cfg, session, args)?;
            let x = args.get("x").and_then(Value::as_f64).unwrap_or(meta.pixel_width as f64 / 2.0);
            let y = args.get("y").and_then(Value::as_f64).unwrap_or(meta.pixel_height as f64 / 2.0);
            validate_point(&meta, x, y)?;
            let steps = args.get("steps").and_then(Value::as_i64).unwrap_or(3).clamp(-50,50) as i32;
            if steps == 0 { return Err("steps 不能为 0".into()); }
            runtime.pointer.request(PointerAction::Scroll { x:x as u32, y:y as u32, width:runtime.width, height:runtime.height, steps })?;
            advance_ui_generation(cfg, session)?;
            result_or_capture(capture_after, "隔离桌面滚动完成", || capture(cfg, session, runtime, cancel, vision_available))
        }
        "type" | "type_text" | "type-text" => {
            let text = args.get("text").and_then(Value::as_str).ok_or("type_text 缺少 text")?;
            if args.get("x").is_some() || args.get("y").is_some() {
                let meta = isolated_frame(cfg, session, args)?;
                let (x, y) = required_point(args)?;
                validate_point(&meta, x, y)?;
                runtime.pointer.request(PointerAction::Click {
                    x: x as u32,
                    y: y as u32,
                    width: runtime.width,
                    height: runtime.height,
                    button: pointer_button("left")?,
                    count: 1,
                })?;
                cancellable_sleep(Duration::from_millis(120), cancel)?;
            }
            isolated_type_text(runtime, text, cancel)?;
            advance_ui_generation(cfg, session)?;
            result_or_capture(capture_after, "隔离桌面文字输入完成", || capture(cfg, session, runtime, cancel, vision_available))
        }
        "send_text" | "send-text" | "submit_text" | "submit-text" => {
            let text = args.get("text").and_then(Value::as_str).ok_or("send_text 缺少 text")?;
            let meta = isolated_frame(cfg, session, args)?;
            let (x, y) = required_point(args).map_err(|_| {
                "send_text 必须提供输入框 x/y 和最新 frame_id；工具会先聚焦输入框再粘贴，避免把文字发给错误控件".to_string()
            })?;
            validate_point(&meta, x, y)?;
            runtime.pointer.request(PointerAction::Click {
                x: x as u32,
                y: y as u32,
                width: runtime.width,
                height: runtime.height,
                button: pointer_button("left")?,
                count: 1,
            })?;
            cancellable_sleep(Duration::from_millis(150), cancel)?;
            isolated_type_text(runtime, text, cancel)?;
            cancellable_sleep(Duration::from_millis(120), cancel)?;
            let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(true);
            if submit {
                let key_args = wtype_key_args("Return")?;
                let output = run_in_sandbox(runtime, "wtype", &key_args, INPUT_TIMEOUT, cancel)?;
                command_success("wtype", output)?;
            }
            advance_ui_generation(cfg, session)?;
            result_or_capture(capture_after, "隔离桌面文字已可靠输入并提交", || capture(cfg, session, runtime, cancel, vision_available))
        }
        "key" | "press_key" | "press-key" => {
            let keys = args.get("keys").or_else(|| args.get("key")).and_then(Value::as_str).ok_or("press_key 缺少 keys")?;
            let key_args = wtype_key_args(keys)?;
            let output = run_in_sandbox(runtime, "wtype", &key_args, INPUT_TIMEOUT, cancel)?;
            command_success("wtype", output)?;
            advance_ui_generation(cfg, session)?;
            result_or_capture(capture_after, "隔离桌面按键完成", || capture(cfg, session, runtime, cancel, vision_available))
        }
        "wait" => {
            wait_after(args, cancel, 500)?;
            result_or_capture(capture_after, "等待完成", || capture(cfg, session, runtime, cancel, vision_available))
        }
        other => Err(format!("隔离桌面不支持 action={other}")),
    }
}

fn start_sandbox(
    cfg: &Config,
    session: &str,
    args: &Value,
    cancel: &AtomicBool,
) -> Result<SandboxRuntime, String> {
    for name in ["sway", "grim", "wtype"] {
        if !command_exists(name) {
            return Err(format!(
                "隔离桌面缺少 {name}；Fedora 请安装 sway grim wtype"
            ));
        }
    }
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
        .join("desktop")
        .join(safe_session(session));
    let runtime_dir = root.join("runtime");
    fs::create_dir_all(&runtime_dir).map_err(|e| format!("创建隔离桌面目录失败: {e}"))?;
    fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    for entry in fs::read_dir(&runtime_dir).into_iter().flatten().flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("wayland-") {
            let _ = fs::remove_file(entry.path());
        }
    }
    let config_path = root.join("sway.conf");
    let log_path = root.join("sway.log");
    fs::write(&config_path, format!("output HEADLESS-1 resolution {width}x{height}\nseat seat0 hide_cursor 3000\nfocus_follows_mouse no\ndefault_border pixel 1\nfont monospace 10\n"))
        .map_err(|e| format!("写入隔离 Sway 配置失败: {e}"))?;
    let log = fs::File::create(&log_path).map_err(|e| e.to_string())?;
    let mut command = Command::new("sway");
    command
        .args(["--unsupported-gpu", "-c"])
        .arg(&config_path)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("WLR_BACKENDS", "headless")
        .env("WLR_RENDERER", "pixman")
        .env("WLR_RENDERER_ALLOW_SOFTWARE", "1")
        .env("WLR_LIBINPUT_NO_DEVICES", "1")
        .env("XDG_SESSION_TYPE", "wayland")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env_remove("NIRI_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    let mut compositor = command
        .spawn()
        .map_err(|e| format!("启动 headless Sway 失败: {e}"))?;
    let started = std::time::Instant::now();
    let socket = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = compositor.kill();
            let _ = compositor.wait();
            return Err("启动隔离桌面时被取消".into());
        }
        if let Some(path) = find_wayland_socket(&runtime_dir) {
            break path;
        }
        if started.elapsed() > Duration::from_secs(10) {
            let log = fs::read_to_string(&log_path).unwrap_or_default();
            let _ = compositor.kill();
            let _ = compositor.wait();
            return Err(format!(
                "headless Sway 未在 10 秒内启动: {}",
                compact_error(&log)
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let pointer = match PointerWorker::start(socket.clone()) {
        Ok(pointer) => pointer,
        Err(error) => {
            let _ = compositor.kill();
            let _ = compositor.wait();
            return Err(error);
        }
    };
    let display = socket
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or("Wayland socket 名称无效")?;
    let keyboard_keeper = match sandbox_command(&runtime_dir, display, "wtype")
        .args(["-s", "999999999", "-k", "F15", "-k", "F15"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            pointer.stop();
            let _ = compositor.kill();
            let _ = compositor.wait();
            return Err(format!("启动隔离虚拟键盘失败: {error}"));
        }
    };
    std::thread::sleep(Duration::from_millis(250));
    Ok(SandboxRuntime {
        compositor,
        keyboard_keeper,
        apps: Vec::new(),
        pointer,
        runtime_dir,
        socket,
        width,
        height,
    })
}

fn capture(
    cfg: &Config,
    session: &str,
    runtime: &mut SandboxRuntime,
    cancel: &AtomicBool,
    vision_available: bool,
) -> Result<String, String> {
    let frame_id = next_frame_id();
    let dir = frame_dir(cfg, session);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{frame_id}.png"));
    let output = run_in_sandbox(
        runtime,
        "grim",
        &[
            "-s".into(),
            "1".into(),
            "-l".into(),
            "1".into(),
            path.to_string_lossy().into_owned(),
        ],
        CAPTURE_TIMEOUT,
        cancel,
    )?;
    command_success("grim", output)?;
    let (pixel_width, pixel_height) = png_size(&path)?;
    let meta = FrameMeta {
        frame_id,
        image_path: path.to_string_lossy().into_owned(),
        target: "isolated".into(),
        output: Some("HEADLESS-1".into()),
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

fn launch_browser(runtime: &mut SandboxRuntime, url: &str) -> Result<(), String> {
    let display = runtime
        .socket
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or("Wayland socket 名称无效")?;
    let profile = runtime
        .runtime_dir
        .parent()
        .unwrap_or(&runtime.runtime_dir)
        .join("firefox-profile");
    fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
    let mut command = if command_exists("firefox") {
        let mut c = sandbox_command(&runtime.runtime_dir, display, "firefox");
        c.env("MOZ_ENABLE_WAYLAND", "1")
            .args(["--no-remote", "--profile"])
            .arg(profile)
            .arg(url);
        c
    } else if let Some(chrome) = [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
    ]
    .into_iter()
    .find(|name| command_exists(name))
    {
        let mut c = sandbox_command(&runtime.runtime_dir, display, chrome);
        c.arg(url);
        c
    } else {
        return Err("隔离桌面没有找到 Firefox/Chromium".into());
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    runtime.apps.push(
        command
            .spawn()
            .map_err(|e| format!("启动隔离浏览器失败: {e}"))?,
    );
    Ok(())
}

fn launch_app(runtime: &mut SandboxRuntime, args: &Value) -> Result<(), String> {
    let program = args
        .get("program")
        .or_else(|| args.get("app"))
        .or_else(|| args.get("command"))
        .and_then(Value::as_str)
        .ok_or("launch 缺少 program")?;
    if program.contains('/') && !Path::new(program).is_file() {
        return Err("program 路径不存在".into());
    }
    let child_args: Vec<String> = args
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or("launch args 必须全是字符串")
                })
                .collect()
        })
        .transpose()?
        .unwrap_or_default();
    let display = runtime
        .socket
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or("Wayland socket 名称无效")?;
    let mut command = sandbox_command(&runtime.runtime_dir, display, program);
    command
        .args(&child_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    runtime.apps.push(
        command
            .spawn()
            .map_err(|e| format!("在隔离桌面启动 {program} 失败: {e}"))?,
    );
    Ok(())
}

#[derive(Serialize)]
struct WindowInfo {
    window_id: u64,
    title: String,
    app_id: String,
    pid: Option<u64>,
    focused: bool,
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

fn list_windows(runtime: &SandboxRuntime, cancel: &AtomicBool) -> Result<String, String> {
    let raw = swaymsg(
        runtime,
        &["-t".into(), "get_tree".into(), "-r".into()],
        cancel,
    )?;
    let root: Value =
        serde_json::from_str(&raw).map_err(|e| format!("解析隔离 Sway 窗口树失败: {e}"))?;
    let mut windows = Vec::new();
    collect_windows(&root, &mut windows);
    serde_json::to_string(&windows).map_err(|e| e.to_string())
}

fn collect_windows(node: &Value, out: &mut Vec<WindowInfo>) {
    let app_id = node.get("app_id").and_then(Value::as_str).unwrap_or("");
    let window_props = node.get("window_properties");
    if !app_id.is_empty() || window_props.is_some_and(|v| !v.is_null()) {
        let rect = node.get("rect").unwrap_or(&Value::Null);
        out.push(WindowInfo {
            window_id: node.get("id").and_then(Value::as_u64).unwrap_or(0),
            title: node
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            app_id: if app_id.is_empty() {
                window_props
                    .and_then(|v| v.get("class"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into()
            } else {
                app_id.into()
            },
            pid: node.get("pid").and_then(Value::as_u64),
            focused: node
                .get("focused")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            x: rect.get("x").and_then(Value::as_i64).unwrap_or(0),
            y: rect.get("y").and_then(Value::as_i64).unwrap_or(0),
            width: rect.get("width").and_then(Value::as_i64).unwrap_or(0),
            height: rect.get("height").and_then(Value::as_i64).unwrap_or(0),
        });
    }
    for key in ["nodes", "floating_nodes"] {
        for child in node
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            collect_windows(child, out);
        }
    }
}

fn swaymsg(
    runtime: &SandboxRuntime,
    args: &[String],
    cancel: &AtomicBool,
) -> Result<String, String> {
    let mut command = Command::new("swaymsg");
    command
        .arg("-s")
        .arg(&runtime.socket)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_command_bounded(command, "swaymsg", INPUT_TIMEOUT, cancel)?;
    command_success(
        "swaymsg",
        ProcessOutput {
            stdout: output.stdout.clone(),
            stderr: output.stderr.clone(),
            code: output.code,
        },
    )?;
    String::from_utf8(output.stdout).map_err(|e| e.to_string())
}

fn run_in_sandbox(
    runtime: &SandboxRuntime,
    program: &str,
    args: &[String],
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<ProcessOutput, String> {
    let display = runtime
        .socket
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or("Wayland socket 名称无效")?;
    let mut command = sandbox_command(&runtime.runtime_dir, display, program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_command_bounded(command, program, timeout, cancel)
}

fn run_command_bounded(
    mut command: Command,
    name: &str,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<ProcessOutput, String> {
    let mut child = command
        .spawn()
        .map_err(|e| format!("启动 {name} 失败: {e}"))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_reader = std::thread::spawn(move || read_pipe(stdout));
    let err_reader = std::thread::spawn(move || read_pipe(stderr));
    let started = std::time::Instant::now();
    let code = loop {
        if cancel.load(Ordering::Relaxed) || started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{name} 已取消或超时"));
        }
        match child.try_wait() {
            Ok(Some(s)) => break s.code().unwrap_or(-1),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(e.to_string()),
        }
    };
    Ok(ProcessOutput {
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
        code,
    })
}

fn sandbox_command(runtime_dir: &Path, display: &str, program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("WAYLAND_DISPLAY", display)
        .env("XDG_SESSION_TYPE", "wayland")
        .env_remove("DISPLAY")
        .env_remove("NIRI_SOCKET");
    command
}

/// QQ/Chromium/Electron 对 wtype 直接注入 CJK 字符的处理并不一致。尤其是 QQ，
/// Unicode 注入序列可能被解释成导航快捷键，从而把刚打开的聊天面板清空。
/// 隔离桌面拥有独立剪贴板，因此优先把原文写入该桌面的 clipboard，再只注入稳定的
/// Ctrl+V。宿主剪贴板不会被修改。缺少 wl-copy 时仅允许 ASCII 直输，拒绝拿中文冒险。
fn isolated_type_text(
    runtime: &SandboxRuntime,
    text: &str,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if command_exists("wl-copy") {
        set_isolated_clipboard(runtime, text, cancel)?;
        let paste = wtype_key_args("CTRL+V")?;
        let output = run_in_sandbox(runtime, "wtype", &paste, INPUT_TIMEOUT, cancel)?;
        return command_success("wtype paste", output);
    }
    if !text.is_ascii() {
        return Err(
            "隔离桌面输入中文需要 wl-copy；请安装 wl-clipboard。已拒绝使用可能让 QQ 会话面板关闭的 wtype Unicode 直输"
                .into(),
        );
    }
    let output = run_in_sandbox(
        runtime,
        "wtype",
        &["--".into(), text.into()],
        INPUT_TIMEOUT,
        cancel,
    )?;
    command_success("wtype", output)
}

fn set_isolated_clipboard(
    runtime: &SandboxRuntime,
    text: &str,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let display = runtime
        .socket
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or("Wayland socket 名称无效")?;
    let mut command = sandbox_command(&runtime.runtime_dir, display, "wl-copy");
    command
        .args(["--type", "text/plain;charset=utf-8"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("启动隔离剪贴板失败: {error}"))?;
    child
        .stdin
        .take()
        .ok_or("无法写入隔离剪贴板")?
        .write_all(text.as_bytes())
        .map_err(|error| format!("写入隔离剪贴板失败: {error}"))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_reader = std::thread::spawn(move || read_pipe(stdout));
    let err_reader = std::thread::spawn(move || read_pipe(stderr));
    let started = std::time::Instant::now();
    let code = loop {
        if cancel.load(Ordering::Relaxed) || started.elapsed() > INPUT_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err("wl-copy 已取消或超时".into());
        }
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(-1),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(error.to_string()),
        }
    };
    command_success(
        "wl-copy",
        ProcessOutput {
            stdout: out_reader.join().unwrap_or_default(),
            stderr: err_reader.join().unwrap_or_default(),
            code,
        },
    )
}

fn wtype_key_args(keys: &str) -> Result<Vec<String>, String> {
    let parts: Vec<_> = keys
        .split('+')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .collect();
    if parts.is_empty() {
        return Err("keys 不能为空".into());
    }
    let mut args = Vec::new();
    for modifier in &parts[..parts.len() - 1] {
        args.push("-M".into());
        args.push(normalize_modifier(modifier)?.into());
    }
    args.push("-k".into());
    args.push(parts[parts.len() - 1].into());
    for modifier in parts[..parts.len() - 1].iter().rev() {
        args.push("-m".into());
        args.push(normalize_modifier(modifier)?.into());
    }
    Ok(args)
}

fn find_wayland_socket(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|v| v.to_str())
                .is_some_and(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
        })
}
fn action_name(args: &Value) -> String {
    args.get("action")
        .and_then(Value::as_str)
        .unwrap_or("observe")
        .trim()
        .to_ascii_lowercase()
}
fn runtime_key(cfg: &Config, session: &str) -> String {
    format!("{}:{}", cfg.data_dir().display(), safe_session(session))
}
fn safe_session(session: &str) -> String {
    let s: String = session
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
    if s.is_empty() {
        "default".into()
    } else {
        s
    }
}
fn compact_error(text: &str) -> String {
    text.lines()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" | ")
}
fn isolated_frame(cfg: &Config, session: &str, args: &Value) -> Result<FrameMeta, String> {
    let latest = load_latest_meta(cfg, session)?;
    let meta = match args.get("frame_id").and_then(Value::as_str) {
        Some(id) if id != latest.frame_id => load_frame_meta(cfg, session, id)?,
        _ => latest,
    };
    if meta.target != "isolated" {
        return Err("frame_id 不是隔离桌面截图，请重新 observe".into());
    }
    if meta.generation != current_ui_generation(cfg, session) {
        return Err("frame_id 已过期，请重新 observe".into());
    }
    Ok(meta)
}
fn validate_point(meta: &FrameMeta, x: f64, y: f64) -> Result<(), String> {
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
            "坐标 ({x},{y}) 超出隔离桌面截图 {}x{}",
            meta.pixel_width, meta.pixel_height
        ))
    }
}
fn result_or_capture<F: FnOnce() -> Result<String, String>>(
    after: bool,
    text: &str,
    capture: F,
) -> Result<String, String> {
    if after {
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

enum PointerAction {
    Move {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
    Click {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        button: u32,
        count: usize,
    },
    Scroll {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        steps: i32,
    },
    Drag {
        from: (u32, u32),
        to: (u32, u32),
        width: u32,
        height: u32,
        button: u32,
    },
}

struct PointerMessage {
    action: Option<PointerAction>,
    reply: mpsc::Sender<Result<(), String>>,
}

struct PointerWorker {
    tx: mpsc::Sender<PointerMessage>,
}
impl PointerWorker {
    fn start(socket: PathBuf) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::spawn(move || pointer_loop(socket, rx, ready_tx));
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "隔离虚拟指针启动超时".to_string())??;
        Ok(Self { tx })
    }
    fn request(&self, action: PointerAction) -> Result<(), String> {
        let (reply, result) = mpsc::channel();
        self.tx
            .send(PointerMessage {
                action: Some(action),
                reply,
            })
            .map_err(|_| "隔离虚拟指针已退出".to_string())?;
        result
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| "隔离虚拟指针 3 秒未响应，拒绝谎报成功".to_string())?
    }

    fn stop(&self) {
        let (reply, _result) = mpsc::channel();
        let _ = self.tx.send(PointerMessage {
            action: None,
            reply,
        });
    }
}

#[derive(Default)]
struct PointerState;
wayland_client::delegate_noop!(PointerState:ignore ZwlrVirtualPointerManagerV1);
wayland_client::delegate_noop!(PointerState:ignore ZwlrVirtualPointerV1);
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for PointerState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

fn pointer_loop(
    socket: PathBuf,
    rx: mpsc::Receiver<PointerMessage>,
    ready: mpsc::Sender<Result<(), String>>,
) {
    let result = (|| {
        let stream =
            UnixStream::connect(&socket).map_err(|e| format!("连接隔离 Wayland 失败: {e}"))?;
        let connection = Connection::from_socket(stream).map_err(|e| e.to_string())?;
        let (globals, mut queue) =
            registry_queue_init::<PointerState>(&connection).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let manager: ZwlrVirtualPointerManagerV1 = globals
            .bind(&qh, 1..=2, ())
            .map_err(|e| format!("隔离 Sway 缺少 virtual pointer: {e}"))?;
        let pointer = manager.create_virtual_pointer(None, &qh, ());
        let mut state = PointerState;
        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
        ready.send(Ok(())).ok();
        while let Ok(message) = rx.recv() {
            let Some(action) = message.action else { break };
            let action_result = (|| {
                match action {
                    PointerAction::Move {
                        x,
                        y,
                        width,
                        height,
                    } => motion(&pointer, x, y, width, height),
                    PointerAction::Click {
                        x,
                        y,
                        width,
                        height,
                        button,
                        count,
                    } => {
                        motion(&pointer, x, y, width, height);
                        for _ in 0..count {
                            pointer.button(event_time_ms(), button, ButtonState::Pressed);
                            pointer.frame();
                            pointer.button(event_time_ms(), button, ButtonState::Released);
                            pointer.frame();
                        }
                    }
                    PointerAction::Scroll {
                        x,
                        y,
                        width,
                        height,
                        steps,
                    } => {
                        motion(&pointer, x, y, width, height);
                        pointer.axis(event_time_ms(), Axis::VerticalScroll, steps as f64 * 15.0);
                        pointer.frame();
                    }
                    PointerAction::Drag {
                        from,
                        to,
                        width,
                        height,
                        button,
                    } => {
                        motion(&pointer, from.0, from.1, width, height);
                        pointer.button(event_time_ms(), button, ButtonState::Pressed);
                        pointer.frame();
                        for step in 1..=16 {
                            let x =
                                from.0 as f64 + (to.0 as f64 - from.0 as f64) * step as f64 / 16.0;
                            let y =
                                from.1 as f64 + (to.1 as f64 - from.1 as f64) * step as f64 / 16.0;
                            motion(&pointer, x as u32, y as u32, width, height);
                        }
                        pointer.button(event_time_ms(), button, ButtonState::Released);
                        pointer.frame();
                    }
                }
                connection.flush().map_err(|e| e.to_string())?;
                queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
                Ok::<(), String>(())
            })();
            let failed = action_result.is_err();
            let _ = message.reply.send(action_result);
            if failed {
                break;
            }
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = result {
        let _ = ready.send(Err(error));
    }
}
fn motion(pointer: &ZwlrVirtualPointerV1, x: u32, y: u32, width: u32, height: u32) {
    pointer.motion_absolute(
        event_time_ms(),
        x.min(width.saturating_sub(1)),
        y.min(height.saturating_sub(1)),
        width.max(1),
        height.max(1),
    );
    pointer.frame();
}
fn event_time_ms() -> u32 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } == 0 {
        (now.tv_sec as u64 * 1000 + now.tv_nsec as u64 / 1_000_000) as u32
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sandbox_session_cannot_escape() {
        assert_eq!(safe_session("../../x"), "______x");
    }
    #[test]
    fn window_collector_finds_nested_nodes() {
        let tree = json!({"nodes":[{"id":7,"name":"App","app_id":"demo","pid":2,"focused":true,"rect":{"x":1,"y":2,"width":3,"height":4},"nodes":[],"floating_nodes":[]}],"floating_nodes":[]});
        let mut out = Vec::new();
        collect_windows(&tree, &mut out);
        assert_eq!(out[0].window_id, 7);
    }
}
