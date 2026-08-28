use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::time::Instant;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use crate::tools::AskQuestion;
const C_GREEN: &str = "\x1b[32m";                
const C_ASST: &str = "\x1b[37m";              
const C_USER_TEXT: &str = "\x1b[1;97m";                               
const C_RED: &str = "\x1b[203m";                          
const C_MAG: &str = "\x1b[141m";                              
const C_WARN: &str = "\x1b[220m";                       
const C_DIM: &str = "\x1b[2m";                     
const C_BOLD: &str = "\x1b[1m";       
const C_RESET: &str = "\x1b[0m";
const C_CURSOR: &str = "\x1b[48;5;244;37m";                                  
const C_BG_USER: &str = "\x1b[48;5;237m";                         
const C_BG_CODE: &str = "\x1b[48;5;234m";                                     
const C_GRAY: &str = "\x1b[244m";                
const C_SUGGEST: &str = "\x1b[209m";                                 
const C_BG_ASK_TAB: &str = "\x1b[48;5;173;30m";                        
// 与 Claude Code Best 的 AlternateScreen 相同：在备用屏幕中开启普通鼠标、
// 按钮拖动和 SGR 扩展坐标。这样终端会把滚轮作为独立鼠标事件发送，键盘
// ↑/↓ 仍然是方向键；1002 只在按住按钮时报告移动，避免无意义的悬停刷新。
const ENABLE_MOUSE_TRACKING: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const DISABLE_MOUSE_TRACKING: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChatRole {
    User,
    Assistant,
    Tool,
    ToolResult,
    System,
    Error,
    Warn,
    Summary,
    Reasoning,
}
#[derive(Debug, Clone)]
pub struct ChatLine {
    pub role: ChatRole,
    pub text: String,
    pub pending: bool,
    pub op: Option<String>,
}
impl ChatLine {
    fn new(role: ChatRole, text: String) -> Self {
        ChatLine { role, text, pending: false, op: None }
    }
}
struct RLine {
    shade: Option<&'static str>,
    styled: String,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    WheelUp,
    WheelDown,
    MouseDown { row: usize, col: usize },
    MouseDrag { row: usize, col: usize },
    MouseUp { row: usize, col: usize },
    PasteStart,
    PasteEnd,
    Tab,
    CtrlC,
    CtrlD,
    CtrlL,
    CtrlO,
    Esc,
    Unknown,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Submit(String),
    Command(String),
    AskSubmit(BTreeMap<String, String>),
    ConfigSubmit(BTreeMap<String, String>),
    Cancel,
    RetrieveQueued,
    PermSubmit(crate::tools::PermDecisionKind),
    Quit,
    Redraw,
    None,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AskKind {
    Agent,
    ProviderConfig,
}
#[derive(Debug, Clone)]
struct AskUi {
    questions: Vec<AskQuestion>,
    current: usize,
    answers: BTreeMap<String, String>,
    focus: usize,
    checked: BTreeSet<usize>,
    notice: Option<String>,
}
struct PermUi {
    cmd: String,
    cmd_kind: String,
    focus: usize,
}
#[derive(Debug, Clone)]
struct CommandCompletion {
    matches: Vec<&'static str>,
    next_index: usize,
    token_start: usize,
    token_end: usize,
    expected_input: String,
    expected_cursor: usize,
}
struct CommandToken {
    start: usize,
    end: usize,
    prefix: String,
}
struct AskPanel {
    lines: Vec<String>,
    cursor: Option<(usize, usize, char)>,
}
type AskPanelRow = (String, Option<String>, Option<(usize, char)>);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MascotState {
    Idle,
    Thinking,
    Tool,
    Asking,
    Success,
    Angry,
    Frantic,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MascotEvent {
    Begin,
    Progress,
    ToolStart,
    ToolOk,
    ToolError,
    Ask,
    Answer,
    Complete,
    Warning,
    Bug,
    Cancel,
}
impl MascotState {
    fn transition(self, event: MascotEvent) -> Self {
        match event {
            MascotEvent::Begin => MascotState::Thinking,
            MascotEvent::Progress if matches!(self, MascotState::Angry | MascotState::Frantic) => self,
            MascotEvent::Progress => MascotState::Thinking,
            MascotEvent::ToolStart => MascotState::Tool,
            MascotEvent::ToolOk => MascotState::Thinking,
            MascotEvent::ToolError => MascotState::Angry,
            MascotEvent::Ask => MascotState::Asking,
            MascotEvent::Answer => MascotState::Thinking,
            MascotEvent::Complete => MascotState::Success,
            MascotEvent::Warning if self == MascotState::Asking => MascotState::Asking,
            MascotEvent::Warning => MascotState::Angry,
            MascotEvent::Bug => MascotState::Frantic,
            MascotEvent::Cancel => MascotState::Idle,
        }
    }
    fn label(self) -> &'static str {
        match self {
            MascotState::Idle => "READY",
            MascotState::Thinking => "THINK",
            MascotState::Tool => "TOOL",
            MascotState::Asking => "ASK",
            MascotState::Success => "DONE",
            MascotState::Angry => "ANGRY",
            MascotState::Frantic => "BUG!",
        }
    }
    fn color(self) -> &'static str {
        match self {
            MascotState::Idle | MascotState::Success => C_GREEN,
            MascotState::Thinking => C_MAG,
            MascotState::Tool | MascotState::Asking => C_WARN,
            MascotState::Angry | MascotState::Frantic => C_RED,
        }
    }
}
pub struct Tui {
    w: usize,
    h: usize,
    header: String,
    ctx_used: usize,
    ctx_window: usize,
    ctx_exact: bool,
    last_exact_prompt_tokens: Option<usize>,
    queued_count: usize,
    pricing: crate::config::Pricing,
    stream_started: Option<Instant>,
    live_committed_usage: crate::llm::Usage,
    live_request_usage: crate::llm::Usage,
    live_request_exact: bool,
    live_generated_bytes: usize,
    chat: Vec<ChatLine>,
    session_usage: crate::llm::Usage,
    scroll: usize,
    follow: bool,
    input: String,
    cursor: usize,
    history: Vec<String>,
    hist_pos: Option<usize>,
    streaming: bool,
    prompt: String,
    hint: String,
    input_scroll: usize,
    commands: &'static [(&'static str, &'static str)],
    command_completion: Option<CommandCompletion>,
    asking: Option<AskUi>,
    ask_kind: AskKind,
    perm_prompt: Option<PermUi>,
    mascot_state: MascotState,
    last_tool_op: Option<String>,
    tool_progress_index: Option<usize>,
    last_frame: String,
    in_paste: bool,
    paste_prev_cr: bool,
    sel_anchor: Option<(usize, usize)>,
    sel_cur: Option<(usize, usize)>,
    copied_notice: Option<String>,
    sel_rows: Vec<String>,
    sel_start_row: usize,
    clipboard_lease: Option<crate::clipboard_copy::ClipboardLease>,
}
pub struct RawGuard {
    orig: libc::termios,
}
impl RawGuard {
    pub fn enable() -> Self {
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        unsafe {
            libc::tcgetattr(0, &mut orig);
        }
        let mut raw = orig;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &raw);
        }
        RawGuard { orig }
    }
}
impl Drop for RawGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.orig);
        }
    }
}
fn term_size() -> (usize, usize) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            (ws.ws_col as usize, ws.ws_row as usize)
        } else {
            (80, 24)
        }
    }
}
impl Default for Tui {
    fn default() -> Self {
        Self::new()
    }
}
impl Tui {
    pub fn new() -> Self {
        let (w, h) = term_size();
        Tui {
            w,
            h,
            header: String::new(),
            ctx_used: 0,
            ctx_window: 0,
            ctx_exact: false,
            last_exact_prompt_tokens: None,
            queued_count: 0,
            pricing: crate::config::Pricing::default(),
            stream_started: None,
            live_committed_usage: crate::llm::Usage::default(),
            live_request_usage: crate::llm::Usage::default(),
            live_request_exact: false,
            live_generated_bytes: 0,
            chat: Vec::new(),
            session_usage: crate::llm::Usage::default(),
            scroll: 0,
            follow: true,
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            hist_pos: None,
            streaming: false,
            prompt: "❯ ".into(),
            hint: "  ⏸ 本地模式 · /help 查看命令 · Esc 中断 · 滚轮滚动对话 · ↑ 撤销排队/历史".into(),
            input_scroll: 0,
            commands: &[],
            command_completion: None,
            asking: None,
            ask_kind: AskKind::Agent,
            perm_prompt: None,
            mascot_state: MascotState::Idle,
            last_tool_op: None,
            tool_progress_index: None,
            last_frame: String::new(),
            in_paste: false,
            paste_prev_cr: false,
            sel_anchor: None,
            sel_cur: None,
            copied_notice: None,
            sel_rows: Vec::new(),
            sel_start_row: 0,
            clipboard_lease: None,
        }
    }
    pub fn ask(&mut self, questions: Vec<AskQuestion>) {
        self.ask_kind = AskKind::Agent;
        self.open_ask(questions);
    }
    pub fn open_provider_setup(&mut self, cfg: &crate::config::Config) {
        self.ask_kind = AskKind::ProviderConfig;
        self.open_ask(crate::setup::provider_questions(cfg));
    }
    fn open_ask(&mut self, questions: Vec<AskQuestion>) {
        self.command_completion = None;
        self.input.clear();
        self.cursor = 0;
        self.asking = Some(AskUi {
            questions,
            current: 0,
            answers: BTreeMap::new(),
            focus: 0,
            checked: BTreeSet::new(),
            notice: None,
        });
        self.mascot_event(MascotEvent::Ask);
    }
    pub fn finish_ask(&mut self) {
        self.asking = None;
        self.ask_kind = AskKind::Agent;
        self.command_completion = None;
        self.input.clear();
        self.cursor = 0;
        if self.mascot_state == MascotState::Asking {
            self.mascot_event(MascotEvent::Answer);
        }
    }
    pub fn is_asking(&self) -> bool {
        self.asking.is_some()
    }
    pub fn open_perm_prompt(&mut self, req: crate::tools::PermRequest) {
        self.command_completion = None;
        self.input.clear();
        self.cursor = 0;
        self.perm_prompt = Some(PermUi { cmd: req.cmd, cmd_kind: req.cmd_kind, focus: 0 });
    }
    pub fn finish_perm_prompt(&mut self) {
        self.perm_prompt = None;
    }
    pub fn is_perm_prompt(&self) -> bool {
        self.perm_prompt.is_some()
    }
    fn handle_perm_key(&mut self, key: Key) -> Action {
        match key {
            Key::Up => {
                if let Some(ui) = self.perm_prompt.as_mut() {
                    ui.focus = (ui.focus + 3) % 4;
                }
                Action::None
            }
            Key::Down | Key::Tab => {
                if let Some(ui) = self.perm_prompt.as_mut() {
                    ui.focus = (ui.focus + 1) % 4;
                }
                Action::None
            }
            Key::Enter => {
                let kind = match self.perm_prompt.as_ref().map(|ui| ui.focus) {
                    Some(0) => crate::tools::PermDecisionKind::Yes,
                    Some(1) => crate::tools::PermDecisionKind::AlwaysAllow,
                    Some(2) => crate::tools::PermDecisionKind::No,
                    _ => crate::tools::PermDecisionKind::AutoEnable,
                };
                Action::PermSubmit(kind)
            }
            Key::Esc => Action::PermSubmit(crate::tools::PermDecisionKind::No),
            Key::Char('1') => Action::PermSubmit(crate::tools::PermDecisionKind::Yes),
            Key::Char('2') => Action::PermSubmit(crate::tools::PermDecisionKind::AlwaysAllow),
            Key::Char('3') => Action::PermSubmit(crate::tools::PermDecisionKind::No),
            Key::Char('4') => Action::PermSubmit(crate::tools::PermDecisionKind::AutoEnable),
            _ => Action::None,
        }
    }
    pub fn set_commands(&mut self, cmds: &'static [(&'static str, &'static str)]) {
        self.command_completion = None;
        self.commands = cmds;
    }
    pub fn enter(&mut self) -> RawGuard {
        let g = RawGuard::enable();
        self.resize();
        print!("\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[?2004h{ENABLE_MOUSE_TRACKING}");
        self.last_frame.clear();
        let _ = std::io::stdout().flush();
        g
    }
    pub fn exit(&mut self) {
        print!("{DISABLE_MOUSE_TRACKING}\x1b[?2004l\x1b[?25h\x1b[?7h\x1b[?1049l");
        let _ = std::io::stdout().flush();
    }
    fn resize(&mut self) {
        let (w, h) = term_size();
        if w != self.w || h != self.h {
            self.w = w;
            self.h = h;
            self.scroll = self
                .scroll
                .min(self.chat_line_count().saturating_sub(self.chat_height().saturating_sub(1)));
        }
    }
    pub fn set_header(&mut self, s: String) {
        self.header = s;
    }
    pub fn set_ctx_estimate(&mut self, used: usize, window: usize) {
        self.ctx_window = window;
        if !self.streaming || !self.ctx_exact {
            self.ctx_used = used;
            self.ctx_exact = false;
        }
    }
    pub fn last_exact_prompt_tokens(&self) -> Option<usize> {
        self.last_exact_prompt_tokens
    }
    pub fn set_queued_count(&mut self, count: usize) {
        self.queued_count = count;
    }
    pub fn set_pricing(&mut self, pricing: crate::config::Pricing) {
        self.pricing = pricing;
    }
    pub fn token_progress(&mut self, progress: crate::llm::TokenProgress) {
        if progress.exact {
            if !self.live_request_exact {
                add_usage(&mut self.live_committed_usage, progress.usage);
            }
            self.live_request_usage = crate::llm::Usage::default();
            self.live_request_exact = true;
            self.ctx_used = progress.usage.prompt_tokens;
            self.ctx_exact = true;
            if progress.usage.prompt_tokens > 0 {
                self.last_exact_prompt_tokens = Some(progress.usage.prompt_tokens);
            }
        } else {
            self.live_request_usage = progress.usage;
            self.live_request_exact = false;
            self.ctx_used = progress.usage.prompt_tokens;
            self.ctx_exact = false;
        }
    }
    pub fn reasoning_token_delta(&mut self, text: &str) {
        self.live_generated_bytes = self.live_generated_bytes.saturating_add(text.len());
    }
    pub fn retrieve_queued(&mut self, text: String) {
        self.queued_count = self.queued_count.saturating_sub(1);
        self.command_completion = None;
        self.input = text;
        self.cursor = self.input.chars().count();
        self.hist_pos = None;
        self.hint = "已取回最近一条排队消息，编辑后回车重新发送".into();
    }
    pub fn push_user(&mut self, text: String) {
        self.seal_reasoning();
        self.mascot_event(MascotEvent::Begin);
        self.chat.push(ChatLine::new(ChatRole::User, text));
        self.follow = true;
        self.scroll = 0;
    }
    pub fn begin_assistant(&mut self) {
        self.seal_reasoning();
        self.streaming = true;
        self.stream_started = Some(Instant::now());
        self.live_committed_usage = crate::llm::Usage::default();
        self.live_request_usage = crate::llm::Usage::default();
        self.live_request_exact = false;
        self.live_generated_bytes = 0;
        self.ctx_exact = false;
        self.mascot_event(MascotEvent::Progress);
        self.chat.push(ChatLine { role: ChatRole::Assistant, text: String::new(), pending: true, op: None });
        if self.follow {
            self.scroll = 0;
        }
    }
    pub fn assistant_delta(&mut self, d: &str) {
        self.seal_reasoning();
        self.live_generated_bytes = self.live_generated_bytes.saturating_add(d.len());
        self.mascot_event(MascotEvent::Progress);
        if let Some(last) = self.chat.last_mut() {
            if last.pending {
                last.text.push_str(d);
                return;
            }
        }
        self.chat.push(ChatLine { role: ChatRole::Assistant, text: d.to_string(), pending: true, op: None });
    }
    pub fn end_assistant(&mut self, full: &str) {
        self.streaming = false;
        self.stream_started = None;
        self.mascot_event(if full.trim().is_empty() {
            MascotEvent::Cancel
        } else {
            MascotEvent::Complete
        });
        if let Some(last) = self.chat.last_mut() {
            if last.pending {
                last.text = full.to_string();
                last.pending = false;
                return;
            }
        }
        self.chat.push(ChatLine::new(ChatRole::Assistant, full.to_string()));
        if self.follow {
            self.scroll = 0;
        }
    }
    pub fn end_assistant_with_metrics(
        &mut self,
        full: &str,
        usage: crate::llm::Usage,
        timings: crate::llm::Timings,
    ) {
        self.end_assistant(full);
        if full.trim().is_empty() {
            return;
        }
        let speed = if timings.predicted_n > 0.0 && timings.predicted_ms > 0.0 {
            format!(
                " · {:.1} tok/s",
                timings.predicted_n / (timings.predicted_ms / 1000.0)
            )
        } else {
            String::new()
        };
        // Bug4 修复：两套口径分开标注，避免“~90% 估计”和真实服务端命中率交替展示造成误读
        let cache = if let Some(percent) = usage.server_cache_percent() {
            format!(
                " · 服务端缓存 命中率 {percent:.1}%（读 {} · 未命中/写 {}）",
                usage.cache_read_tokens, usage.cache_miss_tokens
            )
        } else if usage.cache_prefix_hits + usage.cache_prefix_misses > 0 {
            format!(
                " · 前缀稳定估计 {}/{} 次（最长 {} 条消息）",
                usage.cache_prefix_hits,
                usage.cache_prefix_hits + usage.cache_prefix_misses,
                usage.cache_prefix_messages
            )
        } else {
            String::new()
        };
        let reasoning = if usage.reasoning_tokens > 0 {
            format!(" · 其中思考 {}", usage.reasoning_tokens)
        } else {
            String::new()
        };
        // 累计成本：把本轮 usage 累加到整个会话，显示“本轮 + 窗口累计”两笔
        add_usage(&mut self.session_usage, usage);
        let this_cost = self
            .pricing
            .estimate(
                usage.prompt_tokens,
                usage.cache_read_tokens,
                usage.cache_miss_tokens,
                usage.completion_tokens,
            )
            .map(|value| format!(" · 本轮 {}{}", self.pricing.currency, format_cost(value)))
            .unwrap_or_default();
        let total = self.session_usage;
        let acc_cost = self
            .pricing
            .estimate(
                total.prompt_tokens,
                total.cache_read_tokens,
                total.cache_miss_tokens,
                total.completion_tokens,
            )
            .map(|value| format!(" · 累计 {}{}", self.pricing.currency, format_cost(value)))
            .unwrap_or_default();
        if usage.prompt_tokens > 0 || usage.completion_tokens > 0 || !speed.is_empty() || !cache.is_empty() {
            self.push_summary(format!(
                "▸ ↑ 上传 {} token · ↓ 写入 {} token{reasoning}{speed}{cache}{this_cost}{acc_cost}",
                usage.prompt_tokens, usage.completion_tokens
            ));
        }
    }
    pub fn push_tool(&mut self, op: &str, args: &str) {
        self.seal_reasoning();
        self.mascot_event(MascotEvent::ToolStart);
        let (canonical, display, detail) = tool_display(op, args);
        self.last_tool_op = Some(canonical);
        self.tool_progress_index = None;
        self.chat.push(ChatLine {
            role: ChatRole::Tool,
            text: format!("{display}({detail})"),
            pending: false,
            op: Some(display),
        });
        if self.follow {
            self.scroll = 0;
        }
    }
    pub fn push_tool_progress(&mut self, progress: &crate::tools::ToolProgress) {
        self.mascot_event(MascotEvent::ToolStart);
        let clean = strip_ansi(&progress.output);
        let lines: Vec<&str> = clean.lines().filter(|line| !line.trim().is_empty()).collect();
        let visible = lines.len().min(5);
        let tail = lines[lines.len().saturating_sub(visible)..].join("\n");
        let size = human_bytes(progress.total_bytes);
        let status = if progress.total_lines == 0 {
            format!("Running… {}s · {size}", progress.elapsed_secs)
        } else {
            format!("Running… {}s · {} lines · {size}", progress.elapsed_secs, progress.total_lines)
        };
        let text = if tail.is_empty() {
            status
        } else {
            format!("{tail}\n{status}")
        };
        if let Some(index) = self.tool_progress_index {
            if let Some(line) = self.chat.get_mut(index) {
                line.text = text;
                line.pending = true;
                if self.follow {
                    self.scroll = 0;
                }
                return;
            }
        }
        let index = self.chat.len();
        self.chat.push(ChatLine {
            role: ChatRole::ToolResult,
            text,
            pending: true,
            op: None,
        });
        self.tool_progress_index = Some(index);
        if self.follow {
            self.scroll = 0;
        }
    }
    pub fn push_tool_result(&mut self, result: &str) {
        self.seal_reasoning();
        self.mascot_event(if tool_result_failed(result) {
            MascotEvent::ToolError
        } else {
            MascotEvent::ToolOk
        });
        let op = self.last_tool_op.take();
        let clean_result = strip_ansi(result);
        let text = if matches!(op.as_deref(), Some("readline" | "listdir")) {
            clean_result
        } else {
            let snippet: String = clean_result.chars().take(1_200).collect();
            let more = if clean_result.chars().count() > 1_200 {
                format!("\n…共 {} 字符", clean_result.chars().count())
            } else {
                String::new()
            };
            format!("{snippet}{more}")
        };
        if let Some(index) = self.tool_progress_index.take() {
            if let Some(line) = self.chat.get_mut(index) {
                *line = ChatLine::new(ChatRole::ToolResult, text);
                if self.follow {
                    self.scroll = 0;
                }
                return;
            }
        }
        self.chat.push(ChatLine::new(ChatRole::ToolResult, text));
        if self.follow {
            self.scroll = 0;
        }
    }
    pub fn push_system(&mut self, text: String) {
        self.seal_reasoning();
        self.chat.push(ChatLine::new(ChatRole::System, text));
        if self.follow {
            self.scroll = 0;
        }
    }
    pub fn push_error(&mut self, text: String) {
        self.seal_reasoning();
        self.mascot_event(MascotEvent::Bug);
        self.chat.push(ChatLine::new(ChatRole::Error, text));
        if self.follow {
            self.scroll = 0;
        }
    }
    pub fn push_warn(&mut self, text: String) {
        self.seal_reasoning();
        self.mascot_event(MascotEvent::Warning);
        self.chat.push(ChatLine::new(ChatRole::Warn, text));
        if self.follow {
            self.scroll = 0;
        }
    }
    // bug: 垃圾 token 检测已禁用，暂无用例，保留供后续告警功能使用
    #[allow(dead_code)]
    pub fn push_bug_warn(&mut self, text: String) {
        self.push_warn(text);
        self.mascot_event(MascotEvent::Bug);
    }
    pub fn push_reasoning(&mut self, text: String) {
        self.mascot_event(MascotEvent::Progress);
        if let Some(last) = self.chat.last_mut() {
            if last.role == ChatRole::Reasoning && last.pending {
                last.text.push_str(&text);
                return;
            }
        }
        self.chat.push(ChatLine { role: ChatRole::Reasoning, text, pending: true, op: None });
        if self.follow {
            self.scroll = 0;
        }
    }
    fn seal_reasoning(&mut self) {
        if let Some(last) = self.chat.last_mut() {
            if last.role == ChatRole::Reasoning && last.pending {
                last.pending = false;
            }
        }
    }
    pub fn push_summary(&mut self, text: String) {
        self.seal_reasoning();
        self.chat.push(ChatLine::new(ChatRole::Summary, text));
        if self.follow {
            self.scroll = 0;
        }
    }
    pub fn clear_chat(&mut self) {
        self.chat.clear();
        self.last_tool_op = None;
        self.tool_progress_index = None;
        self.session_usage = crate::llm::Usage::default();
        self.scroll = 0;
        self.mascot_event(MascotEvent::Cancel);
    }
    pub fn is_streaming(&self) -> bool {
        self.streaming
    }
    pub fn cancel_streaming(&mut self) {
        self.streaming = false;
        self.mascot_event(MascotEvent::Cancel);
        if let Some(last) = self.chat.last_mut() {
            if last.pending {
                last.pending = false;
            }
        }
    }
    fn mascot_event(&mut self, event: MascotEvent) {
        self.mascot_state = self.mascot_state.transition(event);
    }
    pub fn submit(&mut self) -> Option<Action> {
        if self.asking.is_some() {
            return Some(self.submit_ask());
        }
        self.command_completion = None;
        let text = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.hist_pos = None;
        if text.trim().is_empty() {
            return Some(Action::None);
        }
        self.history.push(text.clone());
        if self.history.len() > 200 {
            self.history.remove(0);
        }
        if let Some(cmd) = text.strip_prefix('/') {
            Some(Action::Command(cmd.trim().to_string()))
        } else {
            Some(Action::Submit(text))
        }
    }
    fn submit_ask(&mut self) -> Action {
        let Some((focus, option_count, multi_select)) = self.asking.as_ref().and_then(|asking| {
            let question = asking.questions.get(asking.current)?;
            Some((asking.focus, question.options.len(), question.multi_select))
        }) else {
            return Action::None;
        };
        let other_index = option_count;
        if multi_select {
            if focus == other_index + 1 {
                return self.submit_multi_ask();
            }
            if focus == other_index && self.input.trim().is_empty() {
                self.set_ask_notice("请先在 Other 中输入内容");
                return Action::None;
            }
            if let Some(asking) = self.asking.as_mut() {
                if !asking.checked.remove(&focus) {
                    asking.checked.insert(focus);
                }
                asking.notice = None;
            }
            return Action::None;
        }
        let answer = if focus < option_count {
            self.asking
                .as_ref()
                .and_then(|asking| asking.questions.get(asking.current))
                .and_then(|question| question.options.get(focus))
                .map(|option| option.label.clone())
        } else {
            let custom = self.input.trim();
            if custom.is_empty() {
                self.set_ask_notice("请在 Other 中输入内容");
                None
            } else {
                Some(custom.to_string())
            }
        };
        match answer {
            Some(answer) => self.commit_ask_answer(answer),
            None => Action::None,
        }
    }
    fn submit_multi_ask(&mut self) -> Action {
        let Some(asking) = self.asking.as_ref() else { return Action::None };
        let Some(question) = asking.questions.get(asking.current) else { return Action::None };
        let other_index = question.options.len();
        let mut answers = Vec::new();
        for index in &asking.checked {
            if let Some(option) = question.options.get(*index) {
                answers.push(option.label.clone());
            } else if *index == other_index && !self.input.trim().is_empty() {
                answers.push(self.input.trim().to_string());
            }
        }
        if answers.is_empty() {
            self.set_ask_notice("请至少选择一项");
            return Action::None;
        }
        self.commit_ask_answer(answers.join(", "))
    }
    fn commit_ask_answer(&mut self, answer: String) -> Action {
        self.command_completion = None;
        self.input.clear();
        self.cursor = 0;
        self.hist_pos = None;
        let (header, completed) = {
            let asking = self.asking.as_mut().expect("提问状态刚确认存在");
            let question = &asking.questions[asking.current];
            let header = question.header.clone();
            asking.answers.insert(question.question.clone(), answer.clone());
            asking.current += 1;
            asking.focus = if answer == "DeepSeek"
                && asking
                    .questions
                    .get(asking.current)
                    .is_some_and(|next| next.header == "API Key")
            {
                asking.questions[asking.current].options.len()
            } else {
                0
            };
            asking.checked.clear();
            asking.notice = None;
            (header, asking.current >= asking.questions.len())
        };
        let visible_answer = if self.ask_kind == AskKind::ProviderConfig
            && header == "API Key"
            && !matches!(
                answer.as_str(),
                "保留当前密钥" | "清除密钥" | "本地服务无需密钥"
            )
        {
            "已安全填写（不回显）"
        } else {
            &answer
        };
        self.push_user(format!("[{header}] {visible_answer}"));
        if completed {
            let answers = self.asking.as_ref().map(|asking| asking.answers.clone()).unwrap_or_default();
            match self.ask_kind {
                AskKind::Agent => Action::AskSubmit(answers),
                AskKind::ProviderConfig => Action::ConfigSubmit(answers),
            }
        } else {
            self.mascot_event(MascotEvent::Ask);
            Action::None
        }
    }
    fn set_ask_notice(&mut self, notice: &str) {
        if let Some(asking) = self.asking.as_mut() {
            asking.notice = Some(notice.to_string());
        }
    }
    fn ask_focus_is_other(&self) -> bool {
        self.asking.as_ref().is_some_and(|asking| {
            asking
                .questions
                .get(asking.current)
                .is_some_and(|question| asking.focus == question.options.len())
        })
    }
    fn move_ask_focus(&mut self, delta: i32) {
        let Some(asking) = self.asking.as_mut() else { return };
        let Some(question) = asking.questions.get(asking.current) else { return };
        let last = question.options.len() + usize::from(question.multi_select);
        asking.focus = if delta < 0 {
            asking.focus.saturating_sub(1)
        } else {
            (asking.focus + 1).min(last)
        };
        asking.notice = None;
    }
    fn handle_ask_key(&mut self, key: Key) -> Action {
        match key {
            Key::Enter => self.submit_ask(),
            Key::Up => {
                self.move_ask_focus(-1);
                Action::None
            }
            Key::Down | Key::Tab => {
                self.move_ask_focus(1);
                Action::None
            }
            Key::Backspace if self.ask_focus_is_other() => {
                if self.cursor > 0 {
                    let end = self.cursor_byte();
                    let start = self.input[..end].char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                    self.input.replace_range(start..end, "");
                    self.cursor -= 1;
                }
                self.sync_other_selection();
                Action::None
            }
            Key::Delete if self.ask_focus_is_other() => {
                let byte = self.cursor_byte();
                if byte < self.input.len() {
                    self.input.remove(byte);
                }
                self.sync_other_selection();
                Action::None
            }
            Key::Left if self.ask_focus_is_other() => {
                self.cursor = self.cursor.saturating_sub(1);
                Action::None
            }
            Key::Right if self.ask_focus_is_other() => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
                Action::None
            }
            Key::Home if self.ask_focus_is_other() => {
                self.cursor = 0;
                Action::None
            }
            Key::End if self.ask_focus_is_other() => {
                self.cursor = self.input.chars().count();
                Action::None
            }
            Key::Char(c) if self.ask_focus_is_other() => {
                let c = if c == '\n' { ' ' } else { c };
                self.input.insert(self.cursor_byte(), c);
                self.cursor += 1;
                self.sync_other_selection();
                Action::None
            }
            Key::Char(' ') => {
                let is_multi = self.asking.as_ref().and_then(|asking| {
                    asking.questions.get(asking.current).map(|question| question.multi_select)
                }).unwrap_or(false);
                if is_multi {
                    self.submit_ask()
                } else {
                    Action::None
                }
            }
            Key::Char(c) if c.is_ascii_digit() && c != '0' => {
                let index = c.to_digit(10).unwrap_or(0) as usize - 1;
                let Some(option_count) = self.asking.as_ref().and_then(|asking| {
                    let question = asking.questions.get(asking.current)?;
                    Some(question.options.len())
                }) else {
                    return Action::None;
                };
                if index > option_count {
                    return Action::None;
                }
                if let Some(asking) = self.asking.as_mut() {
                    asking.focus = index;
                    asking.notice = None;
                }
                if index < option_count {
                    self.submit_ask()
                } else {
                    Action::None
                }
            }
            Key::PageUp | Key::WheelUp => {
                self.scroll += self.chat_height().saturating_sub(1).max(1);
                Action::None
            }
            Key::PageDown | Key::WheelDown => {
                self.scroll = self.scroll.saturating_sub(self.chat_height().saturating_sub(1).max(1));
                Action::None
            }
            Key::Esc => Action::Cancel,
            Key::CtrlD => Action::Quit,
            Key::CtrlL => Action::Redraw,
            _ => Action::None,
        }
    }
    fn sync_other_selection(&mut self) {
        let has_text = !self.input.trim().is_empty();
        if let Some(asking) = self.asking.as_mut() {
            let Some(question) = asking.questions.get(asking.current) else { return };
            if question.multi_select {
                let other = question.options.len();
                if has_text {
                    asking.checked.insert(other);
                } else {
                    asking.checked.remove(&other);
                }
            }
            asking.notice = None;
        }
    }
    fn cursor_byte(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }
    fn selection_bounds(&self) -> Option<((usize, usize), (usize, usize))> {
        let (Some(anchor), Some(focus)) = (self.sel_anchor, self.sel_cur) else {
            return None;
        };
        Some(if anchor <= focus { (anchor, focus) } else { (focus, anchor) })
    }
    fn extract_selection(&self) -> String {
        let Some((start, end)) = self.selection_bounds() else {
            return String::new();
        };
        let mut out: Vec<String> = Vec::new();
        for (i, line) in self.sel_rows.iter().enumerate() {
            let row = self.sel_start_row + i;                
            if row < start.0 || row > end.0 {
                continue;
            }
            let cols = if start.0 == end.0 {
                (start.1, end.1)
            } else if row == start.0 {
                (start.1, usize::MAX)
            } else if row == end.0 {
                (0, end.1)
            } else {
                (0, usize::MAX)
            };
            let s: String = line
                .chars()
                .scan(0usize, |vis, ch| {
                    let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
                    let keep = *vis <= cols.1 && *vis + cw > cols.0;
                    *vis += cw;
                    Some((keep, ch))
                })
                .filter(|(keep, _)| *keep)
                .map(|(_, ch)| ch)
                .collect();
            out.push(s.trim_end().to_string());
        }
        out.join("\n").trim_matches('\n').to_string()
    }
    fn apply_selection_highlight(&self, styled: &str, row_abs: usize) -> String {
        let Some((start, end)) = self.selection_bounds() else {
            return styled.to_string();
        };
        if row_abs < start.0 || row_abs > end.0 {
            return styled.to_string();
        }
        let cols = if start.0 == end.0 {
            (start.1, end.1)
        } else if row_abs == start.0 {
            (start.1, usize::MAX)
        } else if row_abs == end.0 {
            (0, end.1)
        } else {
            (0, usize::MAX)
        };
        let mut out = String::with_capacity(styled.len() + 16);
        let mut vis = 0usize;
        let mut highlighted = false;
        let mut chars = styled.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                out.push(ch);
                while let Some(&n) = chars.peek() {
                    out.push(n);
                    chars.next();
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
            if !highlighted && vis <= cols.0 && vis + cw > cols.0 {
                out.push_str("\x1b[7m");             
                highlighted = true;
            }
            out.push(ch);
            vis += cw;
            if highlighted && vis > cols.1 {
                out.push_str("\x1b[27m");
                highlighted = false;
            }
        }
        if highlighted {
            out.push_str("\x1b[27m");
        }
        out
    }
    fn mouse_selection_point(&self, row: usize, col: usize, clamp: bool) -> Option<(usize, usize)> {
        if self.sel_rows.is_empty() {
            return None;
        }
        let first = self.sel_start_row;
        let last = first + self.sel_rows.len() - 1;
        let row = if clamp {
            row.clamp(first, last)
        } else if (first..=last).contains(&row) {
            row
        } else {
            return None;
        };
        // SGR 鼠标坐标从 1 开始；内部文本列从 0 开始。
        Some((row, col.saturating_sub(1)))
    }
    fn finish_mouse_selection(&mut self, row: usize, col: usize) -> Action {
        let Some(point) = self.mouse_selection_point(row, col, true) else {
            return Action::None;
        };
        self.sel_cur = Some(point);
        let text = self.extract_selection();
        if text.is_empty() {
            self.sel_anchor = None;
            self.sel_cur = None;
            return Action::Redraw;
        }
        if cfg!(test) {
            self.copied_notice = Some(format!("已复制选区 · {} 字符", text.chars().count()));
            return Action::Redraw;
        }
        match crate::clipboard_copy::copy_to_clipboard(&text) {
            Ok(lease) => {
                self.clipboard_lease = lease;
                self.copied_notice = Some(format!("已复制选区 · {} 字符", text.chars().count()));
            }
            Err(error) => self.copied_notice = Some(format!("复制失败: {error}")),
        }
        Action::Redraw
    }
    #[allow(dead_code)]
    fn copy_to_clipboard(text: &str) -> bool {
        if cfg!(test) {
            return false;
        }
        use std::io::Write as _;
        use std::process::{Command, Stdio};
        let has = |name: &str| {
            Command::new("sh")
                .args(["-c", &format!("command -v {name}")])
                .stdout(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        let is_wayland = std::env::var("XDG_SESSION_TYPE").map(|t| t == "wayland").unwrap_or(false)
            || std::env::var("WAYLAND_DISPLAY").is_ok();
        let feed = |mut cmd: Command| -> bool {
            let Ok(mut child) = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).spawn() else {
                return false;
            };
            let ok = child.stdin.take().is_some_and(|mut s| {
                s.write_all(text.as_bytes()).is_ok() && s.flush().is_ok()
            });
            if !ok {
                return false;
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return status.success(),
                    Ok(None) if std::time::Instant::now() > deadline => {
                        let _ = child.kill();
                        return true;
                    }
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                    Err(_) => return false,
                }
            }
        };
        if is_wayland && has("wl-copy") {
            return feed(Command::new("wl-copy"));
        }
        if has("xsel") {
            let mut cmd = Command::new("xsel");
            cmd.arg("-b");                     
            return feed(cmd);
        }
        if has("xclip") {
            let mut cmd = Command::new("xclip");
            cmd.args(["-selection", "clipboard"]);
            return feed(cmd);
        }
        false
    }
    pub fn copy_last_assistant(&mut self) -> Result<usize, String> {
        let text = self
            .chat
            .iter()
            .rev()
            .find(|line| line.role == ChatRole::Assistant && !line.text.trim().is_empty())
            .map(|line| line.text.clone())
            .ok_or_else(|| "当前还没有可复制的助手回复".to_string())?;
        match crate::clipboard_copy::copy_to_clipboard(&text) {
            Ok(lease) => {
                self.clipboard_lease = lease;
                let chars = text.chars().count();
                self.copied_notice = Some(format!("已复制最近回复 · {chars} 字符"));
                Ok(chars)
            }
            Err(error) => {
                self.copied_notice = Some(format!("复制失败: {error}"));
                Err(error)
            }
        }
    }
    pub fn handle_key(&mut self, key: Key) -> Action {
        if key != Key::Tab {
            self.command_completion = None;
        }
        if self.perm_prompt.is_some() {
            return self.handle_perm_key(key);
        }
        if self.asking.is_some() {
            return self.handle_ask_key(key);
        }
        match key {
            Key::Enter => {
                if self.in_paste {
                    self.paste_prev_cr = true;
                    self.input.insert(self.cursor_byte(), '\n');
                    self.cursor += 1;
                    Action::None
                } else {
                    self.submit().unwrap_or(Action::None)
                }
            }
            Key::Backspace => {
                if self.cursor > 0 {
                    let end = self.cursor_byte();
                    let start = self.input[..end].char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                    self.input.replace_range(start..end, "");
                    self.cursor -= 1;
                }
                Action::None
            }
            Key::Delete => {
                let b = self.cursor_byte();
                if b < self.input.len() {
                    self.input.remove(b);
                }
                Action::None
            }
            Key::Left => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
                Action::None
            }
            Key::Right => {
                if self.cursor < self.input.chars().count() {
                    self.cursor += 1;
                }
                Action::None
            }
            Key::Home => {
                self.cursor = self.cursor_line_info().map(|(_, _, s, _)| s).unwrap_or(0);
                Action::None
            }
            Key::End => {
                self.cursor = self
                    .cursor_line_info()
                    .map(|(_, _, _, e)| e)
                    .unwrap_or(self.input.chars().count());
                Action::None
            }
            Key::Up => {
                if self.input_line_count() > 1 {
                    self.cursor_move(-1);
                    Action::None
                } else if self.queued_count > 0 {
                    Action::RetrieveQueued
                } else {
                    self.hist_move(-1)
                }
            }
            Key::Down => {
                if self.input_line_count() > 1 {
                    self.cursor_move(1);
                    Action::None
                } else {
                    self.hist_move(1)
                }
            }
            Key::Tab => {
                if self.in_paste {
                    self.command_completion = None;
                    self.input.insert(self.cursor_byte(), '\t');
                    self.cursor += 1;
                } else {
                    self.complete_command();
                }
                Action::None
            }
            Key::PageUp => {
                self.scroll += self.chat_height().saturating_sub(1);
                self.follow = false;
                Action::None
            }
            Key::PageDown => {
                self.scroll = self.scroll.saturating_sub(self.chat_height().saturating_sub(1));
                if self.scroll == 0 {
                    self.follow = true;
                }
                Action::None
            }
            Key::WheelUp => {
                self.scroll += 3;
                self.follow = false;
                Action::None
            }
            Key::WheelDown => {
                self.scroll = self.scroll.saturating_sub(3);
                if self.scroll == 0 {
                    self.follow = true;
                }
                Action::None
            }
            Key::MouseDown { row, col } => {
                if let Some(point) = self.mouse_selection_point(row, col, false) {
                    self.sel_anchor = Some(point);
                    self.sel_cur = Some(point);
                    self.copied_notice = None;
                    Action::Redraw
                } else {
                    self.sel_anchor = None;
                    self.sel_cur = None;
                    Action::None
                }
            }
            Key::MouseDrag { row, col } => {
                if self.sel_anchor.is_none() {
                    return Action::None;
                }
                if let Some(point) = self.mouse_selection_point(row, col, true) {
                    self.sel_cur = Some(point);
                    Action::Redraw
                } else {
                    Action::None
                }
            }
            Key::MouseUp { row, col } => {
                if self.sel_anchor.is_some() {
                    self.finish_mouse_selection(row, col)
                } else {
                    Action::None
                }
            }
            Key::PasteStart => {
                self.in_paste = true;
                self.paste_prev_cr = false;
                Action::None
            }
            Key::PasteEnd => {
                self.in_paste = false;
                self.paste_prev_cr = false;
                Action::None
            }
            Key::Char('\n') if self.in_paste && self.paste_prev_cr => {
                self.paste_prev_cr = false;
                Action::None
            }
            Key::Char(c) => {
                self.input.insert(self.cursor_byte(), c);
                self.cursor += 1;
                Action::None
            }
            Key::CtrlC => {
                if self.streaming {
                    // 运行中的唯一中断键是 Esc。Ctrl+C 不再误杀当前任务。
                    Action::None
                } else {
                    self.input.clear();
                    self.cursor = 0;
                    Action::None
                }
            }
            Key::CtrlD => Action::Quit,
            Key::CtrlL => Action::Redraw,
            Key::CtrlO => {
                let _ = self.copy_last_assistant();
                Action::Redraw
            }
            Key::Esc => {
                if self.streaming {
                    Action::Cancel
                } else {
                    self.input.clear();
                    self.cursor = 0;
                    Action::None
                }
            }
            Key::Unknown => Action::None,
        }
    }
    fn hist_move(&mut self, d: i32) -> Action {
        if self.history.is_empty() {
            return Action::None;
        }
        let n = self.history.len();
        let cur = match self.hist_pos {
            Some(p) => {
                let mut np = p as i32 + d;
                if np < 0 {
                    np = -1;
                } else if np >= n as i32 {
                    np = n as i32 - 1;
                }
                np
            }
            None => {
                if d < 0 {
                    n as i32 - 1
                } else {
                    -1
                }
            }
        };
        if cur < 0 {
            self.hist_pos = None;
            self.input.clear();
            self.cursor = 0;
        } else {
            self.hist_pos = Some(cur as usize);
            self.input = self.history[cur as usize].clone();
            self.cursor = self.input.chars().count();
        }
        Action::None
    }
    fn prompt_w(&self) -> usize {
        self.prompt.width()
    }
    fn input_width(&self) -> usize {
        self.w.saturating_sub(self.prompt_w() + 1).max(10)
    }
    fn max_input_lines(&self) -> usize {
        (self.h / 3).clamp(3, 8)
    }
    fn input_line_count(&self) -> usize {
        wrap_input_ranges(&self.input, self.input_width()).len()
    }
    fn visible_input_lines(&self) -> usize {
        if self.asking.is_some() {
            0
        } else {
            self.input_line_count().min(self.max_input_lines())
        }
    }
    fn popup_height(&self) -> usize {
        if self.asking.is_some() {
            return 0;
        }
        let (lines, _) = self.command_popup();
        if lines.is_empty() {
            return 0;
        }
        lines.len().min(
            self.h
                .saturating_sub(self.fixed_header_height() + 4 + self.visible_input_lines()),
        )
    }
    fn cursor_line_info(&self) -> Option<(usize, usize, usize, usize)> {
        let lines = wrap_input_ranges(&self.input, self.input_width());
        if lines.is_empty() {
            return None;
        }
        let c = self.cursor.min(self.input.chars().count());
        for (i, (text, s, e)) in lines.iter().enumerate() {
            if c >= *s && c < *e {
                let col: usize = text
                    .chars()
                    .take(c - *s)
                    .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0).max(1))
                    .sum();
                return Some((i, col, *s, *e));
            }
        }
        let (text, s, e) = &lines[lines.len() - 1];
        let col: usize = text
            .chars()
            .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0).max(1))
            .sum();
        Some((lines.len() - 1, col, *s, *e))
    }
    fn cursor_move(&mut self, d: i32) {
        let lines = wrap_input_ranges(&self.input, self.input_width());
        if lines.len() <= 1 {
            return;
        }
        let Some((cur_line, cur_col, _, _)) = self.cursor_line_info() else { return };
        let target = cur_line as i32 + d;
        if target < 0 || target >= lines.len() as i32 {
            return;
        }
        let (text, s, _) = &lines[target as usize];
        let mut acc = 0usize;
        let mut ci = *s;
        for (j, ch) in text.chars().enumerate() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
            if acc + cw > cur_col {
                break;
            }
            acc += cw;
            ci = s + j + 1;
        }
        self.cursor = ci;
    }
    fn command_token(&self) -> Option<CommandToken> {
        let trimmed = self.input.trim_start();
        let leading = self.input.len().saturating_sub(trimmed.len());
        if self.input.as_bytes().get(leading) != Some(&b'/') {
            return None;
        }
        let start = leading + 1;
        let end = self.input[start..]
            .find(char::is_whitespace)
            .map(|offset| start + offset)
            .unwrap_or(self.input.len());
        let cursor = self.cursor_byte();
        if cursor < start || cursor > end {
            return None;
        }
        Some(CommandToken {
            start,
            end,
            prefix: self.input[start..cursor].to_ascii_lowercase(),
        })
    }
    fn replace_command_token(&mut self, start: usize, end: usize, replacement: &str, add_space: bool) -> usize {
        self.input.replace_range(start..end, replacement);
        let token_end = start + replacement.len();
        let mut cursor = token_end;
        if add_space {
            match self.input[cursor..].chars().next() {
                Some(ch) if ch.is_whitespace() => cursor += ch.len_utf8(),
                _ => {
                    self.input.insert(cursor, ' ');
                    cursor += 1;
                }
            }
        }
        self.cursor = self.input[..cursor].chars().count();
        token_end
    }
    fn common_command_prefix(matches: &[&'static str]) -> String {
        let Some(first) = matches.first() else { return String::new() };
        let mut len = first.len();
        for candidate in &matches[1..] {
            len = first
                .bytes()
                .zip(candidate.bytes())
                .take(len)
                .take_while(|(left, right)| left == right)
                .count();
        }
        first[..len].to_string()
    }
    fn remember_command_completion(
        &mut self,
        matches: Vec<&'static str>,
        next_index: usize,
        token_start: usize,
        token_end: usize,
    ) {
        self.command_completion = Some(CommandCompletion {
            matches,
            next_index,
            token_start,
            token_end,
            expected_input: self.input.clone(),
            expected_cursor: self.cursor,
        });
    }
    fn continue_command_completion(&mut self) -> bool {
        let Some(state) = self.command_completion.take() else { return false };
        if self.input != state.expected_input || self.cursor != state.expected_cursor || state.matches.is_empty() {
            return false;
        }
        let selected = state.next_index % state.matches.len();
        let token_end = self.replace_command_token(
            state.token_start,
            state.token_end,
            state.matches[selected],
            true,
        );
        let next_index = (selected + 1) % state.matches.len();
        self.remember_command_completion(state.matches, next_index, state.token_start, token_end);
        true
    }
    fn complete_command(&mut self) {
        if self.continue_command_completion() {
            return;
        }
        let Some(token) = self.command_token() else {
            self.command_completion = None;
            return;
        };
        let matches: Vec<&'static str> = self
            .commands
            .iter()
            .filter(|(name, _)| name.starts_with(&token.prefix))
            .map(|(name, _)| *name)
            .collect();
        if matches.is_empty() {
            self.command_completion = None;
            return;
        }
        if matches.len() == 1 {
            self.replace_command_token(token.start, token.end, matches[0], true);
            self.command_completion = None;
            return;
        }
        let common = Self::common_command_prefix(&matches);
        if common.len() > token.prefix.len() {
            let token_end = self.replace_command_token(token.start, token.end, &common, false);
            let next_index = matches.iter().position(|name| *name != common).unwrap_or(0);
            self.remember_command_completion(matches, next_index, token.start, token_end);
        } else {
            let token_end = self.replace_command_token(token.start, token.end, matches[0], true);
            let next_index = 1 % matches.len();
            self.remember_command_completion(matches, next_index, token.start, token_end);
        }
    }
    fn command_popup(&self) -> (Vec<String>, usize) {
        if self.asking.is_some() {
            return (Vec::new(), 0);
        }
        let Some(token) = self.command_token() else {
            return (Vec::new(), 0);
        };
        let filter = token.prefix;
        let hits: Vec<_> = self.commands.iter().filter(|(n, _)| n.starts_with(&filter)).collect();
        if hits.is_empty() {
            return (Vec::new(), 0);
        }
        let w = self.w;
        let mut lines = Vec::with_capacity(hits.len());
        for (name, desc) in &hits {
            let max_desc = w.saturating_sub(6 + name.width());
            let desc = truncate_width(desc, max_desc);
            let (matched, tail) = if filter.is_empty() {
                (*name, "")
            } else {
                name.split_at(filter.len().min(name.len()))
            };
            let mut s = format!("  {C_BOLD}/{matched}{C_RESET}");
            if !tail.is_empty() {
                s.push_str(&format!("{C_DIM}{tail}{C_RESET}"));
            }
            if !desc.is_empty() {
                s.push_str(&format!("{C_DIM}  {desc}{C_RESET}"));
            }
            lines.push(s);
        }
        (lines, hits.len())
    }
    fn ask_panel_height(&self) -> usize {
        if self.asking.is_none() {
            return 0;
        }
        self.build_ask_panel(self.h.saturating_sub(self.fixed_header_height()))
            .lines
            .len()
    }
    fn build_ask_panel(&self, max_lines: usize) -> AskPanel {
        let Some(asking) = self.asking.as_ref() else {
            return AskPanel { lines: Vec::new(), cursor: None };
        };
        let Some(question) = asking.questions.get(asking.current) else {
            return AskPanel { lines: Vec::new(), cursor: None };
        };
        if max_lines == 0 {
            return AskPanel { lines: Vec::new(), cursor: None };
        }
        let width = self.w.max(10);
        let option_count = question.options.len();
        let other_index = option_count;
        let max_index_width = (option_count + 1).to_string().len();
        let mut rows: Vec<AskPanelRow> = Vec::new();
        for (index, option) in question.options.iter().enumerate() {
            let focused = asking.focus == index;
            let checked = question.multi_select && asking.checked.contains(&index);
            let pointer = if focused { "❯" } else { " " };
            let number = format!("{}.", index + 1);
            let number = format!("{number:<width$}", width = max_index_width + 2);
            let check = if question.multi_select {
                if checked { "[✓] " } else { "[ ] " }
            } else {
                ""
            };
            let prefix_width = 2 + number.width() + check.width();
            let label = truncate_width(&option.label, width.saturating_sub(prefix_width));
            let color = if checked { C_GREEN } else if focused { C_SUGGEST } else { C_ASST };
            let row = format!(
                "{color}{pointer}{C_RESET} {C_DIM}{number}{C_RESET}{color}{check}{label}{C_RESET}"
            );
            let description = if option.description.is_empty() {
                None
            } else {
                let pad = " ".repeat(prefix_width);
                let text = truncate_width(&option.description, width.saturating_sub(prefix_width));
                let desc_color = if focused { C_SUGGEST } else { C_DIM };
                Some(format!("{pad}{desc_color}{text}{C_RESET}"))
            };
            rows.push((row, description, None));
        }
        let other_focused = asking.focus == other_index;
        let other_checked = question.multi_select && asking.checked.contains(&other_index);
        let pointer = if other_focused { "❯" } else { " " };
        let number = format!("{}.", other_index + 1);
        let number = format!("{number:<width$}", width = max_index_width + 2);
        let check = if question.multi_select {
            if other_checked { "[✓] " } else { "[ ] " }
        } else {
            ""
        };
        let prefix_width = 2 + number.width() + check.width();
        let secret_input = self.ask_kind == AskKind::ProviderConfig && question.header == "API Key";
        let custom = if self.input.is_empty() {
            if secret_input { "Paste API key (hidden).".to_string() } else { "Type something.".to_string() }
        } else if secret_input {
            "•".repeat(self.input.chars().count())
        } else {
            self.input.clone()
        };
        let custom = truncate_width(&custom, width.saturating_sub(prefix_width));
        let color = if other_checked {
            C_GREEN
        } else if other_focused {
            C_SUGGEST
        } else if self.input.is_empty() {
            C_DIM
        } else {
            C_ASST
        };
        let other_row = format!(
            "{color}{pointer}{C_RESET} {C_DIM}{number}{C_RESET}{color}{check}{custom}{C_RESET}"
        );
        let other_cursor = if other_focused {
            let before = if secret_input {
                "•".repeat(self.cursor)
            } else {
                self.input.chars().take(self.cursor).collect()
            };
            let col = prefix_width + before.width() + 1;
            let ch = if self.input.is_empty() {
                if secret_input { 'P' } else { 'T' }
            } else if secret_input {
                '•'
            } else {
                self.input.chars().nth(self.cursor).unwrap_or(' ')
            };
            Some((col.min(width), ch))
        } else {
            None
        };
        rows.push((other_row, None, other_cursor));
        if question.multi_select {
            let submit_index = other_index + 1;
            let focused = asking.focus == submit_index;
            let label = if asking.current + 1 == asking.questions.len() { "Submit" } else { "Next" };
            let pointer = if focused { "❯" } else { " " };
            let color = if focused { C_SUGGEST } else { C_ASST };
            rows.push((
                format!("{color}{pointer}     {C_BOLD}{label}{C_RESET}"),
                None,
                None,
            ));
        }
        let tab_count = asking.questions.len().max(1);
        let tab_width = width.saturating_sub(4).checked_div(tab_count).unwrap_or(width).max(5);
        let mut nav = String::new();
        if asking.questions.len() > 1 || question.multi_select {
            if asking.current == 0 {
                nav.push_str(&format!("{C_DIM}← {C_RESET}"));
            } else {
                nav.push_str("← ");
            }
        }
        for (index, item) in asking.questions.iter().enumerate() {
            let answered = asking.answers.contains_key(&item.question);
            let mark = if answered { "☒" } else { "☐" };
            let header = truncate_width(&item.header, tab_width.saturating_sub(4));
            if index == asking.current {
                nav.push_str(&format!("{C_BG_ASK_TAB} {mark} {header} {C_RESET}"));
            } else {
                nav.push_str(&format!(" {mark} {header} "));
            }
        }
        if asking.questions.len() > 1 || question.multi_select {
            if asking.current + 1 >= asking.questions.len() {
                nav.push_str(&format!("{C_DIM} →{C_RESET}"));
            } else {
                nav.push_str(" →");
            }
        }
        let compact_height = 6 + rows.len();                                         
        let description_count = rows.iter().filter(|(_, desc, _)| desc.is_some()).count();
        let expanded = compact_height + description_count <= max_lines;
        let all_rows_fit = compact_height <= max_lines;
        let (row_start, row_end, include_blank) = if all_rows_fit {
            (0, rows.len(), true)
        } else {
            let visible_count = max_lines.saturating_sub(5).max(1).min(rows.len());
            let max_start = rows.len().saturating_sub(visible_count);
            let start = asking.focus.saturating_sub(visible_count / 2).min(max_start);
            (start, start + visible_count, false)
        };
        let mut lines = Vec::new();
        let mut cursor = None;
        lines.push(format!("{C_GRAY}{}{C_RESET}", "─".repeat(width)));
        lines.push(nav);
        lines.push(format!(
            "{C_BOLD}{}{C_RESET}",
            truncate_width(&question.question, width)
        ));
        if include_blank {
            lines.push(String::new());
        }
        for (row_index, (row, description, row_cursor)) in rows
            .iter()
            .enumerate()
            .skip(row_start)
            .take(row_end.saturating_sub(row_start))
        {
            let panel_row = lines.len();
            lines.push(row.clone());
            if row_index == row_start && row_start > 0 {
                lines[panel_row].push_str(&format!(" {C_DIM}↑{C_RESET}"));
            }
            if row_index + 1 == row_end && row_end < rows.len() {
                lines[panel_row].push_str(&format!(" {C_DIM}↓{C_RESET}"));
            }
            if let Some((col, ch)) = row_cursor {
                cursor = Some((panel_row, *col, *ch));
            }
            if expanded {
                if let Some(description) = description {
                    lines.push(description.clone());
                }
            }
        }
        lines.push(format!("{C_GRAY}{}{C_RESET}", "─".repeat(width)));
        let help = if question.multi_select {
            "Enter/Space to select · ↑/↓ to navigate · Enter on Submit · Esc to cancel"
        } else {
            "Enter to select · ↑/↓ to navigate · Esc to cancel"
        };
        let footer = match asking.notice.as_deref() {
            Some(notice) => format!("{C_WARN}⚠ {notice}{C_RESET} {C_DIM}· {help}{C_RESET}"),
            None => format!("{C_DIM}{help}{C_RESET}"),
        };
        lines.push(footer);
        if lines.len() > max_lines {
            lines.truncate(max_lines);
            cursor = cursor.filter(|(row, _, _)| *row < max_lines);
        }
        AskPanel { lines, cursor }
    }
    fn perm_panel_height(&self) -> usize {
        if self.perm_prompt.is_none() {
            return 0;
        }
        self.build_perm_panel(self.h.saturating_sub(self.fixed_header_height()))
            .lines
            .len()
    }
    fn build_perm_panel(&self, max_lines: usize) -> AskPanel {
        let Some(ui) = self.perm_prompt.as_ref() else {
            return AskPanel { lines: Vec::new(), cursor: None };
        };
        if max_lines == 0 {
            return AskPanel { lines: Vec::new(), cursor: None };
        }
        let width = self.w.max(10);
        let mut lines = Vec::new();
        lines.push(format!("{C_GRAY}{}{C_RESET}", "─".repeat(width)));
        lines.push(format!(
            "{C_WARN}⚠ {C_BOLD}模型想执行命令{C_RESET}{C_DIM}（未开启 /autodangerous，需您批准）{C_RESET}"
        ));
        let cmd_prefix = format!("{C_ASST}❯ {C_RESET}");
        let cmd_width = width.saturating_sub(cmd_prefix.width());
        for (i, (seg, _, _)) in wrap_input_ranges(&ui.cmd, cmd_width).iter().enumerate() {
            if i == 0 {
                lines.push(format!("{cmd_prefix}{C_ASST}{seg}{C_RESET}"));
            } else {
                lines.push(format!("{C_ASST}  {seg}{C_RESET}"));
            }
        }
        lines.push(String::new());
        let kind = if ui.cmd_kind.is_empty() {
            "任意".to_string()
        } else {
            ui.cmd_kind.clone()
        };
        let labels = [
            "Yes，允许执行（仅此一次）".to_string(),
            format!("本会话内全部同意 {kind} 类命令"),
            "No，拒绝执行".to_string(),
            "开启 autodangerous（不再询问）".to_string(),
        ];
        for (index, label) in labels.iter().enumerate() {
            let focused = ui.focus == index;
            let pointer = if focused { "❯" } else { " " };
            let number = format!("{}.", index + 1);
            let number = format!("{number:<3}");
            let color = if focused { C_SUGGEST } else { C_ASST };
            lines.push(format!(
                "{color}{pointer}{C_RESET} {C_DIM}{number}{C_RESET}{color}{}{C_RESET}",
                truncate_width(label, width.saturating_sub(8))
            ));
        }
        lines.push(format!("{C_GRAY}{}{C_RESET}", "─".repeat(width)));
        lines.push(format!(
            "{C_DIM}↑↓ 选择 · 回车确认 · Esc 拒绝 · 数字 1/2/3/4 直达{C_RESET}"
        ));
        if lines.len() > max_lines {
            lines.truncate(max_lines);
        }
        AskPanel { lines, cursor: None }
    }
    fn chat_height(&self) -> usize {
        if self.asking.is_some() || self.perm_prompt.is_some() {
            return self.h.saturating_sub(
                self.fixed_header_height() + self.ask_panel_height() + self.perm_panel_height(),
            );
        }
        self.h.saturating_sub(
            self.fixed_header_height()
                + 3
                + self.visible_input_lines()
                + self.popup_height(),
        )
    }
    fn role_prefix(role: ChatRole) -> &'static str {
        match role {
            ChatRole::Error => "✗ ",
            ChatRole::Warn => "⚠ ",
            ChatRole::Summary => "◆ ",
            ChatRole::ToolResult => "  ⎿  ",
            _ => "",
        }
    }
    fn role_base(role: ChatRole) -> &'static str {
        match role {
            ChatRole::System => C_DIM,
            ChatRole::Error => C_RED,
            ChatRole::Warn => C_WARN,
            ChatRole::Summary => C_MAG,
            ChatRole::Reasoning => C_DIM,
            _ => C_ASST,
        }
    }
    fn strip_tool_blocks(s: &str) -> String {
        let mut out = String::new();
        let mut rest = s;
        loop {
            match rest.find("```tool") {
                Some(i) => {
                    out.push_str(&rest[..i]);
                    let after = &rest[i + "```tool".len()..];
                    match after.find("```") {
                        Some(j) => rest = &after[j + 3..],
                        None => break,              
                    }
                }
                None => {
                    out.push_str(rest);
                    break;
                }
            }
        }
        out
    }
    fn render_lines(&self) -> Vec<RLine> {
        let cw = self.w.saturating_sub(5).max(10);
        let mut lines: Vec<RLine> = Vec::new();
        let last_idx = self.chat.len().saturating_sub(1);
        for (ci, line) in self.chat.iter().enumerate() {
            let text = Self::strip_tool_blocks(&line.text);
            if text.trim().is_empty() {
                if line.role == ChatRole::Assistant && line.pending && ci == last_idx {
                    lines.push(RLine { shade: None, styled: format!("{C_ASST}● ") });
                }
                continue;                                    
            }
            match line.role {
                ChatRole::User => {
                    for wl in wrap(&text, cw) {
                        lines.push(RLine { shade: Some(C_BG_USER), styled: format!("{C_USER_TEXT}{wl}") });
                    }
                }
                ChatRole::Tool => {
                    let wrapped = wrap(&text, cw);
                    for (i, wl) in wrapped.iter().enumerate() {
                        let styled = if i == 0 {
                            match &line.op {
                                Some(name) => {
                                    let rest = wl.strip_prefix(name).unwrap_or(wl);
                                    format!("{C_ASST}● {C_BOLD}{name}{C_RESET}{rest}")
                                }
                                None => format!("{C_ASST}● {wl}"),
                            }
                        } else {
                            format!("  {wl}")
                        };
                        lines.push(RLine { shade: None, styled });
                    }
                }
                ChatRole::Assistant => {
                    self.render_md_lines(&text, cw, &mut lines);
                }
                _ => {
                    let base = Self::role_base(line.role);
                    let prefix = Self::role_prefix(line.role);
                    let text = if line.role == ChatRole::Reasoning && text.chars().count() > 800 {
                        let skip = text.chars().count() - 800;
                        let tail: String = text.chars().skip(skip).collect();
                        format!("…（思考流已折叠，完整内容见 ~/.yjlcoder/trace/）{tail}")
                    } else {
                        text.clone()
                    };
                    for (i, wl) in wrap(&text, cw).iter().enumerate() {
                        let styled = if i == 0 {
                            format!("{base}{prefix}{wl}")
                        } else {
                            format!("  {wl}")
                        };
                        lines.push(RLine { shade: None, styled });
                    }
                }
            }
            lines.push(RLine { shade: None, styled: String::new() });         
        }
        lines
    }
    fn fixed_header_height(&self) -> usize {
        if self.h < 14 {
            1
        } else if self.w < 72 || self.h < 24 {
            7
        } else {
            11
        }
    }
    fn welcome_lines(&self) -> Vec<RLine> {
        let width = self.w.max(10);
        let state = self.mascot_state;
        let color = state.color();
        let label = state.label();
        let face = mascot_compact(state);
        if self.fixed_header_height() == 1 {
            let text = truncate_width(
                &format!("Yujiale Code v{} · {face} {label} · {}", env!("CARGO_PKG_VERSION"), self.header),
                width,
            );
            return vec![RLine { shade: None, styled: format!("{color}{text}{C_RESET}") }];
        }
        let title = format!(
            "─── Yujiale Code v{} · {label} ",
            env!("CARGO_PKG_VERSION")
        );
        let title = truncate_width(&title, width.saturating_sub(2));
        let title_w = title.width();
        let top = format!(
            "╭{}{}╮",
            title,
            "─".repeat(width.saturating_sub(title_w + 2))
        );
        if self.fixed_header_height() == 7 {
            let inner = width.saturating_sub(2);
            let rows = [
                "Welcome back!".to_string(),
                format!("Y仔  {face}  {label}"),
                truncate_width(&welcome_model(&self.header), inner),
                truncate_width(&display_cwd(), inner),
                "local state machine · zero model tokens".to_string(),
            ];
            let mut out = vec![RLine { shade: None, styled: format!("{C_GRAY}{top}{C_RESET}") }];
            for (index, row) in rows.into_iter().enumerate() {
                let row = center_width(&row, inner);
                let styled = if index == 1 {
                    format!("{color}{row}{C_RESET}")
                } else {
                    row
                };
                out.push(RLine {
                    shade: None,
                    styled: format!("{C_GRAY}│{C_RESET}{styled}{C_GRAY}│{C_RESET}"),
                });
            }
            out.push(RLine {
                shade: None,
                styled: format!("{C_GRAY}╰{}╯{C_RESET}", "─".repeat(inner)),
            });
            return out;
        }
        let inner = width - 2;
        let left_w = (inner * 41 / 100).clamp(30, 42);
        let right_w = inner.saturating_sub(left_w + 1);
        let cwd = display_cwd();
        let model = welcome_model(&self.header);
        let mascot = mascot_art(state);
        let left = [
            "Welcome back!".to_string(),
            mascot[0].clone(),
            mascot[1].clone(),
            mascot[2].clone(),
            mascot[3].clone(),
            mascot[4].clone(),
            mascot[5].clone(),
            truncate_width(&model, left_w),
            truncate_width(&cwd, left_w),
        ];
        let right = [
            "Tips for getting started".to_string(),
            "Run /help to see commands and shortcuts".to_string(),
            "────────────────────────────────────────".to_string(),
            "What's new".to_string(),
            "Meet Y仔 · deterministic mood state machine".to_string(),
            "Fixed header · conversation-only scrolling".to_string(),
            "Read and ls pages without silent truncation".to_string(),
            "".to_string(),
            "Yujiale Code · local-first coding agent".to_string(),
        ];
        let mut out = vec![RLine { shade: None, styled: format!("{C_GRAY}{top}{C_RESET}") }];
        for index in 0..left.len().max(right.len()) {
            let l = center_width(left.get(index).map(String::as_str).unwrap_or(""), left_w);
            let l = if (1..=6).contains(&index) {
                format!("{color}{l}{C_RESET}")
            } else {
                l
            };
            let right_text = right.get(index).map(String::as_str).unwrap_or("");
            let r = if right_text.is_empty() {
                " ".repeat(right_w)
            } else {
                pad_width(&format!(" {right_text}"), right_w)
            };
            out.push(RLine {
                shade: None,
                styled: format!("{C_GRAY}│{C_RESET}{l}{C_GRAY}│{C_RESET}{r}{C_GRAY}│{C_RESET}"),
            });
        }
        out.push(RLine {
            shade: None,
            styled: format!("{C_GRAY}╰{}╯{C_RESET}", "─".repeat(inner)),
        });
        out
    }
    fn render_md_lines(&self, text: &str, cw: usize, out: &mut Vec<RLine>) {
        let mut first = true;
        for (is_code, part) in crate::md::block_parts(text) {
            if is_code {
                for wl in wrap(&part, cw) {
                    out.push(RLine { shade: Some(C_BG_CODE), styled: format!("{C_ASST}{wl}") });
                }
                first = false;
                continue;
            }
            let plines: Vec<&str> = part.split_terminator('\n').collect();
            let mut i = 0;
            while i < plines.len() {
                if let Some((table, consumed)) = crate::md::parse_table(&plines[i..]) {
                    for tl in crate::md::render_table(&table, cw) {
                        out.push(RLine { shade: Some(C_BG_CODE), styled: format!("{C_ASST}{tl}") });
                    }
                    first = false;
                    i += consumed;
                    continue;
                }
                let pline = plines[i];
                if pline.trim().is_empty() {
                    out.push(RLine { shade: None, styled: String::new() });
                    i += 1;
                    continue;
                }
                let (head_style, head_prefix, rest) = crate::md::block_prefix(pline);
                let mut segs = crate::md::inline(rest);
                if let Some(st) = head_style {
                    segs = vec![crate::md::Seg::new(st, rest.to_string())];
                } else if !head_prefix.is_empty() {
                    segs.insert(0, crate::md::Seg::new(crate::md::Style::Dim, head_prefix.to_string()));
                }
                for wsegs in crate::md::wrap_segs(&segs, cw) {
                    let styled = if first {
                        format!("{C_ASST}● {}", crate::md::render_segs(&wsegs, C_ASST))
                    } else {
                        format!("  {}", crate::md::render_segs(&wsegs, C_ASST))
                    };
                    out.push(RLine { shade: None, styled });
                    first = false;
                }
                i += 1;
            }
        }
    }
    fn chat_line_count(&self) -> usize {
        self.render_lines().len()
    }
    fn live_usage(&self) -> crate::llm::Usage {
        let mut usage = self.live_committed_usage;
        add_usage(&mut usage, self.live_request_usage);
        let generated = self.live_generated_bytes.saturating_add(3) / 4;
        usage.completion_tokens = usage.completion_tokens.max(generated);
        usage
    }
    fn live_status(&self) -> String {
        if !self.streaming {
            return String::new();
        }
        let elapsed = self.stream_started.map(|started| started.elapsed().as_secs()).unwrap_or(0);
        let phase = match self.mascot_state {
            MascotState::Tool => "Running tool",
            MascotState::Asking => "Waiting for user",
            MascotState::Angry | MascotState::Frantic => "Recovering",
            _ if elapsed < 10 => "Thinking",
            _ if elapsed < 20 => "Still thinking",
            _ if elapsed < 30 => "Thinking more",
            _ if elapsed < 45 => "Thinking some more",
            _ => "Almost done thinking",
        };
        let usage = self.live_usage();
        let estimated = self.live_request_usage.prompt_tokens > 0 && !self.live_request_exact;
        let marker = if estimated { "~" } else { "" };
        // Bug4 修复：直播状态优先用服务端真实缓存口径；无真实数据时用本地前缀稳定估计并加 [est]
        let cache = if let Some(percent) = usage.server_cache_percent() {
            format!(" · cache {marker}{percent:.0}%")
        } else if usage.prompt_tokens > 0 {
            let percent = usage.cache_read_tokens as f64 * 100.0 / usage.prompt_tokens as f64;
            format!(" · cache {marker}{percent:.0}%")
        } else {
            String::new()
        };
        let cost = self
            .pricing
            .estimate(
                usage.prompt_tokens,
                usage.cache_read_tokens,
                usage.cache_miss_tokens,
                usage.completion_tokens,
            )
            .map(|value| format!(" · {}{}", self.pricing.currency, format_cost(value)))
            .unwrap_or_default();
        format!(
            "{phase} {elapsed}s · ↑{marker}{} · ↓{marker}{}{cache}{cost} · ",
            format_token_count(usage.prompt_tokens),
            format_token_count(usage.completion_tokens),
        )
    }
    fn build_frame(&mut self) -> String {
        let mut out = String::with_capacity(self.w * self.h * 2);
        out.push_str("\x1b[H\x1b[0m");
        let pct = if self.ctx_window == 0 {
            0.0
        } else {
            (self.ctx_used as f64 * 100.0 / self.ctx_window as f64).min(100.0)
        };
        let ctx_marker = if self.ctx_exact { "" } else { "~" };
        let context = format!(
            "ctx {ctx_marker}{}/{} ({pct:.2}%)",
            format_token_count(self.ctx_used),
            format_token_count(self.ctx_window),
        );
        let light = if self.streaming {
            format!("{C_MAG}◐{C_RESET}")
        } else {
            format!("{C_GREEN}●{C_RESET}")
        };
        let vis_in = self.visible_input_lines();
        let (mut popup, popup_total) = self.command_popup();
        let popup_h = popup.len().min(self.popup_height());
        if popup.len() > popup_h {
            if popup_h == 0 {
                popup.clear();
            } else {
                popup.truncate(popup_h);
                popup[popup_h - 1] = format!(
                    "{C_DIM}  … 共 {popup_total} 条命令，继续输入筛选{C_RESET}"
                );
            }
        }
        let welcome = self.welcome_lines();
        let welcome_h = welcome.len();
        for rline in &welcome {
            out.push_str(&format!("\x1b[K{}\x1b[0m\r\n", rline.styled));
        }
        let ask_panel = self.build_ask_panel(self.h.saturating_sub(welcome_h));
        let ask_h = ask_panel.lines.len();
        let perm_panel = self.build_perm_panel(self.h.saturating_sub(welcome_h));
        let perm_h = perm_panel.lines.len();
        let normal_bottom_h = if self.asking.is_some() || self.perm_prompt.is_some() {
            0
        } else {
            3 + vis_in + popup_h                      
        };
        let chat_h = self.h.saturating_sub(welcome_h + ask_h + perm_h + normal_bottom_h);
        let mut lines = self.render_lines();
        if self.streaming {
            if let Some(last) = lines.iter_mut().rev().find(|r| !r.styled.is_empty()) {
                if last.shade.is_none() && !last.styled.ends_with('▊') {
                    last.styled.push_str(" ▊");
                }
            }
        }
        let queue_status = if self.queued_count > 0 {
            format!(" · 排队 {}", self.queued_count)
        } else {
            String::new()
        };
        let notice = match &self.copied_notice {
            Some(n) => format!(" · {n}"),
            None => String::new(),
        };
        let live = self.live_status();
        let status_text = truncate_width(
            &format!("● {live}{context}{queue_status}{notice} · {}", self.header),
            self.w.saturating_sub(2),
        );
        let status_pad = self.w.saturating_sub(status_text.width());
        let status_line = RLine {
            shade: None,
            styled: format!(
                "{}{C_DIM}{light} {}{C_RESET}",
                " ".repeat(status_pad.saturating_sub(2)),
                status_text.trim_start_matches('●').trim_start()
            ),
        };
        let content_h = chat_h.saturating_sub(1);
        let total = lines.len();
        let max_scroll = total.saturating_sub(content_h);
        self.scroll = self.scroll.min(max_scroll);
        let start = total.saturating_sub(content_h.saturating_add(self.scroll));
        let visible: Vec<&RLine> = lines.iter().skip(start).take(content_h).collect();
        self.sel_start_row = welcome_h + 1;
        self.sel_rows.clear();
        for (i, rline) in visible.iter().enumerate() {
            let row_abs = welcome_h + 1 + i;
            let styled = self.apply_selection_highlight(&rline.styled, row_abs);
            self.sel_rows.push(strip_ansi(&styled));
            match rline.shade {
                Some(bg) => out.push_str(&format!("\x1b[K{bg}{styled}\x1b[K\x1b[0m\r\n")),
                None => out.push_str(&format!("\x1b[K{styled}\x1b[0m\r\n")),
            }
        }
        for _ in visible.len()..content_h {
            out.push_str("\x1b[K\r\n");
        }
        if chat_h > 0 {
            out.push_str(&format!("\x1b[K{}\x1b[0m\r\n", status_line.styled));
        }
        if self.asking.is_some() {
            for (index, line) in ask_panel.lines.iter().enumerate() {
                out.push_str(&format!("\x1b[K{line}\x1b[0m"));
                if index + 1 < ask_panel.lines.len() {
                    out.push_str("\r\n");
                }
            }
            if let Some((panel_row, col, cursor_char)) = ask_panel.cursor {
                let row = welcome_h + chat_h + panel_row + 1;
                out.push_str(&format!("\x1b[{row};{col}H{C_CURSOR}{cursor_char}\x1b[0m"));
            }
            return out;
        }
        if self.perm_prompt.is_some() {
            for (index, line) in perm_panel.lines.iter().enumerate() {
                out.push_str(&format!("\x1b[K{line}\x1b[0m"));
                if index + 1 < perm_panel.lines.len() {
                    out.push_str("\r\n");
                }
            }
            return out;
        }
        let in_lines = wrap_input_ranges(&self.input, self.input_width());
        let total_in = in_lines.len();
        let max_in = self.max_input_lines();
        let (cur_line, cur_col) = match self.cursor_line_info() {
            Some((l, c, _, _)) => (l, c),
            None => (0, 0),
        };
        self.input_scroll = if total_in > max_in {
            if cur_line < self.input_scroll {
                cur_line
            } else if cur_line >= self.input_scroll + vis_in {
                cur_line - vis_in + 1
            } else {
                self.input_scroll.min(total_in - vis_in)
            }
        } else {
            0
        };
        let prompt = &self.prompt;
        let prompt_w = self.prompt_w();
        out.push_str(&format!("\x1b[K{C_GRAY}{}{C_RESET}\r\n", "─".repeat(self.w)));
        for (i, line) in in_lines.iter().enumerate().skip(self.input_scroll).take(vis_in) {
            let prefix = if i == 0 {
                format!("{C_ASST}{prompt}")
            } else {
                format!("{C_ASST}  ")
            };
            let content = if self.input.is_empty() && i == 0 {
                format!("{C_DIM}Try \"ask about this project\"")
            } else {
                line.0.clone()
            };
            out.push_str(&format!("\x1b[K{prefix}{content}{C_RESET}\r\n"));
        }
        out.push_str(&format!("\x1b[K{C_GRAY}{}{C_RESET}", "─".repeat(self.w)));
        for line in &popup {
            out.push_str(&format!("\r\n\x1b[K{line}"));
        }
        let hint = truncate_width(&self.hint, self.w);
        out.push_str(&format!("\r\n\x1b[K{C_DIM}{hint}{C_RESET}"));
        let col = (prompt_w + 1 + cur_col).min(self.w.saturating_sub(1));
        let row = welcome_h + 2 + chat_h + (cur_line - self.input_scroll);
        let cursor_char = if self.input.is_empty() {
            'T'
        } else {
            self.input.chars().nth(self.cursor).filter(|ch| *ch != '\n').unwrap_or(' ')
        };
        out.push_str(&format!("\x1b[{row};{col}H{C_CURSOR}{cursor_char}\x1b[0m"));
        out
    }
    fn cursor_tail_start(frame: &str) -> Option<usize> {
        let bytes = frame.as_bytes();
        let mut index = 0usize;
        let mut found = None;
        while index + 2 < bytes.len() {
            if bytes[index] != 0x1b || bytes[index + 1] != b'[' {
                index += 1;
                continue;
            }
            let mut end = index + 2;
            while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                end += 1;
            }
            if end >= bytes.len() {
                break;
            }
            if bytes[end] == b'H' {
                let params = &frame[index + 2..end];
                if params.contains(';')
                    && params.bytes().all(|byte| byte.is_ascii_digit() || byte == b';')
                {
                    found = Some(index);
                }
            }
            index = end + 1;
        }
        found
    }
    fn differential_frame(previous: &str, current: &str) -> String {
        if previous.is_empty() {
            return current.to_string();
        }
        let old_tail_at = Self::cursor_tail_start(previous).unwrap_or(previous.len());
        let new_tail_at = Self::cursor_tail_start(current).unwrap_or(current.len());
        let old_rows: Vec<&str> = previous[..old_tail_at].split("\r\n").collect();
        let new_rows: Vec<&str> = current[..new_tail_at].split("\r\n").collect();
        let mut update = String::new();
        for row_index in 0..old_rows.len().max(new_rows.len()) {
            let old = old_rows.get(row_index).copied().unwrap_or("");
            let new = new_rows.get(row_index).copied().unwrap_or("");
            if old != new {
                update.push_str(&format!("\x1b[{};1H", row_index + 1));
                if new.is_empty() {
                    update.push_str("\x1b[K");
                } else {
                    update.push_str(new);
                }
            }
        }
        update.push_str(&current[new_tail_at..]);
        update
    }
    pub fn redraw(&mut self) {
        self.resize();
        let frame = self.build_frame();
        if frame == self.last_frame {
            return;
        }
        let update = Self::differential_frame(&self.last_frame, &frame);
        print!("\x1b[?2026h{update}\x1b[?2026l");
        let _ = std::io::stdout().flush();
        self.last_frame = frame;
    }
}
fn add_usage(total: &mut crate::llm::Usage, delta: crate::llm::Usage) {
    total.prompt_tokens = total.prompt_tokens.saturating_add(delta.prompt_tokens);
    total.completion_tokens = total.completion_tokens.saturating_add(delta.completion_tokens);
    total.cache_read_tokens = total.cache_read_tokens.saturating_add(delta.cache_read_tokens);
    total.cache_miss_tokens = total.cache_miss_tokens.saturating_add(delta.cache_miss_tokens);
    total.reasoning_tokens = total.reasoning_tokens.saturating_add(delta.reasoning_tokens);
    total.cache_prefix_hits = total.cache_prefix_hits.saturating_add(delta.cache_prefix_hits);
    total.cache_prefix_misses = total.cache_prefix_misses.saturating_add(delta.cache_prefix_misses);
    total.cache_prefix_messages = total.cache_prefix_messages.max(delta.cache_prefix_messages);
}
fn format_token_count(tokens: usize) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 10_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else if tokens < 1_000_000 {
        format!("{}k", tokens / 1_000)
    } else {
        format!("{:.2}m", tokens as f64 / 1_000_000.0)
    }
}
fn format_cost(value: f64) -> String {
    if value < 0.01 {
        format!("{value:.6}")
    } else {
        format!("{value:.4}")
    }
}
fn tool_display(op: &str, args: &str) -> (String, String, String) {
    let parsed = crate::tool_compat::parse_args(args);
    let normalized = crate::tool_compat::normalize_call(op, &parsed);
    let canonical = normalized.op;
    let detail = match canonical.as_str() {
        "readline" => {
            let path = normalized.args.get("path").and_then(serde_json::Value::as_str).unwrap_or("file");
            let mut detail = path.to_string();
            if let Some(offset) = normalized.args.get("start").and_then(serde_json::Value::as_u64) {
                detail.push_str(&format!(" · from line {offset}"));
            }
            if let Some(limit) = normalized.args.get("limit").and_then(serde_json::Value::as_u64) {
                detail.push_str(&format!(" · {limit} lines"));
            }
            detail
        }
        "listdir" => {
            let path = normalized.args.get("path").and_then(serde_json::Value::as_str).unwrap_or(".");
            let mut detail = path.to_string();
            if let Some(offset) = normalized.args.get("offset").and_then(serde_json::Value::as_u64) {
                if offset > 0 {
                    detail.push_str(&format!(" · offset {offset}"));
                }
            }
            if let Some(limit) = normalized.args.get("limit").and_then(serde_json::Value::as_u64) {
                detail.push_str(&format!(" · {limit} items"));
            }
            detail
        }
        "execute_command" => normalized
            .args
            .get("cmd")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(args)
            .to_string(),
        _ => args.to_string(),
    };
    let display = match canonical.as_str() {
        "readline" => "Read",
        "writefile" => "Write",
        "editline" => "Edit",
        "appendline" => "Append",
        "execute_command" => "Bash",
        "grep" => "Grep",
        "glob" => "Glob",
        "listdir" => "List",
        _ => op,
    };
    let detail: String = detail.chars().take(160).collect();
    (canonical, display.to_string(), detail)
}
fn tool_result_failed(result: &str) -> bool {
    if result.starts_with("错误:") || result.starts_with("Error") {
        return true;
    }
    result.lines().any(|line| {
        line.strip_prefix("exit code: ")
            .and_then(|code| code.trim().parse::<i32>().ok())
            .is_some_and(|code| code != 0)
    })
}
fn human_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / 1024.0 / 1024.0)
    }
}
fn mascot_art(state: MascotState) -> [String; 6] {
    let (antenna, eyes, mouth, bubble) = match state {
        MascotState::Idle => ("   ╭╮", "• •", "ᴗ", ""),
        MascotState::Thinking => ("   ╭╮", "◔ ◔", "·", "…"),
        MascotState::Tool => ("   ╭╮", "› ‹", "━", ">"),
        MascotState::Asking => ("   ╭╮", "○ ○", "⌣", "?"),
        MascotState::Success => ("   ╭╮", "⌒ ⌒", "▽", "*"),
        MascotState::Angry => ("   ╭╮", "◣ ◢", "⌢", "!"),
        MascotState::Frantic => ("  ╲╱", "⊙ ⊙", "□", "!!"),
    };
    [
        format!("{antenna}  {bubble}"),
        "╭──╯ ╰──╮".into(),
        format!("│  {eyes}  │"),
        format!("│   {mouth}   │"),
        "╰─┬───┬─╯".into(),
        "  ╵   ╵".into(),
    ]
}
fn mascot_compact(state: MascotState) -> String {
    let (_, eyes, mouth, bubble) = match state {
        MascotState::Idle => ("", "• •", "ᴗ", ""),
        MascotState::Thinking => ("", "◔ ◔", "·", "…"),
        MascotState::Tool => ("", "› ‹", "━", ">"),
        MascotState::Asking => ("", "○ ○", "⌣", "?"),
        MascotState::Success => ("", "⌒ ⌒", "▽", "*"),
        MascotState::Angry => ("", "◣ ◢", "⌢", "!"),
        MascotState::Frantic => ("", "⊙ ⊙", "□", "!!"),
    };
    format!("({eyes} {mouth}) {bubble}").trim_end().to_string()
}
fn display_cwd() -> String {
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| ".".into());
    if let Ok(home) = std::env::var("HOME") {
        if cwd == home {
            return "~".into();
        }
        if let Some(rest) = cwd.strip_prefix(&(home + "/")) {
            return format!("~/{rest}");
        }
    }
    cwd
}
fn welcome_model(header: &str) -> String {
    let model = header.split(" │ ").next().unwrap_or(header);
    format!("{model} · Local")
}
fn pad_width(text: &str, width: usize) -> String {
    let text = truncate_width(text, width);
    let padding = width.saturating_sub(text.width());
    format!("{text}{}", " ".repeat(padding))
}
fn center_width(text: &str, width: usize) -> String {
    let text = truncate_width(text, width);
    let padding = width.saturating_sub(text.width());
    let left = padding / 2;
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(padding - left))
}
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    wrap_input_ranges(text, width)
        .into_iter()
        .map(|(s, _, _)| s)
        .collect()
}
fn wrap_input_ranges(text: &str, width: usize) -> Vec<(String, usize, usize)> {
    if width == 0 {
        let n = text.chars().count();
        return vec![(text.to_string(), 0, n)];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut w = 0usize;
    let mut start = 0usize;
    let total = text.chars().count();
    for (i, ch) in text.chars().enumerate() {
        if ch == '\n' {
            out.push((std::mem::take(&mut cur), start, i + 1));
            w = 0;
            start = i + 1;
            continue;
        }
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        if w + cw > width && !cur.is_empty() {
            out.push((std::mem::take(&mut cur), start, i));
            w = 0;
            start = i;
        }
        cur.push(ch);
        w += cw;
    }
    out.push((cur, start, total));
    out
}
fn truncate_width(s: &str, max_w: usize) -> String {
    if s.width() <= max_w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        if w + cw > max_w.saturating_sub(1) {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}
pub struct KeyParser {
    buf: Vec<u8>,
}
impl Default for KeyParser {
    fn default() -> Self {
        Self::new()
    }
}
impl KeyParser {
    pub fn new() -> Self {
        KeyParser { buf: Vec::with_capacity(8) }
    }
    pub fn flush_pending_escape(&mut self) -> Option<Key> {
        if self.buf.as_slice() == [0x1b] {
            self.buf.clear();
            Some(Key::Esc)
        } else {
            None
        }
    }
    pub fn feed(&mut self, b: u8) -> Option<Key> {
        self.buf.push(b);
        if b >= 0x80 {
            if let Ok(s) = std::str::from_utf8(&self.buf) {
                let ch = s.chars().next().unwrap_or('\u{fffd}');
                self.buf.clear();
                return Some(Key::Char(ch));
            }
            if self.buf.len() >= 4 {
                self.buf.clear();
                return Some(Key::Unknown);
            }
            return None;          
        }
        if self.buf[0] == 0x1b {
            if self.buf.len() == 1 {
                return None;           
            }
            if self.buf[1] == b'[' {
                let last = *self.buf.last().unwrap();
                let is_end = last.is_ascii_alphabetic() || last == b'~';
                if !is_end {
                    return None;
                }
                let seq: String = self.buf[1..].iter().map(|&c| c as char).collect();
                self.buf.clear();
                if let Some(rest) = seq.strip_prefix("[<") {
                    let (body, pressed) = rest
                        .strip_suffix('M')
                        .map(|b| (b, true))
                        .or_else(|| rest.strip_suffix('m').map(|b| (b, false)))
                        .unwrap_or((rest, true));
                    let mut parts = body.split(';');
                    let btn: u16 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                    // SGR 顺序固定为 button;column;row。旧实现把后两项
                    // 读反，导致从窗口下方拖拽时选区跳到上方。
                    let col: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                    let row: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                    let base_button = btn & 0x03;
                    return Some(if (btn & 0x40) != 0 {
                        if base_button == 0 { Key::WheelUp } else { Key::WheelDown }
                    } else if pressed && (btn & 0x20) != 0 && base_button == 0 {
                        Key::MouseDrag { row, col }
                    } else if pressed && base_button == 0 {
                        Key::MouseDown { row, col }
                    } else if !pressed && base_button == 0 {
                        Key::MouseUp { row, col }
                    } else {
                        Key::Unknown
                    });
                }
                return Some(match seq.as_str() {
                    "[A" => Key::Up,
                    "[B" => Key::Down,
                    "[C" => Key::Right,
                    "[D" => Key::Left,
                    "[H" => Key::Home,
                    "[F" => Key::End,
                    "[5~" => Key::PageUp,
                    "[6~" => Key::PageDown,
                    "[3~" => Key::Delete,
                    "[200~" => Key::PasteStart,
                    "[201~" => Key::PasteEnd,
                    _ => Key::Unknown,
                });
            }
            self.buf.clear();
            return Some(Key::Esc);
        }
        self.buf.clear();
        Some(match b {
            0x0d => Key::Enter,
            0x0a => Key::Char('\n'),
            0x09 => Key::Tab,
            0x7f | 0x08 => Key::Backspace,
            0x03 => Key::CtrlC,
            0x04 => Key::CtrlD,
            0x0c => Key::CtrlL,
            0x0f => Key::CtrlO,
            0x1b => Key::Esc,
            b => {
                let c = b as char;
                if c.is_control() {
                    Key::Unknown
                } else {
                    Key::Char(c)
                }
            }
        })
    }
}
pub fn spawn_stdin_reader(tx: std::sync::mpsc::Sender<u8>) {
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        let mut buf = [0u8; 64];
        loop {
            match lock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for &b in &buf[..n] {
                        if tx.send(b).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });
}
fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wrap_cjk_and_ascii() {
        let lines = wrap("你好hello世界", 6);
        assert_eq!(lines, vec!["你好he", "llo世", "界"]);
    }
    #[test]
    fn wrap_respects_newlines() {
        let lines = wrap("a\nbb", 10);
        assert_eq!(lines, vec!["a", "bb"]);
    }
    #[test]
    fn key_parser_basic() {
        let mut p = KeyParser::new();
        assert!(matches!(p.feed(b'a'), Some(Key::Char('a'))));
        assert!(matches!(p.feed(0x0d), Some(Key::Enter)));
        assert!(matches!(p.feed(0x0a), Some(Key::Char('\n'))));
        assert!(matches!(p.feed(0x09), Some(Key::Tab)));
        assert!(matches!(p.feed(0x0f), Some(Key::CtrlO)));
        assert!(matches!(p.feed(0x7f), Some(Key::Backspace)));
        assert!(p.feed(0x1b).is_none());
        assert!(matches!(p.flush_pending_escape(), Some(Key::Esc)));
        assert!(p.flush_pending_escape().is_none());
    }
    #[test]
    fn pasted_newline_inserts_not_submits() {
        let mut t = Tui::new();
        for c in "第一行".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        let _ = t.handle_key(Key::Char('\n'));
        for c in "第二行".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        assert_eq!(t.input, "第一行\n第二行");
        assert!(t.input_line_count() >= 2);
        t.cursor = 5;         
        let _ = t.handle_key(Key::Up);
        assert!(t.hist_pos.is_none(), "多行输入 ↑ 不应回填历史");
        assert_eq!(t.cursor, 1, "↑ 保持列位置向上移动: {}", t.cursor);
        assert!(matches!(t.handle_key(Key::Enter), Action::Submit(s) if s == "第一行\n第二行"));
    }
    #[test]
    fn paste_bracketed_crlf_dedup_and_no_submit() {
        let mut t = Tui::new();
        let _ = t.handle_key(Key::PasteStart);
        for c in "第一行".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        let _ = t.handle_key(Key::Enter);      
        let _ = t.handle_key(Key::Char('\n'));               
        for c in "第二行".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        let _ = t.handle_key(Key::Enter);             
        let _ = t.handle_key(Key::Char('\n'));             
        let _ = t.handle_key(Key::PasteEnd);
        assert_eq!(
            t.input,
            "第一行\n第二行\n",
            "CRLF 成对去重后每个换行只留一次: {:?}",
            t.input
        );
        assert!(matches!(t.handle_key(Key::Enter), Action::Submit(s) if s == "第一行\n第二行\n"));
    }
    #[test]
    fn paste_nine_lines_single_submit() {
        let mut t = Tui::new();
        let _ = t.handle_key(Key::PasteStart);
        for i in 0..9 {
            for c in format!("第{i}行").chars() {
                let _ = t.handle_key(Key::Char(c));
            }
            if i < 8 {
                let _ = t.handle_key(Key::Enter);                
            }
        }
        let _ = t.handle_key(Key::PasteEnd);
        let expected = (0..9).map(|i| format!("第{i}行")).collect::<Vec<_>>().join("\n");
        assert_eq!(t.input, expected);
        assert!(matches!(t.handle_key(Key::Enter), Action::Submit(s) if s == expected));
    }
    #[test]
    fn up_arrow_retrieves_one_queued_message() {
        let mut t = Tui::new();
        t.set_queued_count(3);
        t.input.push_str("正在编辑的新内容");
        t.cursor = t.input.chars().count();
        assert_eq!(t.handle_key(Key::Up), Action::RetrieveQueued);
        t.retrieve_queued("第三条".into());
        assert_eq!(t.input, "第三条");
        assert_eq!(t.cursor, "第三条".chars().count());
        assert_eq!(t.queued_count, 2, "只取回一条，剩余继续排队");
        let mut t2 = Tui::new();
        t2.history.push("历史".into());
        assert_eq!(t2.handle_key(Key::Up), Action::None);
        assert!(t2.hist_pos.is_some(), "无排队时 ↑ 应回填历史");
    }
    #[test]
    fn mouse_drag_uses_real_screen_coordinates_and_copies_bottom_to_top() {
        let mut t = Tui::new();
        t.sel_start_row = 5;
        t.sel_rows = vec!["alpha".into(), "bravo".into(), "charlie".into()];
        assert_eq!(t.handle_key(Key::MouseDown { row: 7, col: 3 }), Action::Redraw);
        assert_eq!(t.handle_key(Key::MouseDrag { row: 5, col: 4 }), Action::Redraw);
        assert_eq!(t.extract_selection(), "ha\nbravo\ncha");
        assert_eq!(t.handle_key(Key::MouseUp { row: 5, col: 4 }), Action::Redraw);
        assert!(t.copied_notice.as_deref().is_some_and(|notice| notice.contains("已复制选区")));
    }
    #[test]
    fn differential_frame_only_rewrites_changed_rows() {
        let old = "\x1b[Hone\r\ntwo\r\n\x1b[3;4H";
        let new = "\x1b[Hone\r\nchanged\r\n\x1b[3;4H";
        let update = Tui::differential_frame(old, new);
        assert!(update.contains("\x1b[2;1Hchanged"));
        assert!(!update.contains("\x1b[1;1H"));
        assert!(update.ends_with("\x1b[3;4H"));
    }
    #[test]
    fn perm_panel_keys_focus_and_submit() {
        let mut t = Tui::new();
        let open = |t: &mut Tui| {
            t.open_perm_prompt(crate::tools::PermRequest {
                id: 7,
                cmd: "cargo build".into(),
                cmd_kind: "cargo".into(),
            });
        };
        open(&mut t);
        assert!(t.is_perm_prompt());
        assert_eq!(t.handle_key(Key::CtrlC), Action::None);
        assert!(t.is_perm_prompt(), "Ctrl+C 不应关闭权限面板");
        assert!(matches!(
            t.handle_key(Key::Enter),
            Action::PermSubmit(crate::tools::PermDecisionKind::Yes)
        ));
        t.finish_perm_prompt();
        assert!(!t.is_perm_prompt());
        open(&mut t);
        let _ = t.handle_key(Key::Down);
        let _ = t.handle_key(Key::Down);
        assert!(matches!(
            t.handle_key(Key::Enter),
            Action::PermSubmit(crate::tools::PermDecisionKind::No)
        ));
        t.finish_perm_prompt();
        open(&mut t);
        let _ = t.handle_key(Key::Down);
        let _ = t.handle_key(Key::Down);
        let _ = t.handle_key(Key::Up);
        assert!(matches!(
            t.handle_key(Key::Enter),
            Action::PermSubmit(crate::tools::PermDecisionKind::AlwaysAllow)
        ));
        t.finish_perm_prompt();
        open(&mut t);
        let _ = t.handle_key(Key::Tab);
        assert!(matches!(
            t.handle_key(Key::Enter),
            Action::PermSubmit(crate::tools::PermDecisionKind::AlwaysAllow)
        ));
        t.finish_perm_prompt();
        open(&mut t);
        assert!(matches!(
            t.handle_key(Key::Esc),
            Action::PermSubmit(crate::tools::PermDecisionKind::No)
        ));
        t.finish_perm_prompt();
        open(&mut t);
        assert!(matches!(
            t.handle_key(Key::Char('1')),
            Action::PermSubmit(crate::tools::PermDecisionKind::Yes)
        ));
        t.finish_perm_prompt();
        open(&mut t);
        assert!(matches!(
            t.handle_key(Key::Char('2')),
            Action::PermSubmit(crate::tools::PermDecisionKind::AlwaysAllow)
        ));
        t.finish_perm_prompt();
        open(&mut t);
        assert!(matches!(
            t.handle_key(Key::Char('3')),
            Action::PermSubmit(crate::tools::PermDecisionKind::No)
        ));
        t.finish_perm_prompt();
        open(&mut t);
        assert!(matches!(
            t.handle_key(Key::Char('4')),
            Action::PermSubmit(crate::tools::PermDecisionKind::AutoEnable)
        ));
        t.finish_perm_prompt();
        open(&mut t);
        let _ = t.handle_key(Key::Down);
        let _ = t.handle_key(Key::Down);
        let _ = t.handle_key(Key::Down);
        assert!(matches!(
            t.handle_key(Key::Enter),
            Action::PermSubmit(crate::tools::PermDecisionKind::AutoEnable)
        ));
        t.finish_perm_prompt();
        open(&mut t);
        let _ = t.handle_key(Key::Up);
        assert!(matches!(
            t.handle_key(Key::Enter),
            Action::PermSubmit(crate::tools::PermDecisionKind::AutoEnable)
        ));
        t.finish_perm_prompt();
        open(&mut t);
        let _ = t.handle_key(Key::Char('x'));
        assert_eq!(t.input, "", "批准模式下输入被拦截");
        assert_eq!(t.handle_key(Key::Char('y')), Action::None);
    }
    #[test]
    fn perm_panel_renders_command_and_options() {
        let mut t = Tui::new();
        t.w = 40;
        t.h = 24;
        t.open_perm_prompt(crate::tools::PermRequest {
            id: 1,
            cmd: "cargo build --release".into(),
            cmd_kind: "cargo".into(),
        });
        let panel = t.build_perm_panel(20);
        let plain = strip_ansi(&panel.lines.join("\n"));
        assert!(plain.contains("模型想执行命令"), "警示标题: {plain}");
        assert!(plain.contains("cargo build --release"), "命令全文: {plain}");
        assert!(plain.contains("Yes，允许执行（仅此一次）"), "选项 1: {plain}");
        assert!(plain.contains("本会话内全部同意 cargo 类命令"), "选项 2: {plain}");
        assert!(plain.contains("No，拒绝执行"), "选项 3: {plain}");
        assert!(plain.contains("开启 autodangerous（不再询问）"), "选项 4: {plain}");
        assert!(plain.contains("↑↓ 选择 · 回车确认 · Esc 拒绝"), "快捷键提示: {plain}");
        let yes_line = panel
            .lines
            .iter()
            .find(|l| l.contains("允许执行（仅此一次）"))
            .unwrap();
        assert!(yes_line.contains('❯'), "焦点行应带指针: {yes_line}");
        let no_line = panel
            .lines
            .iter()
            .find(|l| l.contains("拒绝执行"))
            .unwrap();
        assert!(!no_line.contains('❯'), "未聚焦行不应有指针: {no_line}");
    }
    #[test]
    fn perm_panel_frame_overlays_input() {
        let mut t = Tui::new();
        t.w = 60;
        t.h = 20;
        t.set_header("test".into());
        t.open_perm_prompt(crate::tools::PermRequest {
            id: 1,
            cmd: "ls /tmp".into(),
            cmd_kind: "ls".into(),
        });
        let frame = strip_ansi(&t.build_frame());
        assert!(frame.contains("模型想执行命令"), "面板应渲染: {frame}");
        assert!(frame.contains("ls /tmp"), "命令应渲染: {frame}");
        assert!(!frame.contains("Try \"ask about this project\""), "输入框占位不应出现");
        assert!(!frame.contains("⏸ 本地模式"), "全局 hint 不应出现（被面板替换）");
    }
    #[test]
    fn key_parser_escape_sequences() {
        let mut p = KeyParser::new();
        assert!(p.feed(0x1b).is_none());
        assert!(p.feed(b'[').is_none());
        assert!(matches!(p.feed(b'A'), Some(Key::Up)));
        let mut p = KeyParser::new();
        assert!(p.feed(0x1b).is_none());
        assert!(p.feed(b'[').is_none());
        assert!(p.feed(b'5').is_none());
        assert!(matches!(p.feed(b'~'), Some(Key::PageUp)));
    }
    #[test]
    fn key_parser_utf8() {
        let mut p = KeyParser::new();
        assert!(p.feed(0xe4).is_none());     
        assert!(p.feed(0xb8).is_none());
        assert!(matches!(p.feed(0xad), Some(Key::Char('中'))));
    }
    #[test]
    fn cjk_edit_no_panic() {
        let mut t = Tui::new();
        assert_eq!(t.handle_key(Key::Char('你')), Action::None);
        assert_eq!(t.handle_key(Key::Char('好')), Action::None);                         
        assert_eq!(t.handle_key(Key::Backspace), Action::None);
        assert_eq!(t.input, "你");
        assert_eq!(t.handle_key(Key::Char('a')), Action::None);                 
        assert_eq!(t.handle_key(Key::Left), Action::None);            
        assert_eq!(t.handle_key(Key::Char('b')), Action::None);         
        assert_eq!(t.input, "你ba");
        assert_eq!(t.handle_key(Key::Delete), Action::None);                  
        assert_eq!(t.input, "你b");
        assert_eq!(t.handle_key(Key::CtrlC), Action::None);
        assert_eq!(t.input, "");
    }
    #[test]
    fn truncate_width_marks() {
        let t = truncate_width("1234567890", 5);
        assert!(t.ends_with('…'));
        assert!(t.width() <= 5);
    }
    #[test]
    fn frame_visual_elements() {
        let mut t = Tui::new();
        t.w = 80;
        t.h = 26;
        t.set_header("deepseek-v4-flash │ 会话 main │ native".into());
        t.set_ctx_estimate(500_000, 1_000_000);
        let welcome = t.build_frame();
        let welcome_plain = strip_ansi(&welcome);
        assert!(welcome_plain.contains("Yujiale Code v"), "欢迎卡品牌");
        assert!(welcome_plain.contains("Welcome back!"), "Claude Code 式欢迎卡");
        assert!(welcome_plain.contains("Tips for getting started"), "欢迎卡双栏提示");
        assert!(welcome_plain.contains("Meet Y仔"), "原创吉祥物说明");
        assert!(welcome_plain.contains("READY"), "初始微笑状态");
        t.push_user("你好".into());
        t.push_tool("portscan", r#"{"host":"127.0.0.1","ports":"22,1"}"#);
        t.push_tool_result("22 开放\n80 关闭");
        t.begin_assistant();
        t.assistant_delta("正在扫描");
        let frame = t.build_frame();
        assert!(strip_ansi(&frame).contains("Yujiale Code v"), "对话开始后欢迎卡仍固定");
        assert!(strip_ansi(&frame).contains("THINK"), "生成中切换思考状态");
        assert!(strip_ansi(&frame).contains("◔ ◔"), "思考时眼睛改变");
        assert!(frame.contains("\x1b[K"), "整帧重画应逐行清屏");
        assert!(frame.contains("◐"), "生成中状态灯");
        assert!(
            strip_ansi(&frame).contains("ctx ~500k/1.00m (50.00%)"),
            "上下文状态"
        );
        assert!(frame.contains("\x1b[48;5;237m"), "用户消息背景块");
        let plain = strip_ansi(&frame);
        assert!(plain.contains("你好"), "用户消息可见");
        assert!(!plain.contains("❯ 你好"), "用户消息无 ❯ 前缀");
        assert!(plain.contains("● portscan("), "工具调用行");
        assert!(frame.contains("\x1b[1mportscan\x1b[0m"), "工具名粗体");
        assert!(plain.contains("⎿  22 开放") && plain.contains("80 关闭"), "工具结果续行");
        assert!(plain.contains("● 正在扫描"), "助手前缀行");
        assert!(!plain.contains("[你]"), "无角色徽章");
        assert!(plain.contains("❯ "), "输入提示符");
        assert!(plain.matches(&"─".repeat(80)).count() >= 2, "输入区上下边界");
        assert!(plain.contains('╭') && plain.contains('╰'), "圆角只属于固定欢迎卡");
        t.end_assistant("正在扫描完成");
        let frame2 = t.build_frame();
        assert!(frame2.contains('●'), "空闲状态灯");
        assert!(!frame2.contains('▊'), "无流式光标");
        assert!(strip_ansi(&frame2).contains("正在扫描完成"), "助手完成行");
        assert!(strip_ansi(&frame2).contains("DONE"), "任务完成切换成功微笑");
        assert!(strip_ansi(&frame2).contains("⌒ ⌒"), "成功时眯眼微笑");
    }
    #[test]
    fn mascot_state_machine_covers_ask_tool_error_bug_and_success() {
        use crate::tools::AskOption;
        let mut t = Tui::new();
        assert_eq!(t.mascot_state, MascotState::Idle);
        t.begin_assistant();
        assert_eq!(t.mascot_state, MascotState::Thinking);
        t.push_tool("Read", r#"{"file_path":"/missing"}"#);
        assert_eq!(t.mascot_state, MascotState::Tool);
        t.push_tool_result("错误: 文件不存在");
        assert_eq!(t.mascot_state, MascotState::Angry);
        assert!(mascot_art(t.mascot_state).join("\n").contains("◣ ◢"));
        t.push_bug_warn("垃圾 token 检测：<unused>".into());
        assert_eq!(t.mascot_state, MascotState::Frantic);
        assert!(mascot_art(t.mascot_state).join("\n").contains("⊙ ⊙"));
        t.cancel_streaming();
        assert_eq!(t.mascot_state, MascotState::Idle);
        t.ask(vec![AskQuestion {
            question: "继续吗？".into(),
            header: "确认".into(),
            options: vec![AskOption {
                label: "继续".into(),
                description: "继续执行".into(),
                preview: None,
            }],
            multi_select: false,
        }]);
        assert_eq!(t.mascot_state, MascotState::Asking);
        assert!(mascot_art(t.mascot_state).join("\n").contains('?'));
        t.finish_ask();
        assert_eq!(t.mascot_state, MascotState::Thinking);
        t.end_assistant("完成");
        assert_eq!(t.mascot_state, MascotState::Success);
    }
    #[test]
    fn shell_progress_updates_one_row_and_final_result_replaces_it() {
        let mut t = Tui::new();
        t.push_tool("execute_command", r#"{"cmd":"printf first; sleep 2"}"#);
        let before = t.chat.len();
        t.push_tool_progress(&crate::tools::ToolProgress {
            output: "first".into(),
            elapsed_secs: 1,
            total_lines: 1,
            total_bytes: 5,
        });
        assert_eq!(t.chat.len(), before + 1);
        let progress_index = t.tool_progress_index.unwrap();
        assert!(t.chat[progress_index].text.contains("first"));
        assert!(t.chat[progress_index].text.contains("Running… 1s"));
        t.push_tool_progress(&crate::tools::ToolProgress {
            output: "first\nsecond".into(),
            elapsed_secs: 2,
            total_lines: 2,
            total_bytes: 12,
        });
        assert_eq!(t.chat.len(), before + 1, "progress must update instead of appending rows");
        assert!(t.chat[progress_index].text.contains("second"));
        t.push_tool_result("exit code: 0\nfirst\nsecond\n");
        assert_eq!(t.chat.len(), before + 1);
        assert!(t.tool_progress_index.is_none());
        assert!(!t.chat[progress_index].pending);
        assert!(t.chat[progress_index].text.contains("exit code: 0"));
    }
    #[test]
    fn fixed_welcome_header_is_identical_while_chat_scrolls() {
        let mut t = Tui::new();
        t.w = 90;
        t.h = 30;
        t.set_header("local-model │ 会话 main │ native".into());
        for index in 0..30 {
            t.push_system(format!("对话行 {index}"));
        }
        let header_h = t.fixed_header_height();
        let bottom = strip_ansi(&t.build_frame());
        let bottom_header: Vec<&str> = bottom.lines().take(header_h).collect();
        let _ = t.handle_key(Key::PageUp);
        assert!(t.scroll > 0);
        let scrolled = strip_ansi(&t.build_frame());
        let scrolled_header: Vec<&str> = scrolled.lines().take(header_h).collect();
        assert_eq!(bottom_header, scrolled_header, "滚动只能改变欢迎卡下方的聊天视口");
        assert_ne!(bottom, scrolled, "对话区域本身应发生滚动");
        assert!(scrolled_header.iter().any(|line| line.contains("Yujiale Code v")));
        assert!(!scrolled_header.iter().any(|line| line.contains("对话行")));
    }
    #[test]
    fn structured_ask_user_walks_questions_and_returns_answer_map() {
        use crate::tools::AskOption;
        let mut t = Tui::new();
        t.w = 90;
        t.h = 30;
        t.ask(vec![
            AskQuestion {
                question: "使用哪种认证方式？".into(),
                header: "认证".into(),
                options: vec![
                    AskOption {
                        label: "OAuth（推荐）".into(),
                        description: "标准协议，支持多个提供商".into(),
                        preview: None,
                    },
                    AskOption {
                        label: "JWT".into(),
                        description: "无状态，适合 API".into(),
                        preview: None,
                    },
                ],
                multi_select: false,
            },
            AskQuestion {
                question: "启用哪些能力？".into(),
                header: "能力".into(),
                options: vec![
                    AskOption { label: "日志".into(), description: "记录运行信息".into(), preview: None },
                    AskOption { label: "指标".into(), description: "采集性能数据".into(), preview: None },
                    AskOption { label: "告警".into(), description: "异常时通知".into(), preview: None },
                ],
                multi_select: true,
            },
        ]);
        let first = strip_ansi(&t.build_frame());
        assert!(first.contains("☐ 认证"), "应显示 Claude Code 式问题页签");
        assert!(first.contains("使用哪种认证方式？"));
        assert!(first.contains("❯ 1. OAuth（推荐）"));
        assert!(first.contains("标准协议，支持多个提供商"), "说明在选项下一行");
        assert!(first.contains("3. Type something."), "Other 本身就是输入行");
        assert!(!first.contains("Try \"ask about this project\""), "Ask overlay 必须隐藏普通输入框");
        assert!(first.contains("Enter to select · ↑/↓ to navigate · Esc to cancel"));
        assert_eq!(t.handle_key(Key::Down), Action::None);
        assert_eq!(t.handle_key(Key::Enter), Action::None, "第一题后进入第二题");
        let second = strip_ansi(&t.build_frame());
        assert!(second.contains("☒ 认证"));
        assert!(second.contains("☐ 能力"));
        assert!(second.contains("❯ 1. [ ] 日志"));
        assert!(second.contains("Submit"));
        assert_eq!(t.handle_key(Key::Enter), Action::None);        
        assert_eq!(t.handle_key(Key::Down), Action::None);      
        assert_eq!(t.handle_key(Key::Down), Action::None);      
        assert_eq!(t.handle_key(Key::Char(' ')), Action::None);        
        assert_eq!(t.handle_key(Key::Down), Action::None);         
        for c in "自定义".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        let with_other = strip_ansi(&t.build_frame());
        assert!(with_other.contains("❯ 4. [✓] 自定义"), "Other 行内输入并自动勾选");
        assert_eq!(t.handle_key(Key::Down), Action::None);          
        let Action::AskSubmit(answers) = t.handle_key(Key::Enter) else {
            panic!("最后一题应提交结构化答案");
        };
        assert_eq!(answers["使用哪种认证方式？"], "JWT");
        assert_eq!(answers["启用哪些能力？"], "日志, 告警, 自定义");
    }
    #[test]
    fn structured_ask_ignores_typing_on_options_and_accepts_inline_other() {
        use crate::tools::AskOption;
        let mut t = Tui::new();
        t.ask(vec![AskQuestion {
            question: "怎么处理？".into(),
            header: "处理".into(),
            options: vec![
                AskOption { label: "自动".into(), description: "自动处理".into(), preview: None },
                AskOption { label: "手动".into(), description: "人工处理".into(), preview: None },
            ],
            multi_select: false,
        }]);
        let _ = t.handle_key(Key::Char('9'));
        assert!(t.is_asking());
        assert_eq!(t.input, "", "焦点不在 Other 时不能把字符写进普通输入框");
        let _ = t.handle_key(Key::Down);
        let _ = t.handle_key(Key::Down);
        for c in "稍后再决定".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        let frame = strip_ansi(&t.build_frame());
        assert!(frame.contains("❯ 3. 稍后再决定"));
        assert!(!frame.contains("Try \"ask about this project\""));
        let Action::AskSubmit(answers) = t.handle_key(Key::Enter) else {
            panic!("自由文本应作为 Other 提交");
        };
        assert_eq!(answers["怎么处理？"], "稍后再决定");
    }
    #[test]
    fn provider_setup_reuses_ask_ui_but_returns_config_action() {
        let mut t = Tui::new();
        t.open_provider_setup(&crate::config::Config::default());
        assert_eq!(t.handle_key(Key::Enter), Action::None);
        for c in "sk-hidden-test".chars() {
            assert_eq!(t.handle_key(Key::Char(c)), Action::None);
        }
        assert!(!strip_ansi(&t.build_frame()).contains("sk-hidden-test"));
        assert_eq!(t.handle_key(Key::Enter), Action::None);
        assert!(!strip_ansi(&t.build_frame()).contains("sk-hidden-test"));
        assert_eq!(t.handle_key(Key::Enter), Action::None);
        let Action::ConfigSubmit(answers) = t.handle_key(Key::Enter) else {
            panic!("供应商配置最后一步应返回独立配置动作");
        };
        assert_eq!(answers[crate::setup::PROVIDER_QUESTION], "DeepSeek");
        assert_eq!(answers[crate::setup::MODEL_QUESTION], "DeepSeek V4 Flash");
        assert_eq!(answers[crate::setup::AGENT_QUESTION], "开启（单并发）");
    }
    #[test]
    fn structured_ask_escape_cancels_and_tiny_terminal_keeps_header() {
        use crate::tools::AskOption;
        let mut t = Tui::new();
        t.w = 60;
        t.h = 14;
        t.ask(vec![AskQuestion {
            question: "选择一个方案".into(),
            header: "方案".into(),
            options: vec![
                AskOption { label: "A".into(), description: "说明 A".into(), preview: None },
                AskOption { label: "B".into(), description: "说明 B".into(), preview: None },
                AskOption { label: "C".into(), description: "说明 C".into(), preview: None },
            ],
            multi_select: false,
        }]);
        assert_eq!(t.handle_key(Key::CtrlC), Action::None);
        assert!(t.is_asking(), "Ctrl+C 不应打断 ask_user");
        let plain = strip_ansi(&t.build_frame());
        assert!(plain.contains("Yujiale Code v"), "小终端仍保留固定欢迎栏");
        assert!(plain.contains("选择一个方案"));
        assert!(plain.contains("❯ 1. A"), "选项窗口至少显示当前项");
        assert_eq!(t.handle_key(Key::Esc), Action::Cancel);
    }
    #[test]
    fn strip_tool_blocks_removes_blocks() {
        let stripped = Tui::strip_tool_blocks("先看看\n```tool {\"op\":\"x\"}\n```\n然后");
        assert_eq!(stripped, "先看看\n\n然后");
        let s2 = Tui::strip_tool_blocks("前```tool {\"op\":\"x\"}");
        assert_eq!(s2, "前");
        let s3 = Tui::strip_tool_blocks("```json\n{\"a\":1}\n```");
        assert_eq!(s3, "```json\n{\"a\":1}\n```");
        assert_eq!(Tui::strip_tool_blocks("你好"), "你好");
    }
    #[test]
    fn thinking_phase_cursor_not_on_user_block() {
        let mut t = Tui::new();
        t.w = 80;
        t.h = 24;
        t.push_user("你好".into());
        t.begin_assistant();                              
        let frame = t.build_frame();
        let user_line = frame.lines().find(|l| l.contains("\x1b[48;5;237m")).unwrap();
        assert!(!user_line.contains('▊'), "光标不能落在用户消息块上: {user_line:?}");
        let plain = strip_ansi(&frame);
        assert!(plain.contains("● "), "应有助手占位行: {plain:?}");
        t.assistant_delta("正在处理");
        let frame2 = t.build_frame();
        let plain2 = strip_ansi(&frame2);
        assert!(plain2.contains("● 正在处理 ▊"), "光标应跟随助手行");
        let user_line2 = frame2.lines().find(|l| l.contains("\x1b[48;5;237m")).unwrap();
        assert!(!user_line2.contains('▊'));
    }
    #[test]
    fn queued_count_does_not_split_streaming_assistant_line() {
        let mut t = Tui::new();
        t.w = 80;
        t.h = 24;
        t.begin_assistant();
        t.assistant_delta("前半");
        t.set_queued_count(2);
        t.assistant_delta("后半");
        let pending: Vec<_> = t.chat.iter().filter(|line| line.pending).collect();
        assert_eq!(pending.len(), 1, "排队提示不能插进聊天记录并切断流式消息");
        assert_eq!(pending[0].text, "前半后半");
        assert!(strip_ansi(&t.build_frame()).contains("排队 2"));
    }
    #[test]
    fn input_wrap_ranges_tracks_char_ranges() {
        let lines = wrap_input_ranges("你好hello世界", 6);
        assert_eq!(lines.len(), 3);
        assert_eq!((lines[0].0.as_str(), lines[0].1, lines[0].2), ("你好he", 0, 4));
        assert_eq!((lines[1].0.as_str(), lines[1].1, lines[1].2), ("llo世", 4, 8));
        assert_eq!((lines[2].0.as_str(), lines[2].1, lines[2].2), ("界", 8, 9));
        let lines = wrap_input_ranges("a\n\nb", 10);
        assert_eq!(lines.len(), 3);
        assert_eq!((lines[0].0.as_str(), lines[0].1, lines[0].2), ("a", 0, 2));
        assert_eq!((lines[1].0.as_str(), lines[1].1, lines[1].2), ("", 2, 3));
        assert_eq!((lines[2].0.as_str(), lines[2].1, lines[2].2), ("b", 3, 4));
    }
    #[test]
    fn cursor_line_info_locates_multiline_cursor() {
        let mut t = Tui::new();
        t.w = 21;
        t.input = "abcdefghij一二三四五六".into();
        t.cursor = 3;
        let (line, col, s, e) = t.cursor_line_info().unwrap();
        assert_eq!((line, col, s, e), (0, 3, 0, 14));
        t.cursor = 14;
        let (line, col, s, e) = t.cursor_line_info().unwrap();
        assert_eq!((line, col, s, e), (1, 0, 14, 16));
        t.cursor = 15;
        let (line, col, s, e) = t.cursor_line_info().unwrap();
        assert_eq!((line, col, s, e), (1, 2, 14, 16));
        t.cursor = 16;
        let (line, col, _, _) = t.cursor_line_info().unwrap();
        assert_eq!((line, col), (1, 4));
    }
    #[test]
    fn cursor_move_up_down_preserves_column() {
        let mut t = Tui::new();
        t.w = 21;
        t.input = "abcdefghij一二三四五六".into();                  
        t.cursor = 16;
        t.cursor_move(-1);
        assert_eq!(t.cursor, 4, "保持列位置");
        let (line, col, _, _) = t.cursor_line_info().unwrap();
        assert_eq!((line, col), (0, 4));
        t.cursor_move(-1);
        assert_eq!(t.cursor, 4);
        t.cursor_move(1);
        let (line, col, _, _) = t.cursor_line_info().unwrap();
        assert_eq!((line, col), (1, 4));
        t.cursor = 13;
        t.cursor_move(1);
        assert_eq!(t.cursor, 16, "短行应停在行尾");
        t.cursor = 14;
        let (line, col, _, _) = t.cursor_line_info().unwrap();
        assert_eq!((line, col), (1, 0));
        t.cursor_move(1);
        assert_eq!(t.cursor, 14);
        t.input = "单行".into();
        t.cursor = 2;
        t.cursor_move(-1);
        assert_eq!(t.cursor, 2);
    }
    #[test]
    fn home_end_jump_to_line_bounds() {
        let mut t = Tui::new();
        t.w = 21;
        t.input = "abcdefghij一二三四五六".into();
        t.cursor = 15;
        let _ = t.handle_key(Key::Home);
        assert_eq!(t.cursor, 14);
        let _ = t.handle_key(Key::End);
        assert_eq!(t.cursor, 16);
        t.input = "abc".into();
        t.cursor = 1;
        let _ = t.handle_key(Key::Home);
        assert_eq!(t.cursor, 0);
        let _ = t.handle_key(Key::End);
        assert_eq!(t.cursor, 3);
    }
    #[test]
    fn tab_completes_unique_command_case_insensitively() {
        let mut t = Tui::new();
        t.set_commands(&[("help", "查看命令帮助"), ("ls", "列出会话"), ("tool_times", "设置轮数")]);
        for c in "/HE".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        let _ = t.handle_key(Key::Tab);
        assert_eq!(t.input, "/help ");

        let mut t = Tui::new();
        t.set_commands(&[("help", "a")]);
        for c in "h".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        let _ = t.handle_key(Key::Tab);
        assert_eq!(t.input, "h");
    }
    #[test]
    fn tab_expands_common_prefix_then_cycles_candidates() {
        let mut t = Tui::new();
        t.set_commands(&[("model", "a"), ("models", "b"), ("maxtokens", "c")]);
        for c in "/mo".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        let _ = t.handle_key(Key::Tab);
        assert_eq!(t.input, "/model", "第一次 Tab 扩展最长公共前缀");
        let _ = t.handle_key(Key::Tab);
        assert_eq!(t.input, "/models ", "再次 Tab 选择下一个候选");
        let _ = t.handle_key(Key::Tab);
        assert_eq!(t.input, "/model ", "候选应循环");

        let mut t = Tui::new();
        t.set_commands(&[("help", "a"), ("ls", "b")]);
        let _ = t.handle_key(Key::Char('/'));
        let _ = t.handle_key(Key::Tab);
        assert_eq!(t.input, "/help ", "无公共前缀时选择首项");
        let _ = t.handle_key(Key::Tab);
        assert_eq!(t.input, "/ls ", "连续 Tab 循环候选");
    }
    #[test]
    fn tab_completion_uses_cursor_and_preserves_arguments() {
        let mut t = Tui::new();
        t.set_commands(&[("help", "a"), ("history", "b")]);
        t.input = "/hexx --verbose".into();
        t.cursor = 3;
        let _ = t.handle_key(Key::Tab);
        assert_eq!(t.input, "/help --verbose");
        assert_eq!(t.cursor, 6, "光标应位于参数开头");

        let _ = t.handle_key(Key::Char('x'));
        assert_eq!(t.input, "/help x--verbose");
        let _ = t.handle_key(Key::Tab);
        assert_eq!(t.input, "/help x--verbose", "编辑后不得沿用旧候选状态");
    }
    #[test]
    fn pasted_tab_is_inserted_instead_of_triggering_completion() {
        let mut t = Tui::new();
        t.set_commands(&[("help", "a")]);
        let _ = t.handle_key(Key::PasteStart);
        for c in "/he".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        let _ = t.handle_key(Key::Tab);
        let _ = t.handle_key(Key::PasteEnd);
        assert_eq!(t.input, "/he\t");
    }
    #[test]
    fn command_popup_filters_and_styles() {
        let mut t = Tui::new();
        t.w = 60;
        t.set_commands(&[("help", "查看命令帮助"), ("ls", "列出会话"), ("tool_times", "设置轮数")]);
        let (lines, total) = t.command_popup();
        assert!(lines.is_empty() && total == 0);
        for c in "/".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        let (lines, total) = t.command_popup();
        assert_eq!(total, 3);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains(&format!("{C_BOLD}/help{C_RESET}")), "行0: {}", lines[0]);
        assert!(lines[0].contains("查看命令帮助"));
        for c in "to".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        let (lines, total) = t.command_popup();
        assert_eq!(total, 1);
        assert!(lines[0].contains(&format!("{C_BOLD}/to{C_RESET}")), "行: {}", lines[0]);
        assert!(lines[0].contains("\x1b[1m/to\x1b[0m"), "命中段粗体: {}", lines[0]);
        for c in "xyz".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        let (lines, _) = t.command_popup();
        assert!(lines.is_empty());
    }
    #[test]
    fn multiline_input_frame_wraps_and_cursor() {
        let mut t = Tui::new();
        t.w = 40;
        t.h = 20;
        t.set_commands(&[("help", "查看命令帮助")]);
        t.push_user("你好".into());
        for c in "一二三四五六七八九十一二三四五六七八九十一二三四五六七八九十一二三四五六七八九十".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        let frame = t.build_frame();
        let plain = strip_ansi(&frame);
        let border = "─".repeat(40);
        assert!(plain.matches(&border).count() >= 2, "上下边界");
        let mut sections = plain.split(&border);
        let _before = sections.next();
        let box_region = sections.next().unwrap_or("");
        let input_lines: Vec<&str> = box_region.lines().filter(|line| line.contains('一')).collect();
        assert!(input_lines.len() >= 2, "输入应折成多行: {plain:?}");
        assert!(plain.contains("你好"), "聊天区应保留: {plain:?}");
        assert!(frame.contains(&format!("{C_CURSOR} {C_RESET}")), "块光标存在");
        let idx = frame.rfind(C_CURSOR).unwrap();
        let esc = frame[..idx].rsplit('\x1b').next().unwrap();                   
        let pos = esc.trim_start_matches('[').trim_end_matches('H');                 
        let row: usize = pos.split(';').next().unwrap().parse().unwrap_or(0);
        assert!(row == 18, "光标应在输入框末行: row={row}");
    }
    #[test]
    fn slash_popup_shown_in_frame() {
        let mut t = Tui::new();
        t.w = 60;
        t.h = 20;
        t.set_commands(&[("help", "查看命令帮助"), ("tool_times", "设置轮数"), ("ls", "列出会话")]);
        let _ = t.handle_key(Key::Char('/'));
        let frame = t.build_frame();
        let plain = strip_ansi(&frame);
        assert!(plain.contains("/help"), "菜单应显示命令: {plain:?}");
        assert!(plain.contains("/tool_times"), "菜单应显示命令: {plain:?}");
        assert!(plain.contains("查看命令帮助"), "菜单应显示说明");
        let border = "─".repeat(60);
        let after_border = plain.split(&border).nth(2).unwrap_or("");
        assert!(after_border.contains("/help"), "菜单应紧跟输入框: {after_border:?}");
        t.input = String::new();
        t.cursor = 0;
        let frame2 = t.build_frame();
        let plain2 = strip_ansi(&frame2);
        assert!(!plain2.contains("/tool_times"), "无 / 输入时菜单消失: {plain2:?}");
    }
    #[test]
    fn pending_tool_block_line_not_shown() {
        let mut t = Tui::new();
        t.w = 60;
        t.h = 10;
        t.push_user("搜索马斯克".into());
        t.begin_assistant();
        t.assistant_delta("```tool {\"op\":\"web_search\",\"args\":{\"query\":\"马斯克\"}}\n```");
        t.push_tool("web_search", r#"{"query":"马斯克"}"#);
        t.streaming = true;
        let frame = t.build_frame();
        let plain = strip_ansi(&frame);
        let dots = plain.matches('●').count();
        assert_eq!(dots, 1, "不应有多余的空 ● 行: {plain:?}");
        assert!(plain.contains("● web_search"), "工具行正常显示");
    }
    #[test]
    fn key_parser_sgr_mouse_wheel() {
        let mut p = KeyParser::new();
        for b in "\x1b[<64;5;3M".bytes() {
            if let Some(k) = p.feed(b) {
                assert_eq!(k, Key::WheelUp);
                return;
            }
        }
        panic!("未解析出滚轮上事件");
    }
    #[test]
    fn alternate_screen_enables_distinct_mouse_events() {
        assert!(ENABLE_MOUSE_TRACKING.contains("?1000h"));
        assert!(ENABLE_MOUSE_TRACKING.contains("?1002h"));
        assert!(ENABLE_MOUSE_TRACKING.contains("?1006h"));
        assert!(DISABLE_MOUSE_TRACKING.contains("?1006l"));
    }
    #[test]
    fn key_parser_sgr_mouse_down_drag_release() {
        let mut p = KeyParser::new();
        for b in "\x1b[<0;10;5M".bytes() {
            if let Some(k) = p.feed(b) {
                assert_eq!(k, Key::MouseDown { row: 5, col: 10 });
                break;
            }
        }
        let mut p = KeyParser::new();
        for b in "\x1b[<32;3;20M".bytes() {
            if let Some(k) = p.feed(b) {
                assert_eq!(k, Key::MouseDrag { row: 20, col: 3 });
                break;
            }
        }
        let mut p = KeyParser::new();
        for b in "\x1b[<0;10;5m".bytes() {
            if let Some(k) = p.feed(b) {
                assert_eq!(k, Key::MouseUp { row: 5, col: 10 });
                return;
            }
        }
        panic!("未解析出鼠标按下/拖拽/松开事件");
    }
    #[test]
    fn key_parser_bracketed_paste() {
        let mut p = KeyParser::new();
        let mut got = Vec::new();
        for b in "\x1b[200~abc\x1b[201~".bytes() {
            if let Some(k) = p.feed(b) {
                got.push(k);
            }
        }
        assert_eq!(got[0], Key::PasteStart);
        assert_eq!(got[1], Key::Char('a'));
        assert_eq!(got[2], Key::Char('b'));
        assert_eq!(got[3], Key::Char('c'));
        assert_eq!(got[4], Key::PasteEnd);
    }
    #[test]
    fn wheel_scrolls_viewport_not_history() {
        let mut t = Tui::new();
        t.w = 60;
        t.h = 10;
        t.push_user("第一条".into());
        t.push_user("第二条".into());
        t.push_user("第三条".into());
        t.history.push("上一条消息".into());
        let a = t.handle_key(Key::WheelUp);
        assert_eq!(a, Action::None);
        assert!(t.scroll > 0, "滚轮应滚动视口");
        assert!(t.hist_pos.is_none(), "滚轮不应触发历史回填");
        assert!(t.input.is_empty(), "输入框不应被历史填充");
        let a = t.handle_key(Key::WheelDown);
        assert_eq!(a, Action::None);
        assert_eq!(t.scroll, 0, "滚轮下应回到底部");
    }
    #[test]
    fn only_escape_interrupts_a_running_turn() {
        let mut t = Tui::new();
        t.begin_assistant();
        assert_eq!(t.handle_key(Key::CtrlC), Action::None);
        assert!(t.is_streaming(), "Ctrl+C 不应改变运行状态");
        assert_eq!(t.handle_key(Key::Esc), Action::Cancel);
    }
    #[test]
    fn tool_block_not_shown_in_frame() {
        let mut t = Tui::new();
        t.w = 60;
        t.h = 10;
        t.push_user("你好".into());
        t.begin_assistant();
        t.assistant_delta("我需要先查看工具。\n```tool {\"op\":\"list_tools\"}\n```\n");
        t.end_assistant("");
        let frame = t.build_frame();
        assert!(!frame.contains("```tool"), "工具块不应裸奔显示");
        assert!(strip_ansi(&frame).contains("你好"), "用户消息正常显示");
    }
    #[test]
    fn user_block_and_tool_line_styles() {
        let mut t = Tui::new();
        t.w = 60;
        t.h = 13;
        t.push_user("你好".into());
        t.push_tool("portscan", r#"{"host":"127.0.0.1"}"#);
        let frame = t.build_frame();
        let plain = strip_ansi(&frame);
        assert!(plain.contains("你好"), "用户消息无前缀");
        assert!(frame.contains("\x1b[48;5;237m\x1b[1;97m你好"), "用户消息灰底+粗体亮白字");
        assert!(plain.contains("● portscan({\"host\":\"127.0.0.1\"})"), "工具行 ● + (参数)");
        assert!(frame.contains("\x1b[1mportscan\x1b[0m"), "工具名粗体");
        t.push_system("系统提示".into());
        t.push_error("出错了".into());
        t.push_summary("已压缩".into());
        let frame2 = t.build_frame();
        let plain2 = strip_ansi(&frame2);
        assert!(plain2.contains("系统提示"), "系统行可见");
        assert!(plain2.contains("✗ 出错了"), "错误行可见");
        assert!(plain2.contains("◆ 已压缩"), "摘要行可见");
    }
    #[test]
    fn read_tool_ui_shows_the_complete_page_sent_to_the_model() {
        let mut t = Tui::new();
        t.w = 80;
        t.h = 30;
        t.push_user("读取文件".into());
        t.push_tool(
            "Read",
            r#"{"file_path":"/tmp/demo.rs","offset":10,"limit":3}"#,
        );
        t.push_tool_result("10\tfn one() {}\n11\tfn two() {}\n12\tfn three() {}");
        let plain = strip_ansi(&t.build_frame());
        assert!(plain.contains("● Read(/tmp/demo.rs · from line 10 · 3 lines)"));
        assert!(plain.contains("fn one() {}"));
        assert!(plain.contains("fn two() {}"));
        assert!(plain.contains("fn three() {}"));
        assert!(!plain.contains("Read 3 lines"), "不能用摘要隐藏文件正文");
    }
    #[test]
    fn shell_ls_ui_shows_the_complete_directory_page() {
        let mut t = Tui::new();
        t.w = 90;
        t.h = 30;
        t.push_user("列目录".into());
        t.push_tool(
            "execute_command",
            r#"{"cmd":"ls -la \"folder with spaces\"","cwd":"/tmp"}"#,
        );
        t.push_tool_result(
            "Directory: /tmp/folder with spaces\nEntries: 3 total; showing 1-2.\ndir\tsrc/\nfile\tCargo.toml\nNext page: listdir {\"path\":\"/tmp/folder with spaces\",\"offset\":2,\"limit\":2}\n",
        );
        let plain = strip_ansi(&t.build_frame());
        assert!(plain.contains("● List(/tmp/folder with spaces · 200 items)"));
        assert!(plain.contains("Directory: /tmp/folder with spaces"));
        assert!(plain.contains("Cargo.toml"));
        assert!(plain.contains("Next page: listdir"));
        assert!(!plain.contains("Listed 2 of 3 items"), "不能用摘要隐藏目录页");
        assert!(!plain.contains("…共"), "listdir 不再使用 400 字符折叠预览");
    }
    #[test]
    fn frame_streaming_light_and_cursor() {
        let mut t = Tui::new();
        t.w = 60;
        t.h = 8;
        t.begin_assistant();
        t.assistant_delta("hi");
        let frame = t.build_frame();
        assert!(frame.contains("◐"), "生成中状态灯");
        assert!(frame.contains('▊'), "流式块光标");
        t.end_assistant("hi");
        let frame2 = t.build_frame();
        assert!(!frame2.contains('▊'), "结束后无流式光标");
        assert!(frame2.contains('●'), "恢复空闲灯");
    }
    #[test]
    fn input_edit_ops() {
        let mut t = Tui::new();
        assert!(matches!(t.handle_key(Key::Char('h')), Action::None));
        assert!(matches!(t.handle_key(Key::Char('i')), Action::None));
        assert!(matches!(t.handle_key(Key::Left), Action::None));
        assert!(matches!(t.handle_key(Key::Char('x')), Action::None));
        assert_eq!(t.input, "hxi");
        assert!(matches!(t.handle_key(Key::Backspace), Action::None));
        assert_eq!(t.input, "hi");
        assert!(matches!(t.handle_key(Key::Enter), Action::Submit(s) if s == "hi"));
        assert!(t.input.is_empty());
    }
    #[test]
    fn slash_command_detection() {
        let mut t = Tui::new();
        for c in "/help".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        assert!(matches!(t.handle_key(Key::Enter), Action::Command(c) if c == "help"));
    }
    #[test]
    fn history_navigation() {
        let mut t = Tui::new();
        for c in "a".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        let _ = t.handle_key(Key::Enter);
        for c in "b".chars() {
            let _ = t.handle_key(Key::Char(c));
        }
        let _ = t.handle_key(Key::Enter);
        let _ = t.handle_key(Key::Up);
        assert_eq!(t.input, "b");
        let _ = t.handle_key(Key::Up);
        assert_eq!(t.input, "a");
        let _ = t.handle_key(Key::Down);
        assert_eq!(t.input, "b");
    }
    #[test]
    fn md_inline_rendered_in_frame() {
        let mut t = Tui::new();
        t.w = 60;
        t.h = 10;
        t.push_user("测试".into());
        t.begin_assistant();
        t.assistant_delta("**粗体** 和 *斜体* 与 `code`");
        t.end_assistant("**粗体** 和 *斜体* 与 `code`");
        let frame = t.build_frame();
        let plain = strip_ansi(&frame);
        assert!(plain.contains("粗体 和 斜体 与 code"), "可见文本: {plain:?}");
        assert!(!plain.contains("**"), "粗体标记不应裸奔: {plain:?}");
        assert!(!plain.contains("*斜体*"), "斜体标记不应裸奔: {plain:?}");
        assert!(!plain.contains('`'), "行内代码标记不应裸奔: {plain:?}");
        assert!(frame.contains("\x1b[1m粗体\x1b[0m"), "粗体 ANSI: {frame:?}");
        assert!(frame.contains("\x1b[3m斜体\x1b[0m"), "斜体 ANSI: {frame:?}");
        assert!(frame.contains("\x1b[48;5;234mcode\x1b[0m"), "行内代码底色: {frame:?}");
    }
    #[test]
    fn md_code_block_shaded_in_frame() {
        let mut t = Tui::new();
        t.w = 60;
        t.h = 10;
        t.push_user("测试".into());
        t.begin_assistant();
        t.assistant_delta("```rust\nfn main() {}\n```");
        t.end_assistant("```rust\nfn main() {}\n```");
        let frame = t.build_frame();
        let plain = strip_ansi(&frame);
        assert!(plain.contains("fn main() {}"), "代码可见: {plain:?}");
        assert!(!plain.contains("```"), "围栏不应显示: {plain:?}");
        assert!(frame.contains("\x1b[48;5;234m\x1b[37mfn main() {}"), "代码块整行灰底: {frame:?}");
    }
    #[test]
    fn md_header_bold_in_frame() {
        let mut t = Tui::new();
        t.w = 60;
        t.h = 10;
        t.push_user("测试".into());
        t.begin_assistant();
        t.assistant_delta("### 安装说明");
        t.end_assistant("### 安装说明");
        let frame = t.build_frame();
        let plain = strip_ansi(&frame);
        assert!(plain.contains("安装说明"), "标题可见: {plain:?}");
        assert!(!plain.contains("#"), "井号不应显示: {plain:?}");
        assert!(frame.contains("\x1b[1m安装说明\x1b[0m"), "标题粗体: {frame:?}");
    }
    #[test]
    fn md_quote_prefix_in_frame() {
        let mut t = Tui::new();
        t.w = 60;
        t.h = 10;
        t.push_user("测试".into());
        t.begin_assistant();
        t.assistant_delta("> 引用一段");
        t.end_assistant("> 引用一段");
        let frame = t.build_frame();
        let plain = strip_ansi(&frame);
        assert!(plain.contains("│ 引用一段"), "引用行: {plain:?}");
        assert!(!plain.contains("> 引用"), "引用标记应替换为 │: {plain:?}");
    }
    #[test]
    fn md_link_in_frame() {
        let mut t = Tui::new();
        t.w = 80;
        t.h = 10;
        t.push_user("测试".into());
        t.begin_assistant();
        t.assistant_delta("[官方文档](https://example.com)");
        t.end_assistant("[官方文档](https://example.com)");
        let frame = t.build_frame();
        let plain = strip_ansi(&frame);
        assert!(plain.contains("官方文档 (https://example.com)"), "链接行: {plain:?}");
    }
    #[test]
    fn reasoning_streams_merge_and_seal() {
        let mut t = Tui::new();
        t.w = 80;
        t.h = 24;
        t.push_reasoning("桌面".into());
        t.push_reasoning("\n环境".into());
        t.push_reasoning("是".into());
        assert_eq!(t.chat.len(), 1, "思考流 chunk 合并进同一行");
        assert!(t.chat[0].pending, "思考流进行中未封口");
        assert_eq!(t.chat[0].text, "桌面\n环境是", "chunk 顺序拼接");
        t.begin_assistant();
        assert!(!t.chat[0].pending, "正文开始后思考流封口");
        t.assistant_delta("正文");
        assert_eq!(t.chat.len(), 2, "正文是独立的助手行");
        t.push_reasoning("新一轮思考".into());
        assert_eq!(t.chat.len(), 3, "新思考流另起一行");
        assert_eq!(t.chat[2].text, "新一轮思考");
    }
    #[test]
    fn reasoning_long_line_collapsed_in_frame() {
        let mut t = Tui::new();
        t.w = 80;
        t.h = 24;
        let long = "思".repeat(1200);
        t.push_reasoning(long.clone());
        let styled: String = t
            .render_lines()
            .iter()
            .map(|r| strip_ansi(&r.styled))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(styled.contains("思考流已折叠"), "应有折叠提示: {styled:?}");
        let count = styled.chars().filter(|c| *c == '思').count();
        assert!((800..=805).contains(&count), "尾部 800 字符保留（+折叠提示里的字）: {styled:?}");
        assert!(!styled.contains(&"思".repeat(1200)), "完整长文本不渲染");
    }
    #[test]
    fn live_token_status_and_final_cost_are_visible() {
        let mut t = Tui::new();
        t.w = 240;
        t.h = 28;
        t.set_header("deepseek-v4-flash │ 会话 test │ native".into());
        t.set_pricing(crate::config::Pricing::deepseek_flash_cny());
        t.begin_assistant();
        t.stream_started = Some(std::time::Instant::now() - std::time::Duration::from_secs(50));
        t.token_progress(crate::llm::TokenProgress {
            usage: crate::llm::Usage {
                prompt_tokens: 2_000,
                cache_read_tokens: 1_500,
                cache_miss_tokens: 500,
                ..Default::default()
            },
            exact: false,
        });
        t.reasoning_token_delta(&"r".repeat(400));
        t.assistant_delta(&"a".repeat(400));
        let live = strip_ansi(&t.build_frame());
        assert!(live.contains("Almost done thinking 50s"), "状态阶梯: {live:?}");
        assert!(live.contains("↑~2.0k"), "上传估算: {live:?}");
        assert!(live.contains("↓~200"), "写入估算: {live:?}");
        assert!(live.contains("cache ~75%"), "缓存估算: {live:?}");
        assert!(live.contains("¥"), "金额估算: {live:?}");

        t.end_assistant_with_metrics(
            "done",
            crate::llm::Usage {
                prompt_tokens: 2_000,
                completion_tokens: 220,
                cache_read_tokens: 1_500,
                cache_miss_tokens: 500,
                reasoning_tokens: 120,
                ..Default::default()
            },
            crate::llm::Timings::default(),
        );
        let summary = &t.chat.last().unwrap().text;
        assert!(summary.contains("↑ 上传 2000 token"), "最终上传: {summary}");
        assert!(summary.contains("↓ 写入 220 token"), "最终写入: {summary}");
        assert!(summary.contains("服务端缓存 命中率 75.0%"), "最终缓存: {summary}");
        assert!(summary.contains("读 1500"), "最终缓存读取: {summary}");
        assert!(summary.contains("累计 ¥"), "累计金额: {summary}");
        assert!(summary.contains("本轮 ¥"), "本轮金额: {summary}");
    }
    #[test]
    fn session_cost_accumulates_across_turns() {
        let mut t = Tui::new();
        t.w = 240;
        t.h = 28;
        t.set_header("acc-test".into());
        t.set_pricing(crate::config::Pricing::deepseek_flash_cny());
        let u = crate::llm::Usage {
            prompt_tokens: 1_000,
            completion_tokens: 500,
            ..Default::default()
        };
        t.begin_assistant();
        t.end_assistant_with_metrics("第一轮", u, crate::llm::Timings::default());
        let s1 = t.chat.last().unwrap().text.clone();
        assert!(s1.contains("累计 ¥0.002"), "第一轮累计应等于本轮: {s1}");
        t.begin_assistant();
        t.end_assistant_with_metrics("第二轮", u, crate::llm::Timings::default());
        let s2 = t.chat.last().unwrap().text.clone();
        assert!(s2.contains("本轮 ¥0.002"), "第二轮本轮: {s2}");
        assert!(s2.contains("累计 ¥0.004"), "第二轮累计应翻倍(0.001+0.001 输入 + 0.001+0.001 输出): {s2}");
        // clear_chat 重置累计
        t.clear_chat();
        t.begin_assistant();
        t.end_assistant_with_metrics("第三轮", u, crate::llm::Timings::default());
        let s3 = t.chat.last().unwrap().text.clone();
        assert!(s3.contains("累计 ¥0.002"), "clear_chat 后累计应重置: {s3}");
    }
    #[test]
    fn server_prompt_usage_replaces_local_context_estimate_while_streaming() {
        let mut t = Tui::new();
        t.w = 180;
        t.h = 28;
        t.set_header("deepseek-v4-flash │ 会话 test │ native".into());
        t.set_ctx_estimate(9_000, 1_000_000);
        t.begin_assistant();
        t.token_progress(crate::llm::TokenProgress {
            usage: crate::llm::Usage {
                prompt_tokens: 10_321,
                ..Default::default()
            },
            exact: true,
        });
        t.set_ctx_estimate(9_111, 1_000_000);
        let frame = strip_ansi(&t.build_frame());
        assert!(frame.contains("ctx 10k/1.00m (1.03%)"), "精确值应无波浪号: {frame}");
        assert_eq!(t.last_exact_prompt_tokens(), Some(10_321));
    }
    #[test]
    fn tool_progress_heartbeat_keeps_user_scroll_position() {
        let mut t = Tui::new();
        t.w = 60;
        t.h = 24;
        t.push_user("设置一个定时任务".into());
        t.push_tool("timer", r#"{"seconds":90}"#);
        // 用户向上翻看历史
        t.scroll = 5;
        assert!(t.follow, "初始应跟随底部");
        t.follow = false;
        // 定时任务每秒心跳刷新进度：不应把用户拉回底部
        for sec in 1..10u64 {
            let p = crate::tools::ToolProgress {
                output: String::new(),
                elapsed_secs: sec,
                total_lines: 0,
                total_bytes: 0,
            };
            t.push_tool_progress(&p);
        }
        assert_eq!(t.scroll, 5, "心跳刷新不得重置滚动位置");
        // 用户滚回底部：恢复跟随
        t.scroll = 0;
        t.follow = true;
        t.push_tool_progress(&crate::tools::ToolProgress {
            output: String::new(),
            elapsed_secs: 11,
            total_lines: 0,
            total_bytes: 0,
        });
        assert_eq!(t.scroll, 0, "跟随时应保持在底部");
    }

    #[test]
    fn scroll_keys_toggle_follow_flag() {
        let mut t = Tui::new();
        t.w = 60;
        t.h = 24;
        for i in 0..40 {
            t.push_user(format!("历史消息 {i}"));
        }
        let _ = t.handle_key(Key::WheelUp);
        assert!(!t.follow, "向上滚应停止跟随");
        let _ = t.handle_key(Key::PageUp);
        assert!(!t.follow, "PageUp 也应停止跟随");
        t.scroll = 3; // 模拟滚到离底部还有 3 行
        let _ = t.handle_key(Key::WheelDown); // 3-3=0 -> 恢复跟随
        assert!(t.follow, "滚回底部应恢复跟随");
        let _ = t.handle_key(Key::WheelUp);
        assert!(!t.follow);
        t.scroll = 0; // 已在底部
        let _ = t.handle_key(Key::PageDown);
        assert!(t.follow, "PageDown 到底部应保持跟随");
    }

}
