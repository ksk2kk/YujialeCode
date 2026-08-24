use crate::config::Config;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static STORE_LOCK: Mutex<()> = Mutex::new(());
const DEFAULT_INTERVAL_SECS: u64 = 24 * 60 * 60;
const MIN_INTERVAL_SECS: u64 = 60;
const STALE_QUEUE_SECS: u64 = 5 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MasterTask {
    pub id: String,
    pub chat: String,
    pub goal: String,
    pub interval_secs: u64,
    pub enabled: bool,
    pub next_due_at: u64,
    pub last_sent_at: Option<u64>,
    pub queued: bool,
    pub queued_at: Option<u64>,
    pub delivery_count: u64,
    pub created_at: u64,
}

impl Default for MasterTask {
    fn default() -> Self {
        Self {
            id: String::new(),
            chat: String::new(),
            goal: String::new(),
            interval_secs: DEFAULT_INTERVAL_SECS,
            enabled: true,
            next_due_at: 0,
            last_sent_at: None,
            queued: false,
            queued_at: None,
            delivery_count: 0,
            created_at: 0,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct MasterStore {
    version: u32,
    tasks: Vec<MasterTask>,
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn store_path(cfg: &Config) -> PathBuf {
    cfg.data_dir().join("fuck_master.json")
}

fn load_unlocked(cfg: &Config) -> MasterStore {
    fs::read_to_string(store_path(cfg))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| MasterStore {
            version: 1,
            tasks: Vec::new(),
        })
}

fn save_unlocked(cfg: &Config, store: &MasterStore) -> Result<(), String> {
    let path = store_path(cfg);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建 FuckMaster 目录失败: {e}"))?;
    }
    let text =
        serde_json::to_string_pretty(store).map_err(|e| format!("序列化 FuckMaster 失败: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text).map_err(|e| format!("保存 FuckMaster 失败: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("替换 FuckMaster 状态失败: {e}"))
}

fn chat_from_session(session_id: &str) -> Option<String> {
    if let Some(id) = session_id.strip_prefix("qq_g") {
        id.parse::<i64>().ok().map(|id| format!("group:{id}"))
    } else if let Some(id) = session_id.strip_prefix("qq_u") {
        id.parse::<i64>().ok().map(|id| format!("private:{id}"))
    } else {
        None
    }
}

fn default_chat(cfg: &Config, session_id: &str) -> Option<String> {
    chat_from_session(session_id)
        .or_else(|| cfg.qq.admins.first().map(|id| format!("private:{id}")))
        .or_else(|| cfg.qq.users.first().map(|id| format!("private:{id}")))
        .or_else(|| cfg.qq.groups.first().map(|id| format!("group:{id}")))
}

fn chat_allowed(cfg: &Config, chat: &str) -> bool {
    if let Some(id) = chat
        .strip_prefix("group:")
        .and_then(|id| id.parse::<i64>().ok())
    {
        cfg.qq.groups.contains(&id)
    } else if let Some(id) = chat
        .strip_prefix("private:")
        .and_then(|id| id.parse::<i64>().ok())
    {
        cfg.qq.users.contains(&id) || cfg.qq.admins.contains(&id)
    } else {
        false
    }
}

fn parse_interval(raw: &str) -> Result<u64, String> {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Ok(DEFAULT_INTERVAL_SECS);
    }
    let (number, multiplier) = if let Some(n) = normalized.strip_suffix("分钟") {
        (n, 60)
    } else if let Some(n) = normalized.strip_suffix("小时") {
        (n, 60 * 60)
    } else if let Some(n) = normalized.strip_suffix('天') {
        (n, 24 * 60 * 60)
    } else {
        let split = normalized
            .char_indices()
            .find(|(_, c)| !c.is_ascii_digit())
            .map(|(index, _)| index)
            .unwrap_or(normalized.len());
        let (n, unit) = normalized.split_at(split);
        let multiplier = match unit.trim() {
            "" | "m" | "min" | "mins" => 60,
            "h" | "hr" | "hrs" => 60 * 60,
            "d" | "day" | "days" => 24 * 60 * 60,
            "w" | "week" | "weeks" => 7 * 24 * 60 * 60,
            _ => return Err("时间格式错误，请用 30m、2h、1d、1w、30分钟或2小时".into()),
        };
        (n, multiplier)
    };
    let value = number
        .trim()
        .parse::<u64>()
        .map_err(|_| "时间必须是正整数".to_string())?;
    let secs = value.saturating_mul(multiplier);
    if secs < MIN_INTERVAL_SECS {
        return Err("提醒间隔不能少于 1 分钟".into());
    }
    Ok(secs)
}

fn interval_label(secs: u64) -> String {
    if secs % (7 * 86400) == 0 {
        format!("{}周", secs / (7 * 86400))
    } else if secs % 86400 == 0 {
        format!("{}天", secs / 86400)
    } else if secs % 3600 == 0 {
        format!("{}小时", secs / 3600)
    } else {
        format!("{}分钟", secs / 60)
    }
}

fn task_id(tasks: &[MasterTask]) -> String {
    let next = tasks
        .iter()
        .filter_map(|task| task.id.strip_prefix("fm-")?.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    format!("fm-{next:04}")
}

fn arg_text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

pub fn execute_tool(cfg: &Config, session_id: &str, args: &Value) -> Result<String, String> {
    let action = arg_text(args, "action")
        .unwrap_or("list")
        .to_ascii_lowercase();
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "FuckMaster 状态锁损坏".to_string())?;
    let mut store = load_unlocked(cfg);
    match action.as_str() {
        "add" | "create" | "register" => {
            let goal = arg_text(args, "goal")
                .or_else(|| arg_text(args, "target"))
                .ok_or("缺少 goal：要推进的目标是什么")?;
            let chat = arg_text(args, "chat")
                .map(str::to_string)
                .or_else(|| default_chat(cfg, session_id))
                .ok_or(
                    "没有可用的 QQ 主人目标，请先配置管理员，或显式提供 chat，例如 private:123456",
                )?;
            if !chat_allowed(cfg, &chat) {
                return Err(format!("QQ 目标 {chat} 不在白名单或格式错误"));
            }
            let every = arg_text(args, "every")
                .or_else(|| arg_text(args, "interval"))
                .unwrap_or("1d");
            let interval_secs = parse_interval(every)?;
            let now = now_epoch();
            let id = task_id(&store.tasks);
            store.tasks.push(MasterTask {
                id: id.clone(),
                chat: chat.clone(),
                goal: goal.to_string(),
                interval_secs,
                enabled: true,
                next_due_at: now.saturating_add(interval_secs),
                last_sent_at: None,
                queued: false,
                queued_at: None,
                delivery_count: 0,
                created_at: now,
            });
            save_unlocked(cfg, &store)?;
            Ok(format!(
                "已注册 {id}：每 {}向 {chat} 推进目标“{goal}”",
                interval_label(interval_secs)
            ))
        }
        "list" | "status" => {
            let chat_filter = arg_text(args, "chat")
                .map(str::to_string)
                .or_else(|| chat_from_session(session_id));
            let tasks: Vec<&MasterTask> = store
                .tasks
                .iter()
                .filter(|task| chat_filter.as_ref().is_none_or(|chat| &task.chat == chat))
                .collect();
            if tasks.is_empty() {
                return Ok("FuckMaster 暂无推进任务".into());
            }
            let mut out = String::from("FuckMaster 推进任务\n");
            for task in tasks {
                let state = if !task.enabled {
                    "已暂停"
                } else if task.queued {
                    "等待模型空闲"
                } else {
                    "运行中"
                };
                out.push_str(&format!(
                    "{}  {}  每{}  {}  {}\n",
                    task.id,
                    state,
                    interval_label(task.interval_secs),
                    task.chat,
                    task.goal
                ));
            }
            Ok(out.trim_end().to_string())
        }
        "pause" | "resume" | "delete" | "remove" | "now" | "run_now" => {
            let id = arg_text(args, "id").ok_or("缺少任务 id，例如 fm-0001")?;
            let index = store
                .tasks
                .iter()
                .position(|task| task.id == id)
                .ok_or_else(|| format!("没有找到 FuckMaster 任务 {id}"))?;
            if matches!(action.as_str(), "delete" | "remove") {
                store.tasks.remove(index);
                save_unlocked(cfg, &store)?;
                return Ok(format!("已删除 {id}"));
            }
            let now = now_epoch();
            let task = &mut store.tasks[index];
            match action.as_str() {
                "pause" => {
                    task.enabled = false;
                    task.queued = false;
                    task.queued_at = None;
                }
                "resume" => {
                    task.enabled = true;
                    task.queued = false;
                    task.queued_at = None;
                    task.next_due_at = now.saturating_add(task.interval_secs);
                }
                _ => {
                    task.enabled = true;
                    task.queued = false;
                    task.queued_at = None;
                    task.next_due_at = now;
                }
            }
            let state = if action == "pause" {
                "暂停"
            } else if action == "resume" {
                "恢复"
            } else {
                "已放入立即推进队列"
            };
            save_unlocked(cfg, &store)?;
            Ok(format!("{id} 已{state}"))
        }
        _ => Err("action 仅支持 add、list、pause、resume、now、delete".into()),
    }
}

pub fn execute_slash(cfg: &Config, session_id: &str, raw: &str) -> Result<String, String> {
    let text = raw.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("list") || text.eq_ignore_ascii_case("status") {
        return execute_tool(cfg, session_id, &serde_json::json!({"action":"list"}));
    }
    let mut parts = text.split_whitespace();
    let first = parts.next().unwrap_or("");
    let action = first.to_ascii_lowercase();
    if matches!(
        action.as_str(),
        "pause" | "resume" | "now" | "delete" | "remove"
    ) {
        let id = parts.next().unwrap_or("");
        return execute_tool(
            cfg,
            session_id,
            &serde_json::json!({"action":action,"id":id}),
        );
    }
    if action == "add" {
        let second = parts.next().unwrap_or("");
        let (every, goal) = if parse_interval(second).is_ok() {
            (second, parts.collect::<Vec<_>>().join(" "))
        } else {
            (
                "1d",
                std::iter::once(second)
                    .chain(parts)
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        };
        return execute_tool(
            cfg,
            session_id,
            &serde_json::json!({"action":"add","every":every,"goal":goal}),
        );
    }
    execute_tool(
        cfg,
        session_id,
        &serde_json::json!({"action":"add","every":"1d","goal":text}),
    )
}

pub fn claim_due(cfg: &Config, now: u64) -> Result<Option<MasterTask>, String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "FuckMaster 状态锁损坏".to_string())?;
    let mut store = load_unlocked(cfg);
    let mut recovered_stale_queue = false;
    for task in &mut store.tasks {
        if task.queued
            && task
                .queued_at
                .is_some_and(|at| now.saturating_sub(at) >= STALE_QUEUE_SECS)
        {
            task.queued = false;
            task.queued_at = None;
            recovered_stale_queue = true;
        }
    }
    let Some(index) = store
        .tasks
        .iter()
        .enumerate()
        .filter(|(_, task)| task.enabled && !task.queued && task.next_due_at <= now)
        .min_by_key(|(_, task)| task.next_due_at)
        .map(|(index, _)| index)
    else {
        if recovered_stale_queue {
            save_unlocked(cfg, &store)?;
        }
        return Ok(None);
    };
    store.tasks[index].queued = true;
    store.tasks[index].queued_at = Some(now);
    let task = store.tasks[index].clone();
    save_unlocked(cfg, &store)?;
    Ok(Some(task))
}

pub fn still_dispatchable(cfg: &Config, id: &str) -> bool {
    let Ok(_guard) = STORE_LOCK.lock() else {
        return false;
    };
    load_unlocked(cfg)
        .tasks
        .iter()
        .any(|task| task.id == id && task.enabled && task.queued)
}

pub fn mark_delivered(cfg: &Config, id: &str, now: u64) -> Result<(), String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "FuckMaster 状态锁损坏".to_string())?;
    let mut store = load_unlocked(cfg);
    let task = store
        .tasks
        .iter_mut()
        .find(|task| task.id == id)
        .ok_or_else(|| format!("没有找到 FuckMaster 任务 {id}"))?;
    task.queued = false;
    task.queued_at = None;
    task.last_sent_at = Some(now);
    task.delivery_count = task.delivery_count.saturating_add(1);
    task.next_due_at = now.saturating_add(task.interval_secs);
    save_unlocked(cfg, &store)
}

pub fn mark_failed(cfg: &Config, id: &str, now: u64) -> Result<(), String> {
    let _guard = STORE_LOCK
        .lock()
        .map_err(|_| "FuckMaster 状态锁损坏".to_string())?;
    let mut store = load_unlocked(cfg);
    let task = store
        .tasks
        .iter_mut()
        .find(|task| task.id == id)
        .ok_or_else(|| format!("没有找到 FuckMaster 任务 {id}"))?;
    task.queued = false;
    task.queued_at = None;
    task.next_due_at = now.saturating_add(task.interval_secs.min(5 * 60));
    save_unlocked(cfg, &store)
}

pub fn current_epoch() -> u64 {
    now_epoch()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn cfg() -> Config {
        let mut cfg = Config::default();
        cfg.qq.groups.push(123);
        let dir = std::env::temp_dir().join(format!(
            "yjlcoder-fm-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        cfg.set_test_data_dir(dir);
        cfg
    }

    #[test]
    fn tool_add_list_pause_resume_delete() {
        let cfg = cfg();
        let added = execute_tool(
            &cfg,
            "qq_g123",
            &serde_json::json!({"action":"add","goal":"学习 Rust","every":"2h"}),
        )
        .unwrap();
        assert!(added.contains("fm-0001"));
        assert!(
            execute_tool(&cfg, "qq_g123", &serde_json::json!({"action":"list"}))
                .unwrap()
                .contains("学习 Rust")
        );
        assert!(execute_tool(
            &cfg,
            "qq_g123",
            &serde_json::json!({"action":"pause","id":"fm-0001"})
        )
        .unwrap()
        .contains("暂停"));
        assert!(execute_tool(
            &cfg,
            "qq_g123",
            &serde_json::json!({"action":"resume","id":"fm-0001"})
        )
        .unwrap()
        .contains("恢复"));
        assert!(execute_tool(
            &cfg,
            "qq_g123",
            &serde_json::json!({"action":"delete","id":"fm-0001"})
        )
        .unwrap()
        .contains("删除"));
    }

    #[test]
    fn now_claims_once_and_delivery_reschedules() {
        let cfg = cfg();
        execute_slash(&cfg, "qq_g123", "add 1h 推进找工作").unwrap();
        execute_tool(
            &cfg,
            "qq_g123",
            &serde_json::json!({"action":"now","id":"fm-0001"}),
        )
        .unwrap();
        let now = current_epoch();
        let task = claim_due(&cfg, now).unwrap().unwrap();
        assert_eq!(task.goal, "推进找工作");
        assert!(claim_due(&cfg, now).unwrap().is_none());
        mark_delivered(&cfg, &task.id, now).unwrap();
        assert!(claim_due(&cfg, now).unwrap().is_none());
    }

    #[test]
    fn rejects_unknown_chat_and_too_fast_interval() {
        let cfg = cfg();
        assert!(execute_tool(
            &cfg,
            "main",
            &serde_json::json!({"action":"add","goal":"x","chat":"private:999"})
        )
        .is_err());
        assert!(execute_tool(
            &cfg,
            "qq_g123",
            &serde_json::json!({"action":"add","goal":"x","every":"10s"})
        )
        .is_err());
    }
}
