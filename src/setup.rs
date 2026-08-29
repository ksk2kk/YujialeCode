use crate::backend::{Capabilities, discover};
use crate::config::Config;
use crate::tools::{FormFieldKind, FormFieldSpec};
use serde_json::Value;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::time::Duration;

pub const PROVIDER_FORM_TITLE: &str = "模型服务配置";
pub const KEY_PROVIDER: &str = "provider";
pub const KEY_BASE_URL: &str = "base_url";
pub const KEY_API_KEY: &str = "api_key";
pub const KEY_CLEAR_KEY: &str = "clear_api_key";
pub const KEY_MODEL: &str = "model";
pub const KEY_MODEL_CUSTOM: &str = "model_custom";
pub const KEY_AGENTS: &str = "agents";
pub const LOCAL_CANDIDATES: &[(&str, &str)] = &[
    ("http://127.0.0.1:8080", "llama.cpp"),
    ("http://127.0.0.1:11434", "Ollama"),
    ("http://127.0.0.1:1234", "LM Studio"),
];
pub struct LocalEndpoint {
    pub origin: String,
    pub label: String,
    pub caps: Capabilities,
}
impl LocalEndpoint {
    fn summary(&self) -> String {
        let mut parts = vec![self.label.clone()];
        if let Some(n) = self.caps.n_ctx {
            parts.push(format!("上下文 {n}"));
        } else if self.caps.is_llamacpp() {
            parts.push("上下文需 API 密钥（配置后自动探测）".to_string());
        }
        if !self.caps.modalities.is_empty() {
            parts.push(format!("模态 {}", self.caps.modalities.join("/")));
        }
        parts.join(" · ")
    }
    fn model_names(&self) -> Vec<String> {
        let mut names = self.caps.model_id.iter().cloned().collect::<Vec<_>>();
        if names.is_empty() {
            names.push("（服务端默认）".to_string());
        }
        names
    }
}
pub fn probe_local_endpoints(api_key: &str) -> Vec<LocalEndpoint> {
    probe_candidates(LOCAL_CANDIDATES, api_key)
}
fn health_ok(origin: &str) -> bool {
    match ureq::get(&format!("{origin}/health"))
        .timeout(Duration::from_secs(2))
        .call()
    {
        Ok(r) => r.status() == 200,
        Err(_) => false,
    }
}
pub fn needs_setup(cfg: &Config) -> bool {
    let base = cfg.provider.base_url.trim();
    base.is_empty() || (base == "https://api.deepseek.com" && cfg.provider.api_key.trim().is_empty())
}
pub fn run_wizard(cfg: &mut Config) {
    let stdin = std::io::stdin();
    run_wizard_on(cfg, &mut stdin.lock(), LOCAL_CANDIDATES);
    cfg.save();
}
fn run_wizard_on(cfg: &mut Config, input: &mut dyn BufRead, candidates: &[(&str, &str)]) {
    println!();
    println!("==============================================");
    println!("  Yujiale Code 配置向导");
    println!("  配置将写入 {}", crate::config::config_path().display());
    println!("  提示: 模型服务的最大 token / 上下文会自动从服务端探测，");
    println!("        不需要手动配置。");
    println!("==============================================");
    let endpoints = probe_candidates(candidates, &cfg.provider.api_key);
    if !endpoints.is_empty() {
        println!("\n发现本地模型服务:");
        for (i, ep) in endpoints.iter().enumerate() {
            println!("  {}  {} ({})", i + 1, ep.summary(), ep.origin);
        }
        let first_default = if needs_setup(cfg) { 1 } else { 0 };
        let choice = prompt_line(
            &format!("选择要使用的服务（回车默认 {}；已配置时 0 = 保持不变）", first_default),
            "",
            input,
        );
        let idx = if choice.trim().is_empty() {
            first_default
        } else {
            match choice.trim().parse::<usize>() {
                Ok(n) if n >= 1 && n <= endpoints.len() => n,
                _ => {
                    println!("  无效编号，使用第一个");
                    1
                }
            }
        };
        if idx == 0 {
            println!("保持现有配置。");
            if !needs_setup(cfg) {
                return;
            }
        } else {
            let ep = &endpoints[idx - 1];                                   
            let names = ep.model_names();
            let model = if names.len() > 1 {
                println!("  该服务可用模型:");
                for (j, m) in names.iter().enumerate() {
                    println!("    {}  {m}", j + 1);
                }
                let m_choice = prompt_line("选择模型（回车默认 1）", "", input);
                match m_choice.trim().parse::<usize>() {
                    Ok(n) if n >= 1 && n <= names.len() => names[n - 1].clone(),
                    _ => names[0].clone(),
                }
            } else {
                names[0].clone()
            };
            apply_endpoint(cfg, ep, &model);
            if ep.caps.auth_required && cfg.provider.api_key.is_empty() {
                let key = prompt_line(
                    "该服务需要 API 密钥（llama.cpp --api-key），请输入（留空则之后用 /apikey 设置）",
                    "",
                    input,
                );
                if !key.trim().is_empty() {
                    cfg.provider.api_key = key.trim().to_string();
                }
            }
            println!("\n已配置: {}（模型 {}）", cfg.provider.base_url, cfg.provider.model);
            return;                                        
        }
    }
    if !needs_setup(cfg) {
        println!("配置已存在且未修改。可在 TUI 内用 /server /model /apikey 调整，或编辑 ~/.yjlcoder/config.json");
        return;
    }
    let cc = import_claude_code();
    if cc.base_url.is_some() {
        println!("\n检测到 Claude Code 配置（环境变量 / ~/.claude/settings.json / ~/.claude.json）:");
        if let Some(u) = &cc.base_url {
            println!("  端点: {u}");
        }
        if let Some(m) = &cc.model {
            println!("  模型: {m}");
        }
        println!("  （仅读取并转换为 Yujiale Code 配置，Claude Code 的文件不会被修改）");
        if prompt_yes_no("使用该配置？", true, input) {
            apply_cc(cfg, &cc);
            println!("\n已导入 Claude Code 配置。");
            return;
        }
    }
    manual_input(cfg, input);
}
fn probe_candidates(candidates: &[(&str, &str)], api_key: &str) -> Vec<LocalEndpoint> {
    candidates
        .iter()
        .filter_map(|(origin, label)| {
            let caps = discover(origin, api_key);
            if !caps.is_llamacpp() && !health_ok(origin) {
                return None;
            }
            Some(LocalEndpoint { origin: (*origin).to_string(), label: label.to_string(), caps })
        })
        .collect()
}
#[allow(dead_code)]
pub fn quick_setup(cfg: &mut Config) -> String {
    let mut lines = Vec::new();
    let endpoints = probe_local_endpoints(&cfg.provider.api_key);
    if !endpoints.is_empty() {
        let ep = &endpoints[0];
        apply_endpoint(cfg, ep, &ep.model_names()[0]);
        lines.push(format!(
            "已自动配置本地服务 {}（{}，模型 {}）",
            ep.origin, ep.summary(), cfg.provider.model
        ));
        if ep.caps.auth_required && cfg.provider.api_key.is_empty() {
            lines.push("该服务需要 API 密钥（llama.cpp --api-key），请用 /apikey 设置".to_string());
        }
        if endpoints.len() > 1 {
            let others: Vec<&str> = endpoints[1..].iter().map(|e| e.origin.as_str()).collect();
            lines.push(format!(
                "还发现 {}，如需要切换端点请退出 TUI 运行 yjlcoder --setup 或使用 /server",
                others.join("、")
            ));
        }
    } else {
        let cc = import_claude_code();
        if cc.base_url.is_some() {
            apply_cc(cfg, &cc);
            lines.push(format!("已导入 Claude Code 配置: {}", cfg.provider.base_url));
            if let Some(m) = &cc.model {
                lines.push(format!("模型: {m}"));
            }
        } else {
            lines.push(
                "未发现本地模型服务或 Claude Code 配置。交互式向导请退出 TUI 后运行 yjlcoder --setup；\
                 也可用 /server /model /apikey 手动逐项配置"
                    .to_string(),
            );
        }
    }
    cfg.save();
    lines.join("\n")
}

/// Provider 配置表单（照 grok elicitation 表单：字段编辑 + 保存/取消动作区）。
/// 首次启动与 `/config` 共用同一份字段定义。
pub fn provider_form_specs(cfg: &Config) -> Vec<FormFieldSpec> {
    let has_key = !cfg.provider.api_key.trim().is_empty();
    let model_index = match cfg.provider.model.as_str() {
        "deepseek-v4-pro" => 1,
        "deepseek-v4-flash-vision-exp" => 2,
        _ => 0,
    };
    vec![
        FormFieldSpec {
            key: KEY_PROVIDER.into(),
            title: "模型供应商".into(),
            hint: "←/→ 或回车切换；本地服务自动扫描 llama.cpp/Ollama/LM Studio".into(),
            kind: FormFieldKind::Choice {
                options: vec![
                    "DeepSeek".into(),
                    "本地模型（自动探测）".into(),
                    "自定义 OpenAI 兼容地址".into(),
                ],
                index: if cfg.provider.base_url.contains("deepseek") {
                    0
                } else if cfg.provider.base_url.starts_with("http://127.0.0.1")
                    || cfg.provider.base_url.starts_with("http://localhost")
                {
                    1
                } else {
                    2
                },
            },
        },
        FormFieldSpec {
            key: KEY_BASE_URL.into(),
            title: "服务地址 base_url".into(),
            hint: "仅自定义供应商时必填（http/https）".into(),
            kind: FormFieldKind::Text {
                masked: false,
                placeholder: "https://api.example.com/v1".into(),
            },
        },
        FormFieldSpec {
            key: KEY_API_KEY.into(),
            title: "API Key".into(),
            hint: if has_key {
                "已有密钥（不回显）；留空表示保留".into()
            } else {
                "本地服务可留空".into()
            },
            kind: FormFieldKind::Text {
                masked: true,
                placeholder: String::new(),
            },
        },
        FormFieldSpec {
            key: KEY_CLEAR_KEY.into(),
            title: "清除已存密钥".into(),
            hint: "空格切换；删除本机保存的供应商密钥".into(),
            kind: FormFieldKind::Toggle { on: false },
        },
        FormFieldSpec {
            key: KEY_MODEL.into(),
            title: "主模型".into(),
            hint: "flash=速度优先 / pro=质量优先 / vision=截图与图像".into(),
            kind: FormFieldKind::Choice {
                options: vec![
                    "deepseek-v4-flash".into(),
                    "deepseek-v4-pro".into(),
                    "deepseek-v4-flash-vision-exp".into(),
                ],
                index: model_index,
            },
        },
        FormFieldSpec {
            key: KEY_MODEL_CUSTOM.into(),
            title: "自定义模型名".into(),
            hint: "本地或其他供应商时填写；留空用上方选择".into(),
            kind: FormFieldKind::Text {
                masked: false,
                placeholder: String::new(),
            },
        },
        FormFieldSpec {
            key: KEY_AGENTS.into(),
            title: "受限后台 Agent".into(),
            hint: "本地模型始终排队执行，不与主会话抢推理资源".into(),
            kind: FormFieldKind::Toggle { on: cfg.agents.enabled },
        },
    ]
}

/// Applies a completed provider form atomically. Validation happens before
/// `save`, so a bad custom URL or missing cloud key never destroys a working
/// configuration.
pub fn apply_provider_answers(
    cfg: &mut Config,
    answers: &BTreeMap<String, String>,
) -> Result<String, String> {
    let provider = answers
        .get(KEY_PROVIDER)
        .map(String::as_str)
        .map(str::trim)
        .ok_or_else(|| format!("配置表缺少答案: {KEY_PROVIDER}"))?;
    let mut next = cfg.clone();

    if answers.get(KEY_CLEAR_KEY).map(String::as_str) == Some("1") {
        next.provider.api_key.clear();
    }
    let api_key = answers
        .get(KEY_API_KEY)
        .map(String::as_str)
        .map(str::trim)
        .unwrap_or("");
    if !api_key.is_empty() {
        next.provider.api_key = api_key.to_string();
    }

    match provider {
        "DeepSeek" => next.provider.base_url = "https://api.deepseek.com".into(),
        "本地模型（自动探测）" => {
            let endpoints = probe_local_endpoints(&next.provider.api_key);
            let endpoint = endpoints.first().ok_or(
                "没有发现本地模型服务。请先启动 llama.cpp/Ollama/LM Studio，或改用自定义地址",
            )?;
            apply_endpoint(&mut next, endpoint, &endpoint.model_names()[0]);
        }
        _ => {
            let custom = answers
                .get(KEY_BASE_URL)
                .map(String::as_str)
                .map(str::trim)
                .unwrap_or("");
            if custom.starts_with("http://") || custom.starts_with("https://") {
                next.provider.base_url = custom.trim_end_matches('/').to_string();
            } else {
                return Err(
                    "自定义供应商需要 http(s) 的 base_url；或改选 DeepSeek / 本地自动探测".into(),
                );
            }
        }
    }

    let model = answers
        .get(KEY_MODEL_CUSTOM)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
        .or_else(|| {
            answers
                .get(KEY_MODEL)
                .map(String::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(String::from)
        });
    next.provider.model = match model {
        Some(value) => value,
        None => return Err("模型名不能为空".into()),
    };
    next.agents.enabled = answers
        .get(KEY_AGENTS)
        .map(String::as_str)
        .map(|value| value == "1")
        .unwrap_or(next.agents.enabled);
    next.agents.max_concurrent = 1;
    next.agents.max_spawn_per_turn = 1;
    if next.provider.base_url.contains("api.deepseek.com") {
        if next.provider.api_key.trim().is_empty() {
            return Err("DeepSeek 需要 API Key；请在 API Key 字段中输入".into());
        }
        next.pricing = crate::config::Pricing::deepseek_flash_cny();
        next.agents.model = "deepseek-v4-flash".into();
        next.provider.ctx_window = crate::backend::DEEPSEEK_V4_CONTEXT_WINDOW;
        next.provider.ctx_override = None;
    }
    next.save();
    *cfg = next;
    Ok(format!(
        "配置完成：{} · {} · 后台 Agent {}",
        cfg.provider.base_url,
        cfg.provider.model,
        if cfg.agents.enabled { "开启（1 个并发）" } else { "关闭" }
    ))
}
fn apply_endpoint(cfg: &mut Config, ep: &LocalEndpoint, model: &str) {
    cfg.provider.base_url = format!("{}/v1", ep.origin);
    cfg.provider.model = model.trim().to_string();
    if cfg.provider.model == "（服务端默认）" {
        cfg.provider.model.clear();
    }
}
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ClaudeCodeConfig {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
}
pub fn import_claude_code() -> ClaudeCodeConfig {
    let mut out = ClaudeCodeConfig::default();
    if let Some(v) = env::var("ANTHROPIC_BASE_URL").ok() {
        if !v.trim().is_empty() {
            out.base_url = Some(anthropic_to_openai(v.trim()));
        }
    }
    if let Some(v) = env::var("ANTHROPIC_AUTH_TOKEN").ok() {
        if !v.trim().is_empty() {
            out.api_key = Some(v.trim().to_string());
        }
    }
    if let Some(v) = env::var("ANTHROPIC_MODEL").ok() {
        if !v.trim().is_empty() {
            out.model = Some(v.trim().to_string());
        }
    }
    if let Ok(home) = env::var("HOME") {
        let home = PathBuf::from(home);
        fill_env_block(&mut out, &read_env_block(&home.join(".claude").join("settings.json")));
        fill_env_block(&mut out, &read_env_block(&home.join(".claude.json")));
    }
    out
}
fn read_env_block(path: &PathBuf) -> Option<Value> {
    let s = fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&s).ok()?;
    v.get("env").cloned()
}
fn fill_env_block(out: &mut ClaudeCodeConfig, env: &Option<Value>) {
    let Some(env) = env else { return };
    if out.base_url.is_none() {
        if let Some(v) = env.get("ANTHROPIC_BASE_URL").and_then(Value::as_str) {
            if !v.trim().is_empty() {
                out.base_url = Some(anthropic_to_openai(v));
            }
        }
    }
    if out.api_key.is_none() {
        if let Some(v) = env.get("ANTHROPIC_AUTH_TOKEN").and_then(Value::as_str) {
            if !v.trim().is_empty() {
                out.api_key = Some(v.to_string());
            }
        }
    }
    if out.model.is_none() {
        if let Some(v) = env.get("ANTHROPIC_MODEL").and_then(Value::as_str) {
            if !v.trim().is_empty() {
                out.model = Some(v.to_string());
            }
        }
    }
}
pub fn anthropic_to_openai(base: &str) -> String {
    let b = base.trim_end_matches('/');
    let b = b.strip_suffix("/v1/messages").unwrap_or(b);
    let b = b.strip_suffix("/chat/completions").unwrap_or(b);
    let b = b.strip_suffix("/v1").unwrap_or(b);
    format!("{b}/v1")
}
fn apply_cc(cfg: &mut Config, cc: &ClaudeCodeConfig) {
    if let Some(u) = &cc.base_url {
        cfg.provider.base_url = u.clone();
    }
    if let Some(k) = &cc.api_key {
        cfg.provider.api_key = k.clone();
    }
    if let Some(m) = &cc.model {
        cfg.provider.model = m.clone();
    }
}
fn manual_input(cfg: &mut Config, input: &mut dyn BufRead) {
    println!("\n手动配置模型服务:");
    let default_url = if cfg.provider.base_url.trim().is_empty() {
        "http://127.0.0.1:8080/v1"
    } else {
        &cfg.provider.base_url
    };
    cfg.provider.base_url = prompt_line(
        &format!("模型服务地址 base_url（如 {default_url}、https://api.deepseek.com）"),
        default_url,
        input,
    );
    let default_model = if cfg.provider.model.is_empty() || cfg.provider.model == "deepseek-v4-flash" {
        ""
    } else {
        &cfg.provider.model
    };
    cfg.provider.model = prompt_line("模型名 model（本地单模型服务可留空）", default_model, input);
    cfg.provider.api_key = prompt_line(
        "API 密钥 api_key（本地无鉴权可直接回车跳过）",
        "",
        input,
    );
    println!("\n配置已保存。运行 yjlcoder 开始使用；--mock 可离线体验。\n");
}
fn prompt_line(label: &str, default: &str, input: &mut dyn BufRead) -> String {
    let suffix = if default.is_empty() { "" } else { &format!("（回车默认 {default}）") };
    print!("{label}{suffix}: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = input.read_line(&mut line);
    let t = line.trim();                                      
    if t.is_empty() {
        default.to_string()
    } else {
        t.to_string()
    }
}
fn prompt_yes_no(label: &str, default_yes: bool, input: &mut dyn BufRead) -> bool {
    let hint = if default_yes { "Y/n" } else { "y/N" };
    print!("{label}（{hint}）: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = input.read_line(&mut line);
    match line.trim().to_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        "" => default_yes,
        _ => default_yes,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "yjlcoder-setup-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn clear_anthropic_env() -> Vec<(String, Option<String>)> {
        ["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_MODEL"]
            .iter()
            .map(|k| {
                let old = env::var(k).ok();
                env::remove_var(k);
                (k.to_string(), old)
            })
            .collect()
    }
    fn restore_env(saved: &[(String, Option<String>)]) {
        for (k, v) in saved {
            match v {
                Some(v) => env::set_var(k, v),
                None => env::remove_var(k),
            }
        }
    }
    #[test]
    fn needs_setup_trigger_rules() {
        let mut cfg = Config::default();
        assert!(needs_setup(&cfg));
        cfg.provider.api_key = "sk-x".into();
        assert!(!needs_setup(&cfg));
        let mut cfg = Config::default();
        cfg.provider.base_url = "http://127.0.0.1:8080/v1".into();
        assert!(!needs_setup(&cfg));
        let mut cfg = Config::default();
        cfg.provider.base_url = "".into();
        assert!(needs_setup(&cfg));
    }
    #[test]
    fn anthropic_url_normalization() {
        assert_eq!(anthropic_to_openai("http://127.0.0.1:8080"), "http://127.0.0.1:8080/v1");
        assert_eq!(anthropic_to_openai("http://127.0.0.1:8080/"), "http://127.0.0.1:8080/v1");
        assert_eq!(
            anthropic_to_openai("http://127.0.0.1:8080/v1/messages"),
            "http://127.0.0.1:8080/v1"
        );
        assert_eq!(
            anthropic_to_openai("https://proxy.example.com/chat/completions"),
            "https://proxy.example.com/v1"
        );
        assert_eq!(
            anthropic_to_openai("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1"
        );
        assert_eq!(
            anthropic_to_openai("http://127.0.0.1:8080/v1"),
            "http://127.0.0.1:8080/v1"
        );
    }
    #[test]
    fn import_from_settings_json_and_claude_json() {
        let _guard = env_guard();
        let home = tmp_dir("home");
        let claude = home.join(".claude");
        fs::create_dir_all(&claude).unwrap();
        fs::write(
            claude.join("settings.json"),
            r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:8080/v1/messages","ANTHROPIC_AUTH_TOKEN":"tok-settings","ANTHROPIC_MODEL":"claude-sonnet-4-6"}}"#,
        )
        .unwrap();
        fs::write(home.join(".claude.json"), r#"{"env":{"ANTHROPIC_BASE_URL":"http://elsewhere:9999"}}"#).unwrap();
        let old_home = env::var("HOME").ok();
        let saved_env = clear_anthropic_env();
        env::set_var("HOME", &home);
        let cc = import_claude_code();
        restore_env(&saved_env);
        if let Some(h) = old_home {
            env::set_var("HOME", h);
        } else {
            env::remove_var("HOME");
        }
        assert_eq!(cc.base_url.as_deref(), Some("http://127.0.0.1:8080/v1"));
        assert_eq!(cc.api_key.as_deref(), Some("tok-settings"));
        assert_eq!(cc.model.as_deref(), Some("claude-sonnet-4-6"));
    }
    #[test]
    fn import_from_claude_json_when_no_settings() {
        let _guard = env_guard();
        let home = tmp_dir("home2");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join(".claude.json"), r#"{"env":{"ANTHROPIC_BASE_URL":"http://proxy:8000","ANTHROPIC_AUTH_TOKEN":"tok-json"}}"#)
            .unwrap();
        let old_home = env::var("HOME").ok();
        let saved_env = clear_anthropic_env();
        env::set_var("HOME", &home);
        let cc = import_claude_code();
        restore_env(&saved_env);
        if let Some(h) = old_home {
            env::set_var("HOME", h);
        } else {
            env::remove_var("HOME");
        }
        assert_eq!(cc.base_url.as_deref(), Some("http://proxy:8000/v1"));
        assert_eq!(cc.api_key.as_deref(), Some("tok-json"));
        assert_eq!(cc.model, None);
    }
    #[test]
    fn import_empty_when_nothing_configured() {
        let _guard = env_guard();
        let home = tmp_dir("home3");
        fs::create_dir_all(&home).unwrap();
        let old_home = env::var("HOME").ok();
        let saved_env = clear_anthropic_env();
        env::set_var("HOME", &home);
        let cc = import_claude_code();
        restore_env(&saved_env);
        if let Some(h) = old_home {
            env::set_var("HOME", h);
        } else {
            env::remove_var("HOME");
        }
        assert_eq!(cc, ClaudeCodeConfig::default());
    }
    fn serve_mock_llamacpp() -> u16 {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for body in [
                r#"{"default_generation_settings":{"n_ctx":131072,"params":{"max_tokens":-1}},"model_alias":"mock-model","chat_template_caps":{"supports_tools":true},"modalities":{"text":true}}"#,
                r#"{"data":[{"id":"mock-model","meta":{"n_ctx":131072,"n_ctx_train":262144}}]}"#,
                r#"{"data":[{"id":"mock-model","status":{"value":"loaded"}}]}"#,
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes());
            }
        });
        port
    }
    #[test]
    fn wizard_discovers_local_llamacpp_and_applies() {
        let port = serve_mock_llamacpp();
        let mut cfg = Config::default();
        let origin = format!("http://127.0.0.1:{port}");
        let mut input: &[u8] = b"\n\n";
        let candidates = [(origin.as_str(), "mock-llamacpp")];
        run_wizard_on(&mut cfg, &mut input, &candidates);
        assert_eq!(cfg.provider.base_url, format!("{origin}/v1"));
        assert_eq!(cfg.provider.model, "mock-model");
    }
    fn serve_auth_llamacpp() -> u16 {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for (status, body) in [
                (401u16, r#"{"error":{"message":"Invalid API Key"}}"#),
                (200u16, r#"{"data":[{"id":"auth-model","meta":{"n_ctx":131072,"n_ctx_train":262144}}]}"#),
            ] {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf);
                let reason = if status == 200 { "OK" } else { "UNAUTHORIZED" };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes());
            }
        });
        port
    }
    #[test]
    fn wizard_prompts_api_key_when_server_requires_auth() {
        let _guard = env_guard();
        let saved_env = clear_anthropic_env();
        let port = serve_auth_llamacpp();
        let mut cfg = Config::default();
        cfg.provider.base_url = "".into();               
        let origin = format!("http://127.0.0.1:{port}");
        let mut input: &[u8] = b"\nsk-wizard-key\n";                             
        let candidates = [(origin.as_str(), "auth-llamacpp")];
        run_wizard_on(&mut cfg, &mut input, &candidates);
        restore_env(&saved_env);
        assert_eq!(cfg.provider.base_url, format!("{origin}/v1"));
        assert_eq!(cfg.provider.model, "auth-model");
        assert_eq!(cfg.provider.api_key, "sk-wizard-key");
    }
    #[test]
    fn wizard_manual_input_when_nothing_found() {
        let _guard = env_guard();
        let saved_env = clear_anthropic_env();
        let mut cfg = Config::default();
        let mut input: &[u8] = b"http://10.0.0.5:8080/v1\nmy-model\nsk-test\n";
        run_wizard_on(&mut cfg, &mut input, &[]);
        restore_env(&saved_env);
        assert_eq!(cfg.provider.base_url, "http://10.0.0.5:8080/v1");
        assert_eq!(cfg.provider.model, "my-model");
        assert_eq!(cfg.provider.api_key, "sk-test");
    }
    #[test]
    fn wizard_applies_claude_code_import() {
        let _guard = env_guard();
        let home = tmp_dir("home4");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(
            home.join(".claude/settings.json"),
            r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:8080","ANTHROPIC_AUTH_TOKEN":"tok","ANTHROPIC_MODEL":"m1"}}"#,
        )
        .unwrap();
        let old_home = env::var("HOME").ok();
        let saved_env = clear_anthropic_env();
        env::set_var("HOME", &home);
        let mut cfg = Config::default();
        let mut input: &[u8] = b"y\n";        
        run_wizard_on(&mut cfg, &mut input, &[]);
        restore_env(&saved_env);
        if let Some(h) = old_home {
            env::set_var("HOME", h);
        } else {
            env::remove_var("HOME");
        }
        assert_eq!(cfg.provider.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(cfg.provider.api_key, "tok");
        assert_eq!(cfg.provider.model, "m1");
    }
    #[test]
    fn wizard_rejects_cc_import_then_manual() {
        let _guard = env_guard();
        let home = tmp_dir("home5");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(
            home.join(".claude/settings.json"),
            r#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:8080"}}"#,
        )
        .unwrap();
        let old_home = env::var("HOME").ok();
        let saved_env = clear_anthropic_env();
        env::set_var("HOME", &home);
        let mut cfg = Config::default();
        let mut input: &[u8] = b"n\nhttp://127.0.0.1:9000/v1\nmm\n";             
        run_wizard_on(&mut cfg, &mut input, &[]);
        restore_env(&saved_env);
        if let Some(h) = old_home {
            env::set_var("HOME", h);
        } else {
            env::remove_var("HOME");
        }
        assert_eq!(cfg.provider.base_url, "http://127.0.0.1:9000/v1");
        assert_eq!(cfg.provider.model, "mm");
    }
    #[test]
    fn provider_form_applies_deepseek_atomically() {
        let dir = tmp_dir("provider-form");
        let mut cfg = Config::default();
        cfg.set_test_data_dir(dir.clone());
        let answers = BTreeMap::from([
            (KEY_PROVIDER.to_string(), "DeepSeek".to_string()),
            (KEY_API_KEY.to_string(), "sk-form-test".to_string()),
            (KEY_MODEL.to_string(), "deepseek-v4-pro".to_string()),
            (KEY_AGENTS.to_string(), "1".to_string()),
        ]);
        let result = apply_provider_answers(&mut cfg, &answers).unwrap();
        assert!(result.contains("deepseek-v4-pro"));
        assert_eq!(cfg.provider.base_url, "https://api.deepseek.com");
        assert_eq!(cfg.provider.model, "deepseek-v4-pro");
        assert_eq!(cfg.provider.ctx_window, 1_000_000);
        assert_eq!(cfg.provider.ctx_override, None);
        assert_eq!(cfg.agents.model, "deepseek-v4-flash");
        assert!(cfg.agents.enabled);
        assert!(cfg.pricing.enabled);
        let saved = fs::read_to_string(dir.join("config.json")).unwrap();
        assert!(saved.contains("deepseek-v4-pro"));
        let _ = fs::remove_dir_all(dir);
    }
    #[test]
    fn provider_form_rejects_missing_deepseek_key_without_mutation() {
        let mut cfg = Config::default();
        cfg.provider.base_url = "http://127.0.0.1:8080/v1".into();
        let original = cfg.provider.base_url.clone();
        let answers = BTreeMap::from([
            (KEY_PROVIDER.to_string(), "DeepSeek".to_string()),
            (KEY_MODEL.to_string(), "deepseek-v4-flash".to_string()),
            (KEY_AGENTS.to_string(), "0".to_string()),
        ]);
        assert!(apply_provider_answers(&mut cfg, &answers).is_err());
        assert_eq!(cfg.provider.base_url, original);
    }
}
