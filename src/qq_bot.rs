//! NapCat/OneBot 的本机生命周期管理。
//!
//! 这个模块故意不把 WebUI token 交给模型。模型只需要调用 `qq_bot {}`，工具会在
//! 本机创建或接管 NapCat、保存 0600 凭据，并打开 NapCat 自己维护的完整 WebUI
//! 登录页。Yujiale Code 不复制二维码、不刷新页面；只观察登录状态并管理 OneBot 连接。

use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::config::Config;

const DEFAULT_CONTAINER: &str = "napcat";
const DEFAULT_IMAGE: &str = "mlikiowa/napcat-docker:latest";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoginState {
    LoggedIn,
    ScanRequired,
    Starting,
}

#[derive(Debug, Clone)]
struct Docker {
    sudo: bool,
}

impl Docker {
    fn detect() -> Result<Self, String> {
        if command_ok("docker", &["info"]) {
            return Ok(Self { sudo: false });
        }
        if command_ok("sudo", &["-n", "docker", "info"]) {
            return Ok(Self { sudo: true });
        }
        Err(
            "未找到可用 Docker。请先安装 Docker，并让当前用户可直接使用，或配置免密 sudo docker"
                .into(),
        )
    }

    fn output(&self, args: &[&str]) -> Result<Output, String> {
        let mut command = if self.sudo {
            let mut command = Command::new("sudo");
            command.args(["-n", "docker"]);
            command
        } else {
            Command::new("docker")
        };
        command.args(args);
        run_output_timeout(command, Duration::from_secs(20), "Docker")
    }

    fn checked(&self, args: &[&str], label: &str) -> Result<Output, String> {
        let output = self.output(args)?;
        if output.status.success() {
            Ok(output)
        } else {
            let error = String::from_utf8_lossy(&output.stderr);
            Err(format!("{label}失败: {}", sanitize_error(&error)))
        }
    }

    fn container_exists(&self, name: &str) -> bool {
        self.output(&["inspect", name])
            .is_ok_and(|output| output.status.success())
    }

    fn container_running(&self, name: &str) -> bool {
        self.output(&["inspect", "--format", "{{.State.Running}}", name])
            .ok()
            .filter(|output| output.status.success())
            .is_some_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "true")
    }
}

/// NapCat WebUI 把配置 token 的 SHA-256 摘要换成短期 API credential。
/// credential 只存在于本次工具调用的内存中；状态响应也只读取布尔值，二维码 URL
/// 和任何 token 都不会进入模型上下文。
#[derive(Debug)]
struct WebUiStatusClient {
    check_url: String,
    credential: String,
}

impl WebUiStatusClient {
    fn connect(args: &Value, token: &str) -> Result<Self, String> {
        let (webui, _, _) = webui_url(args, token)?;
        let origin = webui
            .split_once("/webui/")
            .map(|(origin, _)| origin)
            .ok_or("NapCat WebUI URL 格式异常")?;
        let salted = format!("{token}.napcat");
        let digest = ring::digest::digest(&ring::digest::SHA256, salted.as_bytes());
        let hash = digest
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let response = ureq::post(&format!("{origin}/api/auth/login"))
            .timeout(Duration::from_secs(3))
            .set("Content-Type", "application/json")
            .send_string(&json!({"hash":hash,"totpCode":""}).to_string())
            .map_err(|_| "NapCat WebUI 状态认证失败".to_string())?;
        let value: Value = serde_json::from_str(
            &response
                .into_string()
                .map_err(|_| "NapCat WebUI 状态认证响应不可读")?,
        )
        .map_err(|_| "NapCat WebUI 状态认证响应不是 JSON")?;
        let credential = ["Credential", "credential", "token"]
            .iter()
            .find_map(|name| {
                value
                    .pointer(&format!("/data/{name}"))
                    .and_then(Value::as_str)
            })
            .filter(|value| !value.is_empty())
            .ok_or("NapCat WebUI 未返回状态凭据")?;
        Ok(Self {
            check_url: format!("{origin}/api/QQLogin/CheckLoginStatus"),
            credential: credential.to_string(),
        })
    }

    fn state(&self) -> Result<LoginState, String> {
        let response = ureq::post(&self.check_url)
            .timeout(Duration::from_secs(3))
            .set("Authorization", &format!("Bearer {}", self.credential))
            .send_string("{}")
            .map_err(|_| "NapCat 登录状态接口请求失败".to_string())?;
        let value: Value = serde_json::from_str(
            &response
                .into_string()
                .map_err(|_| "NapCat 登录状态响应不可读")?,
        )
        .map_err(|_| "NapCat 登录状态响应不是 JSON")?;
        if value.get("code").and_then(Value::as_i64) != Some(0) {
            return Err("NapCat 登录状态接口拒绝请求".into());
        }
        let data = value.get("data").ok_or("NapCat 登录状态缺少 data")?;
        let logged_in = data
            .get("isLogin")
            .and_then(Value::as_bool)
            .ok_or("NapCat 登录状态缺少 isLogin")?;
        let offline = data
            .get("isOffline")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(if logged_in && !offline {
            LoginState::LoggedIn
        } else {
            LoginState::ScanRequired
        })
    }
}

pub fn execute(
    cfg: &Config,
    args: &Value,
    cancel: &AtomicBool,
    _vision_available: bool,
) -> Result<String, String> {
    if !cfg!(target_os = "linux") {
        return Err("qq_bot 的 NapCat 自动扫码管理当前仅支持 Linux Docker".into());
    }
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("login")
        .trim()
        .to_ascii_lowercase();
    let container = safe_container_name(
        args.get("container")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_CONTAINER),
    )?;
    let docker = Docker::detect()?;
    let root = bot_root(cfg);
    ensure_private_dir(&root)?;
    let credentials = root.join("credentials.env");

    match action.as_str() {
        "login" | "setup" | "qr" | "qrcode" => {
            ensure_container(cfg, args, &docker, &container, &root, &credentials)?;
            let token = ensure_credentials(&docker, &container, &credentials)?;
            let status_client = WebUiStatusClient::connect(args, &token).ok();
            let state = wait_for_state(
                &docker,
                &container,
                args,
                status_client.as_ref(),
                20,
                cancel,
            )?;
            if state == LoginState::LoggedIn {
                return Ok(status_json(&container, true, "logged_in", None));
            }
            let (url, host, port) = webui_url(args, &token)?;
            if args.get("open").and_then(Value::as_bool).unwrap_or(true) {
                wait_for_webui(&host, port, 30, cancel)?;
                open_target(&url)?;
            }
            let wait_secs = args
                .get("wait_secs")
                .and_then(Value::as_u64)
                .unwrap_or(120)
                .clamp(1, 600);
            let state = wait_for_login(
                &docker,
                &container,
                args,
                status_client.as_ref(),
                wait_secs,
                cancel,
            )?;
            let (state_name, note) = if state == LoginState::LoggedIn {
                (
                    "logged_in",
                    "用户已在 NapCat 原生 WebUI 完成扫码；OneBot 会自动连接 Yujiale Code",
                )
            } else {
                (
                    "scan_required",
                    "NapCat 原生 WebUI 已打开且仍在等待扫码；不要刷新页面，扫码后可调用 qq_bot action=wait 继续判断",
                )
            };
            Ok(status_json(&container, true, state_name, Some(note)))
        }
        "status" | "check" => {
            if !docker.container_running(&container) {
                return Ok(status_json(&container, false, "stopped", None));
            }
            let token = ensure_credentials(&docker, &container, &credentials)?;
            let status_client = WebUiStatusClient::connect(args, &token).ok();
            let state = detect_login_state(&docker, &container, args, status_client.as_ref())?;
            let state_name = match state {
                LoginState::LoggedIn => "logged_in",
                LoginState::ScanRequired => "scan_required",
                LoginState::Starting => "starting",
            };
            Ok(status_json(&container, true, state_name, None))
        }
        "wait" | "wait_login" | "wait-login" => {
            if !docker.container_running(&container) {
                return Err("NapCat 容器未运行；先调用 qq_bot action=login".into());
            }
            let wait_secs = args
                .get("wait_secs")
                .and_then(Value::as_u64)
                .unwrap_or(120)
                .clamp(1, 600);
            let token = ensure_credentials(&docker, &container, &credentials)?;
            let status_client = WebUiStatusClient::connect(args, &token).ok();
            let state = wait_for_login(
                &docker,
                &container,
                args,
                status_client.as_ref(),
                wait_secs,
                cancel,
            )?;
            Ok(status_json(
                &container,
                true,
                if state == LoginState::LoggedIn {
                    "logged_in"
                } else {
                    "scan_required"
                },
                Some("扫码完成后 OneBot 会自动连接 Yujiale Code，无需手工复制 token"),
            ))
        }
        "start" => {
            ensure_container(cfg, args, &docker, &container, &root, &credentials)?;
            Ok(status_json(&container, true, "starting", None))
        }
        "restart" => {
            if !docker.container_exists(&container) {
                ensure_container(cfg, args, &docker, &container, &root, &credentials)?;
            } else {
                docker.checked(&["restart", &container], "重启 NapCat")?;
            }
            Ok(status_json(&container, true, "starting", None))
        }
        "stop" => {
            if docker.container_exists(&container) {
                docker.checked(&["stop", &container], "停止 NapCat")?;
            }
            Ok(status_json(&container, false, "stopped", None))
        }
        "webui" | "open_webui" | "open-webui" => {
            let token = ensure_credentials(&docker, &container, &credentials)?;
            let (url, host, port) = webui_url(args, &token)?;
            wait_for_webui(&host, port, 30, cancel)?;
            open_target(&url)?;
            Ok("NapCat WebUI 已在本机打开。令牌只用于本机 URL，未返回给模型".into())
        }
        other => Err(format!(
            "qq_bot 不支持 action={other}；可用 login/status/wait/start/restart/stop/webui"
        )),
    }
}

fn ensure_container(
    cfg: &Config,
    args: &Value,
    docker: &Docker,
    container: &str,
    root: &Path,
    credentials: &Path,
) -> Result<(), String> {
    ensure_credentials(docker, container, credentials)?;
    if docker.container_exists(container) {
        if !docker.container_running(container) {
            docker.checked(&["start", container], "启动现有 NapCat")?;
        }
        return Ok(());
    }

    let config_dir = root.join("config");
    let qq_dir = root.join("qq");
    let logs_dir = root.join("logs");
    for dir in [&config_dir, &qq_dir, &logs_dir] {
        ensure_private_dir(dir)?;
    }
    let image = args
        .get("image")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_IMAGE);
    if image.is_empty() || image.starts_with('-') || image.chars().any(char::is_whitespace) {
        return Err("NapCat image 格式不安全".into());
    }
    let onebot = args
        .get("onebot_url")
        .and_then(Value::as_str)
        .unwrap_or("ws://127.0.0.1:6701/onebot/v11/ws");
    let config_mount = format!("{}:/app/napcat/config", config_dir.display());
    let qq_mount = format!("{}:/app/.config/QQ", qq_dir.display());
    let logs_mount = format!("{}:/app/napcat/logs", logs_dir.display());
    let ws_urls = format!("WS_URLS=[\"{onebot}\"]");
    let uid = current_uid().to_string();
    let gid = current_gid().to_string();
    let cred = credentials.to_string_lossy().into_owned();
    let output = docker.checked(
        &[
            "run",
            "-d",
            "--name",
            container,
            "--network",
            "host",
            "--restart",
            "unless-stopped",
            "--env-file",
            &cred,
            "-e",
            "WSR_ENABLE=true",
            "-e",
            &ws_urls,
            "-e",
            "MESSAGE_POST_FORMAT=array",
            "-e",
            &format!("NAPCAT_UID={uid}"),
            "-e",
            &format!("NAPCAT_GID={gid}"),
            "-e",
            "TZ=Asia/Shanghai",
            "-v",
            &config_mount,
            "-v",
            &qq_mount,
            "-v",
            &logs_mount,
            image,
        ],
        "创建 NapCat",
    )?;
    let _ = cfg;
    if output.stdout.is_empty() {
        return Err("Docker 未返回容器 ID，拒绝谎报启动成功".into());
    }
    Ok(())
}

fn ensure_credentials(docker: &Docker, container: &str, path: &Path) -> Result<String, String> {
    if let Ok(content) = fs::read_to_string(path) {
        if let Some(token) = env_value(&content, "WEBUI_TOKEN") {
            return Ok(token.to_string());
        }
    }
    let imported = if docker.container_exists(container) {
        docker
            .output(&[
                "inspect",
                "--format",
                "{{range .Config.Env}}{{println .}}{{end}}",
                container,
            ])
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| {
                let text = String::from_utf8_lossy(&output.stdout);
                env_value(&text, "WEBUI_TOKEN").map(ToOwned::to_owned)
            })
    } else {
        None
    };
    let token = imported.unwrap_or(generate_token()?);
    let content = format!("WEBUI_TOKEN={token}\nACCOUNT=\n");
    write_private(path, content.as_bytes())?;
    Ok(token)
}

fn wait_for_webui(
    host: &str,
    port: u16,
    wait_secs: u64,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let addresses = format!("{host}:{port}")
        .to_socket_addrs()
        .map_err(|error| format!("解析 NapCat WebUI 地址失败: {error}"))?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err("NapCat WebUI 地址没有可连接目标".into());
    }
    let started = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("等待 NapCat WebUI 时被 Esc 取消".into());
        }
        if addresses
            .iter()
            .any(|address| TcpStream::connect_timeout(address, Duration::from_millis(300)).is_ok())
        {
            return Ok(());
        }
        if started.elapsed() >= Duration::from_secs(wait_secs) {
            return Err(format!(
                "NapCat 容器已启动，但 WebUI {host}:{port} 在 {wait_secs} 秒内未就绪"
            ));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn webui_url(args: &Value, token: &str) -> Result<(String, String, u16), String> {
    let scheme = args
        .get("scheme")
        .and_then(Value::as_str)
        .unwrap_or("http")
        .trim()
        .to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        return Err("NapCat WebUI scheme 只允许 http 或 https".into());
    }
    let host = args
        .get("host")
        .and_then(Value::as_str)
        .unwrap_or("127.0.0.1")
        .trim();
    if host.is_empty()
        || !host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | ':' | '-'))
    {
        return Err("NapCat WebUI host 格式不安全".into());
    }
    let port = args.get("port").and_then(Value::as_u64).unwrap_or(6099);
    let port = u16::try_from(port)
        .ok()
        .filter(|port| *port != 0)
        .ok_or("NapCat WebUI port 必须在 1..=65535")?;
    let display_host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let url = format!(
        "{scheme}://{display_host}:{port}/webui/?token={}",
        percent_encode_query(token)
    );
    Ok((url, host.to_string(), port))
}

fn percent_encode_query(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn detect_login_state(
    docker: &Docker,
    container: &str,
    args: &Value,
    status_client: Option<&WebUiStatusClient>,
) -> Result<LoginState, String> {
    let api_state = status_client.and_then(|client| client.state().ok());
    if api_state == Some(LoginState::LoggedIn) || onebot_connected(args) {
        return Ok(LoginState::LoggedIn);
    }
    let output = docker.checked(&["logs", "--tail", "500", container], "读取 NapCat 状态")?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let log_state = parse_login_state(&text);
    if log_state == LoginState::LoggedIn {
        Ok(log_state)
    } else {
        Ok(api_state.unwrap_or(log_state))
    }
}

fn wait_for_state(
    docker: &Docker,
    container: &str,
    args: &Value,
    status_client: Option<&WebUiStatusClient>,
    wait_secs: u64,
    cancel: &AtomicBool,
) -> Result<LoginState, String> {
    let started = Instant::now();
    loop {
        let state = detect_login_state(docker, container, args, status_client)?;
        if state != LoginState::Starting || started.elapsed() >= Duration::from_secs(wait_secs) {
            return Ok(state);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("检查 QQ 登录状态时被 Esc 取消".into());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn wait_for_login(
    docker: &Docker,
    container: &str,
    args: &Value,
    status_client: Option<&WebUiStatusClient>,
    wait_secs: u64,
    cancel: &AtomicBool,
) -> Result<LoginState, String> {
    let started = Instant::now();
    loop {
        let state = detect_login_state(docker, container, args, status_client)?;
        if state == LoginState::LoggedIn || started.elapsed() >= Duration::from_secs(wait_secs) {
            return Ok(state);
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("等待 QQ 扫码时被 Esc 取消".into());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn parse_login_state(text: &str) -> LoginState {
    let mut state = LoginState::Starting;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if line.contains("登录成功")
            || line.contains("已登录,无法重复登录")
            || line.contains("WebSocket反向服务:") && line.contains("已启动")
            || line.contains("OneBot11 适配器初始化完成")
            || lower.contains("qrcodeloginsucceed")
            || lower.contains("onebot11 server started")
        {
            state = LoginState::LoggedIn;
        } else if line.contains("请扫描")
            || line.contains("二维码已保存")
            || line.contains("二维码已更新")
            || lower.contains("login error,errtype")
        {
            state = LoginState::ScanRequired;
        }
    }
    state
}

/// 反向 OneBot WebSocket 只有 QQ 登录完成后才会连接到 Yujiale Code 的 6701 端口。
/// 这是独立于历史日志的当前态兜底，避免群消息刷屏后成功日志被挤出 tail 窗口。
fn onebot_connected(args: &Value) -> bool {
    let port = args
        .get("onebot_port")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port != 0)
        .unwrap_or(6701);
    let output = run_output_timeout(
        {
            let mut command = Command::new("ss");
            command.args(["-Htn", "state", "established"]);
            command
        },
        Duration::from_secs(2),
        "检查 OneBot 连接",
    );
    output.is_ok_and(|output| {
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                line.split_whitespace().any(|part| {
                    part.rsplit_once(':')
                        .is_some_and(|(_, value)| value == port.to_string())
                })
            })
    })
}

fn status_json(container: &str, running: bool, state: &str, note: Option<&str>) -> String {
    json!({
        "backend":"napcat_onebot11",
        "container":container,
        "running":running,
        "state":state,
        "token":"managed_locally",
        "note":note,
    })
    .to_string()
}

fn bot_root(cfg: &Config) -> PathBuf {
    cfg.data_dir().join("qq-bot")
}

fn ensure_private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|error| format!("创建 {} 失败: {error}", path.display()))?;
    set_mode(path, 0o700)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    fs::write(path, bytes).map_err(|error| format!("写入 {} 失败: {error}", path.display()))?;
    set_mode(path, 0o600)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), String> {
    Ok(())
}

fn generate_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| format!("生成安全 token 失败: {error}"))?;
    Ok(bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(""))
}

fn env_value<'a>(content: &'a str, name: &str) -> Option<&'a str> {
    content.lines().find_map(|line| {
        let (key, value) = line.trim().split_once('=')?;
        (key == name && !value.trim().is_empty()).then(|| value.trim().trim_matches('\''))
    })
}

fn open_target(target: &str) -> Result<(), String> {
    Command::new("xdg-open")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("打开本机页面失败: {error}"))
}

fn safe_container_name(name: &str) -> Result<String, String> {
    let valid = !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('-')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'));
    if valid {
        Ok(name.to_string())
    } else {
        Err("container 名称格式不安全".into())
    }
}

fn sanitize_error(text: &str) -> String {
    text.lines()
        .take(6)
        .map(|line| {
            if line.to_ascii_lowercase().contains("token") {
                "[包含敏感 token 的错误行已隐藏]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn command_ok(program: &str, args: &[&str]) -> bool {
    let mut command = Command::new(program);
    command.args(args);
    run_output_timeout(command, Duration::from_secs(5), program)
        .is_ok_and(|output| output.status.success())
}

fn run_output_timeout(
    mut command: Command,
    timeout: Duration,
    label: &str,
) -> Result<Output, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("启动 {label} 失败: {error}"))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = stdout {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = stderr {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{label} 超过 {} 秒，已终止", timeout.as_secs()));
            }
            Err(error) => return Err(format!("等待 {label} 失败: {error}")),
        }
    };
    Ok(Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

#[cfg(target_os = "linux")]
fn current_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(not(target_os = "linux"))]
fn current_uid() -> u32 {
    0
}

#[cfg(target_os = "linux")]
fn current_gid() -> u32 {
    unsafe { libc::getegid() }
}

#[cfg(not(target_os = "linux"))]
fn current_gid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_login_event_wins() {
        assert_eq!(
            parse_login_state("请扫描二维码\n二维码登录成功\nOneBot11 server started"),
            LoginState::LoggedIn
        );
        assert_eq!(
            parse_login_state(
                "请扫描下面的二维码\nWebSocket反向服务: ws://127.0.0.1:6701/onebot/v11/ws, : 已启动\nOneBot11 适配器初始化完成"
            ),
            LoginState::LoggedIn
        );
        assert_eq!(
            parse_login_state("登录成功\n被踢下线\n请扫描下面的二维码"),
            LoginState::ScanRequired
        );
    }

    #[test]
    fn token_is_read_without_being_exposed_elsewhere() {
        assert_eq!(
            env_value("WEBUI_TOKEN=abc123\nACCOUNT=", "WEBUI_TOKEN"),
            Some("abc123")
        );
    }

    #[test]
    fn container_name_is_strict() {
        assert!(safe_container_name("napcat-main_1").is_ok());
        assert!(safe_container_name("--privileged").is_err());
        assert!(safe_container_name("napcat;rm").is_err());
    }

    #[test]
    fn webui_url_opens_the_real_frontend_and_encodes_token() {
        let (url, host, port) =
            webui_url(&json!({"host":"127.0.0.1","port":6099}), "a token&secret").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 6099);
        assert_eq!(url, "http://127.0.0.1:6099/webui/?token=a%20token%26secret");
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "实机验收：需要正在运行或可启动的本机 NapCat Docker"]
    fn live_login_reuses_container_and_never_returns_token() {
        let root =
            std::env::temp_dir().join(format!("yjlcoder_qq_bot_live_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut cfg = Config::default();
        cfg.set_test_data_dir(root.clone());
        let output = execute(
            &cfg,
            &json!({"action":"login","open":false,"wait_secs":5}),
            &AtomicBool::new(false),
            false,
        )
        .unwrap();
        assert!(
            output.contains("scan_required") || output.contains("logged_in"),
            "{output}"
        );
        assert!(!output.contains("qrcode.png"), "{output}");
        assert!(!output.to_ascii_lowercase().contains("webui_token="));
        let credentials = fs::read_to_string(root.join("qq-bot/credentials.env")).unwrap();
        assert!(credentials.contains("WEBUI_TOKEN="));
        assert!(!output.contains(credentials.trim()));
        let _ = fs::remove_dir_all(root);
    }
}
