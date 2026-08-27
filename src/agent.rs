use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::sync::Mutex;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use crate::backend::TokenBudget;
use crate::compress;
use crate::config::Config;
use crate::llm::{ChatRequest, Llm, Msg, ToolCall};
use crate::prompt::{
    CHAT_ONLY_PROMPT, CHAT_PHILOSOPHY, FINALIZE_PROMPT, NATIVE_QQ_SYSTEM_PROMPT,
    NATIVE_SYSTEM_PROMPT, QQ_SYSTEM_PROMPT, SYSTEM_PROMPT,
};
use crate::registry::native_tools_json;
use crate::session::SessionStore;
use crate::tools::{
    AskAnswer, AskRequest, PermDecision, PermRequest, QqOut, ToolCtx, execute,
};
const MAX_RELOADS: usize = 2;
const MAX_IDENTICAL_TOOL_CALLS: usize = 2;
const MAX_IDENTICAL_TOOL_RESULTS: usize = 3;
const MAX_CONSECUTIVE_TOOL_FAILURES: usize = 4;
const NO_PERM_MSG: &str = "（无权限：当前用户只能闲聊，无法操作电脑或调用工具。请礼貌拒绝，不要尝试其他工具。）";
static ACTIVE_AGENT_TURNS: AtomicUsize = AtomicUsize::new(0);

struct ActiveTurnGuard;
impl ActiveTurnGuard {
    fn enter() -> Self {
        ACTIVE_AGENT_TURNS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}
impl Drop for ActiveTurnGuard {
    fn drop(&mut self) {
        ACTIVE_AGENT_TURNS.fetch_sub(1, Ordering::AcqRel);
    }
}
pub fn runtime_is_busy() -> bool {
    ACTIVE_AGENT_TURNS.load(Ordering::Acquire) > 0
}
fn system_prompt_for(cfg: &Config, chat_mode: bool, chat_only: bool) -> String {
    if crate::subagents::in_child() {
        return crate::subagents::child_system_prompt().to_string();
    }
    let base = if chat_only {
        CHAT_ONLY_PROMPT
    } else if chat_mode && cfg.provider.native_tools {
        NATIVE_QQ_SYSTEM_PROMPT
    } else if chat_mode {
        QQ_SYSTEM_PROMPT
    } else if cfg.provider.native_tools {
        NATIVE_SYSTEM_PROMPT
    } else {
        SYSTEM_PROMPT
    };
    if chat_mode && !chat_only {
        format!("{base}\n\n{CHAT_PHILOSOPHY}")
    } else {
        base.to_string()
    }
}
pub fn effective_context_window(cfg: &Config, llm: &Llm) -> usize {
    crate::backend::effective_window_for_model(
        cfg.provider.ctx_override,
        llm.n_ctx(),
        cfg.provider.ctx_window,
        &llm.model_name(),
    )
}
pub fn estimate_context_tokens(
    cfg: &Config,
    llm: &Llm,
    messages: &[Msg],
    chat_mode: bool,
    chat_only: bool,
) -> usize {
    let mut request_messages = vec![Msg::new(
        "system",
        system_prompt_for(cfg, chat_mode, chat_only),
    )];
    request_messages.extend_from_slice(messages);
    llm.estimate_prompt_tokens(&ChatRequest {
        messages: request_messages,
        tools: if cfg.provider.native_tools && !chat_only {
            Some(native_tools_json())
        } else {
            None
        },
        max_tokens: None,
        stream: !chat_mode,
    })
}
#[derive(Default)]
struct ToolLoopGuard {
    last_call: Option<String>,
    identical_calls: usize,
    last_result_hash: Option<u64>,
    identical_results: usize,
    consecutive_failures: usize,
}
impl ToolLoopGuard {
    fn before_call(&mut self, op: &str, args: &Value) -> Option<String> {
        let signature = effective_tool_signature(op, args);
        if self.last_call.as_deref() == Some(signature.as_str()) {
            self.identical_calls += 1;
        } else {
            self.last_call = Some(signature);
            self.identical_calls = 1;
        }
        if self.identical_calls > MAX_IDENTICAL_TOOL_CALLS {
            Some(format!(
                "检测到相同工具调用连续重复 {} 次，已停止空转",
                self.identical_calls
            ))
        } else {
            None
        }
    }
    fn after_result(&mut self, output: &str) -> Option<String> {
        let mut hasher = DefaultHasher::new();
        output.hash(&mut hasher);
        let hash = hasher.finish();
        if self.last_result_hash == Some(hash) {
            self.identical_results += 1;
        } else {
            self.last_result_hash = Some(hash);
            self.identical_results = 1;
        }
        if tool_result_failed(output) {
            self.consecutive_failures += 1;
        } else {
            self.consecutive_failures = 0;
        }
        if self.identical_results >= MAX_IDENTICAL_TOOL_RESULTS {
            return Some(format!(
                "工具已连续 {} 次返回相同结果，已停止空转",
                self.identical_results
            ));
        }
        if self.consecutive_failures >= MAX_CONSECUTIVE_TOOL_FAILURES {
            return Some(format!(
                "工具已连续失败 {} 次，已停止重试",
                self.consecutive_failures
            ));
        }
        None
    }
}
fn effective_tool_signature(op: &str, args: &Value) -> String {
    let (effective_op, effective_args) = if op == "execute_command" {
        match args.get("op").and_then(Value::as_str) {
            Some(inner) if inner != "execute_command" => {
                let inner_args = args.get("args").unwrap_or(args);
                (inner, inner_args)
            }
            _ => (op, args),
        }
    } else {
        (op, args)
    };
    format!(
        "{effective_op}:{}",
        serde_json::to_string(effective_args).unwrap_or_default()
    )
}
fn tool_result_failed(output: &str) -> bool {
    if output.trim_start().starts_with("错误:") {
        return true;
    }
    output
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("exit code: "))
        .and_then(|code| code.trim().parse::<i32>().ok())
        .is_some_and(|code| code != 0)
}
fn focused_finalize_messages(history: &[Msg], reason: &str) -> Vec<Msg> {
    let is_real_user = |m: &&Msg| {
        m.role == "user"
            && !m.content.starts_with("【工具结果】")
            && !m.content.starts_with("【系统提示】")
            && !m.content.starts_with("【控制层】")
            && !m.content.starts_with("（无权限：")
    };
    let question_idx = history
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| is_real_user(m))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let question = history
        .get(question_idx)
        .map(|m| m.content.trim())
        .unwrap_or("");
    let evidence: Vec<&str> = history
        .iter()
        .skip(question_idx.saturating_add(1))
        .filter(|m| m.role == "tool" || m.content.starts_with("【工具结果】"))
        .map(|m| m.content.as_str())
        .collect();
    let mut packet = format!("用户问题：\n{question}\n\n收尾原因：\n{reason}");
    if !evidence.is_empty() {
        packet.push_str("\n\n本回合已有工具结果（这是唯一事实来源）：\n");
        packet.push_str(&evidence.join("\n\n---\n\n"));
    }
    packet.push_str("\n\n直接输出最终答案正文，不要写思考过程或工具调用。");
    vec![Msg::new("system", FINALIZE_PROMPT), Msg::new("user", packet)]
}
fn deterministic_read_line_answer(user_input: &str, output: &str) -> Option<String> {
    let line_no = requested_line_number(user_input)?;
    let prefix = format!("{line_no}\t");
    let content = output
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))?;
    if content.is_empty() {
        Some(format!("第 {line_no} 行是空行。"))
    } else {
        Some(format!("第 {line_no} 行的完整内容是：\n{content}"))
    }
}
fn requested_line_number(input: &str) -> Option<usize> {
    for (idx, _) in input.match_indices('第') {
        let tail = input[idx + '第'.len_utf8()..].trim_start();
        let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            continue;
        }
        let rest = tail[digits.len()..].trim_start();
        if rest.starts_with('行') {
            if let Ok(n) = digits.parse() {
                return Some(n);
            }
        }
    }
    let lower = input.to_ascii_lowercase();
    for marker in ["line ", "line:", "line #"] {
        if let Some(idx) = lower.find(marker) {
            let tail = lower[idx + marker.len()..].trim_start();
            let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(n) = digits.parse() {
                return Some(n);
            }
        }
    }
    None
}
pub enum AgentEvent {
    Delta(String),
    Reasoning(String),
    TokenProgress(crate::llm::TokenProgress),
    ToolRun { op: String, args: String },
    ToolProgress(crate::tools::ToolProgress),
    ToolResult(String),
    Notice(String),
    Error(String),
    Garbage { kind: String, sample: String, run: usize, total: usize, limit: usize },
    Done(String),
}
fn fwd_stream(ev: crate::llm::StreamEvent, on_event: &mut impl FnMut(AgentEvent)) {
    match ev {
        crate::llm::StreamEvent::Delta(d) => on_event(AgentEvent::Delta(d)),
        crate::llm::StreamEvent::Reasoning(r) => on_event(AgentEvent::Reasoning(r)),
        crate::llm::StreamEvent::TokenProgress(progress) => {
            on_event(AgentEvent::TokenProgress(progress))
        }
        crate::llm::StreamEvent::Garbage { kind, sample, run, total, limit } => {
            on_event(AgentEvent::Garbage { kind: kind.to_string(), sample, run, total, limit })
        }
    }
}
fn trace_record_at(data_root: &std::path::Path, session: &str, ev: &AgentEvent) {
    let dir = data_root.join("trace");
    let _ = std::fs::create_dir_all(&dir);
    let date = &crate::time::now_stamp()[..10];
    let path = dir.join(format!("{date}.jsonl"));
    let rec = match ev {
        AgentEvent::Delta(d) => serde_json::json!({"kind": "delta", "text": d}),
        AgentEvent::Reasoning(r) => serde_json::json!({"kind": "reasoning", "text": r}),
        AgentEvent::TokenProgress(progress) => serde_json::json!({
            "kind": "token_progress",
            "exact": progress.exact,
            "prompt_tokens": progress.usage.prompt_tokens,
            "completion_tokens": progress.usage.completion_tokens,
            "cache_read_tokens": progress.usage.cache_read_tokens,
            "cache_miss_tokens": progress.usage.cache_miss_tokens,
            "reasoning_tokens": progress.usage.reasoning_tokens,
        }),
        AgentEvent::Garbage { kind, sample, run, total, limit } => serde_json::json!({
            "kind": "garbage", "pos": kind, "sample": sample, "run": run, "total": total, "limit": limit
        }),
        AgentEvent::ToolRun { op, args } => serde_json::json!({"kind": "tool_run", "op": op, "args": args}),
        AgentEvent::ToolProgress(progress) => serde_json::json!({
            "kind": "tool_progress",
            "elapsed_secs": progress.elapsed_secs,
            "total_lines": progress.total_lines,
            "total_bytes": progress.total_bytes,
        }),
        AgentEvent::ToolResult(r) => serde_json::json!({"kind": "tool_result", "text": r}),
        AgentEvent::Notice(n) => serde_json::json!({"kind": "notice", "text": n}),
        AgentEvent::Error(e) => serde_json::json!({"kind": "error", "text": e}),
        AgentEvent::Done(d) => serde_json::json!({"kind": "done", "text": d}),
    };
    let line = serde_json::json!({
        "ts": crate::time::now_stamp(),
        "session": session,
        "ev": rec,
    })
    .to_string();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
        let _ = f.flush();
    }
}
pub struct Agent {
    pub llm: Llm,
    pub store: SessionStore,
    pub cfg: Config,
    pub qq_tx: Option<Sender<QqOut>>,
    pub cancel: Arc<AtomicBool>,
    pub chat_mode: bool,
    pub chat_only: bool,
    ask_tx: Option<Sender<AskRequest>>,
    answer_rx: Option<Receiver<AskAnswer>>,
    ask_seq: Arc<AtomicU64>,
    perm_tx: Option<Sender<PermRequest>>,
    perm_rx: Option<Receiver<PermDecision>>,
    perm_auto: Arc<AtomicBool>,
    perm_allowed: Arc<Mutex<HashSet<String>>>,
    perm_seq: Arc<AtomicU64>,
    tool_event_tx: Option<Sender<AgentEvent>>,
    pub last_usage: crate::llm::Usage,
    pub last_timings: crate::llm::Timings,
}
impl Agent {
    pub fn with_store(
        cfg: Config,
        llm: Llm,
        store: SessionStore,
        qq_tx: Option<Sender<QqOut>>,
        chat_mode: bool,
        chat_only: bool,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        Agent {
            llm,
            store,
            cfg,
            qq_tx,
            cancel,
            chat_mode,
            chat_only,
            ask_tx: None,
            answer_rx: None,
            ask_seq: Arc::new(AtomicU64::new(0)),
            perm_tx: None,
            perm_rx: None,
            perm_auto: Arc::new(AtomicBool::new(false)),
            perm_allowed: Arc::new(Mutex::new(HashSet::new())),
            perm_seq: Arc::new(AtomicU64::new(0)),
            tool_event_tx: None,
            last_usage: crate::llm::Usage::default(),
            last_timings: crate::llm::Timings::default(),
        }
    }
    pub fn set_ask_channels(&mut self, tx: Sender<AskRequest>, rx: Receiver<AskAnswer>) {
        self.ask_tx = Some(tx);
        self.answer_rx = Some(rx);
    }
    pub fn set_perm_channels(
        &mut self,
        tx: Sender<PermRequest>,
        rx: Receiver<PermDecision>,
        auto: Arc<AtomicBool>,
        allowed: Arc<Mutex<HashSet<String>>>,
    ) {
        self.perm_tx = Some(tx);
        self.perm_rx = Some(rx);
        self.perm_auto = auto;
        self.perm_allowed = allowed;
    }
    pub fn set_tool_event_channel(&mut self, tx: Sender<AgentEvent>) {
        self.tool_event_tx = Some(tx);
    }
    pub fn for_session(
        cfg: Config,
        llm: Llm,
        session_id: &str,
        qq_tx: Option<Sender<QqOut>>,
        chat_mode: bool,
        chat_only: bool,
        cancel: Arc<AtomicBool>,
    ) -> Self {
        let mut store = SessionStore::new(cfg.sessions_dir());
        store.new_session(session_id);
        Agent::with_store(cfg, llm, store, qq_tx, chat_mode, chat_only, cancel)
    }
    fn active_system_prompt(&self) -> String {
        system_prompt_for(&self.cfg, self.chat_mode, self.chat_only)
    }
    fn maybe_auto_compact(&mut self, on_event: &mut impl FnMut(AgentEvent)) {
        let msgs = self.store.current().messages;
        if msgs.is_empty() {
            return;
        }
        let fixed_tokens = estimate_context_tokens(
            &self.cfg,
            &self.llm,
            &[],
            self.chat_mode,
            self.chat_only,
        );
        let tokens = estimate_context_tokens(
            &self.cfg,
            &self.llm,
            &msgs,
            self.chat_mode,
            self.chat_only,
        );
        let window = effective_context_window(&self.cfg, &self.llm);
        let threshold = self.cfg.tui.compress_threshold;
        if tokens >= window || (tokens as f64) >= window as f64 * threshold {
            on_event(AgentEvent::Notice(format!(
                "上下文 {tokens}/{window} tok 超阈值，自动压缩中…"
            )));
            let target_total = (window as f64 * threshold * 0.8) as usize;
            let history_budget = target_total.saturating_sub(fixed_tokens).max(128);
            match compress::compact_to_limit(&self.llm, &msgs, &self.cancel, history_budget) {
                Ok(new_hist) => {
                    let after = estimate_context_tokens(
                        &self.cfg,
                        &self.llm,
                        &new_hist,
                        self.chat_mode,
                        self.chat_only,
                    );
                    if let Err(e) = self.store.replace_current(&new_hist) {
                        on_event(AgentEvent::Error(format!("压缩落盘失败: {e}")));
                        return;
                    }
                    on_event(AgentEvent::Notice(format!("压缩完成：~{tokens} -> ~{after} tok")));
                }
                Err(e) => on_event(AgentEvent::Error(format!("压缩失败: {e}"))),
            }
        }
    }
    pub fn run_turn(&mut self, user_input: &str, on_event: &mut impl FnMut(AgentEvent)) -> Result<String, String> {
        let _local_inference = crate::subagents::local_inference_guard(&self.cfg);
        let _active_turn = ActiveTurnGuard::enter();
        self.cancel.store(false, Ordering::Relaxed);
        self.last_usage = crate::llm::Usage::default();
        self.last_timings = crate::llm::Timings::default();
        let trace_enabled = self.cfg.trace.enabled;
        let session_id = self.store.current_id().to_string();
        let _subagent_turn = crate::subagents::begin_turn(&session_id);
        if !crate::subagents::in_child() {
            for result in crate::subagents::take_parent_results(&session_id) {
                self.store.append(&Msg::new("user", result));
            }
        }
        let trace_session = session_id.clone();
        let trace_root = self.cfg.data_dir();
        let mut traced = move |ev: AgentEvent| {
            if trace_enabled && !matches!(&ev, AgentEvent::Delta(_) | AgentEvent::Reasoning(_)) {
                trace_record_at(&trace_root, &trace_session, &ev);
            }
            on_event(ev);
        };
        self.store.append(&Msg::new("user", user_input));
        if self.store.is_dirty() {
            self.store.set_dirty(false);
        }
        self.maybe_auto_compact(&mut traced);
        let mut tool_guard = ToolLoopGuard::default();
        let mut has_tool_result = false;
        if !self.chat_only {
            if let Some((op, args)) = crate::tools::keyword_tool(user_input) {
            let _ = tool_guard.before_call(op, &args);
            let args_str = serde_json::to_string(&args).unwrap_or_default();
            traced(AgentEvent::ToolRun { op: op.to_string(), args: args_str });
            let output = self.run_op(op, &args);
            has_tool_result = true;
            traced(AgentEvent::ToolResult(output.clone()));
            let _ = tool_guard.after_result(&output);
            let result_msg = format!(
                "【工具结果】{op} 已返回（系统已直接执行，根据结果回答，不要再重复调用该工具）:\n{}",
                self.prepare_tool_output(op, &args, &output)
            );
            self.store.append(&message_with_tool_image("user", result_msg, None));
            if self.cfg.fuckloop && op == "readline" {
                if let Some(answer) = deterministic_read_line_answer(user_input, &output) {
                    self.store.append(&Msg::new("assistant", answer.clone()));
                    traced(AgentEvent::Delta(answer.clone()));
                    if trace_enabled {
                        trace_record_at(
                            &self.cfg.data_dir(),
                            &session_id,
                            &AgentEvent::Done(answer.clone()),
                        );
                    }
                    return Ok(answer);
                }
            }
            }
        }
        let final_text: String;
        let mut iter = 0;
        let mut reloads = 0;
        let mut interrupts = 0;
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err("已取消".into());
            }
            if iter >= self.cfg.tool_times {
                let reason = format!(
                    "工具循环次数已达上限（{}），结束本轮",
                    self.cfg.tool_times
                );
                final_text = self.finalize_without_tools(&reason, &mut traced);
                self.store.append(&Msg::new("assistant", final_text.clone()));
                break;
            }
            iter += 1;
            let msgs = self.store.current().messages;
            let sys = self.active_system_prompt();
            let mut req_msgs = vec![Msg::new("system", sys)];
            req_msgs.extend(msgs.clone());
            let req = ChatRequest {
                messages: req_msgs,
                tools: if self.cfg.provider.native_tools && !self.chat_only { Some(native_tools_json()) } else { None },
                max_tokens: self.llm.max_tokens_for(
                    if self.chat_mode { TokenBudget::QqMain } else { TokenBudget::TuiMain },
                    self.cfg.qq.max_tokens,
                ),
                stream: !self.chat_mode,
            };
            let attempt = if self.cfg.fuckloop && has_tool_result {
                self.llm
                    .stream_without_reasoning(&req, &self.cancel, |ev| fwd_stream(ev, &mut traced))
            } else {
                self.llm
                    .stream(&req, &self.cancel, |ev| fwd_stream(ev, &mut traced))
            };
            let result = match attempt {
                Ok(r) => r,
                Err(e) => {
                    if !self.cfg.provider.auto_reload
                        || !self.llm.is_llamacpp()
                        || self.cancel.load(Ordering::Relaxed)
                    {
                        return Err(e);
                    }
                    if reloads >= MAX_RELOADS {
                        return Err(format!("{e}（重载已达 {MAX_RELOADS} 次上限，可能模型未加载完成，稍后再试）"));
                    }
                    reloads += 1;
                    traced(AgentEvent::Notice(format!("请求失败（{e}），重载模型后重试…")));
                    if self.reload_model(&mut traced).is_err() {
                        return Err(format!("{e}（重载也未成功，结束本轮）"));
                    }
                    interrupts = 0;
                    if self.cfg.fuckloop && has_tool_result {
                        self.llm.stream_without_reasoning(&req, &self.cancel, |ev| {
                            fwd_stream(ev, &mut traced)
                        })?
                    } else {
                        self.llm
                            .stream(&req, &self.cancel, |ev| fwd_stream(ev, &mut traced))?
                    }
                }
            };
            self.acc_usage(&result);
            if trace_enabled {
                if !result.reasoning.is_empty() {
                    trace_record_at(
                        &self.cfg.data_dir(),
                        &session_id,
                        &AgentEvent::Reasoning(result.reasoning.clone()),
                    );
                }
                if !result.text.is_empty() {
                    trace_record_at(
                        &self.cfg.data_dir(),
                        &session_id,
                        &AgentEvent::Delta(result.text.clone()),
                    );
                }
            }
            if !self.cfg.provider.native_tools && result.text.is_empty() {
                let calls = extract_tools_from_reasoning(&result.reasoning);
                if !calls.is_empty() {
                    traced(AgentEvent::Notice(format!(
                        "模型在思考中反复输出工具块（思考流已截断），暴力转发执行 {} 个工具调用",
                        calls.len()
                    )));
                    for call in &calls {
                        let op = call.name.clone();
                        let args_str = call.args.clone();
                        let args: Value = serde_json::from_str(&args_str).unwrap_or(Value::Null);
                        self.store.append(&Msg::new(
                            "assistant",
                            format!("```tool {args_str}```（思考流截断提取，正文为空）"),
                        ));
                        if self.chat_only {
                            self.store.append(&Msg::new("user", NO_PERM_MSG));
                            continue;
                        }
                        traced(AgentEvent::ToolRun { op: op.clone(), args: args_str.clone() });
                        let output = self.run_op(&op, &args);
                        has_tool_result = true;
                        let result_msg = format!(
                            "【工具结果】{op} 返回（从思考流提取）:\n{}",
                            self.prepare_tool_output(&op, &args, &output)
                        );
                        traced(AgentEvent::ToolResult(output.clone()));
                        self.store.append(&message_with_tool_image("user", result_msg, None));
                    }
                    interrupts = 0;                                          
                    continue;
                }
                if !result.reasoning.trim().is_empty() {
                    if self.cfg.fuckloop && has_tool_result {
                        let reason = "工具结果已就绪，但模型思考流空转且没有生成正文";
                        traced(AgentEvent::Notice(
                            "检测到工具结果后的思考流循环，立即切换无思考收尾…".into(),
                        ));
                        final_text = self.finalize_without_tools(reason, &mut traced);
                        self.store.append(&Msg::new("assistant", final_text.clone()));
                        break;
                    }
                    interrupts += 1;
                    if interrupts >= 3 {
                        traced(AgentEvent::Notice(format!("连续打断 {interrupts} 次仍无输出（思考流卡死），重载模型后重试…")));
                        if self.cfg.provider.auto_reload
                            && self.llm.is_llamacpp()
                            && !self.cancel.load(Ordering::Relaxed)
                            && reloads < MAX_RELOADS
                        {
                            reloads += 1;
                            self.reload_wait_cooldown(&mut traced)?;
                            interrupts = 0;                                         
                            continue;
                        }
                        let reason = format!("连续打断 {interrupts} 次仍无正文，模型未能恢复");
                        final_text = self.finalize_without_tools(&reason, &mut traced);
                        self.store.append(&Msg::new("assistant", final_text.clone()));
                        break;
                    }
                    traced(AgentEvent::Notice("模型思考陷入循环（无工具调用），注入打断指令…".into()));
                    self.store.append(&Msg::new(
                        "user",
                        "【系统提示】你的思考流陷入循环已被截断，且没有可用的工具调用。请直接给出最终回答，禁止继续规划、模拟执行或复述命令。",
                    ));
                    continue;
                }
            }
            if self.cfg.provider.native_tools
                && result.text.trim().is_empty()
                && !result.reasoning.trim().is_empty()
                && result.tool_calls.is_empty()
            {
                if self.cfg.fuckloop && has_tool_result {
                    let reason = "工具结果已就绪，但模型思考流空转且没有生成正文";
                    traced(AgentEvent::Notice(
                        "检测到工具结果后的思考流循环，立即切换无思考收尾…".into(),
                    ));
                    final_text = self.finalize_without_tools(reason, &mut traced);
                    self.store.append(&Msg::new("assistant", final_text.clone()));
                    break;
                }
                interrupts += 1;
                if interrupts >= 3 {
                    traced(AgentEvent::Notice(format!("连续打断 {interrupts} 次仍无输出（思考流卡死），重载模型后重试…")));
                    if self.cfg.provider.auto_reload
                        && self.llm.is_llamacpp()
                        && !self.cancel.load(Ordering::Relaxed)
                        && reloads < MAX_RELOADS
                    {
                        reloads += 1;
                        self.reload_wait_cooldown(&mut traced)?;
                        interrupts = 0;                                         
                        continue;
                    }
                    let reason = format!("连续打断 {interrupts} 次仍无正文，模型未能恢复");
                    final_text = self.finalize_without_tools(&reason, &mut traced);
                    self.store.append(&Msg::new("assistant", final_text.clone()));
                    break;
                }
                traced(AgentEvent::Notice("模型思考陷入循环（无工具调用），注入打断指令…".into()));
                self.store.append(&Msg::new(
                    "user",
                    "【系统提示】你的思考流陷入循环已被截断，且没有可用的工具调用。请直接给出最终回答，禁止继续规划、模拟执行或复述命令。",
                ));
                continue;
            }
            if output_looks_broken(&result) {
                if self.cfg.provider.auto_reload
                    && self.llm.is_llamacpp()
                    && !self.cancel.load(Ordering::Relaxed)
                    && reloads < MAX_RELOADS
                {
                    reloads += 1;
                    traced(AgentEvent::Notice("模型输出异常，重载模型后重试…".into()));
                    self.reload_model(&mut traced)?;
                    continue;
                }
                final_text = self.finalize_without_tools("模型连续返回空白或损坏输出", &mut traced);
                self.store.append(&Msg::new("assistant", final_text.clone()));
                break;
            }
            if self.cfg.provider.native_tools && !result.tool_calls.is_empty() {
                interrupts = 0;
                let text = result.text.trim();
                if text.len() >= 10 && (text.ends_with('？') || text.ends_with('?')) {
                    traced(AgentEvent::Notice(
                        "模型正文以问句结尾，视为回合结束（附带工具调用不执行）".into(),
                    ));
                    final_text = result.text.clone();
                    self.store.append(&Msg::new("assistant", final_text.clone()));
                    break;
                }
                let hallucinated: Vec<String> = result
                    .tool_calls
                    .iter()
                    .filter(|c| {
                        let a = c.args.to_lowercase();
                        a.contains("<tool_call") || a.contains("<function=") || a.contains("<parameter=")
                    })
                    .map(|c| c.args.clone())
                    .collect();
                if !hallucinated.is_empty() {
                    traced(AgentEvent::Notice(format!(
                        "检测到模型工具格式幻觉（{} 个调用含 XML 标签），跳过执行",
                        hallucinated.len()
                    )));
                    self.store.append(&Msg::new(
                        "user",
                        "【系统提示】你的工具调用参数格式已损坏（混入了 XML 标签）。禁止继续调用工具，直接用正文回答当前问题。",
                    ));
                    continue;
                }
                let mut guard_reason = None;
                for call in &result.tool_calls {
                    let parsed = crate::tool_compat::parse_args(&call.args);
                    let normalized = crate::tool_compat::normalize_call(&call.name, &parsed);
                    let args = normalized.args;
                    if let Some(reason) = tool_guard.before_call(&normalized.op, &args) {
                        guard_reason = Some(reason);
                        break;
                    }
                }
                if let Some(reason) = guard_reason {
                    final_text = self.finalize_without_tools(&reason, &mut traced);
                    self.store.append(&Msg::new("assistant", final_text.clone()));
                    break;
                }
                let mut asst = Msg::new("assistant", result.text.clone());
                asst.tool_calls = result.tool_calls.clone();
                if !result.reasoning.is_empty() {
                    asst.reasoning_content = Some(result.reasoning.clone());
                }
                self.store.append(&asst);
                let mut guard_reason = None;
                for call in &result.tool_calls {
                    let output = if self.chat_only {
                        NO_PERM_MSG.to_string()
                    } else {
                        let output = self.dispatch(call, &mut traced);
                        has_tool_result = true;
                        output
                    };
                    if guard_reason.is_none() {
                        guard_reason = tool_guard.after_result(&output);
                    }
                    self.store.append(&message_with_tool_image(
                        "tool",
                        output,
                        Some(call.id.clone()),
                    ));
                }
                if let Some(reason) = guard_reason {
                    final_text = self.finalize_without_tools(&reason, &mut traced);
                    self.store.append(&Msg::new("assistant", final_text.clone()));
                    break;
                }
                continue;
            }
            if let Some((op, args)) = parse_atem_calls(&result.text) {
                interrupts = 0;
                let text = result.text.trim();
                if text.len() >= 10 && (text.ends_with('？') || text.ends_with('?')) {
                    traced(AgentEvent::Notice(
                        "模型正文以问句结尾，视为回合结束（附带的 ATEM 工具调用不执行）".into(),
                    ));
                    final_text = result.text.clone();
                    self.store.append(&Msg::new("assistant", final_text.clone()));
                    break;
                }
                if let Some(reason) = tool_guard.before_call(&op, &args) {
                    final_text = self.finalize_without_tools(&reason, &mut traced);
                    self.store.append(&Msg::new("assistant", final_text.clone()));
                    break;
                }
                let clean = strip_atem_block(&result.text);
                self.store.append(&Msg::new("assistant", clean));
                if self.chat_only {
                    self.store.append(&Msg::new("user", NO_PERM_MSG));
                    continue;
                }
                let args_str = serde_json::to_string(&args).unwrap_or_default();
                traced(AgentEvent::ToolRun { op: op.clone(), args: args_str.clone() });
                let output = self.run_op(&op, &args);
                has_tool_result = true;
                let result_msg =
                    format!("【工具结果】{op} 返回:\n{}", self.prepare_tool_output(&op, &args, &output));
                traced(AgentEvent::ToolResult(output.clone()));
                self.store.append(&message_with_tool_image("user", result_msg, None));
                if let Some(reason) = tool_guard.after_result(&output) {
                    final_text = self.finalize_without_tools(&reason, &mut traced);
                    self.store.append(&Msg::new("assistant", final_text.clone()));
                    break;
                }
                continue;
            }
            if let Some((op, args)) = parse_tool_block(&result.text) {
                interrupts = 0;
                let args_str = serde_json::to_string(&args).unwrap_or_default();
                if let Some(reason) = tool_guard.before_call(&op, &args) {
                    final_text = self.finalize_without_tools(&reason, &mut traced);
                    self.store.append(&Msg::new("assistant", final_text.clone()));
                    break;
                }
                self.store.append(&Msg::new("assistant", result.text.clone()));
                if self.chat_only {
                    self.store.append(&Msg::new("user", NO_PERM_MSG));
                    continue;
                }
                traced(AgentEvent::ToolRun { op: op.clone(), args: args_str.clone() });
                let output = self.run_op(&op, &args);
                has_tool_result = true;
                let result_msg =
                    format!("【工具结果】{op} 返回:\n{}", self.prepare_tool_output(&op, &args, &output));
                traced(AgentEvent::ToolResult(output.clone()));
                self.store.append(&message_with_tool_image("user", result_msg, None));
                if let Some(reason) = tool_guard.after_result(&output) {
                    final_text = self.finalize_without_tools(&reason, &mut traced);
                    self.store.append(&Msg::new("assistant", final_text.clone()));
                    break;
                }
                continue;
            }
            if let Some(op) = bare_tool_op(&result.text) {
                interrupts = 0;
                let args = serde_json::json!({});
                if let Some(reason) = tool_guard.before_call(op, &args) {
                    final_text = self.finalize_without_tools(&reason, &mut traced);
                    self.store.append(&Msg::new("assistant", final_text.clone()));
                    break;
                }
                self.store.append(&Msg::new("assistant", result.text.clone()));
                if self.chat_only {
                    self.store.append(&Msg::new("user", NO_PERM_MSG));
                    continue;
                }
                traced(AgentEvent::ToolRun { op: op.into(), args: "{}".into() });
                let output = self.run_op(op, &args);
                has_tool_result = true;
                let result_msg =
                    format!("【工具结果】{op} 返回:\n{}", self.prepare_tool_output(op, &args, &output));
                traced(AgentEvent::ToolResult(output.clone()));
                self.store.append(&message_with_tool_image("user", result_msg, None));
                if let Some(reason) = tool_guard.after_result(&output) {
                    final_text = self.finalize_without_tools(&reason, &mut traced);
                    self.store.append(&Msg::new("assistant", final_text.clone()));
                    break;
                }
                continue;
            }
            final_text = result.text.clone();
            self.store.append(&Msg::new("assistant", final_text.clone()));
            break;
        }
        if trace_enabled {
            trace_record_at(
                &self.cfg.data_dir(),
                &session_id,
                &AgentEvent::Done(final_text.clone()),
            );
        }
        Ok(final_text)
    }
    fn acc_usage(&mut self, result: &crate::llm::ChatResult) {
        if let Some(usage) = result.usage {
            self.last_usage.prompt_tokens += usage.prompt_tokens;
            self.last_usage.completion_tokens += usage.completion_tokens;
            self.last_usage.cache_read_tokens += usage.cache_read_tokens;
            self.last_usage.cache_miss_tokens += usage.cache_miss_tokens;
            self.last_usage.reasoning_tokens += usage.reasoning_tokens;
            self.last_usage.cache_prefix_hits += usage.cache_prefix_hits;
            self.last_usage.cache_prefix_misses += usage.cache_prefix_misses;
            self.last_usage.cache_prefix_messages = self
                .last_usage
                .cache_prefix_messages
                .max(usage.cache_prefix_messages);
        }
        if let Some(timings) = result.timings {
            self.last_timings.predicted_n += timings.predicted_n;
            self.last_timings.predicted_ms += timings.predicted_ms;
        }
    }
    fn finalize_without_tools(
        &mut self,
        reason: &str,
        on_event: &mut impl FnMut(AgentEvent),
    ) -> String {
        on_event(AgentEvent::Notice(format!("{reason}，正在整理已有结果…")));
        let messages = focused_finalize_messages(&self.store.current().messages, reason);
        let req = ChatRequest {
            messages,
            tools: None,
            max_tokens: self.llm.max_tokens_for(
                if self.chat_mode { TokenBudget::QqFinalize } else { TokenBudget::TuiFinalize },
                self.cfg.qq.max_tokens,
            ),
            stream: false,
        };
        let fallback = format!(
            "工具阶段已停止：{reason}。本地模型没有生成可用的最终答复，请缩小任务范围或补充信息后重试。"
        );
        let attempt = self.llm.stream_without_reasoning(&req, &self.cancel, |_| {});
        if let Ok(result) = &attempt {
            self.acc_usage(result);
        }
        match attempt {
            Ok(result)
                if result.tool_calls.is_empty()
                    && !result.text.trim().is_empty()
                    && parse_tool_block(&result.text).is_none()
                    && parse_atem_calls(&result.text).is_none()
                    && bare_tool_op(&result.text).is_none() =>
            {
                let text = result.text.trim().to_string();
                on_event(AgentEvent::Delta(text.clone()));
                text
            }
            _ => {
                on_event(AgentEvent::Delta(fallback.clone()));
                fallback
            }
        }
    }
    fn dispatch(&mut self, call: &ToolCall, on_event: &mut impl FnMut(AgentEvent)) -> String {
        on_event(AgentEvent::ToolRun { op: call.name.clone(), args: call.args.clone() });
        let op = call.name.clone();
        let args = crate::tool_compat::parse_args(&call.args);
        let output = self.run_op(&op, &args);
        on_event(AgentEvent::ToolResult(output.clone()));
        self.prepare_tool_output(&op, &args, &output)
    }
    fn reload_wait_cooldown(&self, on_event: &mut impl FnMut(AgentEvent)) -> Result<(), String> {
        let mut rounds = 0;
        loop {
            match self.llm.reload_model() {
                Ok(()) => {
                    on_event(AgentEvent::Notice("模型已重载".into()));
                    return Ok(());
                }
                Err(e) if e.contains("冷却") && rounds < 2 => {
                    rounds += 1;
                    if rounds == 1 {
                        on_event(AgentEvent::Notice("模型重载被 60s 冷却拦截，等待冷却结束重试…".into()));
                    }
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(65);
                    while std::time::Instant::now() < deadline
                        && !self.cancel.load(Ordering::Relaxed)
                    {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                    if self.cancel.load(Ordering::Relaxed) {
                        return Ok(());                          
                    }
                }
                Err(e) => {
                    let msg = format!("模型重载失败: {e}（服务器可能不支持动态重载，可重启模型服务恢复）");
                    on_event(AgentEvent::Error(msg.clone()));
                    return Err(msg);
                }
            }
        }
    }
    fn reload_model(&self, on_event: &mut impl FnMut(AgentEvent)) -> Result<(), String> {
        match self.llm.reload_model() {
            Ok(()) => {
                on_event(AgentEvent::Notice("模型已重载".into()));
                Ok(())
            }
            Err(e) => {
                let msg = format!("模型重载失败: {e}（服务器可能不支持动态重载，可重启模型服务恢复）");
                on_event(AgentEvent::Error(msg.clone()));
                Err(msg)
            }
        }
    }
    fn run_op(&mut self, op: &str, args: &Value) -> String {
        let ask = match (&self.ask_tx, &self.answer_rx) {
            (Some(tx), Some(rx)) => Some(crate::tools::AskHandle {
                tx,
                rx,
                cancel: &self.cancel,
                seq: &self.ask_seq,
            }),
            _ => None,
        };
        let perm = match (&self.perm_tx, &self.perm_rx) {
            (Some(tx), Some(rx)) => Some(crate::tools::PermHandle {
                tx,
                rx,
                cancel: &self.cancel,
                seq: &self.perm_seq,
                auto: &self.perm_auto,
                allowed: &self.perm_allowed,
            }),
            _ => None,
        };
        let mut ctx = ToolCtx {
            cfg: &self.cfg,
            store: &mut self.store,
            llm: &self.llm,
            qq_tx: self.qq_tx.as_ref(),
            cancel: &self.cancel,
            mem_dir: None,
            ask,
            perm,
            event_tx: self.tool_event_tx.as_ref(),
        };
        match execute(op, args, &mut ctx) {
            Ok(out) => out,
            Err(e) => format!("错误: {e}"),
        }
    }
    fn prepare_tool_output(&self, op: &str, args: &Value, output: &str) -> String {
        let semantic_op = crate::tool_compat::normalize_call(op, args).op;
        if matches!(semantic_op.as_str(), "computer_use" | "qq_bot") {
            // The output is small, and its private marker connects the frame
            // to the next multimodal request. Generic preview storage would
            // separate or truncate that marker and make the screenshot blind.
            return output.to_string();
        }
        crate::tool_output::store_or_preview_for_tool(
            &self.cfg.data_dir(),
            self.store.current_id(),
            &semantic_op,
            output,
            self.cfg.tui.tool_result_max_tokens,
        )
    }
}

fn message_with_tool_image(role: &str, content: String, tool_call_id: Option<String>) -> Msg {
    let (content, image_path) = crate::computer_use::split_image_marker(&content);
    Msg {
        role: role.into(),
        content,
        image_path,
        tool_call_id,
        ..Default::default()
    }
}
fn output_looks_broken(result: &crate::llm::ChatResult) -> bool {
    if !result.tool_calls.is_empty() {
        return false;
    }
    let t = result.text.trim();
    t.is_empty() || t.contains("<unused")
}
fn find_tool_fence(text: &str, from: usize) -> Option<usize> {
    for (off, _) in text[from..].match_indices("```") {
        let abs = from + off;
        let after: Vec<char> = text[abs + 3..].chars().take(4).collect();
        if after.len() >= 4
            && after[0].eq_ignore_ascii_case(&'t')
            && after[1].eq_ignore_ascii_case(&'o')
            && after[2].eq_ignore_ascii_case(&'o')
            && after[3].eq_ignore_ascii_case(&'l')
        {
            return Some(abs);
        }
    }
    None
}
fn extract_tools_from_reasoning(text: &str) -> Vec<ToolCall> {
    const MAX_BLOCKS: usize = 8;
    let mut out = Vec::new();
    let mut search_from = 0;
    while let Some(fence_start) = find_tool_fence(text, search_from) {
        let content_start = fence_start + 7;             
        let content_end = text[content_start..].find("```").map(|i| content_start + i);
        let candidate = match content_end {
            Some(end) => &text[content_start..end],
            None => &text[content_start..],
        };
        if let Some((op, args)) = try_parse_op_json(candidate, true) {
            out.push(ToolCall {
                id: String::new(),
                name: op,
                args: serde_json::to_string(&args).unwrap_or_default(),
            });
            if out.len() >= MAX_BLOCKS {
                break;
            }
        }
        search_from = advance_char_boundary(text, content_start);
    }
    out
}
fn advance_char_boundary(text: &str, from: usize) -> usize {
    match text[from..].chars().next() {
        Some(c) => from + c.len_utf8(),
        None => from,
    }
}
pub fn bare_tool_op(text: &str) -> Option<&'static str> {
    let t = text.trim();
    crate::tools::KNOWN_OPS.iter().find(|op| **op == t).copied()
}
pub fn parse_tool_block(text: &str) -> Option<(String, Value)> {
    let mut search_from = 0;
    while let Some(fence_start) = find_tool_fence(text, search_from) {
        let content_start = fence_start + 7;             
        let content_end = text[content_start..].find("```").map(|i| content_start + i);
        let candidate = match content_end {
            Some(end) => &text[content_start..end],
            None => &text[content_start..],
        };
        if let Some(v) = try_parse_op_json(candidate, true) {
            return Some(v);
        }
        search_from = advance_char_boundary(text, content_start);
    }
    let mut search_from = 0;
    while let Some((rel, _)) = text[search_from..].match_indices("```").next() {
        let fence_start = search_from + rel;
        let after_fence = &text[fence_start + 3..];
        for op in crate::tools::KNOWN_OPS {
            if let Some(_rest) = after_fence.strip_prefix(op) {
                let content_start = fence_start + 3 + op.len();
                let content_end = text[content_start..].find("```").map(|i| content_start + i);
                let candidate = match content_end {
                    Some(end) => &text[content_start..end],
                    None => &text[content_start..],
                };
                if let Some(v) = first_json_object(candidate) {
                    return Some((op.to_string(), v));
                }
            }
        }
        search_from = fence_start + 3;
    }
    for line in text.lines() {
        let l = line.trim_start();
        let rest = l
            .strip_prefix("tool ")
            .or_else(|| l.strip_prefix("tool{"))
            .or_else(|| l.strip_prefix("tool:{").map(|r| &r[1..]));
        if let Some(rest) = rest {
            if let Some(v) = try_parse_op_json(rest, false) {
                return Some(v);
            }
        }
    }
    for line in text.lines() {
        let l = line.trim_start();
        for op in crate::tools::KNOWN_OPS {
            if let Some(rest) = l.strip_prefix(op) {
                if rest.trim_start().starts_with('{') {
                    if let Some(v) = first_json_object(rest) {
                        return Some((op.to_string(), v));
                    }
                }
                break;
            }
        }
    }
    if let Some(v) = try_parse_op_json(text, false) {
        return Some(v);
    }
    None
}
fn parse_atem_calls(text: &str) -> Option<(String, Value)> {
    let calls = text.find("<atem:function_calls>")?;
    let tail = &text[calls..];
    let invoke = tail.find("<atem:invoke name=\"")?;
    let rest = &tail[invoke + "<atem:invoke name=\"".len()..];
    let name_end = rest.find('"')?;
    let name = rest[..name_end].trim().to_string();
    if name.is_empty() {
        return None;
    }
    let body = &rest[name_end..];
    let mut params: Vec<(String, Value)> = Vec::new();
    let mut idx = 0;
    while let Some(pstart) = body[idx..].find("<atem:parameter name=\"") {
        let ps = idx + pstart + "<atem:parameter name=\"".len();
        let Some(pend) = body[ps..].find('"') else { break };
        let key = body[ps..ps + pend].to_string();
        let vs = ps + pend;
        let Some(vtag) = body[vs..].find('>') else { break };
        let content_start = vs + vtag + 1;
        let Some(ctag) = body[content_start..].find("</atem:parameter>") else { break };
        let raw = &body[content_start..content_start + ctag];
        let val = serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_string()));
        params.push((key, val));
        idx = content_start + ctag + "</atem:parameter>".len();
    }
    if params.is_empty() {
        return None;
    }
    let op = params
        .iter()
        .find(|(k, _)| k == "op")
        .and_then(|(_, v)| v.as_str())
        .unwrap_or(&name)
        .to_string();
    let args = if let Some((_, v)) = params.iter().find(|(k, _)| k == "args") {
        v.clone()
    } else {
        let mut m = serde_json::Map::new();
        for (k, v) in params {
            m.insert(k, v);
        }
        Value::Object(m)
    };
    Some((op, args))
}
fn strip_atem_block(text: &str) -> String {
    match (text.find("<atem:function_calls>"), text.rfind("</atem:function_calls>")) {
        (Some(start), Some(end)) if end > start => {
            let block_end = end + "</atem:function_calls>".len();
            format!("{}{}", &text[..start], &text[block_end..])
        }
        _ => text.to_string(),
    }
}
fn first_json_object(s: &str) -> Option<Value> {
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in s[start..].char_indices() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let json = &s[start..start + i + c.len_utf8()];
                    if let Ok(v) = serde_json::from_str::<Value>(json) {
                        return Some(v);
                    }
                    let repaired = repair_json_escapes(json);
                    if repaired != json {
                        return serde_json::from_str::<Value>(&repaired).ok();
                    }
                    return None;
                }
            }
            _ => {}
        }
    }
    None
}
fn repair_json_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(n @ ('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u')) => {
                out.push(c);
                out.push(n);
            }
            Some(n) => out.push(n),
            None => out.push(c),
        }
    }
    out
}
fn parse_op_json(json: &str, strict: bool) -> Option<(String, Value)> {
    let v = serde_json::from_str::<Value>(json).ok()?;
    let op = v.get("op").and_then(|o| o.as_str()).map(String::from)?;
    if strict || crate::tool_compat::is_supported_name(&op) {
        let inner = v.get("args").cloned().unwrap_or(v);
        return Some((op, inner));
    }
    None
}
fn try_parse_op_json(s: &str, strict: bool) -> Option<(String, Value)> {
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in s[start..].char_indices() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;                                  
        }
        match c {
            '"' => in_str = true,                          
            '{' => depth += 1,            
            '}' => {
                depth -= 1;                      
                if depth == 0 {
                    let json = &s[start..start + i + c.len_utf8()];
                    if let Some(v) = parse_op_json(json, strict) {
                        return Some(v);
                    }
                    let repaired = repair_json_escapes(json);
                    if repaired != json {
                        if let Some(v) = parse_op_json(&repaired, strict) {
                            return Some(v);
                        }
                    }
                    return None;
                }
            }
            _ => {}                             
        }
    }
    if depth > 0 {
        let tail = &s[start..];
        if !tail.contains('\n') && tail.starts_with("{\"op\":\"") {
            let mut fixed = tail.to_string();
            for _ in 0..depth {
                fixed.push('}');
            }
            if let Some(v) = parse_op_json(&fixed, strict) {
                return Some(v);
            }
            let repaired = repair_json_escapes(&fixed);
            if repaired != fixed {
                if let Some(v) = parse_op_json(&repaired, strict) {
                    return Some(v);
                }
            }
        }
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_fenced_tool_block() {
        let t = "先看看工具\n```tool {\"op\":\"list_tools\",\"args\":{}}\n```\n完了";
        let (op, args) = parse_tool_block(t).unwrap();
        assert_eq!(op, "list_tools");
        assert_eq!(args, serde_json::json!({}));
    }
    #[test]
    fn parse_json_fence_fallback() {
        let t = "```json\n{\"op\": \"list_tools\", \"args\": {}}\n```";
        let (op, _) = parse_tool_block(t).unwrap();
        assert_eq!(op, "list_tools");
    }
    #[test]
    fn parse_fenced_known_op_variant() {
        let t = "```qq_send {\"chat\":\"group:728563593\",\"text\":\"在呀！老大有啥事吗？\"}```";
        let (op, args) = parse_tool_block(t).unwrap();
        assert_eq!(op, "qq_send");
        assert_eq!(args["chat"], "group:728563593");
        assert_eq!(args["text"], "在呀！老大有啥事吗？");
        let t2 = "```python\nprint(1)\n```\n```web_search {\"query\":\"x\"}```";
        let (op2, args2) = parse_tool_block(t2).unwrap();
        assert_eq!(op2, "web_search");
        assert_eq!(args2["query"], "x");
        assert!(parse_tool_block("```rust\nlet x = 1;\n```").is_none());
    }
    #[test]
    fn parse_bare_json_with_registered_op() {
        let t = "我决定调用 {\"op\":\"readline\",\"args\":{\"path\":\"a\"}} 来看文件";
        let (op, args) = parse_tool_block(t).unwrap();
        assert_eq!(op, "readline");
        assert_eq!(args["path"], "a");
    }
    #[test]
    fn parse_unfenced_tool_prefix_variant() {
        let t = "tool {\"op\":\"is_admin\",\"args\":{\"qq\":3160168215}}";
        let (op, args) = parse_tool_block(t).unwrap();
        assert_eq!(op, "is_admin");
        assert_eq!(args["qq"].as_i64(), Some(3160168215i64));
        assert_eq!(parse_tool_block("tool{\"op\":\"stats\",\"args\":{}}").unwrap().0, "stats");
        assert_eq!(parse_tool_block("tool:{\"op\":\"stats\",\"args\":{}}").unwrap().0, "stats");
        assert!(parse_tool_block("tool {\"op\":\"nothing_here\",\"args\":{}}").is_none());
    }
    #[test]
    fn parse_bare_tool_name_with_json_args() {
        let t = "is_admin {\"qq\":\"3160168215\"}";
        let (op, args) = parse_tool_block(t).unwrap();
        assert_eq!(op, "is_admin");
        assert_eq!(args["qq"], "3160168215");
        let (op, args) = parse_tool_block("list_tools {\"category\":\"file\"}").unwrap();
        assert_eq!(op, "list_tools");
        assert_eq!(args["category"], "file");
        let (op, _) = parse_tool_block("is_admin {\"qq\":1}\n你的权限是...").unwrap();
        assert_eq!(op, "is_admin");
        assert!(parse_tool_block("is_admin 工具可以判断管理员").is_none());
        assert!(parse_tool_block("qq_send 是用来发消息的工具").is_none());
        assert!(parse_tool_block("nothing_here {\"a\":1}").is_none());
    }
    #[test]
    fn parse_json_with_invalid_escapes() {
        let t = "qq_send {\"chat\":\"group:728563593\",\"text\":\"直接说哈～(´▽\\`)\"}";
        let (op, args) = parse_tool_block(t).unwrap();
        assert_eq!(op, "qq_send");
        assert_eq!(args["text"], "直接说哈～(´▽`)");
        let t2 = "{\"op\":\"qq_send\",\"args\":{\"chat\":\"group:1\",\"text\":\"it\\'s fine\"}}";
        let (_, args2) = parse_tool_block(t2).unwrap();
        assert_eq!(args2["text"], "it's fine");
        assert_eq!(repair_json_escapes(r#"a\nb\"c\\d\u4f60"#), r#"a\nb\"c\\d\u4f60"#);
        assert_eq!(repair_json_escapes(r#"a\`b\'c\d"#), "a`b'cd");
        assert_eq!(repair_json_escapes("行尾\\"), "行尾\\");
    }
    #[test]
    fn parse_truncated_json_repair() {
        let t = "{\"op\":\"qq_send\",\"args\":{\"chat\":\"group:728563593\",\"text\":\"哎呀管理员您也太规律了吧～ 太阳从西边出来啦？😄\"}";
        let (op, args) = parse_tool_block(t).unwrap();
        assert_eq!(op, "qq_send");
        assert_eq!(args["chat"], "group:728563593");
        assert_eq!(args["text"], "哎呀管理员您也太规律了吧～ 太阳从西边出来啦？😄");
        let t2 = "{\"op\":\"stats\",\"args\":{\"category\":\"file\"";
        let (op2, _) = parse_tool_block(t2).unwrap();
        assert_eq!(op2, "stats");
        let (op3, _) = parse_tool_block("先分析一下\n{\"op\":\"stats\",\"args\":{\"category\":\"file\"").unwrap();
        assert_eq!(op3, "stats");
        assert!(parse_tool_block("{\"op\":\"nothing_here\",\"args\":{\"a\":1}").is_none());
    }
    #[test]
    fn ignores_unregistered_op_in_prose() {
        let t = "json 结构形如 {\"op\":\"something_unknown\",\"args\":{}}";
        assert!(parse_tool_block(t).is_none());
    }
    #[test]
    fn no_tool_block() {
        assert!(parse_tool_block("直接回答，没有任何工具").is_none());
    }
    #[test]
    fn bare_tool_op_matches_registered_only() {
        assert_eq!(bare_tool_op("list_tools"), Some("list_tools"));
        assert_eq!(bare_tool_op("  list_tools  "), Some("list_tools"));
        assert_eq!(bare_tool_op("web_search"), Some("web_search"));
        assert_eq!(bare_tool_op("list_tools 查看工具"), None);
        assert_eq!(bare_tool_op("something_unknown"), None);
        assert_eq!(bare_tool_op("你好"), None);
        assert_eq!(bare_tool_op("```tool {\"op\":\"list_tools\",\"args\":{}}\n```"), None);
    }
    #[test]
    fn extract_tools_from_reasoning_all_complete_blocks_in_order() {
        let t = String::from("先计划用 web 搜索\n```tool {\"op\":\"web_search\",\"args\":{\"query\":\"x\"}}\n```\n")
            + "```tool {\"op\":\"execute_command\",\"args\":{\"cmd\":\"ls ~/.config/noctalia/\"}}\n```\n"
            + "Executing.\n\nWait, I'll execute `ls ~/.config/noctalia/`.\n";
        let calls = extract_tools_from_reasoning(&t);
        assert_eq!(calls.len(), 2, "应提取全部完整块");
        assert_eq!(calls[0].name, "web_search", "按出现顺序");
        assert_eq!(calls[1].name, "execute_command");
        assert!(calls[1].args.contains("noctalia"));
    }
    #[test]
    fn extract_tools_from_reasoning_ignores_unclosed_or_unregistered() {
        let unclosed = "```tool {\"op\":\"execute_command\",\"args\":{\"cmd\":\"ls ~/.config/noctalia/\"}\nExecuting.\n\nWait, I'll execute `ls`.\n";
        assert!(extract_tools_from_reasoning(unclosed).is_empty());
        let unknown = "```tool {\"op\":\"nothing_here\",\"args\":{}}\n```";
        assert_eq!(extract_tools_from_reasoning(unknown).len(), 1);
        assert!(extract_tools_from_reasoning("正常思考，无需工具").is_empty());
    }
    #[test]
    fn extract_tools_from_reasoning_caps_at_max() {
        let mut t = String::new();
        for i in 0..12 {
            t.push_str(&format!(
                "方案{}: ```tool {{\"op\":\"execute_command\",\"args\":{{\"cmd\":\"echo plan-{i}\"}}}}\n```\n",
                i + 1
            ));
        }
        let calls = extract_tools_from_reasoning(&t);
        assert_eq!(calls.len(), 8, "上限 8 个");
        assert_eq!(calls[0].args, "{\"cmd\":\"echo plan-0\"}");
        assert_eq!(calls[7].args, "{\"cmd\":\"echo plan-7\"}");
    }
    #[test]
    fn tool_fence_scan_survives_multibyte_after_fence() {
        let t = "```tool好的，让我先想想\n```\n```tool {\"op\":\"execute_command\",\"args\":{\"cmd\":\"ls\"}}\n```\n";
        let calls = extract_tools_from_reasoning(t);
        assert_eq!(calls.len(), 1, "应跳过垃圾块找到后面的合法块");
        assert_eq!(calls[0].name, "execute_command");
        let (op, args) = parse_tool_block(t).unwrap();
        assert_eq!(op, "execute_command");
        assert_eq!(args["cmd"], "ls");
        let t2 = "让我执行 ```tool";
        assert!(extract_tools_from_reasoning(t2).is_empty());
        assert!(parse_tool_block(t2).is_none());
    }
    #[test]
    fn tool_fence_scan_handles_lowercase_byte_mismatch() {
        let t = "İſ 分析配置\n```tool {\"op\":\"execute_command\",\"args\":{\"cmd\":\"ls\"}}\n```\n";
        let calls = extract_tools_from_reasoning(t);
        assert_eq!(calls[0].name, "execute_command");
        let (op, _) = parse_tool_block(t).unwrap();
        assert_eq!(op, "execute_command");
        let t2 = "先看下\n```TOOL {\"op\":\"list_tools\",\"args\":{}}\n```\n";
        let calls2 = extract_tools_from_reasoning(t2);
        assert_eq!(calls2[0].name, "list_tools");
        assert_eq!(parse_tool_block(t2).unwrap().0, "list_tools");
    }
    #[test]
    fn parse_nested_braces_in_args() {
        let t = "```tool {\"op\":\"writefile\",\"args\":{\"path\":\"a.json\",\"content\":\"{\\\"k\\\":\\\"v\\\"}\"}}\n```";
        let (op, args) = parse_tool_block(t).unwrap();
        assert_eq!(op, "writefile");
        assert!(args["content"].as_str().unwrap().contains('{'));
    }
    #[test]
    fn keyword_interception_uses_tools_mapping() {
        assert_eq!(
            crate::tools::keyword_tool("搜索马斯克"),
            Some(("web_search", serde_json::json!({"query": "马斯克"})))
        );
        assert_eq!(crate::tools::keyword_tool("搜索功能怎么做"), None);
        assert_eq!(
            crate::tools::keyword_tool("抓取 https://example.com"),
            Some(("web_fetch", serde_json::json!({"url": "https://example.com"})))
        );
    }
    #[test]
    fn output_looks_broken_detection() {
        use crate::llm::ChatResult;
        assert!(output_looks_broken(&ChatResult {
            text: "<unused49>".repeat(50),
            ..Default::default()
        }));
        assert!(output_looks_broken(&ChatResult::default()));
        assert!(!output_looks_broken(&ChatResult {
            text: "你好，我是 YJLcoder。".into(),
            ..Default::default()
        }));
        assert!(!output_looks_broken(&ChatResult {
            text: String::new(),
            tool_calls: vec![crate::llm::ToolCall {
                id: "c1".into(),
                name: "list_tools".into(),
                args: "{}".into(),
            }],
            ..Default::default()
        }));
    }
    #[test]
    fn parse_atem_calls_handles_mixed_and_pure() {
        let mixed = "2 + 3 = 5\n\n现在查看目录：\n<|eom|><|start|>assistant to=listdir<|message|><atem:function_calls>\n<atem:invoke name=\"listdir\">\n<atem:parameter name=\"path\">/workspace/yjlcoder/src</atem:parameter>\n</atem:invoke>\n</atem:function_calls>";
        let (op, args) = parse_atem_calls(mixed).expect("应解析出 atem 工具");
        assert_eq!(op, "listdir");
        assert_eq!(args.get("path").and_then(|v| v.as_str()), Some("/workspace/yjlcoder/src"));
        let clean = strip_atem_block(mixed);
        assert!(!clean.contains("<atem:"), "剥离后不应含 atem 标签: {clean}");
        assert!(clean.contains("2 + 3 = 5"));
        let wrapped = "<atem:function_calls><atem:invoke name=\"execute_command\"><atem:parameter name=\"op\">listdir</atem:parameter><atem:parameter name=\"args\">{\"path\": \"/tmp\"}</atem:parameter></atem:invoke></atem:function_calls>";
        let (op2, args2) = parse_atem_calls(wrapped).unwrap();
        assert_eq!(op2, "listdir");
        assert_eq!(args2.get("path").and_then(|v| v.as_str()), Some("/tmp"));
        assert!(parse_atem_calls("普通文本回答").is_none());
    }
    #[test]
    fn tool_loop_guard_stops_third_identical_call() {
        let mut guard = ToolLoopGuard::default();
        let args = serde_json::json!({"op":"list_tools","args":{"category":"net"}});
        assert!(guard.before_call("execute_command", &args).is_none());
        assert!(guard.before_call("execute_command", &args).is_none());
        let reason = guard.before_call("execute_command", &args).unwrap();
        assert!(reason.contains("重复"));
    }
    #[test]
    fn tool_loop_guard_allows_progress_and_stops_failures() {
        let mut guard = ToolLoopGuard::default();
        for i in 0..4 {
            let args = serde_json::json!({"cmd": format!("missing-command-{i}")});
            assert!(guard.before_call("execute_command", &args).is_none());
            let output = format!("exit code: 1\nnot found {i}");
            let reason = guard.after_result(&output);
            if i < 3 {
                assert!(reason.is_none());
            } else {
                assert!(reason.unwrap().contains("失败"));
            }
        }
    }
    #[test]
    fn chat_only_blocks_all_tools() {
        let d = std::env::temp_dir().join(format!("yjlcoder_test_chat_only_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mut cfg = Config::default();
        cfg.set_test_data_dir(d.clone());
        cfg.provider.native_tools = false;                    
        let store = SessionStore::new(d.clone());
        let mut agent = Agent::with_store(cfg, Llm::mock(), store, None, true, true, Arc::new(AtomicBool::new(false)));
        let mut events: Vec<String> = Vec::new();
        let final_text = agent.run_turn("搜索 马斯克", &mut |ev| match ev {
            AgentEvent::ToolRun { op, .. } => events.push(format!("toolrun:{op}")),
            AgentEvent::ToolResult(_) => events.push("toolresult".into()),
            _ => {}
        }).unwrap();
        assert!(final_text.contains("mock 模式完成"), "final: {final_text}");
        assert!(events.is_empty(), "chat_only 不应产生工具事件: {events:?}");
        let msgs = agent.store.current().messages;
        assert!(!msgs.iter().any(|m| m.content.contains("【工具结果】")), "不得执行工具");
        assert!(msgs.iter().any(|m| m.content.contains("无权限")), "应回灌无权限说明");
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn safe_shell_file_read_reaches_model_without_a_truncated_preview() {
        let d = std::env::temp_dir().join(format!(
            "yjlcoder_test_complete_shell_read_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("noctalia-config.toml");
        let body = (1..=257)
            .map(|line| format!("setting_{line} = \"value_{line}\""))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, body).unwrap();
        let mut cfg = Config::default();
        cfg.set_test_data_dir(d.clone());
        cfg.tui.tool_result_max_tokens = 8;
        let store = SessionStore::new(d.clone());
        let mut agent = Agent::with_store(
            cfg,
            Llm::mock(),
            store,
            None,
            false,
            true,
            Arc::new(AtomicBool::new(false)),
        );
        let args = serde_json::json!({
            "cmd": format!("sed -n '1,260p' {}", path.display())
        });
        let output = agent.run_op("execute_command", &args);
        let model_visible = agent.prepare_tool_output("execute_command", &args, &output);
        assert_eq!(model_visible, output, "readline 语义不得进入通用预览折叠");
        assert!(model_visible.contains("1\tsetting_1"));
        assert!(model_visible.contains("129\tsetting_129"));
        assert!(model_visible.contains("257\tsetting_257"));
        assert!(!model_visible.contains("truncated output"));
        assert!(!model_visible.contains("完整输出已保存"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
