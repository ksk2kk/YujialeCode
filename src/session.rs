use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use crate::llm::Msg;
#[derive(Debug, Clone)]
pub struct Session {
    pub messages: Vec<Msg>,
}
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
    current: String,
    dirty: bool,
}
impl SessionStore {
    pub fn new(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(&dir);
        let current = "main".to_string();
        if !dir.join("main.jsonl").exists() {
            let _ = fs::write(dir.join("main.jsonl"), "");
        }
        SessionStore { dir, current, dirty: false }
    }
    pub fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.jsonl"))
    }
    pub fn current_id(&self) -> &str {
        &self.current
    }
    pub fn current(&self) -> Session {
        self.load(&self.current)
    }
    pub fn load(&self, id: &str) -> Session {
        let mut messages = Vec::new();
        if let Ok(f) = fs::File::open(self.path(id)) {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(m) = serde_json::from_str::<Msg>(line) {
                    messages.push(m);
                }
            }
        }
        Session { messages }
    }
    pub fn append(&mut self, msg: &Msg) {
        let mut f = match fs::OpenOptions::new().create(true).append(true).open(self.path(&self.current)) {
            Ok(f) => f,
            Err(_) => return,                                  
        };
        let line = serde_json::to_string(msg).unwrap_or_default();
        let _ = writeln!(f, "{line}");
        let _ = f.flush();
    }
    pub fn list(&self) -> Vec<String> {
        let mut out: Vec<String> = fs::read_dir(&self.dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
                    .filter_map(|e| {
                        e.path().file_stem().map(|s| s.to_string_lossy().into_owned())
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }
    pub fn switch(&mut self, id: &str) -> Result<(), String> {
        let safe = sanitize_id(id);
        if !self.path(&safe).exists() {
            return Err(format!("会话 {id} 不存在（/ls 查看）"));
        }
        self.current = safe;
        Ok(())
    }
    pub fn new_session(&mut self, id: &str) -> String {
        let safe = sanitize_id(id);
        if !self.path(&safe).exists() {
            let _ = fs::write(self.path(&safe), "");
        }
        self.current = safe.clone();
        safe
    }
    pub fn new_session_timestamped(&mut self) -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.new_session(&format!("s{secs}"))
    }
    pub fn delete(&mut self, id: &str) -> Result<(), String> {
        let safe = sanitize_id(id);
        if safe == self.current {
            return Err("不能删除当前会话".into());
        }
        match fs::remove_file(self.path(&safe)) {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("删除失败: {e}")),
        }
    }
    pub fn replace_current(&mut self, messages: &[Msg]) -> Result<(), String> {
        let p = self.path(&self.current);
        let tmp = p.with_extension("jsonl.tmp");
        let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        for m in messages {
            let line = serde_json::to_string(m).map_err(|e| e.to_string())?;
            let _ = writeln!(f, "{line}");
        }
        f.flush().map_err(|e| e.to_string())?;                             
        fs::rename(&tmp, &p).map_err(|e| e.to_string())?;
        Ok(())
    }
    pub fn set_dirty(&mut self, v: bool) {
        self.dirty = v;
    }
    #[allow(dead_code)]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
}
pub fn export_markdown(id: &str, messages: &[Msg]) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Yujiale Code 会话导出: {id}\n\n"));
    out.push_str(&format!(
        "- 导出时间: {}\n- 消息数: {}\n\n",
        crate::time::now_stamp(),
        messages.len()
    ));
    for (i, m) in messages.iter().enumerate() {
        let role_cn = match m.role.as_str() {
            "user" => "用户",
            "assistant" => "助手",
            "tool" => "工具结果",
            other => other,
        };
        out.push_str(&format!("## [{i}] {role_cn}\n\n"));
        if !m.content.is_empty() {
            out.push_str(m.content.trim_end());
            out.push('\n');
        }
        for tc in &m.tool_calls {
            out.push_str(&format!("\n**工具调用: {}**\n\n```json\n{}\n```\n", tc.name, tc.args));
        }
        out.push('\n');
    }
    out
}
pub fn sanitize_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    if cleaned.is_empty() {
        "s".to_string()
    } else {
        cleaned
    }
}
pub fn session_stats(path: &Path) -> (usize, usize) {
    let mut msgs = 0usize;
    let mut tokens = 0usize;
    if let Ok(f) = fs::File::open(path) {
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            msgs += 1;
            if let Ok(m) = serde_json::from_str::<Msg>(&line) {
                tokens += crate::compress::approx_token_count(&m.content);
            }
        }
    }
    (msgs, tokens)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("yjlcoder_test_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }
    #[test]
    fn session_roundtrip() {
        let d = tmp_dir("roundtrip");
        let mut store = SessionStore::new(d.clone());
        store.append(&Msg::new("user", "你好"));
        store.append(&Msg::new("assistant", "世界"));
        let s = store.current();
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.messages[1].content, "世界");
        let _ = fs::remove_dir_all(&d);
    }
    #[test]
    fn multi_session_switch() {
        let d = tmp_dir("multi");
        let mut store = SessionStore::new(d.clone());
        store.append(&Msg::new("user", "main消息"));
        let id = store.new_session("proj-x");
        assert_eq!(id, "proj-x");
        store.append(&Msg::new("user", "proj消息"));
        store.switch("main").unwrap();
        let s = store.current();
        assert_eq!(s.messages[0].content, "main消息");
        assert!(store.list().contains(&"proj-x".to_string()));
        let _ = fs::remove_dir_all(&d);
    }
    #[test]
    fn timestamped_session_unique() {
        let d = tmp_dir("ts");
        let mut store = SessionStore::new(d.clone());
        let id = store.new_session_timestamped();
        assert!(id.starts_with('s'));
        assert_eq!(store.current_id(), id);
        let _ = fs::remove_dir_all(&d);
    }
    #[test]
    fn export_markdown_contains_all_roles() {
        use crate::llm::ToolCall;
        let mut asst = Msg::new("assistant", "我查一下");
        asst.tool_calls.push(ToolCall {
            id: "c1".into(),
            name: "execute_command".into(),
            args: r#"{"cmd":"ls"}"#.into(),
        });
        let msgs = vec![
            Msg::new("user", "看看有什么文件"),
            asst,
            Msg::new("tool", "main.c"),
        ];
        let md = export_markdown("main", &msgs);
        assert!(md.starts_with("# Yujiale Code 会话导出: main"), "标题含会话 id");
        assert!(md.contains("- 导出时间: "), "含导出时间");
        assert!(md.contains("- 消息数: 3"), "含消息数");
        assert!(md.contains("## [0] 用户"), "用户消息");
        assert!(md.contains("看看有什么文件"));
        assert!(md.contains("## [1] 助手"), "助手消息");
        assert!(md.contains("**工具调用: execute_command**"), "工具调用日志");
        assert!(md.contains(r#"{"cmd":"ls"}"#), "工具参数完整");
        assert!(md.contains("## [2] 工具结果"), "工具结果日志");
        assert!(md.contains("main.c"));
    }
    #[test]
    fn sanitize_rejects_path_traversal() {
        assert_eq!(sanitize_id("../etc"), "___etc");
        assert_eq!(sanitize_id("a/b"), "a_b");
        assert_eq!(sanitize_id(""), "s");
    }
    #[test]
    fn delete_protects_current() {
        let d = tmp_dir("del");
        let mut store = SessionStore::new(d.clone());
        store.new_session("other");
        store.switch("main").unwrap();
        assert!(store.delete("other").is_ok());
        assert!(store.delete("main").is_err());
        let _ = fs::remove_dir_all(&d);
    }
}
