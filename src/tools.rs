use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, Read};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::Duration;
use crate::config::Config;
use crate::llm::Llm;
use crate::registry::{list_categories_text, list_category_text};
use crate::session::SessionStore;
const FILE_READ_DEFAULT_MAX_LINES: usize = 2_000;
const FILE_READ_MAX_LINES_PER_PAGE: usize = 2_000;
const FILE_READ_MAX_SIZE_BYTES: u64 = 256 * 1024;
const FILE_READ_MAX_OUTPUT_TOKENS: usize = 25_000;
const FILE_LISTDIR_DEFAULT_LIMIT: usize = 200;
const FILE_LISTDIR_MAX_LIMIT: usize = 1_000;
const FILE_LISTDIR_MAX_OUTPUT_TOKENS: usize = 25_000;
#[derive(Debug, Clone)]
pub struct QqOut {
    pub chat: String,
    pub text: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskOption {
    pub label: String,
    pub description: String,
    pub preview: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskQuestion {
    pub question: String,
    pub header: String,
    pub options: Vec<AskOption>,
    pub multi_select: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskRequest {
    pub id: u64,
    pub questions: Vec<AskQuestion>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskAnswer {
    pub id: u64,
    pub answers: BTreeMap<String, String>,
}
pub struct AskHandle<'a> {
    pub tx: &'a Sender<AskRequest>,
    pub rx: &'a Receiver<AskAnswer>,
    pub cancel: &'a AtomicBool,
    pub seq: &'a AtomicU64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermRequest {
    pub id: u64,
    pub cmd: String,
    pub cmd_kind: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermDecisionKind {
    Yes,
    AlwaysAllow,
    No,
    AutoEnable,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermDecision {
    pub id: u64,
    pub decision: PermDecisionKind,
}
pub struct PermHandle<'a> {
    pub tx: &'a Sender<PermRequest>,
    pub rx: &'a Receiver<PermDecision>,
    pub cancel: &'a AtomicBool,
    pub seq: &'a AtomicU64,
    pub auto: &'a AtomicBool,
    pub allowed: &'a Mutex<HashSet<String>>,
}
pub struct ToolCtx<'a> {
    pub cfg: &'a Config,
    pub store: &'a mut SessionStore,
    pub llm: &'a Llm,
    pub qq_tx: Option<&'a Sender<QqOut>>,
    pub cancel: &'a AtomicBool,
    pub mem_dir: Option<std::path::PathBuf>,
    pub ask: Option<AskHandle<'a>>,
    pub perm: Option<PermHandle<'a>>,
}
pub const KNOWN_OPS: &[&str] = &[
    "execute_command",
    "list_tools",
    "make_tools",
    "readline",
    "writefile",
    "editline",
    "appendline",
    "glob",
    "grep",
    "listdir",
    "web_search",
    "web_research",
    "web_fetch",
    "http_get",
    "http_headers",
    "portscan",
    "dns_lookup",
    "list_sessions",
    "new_session",
    "switch_session",
    "delete_session",
    "compress",
    "stats",
    "list_skills",
    "install_skill",
    "run_skill",
    "qq_send",
    "is_admin",
    "add_admin",
    "memory_search",
    "memory_write",
    "ask_user",
    "goal",
];
pub fn execute(op: &str, args: &Value, ctx: &mut ToolCtx) -> Result<String, String> {
    if op.eq_ignore_ascii_case("make_tools") || op.eq_ignore_ascii_case("make-tools") {
        let spec = make_tools_payload(args);
        return crate::dynamic_tools::register(ctx.cfg, &spec, KNOWN_OPS);
    }
    if let Some(tool) = crate::dynamic_tools::load(ctx.cfg, op) {
        return execute_dynamic_tool(&tool, args, ctx);
    }
    if op == "execute_command" {
        let recovered_cmd = args
            .get("cmd")
            .and_then(Value::as_str)
            .map(crate::tool_compat::parse_args)
            .filter(|value| dispatch_name(value).is_some());
        let dispatch = if dispatch_name(args).is_some() {
            Some(args)
        } else if let Some(inner) = args.get("args").filter(|inner| dispatch_name(inner).is_some()) {
            Some(inner)
        } else {
            recovered_cmd.as_ref()
        };
        if let Some(dispatch) = dispatch {
            if let Some(name) = dispatch_name(dispatch) {
                let inner = dispatch_payload(dispatch, name);
                if name.eq_ignore_ascii_case("make_tools") || name.eq_ignore_ascii_case("make-tools") {
                    let spec = make_tools_payload(&inner);
                    return crate::dynamic_tools::register(ctx.cfg, &spec, KNOWN_OPS);
                }
                if let Some(tool) = crate::dynamic_tools::load(ctx.cfg, name) {
                    return execute_dynamic_tool(&tool, &inner, ctx);
                }
            }
        }
    }
    let call = crate::tool_compat::normalize_call(op, args);
    let note = if matches!(call.op.as_str(), "readline" | "listdir") {
        None
    } else {
        call.note()
    };
    let result = execute_normalized(&call.op, &call.args, ctx);
    match (note, result) {
        (Some(note), Ok(output)) => Ok(format!("{note}\n{output}")),
        (Some(note), Err(error)) => Err(format!("{note}\n{error}")),
        (None, result) => result,
    }
}
fn dispatch_name(value: &Value) -> Option<&str> {
    let object = value.as_object()?;
    ["op", "tool", "tool_name", "function"]
        .into_iter()
        .find_map(|key| object.get(key).and_then(Value::as_str))
}
fn dispatch_payload(dispatch: &Value, target: &str) -> Value {
    let Some(object) = dispatch.as_object() else {
        return json!({});
    };
    let wrappers: &[&str] = if target.eq_ignore_ascii_case("make_tools")
        || target.eq_ignore_ascii_case("make-tools")
    {
        &["args", "arguments", "params", "input", "payload", "request"]
    } else {
        &["args", "arguments", "parameters", "params", "input", "payload", "request"]
    };
    if let Some(value) = wrappers.iter().find_map(|key| object.get(*key)) {
        return match value {
            Value::String(raw) => crate::tool_compat::parse_args(raw),
            other => other.clone(),
        };
    }
    let mut flat = object.clone();
    for key in ["op", "tool", "tool_name", "function"] {
        flat.remove(key);
    }
    Value::Object(flat)
}
fn make_tools_payload(raw: &Value) -> Value {
    let parsed = match raw {
        Value::String(text) => crate::tool_compat::parse_args(text),
        other => other.clone(),
    };
    let Some(object) = parsed.as_object() else {
        return parsed;
    };
    for key in ["args", "arguments", "params", "input", "payload", "request"] {
        if object.len() == 1 {
            if let Some(inner) = object.get(key) {
                return match inner {
                    Value::String(text) => crate::tool_compat::parse_args(text),
                    other => other.clone(),
                };
            }
        }
    }
    parsed
}
fn execute_normalized(op: &str, args: &Value, ctx: &mut ToolCtx) -> Result<String, String> {
    match op {
        "execute_command" => execute_command(args, ctx),
        "list_tools" => {
            let cat = args.get("category").and_then(|c| c.as_str());
            Ok(match cat {
                Some(crate::dynamic_tools::CUSTOM_CATEGORY) => crate::dynamic_tools::list_detail(ctx.cfg),
                Some(c) => list_category_text(c)
                    .ok_or_else(|| format!("未知分类: {c}（无参数调用 list_tools 查看分类目录）"))?,
                None => {
                    let mut index = list_categories_text();
                    index.push_str(&crate::dynamic_tools::list_index_line(ctx.cfg));
                    index
                }
            })
        }
        "make_tools" => crate::dynamic_tools::register(ctx.cfg, args, KNOWN_OPS),
        "readline" => file_read(args),
        "writefile" => file_write(args),
        "editline" => file_edit(args),
        "appendline" => file_append(args),
        "glob" => file_glob(args),
        "grep" => file_grep(args),
        "listdir" => file_listdir(args),
        "web_search" => crate::web::web_search(args, ctx.cfg),
        "web_research" => crate::web::web_research(args, ctx.cfg),
        "web_fetch" => crate::web::web_fetch(args),
        "http_get" => net_get(args, false),
        "http_headers" => net_get(args, true),
        "portscan" => sec_portscan(args),
        "dns_lookup" => sec_dns(args),
        "list_sessions" => session_list(ctx),
        "new_session" => session_new(args, ctx),
        "switch_session" => session_switch(args, ctx),
        "delete_session" => session_delete(args, ctx),
        "compress" => ctx_compress(ctx),
        "stats" => ctx_stats(ctx),
        "list_skills" => crate::skills::op_list_skills(ctx.cfg),
        "install_skill" => crate::skills::op_install_skill(args, ctx.cfg),
        "run_skill" => crate::skills::op_run_skill(args, ctx.cfg),
        "qq_send" => qq_send(args, ctx),
        "is_admin" => qq_is_admin(args, ctx),
        "add_admin" => qq_add_admin(args, ctx),
        "memory_search" => memory_search(args, ctx),
        "memory_write" => memory_write(args, ctx),
        "ask_user" => ask_user(args, ctx),
        "goal" => crate::goal::execute_tool(ctx.cfg, ctx.store.current_id(), args),
        _ => match crate::dynamic_tools::load(ctx.cfg, op) {
            Some(tool) => execute_dynamic_tool(&tool, args, ctx),
            None => Err(format!("未知工具: {op}（list_tools 查看可用工具）")),
        },
    }
}
fn execute_command(args: &Value, ctx: &mut ToolCtx) -> Result<String, String> {
    let args = if args.get("cmd").is_none() {
        match args.get("args") {
            Some(a) if !a.is_null() && (a.get("cmd").is_some() || a.get("op").is_some()) => a,
            _ => args,
        }
    } else {
        args
    };
    if let Some(op) = args.get("op").and_then(|o| o.as_str()) {
        if !op.is_empty() && op != "execute_command" {
            let inner = match args.get("args") {
                Some(a) if !a.is_null() => a.clone(),
                _ => {
                    let mut m = args.clone();
                    if let Some(obj) = m.as_object_mut() {
                        obj.remove("op");
                    }
                    m
                }
            };
            return execute(op, &inner, ctx);
        }
    }
    let cmd = args
        .get("cmd")
        .and_then(|c| c.as_str())
        .ok_or("缺少 cmd 参数")?
        .trim()
        .to_string();
    if cmd.is_empty() {
        return Err("cmd 为空".into());
    }
    if shell_invokes_cat(&cmd) {
        return Err(
            "禁止通过 shell/cat 读取文件。请调用 readline {\"path\":\"文件路径\",\"offset\":1,\"limit\":2000}；系统会返回连续完整的一页并给出下一页 offset。"
                .into(),
        );
    }
    if let Some((op, inner)) = keyword_tool(&cmd) {
        return execute(op, &inner, ctx);
    }
    let cwd = args.get("cwd").and_then(|c| c.as_str()).unwrap_or(".");
    let timeout = args
        .get("timeout")
        .and_then(|t| t.as_u64())
        .unwrap_or(ctx.cfg.command_timeout_secs.max(1))
        .min(3600);
    let perm = ctx.perm.as_ref();
    if let Some(handle) = perm {
        if !handle.auto.load(Ordering::Relaxed) {
            let cmd_kind = cmd.split_whitespace().next().unwrap_or(cmd.as_str()).to_string();
            let allowed = handle
                .allowed
                .lock()
                .map(|g| g.contains(&cmd_kind))
                .unwrap_or(false);
            if !allowed {
                let id = handle.seq.fetch_add(1, Ordering::Relaxed);
                handle
                    .tx
                    .send(PermRequest { id, cmd: cmd.clone(), cmd_kind: cmd_kind.clone() })
                    .map_err(|_| "权限通道已关闭".to_string())?;
                let decision = loop {
                    if handle.cancel.load(Ordering::Relaxed) {
                        return Err("命令被取消".into());
                    }
                    match handle.rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(d) if d.id == id => break d.decision,
                        Ok(_) => continue,              
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => return Err("权限通道已断开".into()),
                    }
                };
                match decision {
                    PermDecisionKind::Yes => {}
                    PermDecisionKind::AlwaysAllow => {
                        if let Ok(mut g) = handle.allowed.lock() {
                            g.insert(cmd_kind);
                        }
                    }
                    PermDecisionKind::AutoEnable => {
                        handle.auto.store(true, Ordering::Relaxed);
                        if let Ok(mut g) = handle.allowed.lock() {
                            g.insert(cmd_kind);
                        }
                    }
                    PermDecisionKind::No => {
                        return Err(format!(
                            "用户拒绝了命令执行: {cmd}。请不要擅自执行 shell 命令，改用已有工具完成，或向用户说明你需要执行的命令。"
                        ));
                    }
                }
            }
        }
    }
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动命令失败: {e}"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut so) = child.stdout.take() {
                    let _ = so.read_to_string(&mut out);
                }
                if let Some(mut se) = child.stderr.take() {
                    let mut e = String::new();
                    let _ = se.read_to_string(&mut e);
                    if !e.is_empty() {
                        out.push_str(&format!("\n[stderr]\n{e}"));
                    }
                }
                let code = status.code().unwrap_or(-1);
                return Ok(format!("exit code: {code}\n{out}"));
            }
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let mut out = String::new();
                    if let Some(mut so) = child.stdout.take() {
                        let _ = so.read_to_string(&mut out);
                    }
                    if let Some(mut se) = child.stderr.take() {
                        let mut e = String::new();
                        let _ = se.read_to_string(&mut e);
                        if !e.is_empty() {
                            out.push_str(&format!("\n[stderr]\n{e}"));
                        }
                    }
                    let hint = format!(
                        "命令超过 {timeout}s 未结束，已被终止。若需要更长的执行时间，\
                         请在 execute_command 参数中传入 \"timeout\": 秒数，例如 \
                         {{\"cmd\":\"{cmd}\",\"timeout\":900}}"
                    );
                    if out.trim().is_empty() {
                        return Ok(format!(
                            "命令超时（>{timeout}s）已终止，且没有产生任何输出（进程一直在安静等待）。{hint}"
                        ));
                    }
                    return Ok(format!(
                        "命令超时（>{timeout}s）已终止。以下是终止前已产生的输出：\n{out}\n{hint}"
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("等待命令失败: {e}")),
        }
    }
}
fn execute_dynamic_tool(
    tool: &crate::dynamic_tools::DynamicTool,
    args: &Value,
    ctx: &mut ToolCtx,
) -> Result<String, String> {
    let cmd = crate::dynamic_tools::invocation_command(ctx.cfg, tool, args)?;
    execute_command(&json!({"cmd":cmd,"timeout":tool.timeout_secs}), ctx)
}
fn shell_invokes_cat(command: &str) -> bool {
    shell_command_segments(command)
        .iter()
        .any(|segment| segment_invokes_cat(segment))
}
fn segment_invokes_cat(words: &[String]) -> bool {
    let mut first = 0usize;
    while first < words.len()
        && words[first].contains('=')
        && !words[first].starts_with('=')
    {
        first += 1;
    }
    let Some(name) = words.get(first).map(|word| shell_executable_name(word)) else {
        return false;
    };
    if name == "cat" {
        return true;
    }
    if matches!(
        name,
        "command" | "exec" | "env" | "nohup" | "nice" | "sudo" | "xargs" | "busybox"
    ) && words[first + 1..]
        .iter()
        .any(|word| shell_executable_name(word) == "cat")
    {
        return true;
    }
    if matches!(name, "sh" | "bash" | "zsh") {
        if let Some(flag) = words[first + 1..].iter().position(|word| word == "-c") {
            if let Some(script) = words.get(first + 1 + flag + 1) {
                return shell_invokes_cat(script);
            }
        }
    }
    false
}
fn shell_executable_name(word: &str) -> &str {
    Path::new(word)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(word)
}
fn shell_command_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    let mut segment = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let flush_word = |segment: &mut Vec<String>, word: &mut String| {
        if !word.is_empty() {
            segment.push(std::mem::take(word));
        }
    };
    let flush_segment = |segments: &mut Vec<Vec<String>>, segment: &mut Vec<String>| {
        if !segment.is_empty() {
            segments.push(std::mem::take(segment));
        }
    };
    for ch in command.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    word.push(ch);
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => word.push(ch),
            },
            _ => match ch {
                '\'' | '"' => quote = Some(ch),
                '\\' => escaped = true,
                ' ' | '\t' | '\r' => flush_word(&mut segment, &mut word),
                ';' | '|' | '&' | '\n' | '(' | ')' => {
                    flush_word(&mut segment, &mut word);
                    flush_segment(&mut segments, &mut segment);
                }
                _ => word.push(ch),
            },
        }
    }
    flush_word(&mut segment, &mut word);
    flush_segment(&mut segments, &mut segment);
    segments
}
pub fn keyword_tool(text: &str) -> Option<(&'static str, Value)> {
    let t = text.trim();
    for p in [
        "帮我搜索一下",
        "帮我查一下",
        "帮我搜一下",
        "搜索一下",
        "搜一下",
        "查一下",
        "帮我搜索",
        "帮我搜",
        "帮我查",
        "百度一下",
        "查找",
        "搜索",
        "搜",
    ] {
        if let Some(rest) = t.strip_prefix(p) {
            let q = rest.trim();
            if q.is_empty() || is_meta_question(q) || (p == "搜" && q.chars().count() < 2) {
                continue;
            }
            return Some(("web_search", json!({"query": q})));
        }
    }
    if let Some(rest) = t.strip_prefix("search") {
        let q = rest.trim_start();
        if !q.is_empty() && !is_meta_question(q) {
            return Some(("web_search", json!({"query": q})));
        }
    }
    for p in ["抓取", "打开网页", "看网页", "读网页", "打开链接", "fetch "] {
        if let Some(rest) = t.strip_prefix(p) {
            if let Some(url) = extract_url(rest) {
                return Some(("web_fetch", json!({"url": url})));
            }
        }
    }
    for p in ["读取", "查看文件", "打开文件", "读文件"] {
        if let Some(rest) = t.strip_prefix(p) {
            if let Some(path) = extract_leading_file_path(rest) {
                return Some(("readline", json!({"path": path})));
            }
        }
    }
    None
}
fn extract_leading_file_path(input: &str) -> Option<String> {
    let input = input.trim_start();
    if input.is_empty() {
        return None;
    }
    if let Some(quote @ ('\'' | '"')) = input.chars().next() {
        let rest = &input[quote.len_utf8()..];
        let end = rest.find(quote)?;
        let path = rest[..end].trim();
        return (!path.is_empty()).then(|| path.to_string());
    }
    let end = input
        .find(|ch: char| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    ',' | ';' | ':' | '，' | '。' | '；' | '：' | '、' | '！' | '？'
                )
        })
        .unwrap_or(input.len());
    let path = input[..end].trim();
    (!path.is_empty()).then(|| path.to_string())
}
fn is_meta_question(s: &str) -> bool {
    ["怎么", "什么", "如何", "为什么", "吗", "呢", "咋", "哪个", "哪些", "有没有"]
        .iter()
        .any(|w| s.contains(w))
}
fn extract_url(s: &str) -> Option<String> {
    let pos = s.to_lowercase().find("http")?;
    let rest = &s[pos..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '>' || c == '`')
        .unwrap_or(rest.len());
    let url = rest[..end].to_string();
    if url.starts_with("http://") || url.starts_with("https://") {
        Some(url)
    } else {
        None
    }
}
fn arg_str<'v>(args: &'v Value, key: &str) -> Result<&'v str, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("缺少参数: {key}"))
}
fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}
fn file_read(args: &Value) -> Result<String, String> {
    let path = expand_home(arg_str(args, "path")?);
    if is_blocked_read_path(&path) {
        return Err(format!("Cannot read '{path}': this device file would block or produce infinite output."));
    }
    let metadata = std::fs::metadata(&path).map_err(|e| format!("读取失败: {e}"))?;
    if metadata.is_dir() {
        return Err(format!(
            "Cannot read '{path}': the specified path is a directory. Use listdir or execute_command with ls."
        ));
    }
    let start = args.get("start").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
    let explicit_limit = args.get("limit").and_then(Value::as_u64).map(|value| value as usize);
    let explicit_end = args.get("end").and_then(Value::as_u64).map(|value| value as usize);
    let limit = match (explicit_limit, explicit_end) {
        (Some(0), _) => return Err("limit 必须大于 0".into()),
        (Some(limit), _) => limit,
        (None, Some(end)) if end >= start => end - start + 1,
        (None, Some(end)) => return Err(format!("行范围无效: {start}-{end}")),
        (None, None) => FILE_READ_DEFAULT_MAX_LINES,
    };
    if limit > FILE_READ_MAX_LINES_PER_PAGE {
        return Err(format!(
            "limit cannot exceed {FILE_READ_MAX_LINES_PER_PAGE} lines. Read consecutive pages with offset/limit instead."
        ));
    }
    if explicit_limit.is_none() && explicit_end.is_none() && metadata.len() > FILE_READ_MAX_SIZE_BYTES {
        return Err(format!(
            "File content ({}) exceeds maximum allowed size ({}). Use offset and limit parameters to read specific portions of the file, or search for specific content instead of reading the whole file.",
            format_file_size(metadata.len()),
            format_file_size(FILE_READ_MAX_SIZE_BYTES)
        ));
    }
    let file = std::fs::File::open(&path).map_err(|e| format!("读取失败: {e}"))?;
    let mut reader = std::io::BufReader::new(file);
    let mut raw_line = Vec::new();
    let mut total_lines = 0usize;
    let end_exclusive = start.saturating_sub(1).saturating_add(limit);
    let mut selected = String::new();
    let mut selected_lines = 0usize;
    loop {
        raw_line.clear();
        let bytes = reader.read_until(b'\n', &mut raw_line).map_err(|e| format!("读取失败: {e}"))?;
        if bytes == 0 {
            break;
        }
        total_lines = total_lines.saturating_add(1);
        let zero_index = total_lines - 1;
        if zero_index < start - 1 || zero_index >= end_exclusive {
            continue;
        }
        if raw_line.last() == Some(&b'\n') {
            raw_line.pop();
        }
        if raw_line.last() == Some(&b'\r') {
            raw_line.pop();
        }
        if raw_line.contains(&0) {
            return Err(format!(
                "Cannot read '{path}' as text: the file contains NUL bytes and appears to be binary."
            ));
        }
        let line = String::from_utf8_lossy(&raw_line);
        if !selected.is_empty() {
            selected.push('\n');
        }
        selected.push_str(&total_lines.to_string());
        selected.push('\t');
        selected.push_str(&line);
        selected_lines = selected_lines.saturating_add(1);
        let approx_tokens = crate::compress::approx_token_count(&selected);
        if approx_tokens > FILE_READ_MAX_OUTPUT_TOKENS {
            return Err(format!(
                "File content ({approx_tokens} tokens) exceeds maximum allowed tokens ({FILE_READ_MAX_OUTPUT_TOKENS}). Use offset and limit parameters to read specific portions of the file, or search for specific content instead of reading the whole file."
            ));
        }
    }
    if total_lines == 0 {
        return Ok("<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>".into());
    }
    if selected_lines == 0 {
        return Ok(format!(
            "<system-reminder>Warning: the file exists but is shorter than the provided offset ({start}). The file has {total_lines} lines.</system-reminder>"
        ));
    }
    let last_line = start.saturating_add(selected_lines).saturating_sub(1);
    if last_line < total_lines {
        let next = serde_json::json!({
            "path": path,
            "offset": last_line.saturating_add(1),
            "limit": limit,
        });
        selected.push_str(&format!(
            "\n<system-reminder>Read lines {start}-{last_line} of {total_lines}. The page above is complete; the file is not. Continue with readline {next}.</system-reminder>"
        ));
    }
    Ok(selected)
}
fn format_file_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} bytes")
    }
}
fn is_blocked_read_path(path: &str) -> bool {
    const BLOCKED: &[&str] = &[
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
        "/dev/full",
        "/dev/stdin",
        "/dev/tty",
        "/dev/console",
        "/dev/stdout",
        "/dev/stderr",
        "/dev/fd/0",
        "/dev/fd/1",
        "/dev/fd/2",
    ];
    if BLOCKED.contains(&path) {
        return true;
    }
    path.starts_with("/proc/")
        && (path.ends_with("/fd/0") || path.ends_with("/fd/1") || path.ends_with("/fd/2"))
}
fn file_write(args: &Value) -> Result<String, String> {
    let path = expand_home(arg_str(args, "path")?);
    let content = args.get("content").and_then(|c| c.as_str()).unwrap_or("");
    if let Some(parent) = Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, content).map_err(|e| format!("写入失败: {e}"))?;
    Ok(format!("已写入 {path}（{} 字符）", content.chars().count()))
}
fn file_edit(args: &Value) -> Result<String, String> {
    let path = expand_home(arg_str(args, "path")?);
    let old = arg_str(args, "old")?;
    let new = arg_str(args, "new")?;
    if old.is_empty() {
        return Err("old 不能为空（空匹配会导致不确定编辑）".into());
    }
    let content = std::fs::read_to_string(&path).map_err(|e| format!("读取失败: {e}"))?;
    let (bom, body) = match content.strip_prefix('\u{feff}') {
        Some(body) => ("\u{feff}", body),
        None => ("", content.as_str()),
    };
    let line_ending = if body.contains("\r\n") { "\r\n" } else { "\n" };
    let normalized = normalize_lf(body);
    let normalized_old = normalize_lf(old);
    let normalized_new = normalize_lf(new);
    let exact_count = normalized.matches(&normalized_old).count();
    let (edited, mode) = if exact_count == 1 {
        (normalized.replacen(&normalized_old, &normalized_new, 1), "精确匹配")
    } else if exact_count > 1 {
        return Err(format!(
            "找到 {exact_count} 处匹配，拒绝猜测要改哪一处；请在 old 中补充上下文使其唯一"
        ));
    } else {
        let fuzzy_content = normalize_for_fuzzy_match(&normalized);
        let fuzzy_old = normalize_for_fuzzy_match(&normalized_old);
        let fuzzy_count = fuzzy_content.matches(&fuzzy_old).count();
        if fuzzy_count == 0 {
            return Err("未找到要替换的文本（已自动尝试换行、尾随空格、智能引号、破折号和特殊空格兼容）".into());
        }
        if fuzzy_count > 1 {
            return Err(format!(
                "模糊匹配找到 {fuzzy_count} 处，拒绝猜测要改哪一处；请在 old 中补充上下文使其唯一"
            ));
        }
        let index = fuzzy_content.find(&fuzzy_old).expect("count 已确认唯一");
        (
            replace_fuzzy_preserving_unchanged_lines(
                &normalized,
                &fuzzy_content,
                index,
                fuzzy_old.len(),
                &normalized_new,
            )?,
            "容错匹配",
        )
    };
    if edited == normalized {
        return Err("替换前后内容相同，文件未改动".into());
    }
    let restored = if line_ending == "\r\n" { edited.replace('\n', "\r\n") } else { edited };
    let output = format!("{bom}{restored}");
    std::fs::write(&path, output).map_err(|e| format!("写入失败: {e}"))?;
    Ok(format!("已替换 {path} 中的唯一匹配（{mode}）"))
}
fn normalize_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}
fn normalize_for_fuzzy_match(text: &str) -> String {
    text.lines()
        .map(|line| {
            line.trim_end()
                .chars()
                .map(|c| match c {
                    '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
                    '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
                    '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' | '\u{2212}' => '-',
                    '\u{00a0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}' | '\u{2007}'
                    | '\u{2008}' | '\u{2009}' | '\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
                    other => other,
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + if text.ends_with('\n') { "\n" } else { "" }
}
fn line_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for (index, ch) in text.char_indices() {
        if ch == '\n' {
            spans.push((start, index + 1));
            start = index + 1;
        }
    }
    if start < text.len() {
        spans.push((start, text.len()));
    }
    spans
}
fn replace_fuzzy_preserving_unchanged_lines(
    original: &str,
    fuzzy: &str,
    match_index: usize,
    match_len: usize,
    replacement: &str,
) -> Result<String, String> {
    let fuzzy_spans = line_spans(fuzzy);
    let original_lines: Vec<&str> = original.split_inclusive('\n').collect();
    if fuzzy_spans.len() != original_lines.len() {
        return Err("容错编辑内部行映射失败，文件未改动".into());
    }
    let match_end = match_index + match_len;
    let start_line = fuzzy_spans
        .iter()
        .position(|(start, end)| match_index >= *start && match_index < *end)
        .ok_or("容错编辑命中范围越界，文件未改动")?;
    let mut end_line = start_line;
    while end_line + 1 < fuzzy_spans.len() && fuzzy_spans[end_line].1 < match_end {
        end_line += 1;
    }
    let group_start = fuzzy_spans[start_line].0;
    let group_end = fuzzy_spans[end_line].1;
    if match_end > group_end {
        return Err("容错编辑命中范围越界，文件未改动".into());
    }
    let mut out = original_lines[..start_line].concat();
    out.push_str(&fuzzy[group_start..match_index]);
    out.push_str(replacement);
    out.push_str(&fuzzy[match_end..group_end]);
    out.push_str(&original_lines[end_line + 1..].concat());
    Ok(out)
}
fn file_append(args: &Value) -> Result<String, String> {
    let path = expand_home(arg_str(args, "path")?);
    let content = args.get("content").and_then(|c| c.as_str()).unwrap_or("");
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("打开失败: {e}"))?;
    f.write_all(content.as_bytes()).map_err(|e| format!("追加失败: {e}"))?;
    f.write_all(b"\n").ok();
    Ok(format!("已追加到 {path}"))
}
fn file_listdir(args: &Value) -> Result<String, String> {
    let path = expand_home(args.get("path").and_then(|p| p.as_str()).unwrap_or("."));
    let offset_u64 = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
    let limit_u64 = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(FILE_LISTDIR_DEFAULT_LIMIT as u64);
    if limit_u64 == 0 {
        return Err("limit 必须大于 0".into());
    }
    if limit_u64 > FILE_LISTDIR_MAX_LIMIT as u64 {
        return Err(format!(
            "limit 不能超过 {FILE_LISTDIR_MAX_LIMIT}；请用 offset/limit 连续分页"
        ));
    }
    let offset = usize::try_from(offset_u64).map_err(|_| "offset 过大")?;
    let limit = usize::try_from(limit_u64).map_err(|_| "limit 过大")?;
    let include_hidden = args.get("all").and_then(Value::as_bool).unwrap_or(true);
    let entries = std::fs::read_dir(&path).map_err(|e| format!("读取目录失败: {e}"))?;
    let mut items: Vec<(u8, String)> = Vec::new();
    let mut skipped = 0usize;
    for entry in entries {
        let Ok(entry) = entry else {
            skipped = skipped.saturating_add(1);
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let kind = match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => 0,
            Ok(file_type) if file_type.is_symlink() => 1,
            Ok(_) => 2,
            Err(_) => {
                skipped = skipped.saturating_add(1);
                continue;
            }
        };
        items.push((kind, name));
    }
    items.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
            .then_with(|| a.1.cmp(&b.1))
    });
    let total = items.len();
    if offset >= total {
        return Ok(listdir_page_output(&path, total, offset, limit, skipped, &[]));
    }
    let requested_end = offset.saturating_add(limit).min(total);
    let mut page: Vec<String> = items[offset..requested_end]
        .iter()
        .map(|(kind, name)| {
            let label = match kind {
                0 => "dir",
                1 => "link",
                _ => "file",
            };
            let suffix = if *kind == 0 { "/" } else { "" };
            format!("{label}\t{}{suffix}", escape_listing_name(name))
        })
        .collect();
    loop {
        let out = listdir_page_output(&path, total, offset, limit, skipped, &page);
        let approx_tokens = crate::compress::approx_token_count(&out);
        if approx_tokens <= FILE_LISTDIR_MAX_OUTPUT_TOKENS {
            return Ok(out);
        }
        if page.len() <= 1 {
            return Err(format!(
                "Directory listing page ({approx_tokens} tokens) exceeds maximum allowed tokens ({FILE_LISTDIR_MAX_OUTPUT_TOKENS}). Retry listdir with the same path and offset but a smaller limit."
            ));
        }
        page.pop();
    }
}
fn escape_listing_name(name: &str) -> String {
    name.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}
fn listdir_page_output(
    path: &str,
    total: usize,
    offset: usize,
    limit: usize,
    skipped: usize,
    page: &[String],
) -> String {
    let mut out = format!("Directory: {path}\n");
    if total == 0 {
        out.push_str("Entries: 0 total; showing 0.\n");
    } else if page.is_empty() {
        out.push_str(&format!("Entries: {total} total; offset {offset} is past the end.\n"));
    } else {
        out.push_str(&format!(
            "Entries: {total} total; showing {}-{}.\n",
            offset + 1,
            offset + page.len()
        ));
        for item in page {
            out.push_str(item);
            out.push('\n');
        }
    }
    if skipped > 0 {
        out.push_str(&format!("Skipped: {skipped} unreadable entries.\n"));
    }
    let next_offset = offset.saturating_add(page.len());
    if next_offset < total {
        let path_json = serde_json::to_string(path).unwrap_or_else(|_| "\".\"".into());
        out.push_str(&format!(
            "Next page: listdir {{\"path\":{path_json},\"offset\":{next_offset},\"limit\":{limit}}}\n"
        ));
    }
    out
}
fn file_glob(args: &Value) -> Result<String, String> {
    let pattern = arg_str(args, "pattern")?;
    let base = args.get("base").and_then(|b| b.as_str()).unwrap_or(".");
    let max_results = args.get("max").and_then(|m| m.as_u64()).unwrap_or(200) as usize;
    let mut results: Vec<String> = Vec::new();
    walk(Path::new(base), 0, 12, &mut |rel: String, is_dir: bool| {
        if !is_dir && glob_match(pattern, &rel) && results.len() < max_results {
            results.push(rel);
        }
    });
    if results.is_empty() {
        return Ok(format!("未匹配到文件: {pattern}"));
    }
    let mut out = format!("匹配 {pattern}（{} 个）:\n", results.len());
    for r in results {
        out.push_str(&format!("{r}\n"));
    }
    Ok(out)
}
fn walk(dir: &Path, depth: usize, max_depth: usize, f: &mut impl FnMut(String, bool)) {
    if depth > max_depth {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = if depth == 0 { name.clone() } else { format!("{}/{name}", dir.to_string_lossy()) };
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(&e.path(), depth + 1, max_depth, f);
            f(rel, true);
        } else {
            f(rel, false);
        }
    }
}
pub fn glob_match(pattern: &str, s: &str) -> bool {
    fn match_seg(p: &[char], s: &[char]) -> bool {
        if p.is_empty() {
            return s.is_empty();
        }
        if p[0] == '*' {
            let double = p.len() >= 2 && p[1] == '*';
            if double {
                for i in 0..=s.len() {
                    if match_seg(&p[2..], &s[i..]) {
                        return true;
                    }
                }
                false
            } else {
                for i in 0..=s.len() {
                    if i > 0 && s[i - 1] == '/' {
                        break;
                    }
                    if match_seg(&p[1..], &s[i..]) {
                        return true;
                    }
                }
                false
            }
        } else if p[0] == '?' {
            !s.is_empty() && match_seg(&p[1..], &s[1..])
        } else {
            !s.is_empty() && s[0] == p[0] && match_seg(&p[1..], &s[1..])
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = s.chars().collect();
    if p.starts_with(&['*', '*', '/']) {
        let rest = &p[3..];
        for i in 0..=s.len() {
            if (i == 0 || s[i - 1] == '/') && match_seg(rest, &s[i..]) {
                return true;
            }
        }
        return false;
    }
    match_seg(&p, &s)
}
fn file_grep(args: &Value) -> Result<String, String> {
    let pattern = arg_str(args, "pattern")?;
    let base = expand_home(args.get("path").and_then(|p| p.as_str()).unwrap_or("."));
    let glob = args.get("glob").and_then(|g| g.as_str());
    let max_results = args.get("max").and_then(|m| m.as_u64()).unwrap_or(50) as usize;
    let mut hits: Vec<String> = Vec::new();
    let mut total_files = 0usize;
    let bp = Path::new(&base);
    if bp.is_file() {
        total_files += 1;
        if let Ok(content) = std::fs::read_to_string(bp) {
            for (i, line) in content.lines().enumerate() {
                if line.contains(pattern) {
                    hits.push(format!("{base}:{}: {}", i + 1, line.trim().chars().take(200).collect::<String>()));
                    if hits.len() >= max_results {
                        break;
                    }
                }
            }
        }
    } else {
        walk(bp, 0, 10, &mut |rel, is_dir| {
            if is_dir || hits.len() >= max_results {
                return;
            }
            if let Some(g) = glob {
                if !glob_match(g, &rel) {
                    return;
                }
            }
            total_files += 1;
            if let Ok(content) = std::fs::read_to_string(&rel) {
                for (i, line) in content.lines().enumerate() {
                    if line.contains(pattern) {
                        hits.push(format!("{rel}:{}: {}", i + 1, line.trim().chars().take(200).collect::<String>()));
                        if hits.len() >= max_results {
                            return;
                        }
                    }
                }
            }
        });
    }
    if hits.is_empty() {
        return Ok(format!("在 {base} 中未找到 {pattern:?}（扫描 {total_files} 个文件）"));
    }
    Ok(format!("命中 {pattern:?}（{} 处）:\n{}", hits.len(), hits.join("\n")))
}
fn net_get(args: &Value, head_only: bool) -> Result<String, String> {
    let url = arg_str(args, "url")?;
    let timeout = args.get("timeout").and_then(|t| t.as_u64()).unwrap_or(15);
    let mut req = ureq::get(url).timeout(Duration::from_secs(timeout.min(60)));
    if head_only {
        req = ureq::head(url).timeout(Duration::from_secs(timeout.min(60)));
    }
    if let Some(h) = args.get("headers").and_then(|v| v.as_object()) {
        for (k, v) in h {
            if let Some(vs) = v.as_str() {
                req = req.set(k, vs);
            }
        }
    }
    match req.call() {
        Ok(resp) => {
            let status = resp.status();
            let mut out = format!("HTTP {status}\n");
            for k in resp.headers_names() {
                if let Some(v) = resp.header(&k) {
                    out.push_str(&format!("{k}: {v}\n"));
                }
            }
            if !head_only {
                let mut body = String::new();
                let _ = resp.into_reader().take(16 * 1024).read_to_string(&mut body);
                out.push_str(&format!("\n{body}"));
            }
            Ok(out)
        }
        Err(ureq::Error::Status(code, resp)) => {
            let mut body = String::new();
            let _ = resp.into_reader().take(4 * 1024).read_to_string(&mut body);
            Ok(format!("HTTP {code}\n{body}"))
        }
        Err(e) => Err(format!("请求失败: {e}")),
    }
}
fn sec_portscan(args: &Value) -> Result<String, String> {
    let host = arg_str(args, "host")?;
    let ports_spec = arg_str(args, "ports")?;
    let timeout_ms = args.get("timeout_ms").and_then(|t| t.as_u64()).unwrap_or(500);
    let mut ports: Vec<u16> = Vec::new();
    for part in ports_spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            let lo: u16 = a.trim().parse().map_err(|_| format!("端口格式错误: {part}"))?;
            let hi: u16 = b.trim().parse().map_err(|_| format!("端口格式错误: {part}"))?;
            if hi < lo || hi - lo > 10000 {
                return Err(format!("端口范围过大: {part}"));
            }
            ports.extend(lo..=hi);
        } else {
            ports.push(part.parse().map_err(|_| format!("端口格式错误: {part}"))?);
        }
    }
    if ports.is_empty() {
        return Err("缺少端口".into());
    }
    let mut seen = std::collections::HashSet::new();
    ports.retain(|p| seen.insert(*p));
    let ip: std::net::IpAddr = host
        .parse()
        .or_else(|_| {
            (host, 0u16)
                .to_socket_addrs()
                .map_err(|e| format!("域名解析失败: {e}"))?
                .next()
                .map(|sa| sa.ip())
                .ok_or_else(|| format!("无法解析 {host}"))
        })
        .map_err(|e: String| e)?;
    let open: Vec<u16> = {
        let mut out = Vec::new();
        let batch = 64usize;
        for chunk in ports.chunks(batch) {
            for &p in chunk {
                let addr = SocketAddr::new(ip, p);
                if TcpStream::connect_timeout(&addr, Duration::from_millis(timeout_ms)).is_ok() {
                    out.push(p);
                }
            }
        }
        out
    };
    if open.is_empty() {
        Ok(format!("{host}（{ip}）扫描 {} 个端口：全部关闭", ports.len()))
    } else {
        let list: Vec<String> = open.iter().map(|p| p.to_string()).collect();
        Ok(format!("{host}（{ip}）开放端口（{}）: {}", open.len(), list.join(", ")))
    }
}
fn sec_dns(args: &Value) -> Result<String, String> {
    let name = arg_str(args, "name")?;
    let mut ips: Vec<String> = Vec::new();
    for sa in (name, 0u16).to_socket_addrs().map_err(|e| format!("解析失败: {e}"))? {
        let ip = sa.ip().to_string();
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    if ips.is_empty() {
        Ok(format!("{name}: 无 A/AAAA 记录"))
    } else {
        Ok(format!("{name} -> {}", ips.join(", ")))
    }
}
fn session_list(ctx: &mut ToolCtx) -> Result<String, String> {
    let mut out = String::from("会话列表:\n");
    for id in ctx.store.list() {
        let (msgs, tokens) = crate::session::session_stats(&ctx.store.path(&id));
        let mark = if id == ctx.store.current_id() { " *" } else { "" };
        out.push_str(&format!("- {id}（{msgs} 条消息, ~{tokens} tok）{mark}\n"));
    }
    Ok(out)
}
fn session_new(args: &Value, ctx: &mut ToolCtx) -> Result<String, String> {
    let id = arg_str(args, "id")?;
    let id = ctx.store.new_session(id);
    Ok(format!("已新建并切换到会话: {id}"))
}
fn session_switch(args: &Value, ctx: &mut ToolCtx) -> Result<String, String> {
    let id = arg_str(args, "id")?;
    ctx.store.switch(id)?;
    Ok(format!("已切换到会话: {id}"))
}
fn session_delete(args: &Value, ctx: &mut ToolCtx) -> Result<String, String> {
    let id = arg_str(args, "id")?;
    ctx.store.delete(id)?;
    Ok(format!("已删除会话: {id}"))
}
fn ctx_compress(ctx: &mut ToolCtx) -> Result<String, String> {
    let msgs = ctx.store.current().messages;
    let before = crate::compress::approx_total_tokens(&msgs);
    let new_hist = crate::compress::compact(ctx.llm, &msgs, ctx.cancel)?;
    ctx.store.replace_current(&new_hist)?;
    let after = crate::compress::approx_total_tokens(&new_hist);
    Ok(format!("压缩完成：{before} tok -> {after} tok（摘要已注入，最近消息保留）"))
}
fn ctx_stats(ctx: &mut ToolCtx) -> Result<String, String> {
    let msgs = ctx.store.current().messages;
    let tokens = crate::compress::approx_total_tokens(&msgs);
    let window = crate::backend::effective_window(
        ctx.cfg.provider.ctx_override,
        ctx.llm.n_ctx(),
        ctx.cfg.provider.ctx_window,
    );
    Ok(format!(
        "上下文: ~{tokens} / {window} tok（{:.1}%），消息 {} 条",
        tokens as f64 * 100.0 / window as f64,
        msgs.len()
    ))
}
fn qq_send(args: &Value, ctx: &mut ToolCtx) -> Result<String, String> {
    let chat = arg_str(args, "chat")?;
    let text = args.get("text").and_then(|t| t.as_str()).unwrap_or("");
    if text.is_empty() {
        return Err("text 为空".into());
    }
    let allowed = if let Some(gid) = chat.strip_prefix("group:") {
        matches!(gid.parse::<i64>(), Ok(g) if ctx.cfg.qq.groups.contains(&g))
    } else if let Some(uid) = chat.strip_prefix("private:") {
        match uid.parse::<i64>() {
            Ok(u) => ctx.cfg.qq.users.contains(&u) || ctx.cfg.qq.admins.contains(&u),
            _ => false,
        }
    } else {
        false
    };
    if !allowed {
        return Err(format!(
            "qq_send 目标 {chat} 不在白名单，拒绝发送。可用目标: 群 {:?}，私聊 {:?}。回复必须发回消息里标注的 @群号",
            ctx.cfg.qq.groups, ctx.cfg.qq.users
        ));
    }
    match ctx.qq_tx {
        Some(tx) => {
            tx.send(QqOut { chat: chat.to_string(), text: text.to_string() })
                .map_err(|_| "QQ 桥接通道已关闭".to_string())?;
            Ok(format!("已投递 QQ 消息到 {chat}。本轮回复已发送完毕，请直接结束输出，不要再调用 qq_send"))
        }
        None => Err("QQ 桥接未运行（启动方式: yjlcoder --qq）".into()),
    }
}
fn qq_arg_i64(args: &Value) -> Option<i64> {
    args.get("qq").and_then(|v| match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    })
}
fn qq_is_admin(args: &Value, ctx: &ToolCtx) -> Result<String, String> {
    let qq = qq_arg_i64(args).ok_or("缺少 qq 参数（QQ 号）")?;
    let admins = &ctx.cfg.qq.admins;
    if admins.contains(&qq) {
        Ok(format!("QQ {qq} 是管理员，可指挥 agent 操作电脑"))
    } else {
        Ok(format!("QQ {qq} 不是管理员（普通用户，仅闲聊）。当前管理员列表: {:?}", admins))
    }
}
fn qq_add_admin(args: &Value, ctx: &ToolCtx) -> Result<String, String> {
    let qq = qq_arg_i64(args).ok_or("缺少 qq 参数（QQ 号）")?;
    let mut c = ctx.cfg.clone();
    let added = c.qq.add_admin(qq);
    c.save();
    Ok(format!(
        "管理员 {qq} 已{}（已同步加入私聊白名单），重启 QQ 桥接后生效。当前管理员: {:?}",
        if added { "添加" } else { "存在" },
        c.qq.admins
    ))
}
fn mem_dir_of(ctx: &ToolCtx) -> std::path::PathBuf {
    match &ctx.mem_dir {
        Some(d) => d.clone(),
        None => ctx.cfg.data_dir().join("memory"),
    }
}
fn chat_to_mem_id(chat: &str) -> Result<String, String> {
    if let Some(gid) = chat.strip_prefix("group:") {
        let id: i64 = gid.trim().parse().map_err(|_| format!("群号格式错误: {gid}"))?;
        Ok(format!("qq_g{id}"))
    } else if let Some(uid) = chat.strip_prefix("private:") {
        let id: i64 = uid.trim().parse().map_err(|_| format!("QQ 号格式错误: {uid}"))?;
        Ok(format!("qq_u{id}"))
    } else {
        Err(format!("chat 格式错误（应为 group:群号 或 private:QQ号）: {chat}"))
    }
}
fn parse_mem_file(path: &Path) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let Ok(f) = std::fs::File::open(path) else {
        return out;
    };
    let mut cur_time = String::new();
    let mut cur_lines: Vec<String> = Vec::new();
    for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
        if let Some(rest) = line.trim_start().strip_prefix("## ") {
            if !cur_time.is_empty() && !cur_lines.is_empty() {
                out.push((std::mem::take(&mut cur_time), cur_lines.join("\n")));
                cur_lines.clear();
            }
            let mut ts: String = rest.chars().take(16).collect();
            if ts.chars().count() == 16 && rest.chars().nth(16) == Some(':') {
                ts.extend(rest.chars().skip(16).take(3));
            }
            cur_time = ts;
        } else if !line.trim().is_empty() {
            cur_lines.push(line.trim().to_string());
        }
    }
    if !cur_time.is_empty() && !cur_lines.is_empty() {
        out.push((cur_time, cur_lines.join("\n")));
    }
    out
}
fn memory_search(args: &Value, ctx: &ToolCtx) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("memory_search 需要 query 参数（搜索关键词）")?;
    let chat = args.get("chat").and_then(|c| c.as_str()).map(str::trim).filter(|s| !s.is_empty());
    let mem_dir = mem_dir_of(ctx);
    let keywords: Vec<String> = query
        .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if keywords.is_empty() {
        return Err("memory_search 的 query 不能全是符号".into());
    }
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    if let Some(chat) = chat {
        if let Ok(id) = chat_to_mem_id(chat) {
            let f = mem_dir.join(format!("{id}.md"));
            if f.exists() {
                files.push(f);
            }
        }
    } else if let Ok(rd) = std::fs::read_dir(&mem_dir) {
        for e in rd.filter_map(Result::ok) {
            if e.path().extension().map(|x| x == "md").unwrap_or(false) {
                files.push(e.path());
            }
        }
    }
    if files.is_empty() {
        return Ok(format!("没有可搜索的记忆文件（{query}）。记忆由 QQ 对话自动记录，或由 memory_write 写入。"));
    }
    let mut hits: Vec<(String, String, String)> = Vec::new();
    for f in &files {
        let fname = f.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        for (time, content) in parse_mem_file(f) {
            if keywords.iter().all(|k| content.contains(k.as_str())) {
                hits.push((time, content, fname.clone()));
            }
        }
    }
    if hits.is_empty() {
        return Ok(format!("记忆中未找到与“{query}”相关的内容。"));
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0));                       
    let mut out = format!("找到 {} 条相关记忆（关键词: {query}）:\n", hits.len());
    let mut srcs: Vec<String> = Vec::new();
    for (time, content, src) in hits.into_iter().take(5) {
        let snippet: String = content.chars().take(120).collect();
        out.push_str(&format!("[{time}] {snippet}\n"));
        if !srcs.contains(&src) {
            srcs.push(src);
        }
    }
    out.push_str(&format!(
        "（共 {} 个记忆文件: {}；如需查看完整条目，用 readline 读取对应文件）",
        files.len(),
        srcs.join("、")
    ));
    Ok(out)
}
fn memory_write(args: &Value, ctx: &ToolCtx) -> Result<String, String> {
    let chat = args
        .get("chat")
        .and_then(|c| c.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("memory_write 需要 chat 参数（要记住哪个群，如 group:728563593）")?;
    let content = args
        .get("content")
        .and_then(|c| c.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("memory_write 需要 content 参数（要记住的内容，一句话即可）")?;
    let id = chat_to_mem_id(chat)?;
    let mem_dir = mem_dir_of(ctx);
    if std::fs::create_dir_all(&mem_dir).is_err() {
        return Err(format!("无法创建记忆目录 {}", mem_dir.display()));
    }
    let mem_file = mem_dir.join(format!("{id}.md"));
    let entry = format!("\n## {}\n{}\n", crate::time::now_stamp(), content);
    match std::fs::OpenOptions::new().create(true).append(true).open(&mem_file) {
        Ok(mut f) => {
            use std::io::Write;
            if f.write_all(entry.as_bytes()).is_err() {
                return Err("记忆写入失败".into());
            }
            Ok(format!("已写入记忆: {content}\n（文件: {}）", mem_file.display()))
        }
        Err(e) => Err(format!("记忆文件打开失败: {e}")),
    }
}
fn ask_user(args: &Value, ctx: &mut ToolCtx) -> Result<String, String> {
    let handle = ctx
        .ask
        .as_ref()
        .ok_or("当前模式不支持提问（ask_user 仅 TUI 交互可用）")?;
    let questions = parse_ask_questions(args)?;
    let question_order: Vec<String> = questions.iter().map(|question| question.question.clone()).collect();
    let id = handle.seq.fetch_add(1, Ordering::Relaxed);
    handle
        .tx
        .send(AskRequest { id, questions })
        .map_err(|_| "提问通道已关闭".to_string())?;
    loop {
        if handle.cancel.load(Ordering::Relaxed) {
            return Err("提问被取消".into());
        }
        match handle.rx.recv_timeout(Duration::from_millis(200)) {
            Ok(a) if a.id == id => {
                let mut ordered_answers: Vec<(&String, &String)> = question_order
                    .iter()
                    .filter_map(|question| a.answers.get(question).map(|answer| (question, answer)))
                    .collect();
                ordered_answers.extend(
                    a.answers
                        .iter()
                        .filter(|(question, _)| !question_order.contains(question)),
                );
                let answers_text = ordered_answers
                    .into_iter()
                    .map(|(question, answer)| {
                        format!(
                            "{}={}",
                            serde_json::to_string(question).unwrap_or_else(|_| format!("\"{question}\"")),
                            serde_json::to_string(answer).unwrap_or_else(|_| format!("\"{answer}\""))
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                return Ok(format!(
                    "User has answered your questions: {answers_text}. You can now continue with the user's answers in mind."
                ));
            }
            Ok(_) => continue,              
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Err("提问通道已断开".into()),
        }
    }
}
fn parse_ask_questions(args: &Value) -> Result<Vec<AskQuestion>, String> {
    let raw_questions: Vec<Value> = match args.get("questions") {
        Some(Value::Array(values)) => values.clone(),
        Some(Value::String(question)) => vec![Value::String(question.clone())],
        Some(Value::Object(question)) => vec![Value::Object(question.clone())],
        _ => args
            .get("question")
            .cloned()
            .map(|question| vec![question])
            .unwrap_or_default(),
    };
    if raw_questions.is_empty() {
        return Err("ask_user 需要 questions（Claude Code 结构化问题数组；旧版字符串数组也兼容）".into());
    }
    let mut questions = Vec::new();
    let mut used_questions = HashSet::new();
    for (index, raw) in raw_questions.into_iter().take(4).enumerate() {
        let (question, header, options, multi_select) = match raw {
            Value::String(question) => (
                question,
                format!("问题{}", index + 1),
                Vec::new(),
                false,
            ),
            Value::Object(object) => {
                let question = object
                    .get("question")
                    .or_else(|| object.get("text"))
                    .or_else(|| object.get("prompt"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let header = object
                    .get("header")
                    .or_else(|| object.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let options = parse_ask_options(object.get("options"));
                let multi_select = object
                    .get("multiSelect")
                    .or_else(|| object.get("multi_select"))
                    .and_then(boolish)
                    .unwrap_or(false);
                (question, header, options, multi_select)
            }
            _ => continue,
        };
        if question.trim().is_empty() {
            continue;
        }
        let mut unique_question = question.trim().to_string();
        if !used_questions.insert(unique_question.clone()) {
            unique_question = format!("{}（{}）", unique_question, index + 1);
            used_questions.insert(unique_question.clone());
        }
        let header = if header.trim().is_empty() {
            format!("问题{}", index + 1)
        } else {
            header.chars().take(12).collect()
        };
        questions.push(AskQuestion {
            question: unique_question,
            header,
            options,
            multi_select,
        });
    }
    if questions.is_empty() {
        return Err("ask_user 的问题文本不能为空".into());
    }
    Ok(questions)
}
fn parse_ask_options(raw: Option<&Value>) -> Vec<AskOption> {
    let values: Vec<Value> = match raw {
        Some(Value::Array(values)) => values.clone(),
        Some(Value::String(labels)) => labels
            .split([',', '，', '\n'])
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(|label| Value::String(label.to_string()))
            .collect(),
        _ => Vec::new(),
    };
    let mut options = Vec::new();
    let mut labels = HashSet::new();
    for value in values {
        let option = match value {
            Value::String(label) => AskOption {
                label: label.trim().to_string(),
                description: String::new(),
                preview: None,
            },
            Value::Object(object) => AskOption {
                label: object
                    .get("label")
                    .or_else(|| object.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                description: object
                    .get("description")
                    .or_else(|| object.get("desc"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                preview: object
                    .get("preview")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(String::from),
            },
            _ => continue,
        };
        if !option.label.is_empty() && labels.insert(option.label.clone()) {
            options.push(option);
            if options.len() >= 4 {
                break;
            }
        }
    }
    options
}
fn boolish(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        Value::Number(value) => value.as_i64().map(|value| value != 0),
        Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" | "是" => Some(true),
            "false" | "0" | "no" | "off" | "否" => Some(false),
            _ => None,
        },
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    struct TestCtx<'a> {
        cfg: Config,
        store: SessionStore,
        llm: Llm,
        cancel: AtomicBool,
        ask_tx: Option<Sender<AskRequest>>,
        ask_rx: Option<Receiver<AskAnswer>>,
        ask_seq: AtomicU64,
        perm_tx: Option<Sender<PermRequest>>,
        perm_rx: Option<Receiver<PermDecision>>,
        perm_seq: AtomicU64,
        perm_auto: AtomicBool,
        perm_allowed: Mutex<HashSet<String>>,
        _marker: std::marker::PhantomData<&'a ()>,
    }
    impl<'a> TestCtx<'a> {
        fn new(dir: std::path::PathBuf) -> Self {
            TestCtx {
                cfg: Config::default(),
                store: SessionStore::new(dir),
                llm: Llm::mock(),
                cancel: AtomicBool::new(false),
                ask_tx: None,
                ask_rx: None,
                ask_seq: AtomicU64::new(0),
                perm_tx: None,
                perm_rx: None,
                perm_seq: AtomicU64::new(0),
                perm_auto: AtomicBool::new(false),
                perm_allowed: Mutex::new(HashSet::new()),
                _marker: std::marker::PhantomData,
            }
        }
        fn ctx(&mut self) -> ToolCtx<'_> {
            ToolCtx {
                cfg: &self.cfg,
                store: &mut self.store,
                llm: &self.llm,
                qq_tx: None,
                cancel: &self.cancel,
                mem_dir: None,
                ask: self
                    .ask_tx
                    .as_ref()
                    .zip(self.ask_rx.as_ref())
                    .map(|(tx, rx)| AskHandle {
                        tx,
                        rx,
                        cancel: &self.cancel,
                        seq: &self.ask_seq,
                    }),
                perm: self
                    .perm_tx
                    .as_ref()
                    .zip(self.perm_rx.as_ref())
                    .map(|(tx, rx)| PermHandle {
                        tx,
                        rx,
                        cancel: &self.cancel,
                        seq: &self.perm_seq,
                        auto: &self.perm_auto,
                        allowed: &self.perm_allowed,
                    }),
            }
        }
        fn ctx_with_mem(&mut self, mem_dir: std::path::PathBuf) -> ToolCtx<'_> {
            let mut c = self.ctx();
            c.mem_dir = Some(mem_dir);
            c
        }
        fn with_ask(&mut self, tx: Sender<AskRequest>, rx: Receiver<AskAnswer>) {
            self.ask_tx = Some(tx);
            self.ask_rx = Some(rx);
        }
        fn with_perm(&mut self, tx: Sender<PermRequest>, rx: Receiver<PermDecision>) {
            self.perm_tx = Some(tx);
            self.perm_rx = Some(rx);
        }
    }
    #[test]
    fn ask_user_sends_questions_and_waits_for_answer() {
        let tmp = std::env::temp_dir().join(format!("yjlcoder-ask-{}", std::process::id()));
        let mut t = TestCtx::new(tmp.clone());
        let (ask_tx, ask_rx) = std::sync::mpsc::channel();
        let (answer_tx, answer_rx) = std::sync::mpsc::channel();
        t.with_ask(ask_tx, answer_rx);
        let args = serde_json::json!({"questions": [{
            "question": "你想看哪个配置文件？",
            "header": "配置文件",
            "options": [
                {"label": "用户配置", "description": "读取当前用户配置"},
                {"label": "项目配置", "description": "读取项目内配置", "preview": "config.json"}
            ],
            "multiSelect": false
        }]});
        let h = std::thread::spawn(move || {
            let req = ask_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            assert_eq!(req.id, 0);
            assert_eq!(req.questions.len(), 1);
            assert_eq!(req.questions[0].question, "你想看哪个配置文件？");
            assert_eq!(req.questions[0].header, "配置文件");
            assert_eq!(req.questions[0].options[1].preview.as_deref(), Some("config.json"));
            answer_tx
                .send(AskAnswer {
                    id: req.id,
                    answers: BTreeMap::from([("你想看哪个配置文件？".into(), "项目配置".into())]),
                })
                .unwrap();
        });
        let r = ask_user(&args, &mut t.ctx()).unwrap();
        h.join().unwrap();
        assert_eq!(
            r,
            "User has answered your questions: \"你想看哪个配置文件？\"=\"项目配置\". You can now continue with the user's answers in mind."
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
    fn spawn_perm_answerer(
        perm_rx: Receiver<PermRequest>,
        dec_tx: Sender<PermDecision>,
        decisions: Vec<PermDecisionKind>,
    ) -> std::thread::JoinHandle<Vec<PermRequest>> {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            for decision in decisions {
                let req = perm_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
                let _ = dec_tx.send(PermDecision { id: req.id, decision });
                out.push(req);
            }
            out
        })
    }
    #[test]
    fn execute_command_perm_yes_runs_once() {
        let tmp = std::env::temp_dir().join(format!("yjlcoder-perm-{}", std::process::id()));
        let mut t = TestCtx::new(tmp.clone());
        let (perm_tx, perm_rx) = std::sync::mpsc::channel();
        let (dec_tx, dec_rx) = std::sync::mpsc::channel();
        t.with_perm(perm_tx, dec_rx);
        let answerer = spawn_perm_answerer(perm_rx, dec_tx, vec![PermDecisionKind::Yes]);
        let r = execute("execute_command", &json!({"cmd": "echo perm-yes"}), &mut t.ctx()).unwrap();
        let reqs = answerer.join().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].id, 0, "请求号从 0 起");
        assert_eq!(reqs[0].cmd, "echo perm-yes");
        assert_eq!(reqs[0].cmd_kind, "echo", "命令类型 = 首词");
        assert!(r.contains("perm-yes"), "Yes 后命令真正执行: {r}");
        let _ = std::fs::remove_dir_all(&tmp);
    }
    #[test]
    fn execute_command_perm_no_rejects_with_guidance() {
        let tmp = std::env::temp_dir().join(format!("yjlcoder-perm-{}", std::process::id()));
        let mut t = TestCtx::new(tmp.clone());
        let (perm_tx, perm_rx) = std::sync::mpsc::channel();
        let (dec_tx, dec_rx) = std::sync::mpsc::channel();
        t.with_perm(perm_tx, dec_rx);
        let answerer = spawn_perm_answerer(perm_rx, dec_tx, vec![PermDecisionKind::No]);
        let err = execute("execute_command", &json!({"cmd": "echo blocked"}), &mut t.ctx()).unwrap_err();
        answerer.join().unwrap();
        assert!(err.contains("用户拒绝了命令执行"), "拒绝原因要讲清楚: {err}");
        assert!(err.contains("echo blocked"), "拒绝信息里带上命令原文: {err}");
        let _ = std::fs::remove_dir_all(&tmp);
    }
    #[test]
    fn execute_command_perm_always_allow_whitelists_kind() {
        let tmp = std::env::temp_dir().join(format!("yjlcoder-perm-{}", std::process::id()));
        let mut t = TestCtx::new(tmp.clone());
        let (perm_tx, perm_rx) = std::sync::mpsc::channel();
        let (dec_tx, dec_rx) = std::sync::mpsc::channel();
        t.with_perm(perm_tx, dec_rx);
        let answerer = spawn_perm_answerer(
            perm_rx,
            dec_tx,
            vec![PermDecisionKind::AlwaysAllow, PermDecisionKind::No],
        );
        let r = execute("execute_command", &json!({"cmd": "echo first"}), &mut t.ctx()).unwrap();
        assert!(r.contains("first"));
        let r2 = execute("execute_command", &json!({"cmd": "echo second"}), &mut t.ctx()).unwrap();
        assert!(r2.contains("second"));
        let err = execute("execute_command", &json!({"cmd": "uname -s"}), &mut t.ctx()).unwrap_err();
        assert!(err.contains("用户拒绝了命令执行"));
        let reqs = answerer.join().unwrap();
        assert_eq!(reqs.len(), 2, "两条请求都应发出并消费");
        assert_eq!(reqs[0].cmd_kind, "echo");
        assert_eq!(reqs[1].cmd_kind, "uname");
        let _ = std::fs::remove_dir_all(&tmp);
    }
    #[test]
    fn execute_command_perm_auto_skips_prompt() {
        let tmp = std::env::temp_dir().join(format!("yjlcoder-perm-{}", std::process::id()));
        let mut t = TestCtx::new(tmp.clone());
        let (perm_tx, perm_rx) = std::sync::mpsc::channel();
        let (_dec_tx, dec_rx) = std::sync::mpsc::channel();
        t.with_perm(perm_tx, dec_rx);
        t.perm_auto.store(true, Ordering::Relaxed);
        let r = execute("execute_command", &json!({"cmd": "echo auto"}), &mut t.ctx()).unwrap();
        assert!(r.contains("auto"));
        assert_eq!(perm_rx.try_recv().unwrap_err(), std::sync::mpsc::TryRecvError::Empty);
        let _ = std::fs::remove_dir_all(&tmp);
    }
    #[test]
    fn execute_command_perm_auto_enable_runs_and_flips_auto() {
        let tmp = std::env::temp_dir().join(format!("yjlcoder-perm-{}", std::process::id()));
        let mut t = TestCtx::new(tmp.clone());
        let (perm_tx, perm_rx) = std::sync::mpsc::channel();
        let (dec_tx, dec_rx) = std::sync::mpsc::channel();
        t.with_perm(perm_tx, dec_rx);
        let answerer = std::thread::spawn(move || {
            let req = perm_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            let _ = dec_tx.send(PermDecision { id: req.id, decision: PermDecisionKind::AutoEnable });
            match perm_rx.recv_timeout(std::time::Duration::from_millis(300)) {
                Ok(extra) => panic!("auto 开启后仍收到请求: {extra:?}"),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(e) => panic!("通道异常: {e}"),
            }
        });
        let r = execute("execute_command", &json!({"cmd": "echo first"}), &mut t.ctx()).unwrap();
        assert!(r.contains("first"), "AutoEnable 也放行本条命令: {r}");
        assert!(t.perm_auto.load(Ordering::Relaxed), "AutoEnable 应开启 auto 标志");
        let r2 = execute("execute_command", &json!({"cmd": "echo second"}), &mut t.ctx()).unwrap();
        assert!(r2.contains("second"));
        answerer.join().unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }
    #[test]
    fn execute_command_perm_cancel_aborts() {
        let tmp = std::env::temp_dir().join(format!("yjlcoder-perm-{}", std::process::id()));
        let mut t = TestCtx::new(tmp.clone());
        let (perm_tx, _perm_rx) = std::sync::mpsc::channel();
        let (_dec_tx, dec_rx) = std::sync::mpsc::channel();
        t.with_perm(perm_tx, dec_rx);
        t.cancel.store(true, Ordering::Relaxed);
        let err = execute("execute_command", &json!({"cmd": "echo x"}), &mut t.ctx()).unwrap_err();
        assert!(err.contains("命令被取消"), "取消时返回明确错误: {err}");
        let _ = std::fs::remove_dir_all(&tmp);
    }
    #[test]
    fn ask_user_question_string_and_id_filtering() {
        let tmp = std::env::temp_dir().join(format!("yjlcoder-ask2-{}", std::process::id()));
        let mut t = TestCtx::new(tmp.clone());
        let (ask_tx, ask_rx) = std::sync::mpsc::channel();
        let (answer_tx, answer_rx) = std::sync::mpsc::channel();
        t.with_ask(ask_tx, answer_rx);
        let args0 = serde_json::json!({"question": "第一个问题"});
        let h0 = std::thread::spawn(move || {
            let req = ask_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            assert_eq!(req.id, 0);
            assert_eq!(req.questions[0].question, "第一个问题");
            assert!(req.questions[0].options.is_empty(), "旧版自由文本问题保持兼容");
            answer_tx
                .send(AskAnswer {
                    id: 99,
                    answers: BTreeMap::from([("第一个问题".into(), "错轮".into())]),
                })
                .unwrap();
            answer_tx
                .send(AskAnswer {
                    id: req.id,
                    answers: BTreeMap::from([("第一个问题".into(), "回答一".into())]),
                })
                .unwrap();
        });
        let r0 = ask_user(&args0, &mut t.ctx()).unwrap();
        h0.join().unwrap();
        assert!(r0.contains("\"第一个问题\"=\"回答一\""));
        let _ = std::fs::remove_dir_all(&tmp);
    }
    #[test]
    fn ask_user_normalizes_weak_model_shapes() {
        let questions = parse_ask_questions(&serde_json::json!({
            "questions": [
                {
                    "text": "选择哪些功能？",
                    "title": "这是一个超过十二字符的标题",
                    "options": "日志,指标,日志,告警,备份,额外",
                    "multi_select": "yes"
                },
                {"question": "选择哪些功能？", "options": [{"name":"自动"}, {"label":"手动","desc":"自己控制"}]}
            ]
        }))
        .unwrap();
        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0].header.chars().count(), 12);
        assert_eq!(questions[0].options.len(), 4, "去重并限制为 4 个选项");
        assert!(questions[0].multi_select);
        assert_ne!(questions[0].question, questions[1].question, "重复问题文本必须变成唯一键");
        assert_eq!(questions[1].options[1].description, "自己控制");
    }
    #[test]
    fn ask_user_without_channel_errors() {
        let tmp = std::env::temp_dir().join(format!("yjlcoder-ask3-{}", std::process::id()));
        let mut t = TestCtx::new(tmp.clone());
        let args = serde_json::json!({"questions": ["去哪？"]});
        let r = ask_user(&args, &mut t.ctx());
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("仅 TUI"));
        let _ = std::fs::remove_dir_all(&tmp);
    }
    #[test]
    fn ask_user_requires_questions() {
        let tmp = std::env::temp_dir().join(format!("yjlcoder-ask4-{}", std::process::id()));
        let mut t = TestCtx::new(tmp.clone());
        let (ask_tx, _ask_rx) = std::sync::mpsc::channel();
        let (_answer_tx, answer_rx) = std::sync::mpsc::channel();
        t.with_ask(ask_tx, answer_rx);
        let r = ask_user(&serde_json::json!({}), &mut t.ctx());
        assert!(r.is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }
    #[test]
    fn glob_match_basic() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "main.py"));
        assert!(glob_match("**/src/*.rs", "a/b/src/lib.rs"));
        assert!(!glob_match("**/src/*.rs", "a/b/src/lib/x.rs"));
        assert!(glob_match("**/src/*.rs", "src/lib.rs"));
        assert!(glob_match("docs/**", "docs/guide/readme.md"));
    }
    #[test]
    fn file_ops_roundtrip() {
        let d = std::env::temp_dir().join(format!("yjlcoder_tools_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("t.txt");
        let p = f.to_string_lossy().into_owned();
        let mut t = TestCtx::new(d.clone());
        let r = execute("writefile", &json!({"path": p, "content": "hello\nworld\n"}), &mut t.ctx()).unwrap();
        assert!(r.contains("已写入"));
        let r = execute("editline", &json!({"path": p, "old": "world", "new": "rust"}), &mut t.ctx()).unwrap();
        assert!(r.contains("已替换"));
        let r = execute("readline", &json!({"path": p}), &mut t.ctx()).unwrap();
        assert!(r.contains("rust"));
        execute("appendline", &json!({"path": p, "content": "third"}), &mut t.ctx()).unwrap();
        let content = std::fs::read_to_string(&p).unwrap();
        assert!(content.contains("third"));
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn listdir_is_sorted_paged_and_has_an_exact_continuation() {
        let d = std::env::temp_dir().join(format!("yjlcoder_listdir_page_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("z-dir")).unwrap();
        std::fs::create_dir_all(d.join("a-dir")).unwrap();
        std::fs::write(d.join("b.txt"), "b").unwrap();
        std::fs::write(d.join("a.txt"), "a").unwrap();
        std::fs::write(d.join("line\nbreak"), "escaped").unwrap();
        let store = d.with_extension("store");
        let _ = std::fs::remove_dir_all(&store);
        let mut t = TestCtx::new(store.clone());
        let path = d.to_string_lossy().into_owned();
        let first = execute(
            "ls",
            &json!({"file_path": path, "offset":"1", "page_size":"2"}),
            &mut t.ctx(),
        )
        .unwrap();
        assert!(first.contains("Entries: 5 total; showing 2-3."), "{first}");
        let item_lines: Vec<&str> = first
            .lines()
            .filter(|line| line.starts_with("dir\t") || line.starts_with("file\t"))
            .collect();
        assert_eq!(item_lines, vec!["dir\tz-dir/", "file\ta.txt"]);
        assert!(first.contains("\"offset\":3,\"limit\":2"), "{first}");
        assert!(!first.contains("兼容层已纠正"));
        let second = execute(
            "list_directory",
            &json!({"path": d, "offset":3, "limit":2}),
            &mut t.ctx(),
        )
        .unwrap();
        assert!(second.contains("file\tb.txt"));
        assert!(second.contains("file\tline\\nbreak"));
        assert!(!second.contains("Next page:"));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&store);
    }
    #[test]
    fn execute_command_simple_ls_uses_the_same_complete_page() {
        let d = std::env::temp_dir().join(format!("yjlcoder_listdir_shell_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("visible"), "x").unwrap();
        std::fs::write(d.join(".hidden"), "x").unwrap();
        let store = d.with_extension("store");
        let _ = std::fs::remove_dir_all(&store);
        let mut t = TestCtx::new(store.clone());
        let plain = execute(
            "execute_command",
            &json!({"cmd":"ls", "cwd":d}),
            &mut t.ctx(),
        )
        .unwrap();
        assert!(plain.contains("file\tvisible"));
        assert!(!plain.contains(".hidden"));
        assert!(!plain.contains("exit code:"), "simple ls should use listdir: {plain}");
        let all = execute(
            "execute_command",
            &json!({"cmd":"ls -la", "cwd":d}),
            &mut t.ctx(),
        )
        .unwrap();
        assert!(all.contains("file\t.hidden"));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&store);
    }
    #[test]
    fn listdir_empty_past_end_and_invalid_limits_are_explicit() {
        let d = std::env::temp_dir().join(format!("yjlcoder_listdir_edges_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let store = d.with_extension("store");
        let _ = std::fs::remove_dir_all(&store);
        let mut t = TestCtx::new(store.clone());
        let empty = execute("listdir", &json!({"path": d}), &mut t.ctx()).unwrap();
        assert!(empty.contains("Entries: 0 total; showing 0."));
        std::fs::write(d.join("one"), "1").unwrap();
        let past = execute(
            "listdir",
            &json!({"path": d, "offset":99, "limit":10}),
            &mut t.ctx(),
        )
        .unwrap();
        assert!(past.contains("Entries: 1 total; offset 99 is past the end."));
        assert!(execute("listdir", &json!({"path": d, "limit":0}), &mut t.ctx())
            .unwrap_err()
            .contains("大于 0"));
        assert!(execute("listdir", &json!({"path": d, "limit":1001}), &mut t.ctx())
            .unwrap_err()
            .contains("不能超过 1000"));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&store);
    }
    #[test]
    fn read_defaults_to_complete_2000_line_pages() {
        let d = std::env::temp_dir().join(format!("yjlcoder_read_range_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("range.txt");
        let content = (1..=2_105).map(|line| format!("line-{line}\n")).collect::<String>();
        std::fs::write(&f, content).unwrap();
        let mut t = TestCtx::new(d.clone());
        let ranged = execute(
            "Read",
            &json!({"file_path": f.to_string_lossy(), "offset": 2001, "limit": 3}),
            &mut t.ctx(),
        )
        .unwrap();
        assert!(ranged.starts_with("2001\tline-2001\n2002\tline-2002\n2003\tline-2003\n"));
        assert!(ranged.contains("Read lines 2001-2003 of 2105"));
        assert!(ranged.contains("\"offset\":2004"));
        let default_read = execute("readline", &json!({"path": f}), &mut t.ctx()).unwrap();
        assert!(default_read.starts_with("1\tline-1\n"));
        assert!(default_read.contains("2000\tline-2000"));
        assert!(!default_read.contains("2001\tline-2001"));
        assert!(default_read.contains("Read lines 1-2000 of 2105"));
        assert!(default_read.contains("\"offset\":2001"));
        assert!(!default_read.contains("truncated"));
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn read_rejects_large_whole_file_but_allows_explicit_page() {
        let d = std::env::temp_dir().join(format!("yjlcoder_read_large_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("large.txt");
        let content = "abcdefghij\n".repeat(30_000);            
        std::fs::write(&f, content).unwrap();
        let mut t = TestCtx::new(d.clone());
        let error = execute("readline", &json!({"path": f}), &mut t.ctx()).unwrap_err();
        assert!(error.contains("exceeds maximum allowed size"));
        assert!(error.contains("offset and limit"));
        let page = execute(
            "readline",
            &json!({"path": f, "start": 29_999, "limit": 2}),
            &mut t.ctx(),
        )
        .unwrap();
        assert_eq!(page, "29999\tabcdefghij\n30000\tabcdefghij");
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn read_empty_and_past_eof_return_explicit_reminders() {
        let d = std::env::temp_dir().join(format!("yjlcoder_read_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let empty = d.join("empty.txt");
        let short = d.join("short.txt");
        std::fs::write(&empty, "").unwrap();
        std::fs::write(&short, "only\n").unwrap();
        let mut t = TestCtx::new(d.clone());
        let empty_result = execute("readline", &json!({"path": empty}), &mut t.ctx()).unwrap();
        assert!(empty_result.contains("contents are empty"));
        let past = execute(
            "readline",
            &json!({"path": short, "offset": 5, "limit": 2}),
            &mut t.ctx(),
        )
        .unwrap();
        assert!(past.contains("shorter than the provided offset (5)"));
        assert!(past.contains("1 lines"));
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn read_never_silently_truncates_token_overflow() {
        let d = std::env::temp_dir().join(format!("yjlcoder_read_tokens_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("one-long-line.txt");
        std::fs::write(&f, "x".repeat(FILE_READ_MAX_OUTPUT_TOKENS * 4 + 32)).unwrap();
        let mut t = TestCtx::new(d.clone());
        let error = execute(
            "readline",
            &json!({"path": f, "offset": 1, "limit": 1}),
            &mut t.ctx(),
        )
        .unwrap_err();
        assert!(error.contains("exceeds maximum allowed tokens"));
        assert!(!error.contains("truncated"));
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn editline_fuzzy_match_preserves_unaffected_lines_and_crlf() {
        let d = std::env::temp_dir().join(format!("yjlcoder_edit_fuzzy_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("fuzzy.txt");
        std::fs::write(&f, "keep   \r\nlet s = “old”—value;   \r\nlast\r\n").unwrap();
        let p = f.to_string_lossy().into_owned();
        let mut t = TestCtx::new(d.clone());
        let result = execute(
            "editline",
            &json!({"path": p, "old": "let s = \"old\"-value;", "new": "let s = \"new\";"}),
            &mut t.ctx(),
        )
        .unwrap();
        assert!(result.contains("容错匹配"));
        let edited = std::fs::read_to_string(&f).unwrap();
        assert!(edited.starts_with("keep   \r\n"), "未触及行必须保持尾随空格和 CRLF");
        assert!(edited.contains("let s = \"new\";\r\n"));
        assert!(edited.ends_with("last\r\n"));
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn editline_refuses_ambiguous_matches() {
        let d = std::env::temp_dir().join(format!("yjlcoder_edit_ambiguous_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("ambiguous.txt");
        std::fs::write(&f, "same\nsame\n").unwrap();
        let p = f.to_string_lossy().into_owned();
        let mut t = TestCtx::new(d.clone());
        let error = execute("editline", &json!({"path": p, "old": "same", "new": "x"}), &mut t.ctx()).unwrap_err();
        assert!(error.contains("2 处匹配"));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "same\nsame\n");
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn execute_entry_repairs_aliases_nested_args_and_key_names() {
        let d = std::env::temp_dir().join(format!("yjlcoder_compat_entry_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("compat.txt");
        let p = f.to_string_lossy().into_owned();
        let mut t = TestCtx::new(d.clone());
        let written = execute(
            "Functions.Write-File",
            &json!({"arguments": {"file_path": p, "text": "needle\n"}}),
            &mut t.ctx(),
        )
        .unwrap();
        assert!(written.contains("兼容层已纠正"));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "needle\n");
        let searched = execute(
            "search",
            &json!({"path": f.to_string_lossy(), "q": "needle"}),
            &mut t.ctx(),
        )
        .unwrap();
        assert!(searched.contains("search→grep"));
        assert!(searched.contains("needle"));
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn grep_single_file_hits() {
        let d = std::env::temp_dir().join(format!("yjlcoder_grep_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("conf.toml");
        std::fs::write(&f, "line one\nwifi = true\nline three\n").unwrap();
        let p = f.to_string_lossy().into_owned();
        let mut t = TestCtx::new(d.clone());
        let r = execute("grep", &json!({"pattern": "wifi", "path": p}), &mut t.ctx()).unwrap();
        assert!(r.contains("命中"), "单文件 grep 应命中: {r}");
        assert!(r.contains("wifi = true"));
        let r2 = execute("grep", &json!({"pattern": "nomatch", "path": p}), &mut t.ctx()).unwrap();
        assert!(r2.contains("扫描 1 个文件"), "未命中时也应报告扫描 1 个文件: {r2}");
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn execute_command_shell() {
        let mut t = TestCtx::new(std::env::temp_dir().join("yjlcoder_exec_store"));
        let r = execute("execute_command", &json!({"cmd": "echo yjlcoder-test"}), &mut t.ctx()).unwrap();
        assert!(r.contains("yjlcoder-test"));
    }
    #[test]
    fn execute_command_never_runs_cat_for_file_reads() {
        let d = std::env::temp_dir().join(format!("yjlcoder_no_shell_cat_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let file = d.join("demo.txt");
        std::fs::write(&file, "first\nmiddle\nlast\n").unwrap();
        let mut t = TestCtx::new(d.clone());
        let forced_read = execute(
            "execute_command",
            &json!({"cmd": format!("cat {}", file.display())}),
            &mut t.ctx(),
        )
        .unwrap();
        assert!(forced_read.contains("1\tfirst"));
        assert!(forced_read.contains("2\tmiddle"));
        assert!(forced_read.contains("3\tlast"));
        assert!(!forced_read.contains("exit code:"));
        for cmd in [
            format!("cat {} {}", file.display(), file.display()),
            format!("cat {} | grep middle", file.display()),
            format!("/bin/cat {}", file.display()),
            format!("sh -c 'cat {}'", file.display()),
            "printf x | cat".to_string(),
        ] {
            let error = execute("execute_command", &json!({"cmd": cmd}), &mut t.ctx()).unwrap_err();
            assert!(error.contains("禁止通过 shell/cat"), "error: {error}");
            assert!(error.contains("readline"), "error: {error}");
        }
        let harmless = execute(
            "execute_command",
            &json!({"cmd": "printf '%s' cat"}),
            &mut t.ctx(),
        )
        .unwrap();
        assert!(harmless.contains("cat"));
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn read_rejects_binary_and_oversized_pages_explicitly() {
        let d = std::env::temp_dir().join(format!("yjlcoder_read_safety_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let binary = d.join("binary.bin");
        std::fs::write(&binary, b"text\0binary\n").unwrap();
        let text = d.join("text.txt");
        std::fs::write(&text, "line\n").unwrap();
        let mut t = TestCtx::new(d.clone());
        let binary_error = execute("readline", &json!({"path": binary}), &mut t.ctx()).unwrap_err();
        assert!(binary_error.contains("appears to be binary"));
        let page_error = execute(
            "readline",
            &json!({"path": text, "offset":1, "limit":2001}),
            &mut t.ctx(),
        )
        .unwrap_err();
        assert!(page_error.contains("limit cannot exceed 2000"));
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn execute_command_dispatches_op() {
        let mut t = TestCtx::new(std::env::temp_dir().join("yjlcoder_dispatch_store"));
        let r = execute("execute_command", &json!({"op": "list_tools", "args": {}}), &mut t.ctx()).unwrap();
        assert!(r.contains("[file]"));
    }
    fn hot_registration_spec(name: &str) -> Value {
        json!({
            "name":name,
            "description":"读取系统运行状态并返回清晰的纯文本结果；适合诊断机器负载和在线时间。",
            "parameters":{
                "type":"object",
                "properties":{},
                "required":[],
                "additionalProperties":false
            },
            "script":"#!/bin/sh\nprintf 'registered-ok\\n'",
            "timeout_secs":30
        })
    }
    #[test]
    fn execute_command_registers_and_runs_hot_tool_without_restart() {
        let dir = std::env::temp_dir().join(format!("yjlcoder_hot_tool_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut t = TestCtx::new(dir.join("sessions"));
        t.cfg.set_test_data_dir(dir.clone());
        let spec = hot_registration_spec("system_status_test");
        let registered = execute(
            "execute_command",
            &json!({"op":"make_tools", "args":spec}),
            &mut t.ctx(),
        )
        .unwrap();
        assert!(registered.contains("已热注册工具 system_status_test"));
        let output = execute(
            "execute_command",
            &json!({"op":"system_status_test", "args":{}}),
            &mut t.ctx(),
        )
        .unwrap();
        assert!(output.contains("registered-ok"));
        assert!(execute("list_tools", &json!({"category":"custom"}), &mut t.ctx())
            .unwrap()
            .contains("system_status_test"));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn execute_command_recovers_make_tools_json_accidentally_put_in_cmd() {
        let dir = std::env::temp_dir().join(format!("yjlcoder_make_cmd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut t = TestCtx::new(dir.join("sessions"));
        t.cfg.set_test_data_dir(dir.clone());
        let dispatch = json!({
            "op":"make_tools",
            "args":hot_registration_spec("system_status_cmd_test")
        });
        let result = execute(
            "execute_command",
            &json!({"cmd":serde_json::to_string(&dispatch).unwrap()}),
            &mut t.ctx(),
        )
        .unwrap();
        assert!(result.contains("已热注册工具 system_status_cmd_test"));
        assert!(crate::dynamic_tools::load(&t.cfg, "system_status_cmd_test").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn execute_command_self_op_runs_cmd() {
        let mut t = TestCtx::new(std::env::temp_dir().join("yjlcoder_selfop_store"));
        let r = execute("execute_command", &json!({"cmd": "echo hi", "op": "execute_command"}), &mut t.ctx()).unwrap();
        assert!(r.contains("hi"), "r: {r}");
    }
    #[test]
    fn execute_command_nested_self_op() {
        let mut t = TestCtx::new(std::env::temp_dir().join("yjlcoder_nestop_store"));
        let r = execute("execute_command", &json!({"op": "execute_command", "args": {"cmd": "echo nested"}}), &mut t.ctx()).unwrap();
        assert!(r.contains("nested"), "r: {r}");
    }
    #[test]
    fn execute_command_flat_op_form() {
        let mut t = TestCtx::new(std::env::temp_dir().join("yjlcoder_flatop_store"));
        let r = execute("execute_command", &json!({"op": "list_tools"}), &mut t.ctx()).unwrap();
        assert!(r.contains("[file]"), "r: {r}");
    }
    #[test]
    fn execute_command_keyword_cmd() {
        let mut t = TestCtx::new(std::env::temp_dir().join("yjlcoder_kw_store"));
        let p = std::env::temp_dir().join("yjlcoder_kw.txt");
        std::fs::write(&p, "kw-content").unwrap();
        let r = execute("execute_command", &json!({"cmd": format!("读取 {}", p.display())}), &mut t.ctx()).unwrap();
        assert!(r.contains("kw-content"), "r: {r}");
        let _ = std::fs::remove_file(&p);
    }
    #[test]
    fn keyword_tool_mapping() {
        assert_eq!(keyword_tool("搜索 马斯克"), Some(("web_search", json!({"query": "马斯克"}))));
        assert_eq!(keyword_tool("搜索马斯克"), Some(("web_search", json!({"query": "马斯克"}))));
        assert_eq!(keyword_tool("查一下 rust async"), Some(("web_search", json!({"query": "rust async"}))));
        assert_eq!(keyword_tool("帮我搜索一下rust"), Some(("web_search", json!({"query": "rust"}))));
        assert_eq!(keyword_tool("search foo bar"), Some(("web_search", json!({"query": "foo bar"}))));
        assert_eq!(
            keyword_tool("抓取 https://example.com/a b"),
            Some(("web_fetch", json!({"url": "https://example.com/a"})))
        );
        assert_eq!(keyword_tool("读取 Cargo.toml"), Some(("readline", json!({"path": "Cargo.toml"}))));
        assert_eq!(
            keyword_tool("读取 ~/.config/noctalia/noctalia-config.toml，告诉我第129行"),
            Some((
                "readline",
                json!({"path": "~/.config/noctalia/noctalia-config.toml"})
            ))
        );
        assert_eq!(
            keyword_tool("查看文件 \"/tmp/folder with spaces/demo.txt\" 后总结"),
            Some((
                "readline",
                json!({"path": "/tmp/folder with spaces/demo.txt"})
            ))
        );
        assert_eq!(keyword_tool("搜索功能怎么做"), None);
        assert_eq!(keyword_tool("搜索有什么好的工具"), None);
        assert_eq!(keyword_tool("ls -la"), None);
        assert_eq!(keyword_tool("echo 搜索 一下"), None);
        assert_eq!(keyword_tool("搜索"), None);
        assert_eq!(keyword_tool("  "), None);
    }
    #[test]
    fn qq_send_validates_whitelist() {
        let d = std::env::temp_dir().join(format!("yjlcoder_tools_qq_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mut t = TestCtx::new(d.clone());
        let mut cfg = Config::default();
        cfg.qq.groups = vec![728563593];
        cfg.qq.users = vec![3160168215];
        cfg.qq.admins = vec![3160168215];
        t.cfg = cfg;
        let (tx, rx) = std::sync::mpsc::channel::<QqOut>();
        {
            let mut ctx = t.ctx();
            ctx.qq_tx = Some(&tx);
            let r = execute("qq_send", &json!({"chat": "group:3160168215", "text": "hi"}), &mut ctx);
            let e = r.unwrap_err();
            assert!(e.contains("白名单"), "e: {e}");
            assert!(execute("qq_send", &json!({"chat": "private:999", "text": "hi"}), &mut ctx).is_err());
            assert!(execute("qq_send", &json!({"chat": "abc", "text": "hi"}), &mut ctx).is_err());
            assert!(execute("qq_send", &json!({"chat": "group:728563593", "text": ""}), &mut ctx).is_err());
            let r = execute("qq_send", &json!({"chat": "group:728563593", "text": "你好"}), &mut ctx).unwrap();
            assert!(r.contains("已投递"), "r: {r}");
            assert_eq!(rx.try_recv().unwrap().chat, "group:728563593");
            let r = execute("qq_send", &json!({"chat": "private:3160168215", "text": "hi"}), &mut ctx).unwrap();
            assert!(r.contains("已投递"), "r: {r}");
            assert_eq!(rx.try_recv().unwrap().chat, "private:3160168215");
        }
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn portscan_localhost() {
        let mut t = TestCtx::new(std::env::temp_dir().join("yjlcoder_scan_store"));
        let r = execute("portscan", &json!({"host": "127.0.0.1", "ports": "22,1", "timeout_ms": 300}), &mut t.ctx()).unwrap();
        assert!(r.contains("127.0.0.1"));
    }
    #[test]
    fn list_tools_unknown_category() {
        let mut t = TestCtx::new(std::env::temp_dir().join("yjlcoder_cat_store"));
        let r = execute("list_tools", &json!({"category": "nope"}), &mut t.ctx());
        assert!(r.is_err());
    }
    fn mem_dir_tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("yjlcoder_mem_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    #[test]
    fn memory_write_appends_timestamped() {
        let d = mem_dir_tmp("write");
        let mut t = TestCtx::new(std::env::temp_dir().join("yjlcoder_mem_store1"));
        {
            let mut ctx = t.ctx_with_mem(d.clone());
            assert!(execute("memory_write", &json!({"content": "x"}), &mut ctx).is_err());
            assert!(execute("memory_write", &json!({"chat": "group:728563593"}), &mut ctx).is_err());
            assert!(execute("memory_write", &json!({"chat": "abc", "content": "x"}), &mut ctx).is_err());
            let r = execute("memory_write", &json!({"chat": "group:728563593", "content": "刚才搜索马斯克"}), &mut ctx).unwrap();
            assert!(r.contains("已写入记忆"), "r: {r}");
        }
        let f = d.join("qq_g728563593.md");
        let content = std::fs::read_to_string(&f).unwrap();
        assert!(content.contains("刚才搜索马斯克"));
        assert!(content.contains("## "), "应有时间戳节: {content}");
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn memory_search_matches_keywords_by_time() {
        let d = mem_dir_tmp("search");
        std::fs::write(
            d.join("qq_g728563593.md"),
            "## 2026-08-13 15:36:10\n管理员说群公告要更新\n\n\
             ## 2026-08-13 15:37:20\n刚才搜索马斯克，模型是 nemotron\n\n\
             ## 2026-08-13 15:38:30\n马斯克喜欢发推特\n",
        )
        .unwrap();
        let mut t = TestCtx::new(std::env::temp_dir().join("yjlcoder_mem_store3"));
        let mut ctx = t.ctx_with_mem(d.clone());
        let r = execute("memory_search", &json!({"query": "马斯克", "chat": "group:728563593"}), &mut ctx).unwrap();
        assert!(r.contains("找到 2 条相关记忆"), "r: {r}");
        let idx_recent = r.find("马斯克喜欢发推特").unwrap();
        let idx_old = r.find("刚才搜索马斯克").unwrap();
        assert!(idx_recent < idx_old, "时间倒序: {r}");
        assert!(r.contains("readline"), "应提示读全文: {r}");
        let r2 = execute("memory_search", &json!({"query": "马斯克 nemotron"}), &mut ctx).unwrap();
        assert!(r2.contains("找到 1 条"), "AND 优先: {r2}");
        assert!(r2.contains("nemotron"), "{r2}");
        let r3 = execute("memory_search", &json!({"query": "不存在的东西xyz"}), &mut ctx).unwrap();
        assert!(r3.contains("未找到"), "r3: {r3}");
        assert!(execute("memory_search", &json!({"query": ""}), &mut ctx).is_err());
        let r4 = execute("memory_search", &json!({"query": "马斯克", "chat": "group:999"}), &mut ctx).unwrap();
        assert!(r4.contains("没有可搜索的记忆文件"), "r4: {r4}");
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn memory_search_parses_legacy_rollover_format() {
        let d = mem_dir_tmp("legacy");
        let f = d.join("qq_g728563593.md");
        std::fs::write(
            &f,
            "## 2026-08-13 14:00（自动记录，共 3 条消息）\n- 群成员是管理员\n- 聊过马斯克\n",
        )
        .unwrap();
        let mut t = TestCtx::new(std::env::temp_dir().join("yjlcoder_mem_store4"));
        let mut ctx = t.ctx_with_mem(d.clone());
        let r = execute("memory_search", &json!({"query": "马斯克", "chat": "group:728563593"}), &mut ctx).unwrap();
        assert!(r.contains("[2026-08-13 14:00]"), "时间戳来自节标题: {r}");
        assert!(r.contains("马斯克"), "r: {r}");
        let _ = std::fs::remove_dir_all(&d);
    }
    #[test]
    fn chat_to_mem_id_mapping() {
        assert_eq!(chat_to_mem_id("group:728563593").unwrap(), "qq_g728563593");
        assert_eq!(chat_to_mem_id("private:3160168215").unwrap(), "qq_u3160168215");
        assert!(chat_to_mem_id("abc").is_err());
        assert!(chat_to_mem_id("group:abc").is_err());
    }
}
