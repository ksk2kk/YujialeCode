//! YJLcoder bridge — 把 yjlcoder 的 [`yjlcoder::agent::Agent`] 挂到 Grok Build
//! TUI 后面。
//!
//! 实现 [`acp::Agent`]，通过 xai-acp-lib 的内存 ACP 通道与 TUI 通信：
//! - `session/prompt` → 阻塞线程上跑 `Agent::run_turn`，事件流映射为
//!   `session/update`（AgentMessageChunk / AgentThoughtChunk / ToolCall /
//!   ToolCallUpdate）
//! - `session/request_permission` ← 我们的 shell 审批（PermRequest）
//! - `x.ai/ask_user_question` ← 我们的 ask_user_question 工具（负载与 grok
//!   完全同形；取消是成功结果）
//! - `session/new` / `session/load` → `SessionStore`（~/.yjlcoder 状态）
//!
//! 由 `xai-grok-pager::acp::spawn_grok_shell` 的环境变量接缝（`YJL_TUI=1`）
//! 启用；未设置时 grok 自带的 MvpAgent 路径逐字不变。

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

use agent_client_protocol as acp;
use acp::Client as _;
use anyhow::Result;
use tokio_util::sync::CancellationToken;
use xai_acp_lib::{AcpGatewayReceiver, AcpGatewaySender, acp_channels};
use yjlcoder::agent::AgentEvent;
use yjlcoder::config::Config as YjlConfig;
use yjlcoder::llm::Llm;
use yjlcoder::session::SessionStore;
mod plugins;

use yjlcoder::tools::{
    ask_outcome_from_json, AskAnswer, AskOutcome, AskRequest, PermDecision, PermDecisionKind,
    PermRequest,
};

/// 暴露给 TUI 的 agent 标识。
pub const AGENT_NAME: &str = "yjlcoder";
/// 非交互式认证方法：TUI 据此跳过登录页（照 grok 的 AuthMethodKind 约定）。
pub const AUTH_METHOD_ID: &str = "xai.api_key";
/// grok 的 ask_user_question 扩展方法名。
pub const ASK_USER_EXT_METHOD: &str = "x.ai/ask_user_question";

/// 与 yjlcoder 主程序 COMMANDS 对齐的斜杠命令种子。
const COMMANDS: &[(&str, &str)] = &[
    ("help", "查看命令帮助"),
    ("setup", "回到供应商配置页"),
    ("server", "查看/设置模型服务地址"),
    ("model", "查看/切换模型"),
    ("apikey", "查看/设置 API 密钥"),
    ("ctx", "查看/覆盖上下文窗口"),
    ("cfg", "配置总览（/cfg show）"),
    ("agents", "管理受限后台 Agent"),
    ("goal", "持续执行目标（状态/暂停/恢复/清除）"),
    ("FuckMaster", "定时主动通过 QQ 推进目标"),
    ("new", "新建会话"),
    ("ls", "列出会话"),
    ("use", "切换会话"),
    ("rm", "删除会话"),
    ("compress", "手动压缩上下文"),
    ("stats", "上下文统计"),
    ("autodangerous", "命令自动放行开关"),
    ("save", "导出当前会话"),
    ("models", "列出/切换服务端模型"),
    ("skills", "已安装技能"),
    ("clear", "清屏"),
    ("exit", "退出"),
];

/// 桥 spawn 的产物；接缝处包成 pager 的 `SpawnedAgent`。
pub struct BridgedAgent {
    pub thread_handle: std::thread::JoinHandle<Result<()>>,
    pub channel: xai_acp_lib::AcpClientChannel,
    pub cancel: CancellationToken,
}

/// `session/prompt` 期间桥维护的会话状态（即我们 Agent 的全部依赖）。
struct SessionState {
    id: String,
    cfg: YjlConfig,
    llm: Llm,
    store: SessionStore,
    cancel: Arc<AtomicBool>,
    /// 会话级 shell 自动放行（/autodangerous），跨回合生效。
    perm_auto: Arc<AtomicBool>,
    perm_allowed: Arc<Mutex<std::collections::HashSet<String>>>,
    /// 取消通知：session/cancel 到来时唤醒回合泵，做限时兜底退出。
    cancel_notify: Arc<tokio::sync::Notify>,
}

pub struct YjlAgent {
    client: AcpGatewaySender<acp::AgentSide>,
    session: Mutex<Option<SessionState>>,
}

/// 启动桥接 agent worker 线程（结构与 pager 的 `spawn_agent_thread_direct`
/// 一致：current-thread tokio + LocalSet + 网关直派发）。
pub async fn spawn(cancel: &CancellationToken) -> Result<BridgedAgent> {
    let agent_cancel = cancel.child_token();
    let (acp_client, acp_agent) = acp_channels();
    let worker_cancel = agent_cancel.clone();
    let handle = std::thread::Builder::new()
        .name("yjl-acp-agent".into())
        .spawn(move || -> Result<()> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                let client = AcpGatewaySender::new(acp_agent.tx.clone());
                let agent = Rc::new(YjlAgent {
                    client,
                    session: Mutex::new(None),
                });
                let gw_rx = AcpGatewayReceiver::new(acp_agent.rx, agent).with_tracing(true);
                tokio::task::spawn_local(gw_rx.run());
                worker_cancel.cancelled().await;
                anyhow::Result::Ok(())
            })
        })?;
    Ok(BridgedAgent {
        thread_handle: handle,
        channel: acp_client,
        cancel: agent_cancel,
    })
}

fn commands_update() -> acp::SessionUpdate {
    let commands = COMMANDS
        .iter()
        .map(|(name, description)| acp::AvailableCommand::new(*name, *description))
        .collect();
    acp::SessionUpdate::AvailableCommandsUpdate(acp::AvailableCommandsUpdate::new(commands))
}

/// 会话模型表：优先服务端实时列表（/v1/models，4s 超时），失败回退静态表。
async fn live_models(cfg: &YjlConfig) -> acp::SessionModelState {
    let base = cfg.provider.base_url.clone();
    let key = cfg.provider.api_key.clone();
    let fetched = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        tokio::task::spawn_blocking(move || yjlcoder::llm::list_models(&base, &key)),
    )
    .await;
    if let Ok(Ok(Ok(remote))) = fetched {
        if !remote.is_empty() {
            let current = cfg.provider.model.clone();
            let mut ids: Vec<(String, String)> =
                remote.iter().map(|id| (id.clone(), id.clone())).collect();
            if !ids.iter().any(|(id, _)| *id == current) {
                ids.insert(0, (current.clone(), format!("{current}（配置）")));
            }
            let available = ids
                .iter()
                .map(|(id, name)| acp::ModelInfo::new(id.clone(), name.clone()))
                .collect();
            return acp::SessionModelState::new(current, available);
        }
    }
    models_from_config(cfg)
}

/// 会话模型表（静态回退）。
fn models_from_config(cfg: &YjlConfig) -> acp::SessionModelState {
    let current = cfg.provider.model.clone();
    let mut ids: Vec<(String, String)> = vec![
        (
            "deepseek-v4-flash".into(),
            "DeepSeek V4 Flash".into(),
        ),
        ("deepseek-v4-pro".into(), "DeepSeek V4 Pro".into()),
        (
            "deepseek-v4-flash-vision-exp".into(),
            "DeepSeek V4 Flash Vision".into(),
        ),
    ];
    if !ids.iter().any(|(id, _)| *id == current) {
        ids.insert(0, (current.clone(), current.clone()));
    }
    let available = ids
        .iter()
        .map(|(id, name)| acp::ModelInfo::new(id.clone(), name.clone()))
        .collect();
    acp::SessionModelState::new(current, available)
}

fn llm_from_config(cfg: &YjlConfig) -> Llm {
    Llm::remote(
        &cfg.provider.base_url,
        &cfg.provider.api_key,
        &cfg.provider.model,
        cfg.provider.timeout_secs,
        cfg.provider.load_context,
    )
}

/// 我们的问题结构 → grok 线型的 questions 数组（camelCase 信封由外层负责）。
fn questions_to_json(questions: &[yjlcoder::tools::AskQuestion]) -> Vec<serde_json::Value> {
    questions
        .iter()
        .map(|question| {
            serde_json::json!({
                "question": question.question,
                "options": question
                    .options
                    .iter()
                    .map(|option| {
                        serde_json::json!({
                            "label": option.label,
                            "description": option.description,
                            "preview": option.preview,
                        })
                    })
                    .collect::<Vec<_>>(),
                "multi_select": question.multi_select,
            })
        })
        .collect()
}

fn truncate_for_content(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let head: String = text.chars().take(max_chars / 2).collect();
        let tail: String = text.chars().skip(text.chars().count() - max_chars / 2).collect();
        format!("{head}\n…（中间内容已折叠，完整输出见会话）\n{tail}")
    }
}

impl YjlAgent {
    fn internal_error(message: impl Into<String>) -> acp::Error {
        acp::Error::new(acp::ErrorCode::InternalError.into(), message)
    }

    fn lock_session(&self) -> Result<std::sync::MutexGuard<'_, Option<SessionState>>, acp::Error> {
        self.session
            .lock()
            .map_err(|_| Self::internal_error("session lock poisoned"))
    }

    /// 工具调用生命周期：ToolRun 记号 → ToolCall，ToolProgress/ToolResult →
    /// ToolCallUpdate。我们的事件流一次只有一个活跃工具。
    async fn forward_tool_event(
        client: &AcpGatewaySender<acp::AgentSide>,
        session_id: &str,
        active_tools: &mut HashMap<String, String>,
        pending_tool: &mut Option<String>,
        event_tool_id: &mut Option<String>,
        op: &str,
        args: &str,
    ) {
        let tool_id = uuid::Uuid::new_v4().to_string();
        *event_tool_id = Some(tool_id.clone());
        active_tools.insert(op.to_string(), tool_id.clone());
        *pending_tool = Some(tool_id.clone());
        let title = serde_json::from_str::<serde_json::Value>(args)
            .ok()
            .and_then(|value| {
                value
                    .get("cmd")
                    .or_else(|| value.get("path"))
                    .or_else(|| value.get("query"))
                    .and_then(|text| text.as_str().map(String::from))
            })
            .unwrap_or_else(|| op.to_string());
        let raw_input = serde_json::from_str::<serde_json::Value>(args).ok();
        let call = acp::ToolCall::new(tool_id.clone(), format!("{op} {title}"))
            .kind(acp::ToolKind::Execute)
            .status(acp::ToolCallStatus::InProgress)
            .raw_input(raw_input);
        let _ = client
            .session_notification(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::ToolCall(call),
            ))
            .await;
    }

    async fn forward_tool_update(
        client: &AcpGatewaySender<acp::AgentSide>,
        session_id: &str,
        tool_id: &str,
        status: acp::ToolCallStatus,
        output: Option<String>,
    ) {
        let mut fields = acp::ToolCallUpdateFields::new().status(status);
        if let Some(output) = output {
            let text = truncate_for_content(&output, 4000);
            fields = fields.content(vec![acp::ToolCallContent::from(
                acp::ContentBlock::Text(acp::TextContent::new(text)),
            )]);
        }
        let _ = client
            .session_notification(acp::SessionNotification::new(
                session_id.to_string(),
                acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                    tool_id.to_string(),
                    fields,
                )),
            ))
            .await;
    }

    /// 提问往返：发送 grok 的 `x.ai/ask_user_question` ext 请求并等待用户作答。
    async fn ask_via_ext(
        &self,
        client: &AcpGatewaySender<acp::AgentSide>,
        session_id: &str,
        request: &AskRequest,
    ) -> Result<AskAnswer> {
        let payload = serde_json::json!({
            "sessionId": session_id,
            "toolCallId": uuid::Uuid::new_v4().to_string(),
            "questions": questions_to_json(&request.questions),
            "mode": "default",
        });
        let raw: std::sync::Arc<serde_json::value::RawValue> =
            serde_json::value::to_raw_value(&payload)?.into();
        let response = client
            .ext_method(acp::ExtRequest::new(ASK_USER_EXT_METHOD, raw))
            .await
            .map_err(|error| anyhow::anyhow!("ask_user_question 往返失败: {error}"))?;
        let value: serde_json::Value = serde_json::from_str(response.0.get())
            .map_err(|error| anyhow::anyhow!("ask_user_question 响应不是 JSON: {error}"))?;
        let outcome: AskOutcome = ask_outcome_from_json(&value)
            .map_err(|error| anyhow::anyhow!("ask_user_question 响应解析失败: {error}"))?;
        Ok(AskAnswer {
            id: request.id,
            outcome,
        })
    }

    /// 审批往返：shell 命令 → ACP `session/request_permission`。
    async fn perm_via_request(
        client: &AcpGatewaySender<acp::AgentSide>,
        session_id: &str,
        request: &PermRequest,
    ) -> Result<PermDecision> {
        let tool_call = acp::ToolCallUpdate::new(
            uuid::Uuid::new_v4().to_string(),
            acp::ToolCallUpdateFields::new()
                .title(format!("执行 shell 命令：{}", request.cmd))
                .kind(acp::ToolKind::Execute)
                .raw_input(serde_json::json!({"cmd": request.cmd})),
        );
        let options = vec![
            acp::PermissionOption::new("allow_once", "允许一次", acp::PermissionOptionKind::AllowOnce),
            acp::PermissionOption::new(
                "allow_always",
                "本会话总是允许",
                acp::PermissionOptionKind::AllowAlways,
            ),
            acp::PermissionOption::new("reject_once", "拒绝", acp::PermissionOptionKind::RejectOnce),
        ];
        let response = client
            .request_permission(acp::RequestPermissionRequest::new(
                session_id.to_string(),
                tool_call,
                options,
            ))
            .await
            .map_err(|error| anyhow::anyhow!("request_permission 往返失败: {error}"))?;
        let decision = match response.outcome {
            acp::RequestPermissionOutcome::Selected(selected) => match selected.option_id.0.as_ref() {
                "allow_always" => PermDecisionKind::AlwaysAllow,
                "reject_once" | "reject_always" => PermDecisionKind::No,
                _ => PermDecisionKind::Yes,
            },
            acp::RequestPermissionOutcome::Cancelled => PermDecisionKind::No,
            _ => PermDecisionKind::No,
        };
        Ok(PermDecision {
            id: request.id,
            decision,
        })
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for YjlAgent {
    async fn initialize(
        &self,
        _args: acp::InitializeRequest,
    ) -> Result<acp::InitializeResponse, acp::Error> {
        let commands: Vec<acp::AvailableCommand> = COMMANDS
            .iter()
            .map(|(name, description)| acp::AvailableCommand::new(*name, *description))
            .collect();
        let meta = serde_json::json!({
            "grokShell": true,
            "grokShellAgent": AGENT_NAME,
            "defaultAuthMethodId": AUTH_METHOD_ID,
            "cancelRewind": true,
            "availableCommands": commands,
        });
        let auth_method = acp::AuthMethod::Agent(acp::AuthMethodAgent::new(
            AUTH_METHOD_ID,
            "Yujiale Code · 本地模型服务",
        ));
        Ok(acp::InitializeResponse::new(acp::ProtocolVersion::V1)
            .agent_capabilities(acp::AgentCapabilities::new().load_session(true))
            .auth_methods(vec![auth_method])
            .agent_info(acp::Implementation::new(
                AGENT_NAME,
                env!("CARGO_PKG_VERSION"),
            ))
            .meta(meta.as_object().cloned()))
    }

    async fn authenticate(
        &self,
        _args: acp::AuthenticateRequest,
    ) -> Result<acp::AuthenticateResponse, acp::Error> {
        Ok(acp::AuthenticateResponse::new())
    }

    async fn new_session(
        &self,
        _args: acp::NewSessionRequest,
    ) -> Result<acp::NewSessionResponse, acp::Error> {
        let cfg = YjlConfig::load();
        let mut store = SessionStore::new(cfg.sessions_dir());
        // grok 语义：session/new = 全新对话。绝不能落在默认 main 上带旧
        // 历史（否则 /new 后模型仍记得之前内容）。
        store.new_session_timestamped();
        let llm = llm_from_config(&cfg);
        let id = uuid::Uuid::new_v4().to_string();
        let models = live_models(&cfg).await;
        *self.lock_session()? = Some(SessionState {
            id: id.clone(),
            cfg,
            llm,
            store,
            cancel: Arc::new(AtomicBool::new(false)),
            perm_auto: Arc::new(AtomicBool::new(false)),
            perm_allowed: Arc::new(Mutex::new(std::collections::HashSet::new())),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
        });
        let _ = self
            .client
            .session_notification(acp::SessionNotification::new(id.clone(), commands_update()))
            .await;
        Ok(acp::NewSessionResponse::new(id).models(models))
    }

    async fn load_session(
        &self,
        args: acp::LoadSessionRequest,
    ) -> Result<acp::LoadSessionResponse, acp::Error> {
        let session_id = args.session_id.0.to_string();
        let cfg = YjlConfig::load();
        let mut store = SessionStore::new(cfg.sessions_dir());
        // 容错：TUI 重启会带着上次的会话 ID 来恢复。已知 yjlcoder 会话 ID
        // 就直接切换；未知的（grok 侧 UUID）不报错——对齐原版 TUI 启动行为：
        // main 有历史则自动开新会话，避免把旧对话带进"新会话"。
        if store.switch(&session_id).is_err() {
            if !store.current().messages.is_empty() {
                store.new_session_timestamped();
            }
        }
        let messages = store.current().messages.clone();
        for message in &messages {
            let text = message.content.clone();
            if text.trim().is_empty() {
                continue;
            }
            let update = match message.role.as_str() {
                "user" => acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::Text(acp::TextContent::new(text)),
                )),
                "assistant" => acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                    acp::ContentBlock::Text(acp::TextContent::new(text)),
                )),
                _ => continue,
            };
            let meta = serde_json::json!({ "isReplay": true });
            let _ = self
                .client
                .session_notification(
                    acp::SessionNotification::new(session_id.clone(), update)
                        .meta(meta.as_object().cloned()),
                )
                .await;
        }
        let models = live_models(&cfg).await;
        *self.lock_session()? = Some(SessionState {
            id: session_id,
            llm: llm_from_config(&cfg),
            cfg,
            store,
            cancel: Arc::new(AtomicBool::new(false)),
            perm_auto: Arc::new(AtomicBool::new(false)),
            perm_allowed: Arc::new(Mutex::new(std::collections::HashSet::new())),
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
        });
        Ok(acp::LoadSessionResponse::new().models(models))
    }

    async fn prompt(&self, args: acp::PromptRequest) -> Result<acp::PromptResponse, acp::Error> {
        let session_id = args.session_id.0.to_string();
        let input = args
            .prompt
            .iter()
            .find_map(|block| match block {
                acp::ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        // 斜杠命令在 agent 端拦截执行（grok 架构如此），绝不喂给模型。
        let trimmed = input.trim();
        // /setup = opencode 式配置向导（grok 问题卡片）
        if trimmed == "/setup" || trimmed.starts_with("/setup ") {
            let done = self.run_setup_wizard(&session_id).await;
            let text = done.unwrap_or_else(|| "配置向导已取消；输入 /setup 重新开始".into());
            let _ = self
                .client
                .session_notification(acp::SessionNotification::new(
                    session_id.clone(),
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                        acp::ContentBlock::Text(acp::TextContent::new(text)),
                    )),
                ))
                .await;
            return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
        }
        // 首启 onboarding：出厂默认配置时，第一次对话先进向导
        if Self::needs_onboarding(&YjlConfig::load()) {
            self.run_setup_wizard(&session_id).await;
        }
        if trimmed.starts_with('/') {
            let output = self.handle_slash_command(&session_id, trimmed);
            let _ = self
                .client
                .session_notification(acp::SessionNotification::new(
                    session_id.clone(),
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                        acp::ContentBlock::Text(acp::TextContent::new(output)),
                    )),
                ))
                .await;
            return Ok(acp::PromptResponse::new(acp::StopReason::EndTurn));
        }
        let (cfg, llm, store, cancel, perm_auto, perm_allowed, cancel_notify) = {
            let mut guard = self.lock_session()?;
            let Some(session) = guard.as_mut() else {
                return Err(Self::internal_error("session 未初始化（先 session/new）"));
            };
            session.cancel.store(false, Ordering::Relaxed);
            (
                session.cfg.clone(),
                session.llm.clone(),
                session.store.clone(),
                session.cancel.clone(),
                session.perm_auto.clone(),
                session.perm_allowed.clone(),
                session.cancel_notify.clone(),
            )
        };

        // 与主程序 run_tui 相同的三通道结构：事件流 + 提问 + 审批
        let (ev_tx, ev_rx) = std::sync::mpsc::channel::<AgentEvent>();
        let (ask_req_tx, ask_req_rx) = std::sync::mpsc::channel::<AskRequest>();
        let (ask_answer_tx, ask_answer_rx) = std::sync::mpsc::channel::<AskAnswer>();
        let (perm_req_tx, perm_req_rx) = std::sync::mpsc::channel::<PermRequest>();
        let (perm_decision_tx, perm_decision_rx) = std::sync::mpsc::channel::<PermDecision>();
        // std → tokio 转发（run_turn 在阻塞线程池上）
        let (ev_bridge, mut ev_bridge_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let (ask_bridge, mut ask_bridge_rx) = tokio::sync::mpsc::unbounded_channel::<AskRequest>();
        let (perm_bridge, mut perm_bridge_rx) = tokio::sync::mpsc::unbounded_channel::<PermRequest>();
        std::thread::spawn(move || {
            while let Ok(event) = ev_rx.recv() {
                if ev_bridge.send(event).is_err() {
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            while let Ok(request) = ask_req_rx.recv() {
                if ask_bridge.send(request).is_err() {
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            while let Ok(request) = perm_req_rx.recv() {
                if perm_bridge.send(request).is_err() {
                    break;
                }
            }
        });

        let mut turn_handle = tokio::task::spawn_blocking(move || {
            let mut agent =
                yjlcoder::agent::Agent::with_store(cfg, llm, store, None, false, false, cancel);
            agent.set_ask_channels(ask_req_tx, ask_answer_rx);
            agent.set_perm_channels(perm_req_tx, perm_decision_rx, perm_auto, perm_allowed);
            let result = agent.run_turn(&input, &mut |event| {
                let _ = ev_tx.send(event);
            });
            result
        });

        let session_id_ref = session_id.clone();
        let client = self.client.clone();
        let mut active_tools: HashMap<String, String> = HashMap::new();
        let mut current_tool_id: Option<String> = None;
        let mut was_cancelled = false;
        let mut turn_error: Option<String> = None;

        loop {
            tokio::select! {
                biased;
                Some(request) = ask_bridge_rx.recv() => {
                    match self.ask_via_ext(&client, &session_id_ref, &request).await {
                        Ok(answer) => {
                            let _ = ask_answer_tx.send(answer);
                        }
                        Err(error) => {
                            // 传输失败才算工具失败；用户取消走 Cancelled outcome
                            let _ = ask_answer_tx.send(AskAnswer {
                                id: request.id,
                                outcome: AskOutcome::Cancelled,
                            });
                            tracing::warn!(%error, "ask_user_question 往返失败，按取消处理");
                        }
                    }
                }
                Some(request) = perm_bridge_rx.recv() => {
                    match Self::perm_via_request(&client, &session_id_ref, &request).await {
                        Ok(decision) => {
                            let _ = perm_decision_tx.send(decision);
                        }
                        Err(error) => {
                            let _ = perm_decision_tx.send(PermDecision {
                                id: request.id,
                                decision: PermDecisionKind::No,
                            });
                            tracing::warn!(%error, "request_permission 往返失败，按拒绝处理");
                        }
                    }
                }
                event = ev_bridge_rx.recv() => {
                    let Some(event) = event else { break };
                    match event {
                        AgentEvent::Delta(text) => {
                            let _ = client.session_notification(acp::SessionNotification::new(
                                session_id_ref.clone(),
                                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                                    acp::ContentBlock::Text(acp::TextContent::new(text)),
                                )),
                            )).await;
                        }
                        AgentEvent::Reasoning(text) => {
                            let _ = client.session_notification(acp::SessionNotification::new(
                                session_id_ref.clone(),
                                acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(
                                    acp::ContentBlock::Text(acp::TextContent::new(text)),
                                )),
                            )).await;
                        }
                        AgentEvent::ToolRun { op, args } => {
                            Self::forward_tool_event(
                                &client,
                                &session_id_ref,
                                &mut active_tools,
                                &mut None,
                                &mut current_tool_id,
                                &op,
                                &args,
                            )
                            .await;
                        }
                        AgentEvent::ToolProgress(progress) => {
                            if let Some(tool_id) = &current_tool_id {
                                Self::forward_tool_update(
                                    &client,
                                    &session_id_ref,
                                    tool_id,
                                    acp::ToolCallStatus::InProgress,
                                    Some(progress.output),
                                )
                                .await;
                            }
                        }
                        AgentEvent::ToolResult(result) => {
                            if let Some(tool_id) = current_tool_id.take() {
                                let failed = result.starts_with("错误");
                                let _ = active_tools.remove(&tool_id);
                                Self::forward_tool_update(
                                    &client,
                                    &session_id_ref,
                                    &tool_id,
                                    if failed {
                                        acp::ToolCallStatus::Failed
                                    } else {
                                        acp::ToolCallStatus::Completed
                                    },
                                    Some(result),
                                )
                                .await;
                            }
                        }
                        AgentEvent::Notice(text) => {
                            let _ = client.session_notification(acp::SessionNotification::new(
                                session_id_ref.clone(),
                                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                                    acp::ContentBlock::Text(acp::TextContent::new(text)),
                                )),
                            )).await;
                        }
                        AgentEvent::Error(text) => {
                            turn_error = Some(text.clone());
                            let _ = client.session_notification(acp::SessionNotification::new(
                                session_id_ref.clone(),
                                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                                    acp::ContentBlock::Text(acp::TextContent::new(format!(
                                        "⚠ {text}"
                                    ))),
                                )),
                            )).await;
                        }
                        AgentEvent::Done(_) | AgentEvent::TokenProgress(_) => {}
                    }
                }
                result = &mut turn_handle => {
                    let _ = result;
                    was_cancelled = cancel_was_set(&self.session, &session_id_ref);
                    break;
                }
                _ = cancel_notify.notified() => {
                    // session/cancel：置标志后限时等回合退出；超时则放弃该
                    // 回合线程，保证 UI 永远可以继续接收新输入。
                    was_cancelled = true;
                    match tokio::time::timeout(std::time::Duration::from_secs(8), &mut turn_handle).await
                    {
                        Ok(_) => {}
                        Err(_) => {
                            tracing::warn!("turn did not exit within 8s after cancel; detaching");
                        }
                    }
                    break;
                }
            }
        }

        let _ = turn_error;
        let stop_reason = if was_cancelled {
            acp::StopReason::Cancelled
        } else {
            acp::StopReason::EndTurn
        };
        Ok(acp::PromptResponse::new(stop_reason))
    }

    async fn cancel(&self, _args: acp::CancelNotification) -> Result<(), acp::Error> {
        if let Ok(mut guard) = self.session.lock() {
            if let Some(session) = guard.as_mut() {
                session.cancel.store(true, Ordering::Relaxed);
                session.cancel_notify.notify_one();
            }
        }
        Ok(())
    }

    async fn set_session_mode(
        &self,
        _args: acp::SetSessionModeRequest,
    ) -> Result<acp::SetSessionModeResponse, acp::Error> {
        Ok(acp::SetSessionModeResponse::new())
    }

    /// `/model` 切换：写入配置并立即生效（后续回合用新模型）。
    async fn set_session_model(
        &self,
        args: acp::SetSessionModelRequest,
    ) -> Result<acp::SetSessionModelResponse, acp::Error> {
        let model_id = args.model_id.0.to_string();
        let mut guard = self.lock_session()?;
        let Some(session) = guard.as_mut() else {
            return Err(Self::internal_error("session 未初始化"));
        };
        session.cfg.provider.model = model_id.clone();
        let _ = session.cfg.save();
        session.llm.set_model(model_id.clone());
        Ok(acp::SetSessionModelResponse::new())
    }

    async fn ext_method(
        &self,
        args: acp::ExtRequest,
    ) -> Result<acp::ExtResponse, acp::Error> {
        let method = args.method.to_string();
        let params = args.params.get().to_string();
        let _ = &params;
        match method.as_str() {
            // grok 插件市场：list / action（install/uninstall/reload）
            "x.ai/plugins/list" => {
                let body = plugins::list_response_json();
                let raw: std::sync::Arc<serde_json::value::RawValue> =
                    serde_json::value::to_raw_value(&serde_json::json!({ "result": body }))
                        .map_err(|e| Self::internal_error(e.to_string()))?
                        .into();
                Ok(acp::ExtResponse::new(raw))
            }
            "x.ai/plugins/action" => {
                let outcome = plugins::handle_action(&params).await;
                let raw: std::sync::Arc<serde_json::value::RawValue> =
                    serde_json::value::to_raw_value(&serde_json::json!({ "result": outcome }))
                        .map_err(|e| Self::internal_error(e.to_string()))?
                        .into();
                Ok(acp::ExtResponse::new(raw))
            }
            // Skills 面板：yjlcoder 技能 + grok 插件技能合并列表
            "x.ai/skills/list" => {
                let body = plugins::skills_list_json();

                let raw: std::sync::Arc<serde_json::value::RawValue> =
                    serde_json::value::to_raw_value(&serde_json::json!({ "result": body }))
                        .map_err(|e| Self::internal_error(e.to_string()))?
                        .into();
                Ok(acp::ExtResponse::new(raw))
            }
            "x.ai/skills/toggle" | "x.ai/skills/refresh-baseline" => {
                let raw: std::sync::Arc<serde_json::value::RawValue> =
                    serde_json::value::to_raw_value(&serde_json::json!({
                        "result": { "ok": true, "message": "已更新技能状态" }
                    }))
                    .map_err(|e| Self::internal_error(e.to_string()))?
                    .into();
                Ok(acp::ExtResponse::new(raw))
            }
            // 会话选择器兜底：空列表（会话管理走 /ls /use 斜杠命令）
            "x.ai/session/list" | "x.ai/sessions/list" => {
                let raw: std::sync::Arc<serde_json::value::RawValue> =
                    serde_json::value::to_raw_value(&serde_json::json!({
                        "result": { "sessions": [] }
                    }))
                    .map_err(|e| Self::internal_error(e.to_string()))?
                    .into();
                Ok(acp::ExtResponse::new(raw))
            }
            _ => Err(acp::Error::method_not_found()),
        }
    }

    async fn ext_notification(&self, _args: acp::ExtNotification) -> Result<(), acp::Error> {
        Ok(())
    }
}

impl YjlAgent {
    /// 在桥内执行斜杠命令；结果文本直接作为助手消息回显。
    /// 只覆盖无副作用或作用于会话存储的命令；配置类命令提示用 yjlcoder TUI。
    fn handle_slash_command(&self, session_id: &str, line: &str) -> String {
        let mut parts = line.trim_start_matches('/').splitn(2, ' ');
        let name = parts.next().unwrap_or_default().to_ascii_lowercase();
        let rest = parts.next().unwrap_or_default().trim().to_string();
        let mut guard = match self.session.lock() {
            Ok(guard) => guard,
            Err(_) => return "⚠ 会话状态锁损坏".into(),
        };
        let Some(session) = guard.as_mut() else {
            return "⚠ 会话未初始化".into();
        };
        match name.as_str() {
            "help" | "？" | "?" => format!(
                "可用命令：{}\n其余配置命令请在 yjlcoder TUI（yjlcoder）中使用。",
                COMMANDS
                    .iter()
                    .map(|(n, _)| format!("/{n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            "save" => {
                let msgs = session.store.current().messages;
                let md = yjlcoder::session::export_markdown(session.store.current_id(), &msgs);
                let path = if rest.is_empty() {
                    let secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    yjlcoder::config::data_dir()
                        .join("export")
                        .join(format!("{}-{secs}.md", session.store.current_id()))
                } else {
                    std::path::PathBuf::from(&rest)
                };
                let count = msgs.len();
                let shown = path.display().to_string();
                let result = (|| -> Result<(), String> {
                    if let Some(dir) = path.parent() {
                        if !dir.as_os_str().is_empty() {
                            std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败: {e}"))?;
                        }
                    }
                    std::fs::write(&path, md).map_err(|e| format!("写入失败: {e}"))
                })();
                match result {
                    Ok(()) => format!("已保存会话 {}（{count} 条消息，含工具日志）→ {shown}", session.store.current_id()),
                    Err(e) => format!("⚠ 保存失败: {e}"),
                }
            }
            "ls" => {
                let ids = session.store.list();
                if ids.is_empty() {
                    "暂无会话".into()
                } else {
                    format!("会话列表：{}", ids.join("  "))
                }
            }
            "new" => {
                let id = if rest.is_empty() {
                    session.store.new_session_timestamped()
                } else {
                    session.store.new_session(&rest)
                };
                session.id = id.clone();
                format!("已新建并切换到会话 {id}")
            }
            "use" => {
                if rest.is_empty() {
                    return "用法：/use <会话id>".into();
                }
                match session.store.switch(&rest) {
                    Ok(()) => {
                        session.id = rest.clone();
                        format!("已切换到会话 {rest}（历史在下次打开时回放）")
                    }
                    Err(e) => format!("⚠ 切换失败: {e}"),
                }
            }
            "rm" => {
                if rest.is_empty() {
                    return "用法：/rm <会话id>".into();
                }
                match session.store.delete(&rest) {
                    Ok(()) => format!("已删除会话 {rest}"),
                    Err(e) => format!("⚠ 删除失败: {e}"),
                }
            }
            "stats" => {
                let messages = session.store.current().messages;
                let used = yjlcoder::agent::estimate_context_tokens(
                    &session.cfg,
                    &session.llm,
                    &messages,
                    false,
                    false,
                );
                let window = yjlcoder::agent::effective_context_window(&session.cfg, &session.llm);
                format!("会话 {}：约 {used} / {window} tokens（{} 条消息）", session.store.current_id(), messages.len())
            }
            "models" => match yjlcoder::llm::list_models(
                &session.cfg.provider.base_url,
                &session.cfg.provider.api_key,
            ) {
                Ok(models) if models.is_empty() => "服务端未返回模型".into(),
                Ok(models) => format!("服务端模型：{}", models.join("  ")),
                Err(e) => format!("⚠ 获取失败: {e}"),
            },
            "skills" => match yjlcoder::skills::op_list_skills(&session.cfg) {
                Ok(text) => text,
                Err(e) => format!("⚠ {e}"),
            },
            "goal" => {
                // 与原版 TUI 同一入口：goal::handle_slash
                yjlcoder::goal::handle_slash(&session.cfg, session.store.current_id(), &rest).notice
            }
            "fuckmaster" => {
                // 与原版 TUI 同一入口：fuck_master::execute_slash
                match yjlcoder::fuck_master::execute_slash(&session.cfg, session.store.current_id(), &rest)
                {
                    Ok(reply) => reply,
                    Err(e) => format!("⚠ {e}"),
                }
            }
            "agents" => {
                let (action, value) = rest
                    .split_once(' ')
                    .map(|(a, v)| (a.to_ascii_lowercase(), v.trim()))
                    .unwrap_or_else(|| (rest.to_ascii_lowercase(), ""));
                match action.as_str() {
                    "" | "list" | "status" => format!(
                        "后台 Agent: {} · 单并发 {} · 每回合 {} · 最大 {} 步 · 模型 {}\n{}",
                        if session.cfg.agents.enabled { "开启" } else { "关闭" },
                        session.cfg.agents.max_concurrent.clamp(1, 2),
                        session.cfg.agents.max_spawn_per_turn.clamp(1, 2),
                        session.cfg.agents.max_steps.clamp(1, 16),
                        if session.cfg.agents.model.is_empty() { "继承主模型" } else { &session.cfg.agents.model },
                        yjlcoder::subagents::list_text(None),
                    ),
                    "on" => {
                        session.cfg.agents.enabled = true;
                        session.cfg.agents.max_concurrent = 1;
                        session.cfg.agents.max_spawn_per_turn = 1;
                        let _ = session.cfg.save();
                        "后台 Agent 已开启（单并发）".into()
                    }
                    "off" => {
                        session.cfg.agents.enabled = false;
                        let _ = session.cfg.save();
                        "后台 Agent 已关闭".into()
                    }
                    other => format!("用法：/agents [list|on|off|stop|model]，当前动作 {other} 不支持"),
                }
            }
            "autodangerous" => {
                // 会话级 shell 自动放行开关（作用于后续回合的审批）
                let turn_on = match rest.to_ascii_lowercase().as_str() {
                    "on" | "1" | "true" => true,
                    "off" | "0" | "false" => false,
                    "" => !session.perm_auto.load(Ordering::Relaxed),
                    _ => return format!("用法：/autodangerous [on|off]，当前 {}",
                        if session.perm_auto.load(Ordering::Relaxed) { "on" } else { "off" }),
                };
                session.perm_auto.store(turn_on, Ordering::Relaxed);
                format!(
                    "自动放行已{}（/autodangerous {}）",
                    if turn_on { "开启" } else { "关闭" },
                    if turn_on { "on" } else { "off" },
                )
            }
            "qqadmin" => {
                if rest.is_empty() {
                    format!(
                        "管理员: {:?}（用法: /qqadmin <QQ号> 添加管理员，可让管理员指挥 agent 操作电脑；非管理员只能闲聊）",
                        session.cfg.qq.admins
                    )
                } else {
                    match rest.parse::<i64>() {
                        Ok(qq) => {
                            let mut next = session.cfg.clone();
                            let added = next.qq.add_admin(qq);
                            let _ = next.save();
                            session.cfg = next;
                            if added {
                                format!("已添加管理员 {qq}（重启 QQ 桥接后生效）")
                            } else {
                                format!("{qq} 已是管理员")
                            }
                        }
                        Err(_) => "用法: /qqadmin <QQ号>".into(),
                    }
                }
            }
            "qqgroup" => {
                if rest.is_empty() {
                    format!(
                        "允许的群: {:?}（用法: /qqgroup <群号> 添加群，群内成员可闲聊、仅管理员可操作电脑）",
                        session.cfg.qq.groups
                    )
                } else {
                    match rest.parse::<i64>() {
                        Ok(gid) => {
                            let mut next = session.cfg.clone();
                            let added = next.qq.add_group(gid);
                            let _ = next.save();
                            session.cfg = next;
                            if added {
                                format!("已添加群 {gid}（重启 QQ 桥接后生效）")
                            } else {
                                format!("群 {gid} 已在允许列表")
                            }
                        }
                        Err(_) => "用法: /qqgroup <群号>".into(),
                    }
                }
            }
            "qqautonew" => match rest.parse::<usize>() {
                Ok(n) => {
                    let mut next = session.cfg.clone();
                    next.qq.auto_new = n;
                    let _ = next.save();
                    session.cfg = next;
                    format!("已设置：每 {n} 条 QQ 消息自动写群记忆并开启新对话（0=关闭）。重启 QQ 桥接后生效")
                }
                Err(_) => "用法: /qqautonew <消息数>（如 5，0=关闭）".into(),
            },
            "install" => {
                if rest.is_empty() {
                    "用法: /install <技能名>（pdf/docx/...）或 /install <URL/本地路径>；grok 插件请用 /plugin 安装".into()
                } else {
                    match yjlcoder::skills::op_install_skill(&serde_json::json!({"name": rest}), &session.cfg) {
                        Ok(s) => s,
                        Err(e) => format!("⚠ {e}"),
                    }
                }
            }
            "cfg" => {
                if rest == "show" || rest.is_empty() {
                    let p = &session.cfg.provider;
                    let key_state = if p.api_key.is_empty() {
                        "未设置"
                    } else {
                        &format!("{}…{}", &p.api_key[..2.min(p.api_key.len())], &p.api_key[p.api_key.len().saturating_sub(2)..])
                    };
                    format!(
                        "⚙ 配置总览\n服务 {}  ·  模型 {}  ·  密钥 {}\n上下文窗口 {}  ·  思考预算 {:?}\n价格估算 {}（{}）\n\n修改：/setup 向导 · /server <地址> · /apikey <密钥> · /model 切换 · /ctx <N|off> · /budget <N|off>",
                        p.base_url,
                        p.model,
                        key_state,
                        session.cfg.provider.ctx_override.map(|v| v.to_string()).unwrap_or_else(|| "自适应".into()),
                        p.thinking_budget,
                        if session.cfg.pricing.enabled { "开" } else { "关" },
                        session.cfg.pricing.currency,
                    )
                } else {
                    "用法：/cfg show 查看总览；/setup 打开配置向导（/config 被 TUI 内置设置面板占用）".into()
                }
            }
            "server" => {
                if rest.is_empty() {
                    format!("服务地址：{}（用法：/server <http(s)://...> 修改）", session.cfg.provider.base_url)
                } else if rest.starts_with("http") {
                    session.cfg.provider.base_url = rest.trim_end_matches('/').to_string();
                    let _ = session.cfg.save();
                    session.llm.set_base_url(session.cfg.provider.base_url.clone());
                    format!("✓ 服务地址已更新：{}", session.cfg.provider.base_url)
                } else {
                    "地址必须以 http:// 或 https:// 开头".into()
                }
            }
            "apikey" => {
                if rest.is_empty() {
                    "用法：/apikey <密钥> 设置 · /apikey clear 清除（不回显当前值）".into()
                } else if rest == "clear" {
                    session.cfg.provider.api_key.clear();
                    let _ = session.cfg.save();
                    session.llm.set_api_key(String::new());
                    "✓ 密钥已清除".into()
                } else {
                    session.cfg.provider.api_key = rest.to_string();
                    let _ = session.cfg.save();
                    session.llm.set_api_key(rest.to_string());
                    "✓ 密钥已保存（不回显）".into()
                }
            }
            "ctx" => {
                if rest.is_empty() {
                    format!(
                        "上下文窗口：当前有效 {}（/ctx <N> 覆盖 · /ctx off 恢复自适应）",
                        yjlcoder::agent::effective_context_window(&session.cfg, &session.llm)
                    )
                } else if rest == "off" {
                    session.cfg.provider.ctx_override = None;
                    let _ = session.cfg.save();
                    "✓ 上下文窗口恢复自适应".into()
                } else if let Ok(n) = rest.parse::<usize>() {
                    session.cfg.provider.ctx_override = Some(n);
                    let _ = session.cfg.save();
                    format!("✓ 上下文窗口覆盖为 {n}")
                } else {
                    "用法：/ctx <N|off>".into()
                }
            }
            "budget" => {
                if rest.is_empty() {
                    format!(
                        "思考预算：{:?}（/budget <N> 设置 · /budget off 关闭）",
                        session.cfg.provider.thinking_budget
                    )
                } else if rest == "off" {
                    session.cfg.provider.thinking_budget = None;
                    let _ = session.cfg.save();
                    session.llm.set_thinking_budget(None);
                    "✓ 思考预算已关闭".into()
                } else if let Ok(n) = rest.parse::<usize>() {
                    session.cfg.provider.thinking_budget = Some(n);
                    let _ = session.cfg.save();
                    session.llm.set_thinking_budget(Some(n));
                    format!("✓ 思考预算已设为 {n}")
                } else {
                    "用法：/budget <N|off>".into()
                }
            }
            "price" => {
                if rest.is_empty() {
                    let p = &session.cfg.pricing;
                    format!(
                        "价格估算：{} · 缓存命中 {}/百万 · 未命中 {}/百万 · 输出 {}/百万（/price off 关闭）",
                        if p.enabled { "开" } else { "关" },
                        p.cache_read_per_million,
                        p.cache_miss_per_million,
                        p.output_per_million,
                    )
                } else if rest == "off" {
                    session.cfg.pricing.enabled = false;
                    let _ = session.cfg.save();
                    "✓ 价格估算已关闭".into()
                } else {
                    "用法：/price off 关闭；详细单价请在 yjlcoder TUI 设置".into()
                }
            }
            "setup" => "配置向导已移至 /setup（会弹出三步选择卡）".into(),
            "compress" => "⚠ /compress 需要调用模型压缩，请在 yjlcoder TUI 中使用；或让我在对话里总结当前上下文。".into(),
            "clear" => "已清屏（滚动由 TUI 控制：Ctrl+L / 滚轮）".into(),
            "exit" => "请用 Ctrl+Q 退出 TUI".into(),
            other => format!(
                "⚠ 未知命令 /{other}（已拦截，未发送给模型）。/help 查看可用命令。"
            ),
        }
    }
}

/// 内置供应商注册表（OpenAI 兼容 chat 端点；参考 opencode/openclaw 的
/// providers 列表）。`(名称, base_url, 默认模型, 说明)`——选中即自动配置；
/// 默认模型在服务端 /v1/models 拉取失败时作为回退选项。
const PROVIDERS: &[(&str, &str, Option<&str>, &str)] = &[
    ("DeepSeek", "https://api.deepseek.com/v1", Some("deepseek-v4-flash"), "官方 API，便宜快速（Recommended）"),
    ("OpenAI", "https://api.openai.com/v1", Some("gpt-5.1"), "GPT 系列"),
    ("Anthropic", "https://api.anthropic.com/v1", Some("claude-sonnet-4-5"), "Claude 系列（OpenAI 兼容端点）"),
    ("Google Gemini", "https://generativelanguage.googleapis.com/v1beta/openai", Some("gemini-3-flash"), "Gemini 系列（OpenAI 兼容层）"),
    ("xAI Grok", "https://api.x.ai/v1", Some("grok-4-fast"), "Grok 系列"),
    ("Moonshot Kimi", "https://api.moonshot.cn/v1", Some("kimi-k2"), "月之暗面 Kimi"),
    ("阿里通义 Qwen", "https://dashscope.aliyuncs.com/compatible-mode/v1", Some("qwen3-max"), "百炼兼容模式"),
    ("智谱 GLM", "https://open.bigmodel.cn/api/paas/v4", Some("glm-4.6"), "智谱开放平台"),
    ("SiliconFlow 硅基流动", "https://api.siliconflow.cn/v1", Some("Qwen/Qwen3-32B"), "聚合推理，国内直连"),
    ("OpenRouter", "https://openrouter.ai/api/v1", Some("anthropic/claude-sonnet-4.5"), "数百模型一个 key"),
    ("Groq", "https://api.groq.com/openai/v1", Some("llama-3.3-70b-versatile"), "超低延迟 LPU 推理"),
    ("Mistral", "https://api.mistral.ai/v1", Some("mistral-large-latest"), "欧洲开源系厂商"),
    ("Cerebras", "https://api.cerebras.ai/v1", Some("llama3.3-70b"), "极速推理"),
    ("Together", "https://api.together.xyz/v1", Some("meta-llama/Llama-3.3-70B-Instruct-Turbo"), "开源模型聚合"),
    ("Fireworks", "https://api.fireworks.ai/inference/v1", Some("accounts/fireworks/models/llama-v3p3-70b-instruct"), "高性能托管"),
    ("DeepInfra", "https://api.deepinfra.com/v1/openai", Some("meta-llama/Llama-3.3-70B-Instruct"), "低价开源模型"),
    ("Perplexity", "https://api.perplexity.ai", Some("sonar-pro"), "带联网搜索的模型"),
    ("NVIDIA NIM", "https://integrate.api.nvidia.com/v1", Some("meta/llama-3.3-70b-instruct"), "NVIDIA 托管推理"),
    ("Hyperbolic", "https://api.hyperbolic.xyz/v1", Some("meta-llama/Llama-3.3-70B-Instruct"), "GPU 聚合市场"),
    ("Novita", "https://api.novita.ai/v3/openai", Some("meta-llama/llama-3.3-70b-instruct"), "低价聚合"),
    ("Chutes", "https://llm.chutes.ai/v1", Some("deepseek-ai/DeepSeek-V3"), "去中心化推理"),
    ("PPInfra", "https://api.ppinfra.com/v3/openai", Some("Qwen/Qwen2.5-72B-Instruct"), "PPInfra 聚合"),
    ("Baseten", "https://inference.baseten.co/v1", None, "专属 GPU 部署"),
    ("Cohere", "https://api.cohere.ai/compatibility/v1", Some("command-a"), "Command 系列（兼容端点）"),
    ("AI21", "https://api.ai21.com/studio/v1", Some("jamba-large-1.7"), "Jamba 系列"),
];

impl YjlAgent {
    /// 判断是否需要首启配置（全部为出厂默认值即视为未配置）。
    fn needs_onboarding(cfg: &YjlConfig) -> bool {
        cfg.provider.api_key.trim().is_empty()
            && cfg.provider.base_url == "https://api.deepseek.com"
            && cfg.provider.model == "deepseek-v4-flash"
    }

    /// opencode 式配置向导，用 grok 问题卡片承载：服务商 → API Key → 模型。
    /// 返回给用户的总结文本；用户中途取消返回 None。
    async fn run_setup_wizard(&self, session_id: &str) -> Option<String> {
        let notice = |text: String| {
            let client = self.client.clone();
            let sid = session_id.to_string();
            async move {
                let _ = client
                    .session_notification(acp::SessionNotification::new(
                        sid,
                        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                            acp::ContentBlock::Text(acp::TextContent::new(text)),
                        )),
                    ))
                    .await;
            }
        };
        notice("⚙ 配置向导（/setup）：三步选择服务 → 密钥 → 模型。Esc 取消。".into()).await;

        // ── 第 1 步：服务商（动态探测本地端点 + 内置注册表 + 自定义）──
        let mut provider_options: Vec<yjlcoder::tools::AskOption> = Vec::new();
        for endpoint in yjlcoder::setup::probe_local_endpoints("") {
            provider_options.push(yjlcoder::tools::AskOption {
                label: endpoint.label.clone(),
                description: format!("本地服务（在线） {}", endpoint.origin),
                preview: Some(endpoint.origin.clone()),
            });
        }
        for (name, url, _default, desc) in PROVIDERS {
            provider_options.push(yjlcoder::tools::AskOption {
                label: (*name).into(),
                description: (*desc).into(),
                preview: Some((*url).into()),
            });
        }
        provider_options.push(yjlcoder::tools::AskOption {
            label: "自定义 OpenAI 兼容".into(),
            description: "Other 里直接输入 base_url（如 https://api.xxx.com/v1）".into(),
            preview: None,
        });
        let request = AskRequest {
            id: 0,
            questions: vec![yjlcoder::tools::AskQuestion {
                question: "选择模型服务\n\n选一个服务商开始配置；自定义服务请选最后一项并在 Other 输入地址".into(),
                options: provider_options,
                multi_select: false,
            }],
        };
        let answer = self.ask_via_ext(&self.client, session_id, &request).await.ok()?;
        let (first_label, other_text) = match &answer.outcome {
            AskOutcome::Accepted { answers, annotations } => {
                let (_, labels) = answers.first()?;
                let label = labels.first().cloned().unwrap_or_default();
                // Other 的自定义输入文本在 notes 里
                let notes = if label == "Other" {
                    annotations
                        .first()
                        .and_then(|(_, ann)| ann.notes.clone())
                        .map(|n| n.trim().to_string())
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                (label, notes)
            }
            _ => return None,
        };

        // 解析 base_url：Other 自定义 > 注册表 > 本地探测端点
        let base_url = if !other_text.is_empty() && other_text.starts_with("http") {
            other_text.trim_end_matches('/').to_string()
        } else if let Some((_, url, _, _)) = PROVIDERS.iter().find(|(n, ..)| *n == first_label) {
            (*url).to_string()
        } else if let Some(endpoint) = yjlcoder::setup::probe_local_endpoints("")
            .into_iter()
            .find(|e| e.label == first_label)
        {
            format!("{}/v1", endpoint.origin)
        } else {
            return Some(format!("⚠ 无效的服务地址：{first_label}{other_text}。重新输入 /setup 再试").into());
        };

        // ── 第 2 步：API Key（本地服务可跳过）──
        let is_local = base_url.contains("127.0.0.1") || base_url.contains("localhost");
        let mut api_key = String::new();
        if !is_local {
            let request = AskRequest {
                id: 1,
                questions: vec![yjlcoder::tools::AskQuestion {
                    question: "输入 API Key\n\n粘贴密钥（不会回显）；其他 OpenAI 兼容服务的密钥同样适用".into(),
                    options: vec![yjlcoder::tools::AskOption {
                        label: "稍后配置".into(),
                        description: "先跳过，稍后 /setup 或 /apikey 补填".into(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
            };
            let answer = self.ask_via_ext(&self.client, session_id, &request).await.ok()?;
            if let AskOutcome::Accepted { answers, annotations } = &answer.outcome {
                if let Some((_, labels)) = answers.first() {
                    if labels.first().map(String::as_str) == Some("Other") {
                        if let Some((_, ann)) = annotations.first() {
                            if let Some(notes) = &ann.notes {
                                api_key = notes.trim().to_string();
                            }
                        }
                    }
                }
            }
            if api_key.is_empty() && base_url.contains("api.deepseek.com") {
                notice("⚠ DeepSeek 需要密钥；当前留空，发消息会失败。可 /setup 重配。".into()).await;
            }
        }

        // ── 第 3 步：模型（实时拉 /v1/models）──
        let fetched = tokio::task::spawn_blocking({
            let base_url = base_url.clone();
            let api_key = api_key.clone();
            move || yjlcoder::llm::list_models(&base_url, &api_key).unwrap_or_default()
        })
        .await
        .unwrap_or_default();
        let mut model_options: Vec<yjlcoder::tools::AskOption> = fetched
            .iter()
            .take(12)
            .map(|id| yjlcoder::tools::AskOption {
                label: id.clone(),
                description: format!("{base_url} 上的模型"),
                preview: None,
            })
            .collect();
        if model_options.is_empty() {
            // 服务端未返回列表：用注册表默认模型兜底
            if let Some((_, _, Some(default), _)) =
                PROVIDERS.iter().find(|(n, ..)| *n == first_label)
            {
                model_options.push(yjlcoder::tools::AskOption {
                    label: (*default).into(),
                    description: "该服务的推荐默认模型（服务端未返回列表）".into(),
                    preview: None,
                });
            }
            model_options.push(yjlcoder::tools::AskOption {
                label: "手动输入模型名".into(),
                description: "在 Other 里输入模型名".into(),
                preview: None,
            });
        }
        let request = AskRequest {
            id: 2,
            questions: vec![yjlcoder::tools::AskQuestion {
                question: "选择主模型\n\n列表来自服务端 /v1/models；不在列表里就选 Other 输入".into(),
                options: model_options,
                multi_select: false,
            }],
        };
        let answer = self.ask_via_ext(&self.client, session_id, &request).await.ok()?;
        let model = match &answer.outcome {
            AskOutcome::Accepted { answers, annotations } => {
                let (_, labels) = answers.first()?;
                match labels.first().map(String::as_str) {
                    Some("Other") => annotations
                        .first()
                        .and_then(|(_, ann)| ann.notes.clone())
                        .map(|n| n.trim().to_string())?,
                    Some(label) if !label.is_empty() => label.to_string(),
                    _ => return None,
                }
            }
            _ => return None,
        };

        // ── 落盘并生效 ──
        let summary = {
            let mut guard = self.session.lock().ok()?;
            let session = guard.as_mut()?;
            session.cfg.provider.base_url = base_url.clone();
            session.cfg.provider.api_key = api_key.clone();
            session.cfg.provider.model = model.clone();
            let _ = session.cfg.save();
            session.llm.set_base_url(base_url.clone());
            session.llm.set_api_key(api_key.clone());
            session.llm.set_model(model.clone());
            format!(
                "✓ 配置完成：{base_url} · {model}{}",
                if api_key.is_empty() { String::new() } else { " · 密钥已保存".into() }
            )
        };
        notice(summary.clone()).await;
        Some(summary)
    }
}

/// 回合结束时判断是否因取消而退出。
fn cancel_was_set(session: &Mutex<Option<SessionState>>, session_id: &str) -> bool {
    match session.lock() {
        Ok(guard) => guard
            .as_ref()
            .filter(|state| state.id == session_id)
            .is_some_and(|state| state.cancel.load(Ordering::Relaxed)),
        Err(_) => false,
    }
}
