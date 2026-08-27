








use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;

pub const CUSTOM_CATEGORY: &str = "custom";
const MAX_SCRIPT_BYTES: usize = 64 * 1024;
const MAX_DESCRIPTION_CHARS: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamicTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub script: String,
    #[serde(default = "default_language")]
    pub language: String,
    pub timeout_secs: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

fn default_language() -> String {
    "sh".into()
}

pub fn register(cfg: &Config, args: &Value, reserved: &[&str]) -> Result<String, String> {
    let spec = args.get("tool").unwrap_or(args);
    let violations = validate_spec(spec, reserved);
    if !violations.is_empty() {
        return Err(correction_error(&violations));
    }
    let name = spec["name"].as_str().unwrap().to_string();
    let description = spec["description"].as_str().unwrap().trim().to_string();
    let parameters = spec["parameters"].clone();
    let script = normalize_script(spec["script"].as_str().unwrap());
    let language = spec
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("sh")
        .to_string();
    if language != "sh" && language != "python3" {
        return Err("language 只能是 sh 或 python3（python3 时脚本用 sys.argv[1] 读取 JSON 参数）".into());
    }
    let timeout_secs = spec.get("timeout_secs").and_then(Value::as_u64).unwrap_or(120);
    let replace = args.get("replace").and_then(Value::as_bool).unwrap_or(false)
        || spec.get("replace").and_then(Value::as_bool).unwrap_or(false);
    let path = tool_path(cfg, &name);
    if path.exists() && !replace {
        return Err(format!(
            "工具 {name} 已存在。确认覆盖时重试并加 {{\"replace\":true}}；未确认不会覆盖。"
        ));
    }
    let created_at_ms = load(cfg, &name).map(|old| old.created_at_ms).unwrap_or_else(now_ms);
    let tool = DynamicTool {
        name: name.clone(),
        description,
        parameters,
        script,
        language,
        timeout_secs,
        created_at_ms,
        updated_at_ms: now_ms(),
    };
    save_atomic(cfg, &tool)?;
    Ok(format!(
        "已热注册工具 {name}，无需重启。查看: list_tools {{\"category\":\"custom\"}}；调用: execute_command {{\"op\":\"{name}\",\"args\":{{...}}}}"
    ))
}

pub fn load(cfg: &Config, name: &str) -> Option<DynamicTool> {
    if !valid_name(name) {
        return None;
    }
    let text = fs::read_to_string(tool_path(cfg, name)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn delete(cfg: &Config, name: &str) -> Result<String, String> {
    if !valid_name(name) {
        return Err("name 必须匹配 [a-z][a-z0-9_]{2,47}".into());
    }
    let json_path = tool_path(cfg, name);
    let sh_path = script_path(cfg, name);
    let mut removed = false;
    if json_path.exists() {
        fs::remove_file(&json_path).map_err(|e| format!("删除失败: {e}"))?;
        removed = true;
    }
    if sh_path.exists() {
        let _ = fs::remove_file(&sh_path);
    }
    if removed {
        Ok(format!("已删除热注册工具 {name}"))
    } else {
        Err(format!("热注册工具 {name} 不存在"))
    }
}

pub fn list(cfg: &Config) -> Vec<DynamicTool> {
    let dir = tools_dir(cfg);
    let mut out: Vec<DynamicTool> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|x| x == "json"))
                .filter_map(|entry| fs::read_to_string(entry.path()).ok())
                .filter_map(|text| serde_json::from_str::<DynamicTool>(&text).ok())
                .filter(|tool| valid_name(&tool.name))
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

pub fn list_index_line(cfg: &Config) -> String {
    let tools = list(cfg);
    if tools.is_empty() {
        format!("[{CUSTOM_CATEGORY}] 热注册工具（0）: 用 tools 分类中的 make_tools 创建\n")
    } else {
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        format!("[{CUSTOM_CATEGORY}] 热注册工具（{}）: {}\n", tools.len(), names.join(", "))
    }
}

pub fn list_detail(cfg: &Config) -> String {
    let tools = list(cfg);
    if tools.is_empty() {
        return "[custom] 还没有热注册工具。先查看 list_tools {\"category\":\"tools\"} 中的 make_tools。\n".into();
    }
    let mut out = format!("[custom] 热注册工具（{}，修改后下一次调用立即生效）:\n", tools.len());
    for tool in tools {
        out.push_str(&format!(
            "- {} — {}\n  args schema: {}\n  timeout: {}s\n",
            tool.name,
            tool.description,
            serde_json::to_string(&tool.parameters).unwrap_or_default(),
            tool.timeout_secs
        ));
    }
    out
}

pub fn invocation_command(cfg: &Config, tool: &DynamicTool, args: &Value) -> Result<String, String> {
    validate_invocation(tool, args)?;
    let path = tool_path(cfg, &tool.name);
    if !path.exists() {
        return Err(format!("热注册工具 {} 的定义文件已消失，请重新注册", tool.name));
    }
    
    
    let script_path = script_path(cfg, &tool.name);
    write_script_atomic(&script_path, &tool.script)?;
    let args_json = serde_json::to_string(args).map_err(|e| format!("参数序列化失败: {e}"))?;
    Ok(if tool.language == "python3" {
        // python3 脚本直接用 sys.argv[1] 拿到 JSON 参数，省掉 heredoc 模板
        format!(
            "python3 {} {}",
            shell_quote(&script_path.to_string_lossy()),
            shell_quote(&args_json)
        )
    } else {
        format!(
            "sh {} {}",
            shell_quote(&script_path.to_string_lossy()),
            shell_quote(&args_json)
        )
    })
}

fn validate_invocation(tool: &DynamicTool, args: &Value) -> Result<(), String> {
    let object = args.as_object().ok_or_else(|| {
        format!("工具 {} 的 args 必须是 JSON object", tool.name)
    })?;
    let properties = tool
        .parameters
        .get("properties")
        .and_then(Value::as_object)
        .ok_or("已注册工具的 parameters.properties 损坏，请重新注册")?;
    let unknown: Vec<&str> = object
        .keys()
        .filter(|key| !properties.contains_key(*key))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "工具 {} 收到未注册参数: {}。允许参数: {}",
            tool.name,
            unknown.join(", "),
            properties.keys().map(String::as_str).collect::<Vec<_>>().join(", ")
        ));
    }
    if let Some(required) = tool.parameters.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(name) {
                return Err(format!("工具 {} 缺少必填参数 {name}", tool.name));
            }
        }
    }
    for (name, value) in object {
        let expected = properties
            .get(name)
            .and_then(|property| property.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let matches = match expected {
            "string" => value.is_string(),
            "number" => value.is_number(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "boolean" => value.is_boolean(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            _ => false,
        };
        if !matches {
            return Err(format!(
                "工具 {} 参数 {name} 类型错误：需要 {expected}，收到 {}",
                tool.name,
                json_type(value)
            ));
        }
    }
    Ok(())
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn validate_spec(spec: &Value, reserved: &[&str]) -> Vec<String> {
    let mut errors = Vec::new();
    let Some(obj) = spec.as_object() else {
        return vec!["顶层必须是 JSON object（也可放在 tool 字段内）".into()];
    };
    let name = obj.get("name").and_then(Value::as_str).unwrap_or("");
    if !valid_name(name) {
        errors.push("name 必须匹配 [a-z][a-z0-9_]{2,47}".into());
    } else if reserved.contains(&name) {
        errors.push(format!("name={name} 与内建工具重名，禁止覆盖"));
    }
    let desc = obj.get("description").and_then(Value::as_str).unwrap_or("").trim();
    let desc_len = desc.chars().count();
    if !(20..=MAX_DESCRIPTION_CHARS).contains(&desc_len) {
        errors.push(format!("description 必须为 20-{MAX_DESCRIPTION_CHARS} 字符，并说明何时调用、做什么、返回什么"));
    }
    match obj.get("script").and_then(Value::as_str) {
        Some(script) if !script.trim().is_empty() && script.len() <= MAX_SCRIPT_BYTES && !script.contains('\0') => {}
        Some(script) if script.len() > MAX_SCRIPT_BYTES => errors.push(format!("script 超过 {MAX_SCRIPT_BYTES} 字节")),
        _ => errors.push("script 必须是非空 shell 脚本；参数 JSON 在 $1".into()),
    }
    if let Some(timeout) = obj.get("timeout_secs") {
        if !timeout.as_u64().is_some_and(|n| (1..=600).contains(&n)) {
            errors.push("timeout_secs 必须是 1-600 的整数".into());
        }
    }
    match obj.get("parameters") {
        Some(schema) => validate_parameters(schema, &mut errors),
        None => errors.push("缺少 parameters JSON Schema".into()),
    }
    errors
}

fn validate_parameters(schema: &Value, errors: &mut Vec<String>) {
    let Some(obj) = schema.as_object() else {
        errors.push("parameters 必须是 JSON object".into());
        return;
    };
    if obj.get("type").and_then(Value::as_str) != Some("object") {
        errors.push("parameters.type 必须是 object".into());
    }
    let Some(properties) = obj.get("properties").and_then(Value::as_object) else {
        errors.push("parameters.properties 必须是 object（无参数时用 {}）".into());
        return;
    };
    for (name, property) in properties {
        if !valid_argument_name(name) {
            errors.push(format!("参数名 {name:?} 只能含字母、数字、下划线且不能以数字开头"));
        }
        let Some(p) = property.as_object() else {
            errors.push(format!("参数 {name} 的定义必须是 object"));
            continue;
        };
        let ty = p.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(ty, "string" | "number" | "integer" | "boolean" | "array" | "object") {
            errors.push(format!("参数 {name} 的 type 不受支持: {ty:?}"));
        }
        if p.get("description").and_then(Value::as_str).is_none_or(|s| s.trim().is_empty()) {
            errors.push(format!("参数 {name} 缺少 description"));
        }
    }
    if obj.get("additionalProperties").and_then(Value::as_bool) != Some(false) {
        errors.push("parameters.additionalProperties 必须显式为 false，防止弱模型乱塞参数".into());
    }
    if let Some(required) = obj.get("required") {
        let Some(items) = required.as_array() else {
            errors.push("parameters.required 必须是字符串数组".into());
            return;
        };
        for item in items {
            let Some(name) = item.as_str() else {
                errors.push("parameters.required 只能包含字符串".into());
                continue;
            };
            if !properties.contains_key(name) {
                errors.push(format!("required 中的 {name} 不存在于 properties"));
            }
        }
    }
}

fn correction_error(errors: &[String]) -> String {
    let numbered = errors
        .iter()
        .enumerate()
        .map(|(i, e)| format!("{}. {e}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "make_tools 注册被强制校验器拒绝，请逐项修正后重试：\n{numbered}\n\n正确最小示例：\n{{\"name\":\"hello_user\",\"description\":\"接收姓名并输出一句问候；需要生成确定性问候文本时调用，返回纯文本。\",\"parameters\":{{\"type\":\"object\",\"properties\":{{\"name\":{{\"type\":\"string\",\"description\":\"要问候的姓名\"}}}},\"required\":[\"name\"],\"additionalProperties\":false}},\"script\":\"#!/bin/sh\\nprintf '%s\\n' \\\"$1\\\"\",\"timeout_secs\":30}}"
    )
}

fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (3..=48).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

fn valid_argument_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn normalize_script(script: &str) -> String {
    let mut normalized = script.replace("\r\n", "\n").replace('\r', "\n");
    if !normalized.ends_with('\n') {
        normalized.push('\n');
    }
    normalized
}

fn tools_dir(cfg: &Config) -> PathBuf {
    cfg.data_dir().join("tools")
}

fn tool_path(cfg: &Config, name: &str) -> PathBuf {
    tools_dir(cfg).join(format!("{name}.json"))
}

fn script_path(cfg: &Config, name: &str) -> PathBuf {
    tools_dir(cfg).join(format!("{name}.sh"))
}

fn save_atomic(cfg: &Config, tool: &DynamicTool) -> Result<(), String> {
    let dir = tools_dir(cfg);
    fs::create_dir_all(&dir).map_err(|e| format!("创建工具目录失败: {e}"))?;
    let path = tool_path(cfg, &tool.name);
    let tmp = dir.join(format!(".{}-{}-{}.tmp", tool.name, std::process::id(), now_ms()));
    let text = serde_json::to_string_pretty(tool).map_err(|e| e.to_string())?;
    fs::write(&tmp, text).map_err(|e| format!("写工具定义失败: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("提交工具定义失败: {e}")
    })
}

fn write_script_atomic(path: &Path, script: &str) -> Result<(), String> {
    let dir = path.parent().ok_or("脚本路径缺少父目录")?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(".script-{}-{}.tmp", std::process::id(), now_ms()));
    fs::write(&tmp, script).map_err(|e| format!("写脚本失败: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("提交脚本失败: {e}")
    })
}

fn shell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_config(tag: &str) -> (Config, PathBuf) {
        let dir = std::env::temp_dir().join(format!("yjlcoder-tools-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut cfg = Config::default();
        cfg.set_test_data_dir(dir.clone());
        (cfg, dir)
    }

    fn valid_spec() -> Value {
        json!({
            "name":"hello_user",
            "description":"接收姓名并输出一句问候；需要生成确定性问候文本时调用，返回纯文本。",
            "parameters":{
                "type":"object",
                "properties":{"name":{"type":"string","description":"要问候的姓名"}},
                "required":["name"],
                "additionalProperties":false
            },
            "script":"#!/bin/sh\nprintf '%s\\n' \"$1\"",
            "timeout_secs":30
        })
    }

    #[test]
    fn strong_validation_returns_actionable_corrections() {
        let (cfg, dir) = temp_config("invalid");
        let error = register(&cfg, &json!({"name":"X"}), &["readline"]).unwrap_err();
        assert!(error.contains("逐项修正"));
        assert!(error.contains("additionalProperties"));
        assert!(error.contains("正确最小示例"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn register_is_immediately_visible_without_restart() {
        let (cfg, dir) = temp_config("hot");
        register(&cfg, &valid_spec(), &["readline"]).unwrap();
        assert!(list_detail(&cfg).contains("hello_user"));
        let loaded = load(&cfg, "hello_user").unwrap();
        let cmd = invocation_command(&cfg, &loaded, &json!({"name":"小宇"})).unwrap();
        assert!(cmd.contains("hello_user.sh"));
        assert!(cmd.contains("小宇"));
        assert!(invocation_command(&cfg, &loaded, &json!({"wrong":1}))
            .unwrap_err()
            .contains("未注册参数"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn builtins_cannot_be_shadowed() {
        let (cfg, dir) = temp_config("reserved");
        let mut spec = valid_spec();
        spec["name"] = json!("readline");
        assert!(register(&cfg, &spec, &["readline"]).unwrap_err().contains("重名"));
        let _ = fs::remove_dir_all(dir);
    }
}
