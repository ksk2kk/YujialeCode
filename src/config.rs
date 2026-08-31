use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub provider: Provider,
    #[serde(default)]
    pub agents: Agents,
    pub qq: Qq,
    pub tui: Tui,
    #[serde(default)]
    pub pricing: Pricing,
    #[serde(default)]
    pub search: Search,
    #[serde(default = "default_tool_times")]
    pub tool_times: usize,
    #[serde(default = "default_true")]
    pub fuckloop: bool,
    #[serde(default = "default_command_timeout")]
    pub command_timeout_secs: u64,
    #[serde(default)]
    pub llama: Llama,
    #[serde(default)]
    pub trace: Trace,
    #[serde(skip)]
    pub(crate) data_root: Option<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Agents {
    /// Master switch for child/background agents. The runtime still applies
    /// the hard concurrency, per-turn and nesting limits below.
    pub enabled: bool,
    /// Concurrent child inference requests. Values above two are clamped by
    /// the runtime because this project targets single-machine local models.
    pub max_concurrent: usize,
    /// A parent model may create at most this many children in one turn.
    pub max_spawn_per_turn: usize,
    /// Child tool/model steps. On exhaustion the child returns what it has.
    pub max_steps: usize,
    /// Empty means inherit the parent model. DeepSeek setup fills Flash here
    /// so Pro/Vision sessions can delegate cheaply.
    pub model: String,
}
impl Default for Agents {
    fn default() -> Self {
        Agents {
            enabled: true,
            max_concurrent: 1,
            max_spawn_per_turn: 1,
            max_steps: 8,
            model: String::new(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Llama {
    pub auto_start: bool,
    pub service: String,
    pub start_wait_secs: u64,
}
impl Default for Llama {
    fn default() -> Self {
        Llama { auto_start: false, service: String::new(), start_wait_secs: 180 }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Trace {
    pub enabled: bool,
    pub show_reasoning: bool,
}
impl Default for Trace {
    fn default() -> Self {
        Trace { enabled: true, show_reasoning: false }
    }
}
pub fn default_tool_times() -> usize {
    24
}
pub fn default_command_timeout() -> u64 {
    600
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Search {
    pub brave_key: String,
    pub searxng_url: String,
    pub searxng_key: String,
    /// Tavily 免费 key（注册即得，免费额度以官网为准）；配置后自动加入 auto 聚合池
    #[serde(default)]
    pub tavily_key: String,
    /// DuckDuckGo 端点列表：可填 "html" / "lite" 或自定义镜像 URL；留空 = html+lite 自动轮换
    #[serde(default)]
    pub ddg_endpoints: Vec<String>,
    /// 免费引擎（DDG/Bing/Wikipedia）请求超时（秒）
    #[serde(default = "default_search_timeout")]
    pub timeout_secs: u64,
    /// Bing 是否强制国际版结果（ensearch=1），规避中国区结果偏窄
    #[serde(default = "default_true")]
    pub bing_ensearch: bool,
}
impl Default for Search {
    fn default() -> Self {
        Self {
            brave_key: String::new(),
            searxng_url: String::new(),
            searxng_key: String::new(),
            tavily_key: String::new(),
            ddg_endpoints: Vec::new(),
            timeout_secs: default_search_timeout(),
            bing_ensearch: true,
        }
    }
}
fn default_search_timeout() -> u64 {
    10
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Pricing {
    /// Whether cost estimates are shown. Prices are always per one million tokens.
    pub enabled: bool,
    pub currency: String,
    pub cache_read_per_million: f64,
    pub cache_miss_per_million: f64,
    pub output_per_million: f64,
}
impl Default for Pricing {
    fn default() -> Self {
        Pricing {
            enabled: false,
            currency: "¥".into(),
            cache_read_per_million: 0.0,
            cache_miss_per_million: 0.0,
            output_per_million: 0.0,
        }
    }
}
impl Pricing {
    pub fn deepseek_flash_cny() -> Self {
        Pricing {
            enabled: true,
            currency: "¥".into(),
            cache_read_per_million: 0.02,
            cache_miss_per_million: 1.0,
            output_per_million: 2.0,
        }
    }
    pub fn estimate(
        &self,
        prompt_tokens: usize,
        cache_read_tokens: usize,
        cache_miss_tokens: usize,
        output_tokens: usize,
    ) -> Option<f64> {
        if !self.enabled {
            return None;
        }
        let read = cache_read_tokens.min(prompt_tokens);
        let miss = if cache_miss_tokens > 0 || read > 0 {
            cache_miss_tokens.min(prompt_tokens.saturating_sub(read))
        } else {
            // A backend without cache accounting is priced conservatively as a miss.
            prompt_tokens
        };
        Some(
            (read as f64 * self.cache_read_per_million
                + miss as f64 * self.cache_miss_per_million
                + output_tokens as f64 * self.output_per_million)
                / 1_000_000.0,
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub ctx_window: usize,
    #[serde(default)]
    pub ctx_override: Option<usize>,
    pub native_tools: bool,
    pub timeout_secs: u64,
    #[serde(default = "default_true")]
    pub auto_reload: bool,
    #[serde(default = "default_load_context")]
    pub load_context: usize,
    #[serde(default)]
    pub thinking_budget: Option<usize>,
}
fn default_load_context() -> usize {
    16384
}
fn default_true() -> bool {
    true
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Qq {
    pub ws_mode: String,
    pub ws_addr: String,
    pub ws_path: String,
    pub groups: Vec<i64>,
    pub users: Vec<i64>,
    #[serde(default)]
    pub admins: Vec<i64>,
    pub triggers: Vec<String>,
    pub need_at: bool,
    pub max_tokens: usize,
    #[serde(default)]
    pub auto_new: usize,
}
impl Qq {
    pub fn add_admin(&mut self, qq: i64) -> bool {
        let mut added = false;
        if !self.admins.contains(&qq) {
            self.admins.push(qq);
            added = true;
        }
        if !self.users.contains(&qq) {
            self.users.push(qq);
        }
        added
    }
    pub fn add_group(&mut self, gid: i64) -> bool {
        if self.groups.contains(&gid) {
            return false;
        }
        self.groups.push(gid);
        true
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tui {
    pub compress_threshold: f64,
    pub tool_result_max_tokens: usize,
}
impl Default for Config {
    fn default() -> Self {
        Config {
            provider: Provider {
                base_url: "https://api.deepseek.com".into(),
                api_key: String::new(),
                model: "deepseek-v4-flash".into(),
                ctx_window: crate::backend::DEEPSEEK_V4_CONTEXT_WINDOW,
                ctx_override: None,
                native_tools: true,
                timeout_secs: 120,
                auto_reload: true,
                load_context: 16384,
                thinking_budget: None,
            },
            agents: Agents::default(),
            qq: Qq {
                ws_mode: "server".into(),
                ws_addr: "127.0.0.1:6701".into(),
                ws_path: "/onebot/v11/ws".into(),
                groups: Vec::new(),
                users: Vec::new(),
                admins: Vec::new(),
                triggers: vec!["yjlcoder".into()],
                need_at: true,
                max_tokens: 8192,
                auto_new: 0,
            },
            tui: Tui {
                compress_threshold: 0.75,
                tool_result_max_tokens: 1000,
            },
            pricing: Pricing::default(),
            search: Search::default(),
            tool_times: default_tool_times(),
            fuckloop: true,
            command_timeout_secs: default_command_timeout(),
            llama: Llama::default(),
            trace: Trace::default(),
            data_root: None,
        }
    }
}
pub fn data_dir() -> PathBuf {
    if let Ok(h) = env::var("YJLCODER_HOME") {
        return PathBuf::from(h);
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".yjlcoder")
}
pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}
impl Config {
    pub fn load() -> Self {
        let p = config_path();
        match fs::read_to_string(&p) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("config.json 解析失败({e})，使用默认配置");
                    let c = Config::default();
                    c.save();
                    c
                }
            },
            Err(_) => {
                let c = Config::default();
                c.save();
                c
            }
        }
    }
    pub fn save(&self) {
        let dir = self.data_dir();
        let _ = fs::create_dir_all(&dir);                                 
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let path = dir.join("config.json");
            if fs::write(&path, s).is_ok() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
                }
            }
        }
    }
    pub fn data_dir(&self) -> PathBuf {
        self.data_root.clone().unwrap_or_else(data_dir)
    }
    #[cfg(test)]
    pub(crate) fn set_test_data_dir(&mut self, dir: PathBuf) {
        self.data_root = Some(dir);
    }
    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir().join("sessions")
    }
    pub fn skills_dir(&self) -> PathBuf {
        self.data_dir().join("skills")
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_config_roundtrip() {
        let c = Config::default();
        let s = serde_json::to_string_pretty(&c).unwrap();
        let c2: Config = serde_json::from_str(&s).unwrap();
        assert_eq!(c2.provider.model, "deepseek-v4-flash");
        assert_eq!(c2.provider.ctx_window, 1_000_000);
        assert!(c2.agents.enabled);
        assert_eq!(c2.agents.max_concurrent, 1);
        assert_eq!(c2.agents.max_spawn_per_turn, 1);
        assert_eq!(c2.agents.max_steps, 8);
        assert_eq!(c2.tui.compress_threshold, 0.75);
        assert_eq!(c2.qq.auto_new, 0);
        assert!(!c2.pricing.enabled);
        assert_eq!(c2.tool_times, 24);
        assert!(c2.fuckloop);
        let mut v: serde_json::Value = serde_json::from_str(&s).unwrap();
        v["qq"].as_object_mut().unwrap().remove("auto_new");
        let old: Config = serde_json::from_value(v).unwrap();
        assert_eq!(old.qq.auto_new, 0);
        let mut v: serde_json::Value = serde_json::from_str(&s).unwrap();
        v.as_object_mut().unwrap().remove("tool_times");
        let old2: Config = serde_json::from_value(v).unwrap();
        assert_eq!(old2.tool_times, default_tool_times());
        let mut v: serde_json::Value = serde_json::from_str(&s).unwrap();
        v.as_object_mut().unwrap().remove("fuckloop");
        let old_fuckloop: Config = serde_json::from_value(v).unwrap();
        assert!(old_fuckloop.fuckloop);
        let mut v: serde_json::Value = serde_json::from_str(&s).unwrap();
        v.as_object_mut().unwrap().remove("llama");
        v.as_object_mut().unwrap().remove("trace");
        let old3: Config = serde_json::from_value(v).unwrap();
        assert!(!old3.llama.auto_start);
        assert!(old3.llama.service.is_empty());
        assert_eq!(old3.llama.start_wait_secs, 180);
        assert!(old3.trace.enabled);
        assert!(!old3.trace.show_reasoning);
        let mut v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let mut provider = v["provider"].clone();
        provider.as_object_mut().unwrap().remove("ctx_override");
        v["provider"] = provider;
        let old4: Config = serde_json::from_value(v).unwrap();
        assert_eq!(old4.provider.ctx_override, None);
        let mut v: serde_json::Value = serde_json::from_str(&s).unwrap();
        v.as_object_mut().unwrap().remove("pricing");
        let old5: Config = serde_json::from_value(v).unwrap();
        assert!(!old5.pricing.enabled);
        let mut v: serde_json::Value = serde_json::from_str(&s).unwrap();
        v.as_object_mut().unwrap().remove("agents");
        let old6: Config = serde_json::from_value(v).unwrap();
        assert!(old6.agents.enabled);
        assert_eq!(old6.agents.max_concurrent, 1);
    }
    #[test]
    fn pricing_counts_cache_hits_and_misses_separately() {
        let pricing = Pricing::deepseek_flash_cny();
        let cost = pricing.estimate(1_000_000, 800_000, 200_000, 100_000).unwrap();
        assert!((cost - 0.416).abs() < 1e-9);
    }
}
