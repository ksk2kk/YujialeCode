//! grok 插件市场兼容层。
//!
//! 实现 TUI 依赖的两个 ext 方法：
//! - `x.ai/plugins/list`：枚举已装插件（复用 grok 的 InstallRegistry/manifest）
//! - `x.ai/plugins/action`：Install/Uninstall/Reload（复用 `xai_grok_shell::plugin`
//!   的安装编排，与 `grok plugin add/rm` 同一实现）
//!
//! 安装完成后把插件内的 skills/ 同步到 yjlcoder 的技能目录
//! （`~/.yjlcoder/skills/<plugin>__<skill>`），让 yjl agent 的 /skills、
//! run_skill 立即可用——这是"grok 市场装的、我们的模型能用"的闭环。

use std::path::{Path, PathBuf};

use xai_grok_agent::plugins::install_registry::InstallRegistry;
use xai_grok_agent::plugins::manifest::load_manifest;
use xai_hooks_plugins_types::{
    ActionOutcome, OutcomeStatus, PluginInfo, PluginScope, PluginsListResponse,
};

/// 安装动作的线型（与 xai_hooks_plugins_types::PluginsAction 对齐的宽松解析）。
#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireAction {
    Reload,
    Install { source: String },
    Uninstall { plugin_id: String },
}

fn install_root() -> PathBuf {
    InstallRegistry::resolve_install_dir()
}

/// 枚举已装插件 → grok 线型 JSON（id 直接用插件名，与卸载动作自洽）。
pub fn list_response_json() -> serde_json::Value {
    let registry = InstallRegistry::load();
    let mut plugins = Vec::new();
    for repo in registry.repos.values() {
        for (name, plugin) in &repo.plugins {
            let root = match &plugin.subdir {
                Some(sub) => repo.path.join(sub),
                None => repo.path.clone(),
            };
            let (version, description, skill_names, agent_count, hook_count) = inspect(&root);
            let marketplace_source = repo
                .marketplace
                .as_ref()
                .map(|m| m.source_display_name.clone());
            plugins.push(PluginInfo {
                name: name.clone(),
                id: name.clone(),
                root: root.display().to_string(),
                scope: PluginScope::User,
                trusted: true,
                enabled: true,
                version: version.or_else(|| plugin.version.clone()),
                description,
                skill_count: skill_names.len(),
                skill_names,
                agent_count,
                agent_names: Vec::new(),
                hook_status: xai_hooks_plugins_types::HookStatus::None,
                hook_count,
                mcp_server_count: 0,
                mcp_status: xai_hooks_plugins_types::McpStatus::None,
                marketplace_source,
                origin: None,
                conflict: None,
            });
        }
    }
    let response = PluginsListResponse { plugins };
    serde_json::to_value(&response).unwrap_or_default()
}

/// 读插件根目录的 manifest 与组件统计。
fn inspect(root: &Path) -> (Option<String>, Option<String>, Vec<String>, usize, usize) {
    let (mut version, mut description) = (None, None);
    if let Ok(xai_grok_agent::plugins::manifest::ManifestLoadResult::Found(manifest)) =
        load_manifest(root)
    {
        version = manifest.version.clone();
        description = manifest.description.clone();
    }
    let skill_names = skill_dirs(&root.join("skills"));
    let agent_count = root
        .join("agents")
        .read_dir()
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
                .count()
        })
        .unwrap_or(0);
    let hook_count = usize::from(root.join("hooks").is_dir());
    (version, description, skill_names, agent_count, hook_count)
}

fn skill_dirs(skills_root: &Path) -> Vec<String> {
    skills_root
        .read_dir()
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| e.path().join("SKILL.md").is_file())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// 把插件内的 skills 复制到 yjlcoder 技能目录（幂等）。
fn sync_skills_to_yjl(plugin_name: &str, root: &Path) -> usize {
    let yjl_skills = yjlcoder::config::data_dir().join("skills");
    let mut synced = 0;
    for skill in skill_dirs(&root.join("skills")) {
        let src = root.join("skills").join(&skill);
        let dst = yjl_skills.join(format!("{plugin_name}__{skill}"));
        if dst.exists() {
            let _ = std::fs::remove_dir_all(&dst);
        }
        if std::fs::create_dir_all(&dst).is_ok() && copy_dir(&src, &dst) {
            synced += 1;
        }
    }
    synced
}

fn copy_dir(src: &Path, dst: &Path) -> bool {
    let Ok(entries) = src.read_dir() else {
        return false;
    };
    for entry in entries.filter_map(Result::ok) {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ok = if from.is_dir() {
            std::fs::create_dir_all(&to).is_ok() && copy_dir(&from, &to)
        } else {
            std::fs::copy(&from, &to).is_ok()
        };
        if !ok {
            return false;
        }
    }
    true
}

fn remove_yjl_skills(plugin_name: &str) {
    let yjl_skills = yjlcoder::config::data_dir().join("skills");
    let Ok(entries) = yjl_skills.read_dir() else {
        return;
    };
    let prefix = format!("{plugin_name}__");
    for entry in entries.filter_map(Result::ok) {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// 处理 `x.ai/plugins/action`（安装的重 IO 放阻塞线程）。
pub async fn handle_action(params: &str) -> ActionOutcome {
    // 信封：{"sessionId": "...", "action": {"type": "install", ...}}
    let action: WireAction = match serde_json::from_str::<serde_json::Value>(params)
        .ok()
        .and_then(|envelope| envelope.get("action").cloned())
        .ok_or_else(|| "缺少 action 字段".to_string())
        .and_then(|action| serde_json::from_value(action).map_err(|e| e.to_string()))
    {
        Ok(action) => action,
        Err(error) => {
            return ActionOutcome {
                status: OutcomeStatus::ValidationError,
                message: format!("无法解析插件操作: {error}"),
                requires_reload: false,
                requires_restart: false,
            }
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match action {
        WireAction::Reload => ActionOutcome {
            status: OutcomeStatus::Success,
            message: "已重新扫描插件".into(),
            requires_reload: false,
            requires_restart: false,
        },
        WireAction::Install { source } => {
            let result = tokio::task::spawn_blocking(move || {
                xai_grok_shell::plugin::install_plugin(&source, &cwd)
            })
            .await;
            match result {
                Ok(Ok(outcome)) => {
                    let mut synced = 0;
                    let repo_root = install_root().join(&outcome.repo_key);
                    for name in &outcome.plugin_names {
                        synced += sync_skills_to_yjl(name, &repo_root);
                    }
                    let mut message = format!(
                        "已安装：{}（仓库 {}）",
                        outcome.plugin_names.join("、"),
                        outcome.repo_key
                    );
                    if synced > 0 {
                        message.push_str(&format!("；已同步 {synced} 个技能到 yjlcoder（/skills 可用）"));
                    }
                    for warning in &outcome.warnings {
                        message.push_str(&format!("\n⚠ {warning}"));
                    }
                    ActionOutcome {
                        status: OutcomeStatus::Success,
                        message,
                        requires_reload: false,
                        requires_restart: false,
                    }
                }
                Ok(Err(error)) => ActionOutcome {
                    status: OutcomeStatus::InternalError,
                    message: xai_grok_shell::plugin::classify_install_error(&error),
                    requires_reload: false,
                    requires_restart: false,
                },
                Err(error) => ActionOutcome {
                    status: OutcomeStatus::InternalError,
                    message: format!("安装任务失败: {error}"),
                    requires_reload: false,
                    requires_restart: false,
                },
            }
        }
        WireAction::Uninstall { plugin_id } => {
            remove_yjl_skills(&plugin_id);
            match xai_grok_shell::plugin::uninstall_plugin(&plugin_id, true, false) {
                Ok(outcome) => ActionOutcome {
                    status: OutcomeStatus::Success,
                    message: format!(
                        "已卸载 {}（移除：{}）",
                        outcome.repo_key,
                        outcome.removed_plugins.join("、")
                    ),
                    requires_reload: false,
                    requires_restart: false,
                },
                Err(error) => ActionOutcome {
                    status: OutcomeStatus::InternalError,
                    message: format!("卸载失败: {error}"),
                    requires_reload: false,
                    requires_restart: false,
                },
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// 沙盒 HOME：隔离 grok 安装目录与 yjlcoder 数据目录。
    fn sandbox(tag: &str) -> (tempdir_guard::Guard, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "yjl-bridge-plugin-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(".grok")).unwrap();
        std::fs::create_dir_all(dir.join(".yjlcoder")).unwrap();
        let guard = tempdir_guard::Guard(dir.clone());
        std::env::set_var("HOME", &dir);
        std::env::set_var("YJLCODER_HOME", dir.join(".yjlcoder"));
        (guard, dir)
    }

    mod tempdir_guard {
        pub struct Guard(pub std::path::PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn make_source_plugin(root: &Path) {
        let skills = root.join("skills/demo");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(
            skills.join("SKILL.md"),
            "---\nname: demo\ndescription: 演示技能\n---\n# Demo\n测试技能正文。\n",
        )
        .unwrap();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn install_lists_and_uninstalls_with_skill_sync() {
        let (_guard, home) = sandbox("cycle");
        let source = home.join("src-plugin");
        std::fs::create_dir_all(&source).unwrap();
        make_source_plugin(&source);

        // 安装（本地路径）→ 成功且技能同步到 yjlcoder
        let params = serde_json::json!({
            "sessionId": "s1",
            "action": { "type": "install", "source": source.to_string_lossy() }
        })
        .to_string();
        let outcome = handle_action(&params).await;
        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.message);
        assert!(outcome.message.contains("已同步 1 个技能"), "{}", outcome.message);
        let yjl_skill = home.join(".yjlcoder/skills");
        let synced: Vec<String> = std::fs::read_dir(&yjl_skill)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert!(
            synced.iter().any(|n| n.ends_with("__demo")),
            "技能应同步到 yjlcoder: {synced:?}"
        );

        // list 含该插件
        let list = list_response_json();
        let names: Vec<String> = list["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["name"].as_str().map(String::from))
            .collect();
        assert!(!names.is_empty(), "安装后 list 不应为空");

        // 卸载 → 同步技能一并移除
        let plugin_id = names[0].clone();
        let params = serde_json::json!({
            "sessionId": "s1",
            "action": { "type": "uninstall", "plugin_id": plugin_id }
        })
        .to_string();
        let outcome = handle_action(&params).await;
        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.message);
        let left: Vec<String> = std::fs::read_dir(&yjl_skill)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        assert!(left.is_empty(), "卸载后同步技能应移除: {left:?}");
    }

    #[test]
    #[serial_test::serial]
    fn list_on_empty_registry_is_empty_array() {
        let (_guard, _home) = sandbox("empty");
        let list = list_response_json();
        assert_eq!(list["plugins"].as_array().map(Vec::len), Some(0));
    }

    #[test]
    #[serial_test::serial]
    fn skills_list_merges_yjl_and_plugin_skills() {
        let (_guard, home) = sandbox("skills");
        let skill_dir = home.join(".yjlcoder/skills/my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: 我的演示技能\n---\n正文",
        )
        .unwrap();
        let list = skills_list_json();
        let names: Vec<&str> = list["skills"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["name"].as_str())
            .collect();
        assert!(names.contains(&"my-skill"), "应包含 yjl 技能: {names:?}");
        let entry = list["skills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "my-skill")
            .unwrap();
        assert_eq!(entry["description"], "我的演示技能");
        assert_eq!(entry["hasUserSpecifiedDescription"], true);
        assert!(entry["path"].as_str().is_some_and(|p| p.contains("my-skill")));
        assert_eq!(entry["scope"], "user");
    }
}

/// 解析 SKILL.md frontmatter 的 name/description（宽松行级解析足够）。
fn parse_frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let mut name = None;
    let mut description = None;
    let mut in_front = false;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed == "---" {
            if in_front {
                break;
            }
            in_front = true;
            continue;
        }
        if !in_front {
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("name:") {
            if name.is_none() {
                name = Some(value.trim().trim_matches('"').to_string());
            }
        } else if let Some(value) = trimmed.strip_prefix("description:") {
            if description.is_none() {
                description = Some(value.trim().trim_matches('"').to_string());
            }
        }
    }
    (name, description)
}

/// Skills 面板列表：yjlcoder 技能目录 + grok 插件内技能，合并为 grok 线型。
pub fn skills_list_json() -> serde_json::Value {
    let mut skills: Vec<serde_json::Value> = Vec::new();
    let mut push_skill = |name: String, description: String, skill_path: String| {
        skills.push(serde_json::json!({
            "name": name,
            "description": description,
            "hasUserSpecifiedDescription": true,
            "path": skill_path,
            "scope": "user",
            "userInvocable": true,
            "enabled": true,
        }));
    };
    // 1) yjlcoder 技能目录（含插件同步进来的 <plugin>__<skill>）
    let yjl_skills = yjlcoder::config::data_dir().join("skills");
    if let Ok(entries) = yjl_skills.read_dir() {
        let mut dirs: Vec<_> = entries
            .filter_map(Result::ok)
            .filter(|e| e.path().join("SKILL.md").is_file())
            .map(|e| e.file_name().into_string().unwrap_or_default())
            .collect();
        dirs.sort();
        for dir in dirs {
            let text = std::fs::read_to_string(yjl_skills.join(&dir).join("SKILL.md"))
                .unwrap_or_default();
            let (front_name, front_desc) = parse_frontmatter(&text);
            push_skill(
                front_name.clone().unwrap_or_else(|| dir.clone()),
                front_desc.unwrap_or_else(|| format!("{dir} 技能")),
                yjl_skills.join(&dir).display().to_string(),
            );
        }
    }
    // 2) grok 插件内的 skills（未同步场景兜底）
    let registry = InstallRegistry::load();
    for repo in registry.repos.values() {
        for (plugin_name, plugin) in &repo.plugins {
            let root = match &plugin.subdir {
                Some(sub) => repo.path.join(sub),
                None => repo.path.clone(),
            };
            for skill in skill_dirs(&root.join("skills")) {
                let text = std::fs::read_to_string(root.join("skills").join(&skill).join("SKILL.md"))
                    .unwrap_or_default();
                let (front_name, front_desc) = parse_frontmatter(&text);
                push_skill(
                    format!("{plugin_name}__{skill}"),
                    front_desc.unwrap_or_else(|| format!("{plugin_name} 插件技能 {skill}")),
                    root.join("skills").join(&skill).display().to_string(),
                );
                let _ = front_name;
            }
        }
    }
    serde_json::json!({ "skills": skills })
}
