use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::agent::AgentEvent;
use crate::config::Config;
use crate::tools::{ToolCtx, ToolProgress};

pub const CATEGORY: &str = "plugins";
const PROTOCOL: &str = "yjlcoder.plugin/v1";
const MAX_PLUGIN_BYTES: u64 = 2 * 1024 * 1024;
const MAX_MODEL_STRING: usize = 1_200;
const MAX_MODEL_ARRAY: usize = 40;
const DEFAULT_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Clone, Deserialize)]
struct PluginManifest {
    name: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    version: String,
    description: String,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
    #[serde(default)]
    actions: Vec<PluginAction>,
}

#[derive(Debug, Clone, Deserialize)]
struct PluginAction {
    name: String,
    description: String,
    #[serde(default = "empty_object")]
    parameters: Value,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    requires_confirmation: bool,
}

#[derive(Debug, Clone)]
struct Plugin {
    path: PathBuf,
    manifest: PluginManifest,
}

#[derive(Debug)]
struct TaskState {
    id: String,
    plugin: String,
    action: String,
    status: String,
    summary: String,
    last_error: Option<String>,
    attempts: u64,
    started_at_ms: u64,
    updated_at_ms: u64,
    version: u64,
    result: Option<Value>,
    log_path: PathBuf,
    cancel: Arc<AtomicBool>,
}

static TASKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<TaskState>>>>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

fn empty_object() -> Value {
    json!({})
}

fn tasks() -> &'static Mutex<HashMap<String, Arc<Mutex<TaskState>>>> {
    TASKS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn list_index_line(cfg: &Config) -> String {
    let (plugins, errors) = discover(cfg);
    if plugins.is_empty() {
        if errors.is_empty() {
            return "[plugins] 自定义插件（0）: 将单文件 Python 插件放入 ~/.yjlcoder/plugins/python/\n".into();
        }
        return format!("[plugins] 自定义插件（0，可疑文件 {}）\n", errors.len());
    }
    let names = plugins
        .iter()
        .map(|plugin| plugin.manifest.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("[plugins] 自定义插件（{}）: {names}\n", plugins.len())
}

pub fn list_detail(cfg: &Config) -> String {
    let (plugins, errors) = discover(cfg);
    let mut out = format!(
        "[plugins] 自定义插件（{}；详细参数用 execute_command 调用插件的 help action）：\n",
        plugins.len()
    );
    for plugin in plugins {
        let actions = plugin
            .manifest
            .actions
            .iter()
            .map(|action| action.name.as_str())
            .chain([
                "help",
                "task_status",
                "task_wait",
                "task_cancel",
                "task_list",
            ])
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "- {} — {}\n  actions: {}\n  help: execute_command {{\"op\":\"{}\",\"args\":{{\"action\":\"help\"}}}}\n",
            plugin.manifest.name,
            one_line(&plugin.manifest.description, 180),
            actions,
            plugin.manifest.name
        ));
    }
    for error in errors.into_iter().take(8) {
        out.push_str(&format!("- [未加载] {}\n", one_line(&error, 240)));
    }
    out
}

pub fn execute_if_present(
    name: &str,
    args: &Value,
    ctx: &mut ToolCtx<'_>,
) -> Option<Result<String, String>> {
    match find_plugin(ctx.cfg, name) {
        Ok(Some(plugin)) => Some(execute_plugin(&plugin, args, ctx)),
        Ok(None) => None,
        Err(error) => Some(Err(error)),
    }
}

fn execute_plugin(plugin: &Plugin, args: &Value, ctx: &mut ToolCtx<'_>) -> Result<String, String> {
    let action_name = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("help")
        .trim();
    match action_name {
        "" | "help" => return Ok(help_text(plugin, args.get("for").and_then(Value::as_str))),
        "task_status" => return task_status(plugin, args, false),
        "task_wait" => return task_status(plugin, args, true),
        "task_cancel" => return task_cancel(plugin, args),
        "task_list" => return task_list(plugin),
        _ => {}
    }
    let action = plugin
        .manifest
        .actions
        .iter()
        .find(|action| action.name == action_name)
        .ok_or_else(|| {
            format!(
                "插件 {} 没有 action={action_name}。先调用 action=help 查看可用参数。",
                plugin.manifest.name
            )
        })?;
    if action.background {
        if action.requires_confirmation && !confirm_background(plugin, action, args, ctx)? {
            return Ok(json!({
                "ok": true,
                "status": "cancelled",
                "summary": "用户取消了后台任务，未执行任何预约操作"
            })
            .to_string());
        }
        start_background(plugin.clone(), action.clone(), args.clone(), ctx)
    } else {
        run_foreground(plugin, action, args, ctx)
    }
}

fn confirm_background(
    plugin: &Plugin,
    action: &PluginAction,
    args: &Value,
    ctx: &mut ToolCtx<'_>,
) -> Result<bool, String> {
    let summary = format!(
        "确认让「{}」启动 {} 后台任务吗？参数：{}",
        display_name(&plugin.manifest),
        action.description,
        one_line(&args.to_string(), 420)
    );
    let questions = json!({
        "questions": [{
            "header": "确认预约",
            "question": summary,
            "options": [
                {"label":"确认启动","description":"按当前参数开始后台等待和预约"},
                {"label":"取消","description":"不启动任务，也不提交预约"}
            ],
            "multiSelect": false
        }]
    });
    let answers = crate::tools::ask_user_answers(&questions, ctx)?;
    Ok(answers
        .get("answers")
        .and_then(Value::as_object)
        .is_some_and(|entries| {
            entries.values().any(|value| {
                value.as_array().is_some_and(|labels| {
                    labels.iter().any(|label| label.as_str() == Some("确认启动"))
                })
            })
        }))
}

fn help_text(plugin: &Plugin, only: Option<&str>) -> String {
    let manifest = &plugin.manifest;
    let version = if manifest.version.trim().is_empty() {
        String::new()
    } else {
        format!(", v{}", manifest.version)
    };
    let mut out = format!(
        "{} ({}{version}) — {}\n调用: execute_command {{\"op\":\"{}\",\"args\":{{\"action\":\"<action>\"}}}}\n",
        display_name(manifest),
        manifest.name,
        manifest.description,
        manifest.name
    );
    for action in &manifest.actions {
        if only.is_some_and(|name| name != action.name) {
            continue;
        }
        out.push_str(&format!(
            "- {} — {}{}{}\n  args: {}\n",
            action.name,
            action.description,
            if action.background {
                "；后台执行"
            } else {
                ""
            },
            if action.requires_confirmation {
                "；执行前询问用户确认"
            } else {
                ""
            },
            compact_json(&action.parameters)
        ));
    }
    out.push_str(
        "- task_status — 查看后台任务摘要；args: {\"action\":\"task_status\",\"task_id\":\"...\",\"wait_seconds\":0}\n\
         - task_wait — 等待状态变化或完成；args: {\"action\":\"task_wait\",\"task_id\":\"...\",\"wait_seconds\":30}\n\
         - task_cancel — 取消任务；args: {\"action\":\"task_cancel\",\"task_id\":\"...\"}\n\
         - task_list — 列出当前进程内任务；args: {\"action\":\"task_list\"}\n",
    );
    out
}

fn start_background(
    plugin: Plugin,
    action: PluginAction,
    args: Value,
    ctx: &ToolCtx<'_>,
) -> Result<String, String> {
    let id = format!(
        "{}-{}-{:04}",
        plugin.manifest.name,
        now_ms(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed) % 10_000
    );
    let log_path = task_log_path(ctx.cfg, &id)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(TaskState {
        id: id.clone(),
        plugin: plugin.manifest.name.clone(),
        action: action.name.clone(),
        status: "starting".into(),
        summary: "后台任务正在启动".into(),
        last_error: None,
        attempts: 0,
        started_at_ms: now_ms(),
        updated_at_ms: now_ms(),
        version: 1,
        result: None,
        log_path: log_path.clone(),
        cancel: cancel.clone(),
    }));
    tasks()
        .lock()
        .map_err(|_| "插件任务表已损坏".to_string())?
        .insert(id.clone(), state.clone());
    let cfg = ctx.cfg.clone();
    let event_tx = ctx.event_tx.cloned();
    std::thread::spawn(move || run_background_worker(plugin, action, args, cfg, event_tx, state));
    Ok(json!({
        "ok": true,
        "status": "running",
        "task_id": id,
        "summary": "后台任务已启动；模型无需持续读取日志，可按建议间隔查询状态",
        "poll_after_secs": 15,
        "full_log": log_path
    })
    .to_string())
}

fn run_background_worker(
    plugin: Plugin,
    action: PluginAction,
    args: Value,
    cfg: Config,
    event_tx: Option<Sender<AgentEvent>>,
    state: Arc<Mutex<TaskState>>,
) {
    let task_id = state
        .lock()
        .ok()
        .map(|state| state.id.clone())
        .unwrap_or_default();
    let log_path = state
        .lock()
        .ok()
        .map(|state| state.log_path.clone())
        .unwrap_or_default();
    update_task(
        &state,
        "running",
        "插件已启动，正在等待业务结果",
        None,
        None,
        0,
    );
    let mut command = match plugin_command(&plugin, &cfg, Some(&task_id)) {
        Ok(command) => command,
        Err(error) => {
            finish_task_error(&state, "PLUGIN_START_FAILED", &error);
            return;
        }
    };
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            finish_task_error(&state, "PLUGIN_START_FAILED", &error.to_string());
            return;
        }
    };
    let mut stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            finish_task_error(&state, "PLUGIN_PIPE_FAILED", "无法连接插件标准输入");
            let _ = child.kill();
            return;
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            finish_task_error(&state, "PLUGIN_PIPE_FAILED", "无法连接插件标准输出");
            let _ = child.kill();
            return;
        }
    };
    let stderr = child.stderr.take();
    let request = invocation_request(&plugin, &action, &args, Some(&task_id));
    if writeln!(stdin, "{}", request).is_err() || stdin.flush().is_err() {
        finish_task_error(&state, "PLUGIN_PIPE_FAILED", "无法向插件发送参数");
        let _ = child.kill();
        return;
    }
    drop(stdin);
    let (line_tx, line_rx) = channel();
    spawn_stdout_reader(stdout, line_tx);
    if let Some(stderr) = stderr {
        spawn_stderr_logger(stderr, log_path.clone());
    }
    let mut final_result = None;
    let mut legacy = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(action_timeout(&plugin.manifest, &action));
    loop {
        let cancelled = state
            .lock()
            .map(|state| state.cancel.load(Ordering::Relaxed))
            .unwrap_or(true);
        if cancelled {
            terminate_child(&mut child);
            let result = plugin_error("CANCELLED", "用户取消了后台任务", false);
            finish_task(&state, "cancelled", "后台任务已取消", Some(result));
            return;
        }
        if Instant::now() >= deadline {
            terminate_child(&mut child);
            let result = plugin_error("PLUGIN_TIMEOUT", "后台插件超过最长运行时间", true);
            finish_task(&state, "failed", "后台插件运行超时", Some(result));
            return;
        }
        match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                append_log(&log_path, "stdout", &line);
                match serde_json::from_str::<Value>(&line) {
                    Ok(event) => match event.get("type").and_then(Value::as_str) {
                        Some("progress") => {
                            record_progress(&state, &event);
                            if let Some(tx) = event_tx.as_ref() {
                                let view = task_view_value(&state);
                                let summary = view
                                    .get("summary")
                                    .and_then(Value::as_str)
                                    .unwrap_or("后台任务运行中");
                                let attempts =
                                    view.get("attempts").and_then(Value::as_u64).unwrap_or(0);
                                let _ = tx.send(AgentEvent::ToolProgress(ToolProgress {
                                    output: format!("[{task_id}] {summary}"),
                                    elapsed_secs: elapsed_secs(&state),
                                    total_lines: attempts.min(usize::MAX as u64) as usize,
                                    total_bytes: 0,
                                }));
                            }
                        }
                        Some("result") | Some("error") => final_result = Some(event),
                        Some("request_user") => {
                            final_result = Some(plugin_error(
                                "BACKGROUND_INTERACTION_NOT_ALLOWED",
                                "后台任务不能弹出配置问题；请先运行插件 setup",
                                false,
                            ));
                            terminate_child(&mut child);
                        }
                        _ => {}
                    },
                    Err(_) => {
                        if !line.trim().is_empty() {
                            legacy.push(line);
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {}
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let result =
                    final_result.unwrap_or_else(|| legacy_result(status.success(), &legacy));
                let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
                let status_name = if ok {
                    result
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("completed")
                } else {
                    "failed"
                }
                .to_string();
                let summary = result_summary(&result);
                finish_task(&state, &status_name, &summary, Some(result));
                return;
            }
            Ok(None) => {}
            Err(error) => {
                finish_task_error(&state, "PLUGIN_WAIT_FAILED", &error.to_string());
                terminate_child(&mut child);
                return;
            }
        }
    }
}

fn run_foreground(
    plugin: &Plugin,
    action: &PluginAction,
    args: &Value,
    ctx: &mut ToolCtx<'_>,
) -> Result<String, String> {
    let request_id = format!(
        "req-{}-{}",
        now_ms(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    );
    let log_path = invocation_log_path(ctx.cfg, &plugin.manifest.name, &request_id)?;
    let mut command = plugin_command(plugin, ctx.cfg, None)?;
    let mut child = command
        .spawn()
        .map_err(|error| format!("启动 Python 插件失败: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("无法连接插件标准输入")?;
    let stdout = child.stdout.take().ok_or("无法连接插件标准输出")?;
    let stderr = child.stderr.take();
    let request = invocation_request(plugin, action, args, None);
    writeln!(stdin, "{request}").map_err(|error| format!("发送插件参数失败: {error}"))?;
    stdin
        .flush()
        .map_err(|error| format!("刷新插件参数失败: {error}"))?;
    let (line_tx, line_rx) = channel();
    spawn_stdout_reader(stdout, line_tx);
    if let Some(stderr) = stderr {
        spawn_stderr_logger(stderr, log_path.clone());
    }
    let started = Instant::now();
    let deadline = started + Duration::from_secs(action_timeout(&plugin.manifest, action));
    let mut final_result = None;
    let mut legacy = Vec::new();
    loop {
        if ctx.cancel.load(Ordering::Relaxed) {
            terminate_child(&mut child);
            return Ok(compact_result(plugin_error(
                "CANCELLED",
                "用户按 Esc 取消了插件调用",
                false,
            )));
        }
        if Instant::now() >= deadline {
            terminate_child(&mut child);
            return Ok(compact_result(plugin_error(
                "PLUGIN_TIMEOUT",
                "插件超过最大等待时间并已被终止",
                true,
            )));
        }
        match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                append_log(&log_path, "stdout", &line);
                match serde_json::from_str::<Value>(&line) {
                    Ok(event) => match event.get("type").and_then(Value::as_str) {
                        Some("progress") => send_foreground_progress(ctx, started, &event),
                        Some("request_user") => {
                            let questions =
                                event.get("questions").cloned().unwrap_or_else(|| json!([]));
                            let answers = crate::tools::ask_user_answers(
                                &json!({"questions": questions}),
                                ctx,
                            )?;
                            let response = json!({
                                "protocol": PROTOCOL,
                                "type": "user_response",
                                "request_id": event.get("request_id"),
                                "outcome": answers
                            });
                            writeln!(stdin, "{response}")
                                .and_then(|_| stdin.flush())
                                .map_err(|error| format!("向插件返回用户配置失败: {error}"))?;
                        }
                        Some("result") | Some("error") => final_result = Some(event),
                        _ => {}
                    },
                    Err(_) => {
                        if !line.trim().is_empty() {
                            legacy.push(line);
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {}
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let result =
                    final_result.unwrap_or_else(|| legacy_result(status.success(), &legacy));
                return Ok(compact_result(result));
            }
            Ok(None) => {}
            Err(error) => {
                terminate_child(&mut child);
                return Ok(compact_result(plugin_error(
                    "PLUGIN_WAIT_FAILED",
                    &error.to_string(),
                    true,
                )));
            }
        }
    }
}

fn send_foreground_progress(ctx: &ToolCtx<'_>, started: Instant, event: &Value) {
    let Some(tx) = ctx.event_tx else {
        return;
    };
    let summary = event
        .get("message")
        .or_else(|| event.get("summary"))
        .and_then(Value::as_str)
        .unwrap_or("插件运行中");
    let attempts = event.get("attempts").and_then(Value::as_u64).unwrap_or(0);
    let _ = tx.send(AgentEvent::ToolProgress(ToolProgress {
        output: one_line(summary, 500),
        elapsed_secs: started.elapsed().as_secs(),
        total_lines: attempts.min(usize::MAX as u64) as usize,
        total_bytes: 0,
    }));
}

fn plugin_command(plugin: &Plugin, cfg: &Config, task_id: Option<&str>) -> Result<Command, String> {
    let (program, prefix) = python_interpreter()?;
    let data_dir = cfg
        .data_dir()
        .join("plugin-data")
        .join(&plugin.manifest.name);
    let log_dir = cfg
        .data_dir()
        .join("plugin-logs")
        .join(&plugin.manifest.name);
    fs::create_dir_all(&data_dir).map_err(|error| format!("创建插件数据目录失败: {error}"))?;
    fs::create_dir_all(&log_dir).map_err(|error| format!("创建插件日志目录失败: {error}"))?;
    let mut command = Command::new(program);
    command
        .args(prefix)
        .arg(&plugin.path)
        .arg("--yjlcoder-plugin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("PYTHONUNBUFFERED", "1")
        .env("YJLCODER_PLUGIN_PROTOCOL", PROTOCOL)
        .env("YJLCODER_PLUGIN_DATA_DIR", data_dir)
        .env("YJLCODER_PLUGIN_LOG_DIR", log_dir);
    if let Some(task_id) = task_id {
        command.env("YJLCODER_PLUGIN_TASK_ID", task_id);
    }
    configure_process_group(&mut command);
    Ok(command)
}

fn python_interpreter() -> Result<(String, Vec<String>), String> {
    if let Ok(value) = std::env::var("YJLCODER_PYTHON") {
        let value = value.trim();
        if !value.is_empty() {
            return Ok((value.to_string(), Vec::new()));
        }
    }
    #[cfg(windows)]
    let candidates: &[(&str, &[&str])] = &[("py", &["-3"]), ("python", &[]), ("python3", &[])];
    #[cfg(not(windows))]
    let candidates: &[(&str, &[&str])] = &[("python3", &[]), ("python", &[])];
    for (program, args) in candidates {
        if Command::new(program)
            .args(*args)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            return Ok((
                (*program).to_string(),
                args.iter().map(|arg| (*arg).to_string()).collect(),
            ));
        }
    }
    Err("未找到 Python 3。请安装 Python 3，或设置 YJLCODER_PYTHON 为解释器路径。".into())
}

fn invocation_request(
    plugin: &Plugin,
    action: &PluginAction,
    args: &Value,
    task_id: Option<&str>,
) -> Value {
    json!({
        "protocol": PROTOCOL,
        "type": "invoke",
        "request_id": format!("req-{}-{}", now_ms(), NEXT_ID.fetch_add(1, Ordering::Relaxed)),
        "plugin": plugin.manifest.name,
        "action": action.name,
        "args": args,
        "task_id": task_id
    })
}

fn task_status(plugin: &Plugin, args: &Value, force_wait: bool) -> Result<String, String> {
    let id = required_task_id(args)?;
    let state = tasks()
        .lock()
        .map_err(|_| "插件任务表已损坏".to_string())?
        .get(id)
        .cloned()
        .ok_or_else(|| format!("没有找到任务 {id}；可调用 task_list 查看当前任务"))?;
    if state.lock().map_err(|_| "任务状态已损坏")?.plugin != plugin.manifest.name {
        return Err(format!("任务 {id} 不属于插件 {}", plugin.manifest.name));
    }
    let wait = args
        .get("wait_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(if force_wait { 30 } else { 0 })
        .min(120);
    let initial_version = state.lock().map_err(|_| "任务状态已损坏")?.version;
    let deadline = Instant::now() + Duration::from_secs(wait);
    while Instant::now() < deadline {
        {
            let locked = state.lock().map_err(|_| "任务状态已损坏")?;
            if locked.version != initial_version || terminal_status(&locked.status) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(task_view_value(&state).to_string())
}

fn task_cancel(plugin: &Plugin, args: &Value) -> Result<String, String> {
    let id = required_task_id(args)?;
    let state = tasks()
        .lock()
        .map_err(|_| "插件任务表已损坏".to_string())?
        .get(id)
        .cloned()
        .ok_or_else(|| format!("没有找到任务 {id}"))?;
    let locked = state.lock().map_err(|_| "任务状态已损坏")?;
    if locked.plugin != plugin.manifest.name {
        return Err(format!("任务 {id} 不属于插件 {}", plugin.manifest.name));
    }
    if terminal_status(&locked.status) {
        return Ok(task_view_from(&locked).to_string());
    }
    locked.cancel.store(true, Ordering::Relaxed);
    drop(locked);
    Ok(json!({
        "ok": true,
        "status": "cancelling",
        "task_id": id,
        "summary": "取消信号已发送；稍后调用 task_status 确认终态"
    })
    .to_string())
}

fn task_list(plugin: &Plugin) -> Result<String, String> {
    let map = tasks().lock().map_err(|_| "插件任务表已损坏".to_string())?;
    let mut values = map
        .values()
        .filter_map(|state| state.lock().ok())
        .filter(|state| state.plugin == plugin.manifest.name)
        .map(|state| task_view_from(&state))
        .collect::<Vec<_>>();
    values.sort_by_key(|value| {
        std::cmp::Reverse(
            value
                .get("started_at_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        )
    });
    Ok(json!({
        "ok": true,
        "status": "completed",
        "summary": format!("当前进程内共有 {} 个任务", values.len()),
        "tasks": values.into_iter().take(30).collect::<Vec<_>>()
    })
    .to_string())
}

pub fn shutdown_all() {
    let states = tasks()
        .lock()
        .map(|tasks| tasks.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    for state in &states {
        if let Ok(state) = state.lock() {
            if !terminal_status(&state.status) {
                state.cancel.store(true, Ordering::Relaxed);
            }
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if states.iter().all(|state| {
            state
                .lock()
                .map(|state| terminal_status(&state.status))
                .unwrap_or(true)
        }) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn required_task_id(args: &Value) -> Result<&str, String> {
    args.get("task_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| "缺少 task_id；先调用 task_list 查看任务".into())
}

fn task_view_value(state: &Arc<Mutex<TaskState>>) -> Value {
    state
        .lock()
        .map(|state| task_view_from(&state))
        .unwrap_or_else(|_| plugin_error("TASK_STATE_POISONED", "任务状态损坏", false))
}

fn task_view_from(state: &TaskState) -> Value {
    let mut value = json!({
        "ok": state.status != "failed",
        "status": state.status,
        "task_id": state.id,
        "plugin": state.plugin,
        "action": state.action,
        "summary": one_line(&state.summary, 700),
        "attempts": state.attempts,
        "elapsed_secs": now_ms().saturating_sub(state.started_at_ms) / 1000,
        "started_at_ms": state.started_at_ms,
        "updated_at_ms": state.updated_at_ms,
        "last_error": state.last_error.as_deref().map(|error| one_line(error, 700)),
        "full_log": state.log_path,
    });
    if let Some(result) = state.result.as_ref() {
        value["result"] = compact_value(result, 0);
    }
    value
}

fn update_task(
    state: &Arc<Mutex<TaskState>>,
    status: &str,
    summary: &str,
    last_error: Option<String>,
    result: Option<Value>,
    attempts: u64,
) {
    if let Ok(mut state) = state.lock() {
        state.status = status.into();
        state.summary = one_line(summary, 2_000);
        if last_error.is_some() {
            state.last_error = last_error.map(|error| one_line(&error, 2_000));
        }
        if result.is_some() {
            state.result = result;
        }
        state.attempts = state.attempts.max(attempts);
        state.updated_at_ms = now_ms();
        state.version = state.version.saturating_add(1);
        persist_task(&state);
    }
}

fn record_progress(state: &Arc<Mutex<TaskState>>, event: &Value) {
    let summary = event
        .get("message")
        .or_else(|| event.get("summary"))
        .and_then(Value::as_str)
        .unwrap_or("后台任务运行中");
    let last_error = event
        .get("last_error")
        .and_then(Value::as_str)
        .map(str::to_string);
    let attempts = event.get("attempts").and_then(Value::as_u64).unwrap_or(0);
    update_task(state, "running", summary, last_error, None, attempts);
}

fn finish_task(state: &Arc<Mutex<TaskState>>, status: &str, summary: &str, result: Option<Value>) {
    let last_error = result
        .as_ref()
        .and_then(|result| result.get("error"))
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string);
    update_task(state, status, summary, last_error, result, 0);
}

fn finish_task_error(state: &Arc<Mutex<TaskState>>, code: &str, message: &str) {
    let result = plugin_error(code, message, true);
    finish_task(state, "failed", message, Some(result));
}

fn persist_task(state: &TaskState) {
    let Some(dir) = state
        .log_path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
    else {
        return;
    };
    let dir = dir.join("plugin-tasks");
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join(format!("{}.json", state.id));
    let tmp = dir.join(format!(".{}-{}.tmp", state.id, std::process::id()));
    if let Ok(text) = serde_json::to_string_pretty(&task_view_from(state)) {
        if fs::write(&tmp, text).is_ok() {
            let _ = fs::rename(tmp, path);
        }
    }
}

fn elapsed_secs(state: &Arc<Mutex<TaskState>>) -> u64 {
    state
        .lock()
        .map(|state| now_ms().saturating_sub(state.started_at_ms) / 1000)
        .unwrap_or(0)
}

fn terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

fn result_summary(result: &Value) -> String {
    result
        .get("summary")
        .and_then(Value::as_str)
        .or_else(|| {
            result
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
        })
        .unwrap_or("插件已结束")
        .to_string()
}

fn legacy_result(success: bool, lines: &[String]) -> Value {
    if lines.is_empty() {
        return plugin_error(
            "EMPTY_RESULT",
            "插件进程已经退出，但没有返回任何结果；Rust 已结束等待，Agent 不会卡死",
            false,
        );
    }
    let summary = one_line(
        lines.last().map(String::as_str).unwrap_or(""),
        MAX_MODEL_STRING,
    );
    if success {
        json!({
            "type": "result",
            "ok": true,
            "status": "completed",
            "summary": summary,
            "compatibility": "legacy_stdout"
        })
    } else {
        plugin_error("PLUGIN_CRASHED", &summary, true)
    }
}

fn plugin_error(code: &str, message: &str, retryable: bool) -> Value {
    json!({
        "type": "result",
        "ok": false,
        "status": "failed",
        "summary": one_line(message, MAX_MODEL_STRING),
        "error": {
            "code": code,
            "message": one_line(message, MAX_MODEL_STRING),
            "retryable": retryable
        }
    })
}

fn compact_result(mut result: Value) -> String {
    if let Some(object) = result.as_object_mut() {
        object.remove("type");
        object.remove("protocol");
        object.remove("trace");
        object.remove("logs");
        object.remove("stdout");
        object.remove("stderr");
    }
    compact_value(&result, 0).to_string()
}

fn compact_value(value: &Value, depth: usize) -> Value {
    if depth >= 8 {
        return json!("[内容层级过深，已省略]");
    }
    match value {
        Value::String(text) => json!(one_line(text, MAX_MODEL_STRING)),
        Value::Array(items) => {
            let mut out = items
                .iter()
                .take(MAX_MODEL_ARRAY)
                .map(|item| compact_value(item, depth + 1))
                .collect::<Vec<_>>();
            if items.len() > MAX_MODEL_ARRAY {
                out.push(json!({"omitted_items": items.len() - MAX_MODEL_ARRAY}));
            }
            Value::Array(out)
        }
        Value::Object(object) => {
            let mut out = serde_json::Map::new();
            for (key, value) in object {
                if matches!(
                    key.as_str(),
                    "trace" | "logs" | "stdout" | "stderr" | "debug"
                ) {
                    continue;
                }
                out.insert(key.clone(), compact_value(value, depth + 1));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(&compact_value(value, 0)).unwrap_or_else(|_| "{}".into())
}

fn spawn_stdout_reader(stdout: impl std::io::Read + Send + 'static, tx: Sender<String>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
}

fn spawn_stderr_logger(stderr: impl std::io::Read + Send + 'static, log_path: PathBuf) {
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            append_log(&log_path, "stderr", &line);
        }
    });
}

fn append_log(path: &Path, stream: &str, line: &str) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} [{}] {}", now_ms(), stream, redact_line(line));
    }
}

fn redact_line(line: &str) -> String {
    let mut out = line.to_string();
    for marker in ["Bearer ", "\"token\":\"", "\"access_token\":\""] {
        let mut start_at = 0usize;
        while let Some(relative) = out[start_at..].find(marker) {
            let start = start_at + relative + marker.len();
            let end = out[start..]
                .find(|ch: char| ch.is_whitespace() || matches!(ch, '\"' | ',' | '}' | ']'))
                .map(|offset| start + offset)
                .unwrap_or(out.len());
            if end > start {
                out.replace_range(start..end, "<redacted>");
                start_at = start + "<redacted>".len();
            } else {
                break;
            }
        }
    }
    out
}

fn task_log_path(cfg: &Config, id: &str) -> Result<PathBuf, String> {
    let dir = cfg.data_dir().join("plugin-logs").join("tasks");
    fs::create_dir_all(&dir).map_err(|error| format!("创建插件任务日志目录失败: {error}"))?;
    Ok(dir.join(format!("{id}.log")))
}

fn invocation_log_path(cfg: &Config, plugin: &str, id: &str) -> Result<PathBuf, String> {
    let dir = cfg.data_dir().join("plugin-logs").join(plugin);
    fs::create_dir_all(&dir).map_err(|error| format!("创建插件日志目录失败: {error}"))?;
    Ok(dir.join(format!("{id}.log")))
}

fn action_timeout(manifest: &PluginManifest, action: &PluginAction) -> u64 {
    if action.background {
        manifest.timeout_secs.clamp(60, 30 * 24 * 60 * 60)
    } else {
        manifest.timeout_secs.clamp(1, 3_600)
    }
}

fn discover(cfg: &Config) -> (Vec<Plugin>, Vec<String>) {
    let mut plugins = Vec::new();
    let mut errors = Vec::new();
    let mut names = HashSet::new();
    for dir in plugin_dirs(cfg) {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "py"))
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            match read_plugin(&path) {
                Ok(plugin) => {
                    if names.insert(plugin.manifest.name.clone()) {
                        plugins.push(plugin);
                    }
                }
                Err(error) => errors.push(format!("{}: {error}", path.display())),
            }
        }
    }
    plugins.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    (plugins, errors)
}

fn find_plugin(cfg: &Config, name: &str) -> Result<Option<Plugin>, String> {
    if !valid_name(name) {
        return Ok(None);
    }
    let (plugins, errors) = discover(cfg);
    if let Some(plugin) = plugins
        .into_iter()
        .find(|plugin| plugin.manifest.name == name)
    {
        return Ok(Some(plugin));
    }
    if let Some(error) = errors.into_iter().find(|error| error.contains(name)) {
        return Err(error);
    }
    Ok(None)
}

fn plugin_dirs(cfg: &Config) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(current) = std::env::current_dir() {
        let project = current.join("plugins").join("python");
        dirs.push(project);
    }
    let user = cfg.data_dir().join("plugins").join("python");
    if !dirs.contains(&user) {
        dirs.push(user);
    }
    dirs
}

fn read_plugin(path: &Path) -> Result<Plugin, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("插件必须是普通 .py 文件，不能是符号链接".into());
    }
    if metadata.len() > MAX_PLUGIN_BYTES {
        return Err(format!(
            "插件超过 {} MiB 上限",
            MAX_PLUGIN_BYTES / 1024 / 1024
        ));
    }
    let text = fs::read_to_string(path).map_err(|error| format!("读取失败: {error}"))?;
    let manifest = parse_manifest(&text)?;
    validate_manifest(&manifest)?;
    Ok(Plugin {
        path: path.to_path_buf(),
        manifest,
    })
}

fn parse_manifest(text: &str) -> Result<PluginManifest, String> {
    let mut inside = false;
    let mut block = String::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "# /// yjlcoder-plugin" {
            inside = true;
            continue;
        }
        if inside && trimmed == "# ///" {
            break;
        }
        if inside {
            let content = trimmed
                .strip_prefix('#')
                .map(str::trim_start)
                .ok_or("插件元数据块内每一行都必须以 # 开头")?;
            block.push_str(content);
            block.push('\n');
        }
    }
    if !inside || block.trim().is_empty() {
        return Err("缺少 '# /// yjlcoder-plugin' JSON 元数据块".into());
    }
    serde_json::from_str(&block).map_err(|error| format!("插件元数据 JSON 无效: {error}"))
}

fn validate_manifest(manifest: &PluginManifest) -> Result<(), String> {
    if !valid_name(&manifest.name) {
        return Err("name 必须匹配 [a-z][a-z0-9_]{2,47}".into());
    }
    if manifest.description.trim().chars().count() < 12 {
        return Err("description 太短，必须说明何时调用、做什么、返回什么".into());
    }
    if manifest.actions.is_empty() {
        return Err("actions 不能为空".into());
    }
    let mut names = HashSet::new();
    for action in &manifest.actions {
        if !valid_name(&action.name) || !names.insert(action.name.clone()) {
            return Err(format!("action 名称无效或重复: {}", action.name));
        }
        if action.description.trim().is_empty() {
            return Err(format!("action {} 缺少 description", action.name));
        }
        if !action.parameters.is_object() {
            return Err(format!(
                "action {} 的 parameters 必须是 JSON object",
                action.name
            ));
        }
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (3..=48).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'_')
}

fn display_name(manifest: &PluginManifest) -> &str {
    if manifest.display_name.trim().is_empty() {
        &manifest.name
    } else {
        &manifest.display_name
    }
}

fn one_line(text: &str, max_chars: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= max_chars {
        return joined;
    }
    let mut out = joined
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGTERM);
    }
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    #[cfg(not(any(unix, windows)))]
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    #[cfg(not(any(unix, windows)))]
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"# /// yjlcoder-plugin
# {
#   "name":"room_helper",
#   "display_name":"教室助手",
#   "description":"查询教室并返回结构化结果，供本地 Agent 调用",
#   "actions":[{"name":"find_free","description":"查找空闲教室","parameters":{"type":"object"}}]
# }
# ///
print('not executed while discovering')
"#;

    const BACKGROUND_SAMPLE: &str = r#"# /// yjlcoder-plugin
# {
#   "name":"background_probe",
#   "description":"后台发送精简进度并返回确定结果，用于验证任务生命周期",
#   "timeout_secs":5,
#   "actions":[{"name":"run_task","description":"运行一个很短的后台探针","parameters":{"type":"object"},"background":true}]
# }
# ///
import json, sys, time
json.loads(sys.stdin.readline())
print(json.dumps({"type":"progress","message":"probe running","attempts":1}), flush=True)
time.sleep(0.05)
print(json.dumps({"type":"result","ok":True,"status":"completed","summary":"probe done","data":{"value":42}}), flush=True)
"#;

    const INTERACTIVE_SAMPLE: &str = r#"# /// yjlcoder-plugin
# {
#   "name":"interactive_probe",
#   "description":"通过 Ask User 配置桥接收用户输入并返回精简确认结果",
#   "actions":[{"name":"configure","description":"请求一个配置值","parameters":{"type":"object"}}]
# }
# ///
import json, sys
json.loads(sys.stdin.readline())
question = "请输入测试配置值"
print(json.dumps({"type":"request_user","request_id":"ask-1","questions":[{"header":"测试配置","question":question,"options":[]}]}), flush=True)
answer = json.loads(sys.stdin.readline())["outcome"]["answers"][question][0]
print(json.dumps({"type":"result","ok":True,"status":"completed","summary":"configured","data":{"answer":answer}}), flush=True)
"#;

    #[test]
    fn manifest_is_read_without_running_python() {
        let manifest = parse_manifest(SAMPLE).unwrap();
        validate_manifest(&manifest).unwrap();
        assert_eq!(manifest.name, "room_helper");
        assert_eq!(manifest.actions[0].name, "find_free");
    }

    #[test]
    fn user_plugin_directory_is_discovered_without_restart() {
        let root = std::env::temp_dir().join(format!(
            "yjlcoder-plugin-discovery-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let dir = root.join("plugins/python");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("room_helper.py"), SAMPLE).unwrap();
        let mut cfg = Config::default();
        cfg.set_test_data_dir(root.clone());
        let index = list_index_line(&cfg);
        let detail = list_detail(&cfg);
        assert!(index.contains("room_helper"), "index: {index}");
        assert!(detail.contains("action\":\"help"), "detail: {detail}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn background_plugin_returns_task_id_and_reaches_terminal_state() {
        if python_interpreter().is_err() {
            return;
        }
        let root = std::env::temp_dir().join(format!(
            "yjlcoder-plugin-background-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let dir = root.join("plugins/python");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("background_probe.py"), BACKGROUND_SAMPLE).unwrap();
        let mut cfg = Config::default();
        cfg.set_test_data_dir(root.clone());
        let mut store = crate::session::SessionStore::new(cfg.sessions_dir());
        let llm = crate::llm::Llm::mock();
        let cancel = AtomicBool::new(false);
        let mut ctx = ToolCtx {
            cfg: &cfg,
            store: &mut store,
            llm: &llm,
            qq_tx: None,
            cancel: &cancel,
            mem_dir: None,
            ask: None,
            perm: None,
            event_tx: None,
        };
        let started =
            execute_if_present("background_probe", &json!({"action":"run_task"}), &mut ctx)
                .unwrap()
                .unwrap();
        let started: Value = serde_json::from_str(&started).unwrap();
        let task_id = started["task_id"].as_str().unwrap().to_string();
        let mut final_status = String::new();
        for _ in 0..50 {
            let status = execute_if_present(
                "background_probe",
                &json!({"action":"task_status","task_id":task_id}),
                &mut ctx,
            )
            .unwrap()
            .unwrap();
            let status: Value = serde_json::from_str(&status).unwrap();
            final_status = status["status"].as_str().unwrap_or("").to_string();
            if terminal_status(&final_status) {
                assert_eq!(status["result"]["data"]["value"], 42);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(final_status, "completed");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn python_plugin_can_round_trip_through_ask_user() {
        if python_interpreter().is_err() {
            return;
        }
        let root = std::env::temp_dir().join(format!(
            "yjlcoder-plugin-interactive-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let dir = root.join("plugins/python");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("interactive_probe.py"), INTERACTIVE_SAMPLE).unwrap();
        let mut cfg = Config::default();
        cfg.set_test_data_dir(root.clone());
        let mut store = crate::session::SessionStore::new(cfg.sessions_dir());
        let llm = crate::llm::Llm::mock();
        let cancel = AtomicBool::new(false);
        let ask_seq = AtomicU64::new(1);
        let (request_tx, request_rx) = std::sync::mpsc::channel();
        let (answer_tx, answer_rx) = std::sync::mpsc::channel();
        let responder = std::thread::spawn(move || {
            let request: crate::tools::AskRequest =
                request_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            answer_tx
                .send(crate::tools::AskAnswer {
                    id: request.id,
                    outcome: crate::tools::AskOutcome::Accepted {
                        answers: vec![(
                            request.questions[0].question.clone(),
                            vec!["configured-value".to_string()],
                        )],
                        annotations: Vec::new(),
                    },
                })
                .unwrap();
        });
        let mut ctx = ToolCtx {
            cfg: &cfg,
            store: &mut store,
            llm: &llm,
            qq_tx: None,
            cancel: &cancel,
            mem_dir: None,
            ask: Some(crate::tools::AskHandle {
                tx: &request_tx,
                rx: &answer_rx,
                cancel: &cancel,
                seq: &ask_seq,
            }),
            perm: None,
            event_tx: None,
        };
        let output = execute_if_present(
            "interactive_probe",
            &json!({"action":"configure"}),
            &mut ctx,
        )
        .unwrap()
        .unwrap();
        responder.join().unwrap();
        let output: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["data"]["answer"], "configured-value");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn empty_legacy_output_is_a_deterministic_error() {
        let result = legacy_result(true, &[]);
        assert_eq!(result["error"]["code"], "EMPTY_RESULT");
    }

    #[test]
    fn model_result_drops_verbose_logs() {
        let raw = json!({"ok":true,"summary":"ok","logs":["huge"],"data":[1,2]});
        let compact: Value = serde_json::from_str(&compact_result(raw)).unwrap();
        assert!(compact.get("logs").is_none());
        assert_eq!(compact["summary"], "ok");
    }
}
