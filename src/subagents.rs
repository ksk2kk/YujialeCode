use serde_json::Value;
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::agent::{Agent, AgentEvent};
use crate::config::Config;
use crate::llm::Llm;
use crate::session::SessionStore;
use crate::tools::ToolCtx;

const RESULT_LIMIT: usize = 8_000;
const WAIT_LIMIT_SECS: u64 = 10;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CHILD_DEPTH: Cell<usize> = const { Cell::new(0) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskState {
    fn label(self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Running => "running",
            TaskState::Completed => "completed",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
        }
    }

    fn terminal(self) -> bool {
        matches!(self, TaskState::Completed | TaskState::Failed | TaskState::Cancelled)
    }
}

#[derive(Clone)]
struct TaskRecord {
    id: String,
    parent_session: String,
    title: String,
    task: String,
    model: String,
    state: TaskState,
    progress: String,
    result: String,
    delivered_to_parent: bool,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct Runtime {
    tasks: BTreeMap<String, TaskRecord>,
    notifications: VecDeque<String>,
    turn_spawns: HashMap<String, usize>,
    active: usize,
}

fn runtime() -> &'static Mutex<Runtime> {
    static RUNTIME: OnceLock<Mutex<Runtime>> = OnceLock::new();
    RUNTIME.get_or_init(|| Mutex::new(Runtime::default()))
}

fn local_inference_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Serializes every parent and child turn that points at localhost. Even if a
/// user submits a new message in the small handoff window, llama.cpp still sees
/// only one inference request and the TUI remains responsive while it waits.
pub fn local_inference_guard(cfg: &Config) -> Option<MutexGuard<'static, ()>> {
    local_provider(&cfg.provider.base_url)
        .then(|| local_inference_lock().lock().unwrap_or_else(|error| error.into_inner()))
}

pub struct TurnGuard {
    session: Option<String>,
}

/// Resets the spawn allowance once per parent turn. Child turns deliberately
/// do not reset it, which closes the easiest nested-agent bypass.
pub fn begin_turn(session: &str) -> TurnGuard {
    if in_child() {
        return TurnGuard { session: None };
    }
    runtime().lock().unwrap().turn_spawns.insert(session.to_string(), 0);
    TurnGuard { session: Some(session.to_string()) }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            runtime().lock().unwrap().turn_spawns.remove(&session);
        }
    }
}

struct ChildGuard;

impl ChildGuard {
    fn enter() -> Self {
        CHILD_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        ChildGuard
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        CHILD_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

pub fn in_child() -> bool {
    CHILD_DEPTH.with(|depth| depth.get() > 0)
}

pub fn child_system_prompt() -> &'static str {
    "你是受限后台子 Agent。只完成分配给你的一个具体任务，优先读取和搜索证据，禁止修改文件、执行任意 shell、联系用户或创建其他 Agent。最后返回简洁结论、证据路径和未解决问题。不要重复轮询。"
}

fn local_provider(base_url: &str) -> bool {
    let lower = base_url.to_ascii_lowercase();
    lower.contains("127.0.0.1") || lower.contains("localhost") || lower.contains("[::1]")
}

fn trim_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str("\n…结果已截断；完整记录保存在 Agent 独立会话目录。");
    out
}

fn task_id() -> String {
    let seq = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("agent-{}-{seq:04}", std::process::id())
}

fn nested_dispatch_target(args: &Value) -> Option<String> {
    let object = args.as_object()?;
    for key in ["op", "tool", "tool_name", "function"] {
        if let Some(target) = object.get(key).and_then(Value::as_str) {
            return Some(target.to_string());
        }
    }
    for key in ["args", "arguments", "parameters", "params", "input", "payload", "request"] {
        if let Some(value) = object.get(key) {
            if let Some(target) = nested_dispatch_target(value) {
                return Some(target);
            }
        }
    }
    None
}

fn child_tool_allowed(name: &str) -> bool {
    matches!(
        name,
        "list_tools"
            | "readline"
            | "glob"
            | "grep"
            | "listdir"
            | "web_search"
            | "web_research"
            | "web_fetch"
            | "http_get"
            | "http_headers"
            | "dns_lookup"
            | "stats"
            | "list_skills"
            | "run_skill"
    )
}

/// Enforces the child allowlist before dynamic tools, plugins or shell aliases
/// get a chance to run. This is a security/resource boundary, not a prompt.
pub fn authorize_tool_call(op: &str, args: &Value) -> Result<(), String> {
    if !in_child() {
        return Ok(());
    }
    let target = if op == "execute_command" {
        nested_dispatch_target(args).or_else(|| {
            args.get("cmd")
                .and_then(Value::as_str)
                .map(crate::tool_compat::parse_args)
                .as_ref()
                .and_then(nested_dispatch_target)
        })
    } else {
        Some(op.to_string())
    };
    match target {
        Some(name) if child_tool_allowed(&name) => Ok(()),
        Some(name) => Err(format!(
            "后台 Agent 无权调用 {name}；只允许只读文件、搜索和网页抓取工具"
        )),
        None => Err("后台 Agent 禁止执行任意 shell；请调度 readline/grep/listdir 等只读工具".into()),
    }
}

pub fn execute(args: &Value, ctx: &mut ToolCtx) -> Result<String, String> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_else(|| if args.get("task").is_some() { "spawn" } else { "list" });
    match action.to_ascii_lowercase().as_str() {
        "spawn" | "start" | "create" | "run" => spawn(args, ctx),
        "list" | "status" => Ok(list_text(args.get("id").and_then(Value::as_str))),
        "wait" | "poll" | "result" => wait_result(args),
        "stop" | "cancel" => {
            let id = args.get("id").and_then(Value::as_str).ok_or("stop 需要 id")?;
            stop(id)
        }
        _ => Err("未知 action；使用 spawn/list/wait/stop".into()),
    }
}

fn spawn(args: &Value, ctx: &mut ToolCtx) -> Result<String, String> {
    if in_child() {
        return Err("禁止子 Agent 创建子 Agent".into());
    }
    if !ctx.cfg.agents.enabled {
        return Err("后台 Agent 已关闭；用户可在 /config 或 /agents on 中开启".into());
    }
    let task = args
        .get("task")
        .or_else(|| args.get("prompt"))
        .or_else(|| args.get("objective"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|task| !task.is_empty())
        .ok_or("缺少 task")?;
    if task.chars().count() < 12 {
        return Err("task 太短；请写清目标、范围和期望返回内容（至少 12 个字符）".into());
    }
    if task.chars().count() > 4_000 {
        return Err("task 超过 4000 字符；请缩小为一个独立子任务".into());
    }
    let session = ctx.store.current_id().to_string();
    let max_per_turn = ctx.cfg.agents.max_spawn_per_turn.clamp(1, 2);
    let max_concurrent = ctx.cfg.agents.max_concurrent.clamp(1, 2);
    let id = task_id();
    let title = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("后台任务")
        .chars()
        .take(48)
        .collect::<String>();
    let model = if ctx.cfg.agents.model.trim().is_empty() {
        ctx.llm.model_name()
    } else {
        ctx.cfg.agents.model.trim().to_string()
    };
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut rt = runtime().lock().unwrap();
        let spawned = rt.turn_spawns.get(&session).copied().unwrap_or(0);
        if spawned >= max_per_turn {
            return Err(format!(
                "本回合后台 Agent 配额已用完（最多 {max_per_turn} 个）；请先使用现有结果"
            ));
        }
        let nonterminal = rt.tasks.values().filter(|task| !task.state.terminal()).count();
        if nonterminal >= max_concurrent {
            return Err(format!(
                "已有后台 Agent 正在运行或排队（上限 {max_concurrent}）；先用 wait_agent/list_agents 查看"
            ));
        }
        rt.turn_spawns.insert(session.clone(), spawned + 1);
        rt.tasks.insert(
            id.clone(),
            TaskRecord {
                id: id.clone(),
                parent_session: session.clone(),
                title: title.clone(),
                task: task.to_string(),
                model: model.clone(),
                state: TaskState::Queued,
                progress: if local_provider(&ctx.cfg.provider.base_url) {
                    "等待主回合结束，避免与本地模型抢算力".into()
                } else {
                    "等待调度".into()
                },
                result: String::new(),
                delivered_to_parent: false,
                cancel: cancel.clone(),
            },
        );
    }
    let cfg = ctx.cfg.clone();
    let llm = ctx.llm.fork_with_model(&model);
    let task_owned = task.to_string();
    let id_for_thread = id.clone();
    if let Err(error) = std::thread::Builder::new()
        .name(format!("yjlcoder-{id}"))
        .spawn(move || run_worker(id_for_thread, task_owned, cfg, llm, cancel))
    {
        let mut rt = runtime().lock().unwrap();
        rt.tasks.remove(&id);
        if let Some(spawned) = rt.turn_spawns.get_mut(&session) {
            *spawned = spawned.saturating_sub(1);
        }
        return Err(format!("创建后台线程失败: {error}"));
    }
    Ok(format!(
        "已创建 {id}（{title}），模型 {model}。这是后台任务，不要在同一回合反复轮询；完成后界面会通知。"
    ))
}

fn run_worker(id: String, task: String, mut cfg: Config, llm: Llm, cancel: Arc<AtomicBool>) {
    let local = local_provider(&cfg.provider.base_url);
    while local && crate::agent::runtime_is_busy() && !cancel.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(150));
    }
    if cancel.load(Ordering::Relaxed) {
        finish_task(&id, TaskState::Cancelled, "任务在启动前被取消".into());
        return;
    }
    {
        let mut rt = runtime().lock().unwrap();
        if let Some(record) = rt.tasks.get_mut(&id) {
            record.state = TaskState::Running;
            record.progress = "正在独立上下文中执行".into();
        }
        rt.active = rt.active.saturating_add(1);
    }
    cfg.tool_times = cfg.agents.max_steps.clamp(1, 16);
    cfg.agents.enabled = false;
    let dir = cfg.data_dir().join("agents").join(&id);
    let mut store = SessionStore::new(dir);
    store.new_session("transcript");
    let _child = ChildGuard::enter();
    let mut agent = Agent::with_store(cfg, llm, store, None, false, false, cancel.clone());
    let result = agent.run_turn(&task, &mut |event| {
        let progress = match event {
            AgentEvent::ToolRun { op, .. } => Some(format!("正在调用 {op}")),
            AgentEvent::Notice(message) => Some(trim_chars(&message, 180)),
            AgentEvent::Error(message) => Some(format!("错误: {}", trim_chars(&message, 180))),
            _ => None,
        };
        if let Some(progress) = progress {
            if let Some(record) = runtime().lock().unwrap().tasks.get_mut(&id) {
                record.progress = progress;
            }
        }
    });
    match result {
        Ok(text) if cancel.load(Ordering::Relaxed) => {
            finish_task(&id, TaskState::Cancelled, trim_chars(&text, RESULT_LIMIT))
        }
        Ok(text) => finish_task(&id, TaskState::Completed, trim_chars(&text, RESULT_LIMIT)),
        Err(error) if cancel.load(Ordering::Relaxed) => {
            finish_task(&id, TaskState::Cancelled, trim_chars(&error, RESULT_LIMIT))
        }
        Err(error) => finish_task(&id, TaskState::Failed, trim_chars(&error, RESULT_LIMIT)),
    }
}

fn finish_task(id: &str, state: TaskState, result: String) {
    let mut rt = runtime().lock().unwrap();
    let mut notice = None;
    if let Some(record) = rt.tasks.get_mut(id) {
        record.state = state;
        record.progress = state.label().into();
        record.result = result;
        notice = Some(format!(
            "后台 Agent {}（{}）已{}；使用 /agents show {} 查看结果",
            record.id,
            record.title,
            if state == TaskState::Completed { "完成" } else { "停止" },
            record.id
        ));
    }
    rt.active = rt.active.saturating_sub(1);
    if let Some(notice) = notice {
        rt.notifications.push_back(notice);
    }
}

pub fn drain_notifications() -> Vec<String> {
    let mut rt = runtime().lock().unwrap();
    rt.notifications.drain(..).collect()
}

/// Completion results are injected once into the parent's next model turn,
/// mirroring mature background-agent systems without interrupting the turn
/// that originally created the child.
pub fn take_parent_results(parent_session: &str) -> Vec<String> {
    let mut rt = runtime().lock().unwrap();
    let mut out = Vec::new();
    for task in rt.tasks.values_mut() {
        if task.parent_session == parent_session
            && task.state.terminal()
            && !task.delivered_to_parent
        {
            task.delivered_to_parent = true;
            out.push(format!(
                "【后台 Agent 完成通知】{} · {} · {}\n任务: {}\n结果:\n{}",
                task.id,
                task.title,
                task.state.label(),
                trim_chars(&task.task, 500),
                task.result
            ));
        }
    }
    out
}

pub fn list_text(id: Option<&str>) -> String {
    let rt = runtime().lock().unwrap();
    if let Some(id) = id {
        let Some(task) = rt.tasks.get(id) else {
            return format!("没有找到后台 Agent: {id}");
        };
        let result = if task.result.is_empty() {
            task.progress.clone()
        } else {
            task.result.clone()
        };
        return format!(
            "{} · {} · {}\n父会话: {}\n模型: {}\n任务: {}\n\n{}",
            task.id,
            task.title,
            task.state.label(),
            task.parent_session,
            task.model,
            trim_chars(&task.task, 500),
            result
        );
    }
    if rt.tasks.is_empty() {
        return "当前没有后台 Agent".into();
    }
    let mut lines = vec![format!("后台 Agent（运行 {}）:", rt.active)];
    for task in rt.tasks.values().rev().take(20) {
        lines.push(format!(
            "- {} · {} · {} · {}",
            task.id,
            task.state.label(),
            task.model,
            task.title
        ));
    }
    lines.join("\n")
}

fn wait_result(args: &Value) -> Result<String, String> {
    let id = args.get("id").and_then(Value::as_str).ok_or("wait 需要 id")?;
    let wait_secs = args
        .get("wait_secs")
        .or_else(|| args.get("timeout"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(WAIT_LIMIT_SECS);
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    loop {
        let (terminal, local_waiting) = {
            let rt = runtime().lock().unwrap();
            let task = rt.tasks.get(id).ok_or_else(|| format!("没有找到后台 Agent: {id}"))?;
            (
                task.state.terminal(),
                task.state == TaskState::Queued && task.progress.contains("本地模型"),
            )
        };
        if terminal || wait_secs == 0 || Instant::now() >= deadline {
            return Ok(list_text(Some(id)));
        }
        if local_waiting && crate::agent::runtime_is_busy() {
            return Ok(format!(
                "{id} 正等待当前主回合结束后使用本地模型。不要在本回合继续轮询，完成后界面会自动通知。"
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn stop(id: &str) -> Result<String, String> {
    let cancel = {
        let rt = runtime().lock().unwrap();
        let task = rt.tasks.get(id).ok_or_else(|| format!("没有找到后台 Agent: {id}"))?;
        if task.state.terminal() {
            return Ok(format!("{id} 已经是 {}", task.state.label()));
        }
        task.cancel.clone()
    };
    cancel.store(true, Ordering::Relaxed);
    Ok(format!("已请求停止 {id}"))
}

pub fn shutdown_all() {
    let rt = runtime().lock().unwrap();
    for task in rt.tasks.values() {
        if !task.state.terminal() {
            task.cancel.store(true, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_allowlist_denies_shell_and_nested_agents() {
        let _guard = ChildGuard::enter();
        assert!(authorize_tool_call("readline", &serde_json::json!({})).is_ok());
        assert!(authorize_tool_call(
            "execute_command",
            &serde_json::json!({"op":"grep","args":{"pattern":"x"}})
        )
        .is_ok());
        assert!(authorize_tool_call("execute_command", &serde_json::json!({"cmd":"ls"})).is_err());
        assert!(authorize_tool_call("spawn_agent", &serde_json::json!({})).is_err());
    }

    #[test]
    fn local_urls_are_recognized() {
        assert!(local_provider("http://127.0.0.1:8080/v1"));
        assert!(local_provider("http://localhost:11434/v1"));
        assert!(!local_provider("https://api.deepseek.com"));
    }

    #[test]
    fn mock_background_agent_runs_to_completion_and_returns_result() {
        let dir = std::env::temp_dir().join(format!(
            "yjlcoder-subagent-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut cfg = Config::default();
        cfg.set_test_data_dir(dir.clone());
        let mut store = SessionStore::new(cfg.sessions_dir());
        let llm = Llm::mock();
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
        let _turn = begin_turn("main");
        let started = execute(
            &serde_json::json!({
                "action":"spawn",
                "name":"mock-check",
                "task":"读取项目信息并返回一段简短、可验证的测试结论"
            }),
            &mut ctx,
        )
        .unwrap();
        let id = started
            .strip_prefix("已创建 ")
            .and_then(|rest| rest.split('（').next())
            .unwrap()
            .to_string();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if runtime()
                .lock()
                .unwrap()
                .tasks
                .get(&id)
                .is_some_and(|task| task.state.terminal())
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let detail = list_text(Some(&id));
        assert!(detail.contains("completed"), "{detail}");
        assert!(detail.contains("mock 模式完成"), "{detail}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
