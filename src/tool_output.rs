use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use crate::compress;
static OUTPUT_SEQ: AtomicU64 = AtomicU64::new(0);
pub fn store_or_preview_for_tool(
    data_root: &Path,
    session_id: &str,
    op: &str,
    output: &str,
    max_tokens: usize,
) -> String {
    let normalized = crate::tool_compat::normalize_call(op, &serde_json::json!({}));
    if matches!(normalized.op.as_str(), "readline" | "listdir") {
        return output.to_string();
    }
    store_or_preview(data_root, session_id, op, output, max_tokens)
}
pub fn store_or_preview(
    data_root: &Path,
    session_id: &str,
    op: &str,
    output: &str,
    max_tokens: usize,
) -> String {
    let token_count = compress::approx_token_count(output);
    if token_count <= max_tokens {
        return output.to_string();
    }
    let preview = compress::formatted_truncate_text(output, max_tokens);
    match persist(data_root, session_id, op, output) {
        Ok(path) => format!(
            "[完整工具输出已保存]\npath: {}\napprox_tokens: {token_count}\n如需更多内容，请用 readline 的 start/end 分段读取。\n\n{preview}",
            path.display()
        ),
        Err(error) => format!("[完整工具输出落盘失败: {error}]\n\n{preview}"),
    }
}
fn persist(data_root: &Path, session_id: &str, op: &str, output: &str) -> Result<PathBuf, String> {
    let dir = data_root.join("tool-results").join(safe_component(session_id));
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let seq = OUTPUT_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("{millis}-{seq}-{}.txt", safe_component(op)));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| e.to_string())?;
    file.write_all(output.as_bytes()).map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    Ok(path)
}
fn safe_component(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .take(64)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn temp_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "yjlcoder_tool_output_{name}_{}_{}",
            std::process::id(),
            OUTPUT_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }
    #[test]
    fn short_output_is_not_persisted() {
        let root = temp_root("short");
        let rendered = store_or_preview(&root, "main", "readline", "短结果", 100);
        assert_eq!(rendered, "短结果");
        assert!(!root.join("tool-results").exists());
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn long_output_is_persisted_and_previewed() {
        let root = temp_root("long");
        let full = "abcdefghij\n".repeat(1000);
        let rendered = store_or_preview(&root, "../main", "read/file", &full, 32);
        assert!(rendered.contains("[完整工具输出已保存]"));
        assert!(rendered.contains("Warning: truncated output"));
        assert!(rendered.contains("readline"));
        let dir = root.join("tool-results").join("___main");
        let files: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|item| item.path()))
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(fs::read_to_string(&files[0]).unwrap(), full);
        assert!(files[0].file_name().unwrap().to_string_lossy().contains("read_file"));
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn read_output_bypasses_generic_preview_for_all_aliases() {
        let root = temp_root("read_passthrough");
        let full = "1\tline\n".repeat(5_000);
        for op in ["readline", "Read", "read_file"] {
            let rendered = store_or_preview_for_tool(&root, "main", op, &full, 32);
            assert_eq!(rendered, full, "{op} must never be silently truncated");
        }
        assert!(!root.join("tool-results").exists());
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn listdir_pages_bypass_generic_preview_for_all_aliases() {
        let root = temp_root("listdir_passthrough");
        let full = (0..500)
            .map(|index| format!("file\tentry-{index:04}\n"))
            .collect::<String>();
        for op in ["listdir", "ls", "list_directory"] {
            let rendered = store_or_preview_for_tool(&root, "main", op, &full, 32);
            assert_eq!(rendered, full, "{op} must keep a complete directory page");
        }
        assert!(!root.join("tool-results").exists());
        let _ = fs::remove_dir_all(root);
    }
}
