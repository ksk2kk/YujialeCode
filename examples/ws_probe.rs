use serde_json::json;
use std::time::{Duration, Instant};
fn main() {
    let is_group = std::env::args().any(|a| a == "--group");
    let (mut ws, _) =
        tungstenite::connect("ws://127.0.0.1:6701/onebot/v11/ws").expect("连接失败");
    if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_mut() {
        let _ = s.set_nonblocking(true);
    }
    let ev = if is_group {
        json!({
            "post_type": "message",
            "message_type": "group",
            "group_id": 728563593_i64,
            "user_id": 3160168215_i64,
            "self_id": 3503963415_i64,
            "message_id": 777777,
            "message": [{"type": "text", "data": {"text": "yjlcoder 你好，测试一下回复"}}],
            "raw_message": "yjlcoder 你好，测试一下回复",
            "time": 0,
        })
    } else {
        json!({
            "post_type": "message",
            "message_type": "private",
            "user_id": 3160168215_i64,
            "self_id": 3503963415_i64,
            "message_id": 777777,
            "message": [{"type": "text", "data": {"text": "你好，测试一下回复"}}],
            "raw_message": "你好，测试一下回复",
            "time": 0,
        })
    };
    ws.send(tungstenite::Message::Text(ev.to_string())).expect("发送失败");
    eprintln!("[probe] 已发送事件");
    let deadline = Instant::now() + Duration::from_secs(90);
    while Instant::now() < deadline {
        match ws.read() {
            Ok(tungstenite::Message::Text(t)) => {
                println!("[probe] RECV {t}");
                if t.contains("send_group_msg") || t.contains("send_private_msg") {
                    println!("[probe] 收到回复 action");
                    break;
                }
            }
            Ok(_) => {}
            Err(e) => {
                let would_block = matches!(&e, tungstenite::Error::Io(ioe)
                    if ioe.kind() == std::io::ErrorKind::WouldBlock);
                if !would_block {
                    eprintln!("[probe] 读取错误: {e}");
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    eprintln!("[probe] 结束");
}
