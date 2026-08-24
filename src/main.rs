mod agent;
mod backend;
mod clipboard_copy;
mod compress;
mod config;
mod dynamic_tools;
mod fuck_master;
mod goal;
mod llm;
mod md;
mod prompt;
mod qq;
mod registry;
mod session;
mod setup;
mod skills;
mod time;
mod tool_compat;
mod tool_output;
mod tools;
mod tui;
mod web;
use agent::AgentEvent;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use agent::Agent;
use config::Config;
use llm::Llm;
use session::SessionStore;
use tools::{AskAnswer, AskRequest, PermDecision, PermRequest};
use tui::{Action, Tui};
#[derive(Debug, Default)]
struct TurnQueue {
    active: bool,
    pending: VecDeque<String>,
}
#[derive(Debug, PartialEq, Eq)]
enum TurnAdmission {
    Start(String),
    Queued(usize),
}
enum TurnTerminal {
    Done { text: String, usage: crate::llm::Usage, timings: crate::llm::Timings },
    Error(String),
}
impl TurnQueue {
    fn submit(&mut self, input: String) -> TurnAdmission {
        if self.active {
            self.pending.push_back(input);
            TurnAdmission::Queued(self.pending.len())
        } else {
            self.active = true;
            TurnAdmission::Start(input)
        }
    }
    fn is_active(&self) -> bool {
        self.active
    }
    fn pending_len(&self) -> usize {
        self.pending.len()
    }
    fn pop_pending(&mut self) -> Option<String> {
        self.pending.pop_back()
    }
    fn finish_active(&mut self) {
        self.active = false;
    }
    fn start_next(&mut self) -> Option<String> {
        if self.active {
            return None;
        }
        let next = self.pending.pop_front()?;
        self.active = true;
        Some(next)
    }
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut qq_mode = false;
    let mut qq_only = false;
    let mut mock = false;
    let mut setup_flag = false;
    let mut model_override: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--qq" => qq_mode = true,
            "--qq-only" => qq_only = true,
            "--mock" => mock = true,
            "--setup" => setup_flag = true,
            "--model" => {
                i += 1;
                model_override = args.get(i).cloned();
            }
            "--version" | "-V" => {
                println!("YJLcoder {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--help" | "-h" => {
                println!(
                    "YJLcoder — 极简纯 Rust 本地模型 CLI Agent\n\
                     \n\
                     用法:\n\
                       yjlcoder                 TUI 模式\n\
                       yjlcoder --setup         配置向导（探测本地服务 / 导入 Claude Code 配置）\n\
                       yjlcoder --mock          离线演示（无需 API key）\n\
                       yjlcoder --qq            TUI + QQ 桥接\n\
                       yjlcoder --qq-only       QQ 桥接守护\n\
                       yjlcoder --model <name>  临时切换模型\n\
                     \n\
                     配置: ~/.yjlcoder/config.json（首次运行自动生成；模型服务的\n\
                     上下文与 token 上限自动从服务端探测，无需手动配置）"
                );
                return;
            }
            other => {
                eprintln!("未知参数: {other}（--help 查看用法）");
                std::process::exit(1);
            }
        }
        i += 1;
    }
    let mut cfg = Config::load();
    if let Some(m) = model_override {
        cfg.provider.model = m;
        cfg.save();
    }
    if mock {
    } else if setup_flag {
        setup::run_wizard(&mut cfg);
    } else if setup::needs_setup(&cfg) {
        if qq_only {
            eprintln!("首次使用前请先运行配置向导: yjlcoder --setup");
            std::process::exit(1);
        }
        setup::run_wizard(&mut cfg);
    }
    let mut llm = if mock {
        Llm::mock()
    } else {
        Llm::remote(&cfg.provider.base_url, &cfg.provider.api_key, &cfg.provider.model, cfg.provider.timeout_secs, cfg.provider.load_context)
    };
    llm.set_llamacpp_router_bypass(!mock && cfg.llama.auto_start);
    llm.set_thinking_budget(cfg.provider.thinking_budget);
    if !mock {
        llm.probe_server();
    }
    if qq_only {
        let (llama_ok, llama_note) = ensure_llama_online(&cfg);
        if !llama_ok {
            eprintln!("[llama] {llama_note}");
        }
        let cancel = Arc::new(AtomicBool::new(false));
        match qq::run(cfg, llm, cancel) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("QQ 桥接启动失败: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    run_tui(cfg, llm, qq_mode);
}
const COMMANDS: &[(&str, &str)] = &[
    ("help", "查看命令帮助"),
    ("setup", "配置向导（探测本地服务 / 导入 Claude Code 配置）"),
    ("server", "查看/设置模型服务地址"),
    ("model", "查看/切换模型（列出服务端可用模型）"),
    ("apikey", "查看/设置 API 密钥"),
    ("ctx", "查看/覆盖上下文窗口"),
    ("budget", "查看/设置思考预算"),
    ("maxtokens", "查看/设置单轮生成上限"),
    ("config", "查看当前全部配置总览"),
    ("goal", "持续执行目标（状态/暂停/恢复/清除）"),
    ("FuckMaster", "定时主动通过 QQ 推进目标"),
    ("copy", "复制最近一条助手完整回复（Ctrl+O）"),
    ("new", "新建会话"),
    ("ls", "列出会话"),
    ("use", "切换会话"),
    ("rm", "删除会话"),
    ("compress", "手动压缩上下文"),
    ("stats", "上下文统计"),
    ("tool_times", "设置工具调用轮数上限"),
    ("timeout", "设置请求/流式读取超时秒数"),
    ("commandcountdown", "设置命令默认超时秒数（默认 600）"),
    ("autodangerous", "模型执行命令无需批准（on/off，默认 off）"),
    ("trace", "思考流实时显示开关（on/off）"),
    ("fuckloop", "工具后思考循环整理开关（on/off）"),
    ("save", "导出当前会话（含工具日志）到文件"),
    ("models", "列出/切换服务端模型"),
    ("reloadmodel", "重载模型"),
    ("qqadmin", "添加 QQ 管理员"),
    ("qqgroup", "添加允许的群"),
    ("qqautonew", "QQ 自动开新对话间隔"),
    ("skills", "已安装技能"),
    ("install", "安装技能"),
    ("clear", "清屏"),
    ("exit", "退出"),
];
fn llama_health_ok(base_url: &str) -> bool {
    let origin = base_url.trim_end_matches('/');
    let origin = origin.strip_suffix("/v1").unwrap_or(origin);
    let url = format!("{origin}/health");
    match ureq::get(&url).timeout(std::time::Duration::from_secs(2)).call() {
        Ok(r) => r.status() == 200,
        Err(_) => false,
    }
}
fn ensure_llama_online(cfg: &Config) -> (bool, String) {
    if llama_health_ok(&cfg.provider.base_url) {
        return (true, "模型服务在线".into());
    }
    if !cfg.llama.auto_start {
        return (false, format!("模型服务不可达（{}），且未开启自动拉起", cfg.provider.base_url));
    }
    let svc = &cfg.llama.service;
    if svc.is_empty() {
        return (
            false,
            "模型服务不可达，且 auto_start 已开启但未配置服务名（service）。\
             请运行 yjlcoder --setup 或编辑 ~/.yjlcoder/config.json 填写 systemd 服务名"
                .into(),
        );
    }
    let started = std::process::Command::new("sudo")
        .args(["-n", "systemctl", "start", svc])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !started {
        return (false, format!("模型服务不可达，且自动拉起 {svc} 失败（需要免密 sudo）"));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(cfg.llama.start_wait_secs);
    while std::time::Instant::now() < deadline {
        if llama_health_ok(&cfg.provider.base_url) {
            return (true, format!("模型服务已自动拉起（{svc}）并就绪"));
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    (false, format!("已发起拉起 {svc}，但等待 {cfg} 秒后仍未就绪", cfg = cfg.llama.start_wait_secs))
}
fn run_tui(mut cfg: Config, mut llm: Llm, with_qq: bool) {
    let cancel = Arc::new(AtomicBool::new(false));
    if with_qq {
        let c = cfg.clone();
        let l = llm.clone();
        let cancel2 = cancel.clone();
        std::thread::spawn(move || {
            if let Err(e) = qq::run(c, l, cancel2) {
                eprintln!("QQ 桥接失败: {e}");
            }
        });
        println!("QQ 桥接已后台启动（ws://{}{}）", cfg.qq.ws_addr, cfg.qq.ws_path);
    }
    let mut tui = Tui::new();
    tui.set_commands(COMMANDS);
    let guard = tui.enter();
    let (llama_ok, llama_note) = ensure_llama_online(&cfg);
    if !llama_ok {
        tui.push_warn(format!("⚠ {llama_note}"));
    }
    let mut store = SessionStore::new(cfg.sessions_dir());
    if !store.current().messages.is_empty() {
        store.new_session_timestamped();
    }
    let (key_tx, key_rx): (Sender<u8>, Receiver<u8>) = channel();
    tui::spawn_stdin_reader(key_tx);
    let (ev_tx, ev_rx): (Sender<AgentEvent>, Receiver<AgentEvent>) = channel();
    let (terminal_tx, terminal_rx): (Sender<TurnTerminal>, Receiver<TurnTerminal>) = channel();
    let (ask_tx, ask_rx): (Sender<AskRequest>, Receiver<AskRequest>) = channel();
    let (perm_tx, perm_rx): (Sender<PermRequest>, Receiver<PermRequest>) = channel();
    let perm_auto = Arc::new(AtomicBool::new(false));
    let perm_allowed: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let mut parser = tui::KeyParser::new();
    let mut turns = TurnQueue::default();
    let mut quit = false;
    let mut cur_answer_tx: Option<Sender<AskAnswer>> = None;
    let mut cur_ask_id: Option<u64> = None;
    let mut cur_perm_tx: Option<Sender<PermDecision>> = None;
    let mut cur_perm_id: Option<u64> = None;
    let mut pending_goal_retry: Option<(std::time::Instant, usize, String)> = None;
    refresh_header(&mut tui, &cfg, &llm, &store);
    while !quit {
        let key = match key_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(b) => parser.feed(b),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => parser.flush_pending_escape(),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
        };
        if let Some(key) = key {
                match tui.handle_key(key) {
                    Action::Submit(text) => {
                        pending_goal_retry = None;
                        match turns.submit(text) {
                            TurnAdmission::Queued(position) => {
                                tui.set_queued_count(position);
                            }
                            TurnAdmission::Start(text) => {
                                let channels = start_tui_turn(
                                    &mut tui,
                                    text,
                                    TurnContext {
                                        cfg: &cfg,
                                        llm: &llm,
                                        store: &store,
                                        ev_tx: &ev_tx,
                                        cancel: &cancel,
                                        ask_tx: &ask_tx,
                                        perm_tx: &perm_tx,
                                        perm_auto: &perm_auto,
                                        perm_allowed: &perm_allowed,
                                        terminal_tx: &terminal_tx,
                                    },
                                );
                                cur_answer_tx = Some(channels.answer_tx);
                                cur_perm_tx = Some(channels.decision_tx);
                            }
                        }
                    }
                    Action::Command(cmd) => {
                        let live_cmd = matches!(cmd.split(' ').next(), Some("autodangerous" | "copy"))
                            || goal_control_is_safe_while_running(&cmd)
                            || cmd.split_whitespace().next().is_some_and(|name| name.eq_ignore_ascii_case("FuckMaster"));
                        if turns.is_active() && !live_cmd {
                            tui.push_warn(
                                "当前回合仍在运行，斜杠命令未执行；请等待完成或按 Esc 中断".into(),
                            );
                        } else {
                            let (command_name, command_args) = cmd
                                .split_once(' ')
                                .map(|(name, args)| (name, args.trim()))
                                .unwrap_or((cmd.as_str(), ""));
                            if command_name == "goal" {
                                pending_goal_retry = None;
                                let outcome = goal::handle_slash(&cfg, store.current_id(), command_args);
                                tui.push_system(outcome.notice);
                                if let Some(prompt) = outcome.prompt {
                                    match turns.submit(prompt) {
                                        TurnAdmission::Start(prompt) => {
                                            let channels = start_tui_goal_turn(
                                                &mut tui,
                                                prompt,
                                                TurnContext {
                                                    cfg: &cfg,
                                                    llm: &llm,
                                                    store: &store,
                                                    ev_tx: &ev_tx,
                                                    cancel: &cancel,
                                                    ask_tx: &ask_tx,
                                                    perm_tx: &perm_tx,
                                                    perm_auto: &perm_auto,
                                                    perm_allowed: &perm_allowed,
                                                    terminal_tx: &terminal_tx,
                                                },
                                            );
                                            cur_answer_tx = Some(channels.answer_tx);
                                            cur_perm_tx = Some(channels.decision_tx);
                                        }
                                        TurnAdmission::Queued(position) => tui.set_queued_count(position),
                                    }
                                }
                            } else {
                                handle_command(
                                    &cmd,
                                    &mut tui,
                                    &mut store,
                                    &mut cfg,
                                    &mut llm,
                                    &ev_tx,
                                    &cancel,
                                    &perm_auto,
                                    &perm_allowed,
                                );
                            }
                        }
                    }
                    Action::RetrieveQueued => {
                        if let Some(text) = turns.pop_pending() {
                            tui.retrieve_queued(text);
                        }
                    }
                    Action::AskSubmit(answers) => {
                        if let (Some(atx), Some(id)) = (cur_answer_tx.as_ref(), cur_ask_id.take()) {
                            let _ = atx.send(AskAnswer { id, answers });
                        }
                        tui.finish_ask();
                    }
                    Action::PermSubmit(decision) => {
                        if let (Some(ptx), Some(id)) = (cur_perm_tx.as_ref(), cur_perm_id.take()) {
                            let _ = ptx.send(PermDecision { id, decision });
                        }
                        tui.finish_perm_prompt();
                    }
                    Action::Cancel => {
                        cancel.store(true, Ordering::Relaxed);
                        if tui.is_asking() {
                            tui.finish_ask();
                            cur_answer_tx = None;
                            cur_ask_id = None;
                        }
                        if tui.is_streaming() {
                            tui.cancel_streaming();
                        }
                    }
                    Action::Quit => quit = true,
                    Action::Redraw => {}
                    Action::None => {}
                }
        }
        while let Ok(req) = ask_rx.try_recv() {
            cur_ask_id = Some(req.id);
            tui.ask(req.questions);
        }
        while let Ok(req) = perm_rx.try_recv() {
            cur_perm_id = Some(req.id);
            tui.open_perm_prompt(req);
        }
        while let Ok(ev) = ev_rx.try_recv() {
            match ev {
                AgentEvent::Delta(d) => tui.assistant_delta(&d),
                AgentEvent::Reasoning(r) => {
                    if cfg.trace.show_reasoning {
                        tui.push_reasoning(r);
                    }
                }
                AgentEvent::ToolRun { op, args } => tui.push_tool(&op, &args),
                AgentEvent::ToolProgress(progress) => tui.push_tool_progress(&progress),
                AgentEvent::ToolResult(r) => tui.push_tool_result(&r),
                AgentEvent::Notice(n) => tui.push_system(n),
                AgentEvent::Garbage { kind, sample, run, total, limit } => {
                    let count = if limit == 0 {
                        format!("（累计第 {total} 次，仅记录不中止）")
                    } else {
                        format!("（累计第 {total} 次 · 连续 {run}/{limit}）")
                    };
                    tui.push_bug_warn(format!("垃圾 token 检测（{kind}）: {sample}{count}"));
                }
                AgentEvent::Error(e) => {
                    tui.push_error(e);
                }
                AgentEvent::Done(final_text) => {
                    tui.end_assistant(&final_text);
                }
            }
        }
        let mut normal_turn_completed = false;
        while let Ok(terminal) = terminal_rx.try_recv() {
            match terminal {
                TurnTerminal::Done { text, usage, timings } => {
                    tui.end_assistant_with_metrics(&text, usage, timings);
                    pending_goal_retry = None;
                    normal_turn_completed = true;
                    if let Some(state) = goal::record_turn_usage(
                        &cfg,
                        store.current_id(),
                        usage.prompt_tokens.saturating_add(usage.completion_tokens),
                    ) {
                        if state.status == goal::GoalStatus::BudgetLimited {
                            tui.push_warn(format!(
                                "目标 token 预算已用完（{}），已停止自动续跑",
                                state.tokens_used
                            ));
                        }
                    }
                }
                TurnTerminal::Error(error) => {
                    if tui.is_streaming() {
                        tui.end_assistant("");
                    }
                    tui.push_error(error.clone());
                    if error.trim() == "已取消" {
                        let outcome = goal::handle_slash(&cfg, store.current_id(), "pause");
                        if outcome.notice == "目标已暂停" {
                            pending_goal_retry = None;
                            tui.push_warn("当前 Goal 已随用户中断暂停；用 /goal resume 继续".into());
                        }
                    } else {
                        match goal::record_runtime_failure(&cfg, store.current_id(), &error) {
                            Ok(Some(goal::RuntimeFailureAction::Retry {
                                attempt,
                                delay_secs,
                                turn,
                                prompt,
                            })) => {
                                pending_goal_retry = Some((
                                    std::time::Instant::now()
                                        + std::time::Duration::from_secs(delay_secs),
                                    turn,
                                    prompt,
                                ));
                                tui.push_warn(format!(
                                    "Goal 第 {attempt}/{} 次运行错误；{delay_secs} 秒后从中断点自动重试",
                                    goal::RUNTIME_FAILURE_LIMIT
                                ));
                            }
                            Ok(Some(goal::RuntimeFailureAction::Paused { attempts })) => {
                                pending_goal_retry = None;
                                tui.push_warn(format!(
                                    "Goal 连续 {attempts} 次运行错误，已安全暂停；服务恢复后用 /goal resume 继续"
                                ));
                            }
                            Ok(None) => {}
                            Err(e) => tui.push_error(format!("Goal 错误状态保存失败: {e}")),
                        }
                    }
                }
            }
            turns.finish_active();
            if tui.is_asking() {
                tui.finish_ask();
            }
            if tui.is_perm_prompt() {
                tui.finish_perm_prompt();
            }
            cur_answer_tx = None;
            cur_ask_id = None;
            cur_perm_tx = None;
            cur_perm_id = None;
        }
        if !tui.is_asking() && !tui.is_perm_prompt() {
            if let Some(input) = turns.start_next() {
                tui.set_queued_count(turns.pending_len());
                let channels = start_tui_turn(
                    &mut tui,
                    input,
                    TurnContext {
                        cfg: &cfg,
                        llm: &llm,
                        store: &store,
                        ev_tx: &ev_tx,
                        cancel: &cancel,
                        ask_tx: &ask_tx,
                        perm_tx: &perm_tx,
                        perm_auto: &perm_auto,
                        perm_allowed: &perm_allowed,
                        terminal_tx: &terminal_tx,
                    },
                );
                cur_answer_tx = Some(channels.answer_tx);
                cur_perm_tx = Some(channels.decision_tx);
            } else if pending_goal_retry
                .as_ref()
                .is_some_and(|(due, _, _)| std::time::Instant::now() >= *due)
            {
                let (_, turn, prompt) = pending_goal_retry.take().unwrap();
                if goal::load(&cfg, store.current_id())
                    .is_some_and(|state| state.status == goal::GoalStatus::Active)
                {
                    if let TurnAdmission::Start(prompt) = turns.submit(prompt) {
                        tui.push_system(format!("◎ /goal 运行错误恢复 · 第 {turn} 轮"));
                        let channels = start_tui_goal_turn(
                            &mut tui,
                            prompt,
                            TurnContext {
                                cfg: &cfg,
                                llm: &llm,
                                store: &store,
                                ev_tx: &ev_tx,
                                cancel: &cancel,
                                ask_tx: &ask_tx,
                                perm_tx: &perm_tx,
                                perm_auto: &perm_auto,
                                perm_allowed: &perm_allowed,
                                terminal_tx: &terminal_tx,
                            },
                        );
                        cur_answer_tx = Some(channels.answer_tx);
                        cur_perm_tx = Some(channels.decision_tx);
                    }
                }
            } else if normal_turn_completed {
                match goal::next_continuation(&cfg, store.current_id()) {
                    Ok(Some((turn, prompt))) => {
                        if let TurnAdmission::Start(prompt) = turns.submit(prompt) {
                            tui.push_system(format!("◎ /goal 自动续跑 · 第 {turn} 轮"));
                            let channels = start_tui_goal_turn(
                                &mut tui,
                                prompt,
                                TurnContext {
                                    cfg: &cfg,
                                    llm: &llm,
                                    store: &store,
                                    ev_tx: &ev_tx,
                                    cancel: &cancel,
                                    ask_tx: &ask_tx,
                                    perm_tx: &perm_tx,
                                    perm_auto: &perm_auto,
                                    perm_allowed: &perm_allowed,
                                    terminal_tx: &terminal_tx,
                                },
                            );
                            cur_answer_tx = Some(channels.answer_tx);
                            cur_perm_tx = Some(channels.decision_tx);
                        }
                    }
                    Ok(None) => {
                        if goal::load(&cfg, store.current_id())
                            .is_some_and(|state| state.status == goal::GoalStatus::MaxTurns)
                        {
                            tui.push_warn(format!(
                                "目标已达到 {} 轮安全上限；用 /goal continue 明确继续",
                                goal::MAX_GOAL_TURNS
                            ));
                        }
                    }
                    Err(e) => tui.push_error(format!("目标续跑状态保存失败: {e}")),
                }
            }
        }
        refresh_header(&mut tui, &cfg, &llm, &store);
        tui.redraw();
    }
    cancel.store(true, Ordering::Relaxed);
    tui.exit();
    drop(guard);
    println!("bye");
}
struct TurnContext<'a> {
    cfg: &'a Config,
    llm: &'a Llm,
    store: &'a SessionStore,
    ev_tx: &'a Sender<AgentEvent>,
    cancel: &'a Arc<AtomicBool>,
    ask_tx: &'a Sender<AskRequest>,
    perm_tx: &'a Sender<PermRequest>,
    perm_auto: &'a Arc<AtomicBool>,
    perm_allowed: &'a Arc<Mutex<HashSet<String>>>,
    terminal_tx: &'a Sender<TurnTerminal>,
}
fn start_tui_turn(tui: &mut Tui, input: String, context: TurnContext<'_>) -> TurnChannels {
    context.cancel.store(false, Ordering::Relaxed);
    tui.push_user(input.clone());
    tui.begin_assistant();
    spawn_turn(TurnSpawn {
        cfg: context.cfg.clone(),
        llm: context.llm.clone(),
        store: context.store.clone(),
        ev_tx: context.ev_tx.clone(),
        cancel: context.cancel.clone(),
        ask_tx: context.ask_tx.clone(),
        perm_tx: context.perm_tx.clone(),
        perm_auto: context.perm_auto.clone(),
        perm_allowed: context.perm_allowed.clone(),
        terminal_tx: context.terminal_tx.clone(),
        input,
    })
}
fn start_tui_goal_turn(tui: &mut Tui, input: String, context: TurnContext<'_>) -> TurnChannels {
    context.cancel.store(false, Ordering::Relaxed);
    tui.begin_assistant();
    spawn_turn(TurnSpawn {
        cfg: context.cfg.clone(),
        llm: context.llm.clone(),
        store: context.store.clone(),
        ev_tx: context.ev_tx.clone(),
        cancel: context.cancel.clone(),
        ask_tx: context.ask_tx.clone(),
        perm_tx: context.perm_tx.clone(),
        perm_auto: context.perm_auto.clone(),
        perm_allowed: context.perm_allowed.clone(),
        terminal_tx: context.terminal_tx.clone(),
        input,
    })
}
fn goal_control_is_safe_while_running(cmd: &str) -> bool {
    let (name, args) = cmd
        .split_once(' ')
        .map(|(name, args)| (name, args.trim().to_ascii_lowercase()))
        .unwrap_or((cmd, String::new()));
    name == "goal"
        && matches!(
            args.as_str(),
            "" | "status" | "pause" | "clear" | "stop" | "off" | "reset" | "none" | "cancel" | "complete"
        )
}
fn refresh_header(tui: &mut Tui, cfg: &Config, llm: &Llm, store: &SessionStore) {
    let (_msgs, tokens) = session::session_stats(&store.path(store.current_id()));
    let mode = if cfg.provider.native_tools { "native" } else { "text" };
    tui.set_header(format!(
        "{} │ 会话 {} │ {}",
        llm.model_name(),
        store.current_id(),
        mode
    ));
    tui.set_ctx(
        tokens,
        backend::effective_window(cfg.provider.ctx_override, llm.n_ctx(), cfg.provider.ctx_window),
    );
}
struct TurnSpawn {
    cfg: Config,
    llm: Llm,
    store: SessionStore,
    ev_tx: Sender<AgentEvent>,
    cancel: Arc<AtomicBool>,
    ask_tx: Sender<AskRequest>,
    perm_tx: Sender<PermRequest>,
    perm_auto: Arc<AtomicBool>,
    perm_allowed: Arc<Mutex<HashSet<String>>>,
    terminal_tx: Sender<TurnTerminal>,
    input: String,
}
struct TurnChannels {
    answer_tx: Sender<AskAnswer>,
    decision_tx: Sender<PermDecision>,
}
fn spawn_turn(turn: TurnSpawn) -> TurnChannels {
    let (answer_tx, answer_rx) = channel();
    let (decision_tx, decision_rx) = channel();
    std::thread::spawn(move || {
        let mut agent =
            Agent::with_store(turn.cfg, turn.llm, turn.store, None, false, false, turn.cancel);
        agent.set_ask_channels(turn.ask_tx, answer_rx);
        agent.set_perm_channels(turn.perm_tx, decision_rx, turn.perm_auto, turn.perm_allowed);
        agent.set_tool_event_channel(turn.ev_tx.clone());
        match agent.run_turn(&turn.input, &mut |ev| {
            let _ = turn.ev_tx.send(ev);
        }) {
            Ok(final_text) => {
                let usage = agent.last_usage;
                let timings = agent.last_timings;
                let _ = turn.terminal_tx.send(TurnTerminal::Done {
                    text: final_text,
                    usage,
                    timings,
                });
            }
            Err(e) => {
                let _ = turn.terminal_tx.send(TurnTerminal::Error(e));
            }
        }
    });
    TurnChannels { answer_tx, decision_tx }
}
fn sync_llm_from_cfg(llm: &mut Llm, cfg: &Config) {
    llm.set_base_url(cfg.provider.base_url.clone());
    llm.set_api_key(cfg.provider.api_key.clone());
    llm.set_thinking_budget(cfg.provider.thinking_budget);
    llm.set_llamacpp_router_bypass(cfg.llama.auto_start);
    llm.probe_server();
}
fn server_caps_note(llm: &Llm) -> String {
    if let Some(n) = llm.n_ctx() {
        format!("服务端: llama.cpp · 上下文 n_ctx {n} · 模型 {}", llm.model_name())
    } else if llm.is_llamacpp() {
        format!("服务端: llama.cpp（需 API 密钥后重新探测能力）")
    } else {
        format!("服务端: 非 llama.cpp（能力未知，使用配置回退值）")
    }
}
fn handle_command(
    cmd: &str,
    tui: &mut Tui,
    store: &mut SessionStore,
    cfg: &mut Config,
    llm: &mut Llm,
    ev_tx: &Sender<AgentEvent>,
    cancel: &AtomicBool,
    perm_auto: &Arc<AtomicBool>,
    perm_allowed: &Arc<Mutex<HashSet<String>>>,
) {
    let (name, rest) = match cmd.split_once(' ') {
        Some((n, r)) => (n, r.trim()),
        None => (cmd, ""),
    };
    let name = name.to_ascii_lowercase();
    match name.as_str() {
        "help" => {
            tui.push_system(
                "命令:\n\
                 /new [id]     新建会话\n\
                 /ls           列出会话\n\
                 /use <id>     切换会话\n\
                 /rm <id>      删除会话（不能删当前）\n\
                 /compress     手动压缩上下文\n\
                 /stats        上下文统计\n\
                 /goal <目标>  持续执行直到完成（status/pause/resume/continue/clear）\n\
                 /FuckMaster [目标] 主动推进；add 2h 目标 / list / pause|resume|now|delete fm-0001\n\
                 /copy         复制最近一条助手完整回复（Ctrl+O；不受屏幕折行影响）\n\
                 /tool_times <N> 工具调用轮数上限（默认 24，1-200）\n\
                 /timeout <秒> 请求/流式读取超时（默认 120，10-3600）\n\
                 /commandcountdown <秒> 命令默认超时（默认 600，1-3600；超时后已产出输出会交回模型）\n\
                 /autodangerous [on|off] 模型执行命令无需批准（默认 off：命令先停下等您拍板，\n\
                 ↑↓ 选择 + 回车：允许一次 / 本会话全部同意同类 / 拒绝；随时可开，\n\
                 正在跑的回合不打断，批准面板里按 4 也可直接开启）\n\
                 /trace [on|off] 思考流实时显示开关（默认关闭；思考流全文始终记录到 ~/.yjlcoder/trace/）\n\
                 /fuckloop [on|off] 工具后思考循环整理开关（默认开启）\n\
                 /save [路径]  导出当前会话（含工具日志）到文件（默认 ~/.yjlcoder/export/）\n\
                 /reloadmodel  重载模型（卡死/输出异常时恢复）\n\
                 \n\
                 [配置]\n\
                 /setup        配置向导（探测本地服务/导入 Claude Code 配置）\n\
                 /server [地址] 查看或设置模型服务地址（llama.cpp/Ollama/LM Studio/兼容 API）\n\
                 /model <名称>  查看或切换模型（llama.cpp 自动列出服务端可用模型）\n\
                 /apikey [密钥] 查看或设置 API 密钥（/apikey clear 清除）\n\
                 /ctx [N|off]  查看或覆盖上下文窗口（默认自适应服务端 n_ctx）\n\
                 /budget [N|off] 查看或设置思考预算（llama.cpp 由服务端管理）\n\
                 /maxtokens [N] 查看或设置 QQ/聊天模式单轮回复上限（TUI 自动拉满 n_ctx）\n\
                 /config       查看当前全部配置总览\n\
                 \n\
                 [QQ]\n\
                 /qqadmin <QQ> 添加 QQ 管理员（可操作电脑）\n\
                 /qqgroup <群> 添加允许的群（群成员可闲聊）\n\
                 /qqautonew <N> 每 N 条 QQ 消息自动写群记忆+开新对话（0=关闭）\n\
                 \n\
                 [其他]\n\
                 /skills       已安装技能\n\
                 /install <n>  安装技能（pdf/docx/... 或 URL/路径）\n\
                 /clear        清屏\n\
                 /exit         退出\n\
                 \n\
                 其余输入即对话。输入「搜索 xxx / 查一下 xxx / 抓取 URL / 读取 文件」会直接触发对应工具，\n\
                 无需模型介入。启动时若上次会话有历史会自动开新会话，/use 可切回旧会话".into(),
            );
        }
        "fuckmaster" => {
            match fuck_master::execute_slash(cfg, store.current_id(), rest) {
                Ok(reply) => tui.push_system(reply),
                Err(error) => tui.push_error(error),
            }
        }
        "copy" => {
            let _ = tui.copy_last_assistant();
        }
        "ls" => {
            let mut out = String::from("会话列表:\n");
            for id in store.list() {
                let (msgs, tokens) = session::session_stats(&store.path(&id));
                let mark = if id == store.current_id() { " *" } else { "" };
                out.push_str(&format!("- {id}（{msgs} 条, ~{tokens} tok）{mark}\n"));
            }
            tui.push_system(out);
        }
        "new" => {
            cancel.store(true, Ordering::Relaxed);
            tui.push_system("已中止当前回合生成".into());
            let id = if rest.is_empty() {
                store.new_session_timestamped()
            } else {
                store.new_session(rest)
            };
            tui.push_system(format!("已新建并切换到会话: {id}"));
            let llm2 = llm.clone();
            let ev_tx2 = ev_tx.clone();
            std::thread::spawn(move || {
                let _ = ev_tx2.send(match llm2.clear_kv() {
                    Ok(()) => AgentEvent::Notice("新会话 KV 已清空（模型重载完成）".into()),
                    Err(e) => AgentEvent::Notice(format!("KV 清理跳过: {e}")),
                });
            });
        }
        "use" => match store.switch(rest) {
            Ok(()) => tui.push_system(format!("已切换到会话: {rest}")),
            Err(e) => tui.push_error(e),
        },
        "rm" => match store.delete(rest) {
            Ok(()) => tui.push_system(format!("已删除会话: {rest}")),
            Err(e) => tui.push_error(e),
        },
        "compress" | "compact" => {
            let msgs = store.current().messages;
            let cancel = Arc::new(AtomicBool::new(false));
            let before = compress::approx_total_tokens(&msgs);
            match compress::compact(llm, &msgs, &cancel) {
                Ok(new_hist) => match store.replace_current(&new_hist) {
                    Ok(()) => {
                        let after = compress::approx_total_tokens(&new_hist);
                        tui.push_summary(format!("压缩完成：~{before} tok -> ~{after} tok"));
                    }
                    Err(e) => tui.push_error(e),
                },
                Err(e) => tui.push_error(format!("压缩失败: {e}")),
            }
        }
        "stats" => {
            let msgs = store.current().messages;
            let tokens = compress::approx_total_tokens(&msgs);
            let window = backend::effective_window(
                cfg.provider.ctx_override,
                llm.n_ctx(),
                cfg.provider.ctx_window,
            );
            tui.push_system(format!(
                "上下文: ~{tokens} / {window} tok（{:.1}%），消息 {} 条",
                tokens as f64 * 100.0 / window as f64,
                msgs.len()
            ));
        }
        "model" | "models" => {
            if rest.is_empty() {
                let (ev_tx2, cfg2) = (ev_tx.clone(), cfg.clone());
                std::thread::spawn(move || {
                    let msg = match llm::list_models(&cfg2.provider.base_url, &cfg2.provider.api_key) {
                        Ok(ids) => {
                            let list: Vec<String> = ids.iter().enumerate().map(|(i, id)| format!("  {}  {}", i + 1, id)).collect();
                            format!("服务端可用模型（{} 个）:\n{}\n输入 /models <编号或关键词> 切换", ids.len(), list.join("\n"))
                        }
                        Err(e) => format!("{e}（可用 /model <名称> 直接指定）"),
                    };
                    let _ = ev_tx2.send(AgentEvent::Notice(msg));
                });
            } else {
                let name = match rest.parse::<usize>() {
                    Ok(n) if n >= 1 => match llm::list_models(&cfg.provider.base_url, &cfg.provider.api_key) {
                        Ok(ids) => match ids.get(n - 1) {
                            Some(id) => id.clone(),
                            None => {
                                tui.push_error(format!("编号 {n} 超出范围（共 {} 个模型）", ids.len()));
                                return;
                            }
                        },
                        Err(e) => {
                            tui.push_error(format!("查询模型列表失败: {e}"));
                            return;
                        }
                    },
                    _ => {
                        let kw = rest.to_string();
                        match llm::list_models(&cfg.provider.base_url, &cfg.provider.api_key) {
                            Ok(ids) => {
                                if ids.contains(&kw) {
                                    kw
                                } else {
                                    let hits: Vec<&String> = ids.iter().filter(|id| id.contains(&kw)).collect();
                                    match hits.len() {
                                        1 => hits[0].clone(),
                                        0 => {
                                            tui.push_error(format!("没有找到包含 \"{kw}\" 的模型（/models 查看列表）"));
                                            return;
                                        }
                                        _ => {
                                            tui.push_error(format!("\"{kw}\" 匹配到多个模型: {:?}（/models 查看编号）", hits));
                                            return;
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                tui.push_error(format!("查询模型列表失败: {e}"));
                                return;
                            }
                        }
                    }
                };
                let mut cfg2 = cfg.clone();
                cfg2.provider.model = name.clone();
                cfg2.save();
                llm.set_model(name.clone());
                tui.push_system(format!("模型已切换为 {name}，下一条消息立即生效"));
                let llm2 = llm.clone();
                std::thread::spawn(move || {
                    if let Err(e) = llm2.reload_model() {
                        eprintln!("模型加载提示失败（忽略，请求时会自动加载）: {e}");
                    }
                });
            }
        }
        "setup" => {
            let msg = setup::quick_setup(cfg);
            tui.push_system(msg);
            sync_llm_from_cfg(llm, cfg);
            tui.push_system(server_caps_note(llm));
        }
        "server" => {
            if rest.is_empty() {
                tui.push_system(format!(
                    "模型服务地址: {}\n{}\n用法: /server <地址>（如 http://127.0.0.1:8080/v1）",
                    cfg.provider.base_url,
                    server_caps_note(llm)
                ));
            } else {
                cfg.provider.base_url = rest.to_string();
                cfg.save();
                sync_llm_from_cfg(llm, cfg);
                tui.push_system(format!(
                    "模型服务地址已设为 {}，并已重新探测服务端能力\n{}",
                    cfg.provider.base_url,
                    server_caps_note(llm)
                ));
            }
        }
        "apikey" => {
            if rest.is_empty() {
                let masked = if cfg.provider.api_key.is_empty() {
                    "（未设置，本地无鉴权可保持为空）".to_string()
                } else {
                    let head: String = cfg.provider.api_key.chars().take(4).collect();
                    format!("{head}…（已设置）")
                };
                tui.push_system(format!(
                    "API 密钥: {masked}\n用法: /apikey <密钥>（/apikey clear 清除）"
                ));
            } else if matches!(rest, "clear" | "none" | "-") {
                cfg.provider.api_key.clear();
                cfg.save();
                sync_llm_from_cfg(llm, cfg);
                tui.push_system("API 密钥已清除".to_string());
            } else {
                cfg.provider.api_key = rest.to_string();
                cfg.save();
                sync_llm_from_cfg(llm, cfg);
                tui.push_system("API 密钥已更新，并已重新探测服务端能力".to_string());
            }
        }
        "ctx" => {
            if rest.is_empty() {
                let effective = backend::effective_window(
                    cfg.provider.ctx_override,
                    llm.n_ctx(),
                    cfg.provider.ctx_window,
                );
                let source = if let Some(o) = cfg.provider.ctx_override {
                    format!("手动覆盖 {o}")
                } else if let Some(n) = llm.n_ctx() {
                    format!("自适应（服务端 n_ctx {n}）")
                } else {
                    format!("回退值（非 llama.cpp 后端）")
                };
                tui.push_system(format!(
                    "上下文窗口: {effective} tok（{source}）\n用法: /ctx <N> 手动覆盖（如 65536）；/ctx off 恢复自适应"
                ));
            } else if matches!(rest, "off" | "0" | "auto") {
                cfg.provider.ctx_override = None;
                cfg.save();
                tui.push_system("已恢复自适应上下文窗口（服务端 n_ctx 优先）".to_string());
            } else {
                match rest.parse::<usize>() {
                    Ok(n) if n >= 1024 => {
                        cfg.provider.ctx_override = Some(n);
                        cfg.save();
                        tui.push_system(format!("上下文窗口已手动覆盖为 {n} tok，立即生效"));
                    }
                    _ => tui.push_error(format!(
                        "用法: /ctx <N>（token 数，≥1024，当前有效值 {}）",
                        backend::effective_window(
                            cfg.provider.ctx_override,
                            llm.n_ctx(),
                            cfg.provider.ctx_window,
                        )
                    )),
                }
            }
        }
        "budget" => {
            if rest.is_empty() {
                if llm.is_llamacpp() {
                    tui.push_system(
                        "思考预算由 llama.cpp 服务端管理（--reasoning-budget 启动参数），客户端不发送。\
                         \n可在服务端调整后重启生效"
                            .to_string(),
                    );
                } else {
                    tui.push_system(format!(
                        "思考预算: {}（LM Studio 等支持 thinking 控制的后端生效）\n用法: /budget <N> 设置；/budget off 关闭",
                        cfg.provider
                            .thinking_budget
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "未设置".into())
                    ));
                }
            } else if llm.is_llamacpp() {
                tui.push_error(
                    "思考预算由 llama.cpp 服务端管理（--reasoning-budget），客户端无法逐请求设置".into(),
                );
            } else if matches!(rest, "off" | "0" | "none") {
                cfg.provider.thinking_budget = None;
                cfg.save();
                llm.set_thinking_budget(None);
                tui.push_system("思考预算已关闭".to_string());
            } else {
                match rest.parse::<usize>() {
                    Ok(n) if n >= 128 => {
                        cfg.provider.thinking_budget = Some(n);
                        cfg.save();
                        llm.set_thinking_budget(Some(n));
                        tui.push_system(format!("思考预算已设为 {n} token，下一条消息生效"));
                    }
                    _ => tui.push_error(format!(
                        "用法: /budget <N>（token 数，≥128，当前 {}）",
                        cfg.provider
                            .thinking_budget
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "未设置".into())
                    )),
                }
            }
        }
        "maxtokens" => {
            if rest.is_empty() {
                let tui_cap = llm
                    .max_tokens_for(backend::TokenBudget::TuiMain, cfg.qq.max_tokens)
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "由服务端决定".into());
                let qq_cap = llm
                    .max_tokens_for(backend::TokenBudget::QqMain, cfg.qq.max_tokens)
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "由服务端决定".into());
                let tui_note = if llm.n_ctx().is_some() {
                    "自动拉满服务端 n_ctx"
                } else {
                    "探测不到 n_ctx，不发送"
                };
                tui.push_system(format!(
                    "单轮生成上限:\n  TUI: {tui_cap}（{tui_note}）\n  QQ: {qq_cap}（配置 {}\n用法: /maxtokens <N> 设置 QQ/聊天模式单轮回复上限",
                    cfg.qq.max_tokens
                ));
            } else {
                match rest.parse::<usize>() {
                    Ok(n) if (64..=1_000_000).contains(&n) => {
                        cfg.qq.max_tokens = n;
                        cfg.save();
                        tui.push_system(format!(
                            "QQ/聊天模式单轮回复上限已设为 {n}（TUI 始终自动拉满服务端 n_ctx）"
                        ));
                    }
                    _ => tui.push_error(format!(
                        "用法: /maxtokens <N>（64-1000000，当前 QQ 上限 {}）",
                        cfg.qq.max_tokens
                    )),
                }
            }
        }
        "config" => {
            let masked_key = if cfg.provider.api_key.is_empty() {
                "（未设置）".to_string()
            } else {
                let head: String = cfg.provider.api_key.chars().take(4).collect();
                format!("{head}…")
            };
            let window = backend::effective_window(
                cfg.provider.ctx_override,
                llm.n_ctx(),
                cfg.provider.ctx_window,
            );
            let window_src = if let Some(o) = cfg.provider.ctx_override {
                format!("手动覆盖 {o}")
            } else if let Some(n) = llm.n_ctx() {
                format!("自适应（服务端 n_ctx {n}）")
            } else {
                format!("回退值")
            };
            let tui_cap = llm
                .max_tokens_for(backend::TokenBudget::TuiMain, cfg.qq.max_tokens)
                .map(|v| v.to_string())
                .unwrap_or_else(|| "由服务端决定".into());
            let budget = if llm.is_llamacpp() {
                "由服务端管理".to_string()
            } else {
                cfg.provider
                    .thinking_budget
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "未设置".into())
            };
            let llama_svc = if cfg.llama.auto_start {
                if cfg.llama.service.is_empty() {
                    "自动拉起已开启但未配置服务名！".to_string()
                } else {
                    format!("自动拉起 {}（{s}s）", cfg.llama.service, s = cfg.llama.start_wait_secs)
                }
            } else {
                "未开启自动拉起".to_string()
            };
            tui.push_system(format!(
                "当前配置总览:\n\
                 \n  [模型服务]\n\
                 \x20 地址:   {}\n\
                 \x20 模型:   {}\n\
                 \x20 密钥:   {}\n\
                 \x20 类型:   {}\n\
                 \n  [生成限制]\n\
                 \x20 上下文窗口: {window} tok（{window_src}）\n\
                 \x20 TUI 单轮上限: {tui_cap}\n\
                 \x20 QQ 单轮上限: {}（/maxtokens 调整）\n\
                 \x20 思考预算: {budget}\n\
                 \n  [其他]\n\
                 \x20 工具轮数上限: {}（/tool_times）\n\
                 \x20 请求超时: {}s（/timeout）\n\
                 \x20 原生工具: {}（/fuckloop 相关）\n\
                 \x20 模型服务: {llama_svc}",
                cfg.provider.base_url,
                cfg.provider.model,
                masked_key,
                server_caps_note(llm),
                cfg.qq.max_tokens,
                cfg.tool_times,
                cfg.provider.timeout_secs,
                if cfg.provider.native_tools { "开启" } else { "关闭" },
            ));
        }
        "qqadmin" => {
            if rest.is_empty() {
                tui.push_system(format!(
                    "管理员: {:?}（用法: /qqadmin <QQ号> 添加管理员，可让管理员指挥 agent 操作电脑；非管理员只能闲聊）",
                    cfg.qq.admins
                ));
            } else {
                match rest.parse::<i64>() {
                    Ok(qq) => {
                        let mut cfg2 = cfg.clone();
                        let added = cfg2.qq.add_admin(qq);
                        cfg2.save();
                        tui.push_system(format!(
                            "管理员 {qq} 已{}（私聊白名单同步加入）。重启 QQ 桥接后生效，当前管理员: {:?}",
                            if added { "添加" } else { "存在" },
                            cfg2.qq.admins
                        ));
                    }
                    Err(_) => tui.push_error(format!("QQ 号格式不对: {rest}")),
                }
            }
        }
        "qqgroup" => {
            if rest.is_empty() {
                tui.push_system(format!(
                    "允许的群: {:?}（用法: /qqgroup <群号> 添加群，群内成员可闲聊、仅管理员可操作电脑）",
                    cfg.qq.groups
                ));
            } else {
                match rest.parse::<i64>() {
                    Ok(gid) => {
                        let mut cfg2 = cfg.clone();
                        let added = cfg2.qq.add_group(gid);
                        cfg2.save();
                        tui.push_system(format!(
                            "群 {gid} 已{}。重启 QQ 桥接后生效，允许的群: {:?}",
                            if added { "添加" } else { "存在" },
                            cfg2.qq.groups
                        ));
                    }
                    Err(_) => tui.push_error(format!("群号格式不对: {rest}")),
                }
            }
        }
        "qqautonew" => {
            match rest.parse::<usize>() {
                Ok(n) => {
                    let mut cfg2 = cfg.clone();
                    cfg2.qq.auto_new = n;
                    cfg2.save();
                    tui.push_system(format!(
                        "已设置：每 {n} 条 QQ 消息自动写群记忆并开启新对话（0=关闭）。重启 QQ 桥接后生效"
                    ));
                }
                Err(_) => tui.push_error("用法: /qqautonew <消息数>（如 /qqautonew 5，0 = 关闭）".to_string()),
            }
        }
        "skills" => match skills::op_list_skills(cfg) {
            Ok(s) => tui.push_system(s),
            Err(e) => tui.push_error(e),
        },
        "install" => {
            if rest.is_empty() {
                tui.push_system("用法: /install <技能名>（如 pdf/docx/pptx）或 /install <URL> 或 /install <本地路径>".to_string());
            } else {
                let ev_tx2 = ev_tx.clone();
                let cfg2 = cfg.clone();
                let name = rest.to_string();
                std::thread::spawn(move || {
                    let r = skills::op_install_skill(&serde_json::json!({"name": name}), &cfg2);
                    let _ = ev_tx2.send(match r {
                        Ok(s) => AgentEvent::Notice(s),
                        Err(e) => AgentEvent::Error(e),
                    });
                });
            }
        }
        "reloadmodel" | "reload" => {
            let llm2 = llm.clone();
            let ev_tx2 = ev_tx.clone();
            std::thread::spawn(move || {
                let _ = ev_tx2.send(match llm2.reload_model() {
                    Ok(()) => AgentEvent::Notice("模型已重载".into()),
                    Err(e) => AgentEvent::Error(format!("模型重载失败: {e}")),
                });
            });
        }
        "tool_times" | "tooltimes" => match rest.parse::<usize>() {
            Ok(n) if (1..=200).contains(&n) => {
                cfg.tool_times = n;
                cfg.save();
                tui.push_system(format!("工具调用轮数上限已设为 {n}，下一条消息生效"));
            }
            _ => tui.push_error(format!(
                "用法: /tool_times <N>（1-200，当前上限 {}）",
                cfg.tool_times
            )),
        },
        "timeout" => match rest.parse::<u64>() {
            Ok(n) if (10..=3600).contains(&n) => {
                cfg.provider.timeout_secs = n;
                cfg.save();
                llm.set_timeout(n);
                tui.push_system(format!("请求/流式读取超时已设为 {n} 秒，下一条消息生效"));
            }
            _ => tui.push_error(format!(
                "用法: /timeout <秒>（10-3600，当前 {} 秒）",
                cfg.provider.timeout_secs
            )),
        },
        "commandcountdown" => match rest.parse::<u64>() {
            Ok(n) if (1..=3600).contains(&n) => {
                cfg.command_timeout_secs = n;
                cfg.save();
                tui.push_system(format!("命令默认超时已设为 {n} 秒，下一条消息生效"));
            }
            _ => tui.push_error(format!(
                "用法: /commandcountdown <秒>（1-3600，当前 {} 秒）",
                cfg.command_timeout_secs
            )),
        },
        "autodangerous" => {
            let on = match rest {
                "on" => true,
                "off" => false,
                "" => !perm_auto.load(Ordering::Relaxed),
                other => {
                    tui.push_error(format!("用法: /autodangerous [on|off]（未知参数 {other}）"));
                    return;
                }
            };
            perm_auto.store(on, Ordering::Relaxed);
            if !on {
                if let Ok(mut g) = perm_allowed.lock() {
                    g.clear();
                }
            }
            tui.push_system(if on {
                "autodangerous 已开启：模型可执行任意命令，不再请求批准。".into()
            } else {
                "autodangerous 已关闭：模型执行命令前需您批准（已清除会话内全部同意记忆）。".into()
            });
        }
        "trace" => {
            let on = match rest {
                "on" => true,
                "off" => false,
                "" => !cfg.trace.show_reasoning,
                other => {
                    tui.push_error(format!("用法: /trace [on|off]（未知参数 {other}）"));
                    return;
                }
            };
            cfg.trace.show_reasoning = on;
            cfg.save();
            tui.push_system(format!(
                "思考流实时显示已{}（思考流全文始终写入 ~/.yjlcoder/trace/ 供分析）",
                if on { "开启" } else { "关闭" }
            ));
        }
        "fuckloop" => {
            let on = match rest {
                "on" => true,
                "off" => false,
                "" => {
                    tui.push_system(format!(
                        "fuckloop 当前已{}。用法: /fuckloop on|off",
                        if cfg.fuckloop { "开启" } else { "关闭" }
                    ));
                    return;
                }
                other => {
                    tui.push_error(format!(
                        "用法: /fuckloop on|off（未知参数 {other}）"
                    ));
                    return;
                }
            };
            cfg.fuckloop = on;
            cfg.save();
            tui.push_system(if on {
                "fuckloop 已开启：工具结果后禁止长思考，循环时硬截断并整理；精确 Read 行号直接回答。下一条消息生效。".into()
            } else {
                "fuckloop 已关闭：工具结果后的整理、禁思考、短硬截断和精确 Read 直答均停用，恢复模型原始流程。下一条消息生效。".into()
            });
        }
        "save" => {
            let msgs = store.current().messages;
            let md = session::export_markdown(store.current_id(), &msgs);
            let path = if rest.is_empty() {
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                crate::config::data_dir().join("export").join(format!("{}-{secs}.md", store.current_id()))
            } else {
                std::path::PathBuf::from(rest)
            };
            match (|| -> Result<(), String> {
                if let Some(dir) = path.parent() {
                    if !dir.as_os_str().is_empty() {
                        std::fs::create_dir_all(dir).map_err(|e| format!("创建目录失败: {e}"))?;
                    }
                }
                std::fs::write(&path, md).map_err(|e| format!("写入失败: {e}"))?;
                Ok(())
            })() {
                Ok(()) => tui.push_system(format!(
                    "已保存会话 {}（{} 条消息，含工具日志）→ {}",
                    store.current_id(),
                    msgs.len(),
                    path.display()
                )),
                Err(e) => tui.push_error(format!("保存失败: {e}")),
            }
        },
        "clear" => {
            tui.clear_chat();
            tui.push_system("已清屏（历史仍保留在会话中）".to_string());
        }
        "exit" | "quit" => {
            tui.push_system("bye".to_string());
            std::process::exit(0);
        }
        "" => {}
        other => {
            tui.push_error(format!("未知命令: /{other}（/help 查看）"));
        }
    }
}
#[cfg(test)]
mod turn_queue_tests {
    use super::*;
    #[test]
    fn running_turn_queues_messages_fifo_and_cancel_does_not_release_slot() {
        let mut queue = TurnQueue::default();
        assert_eq!(queue.submit("first".into()), TurnAdmission::Start("first".into()));
        assert_eq!(queue.submit("second".into()), TurnAdmission::Queued(1));
        assert_eq!(queue.submit("third".into()), TurnAdmission::Queued(2));
        assert!(queue.is_active());
        assert_eq!(queue.start_next(), None);
        queue.finish_active();
        assert_eq!(queue.start_next().as_deref(), Some("second"));
        assert!(queue.is_active());
        assert_eq!(queue.start_next(), None);
        queue.finish_active();
        assert_eq!(queue.start_next().as_deref(), Some("third"));
        queue.finish_active();
        assert_eq!(queue.start_next(), None);
        assert!(!queue.is_active());
    }
    #[test]
    fn pop_pending_takes_latest_only() {
        let mut queue = TurnQueue::default();
        assert_eq!(queue.submit("first".into()), TurnAdmission::Start("first".into()));
        assert_eq!(queue.submit("second".into()), TurnAdmission::Queued(1));
        assert_eq!(queue.submit("third".into()), TurnAdmission::Queued(2));
        assert_eq!(queue.pop_pending().as_deref(), Some("third"));
        assert_eq!(queue.pending_len(), 1, "只弹出一条");
        assert!(queue.is_active(), "正在运行的回合不受影响");
        assert_eq!(queue.pop_pending().as_deref(), Some("second"));
        assert_eq!(queue.pop_pending(), None, "队列空了再取返回 None");
        assert_eq!(queue.pending_len(), 0);
    }
    #[test]
    fn autodangerous_toggles_and_clears_whitelist() {
        let tmp = std::env::temp_dir().join(format!("yjlcoder-cmd-{}", std::process::id()));
        let mut store = SessionStore::new(tmp.clone());
        let mut cfg = Config::default();
        let mut llm = Llm::mock();
        let (ev_tx, _ev_rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let perm_auto = Arc::new(AtomicBool::new(false));
        let perm_allowed: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        perm_allowed.lock().unwrap().insert("cargo".to_string());
        let mut tui = Tui::new();
        let mut call = |cmd: &str| {
            handle_command(
                cmd,
                &mut tui,
                &mut store,
                &mut cfg,
                &mut llm,
                &ev_tx,
                &cancel,
                &perm_auto,
                &perm_allowed,
            );
        };
        call("autodangerous");
        assert!(perm_auto.load(Ordering::Relaxed));
        call("autodangerous");
        assert!(!perm_auto.load(Ordering::Relaxed));
        assert!(perm_allowed.lock().unwrap().is_empty(), "关闭时清空白名单");
        call("autodangerous on");
        assert!(perm_auto.load(Ordering::Relaxed));
        call("autodangerous off");
        assert!(!perm_auto.load(Ordering::Relaxed));
        call("autodangerous maybe");
        assert!(!perm_auto.load(Ordering::Relaxed), "未知参数不改状态");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
