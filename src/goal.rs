









use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;

pub const BLOCKED_CONSECUTIVE_THRESHOLD: usize = 3;


pub const RUNTIME_FAILURE_LIMIT: usize = 3;
pub const MAX_GOAL_TURNS: usize = 150;
pub const MAX_OBJECTIVE_CHARS: usize = 4_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    BudgetLimited,
    MaxTurns,
    Complete,
}

impl GoalStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "进行中",
            Self::Paused => "已暂停",
            Self::Blocked => "已阻塞",
            Self::BudgetLimited => "预算已用完",
            Self::MaxTurns => "达到续跑上限",
            Self::Complete => "已完成",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalState {
    pub objective: String,
    pub status: GoalStatus,
    pub token_budget: Option<usize>,
    pub tokens_used: usize,
    
    pub active_started_at_ms: u64,
    pub accumulated_active_ms: u64,
    pub blocked_attempts: usize,
    pub last_block_reason: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub turns_executed: usize,
    
    
    #[serde(default)]
    pub runtime_failures: usize,
    #[serde(default)]
    pub last_runtime_error: Option<String>,
}

impl GoalState {
    pub fn new(objective: String, token_budget: Option<usize>) -> Self {
        let now = now_ms();
        Self {
            objective,
            status: GoalStatus::Active,
            token_budget: token_budget.filter(|n| *n > 0),
            tokens_used: 0,
            active_started_at_ms: now,
            accumulated_active_ms: 0,
            blocked_attempts: 0,
            last_block_reason: None,
            created_at_ms: now,
            updated_at_ms: now,
            turns_executed: 0,
            runtime_failures: 0,
            last_runtime_error: None,
        }
    }

    pub fn active_elapsed_ms(&self) -> u64 {
        let ongoing = if self.status == GoalStatus::Active {
            now_ms().saturating_sub(self.active_started_at_ms)
        } else {
            0
        };
        self.accumulated_active_ms.saturating_add(ongoing)
    }

    pub fn elapsed_label(&self) -> String {
        let seconds = self.active_elapsed_ms() / 1_000;
        if seconds < 60 {
            format!("{seconds}s")
        } else {
            format!("{}m {}s", seconds / 60, seconds % 60)
        }
    }

    fn stop_active_clock(&mut self) {
        if self.status == GoalStatus::Active {
            self.accumulated_active_ms = self
                .accumulated_active_ms
                .saturating_add(now_ms().saturating_sub(self.active_started_at_ms));
        }
    }

    pub fn pause(&mut self) -> Result<(), String> {
        if self.status != GoalStatus::Active {
            return Err(format!("目标当前状态是{}，不能暂停", self.status.label()));
        }
        self.stop_active_clock();
        self.status = GoalStatus::Paused;
        self.updated_at_ms = now_ms();
        Ok(())
    }

    pub fn resume(&mut self) -> Result<(), String> {
        if self.status != GoalStatus::Paused {
            return Err("只有已暂停的目标可以 /goal resume".into());
        }
        self.status = GoalStatus::Active;
        self.active_started_at_ms = now_ms();
        self.updated_at_ms = self.active_started_at_ms;
        self.blocked_attempts = 0;
        self.last_block_reason = None;
        self.runtime_failures = 0;
        self.last_runtime_error = None;
        Ok(())
    }

    pub fn continue_from_max_turns(&mut self) -> Result<(), String> {
        if self.status != GoalStatus::MaxTurns {
            return Err("当前目标没有处于续跑上限状态".into());
        }
        self.turns_executed = 0;
        self.status = GoalStatus::Active;
        self.active_started_at_ms = now_ms();
        self.updated_at_ms = self.active_started_at_ms;
        self.blocked_attempts = 0;
        self.last_block_reason = None;
        self.runtime_failures = 0;
        self.last_runtime_error = None;
        Ok(())
    }

    pub fn complete(&mut self) {
        self.stop_active_clock();
        self.status = GoalStatus::Complete;
        self.updated_at_ms = now_ms();
    }

    
    pub fn record_blocked(&mut self, reason: &str) -> Result<usize, String> {
        if self.status != GoalStatus::Active {
            return Err("目标不在进行中，不能登记阻塞".into());
        }
        let reason = reason.trim();
        if reason.is_empty() {
            return Err("blocked 必须提供具体 reason".into());
        }
        let normalized = normalize_reason(reason);
        let same = self
            .last_block_reason
            .as_deref()
            .map(normalize_reason)
            .as_deref()
            == Some(normalized.as_str());
        self.blocked_attempts = if same { self.blocked_attempts + 1 } else { 1 };
        self.last_block_reason = Some(reason.to_string());
        self.updated_at_ms = now_ms();
        if self.blocked_attempts >= BLOCKED_CONSECUTIVE_THRESHOLD {
            self.stop_active_clock();
            self.status = GoalStatus::Blocked;
        }
        Ok(self.blocked_attempts)
    }

    pub fn add_tokens(&mut self, delta: usize) {
        if self.status != GoalStatus::Active || delta == 0 {
            return;
        }
        self.tokens_used = self.tokens_used.saturating_add(delta);
        self.updated_at_ms = now_ms();
        if self.token_budget.is_some_and(|limit| self.tokens_used >= limit) {
            self.stop_active_clock();
            self.status = GoalStatus::BudgetLimited;
        }
    }

    pub fn status_text(&self) -> String {
        let budget = self
            .token_budget
            .map(|n| format!("{} / {n}", self.tokens_used))
            .unwrap_or_else(|| self.tokens_used.to_string());
        let mut out = format!(
            "目标: {}\n状态: {}\n活跃时间: {}\nToken: {}\n续跑轮数: {} / {}",
            self.objective,
            self.status.label(),
            self.elapsed_label(),
            budget,
            self.turns_executed,
            MAX_GOAL_TURNS
        );
        if self.status == GoalStatus::MaxTurns {
            out.push_str("\n提示: 输入 /goal continue 清零轮数并继续");
        }
        if let Some(reason) = &self.last_block_reason {
            out.push_str(&format!("\n最近阻塞: {reason}（{}/{}）", self.blocked_attempts, BLOCKED_CONSECUTIVE_THRESHOLD));
        }
        if let Some(error) = &self.last_runtime_error {
            out.push_str(&format!(
                "\n最近运行错误: {error}（连续 {}/{}）",
                self.runtime_failures, RUNTIME_FAILURE_LIMIT
            ));
        }
        out
    }
}



#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeFailureAction {
    Retry { attempt: usize, delay_secs: u64, turn: usize, prompt: String },
    Paused { attempts: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalCommandResult {
    pub notice: String,
    
    pub prompt: Option<String>,
}

pub fn handle_slash(cfg: &Config, session_id: &str, args: &str) -> GoalCommandResult {
    let trimmed = args.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("status") {
        return GoalCommandResult {
            notice: load(cfg, session_id)
                .map(|g| g.status_text())
                .unwrap_or_else(|| "当前没有目标。用 /goal <目标> 开始。".into()),
            prompt: None,
        };
    }
    let lower = trimmed.to_ascii_lowercase();
    if matches!(lower.as_str(), "clear" | "stop" | "off" | "reset" | "none" | "cancel") {
        let existed = clear(cfg, session_id);
        return GoalCommandResult {
            notice: if existed { "目标已清除".into() } else { "当前没有目标".into() },
            prompt: None,
        };
    }

    let mut state = match load(cfg, session_id) {
        Some(g) => g,
        None if matches!(lower.as_str(), "pause" | "resume" | "continue" | "complete") => {
            return GoalCommandResult { notice: "当前没有目标".into(), prompt: None };
        }
        None => {
            let (budget, objective) = parse_objective(trimmed);
            return set_from_command(cfg, session_id, objective, budget);
        }
    };

    let action = match lower.as_str() {
        "pause" => state.pause().map(|_| ("目标已暂停".into(), None)),
        "resume" => state.resume().map(|_| {
            state.turns_executed += 1;
            ("目标已恢复，立即继续".into(), Some(build_continuation_prompt(&state)))
        }),
        "continue" => state.continue_from_max_turns().map(|_| {
            state.turns_executed = 1;
            ("续跑计数已清零，立即继续".into(), Some(build_continuation_prompt(&state)))
        }),
        "complete" => {
            state.complete();
            Ok(("目标已手动标记完成".into(), None))
        }
        _ => {
            let (budget, objective) = parse_objective(trimmed);
            return set_from_command(cfg, session_id, objective, budget);
        }
    };
    match action {
        Ok((notice, prompt)) => {
            let _ = save(cfg, session_id, &state);
            GoalCommandResult { notice, prompt }
        }
        Err(e) => GoalCommandResult { notice: e, prompt: None },
    }
}

fn set_from_command(
    cfg: &Config,
    session_id: &str,
    objective: String,
    token_budget: Option<usize>,
) -> GoalCommandResult {
    let count = objective.chars().count();
    if objective.trim().is_empty() {
        return GoalCommandResult { notice: "目标不能为空".into(), prompt: None };
    }
    if count > MAX_OBJECTIVE_CHARS {
        return GoalCommandResult {
            notice: format!("目标太长（{count} 字符，上限 {MAX_OBJECTIVE_CHARS}）；请把细节写进文件后引用"),
            prompt: None,
        };
    }
    let mut state = GoalState::new(objective, token_budget);
    state.turns_executed = 1;
    state.updated_at_ms = now_ms();
    let prompt = build_objective_prompt(&state);
    match save(cfg, session_id, &state) {
        Ok(()) => GoalCommandResult { notice: "目标已设置，开始执行".into(), prompt: Some(prompt) },
        Err(e) => GoalCommandResult { notice: format!("目标保存失败: {e}"), prompt: None },
    }
}


pub fn record_turn_usage(cfg: &Config, session_id: &str, tokens: usize) -> Option<GoalState> {
    let mut state = load(cfg, session_id)?;
    
    state.runtime_failures = 0;
    state.last_runtime_error = None;
    state.add_tokens(tokens);
    let _ = save(cfg, session_id, &state);
    Some(state)
}





pub fn record_runtime_failure(
    cfg: &Config,
    session_id: &str,
    error: &str,
) -> Result<Option<RuntimeFailureAction>, String> {
    let Some(mut state) = load(cfg, session_id) else { return Ok(None) };
    if state.status != GoalStatus::Active {
        return Ok(None);
    }

    state.runtime_failures = state.runtime_failures.saturating_add(1);
    state.last_runtime_error = Some(error.trim().chars().take(1_000).collect());
    state.updated_at_ms = now_ms();
    let attempt = state.runtime_failures;

    if attempt >= RUNTIME_FAILURE_LIMIT {
        state.stop_active_clock();
        state.status = GoalStatus::Paused;
        save(cfg, session_id, &state)?;
        return Ok(Some(RuntimeFailureAction::Paused { attempts: attempt }));
    }

    if state.turns_executed >= MAX_GOAL_TURNS {
        state.stop_active_clock();
        state.status = GoalStatus::MaxTurns;
        save(cfg, session_id, &state)?;
        return Ok(None);
    }

    state.turns_executed += 1;
    let turn = state.turns_executed;
    let delay_secs = 1u64 << attempt; 
    let prompt = format!(
        "<goal-steering type=\"runtime_recovery\">\n上一轮因临时运行错误中断：{}\n继续推进目标：{}\n当前续跑轮数：{}。保留已经完成的工作，不要从头重复；先核对当前文件和服务状态，再从中断点继续。\n</goal-steering>",
        state.last_runtime_error.as_deref().unwrap_or("未知错误"),
        state.objective,
        turn
    );
    save(cfg, session_id, &state)?;
    Ok(Some(RuntimeFailureAction::Retry { attempt, delay_secs, turn, prompt }))
}


pub fn next_continuation(cfg: &Config, session_id: &str) -> Result<Option<(usize, String)>, String> {
    let Some(mut state) = load(cfg, session_id) else { return Ok(None) };
    if state.status != GoalStatus::Active {
        return Ok(None);
    }
    if state.turns_executed >= MAX_GOAL_TURNS {
        state.stop_active_clock();
        state.status = GoalStatus::MaxTurns;
        state.updated_at_ms = now_ms();
        save(cfg, session_id, &state)?;
        return Ok(None);
    }
    state.turns_executed += 1;
    state.updated_at_ms = now_ms();
    let turn = state.turns_executed;
    let prompt = build_continuation_prompt(&state);
    save(cfg, session_id, &state)?;
    Ok(Some((turn, prompt)))
}

pub fn execute_tool(cfg: &Config, session_id: &str, args: &Value) -> Result<String, String> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_else(|| if args.get("status").is_some() { "update" } else { "get" });
    if action == "get" {
        return Ok(match load(cfg, session_id) {
            Some(g) => json!({
                "success": true,
                "goal": {
                    "objective": g.objective,
                    "status": g.status.label(),
                    "tokens_used": g.tokens_used,
                    "token_budget": g.token_budget,
                    "elapsed": g.elapsed_label(),
                    "turns_executed": g.turns_executed
                }
            })
            .to_string(),
            None => json!({"success":true,"message":"当前没有目标；用户可用 /goal <目标> 创建"}).to_string(),
        });
    }
    if action != "update" {
        return Err("goal.action 只能是 get 或 update".into());
    }
    let status = args.get("status").and_then(Value::as_str).ok_or(
        "goal update 缺少 status；只允许 complete 或 blocked",
    )?;
    let mut state = load(cfg, session_id).ok_or("当前没有可更新的目标")?;
    match status {
        "complete" => {
            let reason = args.get("reason").and_then(Value::as_str).unwrap_or("").trim();
            if reason.is_empty() {
                return Err("标记 complete 前必须提供 completion audit 的证据摘要到 reason".into());
            }
            state.complete();
            save(cfg, session_id, &state)?;
            Ok(format!(
                "目标已完成。Token {}{}，活跃时间 {}，续跑 {} 轮。证据: {}",
                state.tokens_used,
                state.token_budget.map(|n| format!("/{n}")).unwrap_or_default(),
                state.elapsed_label(),
                state.turns_executed,
                reason
            ))
        }
        "blocked" => {
            let reason = args.get("reason").and_then(Value::as_str).ok_or(
                "标记 blocked 必须提供不可克服的具体 reason",
            )?;
            let attempts = state.record_blocked(reason)?;
            save(cfg, session_id, &state)?;
            if state.status == GoalStatus::Blocked {
                Ok(format!("同一阻塞连续 {attempts} 轮，目标已标记阻塞: {reason}"))
            } else {
                Ok(format!(
                    "已登记阻塞尝试 {attempts}/{BLOCKED_CONSECUTIVE_THRESHOLD}；目标仍继续，不能提前停止"
                ))
            }
        }
        _ => Err("goal.status 只能是 complete 或 blocked".into()),
    }
}

pub fn build_objective_prompt(goal: &GoalState) -> String {
    format!(
        "<goal-steering type=\"objective_updated\">\n目标：{}\n立即开始执行。保持完整范围；完成后先核对每项要求及测试/文件/命令证据，再调用 execute_command 调度 goal：{{\"op\":\"goal\",\"args\":{{\"action\":\"update\",\"status\":\"complete\",\"reason\":\"证据摘要\"}}}}。遇到阻碍继续尝试，同一不可克服原因连续三轮后才可报告 blocked。\n</goal-steering>",
        goal.objective
    )
}

pub fn build_continuation_prompt(goal: &GoalState) -> String {
    format!(
        "<goal-steering type=\"continuation\">\n继续推进目标：{}\n当前续跑轮数：{}。不要缩小范围，也不要只汇报计划。完成前逐项核对原始要求和权威证据；完全完成才调用 goal 标记 complete。困难、缓慢或部分未完成不算 blocked；同一不可克服原因连续三轮后才可调用 goal 报 blocked。现在继续工作。\n</goal-steering>",
        goal.objective, goal.turns_executed
    )
}

pub fn load(cfg: &Config, session_id: &str) -> Option<GoalState> {
    let text = fs::read_to_string(goal_path(cfg, session_id)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save(cfg: &Config, session_id: &str, state: &GoalState) -> Result<(), String> {
    let path = goal_path(cfg, session_id);
    let dir = path.parent().ok_or("目标路径缺少父目录")?;
    fs::create_dir_all(dir).map_err(|e| format!("创建目标目录失败: {e}"))?;
    let tmp = dir.join(format!(
        ".goal-{}-{}-{}.tmp",
        safe_id(session_id),
        std::process::id(),
        now_ms()
    ));
    let text = serde_json::to_string_pretty(state).map_err(|e| e.to_string())?;
    fs::write(&tmp, text).map_err(|e| format!("写目标临时文件失败: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("提交目标状态失败: {e}")
    })
}

pub fn clear(cfg: &Config, session_id: &str) -> bool {
    fs::remove_file(goal_path(cfg, session_id)).is_ok()
}

fn goal_path(cfg: &Config, session_id: &str) -> PathBuf {
    cfg.data_dir().join("goals").join(format!("{}.json", safe_id(session_id)))
}

fn safe_id(id: &str) -> String {
    let filtered: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '_' })
        .take(120)
        .collect();
    if filtered.is_empty() { "main".into() } else { filtered }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn normalize_reason(reason: &str) -> String {
    reason.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase()
}

fn parse_objective(input: &str) -> (Option<usize>, String) {
    let trimmed = input.trim();
    if let Some(rest) = trimmed.strip_prefix("--tokens ") {
        if let Some((number, objective)) = rest.trim().split_once(' ') {
            if let Ok(n) = number.parse::<usize>() {
                if n > 0 {
                    return (Some(n), objective.trim().to_string());
                }
            }
        }
    }
    (None, trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config(tag: &str) -> (Config, PathBuf) {
        let dir = std::env::temp_dir().join(format!("yjlcoder-goal-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut cfg = Config::default();
        cfg.set_test_data_dir(dir.clone());
        (cfg, dir)
    }

    #[test]
    fn state_persists_per_session_and_continues() {
        let (cfg, dir) = temp_config("persist");
        let first = handle_slash(&cfg, "s1", "--tokens 1000 完成全部测试");
        assert!(first.prompt.is_some());
        let g = load(&cfg, "s1").unwrap();
        assert_eq!(g.turns_executed, 1);
        assert_eq!(g.token_budget, Some(1000));
        assert!(load(&cfg, "s2").is_none());
        let (turn, prompt) = next_continuation(&cfg, "s1").unwrap().unwrap();
        assert_eq!(turn, 2);
        assert!(prompt.contains("完成全部测试"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn blocked_requires_same_reason_three_times() {
        let mut g = GoalState::new("x".into(), None);
        assert_eq!(g.record_blocked("网络断开").unwrap(), 1);
        assert_eq!(g.record_blocked("权限缺失").unwrap(), 1);
        assert_eq!(g.record_blocked("权限   缺失").unwrap(), 2);
        assert_eq!(g.record_blocked("权限 缺失").unwrap(), 3);
        assert_eq!(g.status, GoalStatus::Blocked);
    }

    #[test]
    fn command_controls_lifecycle() {
        let (cfg, dir) = temp_config("commands");
        handle_slash(&cfg, "main", "修复问题");
        assert!(handle_slash(&cfg, "main", "pause").notice.contains("暂停"));
        assert_eq!(load(&cfg, "main").unwrap().status, GoalStatus::Paused);
        assert!(handle_slash(&cfg, "main", "resume").prompt.is_some());
        assert!(handle_slash(&cfg, "main", "clear").notice.contains("清除"));
        assert!(load(&cfg, "main").is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn runtime_errors_retry_then_pause_instead_of_zombie_active() {
        let (cfg, dir) = temp_config("runtime-recovery");
        handle_slash(&cfg, "main", "修复所有问题");

        let first = record_runtime_failure(&cfg, "main", "HTTP 断链")
            .unwrap()
            .unwrap();
        assert!(matches!(
            first,
            RuntimeFailureAction::Retry { attempt: 1, delay_secs: 2, turn: 2, .. }
        ));
        assert_eq!(load(&cfg, "main").unwrap().status, GoalStatus::Active);

        let second = record_runtime_failure(&cfg, "main", "HTTP 断链")
            .unwrap()
            .unwrap();
        assert!(matches!(
            second,
            RuntimeFailureAction::Retry { attempt: 2, delay_secs: 4, turn: 3, .. }
        ));

        let third = record_runtime_failure(&cfg, "main", "HTTP 断链")
            .unwrap()
            .unwrap();
        assert_eq!(third, RuntimeFailureAction::Paused { attempts: 3 });
        let saved = load(&cfg, "main").unwrap();
        assert_eq!(saved.status, GoalStatus::Paused);
        assert_eq!(saved.runtime_failures, 3);

        let resumed = handle_slash(&cfg, "main", "resume");
        assert!(resumed.prompt.is_some());
        let saved = load(&cfg, "main").unwrap();
        assert_eq!(saved.runtime_failures, 0);
        assert!(saved.last_runtime_error.is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_goal_json_defaults_new_runtime_fields() {
        let json = r#"{
            "objective":"旧目标","status":"active","token_budget":null,"tokens_used":0,
            "active_started_at_ms":1,"accumulated_active_ms":0,"blocked_attempts":0,
            "last_block_reason":null,"created_at_ms":1,"updated_at_ms":1,"turns_executed":1
        }"#;
        let state: GoalState = serde_json::from_str(json).unwrap();
        assert_eq!(state.runtime_failures, 0);
        assert!(state.last_runtime_error.is_none());
    }
}
