use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};
use serde_json::json;
use yjlcoder::agent::Agent;
use yjlcoder::config::Config;
use yjlcoder::llm::{Llm, Msg};
use yjlcoder::session::SessionStore;
static HOME_LOCK: Mutex<()> = Mutex::new(());
fn tmp_home(name: &str) -> (std::path::PathBuf, MutexGuard<'static, ()>) {
    let guard = HOME_LOCK.lock().unwrap();
    let d = std::env::temp_dir().join(format!("yjlcoder_e2e_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::env::set_var("YJLCODER_HOME", &d);
    (d, guard)
}
fn stats(home: &std::path::Path) -> usize {
    yjlcoder::session::session_stats(&home.join("sessions/main.jsonl")).0
}
fn read_full_request(stream: &mut std::net::TcpStream) -> String {
    use std::io::Read;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let mut header_end = None;
    while buf.len() < 64 * 1024 {
        let n = stream.read(&mut tmp).unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if header_end.is_none() {
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                header_end = Some(pos + 4);
            }
        }
        let Some(end) = header_end else { continue };
        let headers = String::from_utf8_lossy(&buf[..end]);
        let clen: usize = headers
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse().ok())
            })
            .unwrap_or(0);
        if buf.len() >= end + clen {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}
#[test]
fn mock_agent_full_turn_with_tool_loop() {
    let (home, _home_guard) = tmp_home("turn");
    let cfg = Config::default();
    let llm = Llm::mock();
    let cancel = Arc::new(AtomicBool::new(false));
    let store = SessionStore::new(home.join("sessions"));
    let mut agent = Agent::with_store(cfg.clone(), llm, store, None, false, false, cancel);
    let mut deltas = 0usize;
    let mut tools = 0usize;
    let reply = agent
        .run_turn("你好", &mut |ev| match ev {
            yjlcoder::agent::AgentEvent::Delta(_) => deltas += 1,
            yjlcoder::agent::AgentEvent::ToolRun { .. } => tools += 1,
            _ => {}
        })
        .unwrap();
    assert_eq!(tools, 1, "应执行一次工具调用");
    assert!(deltas > 0, "应有流式增量");
    assert!(reply.contains("mock 模式完成"), "最终回复: {reply}");
    assert_eq!(stats(&home), 4, "会话应有 4 条消息");
    let trace_path = std::fs::read_dir(home.join("trace"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .unwrap();
    let trace = std::fs::read_to_string(trace_path).unwrap();
    let lines: Vec<&str> = trace.lines().collect();
    let deltas = lines
        .iter()
        .filter(|line| line.contains(r#""kind":"delta""#))
        .count();
    assert_eq!(deltas, 2, "两次模型请求应各记录一条合并正文");
    assert!(lines.len() <= 6, "trace 不应逐 token 刷屏: {} 行", lines.len());
    let _ = std::fs::remove_dir_all(&home);
}
#[test]
fn mock_auto_compact_triggers() {
    let (home, _home_guard) = tmp_home("compact");
    let mut cfg = Config::default();
    cfg.provider.ctx_window = 2000;
    cfg.tui.compress_threshold = 0.5;
    let llm = Llm::mock();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut store = SessionStore::new(home.join("sessions"));
    for i in 0..20 {
        store.append(&Msg::new("user", format!("{}:{}", "x".repeat(300), i)));
        store.append(&Msg::new("assistant", "y".repeat(50)));
    }
    let mut agent = Agent::with_store(cfg.clone(), llm, store, None, false, false, cancel);
    let reply = agent.run_turn("继续", &mut |_| {}).unwrap();
    assert!(reply.contains("mock 模式完成"), "回复: {reply}");
    let msgs = stats(&home);
    assert!(msgs < 30, "压缩后消息数应远小于 42，实际 {msgs}");
    let _ = std::fs::remove_dir_all(&home);
}
fn serve_repeating_native_tool() -> u16 {
    use std::io::Write;
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for (round, conn) in listener.incoming().enumerate() {
            let Ok(mut stream) = conn else { break };
            let request = read_full_request(&mut stream);
            let finalizing = round >= 3
                || request.contains("工具阶段已经结束")
                || request.contains("工具已禁用");
            if finalizing {
                let body = json!({
                    "choices": [{"message": {"role": "assistant", "content": "已停止重复调用，并根据已有结果完成收尾。"}}]
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                break;
            }
            let chunk = json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "repeat-call",
                    "function": {
                        "name": "execute_command",
                        "arguments": "{\"op\":\"list_tools\",\"args\":{\"category\":\"net\"}}"
                    }
                }]}}]
            })
            .to_string();
            let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                sse.len(),
                sse
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}
#[test]
fn repeated_native_tool_is_fused_and_finalized() {
    let (home, _home_guard) = tmp_home("repeat_native");
    let mut cfg = Config::default();
    cfg.provider.native_tools = true;
    cfg.provider.auto_reload = false;
    cfg.tool_times = 10;
    let port = serve_repeating_native_tool();
    let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k", "m", 10, 1024);
    let store = SessionStore::new(home.join("sessions"));
    let cancel = Arc::new(AtomicBool::new(false));
    let mut agent = Agent::with_store(cfg, llm, store, None, false, false, cancel);
    let mut tool_runs = 0usize;
    let mut fused = false;
    let reply = agent
        .run_turn("查看网络工具", &mut |event| match event {
            yjlcoder::agent::AgentEvent::ToolRun { .. } => tool_runs += 1,
            yjlcoder::agent::AgentEvent::Notice(text) if text.contains("停止空转") => fused = true,
            _ => {}
        })
        .unwrap();
    assert_eq!(tool_runs, 2, "第三次相同调用应在执行前熔断");
    assert!(fused, "应向界面报告熔断原因");
    assert!(reply.contains("停止重复调用"), "应生成无工具收尾: {reply}");
    assert_eq!(stats(&home), 6, "user + 两轮工具 + 最终回答");
    let _ = std::fs::remove_dir_all(&home);
}
fn serve_reasoning_loop_rescue() -> u16 {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for (round, conn) in listener.incoming().enumerate() {
            let Ok(mut s) = conn else { break };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let chunks: Vec<String> = if round == 0 {
                let unit = "```tool {\"op\":\"execute_command\",\"args\":{\"cmd\":\"echo rescue-ok\"}}\n```\nExecuting.\n\nWait, I'll execute `echo rescue-ok`.\n\n";
                (0..130)
                    .map(|_| json!({"choices":[{"delta":{"reasoning_content": unit}}]}).to_string())
                    .collect()
            } else {
                vec![
                    json!({"choices":[{"delta":{"content":"rescue 成功"}}]}).to_string(),
                    "[DONE]".to_string(),
                ]
            };
            let mut sse = String::new();
            for c in &chunks {
                sse.push_str(&format!("data: {c}\n\n"));
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                sse.len(),
                sse
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
    port
}
#[test]
fn reasoning_loop_rescued_by_extracted_tool() {
    let (home, _home_guard) = tmp_home("rescue");
    let mut cfg = Config::default();
    cfg.provider.native_tools = false;                   
    let port = serve_reasoning_loop_rescue();
    let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k", "m", 10, 1024);
    let cancel = Arc::new(AtomicBool::new(false));
    let store = SessionStore::new(home.join("sessions"));
    let mut agent = Agent::with_store(cfg.clone(), llm, store, None, false, false, cancel);
    let mut notice = String::new();
    let mut tool_run = String::new();
    let mut tool_result = String::new();
    let reply = agent
        .run_turn("看看 noctalia 配置", &mut |ev| match ev {
            yjlcoder::agent::AgentEvent::Notice(n) => notice = n.clone(),
            yjlcoder::agent::AgentEvent::ToolRun { op, .. } => tool_run = op.clone(),
            yjlcoder::agent::AgentEvent::ToolResult(r) => tool_result = r.clone(),
            _ => {}
        })
        .unwrap();
    assert_eq!(tool_run, "execute_command", "应从思考流提取并执行工具");
    assert!(notice.contains("思考流"), "应有截断提取说明: {notice}");
    assert!(notice.contains("3"), "3 个重复块应全部转发: {notice}");
    assert!(tool_result.contains("rescue-ok"), "工具结果应注入: {tool_result}");
    assert_eq!(reply, "rescue 成功", "第二轮回合应给出正常回答: {reply}");
    assert_eq!(stats(&home), 8, "3 个工具块全部执行后应有 8 条消息");
    let _ = std::fs::remove_dir_all(&home);
}
fn serve_post_tool_reasoning_loop() -> (u16, Arc<Mutex<Vec<String>>>) {
    use std::io::Write;
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    std::thread::spawn(move || {
        for (round, conn) in listener.incoming().take(2).enumerate() {
            let Ok(mut stream) = conn else { break };
            let request = read_full_request(&mut stream);
            captured.lock().unwrap().push(request);
            if round == 0 {
                let mut sse = String::new();
                for i in 0..80 {
                    let chunk = json!({
                        "choices": [{"delta": {"reasoning_content": format!(
                            "第{i}段坏思考仍在重新怀疑已经取得的工具事实；"
                        )}}]
                    })
                    .to_string();
                    sse.push_str(&format!("data: {chunk}\n\n"));
                }
                sse.push_str("data: [DONE]\n\n");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse.len(),
                    sse
                );
                let _ = stream.write_all(response.as_bytes());
            } else {
                let body = json!({
                    "choices": [{"message": {"role": "assistant", "content":
                        "文件包含 alpha、beta、gamma 三行。"}}]
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        }
    });
    (port, requests)
}
#[test]
fn tool_result_reasoning_loop_finalizes_immediately_without_thinking() {
    let (home, _home_guard) = tmp_home("post_tool_loop");
    let file = home.join("lines.txt");
    std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
    let mut cfg = Config::default();
    cfg.provider.native_tools = true;
    cfg.provider.auto_reload = false;
    let (port, requests) = serve_post_tool_reasoning_loop();
    let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k", "m", 10, 1024);
    let store = SessionStore::new(home.join("sessions"));
    let cancel = Arc::new(AtomicBool::new(false));
    let mut agent = Agent::with_store(cfg, llm, store, None, false, false, cancel);
    let mut notices = Vec::new();
    let reply = agent
        .run_turn(
            &format!("读取 {}，请概括这个文件", file.display()),
            &mut |event| {
                if let yjlcoder::agent::AgentEvent::Notice(text) = event {
                    notices.push(text);
                }
            },
        )
        .unwrap();
    assert_eq!(reply, "文件包含 alpha、beta、gamma 三行。");
    assert!(
        notices.iter().any(|n| n.contains("立即切换无思考收尾")),
        "应立即收尾: {notices:?}"
    );
    assert!(
        notices.iter().all(|n| !n.contains("注入打断指令")),
        "已有工具结果后不得再注入打断消息: {notices:?}"
    );
    assert_eq!(stats(&home), 3, "不应把打断消息写进会话");
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 2, "只允许原请求 + 一次收尾请求");
    assert!(
        captured[0].contains(r#""reasoning_effort":"none""#),
        "工具结果后的首个请求就必须关闭 llama.cpp 思考"
    );
    assert!(
        captured[1].contains(r#""reasoning_effort":"none""#),
        "收尾请求必须关闭 llama.cpp 思考: {}",
        captured[1]
    );
    assert!(captured[1].contains("beta"), "收尾请求必须携带工具证据");
    assert!(
        !captured[1].contains("注入打断指令"),
        "最小上下文不得携带旧打断消息"
    );
    let _ = std::fs::remove_dir_all(&home);
}
#[test]
fn exact_read_line_is_answered_without_calling_the_model() {
    let (home, _home_guard) = tmp_home("exact_read_line");
    let file = home.join("lines.txt");
    std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
    let mut cfg = Config::default();
    cfg.provider.native_tools = true;
    cfg.provider.auto_reload = false;
    let llm = Llm::remote("http://127.0.0.1:9/v1", "k", "m", 1, 1024);
    let store = SessionStore::new(home.join("sessions"));
    let cancel = Arc::new(AtomicBool::new(false));
    let mut agent = Agent::with_store(cfg, llm, store, None, false, false, cancel);
    let reply = agent
        .run_turn(
            &format!("读取 {}，告诉我第 2 行完整内容，不要猜", file.display()),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(reply, "第 2 行的完整内容是：\nbeta");
    assert_eq!(stats(&home), 3, "user + Read 结果 + 确定性答案");
    let _ = std::fs::remove_dir_all(&home);
}
fn serve_single_raw_answer() -> (u16, Arc<Mutex<Vec<String>>>) {
    use std::io::Write;
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        captured.lock().unwrap().push(read_full_request(&mut stream));
        let chunk = json!({
            "choices": [{"delta": {"content": "模型原始回答"}}]
        })
        .to_string();
        let sse = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            sse.len(),
            sse
        );
        let _ = stream.write_all(response.as_bytes());
    });
    (port, requests)
}
#[test]
fn fuckloop_off_restores_raw_post_tool_model_flow() {
    let (home, _home_guard) = tmp_home("fuckloop_off");
    let file = home.join("lines.txt");
    std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
    let mut cfg = Config::default();
    cfg.provider.native_tools = true;
    cfg.provider.auto_reload = false;
    cfg.fuckloop = false;
    let (port, requests) = serve_single_raw_answer();
    let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k", "m", 10, 1024);
    let store = SessionStore::new(home.join("sessions"));
    let cancel = Arc::new(AtomicBool::new(false));
    let mut agent = Agent::with_store(cfg, llm, store, None, false, false, cancel);
    let reply = agent
        .run_turn(
            &format!("读取 {}，告诉我第 2 行完整内容", file.display()),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(reply, "模型原始回答", "off 时不得使用确定性 Read 直答");
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 1, "off 时应把工具结果交给原始模型流程");
    assert!(
        !captured[0].contains("reasoning_effort"),
        "off 时不得注入禁思考参数"
    );
    assert!(
        !captured[0].contains("chat_template_kwargs"),
        "off 时不得注入模板思考开关"
    );
    assert_eq!(stats(&home), 3);
    let _ = std::fs::remove_dir_all(&home);
}
fn serve_plan_loop_rescue() -> u16 {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for (round, conn) in listener.incoming().enumerate() {
            let Ok(mut s) = conn else { break };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let chunks: Vec<String> = if round == 0 {
                [
                    "```tool {\"op\":\"execute_command\",\"args\":{\"cmd\":\"echo plan-1\"}}\n```\n先执行这个。\n\n",
                    "```tool {\"op\":\"execute_command\",\"args\":{\"cmd\":\"echo plan-2\"}}\n```\n如果没结果再试。\n\n",
                    "```tool {\"op\":\"execute_command\",\"args\":{\"cmd\":\"echo plan-ok\"}}\n```\n如果找不到就这样。\n\n",
                ]
                .iter()
                .map(|b| json!({"choices":[{"delta":{"reasoning_content": b}}]}).to_string())
                .collect()
            } else {
                vec![
                    json!({"choices":[{"delta":{"content":"规划循环救援成功"}}]}).to_string(),
                    "[DONE]".to_string(),
                ]
            };
            let mut sse = String::new();
            for c in &chunks {
                sse.push_str(&format!("data: {c}\n\n"));
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                sse.len(),
                sse
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
    port
}
#[test]
fn plan_loop_rescued_by_extracted_tool() {
    let (home, _home_guard) = tmp_home("planloop");
    let mut cfg = Config::default();
    cfg.provider.native_tools = false;
    let port = serve_plan_loop_rescue();
    let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k", "m", 10, 1024);
    let cancel = Arc::new(AtomicBool::new(false));
    let store = SessionStore::new(home.join("sessions"));
    let mut agent = Agent::with_store(cfg.clone(), llm, store, None, false, false, cancel);
    let mut notice = String::new();
    let mut tool_run = String::new();
    let mut tool_result = String::new();
    let reply = agent
        .run_turn("看看 noctalia 配置", &mut |ev| match ev {
            yjlcoder::agent::AgentEvent::Notice(n) => notice = n.clone(),
            yjlcoder::agent::AgentEvent::ToolRun { op, .. } => tool_run = op.clone(),
            yjlcoder::agent::AgentEvent::ToolResult(r) => tool_result = r.clone(),
            _ => {}
        })
        .unwrap();
    assert_eq!(tool_run, "execute_command", "应从思考流提取并执行工具");
    assert!(notice.contains("思考流"), "应有截断提取说明: {notice}");
    assert!(notice.contains("3"), "应注明暴力转发 3 个块: {notice}");
    assert!(tool_result.contains("plan-ok"), "应提取最后一个块执行: {tool_result}");
    assert_eq!(reply, "规划循环救援成功", "第二轮回合应给出正常回答: {reply}");
    assert_eq!(stats(&home), 8, "3 个工具块全部执行后应有 8 条消息");
    let _ = std::fs::remove_dir_all(&home);
}
fn serve_ask_user_flow() -> u16 {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for (round, conn) in listener.incoming().enumerate() {
            let Ok(mut s) = conn else { break };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let chunks: Vec<String> = if round == 0 {
                let tool_call = json!({
                    "op": "ask_user",
                    "args": {
                        "questions": [
                            {
                                "question": "你想去哪？",
                                "header": "目的地",
                                "options": [
                                    {"label":"北京", "description":"明天出发"},
                                    {"label":"上海", "description":"后天出发"}
                                ],
                                "multiSelect": false
                            },
                            {
                                "question": "什么时候出发？",
                                "header": "时间",
                                "options": [
                                    {"label":"明天", "description":"尽快出发"},
                                    {"label":"后天", "description":"多准备一天"}
                                ],
                                "multiSelect": false
                            }
                        ]
                    }
                });
                vec![
                    json!({"choices":[{"delta":{"content":format!("```tool {tool_call}\n```")}}]}).to_string(),
                    "[DONE]".to_string(),
                ]
            } else {
                vec![
                    json!({"choices":[{"delta":{"content":"好的，明天去北京"}}]}).to_string(),
                    "[DONE]".to_string(),
                ]
            };
            let mut sse = String::new();
            for c in &chunks {
                sse.push_str(&format!("data: {c}\n\n"));
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                sse.len(),
                sse
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
    port
}
#[test]
fn ask_user_blocks_until_answer_then_continues() {
    use std::sync::mpsc;
    use yjlcoder::tools::{AskAnswer, AskRequest};
    let (home, _home_guard) = tmp_home("askuser");
    let cfg = Config::default();
    let port = serve_ask_user_flow();
    let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k", "m", 10, 1024);
    let cancel = Arc::new(AtomicBool::new(false));
    let store = SessionStore::new(home.join("sessions"));
    let (ask_tx, ask_rx) = mpsc::channel::<AskRequest>();
    let (answer_tx, answer_rx) = mpsc::channel::<AskAnswer>();
    let responder = std::thread::spawn(move || {
        let req = ask_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("模型应发出提问请求");
        assert_eq!(req.questions.len(), 2);
        assert_eq!(req.questions[0].header, "目的地");
        assert_eq!(req.questions[0].options[0].label, "北京");
        answer_tx
            .send(AskAnswer {
                id: req.id,
                answers: std::collections::BTreeMap::from([
                    ("你想去哪？".into(), "北京".into()),
                    ("什么时候出发？".into(), "明天".into()),
                ]),
            })
            .unwrap();
    });
    let mut agent = Agent::with_store(cfg.clone(), llm, store, None, false, false, cancel);
    agent.set_ask_channels(ask_tx, answer_rx);
    let mut tool_result = String::new();
    let reply = agent
        .run_turn("安排一下行程", &mut |ev| {
            if let yjlcoder::agent::AgentEvent::ToolResult(r) = ev {
                tool_result = r.clone();
            }
        })
        .unwrap();
    responder.join().unwrap();
    assert!(
        tool_result.contains("\"你想去哪？\"=\"北京\"")
            && tool_result.contains("\"什么时候出发？\"=\"明天\""),
        "结构化回答应作为问题→答案映射注入: {tool_result}"
    );
    assert_eq!(reply, "好的，明天去北京", "模型应基于用户回答继续: {reply}");
    assert_eq!(stats(&home), 4, "会话应有 4 条消息");
    let _ = std::fs::remove_dir_all(&home);
}
type SeenRequests = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;
fn serve_reload_probe(mode: &'static str) -> (u16, SeenRequests) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: SeenRequests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let state: std::sync::Arc<std::sync::Mutex<String>> =
        std::sync::Arc::new(std::sync::Mutex::new("loaded".to_string()));
    let state2 = state.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { break };
            let mut buf = [0u8; 8192];
            let _ = s.read(&mut buf);
            let req = String::from_utf8_lossy(&buf[..]);
            let path = req
                .lines()
                .next()
                .unwrap_or("")
                .split(' ')
                .nth(1)
                .unwrap_or("")
                .to_string();
            let auth = req
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                .unwrap_or("")
                .to_string();
            seen2.lock().unwrap().push((path.clone(), auth));
            let (status, body): (u16, String) = if path.starts_with("/models?") || path == "/models" {
                if mode == "llama" {
                    let s = state2.lock().unwrap().clone();
                    (
                        200,
                        format!(r#"{{"data":[{{"id":"m","status":{{"value":"{s}"}}}}]}}"#),
                    )
                } else {
                    (404, r#"{"error":{"message":"File Not Found","type":"not_found_error","code":404}}"#.into())
                }
            } else if path.ends_with("/models/load") && mode == "llama" {
                *state2.lock().unwrap() = "loaded".to_string();
                (200, r#"{"success":true}"#.into())
            } else if path.ends_with("/models/unload") && mode == "llama" {
                *state2.lock().unwrap() = "unloaded".to_string();
                (200, r#"{"success":true}"#.into())
            } else if path.contains("/api/v1/models/load") {
                (200, r#"{"success":true}"#.into())
            } else {
                (404, r#"{"error":{"message":"File Not Found","type":"not_found_error","code":404}}"#.into())
            };
            let resp = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
    (port, seen)
}
#[test]
fn reload_uses_llamacpp_router_protocol_with_auth() {
    let (port, seen) = serve_reload_probe("llama");
    let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k3y", "m", 10, 1024);
    llm.reload_model().unwrap();
    let seen = seen.lock().unwrap();
    let paths: Vec<&str> = seen.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        paths.first(),
        Some(&"/models/unload"),
        "应先走 llama.cpp 路由器协议卸载: {paths:?}"
    );
    assert!(
        paths.contains(&"/models/load"),
        "应包含路由器协议加载: {paths:?}"
    );
    let ui = paths.iter().position(|p| *p == "/models/unload").unwrap();
    let li = paths.iter().position(|p| *p == "/models/load").unwrap();
    assert!(ui < li, "unload 必须在 load 之前: {paths:?}");
    assert!(
        paths.iter().all(|p| p.starts_with("/models/unload")
            || p.starts_with("/models/load")
            || p.starts_with("/models?reload")),
        "只应访问路由器协议端点: {paths:?}"
    );
    assert!(
        seen.iter().all(|(_, a)| a.contains("Bearer k3y")),
        "所有控制请求必须带 Authorization: {seen:?}"
    );
}
#[test]
fn reload_falls_back_to_lmstudio_protocol_when_router_missing() {
    let (port, seen) = serve_reload_probe("lmstudio");
    let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k3y", "m", 10, 1024);
    llm.reload_model().unwrap();
    let seen = seen.lock().unwrap();
    let paths: Vec<&str> = seen.iter().map(|(p, _)| p.as_str()).collect();
    assert!(
        paths.contains(&"/api/v1/models/load"),
        "应回落 LM Studio 协议: {paths:?}"
    );
    assert!(
        seen.iter().all(|(_, a)| a.contains("Bearer k3y")),
        "回落请求也必须带 Authorization: {seen:?}"
    );
}
