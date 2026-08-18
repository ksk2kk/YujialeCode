use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use crate::agent::Agent;
use crate::config::{Config, Qq};
use crate::llm::Msg;
use crate::llm::Llm;
use crate::session::SessionStore;
use crate::tools::QqOut;
struct ChatState {
    running: bool,
    pending: Option<(String, bool)>,
    msg_since_roll: usize,
    cancel: Arc<AtomicBool>,
}
fn is_admin(inner: &BridgeInner, ev: &Value) -> bool {
    let uid = ev["user_id"].as_i64().unwrap_or(0);
    inner.qq_cfg.admins.contains(&uid)
}
struct BridgeInner {
    cfg: Config,
    llm: Llm,
    qq_cfg: Qq,
    states: Mutex<HashMap<String, ChatState>>,
    cancel: Arc<AtomicBool>,
}
pub fn run(cfg: Config, llm: Llm, cancel: Arc<AtomicBool>) -> Result<(), String> {
    let qq_cfg = cfg.qq.clone();                                            
    let inner = Arc::new(BridgeInner {
        cfg: cfg.clone(),                                    
        llm,
        qq_cfg: qq_cfg.clone(),
        states: Mutex::new(HashMap::new()),
        cancel,
    });
    match qq_cfg.ws_mode.as_str() {
        "server" => run_server(inner, &qq_cfg),
        "client" => run_client(inner, &qq_cfg),
        other => Err(format!("未知 ws_mode: {other}（server / client）")),
    }
}
fn log(msg: &str) {
    let line = format!("[qq] {msg}\n");
    let _ = std::io::stderr().write_all(line.as_bytes());
}
fn run_server(inner: Arc<BridgeInner>, qq: &Qq) -> Result<(), String> {
    let addr = qq.ws_addr.clone();
    let listener = TcpListener::bind(&addr).map_err(|e| format!("监听 {addr} 失败: {e}"))?;
    log(&format!("OneBot 反向 WS 服务端已启动: ws://{addr}{}", qq.ws_path));
    log("请在 NapCat 中配置「反向 WebSocket」指向该地址（如 ws://127.0.0.1:6701/onebot/v11/ws）");
    for stream in listener.incoming() {
        let inner = inner.clone();
        match stream {
            Ok(s) => {
                thread::spawn(move || {
                    if let Err(e) = handle_ws_conn(inner, s) {
                        log(&format!("连接处理结束: {e}"));
                    }
                });
            }
            Err(e) => log(&format!("接受连接失败: {e}")),
        }
    }
    Ok(())
}
fn handle_ws_conn(
    inner: Arc<BridgeInner>,
    stream: std::net::TcpStream,
) -> Result<(), String> {
    let mut ws = tungstenite::accept(stream).map_err(|e| format!("WS 握手失败: {e}"))?;
    ws.get_mut().set_nonblocking(true).map_err(|e| format!("设置非阻塞失败: {e}"))?;
    let ws = Arc::new(Mutex::new(ws));                
    let (out_tx, out_rx): (Sender<String>, Receiver<String>) = channel();
    log("NapCat 已连接");
    let ws_reader = ws.clone();
    let inner_r = inner.clone();
    let reader = thread::spawn(move || {
        loop {
            if inner_r.cancel.load(Ordering::Relaxed) {
                return;
            }
            let (would_block, msg, err) = {
                let mut guard = ws_reader.lock().unwrap();
                match guard.read() {
                    Ok(m) => (false, Some(m), String::new()),
                    Err(e) => {
                        let wb = matches!(&e, tungstenite::Error::Io(ioe)
                            if ioe.kind() == std::io::ErrorKind::WouldBlock);
                        (wb, None, e.to_string())
                    }
                }
            };
            if let Some(msg) = msg {
                match msg {
                    tungstenite::Message::Text(t) => {
                        handle_event(inner_r.clone(), &t, out_tx.clone());
                    }
                    tungstenite::Message::Close(_) => {
                        log("连接已关闭");
                        return;
                    }
                    _ => {}
                }
            } else if would_block {
                std::thread::sleep(std::time::Duration::from_millis(20));
            } else {
                log(&format!("WS 读取错误: {err}"));
                return;
            }
        }
    });
    let ws_writer = ws.clone();
    let writer = thread::spawn(move || {
        while let Ok(text) = out_rx.recv() {
            let mut guard = ws_writer.lock().unwrap();
            if let Err(e) = guard.write(tungstenite::Message::Text(text)) {
                log(&format!("发送失败: {e}"));
            }
            if let Err(e) = guard.flush() {
                log(&format!("发送 flush 失败: {e}"));
            }
        }
    });
    reader.join().ok();
    writer.join().ok();
    Ok(())
}
fn run_client(inner: Arc<BridgeInner>, qq: &Qq) -> Result<(), String> {
    let url = qq.ws_addr.clone();
    log(&format!("OneBot 正向 WS 客户端模式: 连接 {url}"));
    loop {
        match connect_client(inner.clone(), &url) {
            Ok(()) => log("连接断开，3 秒后重连…"),
            Err(e) => log(&format!("连接失败: {e}，3 秒后重连…")),
        }
        if inner.cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        for _ in 0..30 {
            if inner.cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}
fn connect_client(inner: Arc<BridgeInner>, url: &str) -> Result<(), String> {
    let (mut ws, _resp) = tungstenite::connect(url).map_err(|e| e.to_string())?;
    if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_nonblocking(true).map_err(|e| e.to_string())?;
    }
    let ws = Arc::new(Mutex::new(ws));
    let (out_tx, out_rx): (Sender<String>, Receiver<String>) = channel();
    log("已连接 OneBot");
    let ws_reader = ws.clone();
    let inner_r = inner.clone();
    let reader = thread::spawn(move || loop {
        let (would_block, msg, err) = {
            let mut guard = ws_reader.lock().unwrap();
            match guard.read() {
                Ok(m) => (false, Some(m), String::new()),
                Err(e) => {
                    let wb = matches!(&e, tungstenite::Error::Io(ioe)
                        if ioe.kind() == std::io::ErrorKind::WouldBlock);
                    (wb, None, e.to_string())
                }
            }
        };
        if let Some(msg) = msg {
            match msg {
                tungstenite::Message::Text(t) => handle_event(inner_r.clone(), &t, out_tx.clone()),
                tungstenite::Message::Close(_) => return,
                _ => {}
            }
        } else if would_block {
            std::thread::sleep(std::time::Duration::from_millis(20));
        } else {
            log(&format!("WS 读取错误: {err}"));
            return;
        }
    });
    let ws_writer = ws.clone();
    let writer = thread::spawn(move || {
        while let Ok(text) = out_rx.recv() {
            let mut guard = ws_writer.lock().unwrap();
            let _ = guard.write(tungstenite::Message::Text(text));
            let _ = guard.flush();
        }
    });
    reader.join().ok();
    writer.join().ok();
    Ok(())
}
fn handle_event(inner: Arc<BridgeInner>, raw: &str, out_tx: Sender<String>) {
    let ev: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return,
    };
    let post_type = ev["post_type"].as_str().unwrap_or("");
    match post_type {
        "message" => handle_message(inner, &ev, out_tx),
        "meta_event" | "notice" | "request" => {}
        _ => {}
    }
}
fn extract_text(ev: &Value) -> String {
    if let Some(arr) = ev["message"].as_array() {
        let mut out = String::new();
        for seg in arr {
            if seg["type"] == "text" {
                out.push_str(seg["data"]["text"].as_str().unwrap_or(""));
            }
        }
        if !out.trim().is_empty() {
            return out;
        }
    }
    ev["raw_message"].as_str().unwrap_or("").to_string()
}
fn at_me(ev: &Value, self_id: &str) -> bool {
    if let Some(arr) = ev["message"].as_array() {
        for seg in arr {
            if seg["type"] == "at" {
                let qq = seg["data"]["qq"].as_str().unwrap_or("");
                if qq == self_id || qq == "all" {
                    return true;
                }
            }
        }
    }
    ev["raw_message"].as_str().map(|r| r.contains(&format!("[CQ:at,qq={self_id}]"))).unwrap_or(false)
}
fn should_respond(inner: &BridgeInner, ev: &Value, self_id: &str) -> bool {
    let qq = &inner.qq_cfg;
    match ev["message_type"].as_str() {
        Some("group") => {
            let gid = ev["group_id"].as_i64().unwrap_or(0);
            if qq.groups.is_empty() || !qq.groups.contains(&gid) {
                return false;
            }
            let text = extract_text(ev);
            let has_trigger = qq.triggers.iter().any(|t| {
                !t.is_empty() && text.to_lowercase().contains(&t.to_lowercase())
            });
            if has_trigger {
                return true;
            }
            if qq.need_at {
                at_me(ev, self_id)
            } else if qq.triggers.is_empty() {
                true                   
            } else {
                at_me(ev, self_id)
            }
        }
        Some("private") => {
            let uid = ev["user_id"].as_i64().unwrap_or(0);
            if qq.users.is_empty() || !qq.users.contains(&uid) {
                return false;
            }
            true
        }
        _ => false,
    }
}
fn chat_id(ev: &Value) -> String {
    match ev["message_type"].as_str() {
        Some("group") => format!("qq_g{}", ev["group_id"].as_i64().unwrap_or(0)),
        _ => format!("qq_u{}", ev["user_id"].as_i64().unwrap_or(0)),
    }
}
fn handle_message(inner: Arc<BridgeInner>, ev: &Value, out_tx: Sender<String>) {
    let self_id = ev["self_id"].as_i64().unwrap_or(0).to_string();
    if !should_respond(&inner, ev, &self_id) {
        return;
    }
    let text = extract_text(ev);
    if text.trim().is_empty() {
        return;
    }
    if let Some(message_id) = ev["message_id"].as_i64() {
        let _ = out_tx.send(
            json!({
                "action": "set_msg_emoji_like",
                "params": { "message_id": message_id, "emoji_id": "76" }
            })
            .to_string(),
        );
    }
    let cid = chat_id(ev);
    let msg_type = ev["message_type"].as_str().unwrap_or("").to_string();
    let target_id = ev["group_id"].as_i64().unwrap_or(ev["user_id"].as_i64().unwrap_or(0));
    log(&format!("[{cid}] {text}"));
    if text.trim().starts_with("/compact") {
        let inner2 = inner.clone();
        let out_tx2 = out_tx.clone();
        let cid2 = cid.clone();
        let msg_type2 = msg_type.clone();
        thread::spawn(move || {
            let dir = inner2.cfg.sessions_dir();
            let reply = do_manual_compact(&inner2, &cid2, &dir);
            let action = match msg_type2.as_str() {
                "group" => json!({ "action": "send_group_msg", "params": { "group_id": target_id, "message": reply } }),
                _ => json!({ "action": "send_private_msg", "params": { "user_id": target_id, "message": reply } }),
            };
            let _ = out_tx2.send(action.to_string());
        });
        return;
    }
    if text.trim().starts_with("/qqautonew") {
        let n = text.split_whitespace().nth(1).and_then(|s| s.parse::<usize>().ok());
        match n {
            Some(n) => {
                let mut c = inner.cfg.clone();
                c.qq.auto_new = n;
                c.save();
                let reply = if n == 0 {
                    "已关闭自动滚动（每 N 条写记忆 + 开新对话）".to_string()
                } else {
                    format!("已设置：每 {n} 条消息自动写群记忆并开启新对话（重启桥接后生效，0 关闭）")
                };
                let action = match msg_type.as_str() {
                    "group" => json!({ "action": "send_group_msg", "params": { "group_id": target_id, "message": reply } }),
                    _ => json!({ "action": "send_private_msg", "params": { "user_id": target_id, "message": reply } }),
                };
                let _ = out_tx.send(action.to_string());
            }
            None => {
                let action = match msg_type.as_str() {
                    "group" => json!({ "action": "send_group_msg", "params": { "group_id": target_id, "message": "用法: /qqautonew <消息数>，如 /qqautonew 5（0 = 关闭）" } }),
                    _ => json!({ "action": "send_private_msg", "params": { "user_id": target_id, "message": "用法: /qqautonew <消息数>，如 /qqautonew 5（0 = 关闭）" } }),
                };
                let _ = out_tx.send(action.to_string());
            }
        }
        return;
    }
    if text.trim().starts_with("/stop") {
        if !is_admin(&inner, ev) {
            let action = match msg_type.as_str() {
                "group" => json!({ "action": "send_group_msg", "params": { "group_id": target_id, "message": "无权限：/stop 仅管理员可用" } }),
                _ => json!({ "action": "send_private_msg", "params": { "user_id": target_id, "message": "无权限：/stop 仅管理员可用" } }),
            };
            let _ = out_tx.send(action.to_string());
            return;
        }
        let mut stopped = 0usize;
        {
            let mut states = inner.states.lock().unwrap();
            for (cid2, st) in states.iter_mut() {
                st.cancel.store(true, Ordering::Relaxed);
                st.running = false;
                st.pending = None;
                stopped += 1;
                log(&format!("[{cid2}] /stop 强制终结"));
            }
        }
        log(&format!("[qq] /stop 强制终结所有对话（{stopped} 个会话）"));
        let reply = format!("已强制终结所有对话（{stopped} 个会话，正在处理的请求已中断）");
        let action = match msg_type.as_str() {
            "group" => json!({ "action": "send_group_msg", "params": { "group_id": target_id, "message": reply } }),
            _ => json!({ "action": "send_private_msg", "params": { "user_id": target_id, "message": reply } }),
        };
        let _ = out_tx.send(action.to_string());
        return;
    }
    if text.trim().starts_with("/new") {
        let inner2 = inner.clone();
        let out_tx2 = out_tx.clone();
        let cid2 = cid.clone();
        let msg_type2 = msg_type.clone();
        thread::spawn(move || {
            let dir = inner2.cfg.sessions_dir();
            let mem_dir = inner2.cfg.data_dir().join("memory");
            let reply = do_manual_new(&inner2, &cid2, &dir, &mem_dir);
            log(&format!("[{cid2}] /new 结果: {reply}"));
            let action = match msg_type2.as_str() {
                "group" => json!({ "action": "send_group_msg", "params": { "group_id": target_id, "message": reply } }),
                _ => json!({ "action": "send_private_msg", "params": { "user_id": target_id, "message": reply } }),
            };
            let _ = out_tx2.send(action.to_string());
        });
        return;
    }
    let allow_tools = is_admin(&inner, ev);
    let uid = ev["user_id"].as_i64().unwrap_or(0);
    let role_tag = if allow_tools {
        format!("管理员 {uid}")
    } else {
        format!("普通用户 {uid}")
    };
    let where_tag = if msg_type == "group" {
        format!("群{target_id}")
    } else {
        "私聊".to_string()
    };
    let agent_text = format!("【{role_tag} @{where_tag}】{text}");
    let mut states = inner.states.lock().unwrap();
    let st = states.entry(cid.clone()).or_insert(ChatState { running: false, pending: None, msg_since_roll: 0, cancel: Arc::new(AtomicBool::new(false)) });
    st.pending = Some((agent_text, allow_tools));
    if st.running {
        return;                                      
    }
    st.running = true;                               
    drop(states);                             
    let inner2 = inner.clone();
    let out_tx2 = out_tx.clone();
    thread::spawn(move || {
        loop {
            let mut states = inner2.states.lock().unwrap();
            let st = states.entry(cid.clone()).or_insert(ChatState { running: false, pending: None, msg_since_roll: 0, cancel: Arc::new(AtomicBool::new(false)) });
            if !st.running {
                return;                                
            }
            let (job, allow_tools) = match &st.pending {
                Some(p) => p.clone(),
                None => break,                            
            };
            st.pending = None;            
            let turn_cancel = st.cancel.clone();
            drop(states);                         
            let reply = run_chat_turn(&inner2, &cid, &job, allow_tools, out_tx2.clone(), turn_cancel);
            {
                let mem_inner = inner2.clone();
                let mem_cid = cid.clone();
                let mem_dir = inner2.cfg.data_dir().join("memory");
                let msgs = SessionStore::new(inner2.cfg.sessions_dir()).load(&mem_cid).messages;
                thread::spawn(move || remember_one_turn(&mem_inner, &mem_cid, &mem_dir, msgs));
            }
            if reply.is_empty() {
                continue;
            }
            let cleaned = qq_clean(&reply);
            if cleaned.trim().is_empty() {
                log(&format!("[qq] 回复过滤后为空，跳过发送: {reply:?}"));
                continue;
            }
            let action = match msg_type.as_str() {
                "group" => json!({
                    "action": "send_group_msg",
                    "params": { "group_id": target_id, "message": cleaned }
                }),
                _ => json!({
                    "action": "send_private_msg",
                    "params": { "user_id": target_id, "message": cleaned }
                }),
            };
            let _ = out_tx2.send(action.to_string());
            let auto_new = inner2.cfg.qq.auto_new;
            if auto_new > 0 {
                let mut hit = false;
                {
                    let mut states = inner2.states.lock().unwrap();
                    if let Some(st) = states.get_mut(&cid) {
                        st.msg_since_roll += 1;
                        hit = st.msg_since_roll >= auto_new;
                    }
                }
                if hit {
                    auto_new_rollover(&inner2, &cid, &msg_type, target_id, out_tx2.clone());
                    let mut states = inner2.states.lock().unwrap();
                    if let Some(st) = states.get_mut(&cid) {
                        st.msg_since_roll = 0;
                    }
                }
            }
        }
        let mut states = inner2.states.lock().unwrap();
        if let Some(st) = states.get_mut(&cid) {
            st.running = false;
        }
    });
}
fn qq_out_to_action(out: &QqOut) -> String {
    let (action, key, id) = if let Some(gid) = out.chat.strip_prefix("group:") {
        ("send_group_msg", "group_id", gid)
    } else if let Some(uid) = out.chat.strip_prefix("private:") {
        ("send_private_msg", "user_id", uid)
    } else {
        ("send_group_msg", "group_id", out.chat.as_str())
    };
    let id: i64 = id.parse().unwrap_or(0);
    json!({ "action": action, "params": { key: id, "message": qq_clean(&out.text) } }).to_string()
}
fn strip_role_tag(s: &str) -> Option<String> {
    let t = s.trim_start();
    for prefix in ["【管理员】", "【普通用户】"] {
        if let Some(after) = t.strip_prefix(prefix) {
            return Some(after.trim_start().to_string());
        }
    }
    let open = t.strip_prefix('【')?;
    let close = open.find('】')?;
    let body = &open[..close];
    let rest = body
        .strip_prefix("管理员")
        .or_else(|| body.strip_prefix("普通用户"))?;
    let rest = rest.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = rest[digits.len()..].trim_start();
    let where_ok = if let Some(g) = rest.strip_prefix("@群") {
        let gd: String = g.chars().take_while(|c| c.is_ascii_digit()).collect();
        !gd.is_empty()
    } else {
        rest.strip_prefix("@私聊").is_some()
    };
    if !where_ok {
        return None;
    }
    let after = &open[close + '】'.len_utf8()..];
    Some(after.trim_start().to_string())
}
fn qq_clean(text: &str) -> String {
    let mut out: Vec<String> = Vec::with_capacity(text.lines().count());
    for raw in text.lines() {
        let line = raw.trim();
        let line = strip_role_tag(line).unwrap_or_else(|| line.to_string());
        if line.starts_with("```") || line.starts_with("~~~") {
            continue;
        }
        let n = line.chars().count();
        if n >= 3 && line.chars().all(|c| c == '-' || c == '=' || c == '*') {
            continue;
        }
        let mut s = line.to_string();
        loop {
            let t = s.trim_start();
            let rest = if let Some(r) = t.strip_prefix('#') {
                Some(r.trim_start())
            } else if let Some(r) = t.strip_prefix('>') {
                Some(r.trim_start())
            } else if let Some(r) = t.strip_prefix('-') {
                Some(r.trim_start())
            } else if let Some(r) = t.strip_prefix('*') {
                Some(r.trim_start())
            } else if let Some(r) = t.strip_prefix('+') {
                Some(r.trim_start())
            } else if let Some(dot) = t.find('.') {
                let after = t[dot + 1..].chars().next();
                let after_ok = after.is_none_or(char::is_whitespace);
                if dot > 0 && after_ok && t[..dot].chars().all(|c| c.is_ascii_digit()) {
                    Some(t[dot + 1..].trim_start())
                } else {
                    None
                }
            } else {
                None
            };
            match rest {
                Some(r) => s = r.to_string(),
                None => break,
            }
        }
        out.push(strip_inline(&s));
    }
    out.join("\n")
}
fn strip_inline(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '[' || (c == '!' && i + 1 < chars.len() && chars[i + 1] == '[') {
            let from = if c == '!' { i + 1 } else { i };
            if let Some(close) = find_seq(&chars, from + 1, "]") {
                if close + 1 < chars.len() && chars[close + 1] == '(' {
                    if let Some(paren) = find_seq(&chars, close + 2, ")") {
                        let label: String = chars[from + 1..close].iter().collect();
                        let url: String = chars[close + 2..paren].iter().collect();
                        let label = strip_inline(&label);
                        if c == '!' {
                            out.push_str(&url);
                        } else if label.is_empty() {
                            out.push_str(&url);
                        } else if url.is_empty() || url == label {
                            out.push_str(&label);
                        } else {
                            out.push_str(&format!("{label}（{url}）"));
                        }
                        i = paren + 1;
                        continue;
                    }
                }
            }
            out.push(c);
            i += 1;
            continue;
        }
        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_seq(&chars, i + 2, "**") {
                out.push_str(&strip_inline(&chars[i + 2..end].iter().collect::<String>()));
                i = end + 2;
                continue;
            }
        }
        if c == '`' {
            if let Some(end) = find_seq(&chars, i + 1, "`") {
                out.push_str(&chars[i + 1..end].iter().collect::<String>());
                i = end + 1;
                continue;
            }
        }
        if c == '*' {
            let prev_digit = i > 0 && chars[i - 1].is_ascii_digit();
            let next_digit = i + 1 < chars.len() && chars[i + 1].is_ascii_digit();
            if !prev_digit && !next_digit {
                if let Some(end) = find_seq(&chars, i + 1, "*") {
                    out.push_str(&chars[i + 1..end].iter().collect::<String>());
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}
fn find_seq(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() || from + n.len() > chars.len() {
        return None;
    }
    let mut j = from;
    while j + n.len() <= chars.len() {
        if chars[j..j + n.len()] == n[..] {
            return Some(j);
        }
        j += 1;
    }
    None
}
fn do_manual_compact(inner: &BridgeInner, cid: &str, sessions_dir: &std::path::Path) -> String {
    let running = {
        let states = inner.states.lock().unwrap();
        states.get(cid).map(|st| st.running).unwrap_or(false)
    };
    if running {
        return "正在处理上一条消息，稍等片刻再压缩。".to_string();
    }
    let mut store = SessionStore::new(sessions_dir.to_path_buf());
    store.new_session(cid);
    let msgs = store.current().messages;
    if msgs.is_empty() {
        return "当前没有可压缩的上下文。".to_string();
    }
    let before = crate::compress::approx_total_tokens(&msgs);
    match crate::compress::compact(&inner.llm, &msgs, &inner.cancel) {
        Ok(new_hist) => {
            if let Err(e) = store.replace_current(&new_hist) {
                return format!("压缩落盘失败: {e}");
            }
            let after = crate::compress::approx_total_tokens(&new_hist);
            format!("上下文已压缩：{before} tok -> {after} tok")
        }
        Err(e) => format!("压缩失败: {e}"),
    }
}
fn append_memory_entry_to(mem_dir: &std::path::Path, cid: &str, text: &str) -> bool {
    if std::fs::create_dir_all(mem_dir).is_err() {
        return false;
    }
    let mem_file = mem_dir.join(format!("{cid}.md"));
    let entry = format!("\n## {}\n{}\n", crate::time::now_stamp(), text.trim());
    match std::fs::OpenOptions::new().create(true).append(true).open(&mem_file) {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(entry.as_bytes()).is_ok()
        }
        Err(_) => false,
    }
}
fn remember_one_turn(inner: &BridgeInner, cid: &str, mem_dir: &std::path::Path, msgs: Vec<Msg>) {
    let mut last_user: Option<String> = None;
    let mut last_asst: Option<String> = None;
    for m in &msgs {
        if m.role == "user" {
            let c = m.content.trim();
            if c.starts_with("【工具结果】") || c.is_empty() {
                continue;
            }
            let text = if c.starts_with('【') {
                strip_role_tag(c).unwrap_or_else(|| c.to_string())
            } else {
                c.to_string()
            };
            let text = text.trim();
            if !text.is_empty() {
                last_user = Some(text.to_string());
            }
        } else if m.role == "assistant" {
            let c = m.content.trim();
            if !c.is_empty() && !c.starts_with('{') && !c.contains("```") {
                last_asst = Some(c.to_string());
            }
        }
    }
    if last_user.is_none() && last_asst.is_none() {
        return;                        
    }
    let sys = "这是简单任务，不要思考、不要分析，直接输出答案。把这条 QQ 对话概括成一句话（30 字以内），记录发生了什么，用于长期记忆。直接输出概括本身，不要任何前缀、引号或格式。";
    let user_text = format!(
        "用户: {}\n助手: {}",
        last_user.as_deref().unwrap_or("（无）"),
        last_asst.as_deref().unwrap_or("（无）")
    );
    let req = crate::llm::ChatRequest {
        messages: vec![Msg::new("system", sys), Msg::new("user", user_text)],
        tools: None,
        max_tokens: inner.llm.max_tokens_for(crate::backend::TokenBudget::MemorySummary, inner.cfg.qq.max_tokens),
        stream: false,
    };
    let entry = match inner.llm.stream(&req, &inner.cancel, |_| {}) {
        Ok(r) => {
            let t = r.text.trim();
            if !t.is_empty() { t.to_string() } else { memory_fallback_entry(&last_user, &last_asst) }
        }
        Err(_) => memory_fallback_entry(&last_user, &last_asst),
    };
    if !entry.is_empty() && append_memory_entry_to(mem_dir, cid, &entry) {
        log(&format!("[{cid}] 自动记忆: {entry}"));
    }
}
fn do_manual_new(inner: &BridgeInner, cid: &str, sessions_dir: &std::path::Path, mem_dir: &std::path::Path) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let running = {
            let states = inner.states.lock().unwrap();
            states.get(cid).map(|st| st.running).unwrap_or(false)
        };
        if !running {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return "正在处理上一条消息（等待超时），稍后再试。".to_string();
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let mut store = SessionStore::new(sessions_dir.to_path_buf());
    store.new_session(cid);
    let msgs = store.current().messages;
    if msgs.is_empty() {
        return "当前已经是新对话了。".to_string();
    }
    match summarize_conversation(inner, &msgs) {
        Ok(text) if !text.trim().is_empty() => {
            let entry = format!("（/new 归档记忆，共 {} 条消息）\n{}", msgs.len(), text.trim());
            if append_memory_entry_to(mem_dir, cid, &entry) {
                log(&format!("[{cid}] /new 强制更新记忆已写入"));
            }
        }
        _ => log(&format!("[{cid}] /new 记忆总结失败，跳过写记忆")),
    }
    let archive_id = {
        let prefix = format!("{cid}_");
        let mut max = 0usize;
        for id in store.list() {
            if let Some(rest) = id.strip_prefix(&prefix) {
                if let Ok(n) = rest.parse::<usize>() {
                    max = max.max(n);
                }
            }
        }
        format!("{prefix}{}", max + 1)
    };
    let mut arch = SessionStore::new(sessions_dir.to_path_buf());
    arch.new_session(&archive_id);
    for m in &msgs {
        arch.append(m);
    }
    if let Err(e) = store.replace_current(&[]) {
        return format!("开启新对话失败: {e}");
    }
    format!("已开启新对话（原 {} 条消息已归档为 {archive_id}）", msgs.len())
}
fn auto_new_rollover(inner: &BridgeInner, cid: &str, msg_type: &str, target_id: i64, out_tx: Sender<String>) {
    let dir = inner.cfg.sessions_dir();
    let store = SessionStore::new(dir.clone());
    let msgs = store.current().messages;
    let mem_dir = inner.cfg.data_dir().join("memory");
    match summarize_conversation(inner, &msgs) {
        Ok(text) if !text.trim().is_empty() => {
            let entry = format!("（自动记录，共 {} 条消息）\n{}", msgs.len(), text.trim());
            if append_memory_entry_to(&mem_dir, cid, &entry) {
                log(&format!("[{cid}] 自动滚动: 记忆已写入 {}", mem_dir.join(format!("{cid}.md")).display()));
            }
        }
        _ => log(&format!("[{cid}] 自动滚动: 对话总结失败，跳过写记忆")),
    }
    let reply = do_manual_new(inner, cid, &dir, &mem_dir);
    let text = format!("已自动记住这段对话要点并开启新对话（{reply}）");
    let action = match msg_type {
        "group" => json!({ "action": "send_group_msg", "params": { "group_id": target_id, "message": qq_clean(&text) } }),
        _ => json!({ "action": "send_private_msg", "params": { "user_id": target_id, "message": qq_clean(&text) } }),
    };
    let _ = out_tx.send(action.to_string());
}
fn summarize_conversation(inner: &BridgeInner, msgs: &[Msg]) -> Result<String, String> {
    let mut dialog: Vec<String> = Vec::new();
    let mut last_user: Option<String> = None;
    let mut last_asst: Option<String> = None;
    for m in msgs {
        if m.role != "user" && m.role != "assistant" {
            continue;
        }
        let c = m.content.trim();
        if c.is_empty() || c.starts_with("【工具结果】") {
            continue;
        }
        let text = if c.starts_with('【') {
            strip_role_tag(c).unwrap_or_else(|| c.to_string())
        } else {
            c.to_string()
        };
        if m.role == "user" {
            last_user = Some(text.clone());
        } else {
            last_asst = Some(text);
        }
        dialog.push(format!("{}: {c}", if m.role == "user" { "用户" } else { "助手" }));
    }
    if dialog.is_empty() {
        return Ok(String::new());
    }
    let mut dialog_text = dialog.join("\n");
    if dialog_text.chars().count() > 6000 {
        let chars: Vec<char> = dialog_text.chars().collect();
        dialog_text = chars[..6000].iter().collect();
    }
    let sys = "你是记忆整理助手。这是简单任务，不要思考、不要分析，直接输出。总结以下 QQ 群对话中值得长期记住的信息：群成员身份与权限、话题与偏好、约定与决定、重要事实。用中文输出简洁要点列表（每条一行，- 开头），不要复述对话内容，不要寒暄，不要输出标题。";
    let req = crate::llm::ChatRequest {
        messages: vec![Msg::new("system", sys), Msg::new("user", dialog_text)],
        tools: None,
        max_tokens: inner.llm.max_tokens_for(crate::backend::TokenBudget::MemorySummary, inner.cfg.qq.max_tokens),
        stream: false,
    };
    match inner.llm.stream(&req, &inner.cancel, |_| {}) {
        Ok(r) if !r.text.trim().is_empty() => Ok(r.text),
        _ => Ok(memory_fallback_entry(&last_user, &last_asst)),
    }
}
fn memory_fallback_entry(last_user: &Option<String>, last_asst: &Option<String>) -> String {
    let s = match (last_user.as_deref(), last_asst.as_deref()) {
        (Some(u), Some(a)) => format!("用户: {u} / 助手: {a}"),
        (Some(u), None) => format!("用户: {u}"),
        (None, Some(a)) => format!("助手: {a}"),
        (None, None) => return String::new(),
    };
    s.replace('\n', " ").chars().take(200).collect()
}
fn run_chat_turn(
    inner: &BridgeInner,
    cid: &str,
    text: &str,
    allow_tools: bool,
    out_tx: Sender<String>,
    cancel: Arc<AtomicBool>,
) -> String {
    let (agent_qq_tx, agent_qq_rx) = channel::<QqOut>();
    let out_tx2 = out_tx.clone();
    let qq_sent = Arc::new(AtomicBool::new(false));
    let qq_bridge_sent = Arc::new(AtomicBool::new(false));
    let bridge_flag = qq_bridge_sent.clone();
    thread::spawn(move || {
        while let Ok(out) = agent_qq_rx.recv() {
            if bridge_flag.swap(true, Ordering::Relaxed) {
                log(&format!("[qq] 同一轮重复 qq_send 已丢弃: {}", out.text));
                continue;
            }
            let action = qq_out_to_action(&out);
            log(&format!("[qq] qq_send 投递 action: {action}"));
            let _ = out_tx2.send(action);
        }
    });
    let llm = inner.llm.clone();
    let mut agent = Agent::for_session(
        inner.cfg.clone(),
        llm,
        cid,
        Some(agent_qq_tx),
        true,
        !allow_tools,
        cancel.clone(),
    );
    let qq_sent2 = qq_sent.clone();
    let reply = match agent.run_turn(text, &mut |ev| {
        if let crate::agent::AgentEvent::ToolRun { op, .. } = &ev {
            if op == "qq_send" {
                qq_sent2.store(true, Ordering::Relaxed);
            }
        }
        if let crate::agent::AgentEvent::Error(e) = ev {
            log(&format!("[{cid}] 错误: {e}"));
        }
    }) {
        Ok(t) => t,
        Err(_) if cancel.load(Ordering::Relaxed) => String::new(),
        Err(e) => format!("出错了: {e}"),
    };
    let reply = reply.trim();
    if reply.is_empty() || qq_sent.load(Ordering::Relaxed) {
        String::new()
    } else if reply_has_tool_trace(reply) {
        log(&format!("[{cid}] 丢弃疑似工具调用的回复: {reply}"));
        String::new()
    } else {
        reply.to_string()
    }
}
fn reply_has_tool_trace(reply: &str) -> bool {
    let lower = reply.to_lowercase();
    if lower.contains("```tool") || lower.contains("```json") {
        return true;
    }
    if reply.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("tool ") || t.starts_with("tool{") || t.starts_with("tool:{")
    }) {
        return true;
    }
    if crate::agent::bare_tool_op(reply).is_some() {
        return true;
    }
    if reply.lines().any(|l| {
        l.trim_start()
            .strip_prefix("```")
            .is_some_and(|rest| {
                crate::tools::KNOWN_OPS
                    .iter()
                    .any(|op| rest.trim_start().starts_with(op))
            })
    }) {
        return true;
    }
    if reply.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("{\"op\":\"")
            && crate::tools::KNOWN_OPS
                .iter()
                .any(|op| t.contains(&format!("\"{op}\"")))
    }) {
        return true;
    }
    reply.lines().any(|l| {
        let t = l.trim_start();
        crate::tools::KNOWN_OPS
            .iter()
            .any(|op| t.strip_prefix(op).is_some_and(|rest| rest.trim_start().starts_with('{')))
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    static TEST_DIR_SEQ: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    #[test]
    fn qq_clean_strips_md_marks() {
        let src = "# 标题\n> 引用\n- 列表1\n* 列表2\n1. 步骤\n---\n```\ncode\n```\n结尾";
        assert_eq!(
            qq_clean(src),
            "标题\n引用\n列表1\n列表2\n步骤\ncode\n结尾"
        );
        assert_eq!(qq_clean("**粗体** 和 *斜体* 与 `code`"), "粗体 和 斜体 与 code");
        assert_eq!(qq_clean("[链接](https://example.com/a?b=1)"), "链接（https://example.com/a?b=1）");
        assert_eq!(qq_clean("![图](http://x.io/1.png)"), "http://x.io/1.png");
    }
    #[test]
    fn qq_clean_keeps_plain_text() {
        assert_eq!(qq_clean("2*3=6，买 3*4 个"), "2*3=6，买 3*4 个");
        assert_eq!(qq_clean("价格 * 优惠后 50"), "价格 * 优惠后 50");
        assert_eq!(qq_clean("你好，这是普通消息。"), "你好，这是普通消息。");
        assert_eq!(qq_clean("文件 config_file.yaml"), "文件 config_file.yaml");
        assert_eq!(qq_clean("10. 第十条"), "第十条");
        assert_eq!(qq_clean("3.14 是圆周率"), "3.14 是圆周率");
        assert_eq!(qq_clean("10.5元"), "10.5元");
        assert_eq!(qq_clean("版本 1.2.3"), "版本 1.2.3");
    }
    #[test]
    fn qq_clean_nested_inline() {
        assert_eq!(qq_clean("**[粗](http://a.b)** 中的代码 `x`"), "粗（http://a.b） 中的代码 x");
    }
    #[test]
    fn qq_out_to_action_json_shape() {
        let out = QqOut { chat: "group:728563593".into(), text: "在呀！老大有啥事吗？".into() };
        let v: serde_json::Value = serde_json::from_str(&qq_out_to_action(&out)).unwrap();
        assert_eq!(v["action"], "send_group_msg");
        assert_eq!(v["params"]["group_id"], 728563593);
        assert_eq!(v["params"]["message"], "在呀！老大有啥事吗？");
        let out2 = QqOut { chat: "private:3160168215".into(), text: "hi".into() };
        let v2: serde_json::Value = serde_json::from_str(&qq_out_to_action(&out2)).unwrap();
        assert_eq!(v2["action"], "send_private_msg");
        assert_eq!(v2["params"]["user_id"], 3160168215i64);
    }
    #[test]
    fn qq_clean_strips_role_tag() {
        assert_eq!(qq_clean("【管理员 3160168215 @群728563593】你是管理员"), "你是管理员");
        assert_eq!(qq_clean("【普通用户 123 @私聊】你好"), "你好");
        assert_eq!(qq_clean("【管理员 1 @群2】\n正文"), "\n正文");
        assert_eq!(qq_clean("【管理员 1 @群2】正文"), "正文");
        assert_eq!(qq_clean("你好\n【管理员 1 @群2】"), "你好\n");
        assert_eq!(qq_clean("【重要】通知"), "【重要】通知");
        assert_eq!(qq_clean("【路人 1 @群2】"), "【路人 1 @群2】");
        assert_eq!(qq_clean("【管理员】在呀~ 有什么事吗？"), "在呀~ 有什么事吗？");
        assert_eq!(qq_clean("【管理员】"), "");
        assert_eq!(qq_clean("【普通用户】你好"), "你好");
    }
    #[test]
    fn strip_role_tag_matches_exact_format() {
        assert_eq!(strip_role_tag("【管理员 3160168215 @群728563593】hi"), Some("hi".into()));
        assert_eq!(strip_role_tag("【普通用户 7 @私聊】hi"), Some("hi".into()));
        assert_eq!(strip_role_tag("【管理员 1 @群2】"), Some("".into()));
        assert_eq!(strip_role_tag("【重要】通知"), None);
        assert_eq!(strip_role_tag("【管理员 x @群2】"), None);
        assert_eq!(strip_role_tag("【管理员 1 群2】"), None);
    }
    #[test]
    fn reply_tool_trace_detection() {
        assert!(reply_has_tool_trace("```tool {\"op\":\"list_tools\",\"args\":{}}\n```"));
        assert!(reply_has_tool_trace("tool {\"op\":\"is_admin\",\"args\":{\"qq\":1}}"));
        assert!(reply_has_tool_trace("tool{"));
        assert!(reply_has_tool_trace("```json\n{\"op\":\"stats\"}\n```"));
        assert!(reply_has_tool_trace("list_tools"));
        assert!(reply_has_tool_trace("is_admin {\"qq\":\"3160168215\"}"));
        assert!(reply_has_tool_trace("readline {\"path\":\"a\"}\n然后告诉你内容"));
        assert!(reply_has_tool_trace("{\"op\":\"qq_send\",\"args\":{\"chat\":\"group:1\",\"text\":\"你好\"}}"));
        assert!(reply_has_tool_trace("{\"op\":\"qq_send\",\"args\":{\"chat\":\"group:1\",\"text\":\"你好\"}"));
        assert!(!reply_has_tool_trace("{\"op\":\"something_unknown\",\"args\":{}}"));
        assert!(!reply_has_tool_trace("is_admin 工具可以判断管理员"));
        assert!(!reply_has_tool_trace("你是管理员"));
        assert!(!reply_has_tool_trace("你是管理员"));
        assert!(!reply_has_tool_trace("东北雨姐？不，是马斯克😄"));
        assert!(!reply_has_tool_trace("搜索马斯克的结果如下..."));
    }
    fn qq_cfg(groups: Vec<i64>, users: Vec<i64>) -> Qq {
        Qq {
            ws_mode: "server".into(),
            ws_addr: "127.0.0.1:6701".into(),
            ws_path: "/onebot/v11/ws".into(),
            groups,
            users,
            admins: Vec::new(),
            triggers: vec!["yjlcoder".into()],
            need_at: true,
            max_tokens: 1024,
            auto_new: 0,
        }
    }
    fn bridge_with(qq: Qq) -> Arc<BridgeInner> {
        let cfg = Config {
            qq: qq.clone(),
            data_root: Some(tmp_sessions_dir("bridge_data")),
            ..Default::default()
        };
        Arc::new(BridgeInner {
            cfg,
            llm: Llm::mock(),
            qq_cfg: qq,
            states: Mutex::new(HashMap::new()),
            cancel: Arc::new(AtomicBool::new(false)),
        })
    }
    fn tmp_sessions_dir(tag: &str) -> std::path::PathBuf {
        let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "yjlcoder_compact_{tag}_{}_{}",
            std::process::id(),
            seq
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }
    #[test]
    fn summarize_skips_tool_results() {
        let inner = bridge_with(qq_cfg(vec![123], vec![456]));
        let msgs = vec![
            Msg::new("system", "sys"),
            Msg::new("user", "【管理员 1 @群2】今天聊什么"),
            Msg::new("assistant", "聊 Rust"),
            Msg::new("user", "【工具结果】readline 返回: xxx"),
            Msg::new("tool", "tool 消息"),
            Msg::new("user", "   "),
        ];
        let s = summarize_conversation(&inner, &msgs).unwrap();
        assert!(!s.is_empty(), "mock 应返回固定文本");
        assert!(!s.contains("【工具结果】"), "工具结果不应进入总结: {s}");
        let empty = summarize_conversation(&inner, &[Msg::new("tool", "x")]).unwrap();
        assert!(empty.is_empty(), "无可总结内容应返回空: {empty}");
    }
    #[test]
    fn now_stamp_formats() {
        let s = crate::time::now_stamp();
        assert_eq!(s.len(), 19, "格式应为 YYYY-MM-DD HH:MM:SS: {s}");
        assert!(s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-' && s.as_bytes()[10] == b' ', "分隔符: {s}");
        assert!(s.contains(':'), "应含时分: {s}");
    }
    #[test]
    fn manual_compact_replaces_history_with_summary() {
        let sessions = tmp_sessions_dir("replace");
        let inner = bridge_with(qq_cfg(vec![123], vec![456]));
        let mut store = SessionStore::new(sessions.clone());
        store.new_session("qq_g456");
        for i in 0..6 {
            store.append(&Msg::new("user", format!("第 {i} 条旧消息")));
        }
        let reply = do_manual_compact(&inner, "qq_g456", &sessions);
        assert!(reply.contains("已压缩"), "应返回压缩结果: {reply}");
        let mut check = SessionStore::new(sessions);
        check.new_session("qq_g456");
        let msgs = check.current().messages;
        let last = msgs.last().expect("压缩后应有摘要消息");
        assert_eq!(last.role, "user");
        assert!(crate::compress::is_summary_message(&last.content), "末条应为摘要");
        assert_eq!(msgs.len(), 7, "应为 6 条保留 + 1 条摘要: {}", msgs.len());
    }
    #[test]
    fn manual_compact_skips_when_running() {
        let sessions = tmp_sessions_dir("running");
        let inner = bridge_with(qq_cfg(vec![123], vec![456]));
        inner.states.lock().unwrap().insert("qq_g123".into(), ChatState { running: true, pending: None, msg_since_roll: 0, cancel: Arc::new(AtomicBool::new(false)) });
        let reply = do_manual_compact(&inner, "qq_g123", &sessions);
        assert!(reply.contains("稍等"), "生成中应提示稍等: {reply}");
    }
    #[test]
    fn manual_new_archives_and_resets() {
        let sessions = tmp_sessions_dir("new_archive");
        let mem_dir = tmp_sessions_dir("new_archive_mem");
        let inner = bridge_with(qq_cfg(vec![123], vec![456]));
        let mut store = SessionStore::new(sessions.clone());
        store.new_session("qq_g456");
        for i in 0..3 {
            store.append(&Msg::new("user", format!("旧消息 {i}")));
        }
        let reply = do_manual_new(&inner, "qq_g456", &sessions, &mem_dir);
        assert!(reply.contains("已开启新对话"), "应返回开启结果: {reply}");
        assert!(reply.contains("qq_g456_1"), "应提到归档名: {reply}");
        let mut cur = SessionStore::new(sessions.clone());
        cur.new_session("qq_g456");
        assert!(cur.current().messages.is_empty(), "当前会话应为空");
        let mut arch = SessionStore::new(sessions);
        arch.new_session("qq_g456_1");
        let msgs = arch.current().messages;
        assert_eq!(msgs.len(), 3, "归档应保留 3 条消息");
        assert_eq!(msgs[0].content, "旧消息 0");
        let mem_content = std::fs::read_to_string(mem_dir.join("qq_g456.md")).unwrap();
        assert!(mem_content.contains("## "), "记忆应有时间戳节: {mem_content}");
        assert!(mem_content.contains("（/new 归档记忆，共 3 条消息）"), "{mem_content}");
    }
    #[test]
    fn manual_new_waits_for_running() {
        let sessions = tmp_sessions_dir("new_running");
        let mem_dir = tmp_sessions_dir("new_running_mem");
        let inner = bridge_with(qq_cfg(vec![123], vec![456]));
        let mut store = SessionStore::new(sessions.clone());
        store.new_session("qq_g123");
        for i in 0..3 {
            store.append(&Msg::new("user", format!("旧消息 {i}")));
        }
        inner.states.lock().unwrap().insert("qq_g123".into(), ChatState { running: true, pending: None, msg_since_roll: 0, cancel: Arc::new(AtomicBool::new(false)) });
        let inner2 = inner.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            inner2.states.lock().unwrap().get_mut("qq_g123").unwrap().running = false;
        });
        let reply = do_manual_new(&inner, "qq_g123", &sessions, &mem_dir);
        t.join().unwrap();
        assert!(reply.contains("已开启新对话"), "等待后应完成归档重置: {reply}");
        let mut cur = SessionStore::new(sessions);
        cur.new_session("qq_g123");
        assert!(cur.current().messages.is_empty(), "等待后当前会话应已重置");
        let _ = std::fs::remove_dir_all(&mem_dir);
    }
    #[test]
    fn remember_one_turn_writes_single_line() {
        let mem_dir = tmp_sessions_dir("remember");
        let sessions = tmp_sessions_dir("remember_sess");
        let inner = bridge_with(qq_cfg(vec![123], vec![456]));
        let mut store = SessionStore::new(sessions.clone());
        store.new_session("qq_g456");
        store.append(&Msg::new("user", "搜索马斯克"));
        store.append(&Msg::new("assistant", "搜到了，马斯克买了 twitter"));
        let msgs = store.current().messages;
        remember_one_turn(&inner, "qq_g456", &mem_dir, msgs);
        let content = std::fs::read_to_string(mem_dir.join("qq_g456.md")).unwrap();
        assert!(content.contains("## "), "应有时间戳节: {content}");
        assert!(content.contains("mock"), "应含 mock 摘要: {content}");
        store.append(&Msg::new("assistant", "{\"op\":\"qq_send\",\"args\":{}}"));
        store.append(&Msg::new("user", "【工具结果】qq_send 返回: ok"));
        let msgs2 = store.current().messages;
        remember_one_turn(&inner, "qq_g456", &mem_dir, msgs2);
        let content2 = std::fs::read_to_string(mem_dir.join("qq_g456.md")).unwrap();
        assert_eq!(content2.matches("## ").count(), 2, "两次对话各写一条: {content2}");
        let _ = std::fs::remove_dir_all(&mem_dir);
    }
    #[test]
    fn memory_fallback_entry_one_line() {
        assert_eq!(memory_fallback_entry(&Some("搜索马斯克".into()), &None), "用户: 搜索马斯克");
        assert_eq!(
            memory_fallback_entry(&Some("在吗".into()), &Some("在呀".into())),
            "用户: 在吗 / 助手: 在呀"
        );
        assert_eq!(memory_fallback_entry(&None, &Some("好的".into())), "助手: 好的");
        assert_eq!(
            memory_fallback_entry(&Some("在吗".into()), &Some("在呀\n有事吗".into())),
            "用户: 在吗 / 助手: 在呀 有事吗"
        );
        assert!(memory_fallback_entry(&None, &None).is_empty());
        let long = "x".repeat(500);
        assert_eq!(memory_fallback_entry(&Some(long), &None).chars().count(), 200);
    }
    #[test]
    fn remember_one_turn_strips_role_tag() {
        let mem_dir = tmp_sessions_dir("remember_role");
        let sessions = tmp_sessions_dir("remember_role_sess");
        let inner = bridge_with(qq_cfg(vec![123], vec![456]));
        let mut store = SessionStore::new(sessions.clone());
        store.new_session("qq_g456");
        store.append(&Msg::new("user", "【管理员 3160168215 @群728563593】搜索马斯克"));
        store.append(&Msg::new("assistant", "搜到了"));
        let msgs = store.current().messages;
        remember_one_turn(&inner, "qq_g456", &mem_dir, msgs);
        let content = std::fs::read_to_string(mem_dir.join("qq_g456.md")).unwrap();
        assert!(content.contains("## "), "应写出记忆: {content}");
        let _ = std::fs::remove_dir_all(&mem_dir);
    }
    fn group_ev(gid: i64, text: &str) -> Value {
        serde_json::json!({
            "message_type": "group",
            "group_id": gid,
            "self_id": 10001,
            "message": [{"type": "text", "data": {"text": text}}]
        })
    }
    fn private_ev(uid: i64, text: &str) -> Value {
        serde_json::json!({
            "message_type": "private",
            "user_id": uid,
            "self_id": 10001,
            "message": [{"type": "text", "data": {"text": text}}]
        })
    }
    #[test]
    fn empty_allowlist_denies_everything() {
        let inner = bridge_with(qq_cfg(vec![], vec![]));
        assert!(!should_respond(&inner, &group_ev(123, "yjlcoder 你好"), "10001"));
        assert!(!should_respond(&inner, &group_ev(123, "@bot"), "10001"));
        assert!(!should_respond(&inner, &private_ev(456, "你好"), "10001"));
    }
    #[test]
    fn allowlisted_group_and_user_respond() {
        let inner = bridge_with(qq_cfg(vec![123], vec![456]));
        assert!(should_respond(&inner, &group_ev(123, "yjlcoder 你好"), "10001"));
        assert!(!should_respond(&inner, &group_ev(999, "yjlcoder 你好"), "10001"));
        assert!(should_respond(&inner, &private_ev(456, "你好"), "10001"));
        assert!(!should_respond(&inner, &private_ev(789, "你好"), "10001"));
    }
    #[test]
    fn single_message_gets_reply() {
        let inner = bridge_with(qq_cfg(vec![123], vec![456]));
        let (tx, rx) = channel::<String>();
        handle_message(inner.clone(), &group_ev(123, "yjlcoder 你好"), tx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut got: Option<String> = None;
        while std::time::Instant::now() < deadline {
            if let Ok(s) = rx.try_recv() {
                got = Some(s);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let s = got.expect("第一条消息应触发回复 action");
        assert!(s.contains("send_group_msg"), "action: {s}");
        assert!(s.contains("\"group_id\":123"), "action: {s}");
        assert!(s.contains("mock 模式完成"), "action: {s}");
        let _ = std::fs::remove_file(inner.cfg.sessions_dir().join("qq_g123.jsonl"));
    }
    #[test]
    fn reacted_emoji_before_reply() {
        let inner = bridge_with(qq_cfg(vec![123], vec![456]));
        let (tx, rx) = channel::<String>();
        let mut ev = group_ev(123, "yjlcoder 你好");
        ev["message_id"] = json!(42);
        handle_message(inner.clone(), &ev, tx);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut first: Option<String> = None;
        let mut saw_reply = false;
        while std::time::Instant::now() < deadline {
            if let Ok(s) = rx.try_recv() {
                if first.is_none() {
                    first = Some(s.clone());
                }
                if s.contains("send_group_msg") {
                    saw_reply = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let f = first.expect("应首先收到表情 action");
        assert!(f.contains("set_msg_emoji_like"), "first action: {f}");
        assert!(f.contains("\"message_id\":42"), "first action: {f}");
        assert!(f.contains("\"emoji_id\":\"76\""), "first action: {f}");
        assert!(saw_reply, "随后应有回复 action");
        let _ = std::fs::remove_file(inner.cfg.sessions_dir().join("qq_g123.jsonl"));
    }
    #[test]
    fn pending_keeps_latest_message() {
        let inner = bridge_with(qq_cfg(vec![123], vec![456]));
        let cid = "qq_g123".to_string();
        {
            let mut states = inner.states.lock().unwrap();
            states.insert(cid.clone(), ChatState { running: true, pending: Some(("第一条".into(), true)), msg_since_roll: 0, cancel: Arc::new(AtomicBool::new(false)) });
        }
        let (tx, _rx) = channel::<String>();
        handle_message(inner.clone(), &group_ev(123, "yjlcoder 第二条"), tx);
        let guard = inner.states.lock().unwrap();
        let st = guard.get(&cid).unwrap();
        assert_eq!(st.pending.as_ref().map(|p| p.0.as_str()), Some("【普通用户 0 @群123】yjlcoder 第二条"));
        assert!(st.running, "生成中不应另起 worker");
    }
    #[test]
    fn stop_command_terminates_all_chats() {
        let mut qq = qq_cfg(vec![123], vec![456]);
        qq.admins = vec![123];
        let inner = bridge_with(qq);
        {
            let mut states = inner.states.lock().unwrap();
            states.insert("qq_g123".into(), ChatState { running: true, pending: Some(("旧消息".into(), true)), msg_since_roll: 0, cancel: Arc::new(AtomicBool::new(false)) });
            states.insert("qq_u456".into(), ChatState { running: true, pending: None, msg_since_roll: 0, cancel: Arc::new(AtomicBool::new(false)) });
        }
        let (tx, rx) = channel::<String>();
        let mut ev = group_ev(123, "/stop");
        ev["message"] = json!([
            {"type": "text", "data": {"text": "/stop"}},
            {"type": "at", "data": {"qq": "10001"}}
        ]);
        ev["user_id"] = json!(123);       
        handle_message(inner.clone(), &ev, tx);
        let reply = loop {
            let s = rx.recv_timeout(std::time::Duration::from_secs(2)).expect("应有回复");
            if s.contains("send_group_msg") {
                break s;
            }
        };
        assert!(reply.contains("已强制终结所有对话"), "reply: {reply}");
        let states = inner.states.lock().unwrap();
        assert_eq!(states.len(), 2);
        for st in states.values() {
            assert!(!st.running, "running 应被清除");
            assert!(st.pending.is_none(), "pending 应被清空");
            assert!(st.cancel.load(Ordering::Relaxed), "取消标志应置位");
        }
    }
    #[test]
    fn stop_command_requires_admin() {
        let inner = bridge_with(qq_cfg(vec![123], vec![456]));
        let (tx, rx) = channel::<String>();
        let mut ev = group_ev(123, "/stop");
        ev["message"] = json!([
            {"type": "text", "data": {"text": "/stop"}},
            {"type": "at", "data": {"qq": "10001"}}
        ]);
        ev["user_id"] = json!(999);        
        handle_message(inner.clone(), &ev, tx);
        let reply = loop {
            let s = rx.recv_timeout(std::time::Duration::from_secs(2)).expect("应有回复");
            if s.contains("send_group_msg") {
                break s;
            }
        };
        assert!(reply.contains("无权限"), "reply: {reply}");
        assert!(inner.states.lock().unwrap().is_empty());
    }
    #[test]
    fn admin_only_can_operate_computer() {
        let mut cfg = qq_cfg(vec![123], vec![456]);
        cfg.admins = vec![456];
        let inner = bridge_with(cfg);
        let mut group_admin = group_ev(123, "yjlcoder 帮我看看服务器负载");
        group_admin["user_id"] = json!(456);
        let mut group_user = group_ev(123, "yjlcoder 帮我看看服务器负载");
        group_user["user_id"] = json!(789);
        assert!(should_respond(&inner, &group_admin, "10001"));
        assert!(should_respond(&inner, &group_user, "10001"));
        assert!(is_admin(&inner, &group_admin));
        assert!(!is_admin(&inner, &group_user));
        let priv_admin = private_ev(456, "你好");
        let priv_user = private_ev(789, "你好");
        assert!(is_admin(&inner, &priv_admin));
        assert!(!is_admin(&inner, &priv_user));
    }
}
#[cfg(test)]
mod debug_tests {
    use super::*;
    #[test]
    fn debug_qq_clean_reply() {
        let r = qq_clean("【管理员】在呀~ 有什么事吗？");
        println!("qq_clean 输出: {:?} (len={})", r, r.len());
        assert!(!r.is_empty());
    }
}
