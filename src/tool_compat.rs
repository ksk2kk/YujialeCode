use serde_json::{Map, Value};
use std::path::Path;
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedCall {
    pub op: String,
    pub args: Value,
    pub notes: Vec<String>,
}
impl NormalizedCall {
    pub fn note(&self) -> Option<String> {
        if self.notes.is_empty() {
            None
        } else {
            Some(format!("[兼容层已纠正: {}]", self.notes.join("；")))
        }
    }
}
pub fn parse_args(raw: &str) -> Value {
    let trimmed = raw.trim().trim_matches('`').trim();                       
    if trimmed.is_empty() {
        return Value::Object(Map::new());
    }
    if let Ok(v) = serde_json::from_str(trimmed) {
        return v;
    }
    let repaired = repair_json_escapes(trimmed);
    if let Ok(v) = serde_json::from_str(&repaired) {
        return v;
    }
    if let Some(fixed) = close_truncated_json(&repaired) {
        if let Ok(v) = serde_json::from_str(&fixed) {
            return v;
        }
    }
    if let Some(v) = parse_key_values(trimmed) {
        return v;
    }
    Value::String(trimmed.trim_matches('"').to_string())
}
pub fn is_supported_name(name: &str) -> bool {
    let n = clean_name(name);
    crate::registry::find_tool(&n).is_some() || alias_name(&n).is_some() || is_generic_name(&n)
}
pub fn normalize_call(raw_op: &str, raw_args: &Value) -> NormalizedCall {
    let cleaned = clean_name(raw_op);
    let mut notes = Vec::new();
    let mut op = match alias_name(&cleaned) {
        Some(canonical) => {
            if canonical != cleaned {
                notes.push(format!("工具 {raw_op}→{canonical}"));
            }
            canonical.to_string()
        }
        None => cleaned.clone(),
    };
    let mut args = object_from_value(&op, raw_args, &mut notes);
    flatten_wrappers(&mut args, &mut notes);
    if op == "execute_command" {
        alias_key(&mut args, "cmd", &["command", "shell", "script"], &mut notes);
        let target = string_at(&args, &["op", "tool", "tool_name", "function"]);
        if let Some(target) = target.filter(|t| clean_name(t) != "execute_command") {
            let inner = take_dispatch_args(&args);
            let mut call = normalize_call(target, &inner);
            notes.append(&mut call.notes);
            call.notes = notes;
            return call;
        }
        if let Some(list_args) = simple_ls_args(&args) {
            notes.push("安全 ls 命令→listdir".into());
            op = "listdir".into();
            args = list_args;
        } else if let Some(read_args) = simple_file_read_args(&args) {
            notes.push("安全文件读取命令→readline".into());
            op = "readline".into();
            args = read_args;
        }
    }
    if is_generic_name(&op) {
        let inferred = infer_op(&args).unwrap_or(match op.as_str() {
            "open" | "cat" => "readline",
            "find" | "files" => "glob",
            "research" | "deep_search" => "web_research",
            "search" | "google" | "internet" => "web_search",
            _ => "execute_command",
        });
        if inferred != op {
            notes.push(format!("按参数识别 {op}→{inferred}"));
            op = inferred.to_string();
        }
    } else if crate::registry::find_tool(&op).is_none() {
        if let Some(inferred) = infer_op(&args) {
            notes.push(format!("未知工具 {raw_op}→{inferred}"));
            op = inferred.to_string();
        }
    }
    normalize_keys(&op, &mut args, &mut notes);
    if let Some(inferred) = repair_mismatched_op(&op, &args) {
        notes.push(format!("参数与工具不符 {op}→{inferred}"));
        op = inferred.to_string();
        normalize_keys(&op, &mut args, &mut notes);
    }
    coerce_types(&mut args, &mut notes);
    NormalizedCall { op, args: Value::Object(args), notes }
}
fn clean_name(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    lower
        .rsplit(['.', ':', '/'])
        .next()
        .unwrap_or(&lower)
        .trim_start_matches("functions_")
        .to_string()
}
fn alias_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "bash" | "shell" | "run" | "exec" | "terminal" | "run_command" => "execute_command",
        "read" | "read_file" | "readfile" | "cat_file" => "readline",
        "write" | "write_file" | "save" | "save_file" => "writefile",
        "edit" | "edit_file" | "replace" | "replace_file" => "editline",
        "append" | "append_file" => "appendline",
        "ls" | "dir" | "list_dir" | "list_directory" => "listdir",
        "find_files" | "file_search" => "glob",
        "ripgrep" | "rg" | "search_text" => "grep",
        "search_web" | "websearch" | "google_search" | "internet_search" => "web_search",
        "fetch_url" | "fetch_web" | "open_url" | "webfetch" => "web_fetch",
        "exa" | "web_researcher" => "web_research",
        "http" | "get_url" => "http_get",
        "remember" => "memory_write",
        "recall" => "memory_search",
        "ask" | "clarify" | "ask_user_question" | "askuserquestion" => "ask_user",
        "tools" | "help_tools" => "list_tools",
        _ => return None,
    })
}
fn is_generic_name(name: &str) -> bool {
    matches!(name, "open" | "cat" | "find" | "files" | "search" | "google" | "internet" | "research" | "deep_search")
}
fn primary_key(op: &str) -> &'static str {
    match op {
        "execute_command" => "cmd",
        "readline" | "listdir" => "path",
        "glob" | "grep" => "pattern",
        "web_fetch" | "http_get" | "http_headers" => "url",
        "web_search" | "web_research" | "memory_search" => "query",
        "writefile" | "appendline" | "memory_write" => "content",
        "ask_user" => "questions",
        _ => "input",
    }
}
fn object_from_value(op: &str, value: &Value, notes: &mut Vec<String>) -> Map<String, Value> {
    match value {
        Value::Object(m) => m.clone(),
        Value::String(s) => match parse_args(s) {
            Value::Object(m) => {
                notes.push("解开字符串 JSON 参数".into());
                m
            }
            v => {
                notes.push(format!("单值参数→{}", primary_key(op)));
                Map::from_iter([(primary_key(op).to_string(), v)])
            }
        },
        Value::Array(a) if matches!(op, "web_fetch") => {
            notes.push("数组参数→urls".into());
            Map::from_iter([("urls".into(), Value::Array(a.clone()))])
        }
        Value::Array(a) if matches!(op, "web_search" | "web_research") => {
            notes.push("数组参数→queries".into());
            Map::from_iter([("queries".into(), Value::Array(a.clone()))])
        }
        Value::Null => Map::new(),
        v => {
            notes.push(format!("单值参数→{}", primary_key(op)));
            Map::from_iter([(primary_key(op).to_string(), v.clone())])
        }
    }
}
fn flatten_wrappers(args: &mut Map<String, Value>, notes: &mut Vec<String>) {
    for _ in 0..3 {
        let wrapper = ["arguments", "parameters", "params", "payload", "request", "input"]
            .into_iter()
            .find(|key| args.contains_key(*key));
        let Some(key) = wrapper else { break };
        let Some(raw_inner) = args.get(key).cloned() else { break };
        let inner = match raw_inner {
            Value::Object(m) => Some(m),
            Value::String(s) => parse_args(&s).as_object().cloned(),
            _ => None,
        };
        let Some(mut inner) = inner else { break };
        let outer = std::mem::take(args);
        for (k, v) in outer {
            if k != key {
                inner.entry(k).or_insert(v);
            }
        }
        *args = inner;
        notes.push(format!("解开 {key} 包装"));
    }
    if !args.contains_key("op") && args.len() == 1 {
        if let Some(raw_inner) = args.get("args").cloned() {
            if let Some(inner) = match raw_inner {
                Value::Object(m) => Some(m),
                Value::String(s) => parse_args(&s).as_object().cloned(),
                _ => None,
            } {
                *args = inner;
                notes.push("解开 args 包装".into());
            }
        }
    }
}
fn take_dispatch_args(args: &Map<String, Value>) -> Value {
    for key in ["args", "arguments", "parameters", "params", "input"] {
        if let Some(v) = args.get(key) {
            return match v {
                Value::String(s) => parse_args(s),
                _ => v.clone(),
            };
        }
    }
    let mut flat = args.clone();
    for key in ["op", "tool", "tool_name", "function"] {
        flat.remove(key);
    }
    Value::Object(flat)
}
fn string_at<'a>(args: &'a Map<String, Value>, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| args.get(*key).and_then(Value::as_str))
}
fn has_any(args: &Map<String, Value>, keys: &[&str]) -> bool {
    keys.iter().any(|key| args.contains_key(*key))
}
fn infer_op(args: &Map<String, Value>) -> Option<&'static str> {
    if has_any(args, &["cmd", "command", "shell", "script"]) {
        return Some("execute_command");
    }
    if has_any(args, &["url", "urls", "uri", "link"]) {
        return Some("web_fetch");
    }
    let has_path = has_any(args, &["path", "file", "file_path", "filename", "target"]);
    if has_path && has_any(args, &["old", "old_text", "oldText", "new", "new_text", "newText", "replacement"]) {
        return Some("editline");
    }
    if has_path && has_any(args, &["content", "data", "body"]) {
        return Some("writefile");
    }
    if has_path && has_any(args, &["pattern", "regex", "needle", "search", "query", "q", "text"]) {
        return Some("grep");
    }
    if has_any(args, &["queries"]) {
        return Some("web_research");
    }
    if has_any(args, &["query", "q", "keyword", "keywords"]) {
        return Some("web_search");
    }
    if has_any(args, &["pattern", "glob"]) {
        return Some("glob");
    }
    if has_path {
        return Some("readline");
    }
    None
}
fn normalize_keys(op: &str, args: &mut Map<String, Value>, notes: &mut Vec<String>) {
    match op {
        "execute_command" => {
            alias_key(args, "cmd", &["command", "shell", "script", "input"], notes);
            alias_key(args, "cwd", &["workdir", "working_directory", "dir"], notes);
        }
        "readline" | "writefile" | "editline" | "appendline" | "listdir" => {
            alias_key(args, "path", &["file_path", "filepath", "file", "filename", "target"], notes);
        }
        "glob" => {
            alias_key(args, "pattern", &["glob", "query", "q", "name"], notes);
            alias_key(args, "base", &["path", "dir", "directory", "cwd"], notes);
        }
        "grep" => {
            alias_key(args, "pattern", &["query", "q", "regex", "needle", "search", "text"], notes);
            alias_key(args, "path", &["base", "dir", "directory", "cwd"], notes);
        }
        "web_search" | "web_research" => {
            alias_key(args, "query", &["q", "keyword", "keywords", "search", "text", "prompt"], notes);
            alias_key(args, "count", &["limit", "num_results", "numResults", "max_results"], notes);
            alias_key(args, "include_domains", &["includeDomains", "domains", "site"], notes);
            alias_key(args, "exclude_domains", &["excludeDomains", "exclude_sites"], notes);
            alias_key(args, "fetch_top", &["fetchTop", "fetch_count"], notes);
        }
        "web_fetch" | "http_get" | "http_headers" => {
            alias_key(args, "url", &["uri", "link", "href"], notes);
            alias_key(args, "max_chars", &["maxChars", "limit", "max_length"], notes);
        }
        "memory_search" => alias_key(args, "query", &["q", "keyword", "keywords", "text"], notes),
        "memory_write" => alias_key(args, "content", &["text", "data", "body", "value"], notes),
        "ask_user" => alias_key(args, "questions", &["question", "prompt", "text"], notes),
        _ => {}
    }
    if matches!(op, "writefile" | "appendline") {
        alias_key(args, "content", &["text", "data", "body", "value"], notes);
    }
    if op == "editline" {
        alias_key(args, "old", &["old_text", "oldText", "find", "search"], notes);
        alias_key(args, "new", &["new_text", "newText", "replace", "replacement"], notes);
    }
    if op == "readline" {
        alias_key(args, "start", &["line", "start_line", "startLine", "offset"], notes);
        alias_key(args, "end", &["end_line", "endLine", "stop"], notes);
        alias_key(args, "limit", &["count", "max_lines", "maxLines"], notes);
    }
    if op == "listdir" {
        alias_key(args, "offset", &["start", "skip", "page_offset", "pageOffset"], notes);
        alias_key(args, "limit", &["count", "max", "max_results", "page_size", "pageSize"], notes);
        alias_key(args, "all", &["hidden", "include_hidden", "includeHidden"], notes);
    }
}
fn alias_key(args: &mut Map<String, Value>, canonical: &str, aliases: &[&str], notes: &mut Vec<String>) {
    if args.contains_key(canonical) {
        return;
    }
    for alias in aliases {
        if let Some(value) = args.remove(*alias) {
            args.insert(canonical.to_string(), value);
            notes.push(format!("参数 {alias}→{canonical}"));
            return;
        }
    }
}
fn repair_mismatched_op<'a>(op: &str, args: &'a Map<String, Value>) -> Option<&'a str> {
    match op {
        "web_search" if !args.contains_key("query") && !args.contains_key("queries") && has_any(args, &["url", "urls"]) => {
            Some("web_fetch")
        }
        "web_fetch" if !args.contains_key("url") && !args.contains_key("urls") && has_any(args, &["query", "queries"]) => {
            Some("web_search")
        }
        "readline" if !args.contains_key("path") && args.contains_key("cmd") => Some("execute_command"),
        _ => None,
    }
}
fn coerce_types(args: &mut Map<String, Value>, notes: &mut Vec<String>) {
    for key in [
        "start", "end", "offset", "limit", "count", "max", "max_chars", "timeout",
        "timeout_ms", "fetch_top",
    ] {
        if let Some(Value::String(s)) = args.get(key) {
            if let Ok(n) = s.trim().parse::<u64>() {
                args.insert(key.into(), Value::Number(n.into()));
                notes.push(format!("{key} 字符串→数字"));
            }
        }
    }
    for key in ["deep", "recursive", "livecrawl", "all"] {
        if let Some(Value::String(s)) = args.get(key) {
            let b = match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" | "是" => Some(true),
                "false" | "0" | "no" | "off" | "否" => Some(false),
                _ => None,
            };
            if let Some(b) = b {
                args.insert(key.into(), Value::Bool(b));
                notes.push(format!("{key} 字符串→布尔"));
            }
        }
    }
    for key in ["urls", "queries", "include_domains", "exclude_domains", "must_include", "questions"] {
        if let Some(Value::String(s)) = args.get(key).cloned() {
            let parsed = parse_args(&s);
            let values = match parsed {
                Value::Array(a) => a,
                Value::String(one) => one
                    .split(['\n', ','])
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(|v| Value::String(v.to_string()))
                    .collect(),
                other => vec![other],
            };
            args.insert(key.into(), Value::Array(values));
            notes.push(format!("{key} 单值→数组"));
        }
    }
}
fn simple_ls_args(args: &Map<String, Value>) -> Option<Map<String, Value>> {
    let cmd = args.get("cmd")?.as_str()?.trim();
    let words = simple_shell_words(cmd)?;
    if words.first().map(String::as_str) != Some("ls") {
        return None;
    }
    let mut include_hidden = false;
    let mut options_done = false;
    let mut paths = Vec::new();
    for word in &words[1..] {
        if !options_done && word == "--" {
            options_done = true;
        } else if !options_done && word.starts_with("--") {
            match word.as_str() {
                "--all" | "--almost-all" => include_hidden = true,
                "--color" | "--color=auto" | "--color=always" | "--color=never"
                | "--format=single-column" | "--group-directories-first" => {}
                _ => return None,
            }
        } else if !options_done && word.starts_with('-') && word != "-" {
            for flag in word[1..].chars() {
                match flag {
                    'a' | 'A' => include_hidden = true,
                    '1' | 'l' | 'h' | 'F' | 'p' => {}
                    _ => return None,
                }
            }
        } else {
            paths.push(word.clone());
        }
    }
    if paths.len() > 1 {
        return None;
    }
    let cwd = args.get("cwd").and_then(Value::as_str).unwrap_or(".");
    let raw_path = paths.first().map(String::as_str).unwrap_or(".");
    let path = resolve_shell_path(raw_path, cwd);
    Some(Map::from_iter([
        ("path".into(), Value::String(path)),
        ("offset".into(), Value::Number(0u64.into())),
        ("limit".into(), Value::Number(200u64.into())),
        ("all".into(), Value::Bool(include_hidden)),
    ]))
}
fn simple_file_read_args(args: &Map<String, Value>) -> Option<Map<String, Value>> {
    let cmd = args.get("cmd")?.as_str()?.trim();
    let words = simple_shell_words(cmd)?;
    let cwd = args.get("cwd").and_then(Value::as_str).unwrap_or(".");
    let (raw_path, start, limit) = match words.first().map(String::as_str)? {
        "cat" => {
            let rest = if words.get(1).map(String::as_str) == Some("--") {
                &words[2..]
            } else {
                &words[1..]
            };
            if rest.len() != 1 || rest[0].starts_with('-') {
                return None;
            }
            (rest[0].as_str(), None, None)
        }
        "sed" => {
            let rest = match words.as_slice() {
                [_, flag, pattern, path] if flag == "-n" || flag == "--quiet" || flag == "--silent" => {
                    (pattern.as_str(), path.as_str())
                }
                [_, flag, separator, pattern, path]
                    if (flag == "-n" || flag == "--quiet" || flag == "--silent")
                        && separator == "--" =>
                {
                    (pattern.as_str(), path.as_str())
                }
                _ => return None,
            };
            let (start, limit) = parse_sed_print_range(rest.0)?;
            (rest.1, Some(start), Some(limit))
        }
        "head" => {
            let (count, path) = match words.as_slice() {
                [_, flag, count, path] if flag == "-n" || flag == "--lines" => {
                    (count.parse::<u64>().ok()?, path.as_str())
                }
                [_, compact, path] if compact.starts_with('-') && compact.len() > 1 => {
                    (compact[1..].parse::<u64>().ok()?, path.as_str())
                }
                _ => return None,
            };
            if count == 0 {
                return None;
            }
            (path, Some(1), Some(count))
        }
        _ => return None,
    };
    if raw_path.starts_with('-') {
        return None;
    }
    let mut out = Map::new();
    out.insert("path".into(), Value::String(resolve_shell_path(raw_path, cwd)));
    if let Some(start) = start {
        out.insert("start".into(), Value::Number(start.into()));
    }
    if let Some(limit) = limit {
        out.insert("limit".into(), Value::Number(limit.into()));
    }
    Some(out)
}
fn parse_sed_print_range(pattern: &str) -> Option<(u64, u64)> {
    let body = pattern.strip_suffix('p')?;
    if let Some((start, end)) = body.split_once(',') {
        let start = start.parse::<u64>().ok()?;
        let end = end.parse::<u64>().ok()?;
        if start == 0 || end < start {
            return None;
        }
        Some((start, end - start + 1))
    } else {
        let line = body.parse::<u64>().ok()?;
        (line > 0).then_some((line, 1))
    }
}
fn resolve_shell_path(raw_path: &str, cwd: &str) -> String {
    if raw_path.starts_with("~/") || Path::new(raw_path).is_absolute() || cwd == "." {
        raw_path.to_string()
    } else {
        Path::new(cwd).join(raw_path).to_string_lossy().into_owned()
    }
}
fn simple_shell_words(input: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut touched = false;
    for ch in input.chars() {
        if escaped {
            word.push(ch);
            touched = true;
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    quote = None;
                } else {
                    word.push(ch);
                    touched = true;
                }
            }
            Some('"') => match ch {
                '"' => quote = None,
                '\\' => escaped = true,
                '$' | '`' => return None,
                _ => {
                    word.push(ch);
                    touched = true;
                }
            },
            _ => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    touched = true;
                }
                '\\' => escaped = true,
                ' ' | '\t' if touched => {
                    words.push(std::mem::take(&mut word));
                    touched = false;
                }
                ' ' | '\t' => {}
                '\n' | '\r' | ';' | '|' | '&' | '<' | '>' | '$' | '`' | '*' | '?' | '[' | ']' => {
                    return None;
                }
                _ => {
                    word.push(ch);
                    touched = true;
                }
            },
        }
    }
    if escaped || quote.is_some() {
        return None;
    }
    if touched {
        words.push(word);
    }
    Some(words)
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
                out.push('\\');
                out.push(n);
            }
            Some(n) => out.push(n),
            None => out.push('\\'),
        }
    }
    out
}
fn close_truncated_json(s: &str) -> Option<String> {
    let mut stack = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' if stack.last() == Some(&c) => {
                stack.pop();
            }
            _ => {}
        }
    }
    if in_string || stack.is_empty() || !s.trim_start().starts_with(['{', '[']) {
        return None;
    }
    let mut fixed = s.to_string();
    while let Some(c) = stack.pop() {
        fixed.push(c);
    }
    Some(fixed)
}
fn parse_key_values(s: &str) -> Option<Value> {
    let mut map = Map::new();
    for part in s.split(['\n', ',']) {
        let (key, raw) = part.split_once('=')?;
        let key = key.trim().trim_matches(['"', '\'']);
        if key.is_empty() {
            return None;
        }
        let raw = raw.trim();
        let value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.trim_matches(['"', '\'']).to_string()));
        map.insert(key.to_string(), value);
    }
    (!map.is_empty()).then_some(Value::Object(map))
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn aliases_and_unwraps_nested_json() {
        let call = normalize_call("Functions.Web-Search", &json!({"arguments":"{\"q\":\"rust async\",\"limit\":\"5\"}"}));
        assert_eq!(call.op, "web_search");
        assert_eq!(call.args["query"], "rust async");
        assert_eq!(call.args["count"], 5);
        assert!(!call.notes.is_empty());
    }
    #[test]
    fn dispatch_and_wrong_tool_are_forced_to_intent() {
        let call = normalize_call("execute_command", &json!({"tool":"edit", "args":{"file_path":"a.rs","oldText":"x","newText":"y"}}));
        assert_eq!(call.op, "editline");
        assert_eq!(call.args["path"], "a.rs");
        assert_eq!(call.args["old"], "x");
        let wrong = normalize_call("web_search", &json!({"url":"https://example.com"}));
        assert_eq!(wrong.op, "web_fetch");
    }
    #[test]
    fn scalar_and_malformed_arguments_survive() {
        assert_eq!(normalize_call("bash", &Value::String("pwd".into())).args["cmd"], "pwd");
        assert_eq!(parse_args("{\"q\":\"rust\""), json!({"q":"rust"}));
        assert_eq!(parse_args("q=rust,count=3"), json!({"q":"rust","count":3}));
    }
    #[test]
    fn arrays_and_booleans_are_coerced() {
        let call = normalize_call("research", &json!({"queries":"one,two", "deep":"yes", "fetchTop":"2"}));
        assert_eq!(call.op, "web_research");
        assert_eq!(call.args["queries"].as_array().unwrap().len(), 2);
        assert_eq!(call.args["deep"], true);
        assert_eq!(call.args["fetch_top"], 2);
    }
    #[test]
    fn claude_ask_user_question_name_maps_to_local_tool() {
        let call = normalize_call(
            "AskUserQuestion",
            &json!({"questions":[{"question":"选哪个？","header":"选择","options":[{"label":"A","description":"方案 A"},{"label":"B","description":"方案 B"}],"multiSelect":false}]}),
        );
        assert_eq!(call.op, "ask_user");
        assert!(call.args["questions"].is_array());
    }
    #[test]
    fn listdir_aliases_and_paging_fields_are_normalized() {
        let call = normalize_call(
            "List-Directory",
            &json!({"file_path":"/tmp/demo", "skip":"20", "pageSize":"50", "includeHidden":"yes"}),
        );
        assert_eq!(call.op, "listdir");
        assert_eq!(call.args["path"], "/tmp/demo");
        assert_eq!(call.args["offset"], 20);
        assert_eq!(call.args["limit"], 50);
        assert_eq!(call.args["all"], true);
    }
    #[test]
    fn safe_ls_shell_commands_are_forced_to_listdir() {
        let plain = normalize_call(
            "execute_command",
            &json!({"cmd":"ls -la \"folder with spaces\"", "cwd":"/tmp"}),
        );
        assert_eq!(plain.op, "listdir");
        assert_eq!(plain.args["path"], "/tmp/folder with spaces");
        assert_eq!(plain.args["offset"], 0);
        assert_eq!(plain.args["limit"], 200);
        assert_eq!(plain.args["all"], true);
        for complex in ["ls -R .", "ls a b", "ls | head", "ls *.rs", "ls $HOME"] {
            assert_eq!(
                normalize_call("execute_command", &json!({"cmd": complex})).op,
                "execute_command",
                "complex shell semantics must be preserved: {complex}"
            );
        }
    }
    #[test]
    fn safe_shell_file_reads_are_forced_to_readline() {
        let cat = normalize_call(
            "execute_command",
            &json!({"cmd":"cat -- \"folder with spaces/demo.toml\"", "cwd":"/tmp"}),
        );
        assert_eq!(cat.op, "readline");
        assert_eq!(cat.args["path"], "/tmp/folder with spaces/demo.toml");
        assert!(cat.args.get("start").is_none());
        let sed = normalize_call(
            "execute_command",
            &json!({"cmd":"sed -n '1,260p' ~/.config/noctalia/noctalia-config.toml"}),
        );
        assert_eq!(sed.op, "readline");
        assert_eq!(sed.args["start"], 1);
        assert_eq!(sed.args["limit"], 260);
        let one = normalize_call(
            "execute_command",
            &json!({"cmd":"sed --quiet '77p' demo.txt"}),
        );
        assert_eq!(one.op, "readline");
        assert_eq!(one.args["start"], 77);
        assert_eq!(one.args["limit"], 1);
        let head = normalize_call(
            "execute_command",
            &json!({"cmd":"head -n 80 demo.txt", "cwd":"/work"}),
        );
        assert_eq!(head.op, "readline");
        assert_eq!(head.args["path"], "/work/demo.txt");
        assert_eq!(head.args["start"], 1);
        assert_eq!(head.args["limit"], 80);
        for complex in [
            "cat a b",
            "cat *.rs",
            "cat $HOME/demo",
            "cat demo | head",
            "sed -n '1,$p' demo",
            "sed -n '/start/,/end/p' demo",
            "head demo",
        ] {
            assert_eq!(
                normalize_call("execute_command", &json!({"cmd": complex})).op,
                "execute_command",
                "complex shell semantics must be preserved: {complex}"
            );
        }
    }
}
