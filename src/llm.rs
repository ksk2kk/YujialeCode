use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use crate::compress;
const REASONING_LOOP_MAX: usize = 12_000;
const POST_TOOL_REASONING_MAX: usize = 512;
const PLAN_LOOP_TOOL_BLOCKS: usize = 3;
const TAIL_REPEAT_WINDOW: usize = 40;
const TAIL_REPEAT_MIN: usize = 3;
const APPROX_IMAGE_TOKENS: usize = 1_024;
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Msg {
    pub role: String,
    pub content: String,
    /// Some OpenAI-compatible reasoning APIs (notably DeepSeek) require the
    /// exact reasoning text to be echoed with an assistant tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Local screenshot path. It is persisted as a small path rather than a
    /// base64 blob; only the newest still-relevant frame is encoded for a
    /// vision-capable backend when the request is built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}
impl Msg {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Msg { role: role.into(), content: content.into(), ..Default::default() }
    }
}
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: String,
}
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta(String),
    Reasoning(String),
    TokenProgress(TokenProgress),
    Garbage { kind: &'static str, sample: String, run: usize, total: usize, limit: usize },
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenProgress {
    pub usage: Usage,
    /// False means the upload count is a local preflight estimate. True means
    /// the numbers came from the provider's usage object.
    pub exact: bool,
}
#[derive(Debug, Clone, Default)]
pub struct ChatResult {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub timings: Option<Timings>,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
pub struct Usage {
    #[serde(default, alias = "input_tokens")]
    pub prompt_tokens: usize,
    #[serde(default, alias = "output_tokens")]
    pub completion_tokens: usize,
    #[serde(default, alias = "prompt_cache_hit_tokens", alias = "cache_read_input_tokens")]
    pub cache_read_tokens: usize,
    #[serde(default, alias = "prompt_cache_miss_tokens", alias = "cache_creation_input_tokens")]
    pub cache_miss_tokens: usize,
    #[serde(default, skip_deserializing)]
    pub reasoning_tokens: usize,
    #[serde(default, skip_deserializing)]
    pub cache_prefix_hits: usize,
    #[serde(default, skip_deserializing)]
    pub cache_prefix_misses: usize,
    #[serde(default, skip_deserializing)]
    pub cache_prefix_messages: usize,
}
impl Usage {
    pub fn server_cache_percent(self) -> Option<f64> {
        if self.cache_read_tokens == 0 && self.cache_miss_tokens == 0 {
            return None;
        }
        let denominator = if self.cache_miss_tokens > 0 {
            self.cache_read_tokens.saturating_add(self.cache_miss_tokens)
        } else {
            self.prompt_tokens.max(self.cache_read_tokens)
        };
        (denominator > 0).then(|| self.cache_read_tokens as f64 * 100.0 / denominator as f64)
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Deserialize)]
pub struct Timings {
    pub predicted_n: f64,
    pub predicted_ms: f64,
}
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub messages: Vec<Msg>,
    pub tools: Option<Value>,
    pub max_tokens: Option<usize>,
    pub stream: bool,
}
#[derive(Clone)]
pub enum Llm {
    Remote(RemoteClient),
    Mock(MockClient),
}
impl Llm {
    pub fn remote(base_url: &str, api_key: &str, model: &str, timeout_secs: u64, load_context: usize) -> Self {
        Llm::Remote(RemoteClient {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: Arc::new(Mutex::new(model.to_string())),
            timeout_secs,
            load_context,
            thinking_budget: None,
            bypass_local_llamacpp_router: false,
            server_caps: Arc::new(Mutex::new(crate::backend::Capabilities::unknown())),
            last_reload: Arc::new(Mutex::new(
                std::time::Instant::now() - Duration::from_secs(60),
            )),
            cache_tracker: Arc::new(Mutex::new(RequestCacheTracker::default())),
            prompt_calibration: Arc::new(Mutex::new(PromptCalibration::default())),
        })
    }
    pub fn mock() -> Self {
        Llm::Mock(MockClient { calls: Arc::new(AtomicUsize::new(0)) })
    }
    pub fn model_name(&self) -> String {
        match self {
            Llm::Remote(c) => c.model.lock().unwrap().clone(),
            Llm::Mock(_) => "mock".into(),
        }
    }
    pub fn set_model(&self, name: String) {
        if let Llm::Remote(c) = self {
            *c.model.lock().unwrap() = name;
        }
    }
    /// Creates an inference client with an independent model selector. A plain
    /// `Clone` intentionally shares the selector for live `/model` updates;
    /// background agents must not use that clone because selecting their cheap
    /// model would silently switch the user's main conversation too.
    pub fn fork_with_model(&self, model: &str) -> Self {
        match self {
            Llm::Remote(client) => {
                let mut child = client.clone();
                child.model = Arc::new(Mutex::new(if model.trim().is_empty() {
                    client.model.lock().unwrap().clone()
                } else {
                    model.trim().to_string()
                }));
                child.cache_tracker = Arc::new(Mutex::new(RequestCacheTracker::default()));
                Llm::Remote(child)
            }
            Llm::Mock(_) => Llm::mock(),
        }
    }
    pub fn stream(
        &self,
        req: &ChatRequest,
        cancel: &AtomicBool,
        mut on_event: impl FnMut(StreamEvent),
    ) -> Result<ChatResult, String> {
        match self {
            Llm::Remote(c) => c.stream(req, cancel, &mut on_event),
            Llm::Mock(m) => m.stream(req, cancel, &mut on_event),
        }
    }
    pub fn stream_without_reasoning(
        &self,
        req: &ChatRequest,
        cancel: &AtomicBool,
        mut on_event: impl FnMut(StreamEvent),
    ) -> Result<ChatResult, String> {
        match self {
            Llm::Remote(c) => c.stream_inner(req, cancel, &mut on_event, true),
            Llm::Mock(m) => m.stream(req, cancel, &mut on_event),
        }
    }
    pub fn reload_model(&self) -> Result<(), String> {
        match self {
            Llm::Remote(c) => c.reload_model(),
            Llm::Mock(_) => Ok(()),
        }
    }
    pub fn clear_kv(&self) -> Result<(), String> {
        match self {
            Llm::Remote(c) => c.clear_kv(),
            Llm::Mock(_) => Ok(()),
        }
    }
    pub fn set_timeout(&mut self, secs: u64) {
        if let Llm::Remote(c) = self {
            c.timeout_secs = secs;
        }
    }
    pub fn set_thinking_budget(&mut self, budget: Option<usize>) {
        if let Llm::Remote(c) = self {
            c.thinking_budget = budget;
        }
    }
    pub fn set_llamacpp_router_bypass(&mut self, enabled: bool) {
        if let Llm::Remote(c) = self {
            c.bypass_local_llamacpp_router = enabled;
        }
    }
    pub fn set_base_url(&mut self, url: String) {
        if let Llm::Remote(c) = self {
            c.base_url = url.trim_end_matches('/').to_string();
        }
    }
    pub fn set_api_key(&mut self, key: String) {
        if let Llm::Remote(c) = self {
            c.api_key = key;
        }
    }
    pub fn probe_server(&self) {
        if let Llm::Remote(c) = self {
            *c.server_caps.lock().unwrap() =
                crate::backend::discover(&c.base_url, &c.api_key);
        }
    }
    pub fn n_ctx(&self) -> Option<usize> {
        match self {
            Llm::Remote(c) => c.server_caps.lock().unwrap().n_ctx,
            Llm::Mock(_) => None,
        }
    }
    pub fn is_llamacpp(&self) -> bool {
        match self {
            Llm::Remote(c) => c.server_caps.lock().unwrap().is_llamacpp(),
            Llm::Mock(_) => false,
        }
    }
    pub fn supports_vision(&self) -> bool {
        match self {
            Llm::Remote(c) => {
                let model_looks_visual = c
                    .model
                    .lock()
                    .map(|model| {
                        let model = model.to_ascii_lowercase();
                        model.contains("vision") || model.contains("vl") || model.contains("multimodal")
                    })
                    .unwrap_or(false);
                model_looks_visual || c.server_caps.lock().map(|caps| {
                    caps.modalities.iter().any(|modality| {
                        matches!(
                            modality.to_ascii_lowercase().as_str(),
                            "vision" | "image" | "images" | "image_url"
                        )
                    })
                })
                .unwrap_or(false)
            }
            Llm::Mock(_) => false,
        }
    }
    pub fn max_tokens_for(&self, kind: crate::backend::TokenBudget, qq_max: usize) -> Option<usize> {
        let caps = match self {
            Llm::Remote(c) => c.server_caps.lock().unwrap().clone(),
            Llm::Mock(_) => crate::backend::Capabilities::unknown(),
        };
        crate::backend::derive_max_tokens(&caps, kind, qq_max)
    }
    pub fn estimate_prompt_tokens(&self, req: &ChatRequest) -> usize {
        let messages: Vec<Value> = req.messages.iter().map(to_openai_msg).collect();
        let image_count = usize::from(self.supports_vision() && live_image_path(&req.messages).is_some());
        let raw = approximate_prompt_tokens(&messages, &req.tools, image_count);
        match self {
            Llm::Remote(client) => {
                let model = client.model.lock().unwrap().clone();
                client.calibrated_prompt_tokens(&model, raw)
            }
            Llm::Mock(_) => raw,
        }
    }
}
#[derive(Debug, Default)]
struct PromptCalibration {
    factors: HashMap<String, f64>,
}
impl PromptCalibration {
    fn factor(&self, model: &str) -> f64 {
        self.factors
            .get(&model.to_ascii_lowercase())
            .copied()
            .unwrap_or_else(|| default_prompt_factor(model))
    }
    fn observe(&mut self, model: &str, raw: usize, exact: usize) {
        if raw == 0 || exact == 0 {
            return;
        }
        let observed = (exact as f64 / raw as f64).clamp(0.5, 4.0);
        self.factors
            .entry(model.to_ascii_lowercase())
            .and_modify(|factor| *factor = *factor * 0.75 + observed * 0.25)
            .or_insert(observed);
    }
}
fn default_prompt_factor(model: &str) -> f64 {
    if crate::backend::known_model_context_window(model).is_some() {
        1.6
    } else {
        1.0
    }
}
#[derive(Debug, Clone, Default)]
struct RequestCacheSnapshot {
    model: String,
    tools: Option<Value>,
    messages: Vec<Value>,
    approx_prompt_tokens: usize,
}
#[derive(Debug, Clone, Copy, Default)]
struct CacheObservation {
    has_previous: bool,
    stable_prefix: bool,
    prefix_messages: usize,
    estimated_prefix_tokens: usize,
}
#[derive(Debug, Default)]
struct RequestCacheTracker {
    previous: Option<RequestCacheSnapshot>,
}
impl RequestCacheTracker {
    fn observe(&mut self, model: &str, tools: &Option<Value>, messages: &[Value]) -> CacheObservation {
        let observation = match &self.previous {
            Some(previous) => {
                let stable = previous.model == model
                    && previous.tools == *tools
                    && messages.len() >= previous.messages.len()
                    && previous.messages.iter().zip(messages).all(|(old, new)| old == new);
                CacheObservation {
                    has_previous: true,
                    stable_prefix: stable,
                    prefix_messages: if stable { previous.messages.len() } else { 0 },
                    estimated_prefix_tokens: if stable {
                        previous.approx_prompt_tokens
                    } else {
                        0
                    },
                }
            }
            None => CacheObservation::default(),
        };
        self.previous = Some(RequestCacheSnapshot {
            model: model.to_string(),
            tools: tools.clone(),
            messages: messages.to_vec(),
            approx_prompt_tokens: approximate_prompt_tokens(
                messages,
                tools,
                count_image_values(messages),
            ),
        });
        observation
    }
}
#[derive(Clone)]
pub struct RemoteClient {
    pub base_url: String,
    pub api_key: String,
    pub model: Arc<Mutex<String>>,
    pub timeout_secs: u64,
    pub load_context: usize,
    pub thinking_budget: Option<usize>,
    pub bypass_local_llamacpp_router: bool,
    pub server_caps: Arc<Mutex<crate::backend::Capabilities>>,
    pub last_reload: Arc<Mutex<std::time::Instant>>,
    cache_tracker: Arc<Mutex<RequestCacheTracker>>,
    prompt_calibration: Arc<Mutex<PromptCalibration>>,
}
enum HttpEvent {
    Headers(u16),
    Line(String),
    Body(Vec<u8>),
    Done,
    Error(String),
}
struct CancellableHttp {
    rx: mpsc::Receiver<HttpEvent>,
    abort: Arc<AtomicBool>,
}
impl CancellableHttp {
    fn post_json(
        url: String,
        api_key: String,
        payload: String,
        stream: bool,
        timeout: Duration,
    ) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let abort = Arc::new(AtomicBool::new(false));
        let worker_abort = abort.clone();
        std::thread::Builder::new()
            .name("yjlcoder-http".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        let _ = tx.send(HttpEvent::Error(format!("初始化 HTTP 运行时失败: {e}")));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let client = match reqwest::Client::builder()
                        .connect_timeout(Duration::from_secs(10))
                        .timeout(timeout)
                        .build()
                    {
                        Ok(client) => client,
                        Err(e) => {
                            let _ = tx.send(HttpEvent::Error(format!("初始化 HTTP 客户端失败: {e}")));
                            return;
                        }
                    };
                    let mut request = client
                        .post(url)
                        .header("Content-Type", "application/json")
                        .header("Accept", "text/event-stream")
                        .body(payload);
                    if !api_key.is_empty() {
                        request = request.bearer_auth(api_key);
                    }
                    let mut response = tokio::select! {
                        _ = wait_for_abort(&worker_abort) => return,
                        result = request.send() => match result {
                            Ok(response) => response,
                            Err(e) => {
                                let _ = tx.send(HttpEvent::Error(format!("请求失败: {e}")));
                                return;
                            }
                        },
                    };
                    let status = response.status().as_u16();
                    if tx.send(HttpEvent::Headers(status)).is_err() {
                        return;
                    }
                    if !stream || status != 200 {
                        let body = tokio::select! {
                            _ = wait_for_abort(&worker_abort) => return,
                            result = response.bytes() => match result {
                                Ok(body) => body.to_vec(),
                                Err(e) => {
                                    let _ = tx.send(HttpEvent::Error(format!("读取响应失败: {e}")));
                                    return;
                                }
                            },
                        };
                        if tx.send(HttpEvent::Body(body)).is_err() {
                            return;
                        }
                        let _ = tx.send(HttpEvent::Done);
                        return;
                    }
                    let mut pending = Vec::<u8>::new();
                    loop {
                        let chunk = tokio::select! {
                            _ = wait_for_abort(&worker_abort) => return,
                            result = response.chunk() => match result {
                                Ok(chunk) => chunk,
                                Err(e) => {
                                    let _ = tx.send(HttpEvent::Error(format!("读取流失败: {e}")));
                                    return;
                                }
                            },
                        };
                        let Some(chunk) = chunk else {
                            if !pending.is_empty() && send_sse_line(&tx, std::mem::take(&mut pending)).is_err() {
                                return;
                            }
                            let _ = tx.send(HttpEvent::Done);
                            return;
                        };
                        pending.extend_from_slice(&chunk);
                        while let Some(newline) = pending.iter().position(|b| *b == b'\n') {
                            let rest = pending.split_off(newline + 1);
                            let line = std::mem::replace(&mut pending, rest);
                            if send_sse_line(&tx, line).is_err() {
                                return;
                            }
                        }
                    }
                });
            })
            .map_err(|e| format!("启动 HTTP 请求线程失败: {e}"))?;
        Ok(Self { rx, abort })
    }
    fn next(&self, cancel: &AtomicBool) -> Result<HttpEvent, String> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                self.abort.store(true, Ordering::Release);
                return Err("已取消".into());
            }
            match self.rx.recv_timeout(Duration::from_millis(25)) {
                Ok(event) => return Ok(event),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("HTTP 请求线程意外结束".into());
                }
            }
        }
    }
}
impl Drop for CancellableHttp {
    fn drop(&mut self) {
        self.abort.store(true, Ordering::Release);
    }
}
async fn wait_for_abort(abort: &AtomicBool) {
    while !abort.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
fn send_sse_line(tx: &mpsc::Sender<HttpEvent>, mut bytes: Vec<u8>) -> Result<(), ()> {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    let line = String::from_utf8(bytes)
        .map_err(|e| {
            let _ = tx.send(HttpEvent::Error(format!("SSE 不是有效 UTF-8: {e}")));
        })?;
    tx.send(HttpEvent::Line(line)).map_err(|_| ())
}
fn collect_http_body(http: &CancellableHttp, cancel: &AtomicBool) -> Result<String, String> {
    let mut body = Vec::new();
    loop {
        match http.next(cancel)? {
            HttpEvent::Body(bytes) => body.extend(bytes),
            HttpEvent::Done => {
                return String::from_utf8(body)
                    .map_err(|e| format!("响应不是有效 UTF-8: {e}"));
            }
            HttpEvent::Error(e) => return Err(e),
            HttpEvent::Line(line) => {
                body.extend_from_slice(line.as_bytes());
                body.push(b'\n');
            }
            HttpEvent::Headers(_) => return Err("HTTP 响应重复发送状态行".into()),
        }
    }
}
pub(crate) fn api_origin(base_url: &str) -> &str {
    let base = base_url.trim_end_matches('/');
    let base = base.strip_suffix("/chat/completions").unwrap_or(base);
    base.strip_suffix("/v1").unwrap_or(base)
}
fn is_deepseek_api(base_url: &str) -> bool {
    let origin = api_origin(base_url).to_ascii_lowercase();
    origin == "https://api.deepseek.com" || origin == "https://api.deepseek.com/"
}
fn is_loopback_http_origin(origin: &str) -> bool {
    let Some(authority) = origin.strip_prefix("http://").map(|s| s.split('/').next().unwrap_or(s)) else {
        return false;
    };
    authority == "localhost"
        || authority.starts_with("localhost:")
        || authority == "127.0.0.1"
        || authority.starts_with("127.0.0.1:")
        || authority == "[::1]"
        || authority.starts_with("[::1]:")
}
fn backend_chat_url_from_models(origin: &str, model: &str, response: &Value) -> Option<String> {
    if !is_loopback_http_origin(origin) {
        return None;
    }
    let entry = response.get("data")?.as_array()?.iter().find(|entry| {
        entry.get("id").and_then(Value::as_str) == Some(model)
    })?;
    let status = entry.get("status")?;
    if status.get("value").and_then(Value::as_str) != Some("loaded") {
        return None;
    }
    let args = status.get("args")?.as_array()?;
    let port = args.windows(2).find_map(|pair| {
        (pair[0].as_str() == Some("--port"))
            .then(|| pair[1].as_str()?.parse::<u16>().ok())
            .flatten()
    })?;
    if port == 0 {
        return None;
    }
    Some(format!("http://127.0.0.1:{port}/v1/chat/completions"))
}
fn get_local_router_models(origin: &str, api_key: &str) -> Option<Value> {
    let mut request = ureq::get(&format!("{origin}/models?reload=1"))
        .timeout(Duration::from_secs(2));
    if !api_key.is_empty() {
        request = request.set("Authorization", &format!("Bearer {api_key}"));
    }
    let response = request.call().ok()?;
    if response.status() != 200 {
        return None;
    }
    let body = response.into_string().ok()?;
    serde_json::from_str(&body).ok()
}
fn local_router_model_state<'a>(response: &'a Value, model: &str) -> Option<&'a str> {
    response
        .get("data")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(model))?
        .get("status")?
        .get("value")?
        .as_str()
}
fn discover_local_llamacpp_backend(
    base_url: &str,
    api_key: &str,
    model: &str,
    cancel: &AtomicBool,
) -> Result<Option<String>, String> {
    let origin = api_origin(base_url);
    if !is_loopback_http_origin(origin) {
        return Ok(None);
    }
    let Some(initial) = get_local_router_models(origin, api_key) else {
        return Ok(None);
    };
    if let Some(url) = backend_chat_url_from_models(origin, model, &initial) {
        return Ok(Some(url));
    }
    let Some(state) = local_router_model_state(&initial, model) else {
        return Ok(None);
    };
    if state == "unloaded" {
        let load = CancellableHttp::post_json(
            format!("{origin}/models/load"),
            api_key.to_string(),
            json!({"model": model}).to_string(),
            false,
            Duration::from_secs(15),
        )?;
        let status = match load.next(cancel)? {
            HttpEvent::Headers(status) => status,
            HttpEvent::Error(e) => return Err(format!("加载本地模型失败: {e}")),
            _ => return Err("加载本地模型时响应缺少状态行".into()),
        };
        let body = collect_http_body(&load, cancel)?;
        if status != 200 {
            return Err(format!("加载本地模型失败（HTTP {status}）: {body}"));
        }
    } else if state != "loading" && state != "unloading" {
        return Err(format!("本地模型 {model} 状态异常: {state}"));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(180);
    while std::time::Instant::now() < deadline {
        if cancel.load(Ordering::Relaxed) {
            return Err("已取消".into());
        }
        if let Some(models) = get_local_router_models(origin, api_key) {
            if let Some(url) = backend_chat_url_from_models(origin, model, &models) {
                return Ok(Some(url));
            }
            if let Some("error") = local_router_model_state(&models, model) {
                return Err(format!("本地模型 {model} 加载失败"));
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(format!("等待本地模型 {model} 加载超时（180s）"))
}
pub fn chat_url(base_url: &str) -> String {
    let b = base_url.trim_end_matches('/');
    if b.ends_with("/chat/completions") {
        return b.to_string();
    }
    if b.ends_with("/v1") {
        return format!("{b}/chat/completions");
    }
    format!("{b}/v1/chat/completions")
}
pub fn list_models(base_url: &str, api_key: &str) -> Result<Vec<String>, String> {
    let origin = base_url.trim_end_matches('/');
    let origin = origin.strip_suffix("/chat/completions").unwrap_or(origin);
    let url = if origin.ends_with("/v1") { format!("{origin}/models") } else { format!("{origin}/v1/models") };
    let mut builder = ureq::get(&url)
        .timeout(Duration::from_secs(15))
        .set("Content-Type", "application/json");
    if !api_key.is_empty() {
        builder = builder.set("Authorization", &format!("Bearer {api_key}"));
    }
    let resp = builder.call().map_err(|e| format!("查询模型列表失败: {e}"))?;
    if resp.status() != 200 {
        return Err(format!("HTTP {}: {}", resp.status(), resp.into_string().unwrap_or_default()));
    }
    let v: Value = serde_json::from_str(&resp.into_string().map_err(|e| format!("读取响应失败: {e}"))?)
        .map_err(|e| format!("响应解析失败: {e}"))?;
    let mut ids: Vec<String> = v["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    if ids.is_empty() {
        return Err(format!("端点 {url} 未返回任何模型"));
    }
    Ok(ids)
}
impl RemoteClient {
    fn calibrated_prompt_tokens(&self, model: &str, raw: usize) -> usize {
        let factor = self
            .prompt_calibration
            .lock()
            .map(|calibration| calibration.factor(model))
            .unwrap_or_else(|_| default_prompt_factor(model));
        ((raw as f64 * factor).ceil() as usize).max(raw)
    }
    fn observe_prompt_usage(&self, model: &str, raw: usize, exact: usize) {
        if let Ok(mut calibration) = self.prompt_calibration.lock() {
            calibration.observe(model, raw, exact);
        }
    }
    fn stream(
        &self,
        req: &ChatRequest,
        cancel: &AtomicBool,
        on_event: &mut impl FnMut(StreamEvent),
    ) -> Result<ChatResult, String> {
        self.stream_inner(req, cancel, on_event, false)
    }
    fn stream_inner(
        &self,
        req: &ChatRequest,
        cancel: &AtomicBool,
        on_event: &mut impl FnMut(StreamEvent),
        disable_reasoning: bool,
    ) -> Result<ChatResult, String> {
        let model = self.model.lock().unwrap().clone();
        let deepseek_api = is_deepseek_api(&self.base_url);
        let url = if self.bypass_local_llamacpp_router {
            discover_local_llamacpp_backend(&self.base_url, &self.api_key, &model, cancel)?
                .unwrap_or_else(|| chat_url(&self.base_url))
        } else {
            chat_url(&self.base_url)
        };
        let vision_available = self
            .server_caps
            .lock()
            .map(|caps| {
                caps.modalities.iter().any(|modality| {
                    matches!(
                        modality.to_ascii_lowercase().as_str(),
                        "vision" | "image" | "images" | "image_url"
                    )
                })
            })
            .unwrap_or(false);
        let openai_messages = to_openai_messages(&req.messages, vision_available);
        let cache_observation = self
            .cache_tracker
            .lock()
            .map(|mut tracker| tracker.observe(&model, &req.tools, &openai_messages))
            .unwrap_or_default();
        let mut payload = json!({
            "model": model,
            "messages": openai_messages,
            "stream": req.stream,
        });
        if let Some(t) = &req.tools {
            payload["tools"] = t.clone();
        }
        if let Some(m) = req.max_tokens {
            payload["max_tokens"] = json!(m);
        }
        if let Some(b) = self.thinking_budget {
            if !self.server_caps.lock().unwrap().is_llamacpp() {
                payload["thinking"] = if deepseek_api {
                    json!({ "type": "enabled" })
                } else {
                    json!({ "type": "enabled", "budget_tokens": b })
                };
            }
        }
        if disable_reasoning {
            if deepseek_api {
                payload["thinking"] = json!({ "type": "disabled" });
            } else {
                payload["reasoning_effort"] = json!("none");
                payload["chat_template_kwargs"] = json!({ "enable_thinking": false });
            }
        }
        if req.stream && deepseek_api {
            payload["stream_options"] = json!({ "include_usage": true });
        }
        let payload_text = payload.to_string();
        let image_count = usize::from(live_image_path(&req.messages).is_some() && vision_available);
        let raw_prompt_tokens =
            approximate_prompt_tokens(&openai_messages, &req.tools, image_count);
        let estimated_prompt_tokens = self.calibrated_prompt_tokens(&model, raw_prompt_tokens);
        let estimated_cache_read = self
            .calibrated_prompt_tokens(&model, cache_observation.estimated_prefix_tokens)
            .min(estimated_prompt_tokens);
        on_event(StreamEvent::TokenProgress(TokenProgress {
            usage: Usage {
                prompt_tokens: estimated_prompt_tokens,
                cache_read_tokens: estimated_cache_read,
                cache_miss_tokens: estimated_prompt_tokens.saturating_sub(estimated_cache_read),
                ..Usage::default()
            },
            exact: false,
        }));
        let http = CancellableHttp::post_json(
            url,
            self.api_key.clone(),
            payload_text,
            req.stream,
            Duration::from_secs(self.timeout_secs),
        )?;
        let status = match http.next(cancel)? {
            HttpEvent::Headers(status) => status,
            HttpEvent::Error(e) => return Err(e),
            _ => return Err("HTTP 响应缺少状态行".into()),
        };
        if status != 200 {
            let body = collect_http_body(&http, cancel)?;
            return Err(format!("HTTP {status}: {body}"));
        }
        if !req.stream {
            let body = collect_http_body(&http, cancel)?;
            let mut result = parse_full_response(&body, on_event)?;
            if let Some(usage) = result.usage {
                self.observe_prompt_usage(&model, raw_prompt_tokens, usage.prompt_tokens);
                on_event(StreamEvent::TokenProgress(TokenProgress { usage, exact: true }));
            }
            attach_cache_observation(&mut result, cache_observation);
            return Ok(result);
        }
        let mut result = ChatResult::default();
        let mut garbage_run: usize = 0;                                   
        let mut garbage_total: usize = 0;                     
        let mut reasoning_garbage_run: usize = 0;                  
        let mut reasoning_garbage_total: usize = 0;                
        let mut tool_bufs: std::collections::HashMap<usize, (String, String, String)> =
            std::collections::HashMap::new();                             
        loop {
            let line = match http.next(cancel)? {
                HttpEvent::Line(line) => line,
                HttpEvent::Done => break,
                HttpEvent::Error(e) => return Err(e),
                HttpEvent::Body(_) | HttpEvent::Headers(_) => {
                    return Err("流式响应协议异常".into());
                }
            };
            let line = line.trim();
            if !line.starts_with("data:") {
                continue;
            }
            let data = line[5..].trim();
            if data == "[DONE]" {
                break;
            }
            let v: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(e) => return Err(format!("SSE 解析失败: {e} ({data:?})")),
            };
            if let Some(err) = v.get("error") {
                return Err(format!("API 错误: {}", err));
            }
            if let Some(usage_value) = v.get("usage").filter(|usage| usage.is_object()) {
                let usage = parse_usage(usage_value);
                self.observe_prompt_usage(&model, raw_prompt_tokens, usage.prompt_tokens);
                result.usage = Some(usage);
                on_event(StreamEvent::TokenProgress(TokenProgress { usage, exact: true }));
            }
            if let Some(timings) = v.get("timings") {
                result.timings = serde_json::from_value::<Timings>(timings.clone()).ok();
            }
            let Some(choice) = v.pointer("/choices/0") else { continue };
            let delta = &choice["delta"];
            if let Some(tc) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for call in tc {
                    let idx = call["index"].as_u64().unwrap_or(0) as usize;
                    let entry = tool_bufs.entry(idx).or_default();
                    if let Some(id) = call["id"].as_str() {
                        entry.0 = id.to_string();
                    }
                    if let Some(n) = call["function"]["name"].as_str() {
                        entry.1 = n.to_string();
                    }
                    if let Some(a) = call["function"]["arguments"].as_str() {
                        entry.2.push_str(a);
                    }
                }
            }
            if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                let chars: Vec<char> = content.chars().collect();
                let junk = content.contains("<unused")
                    || (chars.len() >= 4
                        && (chars.iter().all(|c| *c == chars[0])                           
                            || chars.iter().all(|c| c.is_whitespace() || c.is_control())))         
                    ;
                if junk {
                    garbage_run += 1;
                    garbage_total += 1;
                    on_event(StreamEvent::Garbage {
                        kind: "正文",
                        sample: content.chars().take(40).collect(),
                        run: garbage_run,
                        total: garbage_total,
                        limit: 8,
                    });
                    if garbage_run >= 8 {
                        return Err("模型输出异常（垃圾 token），已中止请求".into());
                    }
                } else {
                    garbage_run = 0;
                }
                result.text.push_str(content);
                on_event(StreamEvent::Delta(content.to_string()));
            }
            if let Some(r) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                result.reasoning.push_str(r);
                let chars: Vec<char> = r.chars().collect();
                let junk = r.contains("<unused")
                    || (chars.len() >= 4
                        && (chars.iter().all(|c| *c == chars[0])         
                            || chars.iter().all(|c| c.is_whitespace() || c.is_control())));         
                if junk {
                    reasoning_garbage_run += 1;
                    reasoning_garbage_total += 1;
                    on_event(StreamEvent::Garbage {
                        kind: "思考流",
                        sample: r.chars().take(40).collect(),
                        run: reasoning_garbage_run,
                        total: reasoning_garbage_total,
                        limit: 0,          
                    });
                } else {
                    reasoning_garbage_run = 0;
                }
                on_event(StreamEvent::Reasoning(r.to_string()));
                let reasoning_limit = if disable_reasoning {
                    POST_TOOL_REASONING_MAX
                } else {
                    REASONING_LOOP_MAX
                };
                if !deepseek_api {
                    if result.reasoning.chars().count() > reasoning_limit {
                        break;
                    }
                    if count_complete_tool_blocks(&result.reasoning) >= PLAN_LOOP_TOOL_BLOCKS {
                        break;
                    }
                    if tail_repeats(&result.reasoning, TAIL_REPEAT_WINDOW, TAIL_REPEAT_MIN) {
                        break;
                    }
                }
            }
        }
        let mut idxs: Vec<usize> = tool_bufs.keys().copied().collect();
        idxs.sort_unstable();
        for i in idxs {
            let (id, name, args) = tool_bufs.remove(&i).unwrap();
            if name.is_empty() {
                continue;
            }
            result.tool_calls.push(ToolCall { id, name, args });
        }
        attach_cache_observation(&mut result, cache_observation);
        Ok(result)
    }
    fn reload_model(&self) -> Result<(), String> {
        let mut last = self.last_reload.lock().unwrap();
        if last.elapsed() < Duration::from_secs(60) {
            return Err(format!(
                "模型上次重载仅 {}s 前（60s 冷却防重载风暴），请稍后再试",
                last.elapsed().as_secs()
            ));
        }
        *last = std::time::Instant::now();
        drop(last);
        self.reload_impl()
    }
    fn clear_kv(&self) -> Result<(), String> {
        {
            let last = self.last_reload.lock().unwrap();
            if last.elapsed() < Duration::from_secs(10) {
                return Err(format!(
                    "KV 清理与上次重载仅间隔 {}s（防打断加载），请稍后再试",
                    last.elapsed().as_secs()
                ));
            }
        }
        *self.last_reload.lock().unwrap() = std::time::Instant::now();
        self.reload_impl()
    }
    fn reload_impl(&self) -> Result<(), String> {
        let origin = {
            let b = self.base_url.trim_end_matches('/');
            let b = b.strip_suffix("/chat/completions").unwrap_or(b);
            b.strip_suffix("/v1").unwrap_or(b)
        };
        let model = self.model.lock().unwrap().clone();
        let auth = if self.api_key.is_empty() {
            None
        } else {
            Some(format!("Bearer {}", self.api_key))
        };
        let post = |url: String, body: &Value, timeout: Duration| -> Result<(), String> {
            let mut builder = ureq::post(&url)
                .timeout(timeout)
                .set("Content-Type", "application/json");
            if let Some(a) = &auth {
                builder = builder.set("Authorization", a);
            }
            let resp = builder
                .send_string(&body.to_string())
                .map_err(|e| format!("重载请求失败: {e}"))?;
            if resp.status() != 200 {
                return Err(format!("重载 HTTP {}", resp.status()));
            }
            Ok(())
        };
        let llama_ok = post(
            format!("{origin}/models/unload"),
            &json!({ "model": model }),
            Duration::from_secs(15),
        )
        .map_err(|e| format!("卸载请求失败: {e}"))
        .and_then(|_| {
            wait_model_state(origin, &model, "unloaded", 30, &auth)?;
            let mut last_err = String::new();
            for attempt in 0..4 {
                match post(
                    format!("{origin}/models/load"),
                    &json!({ "model": model }),
                    Duration::from_secs(180),
                ) {
                    Ok(()) => {
                        return wait_model_state(origin, &model, "loaded", 150, &auth)
                            .map_err(|e| format!("等待加载完成失败: {e}"));
                    }
                    Err(e) => {
                        last_err = e;
                        if attempt < 3 {
                            std::thread::sleep(Duration::from_secs(2));
                        }
                    }
                }
            }
            Err(format!("加载请求失败: {last_err}"))
        });
        if let Err(main_err) = llama_ok {
            let base = format!("{origin}/api/v1");
            let _ = post(
                format!("{base}/models/unload"),
                &json!({ "instance_id": model }),
                Duration::from_secs(15),
            );
            match post(
                format!("{base}/models/load"),
                &json!({ "model": model, "context_length": self.load_context }),
                Duration::from_secs(180),
            ) {
                Ok(()) => {}
                Err(fallback_err) => {
                    return Err(format!("{main_err}；LM Studio 回落也失败: {fallback_err}"));
                }
            }
        }
        Ok(())
    }
}
fn wait_model_state(origin: &str, model: &str, want: &str, timeout_secs: u64, auth: &Option<String>) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    let mut last_issue = String::new();
    while std::time::Instant::now() < deadline {
        let mut builder = ureq::get(&format!("{origin}/models?reload=1"))
            .timeout(Duration::from_secs(10));
        if let Some(a) = auth {
            builder = builder.set("Authorization", a);
        }
        let state = match builder.call() {
            Ok(r) => {
                if r.status() == 404 {
                    return Err("模型状态接口不存在（非路由器模式）".into());
                }
                match r.into_string() {
                    Ok(body) => serde_json::from_str::<Value>(&body).ok().and_then(|v| {
                        v.get("data").and_then(|d| d.as_array()).and_then(|arr| {
                            arr.iter()
                                .find(|m| m.get("id").and_then(|i| i.as_str()) == Some(model))
                                .and_then(|m| m.get("status").and_then(|s| s.get("value")).and_then(|v| v.as_str()).map(String::from))
                        })
                    }),
                    Err(_) => None,
                }
            }
            Err(_) => None,
        };
        match state {
            Some(s) if s == want => return Ok(()),
            Some(s) => last_issue = format!("当前状态 {s}"),
            None => last_issue = "状态解析失败（模型未发现？）".into(),
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err(format!("等待模型状态 {want} 超时（{timeout_secs}s，{last_issue}）"))
}
fn tail_repeats(s: &str, window: usize, min: usize) -> bool {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < window * min {
        return false;
    }
    let tail: String = chars[chars.len() - window..].iter().collect();
    s.matches(&tail).count() >= min
}
pub fn count_complete_tool_blocks(s: &str) -> usize {
    let mut n = 0;
    let mut search_from = 0;
    while let Some(fence_start) = find_tool_fence(s, search_from) {
        let content_start = fence_start + 7;             
        let Some(rel_end) = s[content_start..].find("```") else { break };
        let candidate = &s[content_start..content_start + rel_end];
        if serde_json::from_str::<Value>(candidate.trim()).is_ok() {
            n += 1;
        }
        search_from = content_start + rel_end + 3;
    }
    n
}
fn find_tool_fence(text: &str, from: usize) -> Option<usize> {
    for (off, _) in text[from..].match_indices("```") {
        let abs = from + off;
        let after: Vec<char> = text[abs + 3..].chars().take(4).collect();
        if after.len() >= 4
            && after[0].eq_ignore_ascii_case(&'t')
            && after[1].eq_ignore_ascii_case(&'o')
            && after[2].eq_ignore_ascii_case(&'o')
            && after[3].eq_ignore_ascii_case(&'l')
        {
            return Some(abs);
        }
    }
    None
}
fn parse_usage(value: &Value) -> Usage {
    let mut usage = serde_json::from_value::<Usage>(value.clone()).unwrap_or_default();
    if usage.cache_read_tokens == 0 {
        usage.cache_read_tokens = value
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
    }
    if usage.cache_miss_tokens == 0
        && usage.prompt_tokens >= usage.cache_read_tokens
        && value.pointer("/prompt_tokens_details/cached_tokens").is_some()
    {
        usage.cache_miss_tokens = usage.prompt_tokens - usage.cache_read_tokens;
    }
    usage.reasoning_tokens = value
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    usage
}
fn attach_cache_observation(result: &mut ChatResult, observation: CacheObservation) {
    if !observation.has_previous && result.usage.is_none() {
        return;
    }
    let mut usage = result.usage.unwrap_or_default();
    if observation.has_previous {
        if observation.stable_prefix {
            usage.cache_prefix_hits = 1;
            usage.cache_prefix_messages = observation.prefix_messages;
        } else {
            usage.cache_prefix_misses = 1;
        }
    }
    result.usage = Some(usage);
}
fn parse_full_response(body: &str, on_event: &mut impl FnMut(StreamEvent)) -> Result<ChatResult, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("响应解析失败: {e}"))?;
    if let Some(err) = v.get("error") {
        return Err(format!("API 错误: {}", err));
    }
    let Some(msg) = v.pointer("/choices/0/message") else {
        return Err(format!("响应缺少 message: {}", &body[..body.len().min(200)]));
    };
    let mut result = ChatResult::default();
    if let Some(usage) = v.get("usage") {
        result.usage = Some(parse_usage(usage));
    }
    if let Some(timings) = v.get("timings") {
        result.timings = serde_json::from_value::<Timings>(timings.clone()).ok();
    }
    if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
        result.text = content.to_string();
        for chunk in split_chunks(content) {
            on_event(StreamEvent::Delta(chunk));
        }
    }
    if let Some(reasoning) = msg.get("reasoning_content").and_then(|content| content.as_str()) {
        result.reasoning = reasoning.to_string();
        for chunk in split_chunks(reasoning) {
            on_event(StreamEvent::Reasoning(chunk));
        }
    }
    if let Some(calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        for call in calls {
            let id = call["id"].as_str().unwrap_or("").to_string();
            let name = call["function"]["name"].as_str().unwrap_or("").to_string();
            let args = call["function"]["arguments"].as_str().unwrap_or("").to_string();
            if !name.is_empty() {
                result.tool_calls.push(ToolCall { id, name, args });
            }
        }
    }
    Ok(result)
}
#[derive(Clone)]
pub struct MockClient {
    calls: Arc<AtomicUsize>,
}
impl MockClient {
    fn stream(
        &self,
        req: &ChatRequest,
        _cancel: &AtomicBool,
        on_event: &mut impl FnMut(StreamEvent),
    ) -> Result<ChatResult, String> {
        let summarized = req.messages.iter().any(|m| m.content.contains(compress::SUMMARIZATION_PROMPT));
        if summarized {
            let text = "用户的目标是测试 YJLcoder。已完成：配置加载、工具注册、会话持久化、上下文压缩。下一步：接入真实模型与 QQ 桥接。".to_string();
            for chunk in split_chunks(&text) {
                on_event(StreamEvent::Delta(chunk));
            }
            return Ok(ChatResult { text, ..Default::default() });
        }
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let text = if n == 0 {
            "我需要先查看可用工具。\n```tool {\"op\":\"execute_command\",\"args\":{\"cmd\":\"echo mock-ok\"}}\n```\n"
        } else {
            "mock 模式完成。工具调用已执行，一切正常。"
        };
        for chunk in split_chunks(text) {
            on_event(StreamEvent::Delta(chunk));
        }
        Ok(ChatResult { text: text.into(), ..Default::default() })
    }
}
fn split_chunks(s: &str) -> Vec<String> {
    s.chars().collect::<Vec<_>>().chunks(3).map(|c| c.iter().collect()).collect()
}
fn to_openai_msg(m: &Msg) -> Value {
    let mut v = json!({"role": m.role, "content": m.content});
    if let Some(reasoning) = &m.reasoning_content {
        v["reasoning_content"] = json!(reasoning);
    }
    if !m.tool_calls.is_empty() {
        v["tool_calls"] = json!(m
            .tool_calls
            .iter()
            .map(|t| json!({
                "id": t.id,
                "type": "function",
                "function": { "name": t.name, "arguments": t.args }
            }))
            .collect::<Vec<_>>());
    }
    if let Some(id) = &m.tool_call_id {
        v["tool_call_id"] = json!(id);
    }
    v
}

fn approximate_prompt_tokens(
    messages: &[Value],
    tools: &Option<Value>,
    image_count: usize,
) -> usize {
    let normalized: Vec<Value> = messages
        .iter()
        .cloned()
        .map(|mut message| {
            if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
                for part in parts {
                    if part.get("type").and_then(Value::as_str) == Some("image_url") {
                        if let Some(url) = part.pointer_mut("/image_url/url") {
                            *url = json!("<image>");
                        }
                    }
                }
            }
            message
        })
        .collect();
    compress::approx_token_count(&json!({"messages": normalized, "tools": tools}).to_string())
        .saturating_add(image_count.saturating_mul(APPROX_IMAGE_TOKENS))
}

fn count_image_values(messages: &[Value]) -> usize {
    messages
        .iter()
        .filter_map(|message| message.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("image_url"))
        .count()
}

fn live_image_path(messages: &[Msg]) -> Option<&str> {
    let (index, path) = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| message.image_path.as_deref().map(|path| (index, path)))?;
    if messages[index + 1..]
        .iter()
        .any(|message| matches!(message.role.as_str(), "assistant" | "user"))
    {
        None
    } else {
        Some(path)
    }
}

fn to_openai_messages(messages: &[Msg], vision_available: bool) -> Vec<Value> {
    let mut out: Vec<Value> = messages.iter().map(to_openai_msg).collect();
    if !vision_available {
        return out;
    }

    // An old screenshot must not unexpectedly reappear on a later user turn.
    // Keep it live only while no later assistant/user message has superseded
    // the tool result. Trailing native tool responses are allowed.
    let Some(path) = live_image_path(messages) else {
        return out;
    };

    if let Ok(data_url) = crate::computer_use::image_data_url(path) {
        out.push(json!({
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": "这是 computer_use 刚返回的最新 Wayland 截图。动作坐标使用工具结果里的 frame_id 和截图像素坐标。"
                },
                {
                    "type": "image_url",
                    "image_url": { "url": data_url }
                }
            ]
        }));
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chat_url_normalization() {
        assert_eq!(chat_url("http://localhost:1234"), "http://localhost:1234/v1/chat/completions");
        assert_eq!(chat_url("https://api.deepseek.com"), "https://api.deepseek.com/v1/chat/completions");
        assert_eq!(chat_url("http://localhost:11434/v1"), "http://localhost:11434/v1/chat/completions");
        assert_eq!(chat_url("http://localhost:11434/v1/"), "http://localhost:11434/v1/chat/completions");
        assert_eq!(
            chat_url("https://host/custom/chat/completions"),
            "https://host/custom/chat/completions"
        );
    }
    #[test]
    fn usage_parses_deepseek_and_openai_cache_shapes() {
        let deepseek = parse_usage(&json!({
            "prompt_tokens": 1000,
            "completion_tokens": 20,
            "prompt_cache_hit_tokens": 800,
            "prompt_cache_miss_tokens": 200,
            "completion_tokens_details": {"reasoning_tokens": 12}
        }));
        assert_eq!(deepseek.cache_read_tokens, 800);
        assert_eq!(deepseek.cache_miss_tokens, 200);
        assert_eq!(deepseek.server_cache_percent(), Some(80.0));
        assert_eq!(deepseek.reasoning_tokens, 12);
        let openai = parse_usage(&json!({
            "prompt_tokens": 500,
            "completion_tokens": 10,
            "prompt_tokens_details":{"cached_tokens":450}
        }));
        assert_eq!(openai.cache_read_tokens, 450);
        assert_eq!(openai.cache_miss_tokens, 50);
        assert_eq!(openai.server_cache_percent(), Some(90.0));
        let anthropic = parse_usage(&json!({
            "input_tokens": 300,
            "output_tokens": 40,
            "cache_read_input_tokens": 250,
            "cache_creation_input_tokens": 50
        }));
        assert_eq!(anthropic.prompt_tokens, 300);
        assert_eq!(anthropic.completion_tokens, 40);
        assert_eq!(anthropic.cache_read_tokens, 250);
        assert_eq!(anthropic.cache_miss_tokens, 50);
    }
    #[test]
    fn request_cache_tracker_requires_exact_append_only_prefix() {
        let tools = Some(json!([{"name":"execute_command"},{"name":"list_tools"}]));
        let mut tracker = RequestCacheTracker::default();
        let first = vec![json!({"role":"system","content":"stable"}), json!({"role":"user","content":"a"})];
        assert!(!tracker.observe("m", &tools, &first).has_previous);
        let mut extended = first.clone();
        extended.push(json!({"role":"assistant","content":"b"}));
        let hit = tracker.observe("m", &tools, &extended);
        assert!(hit.stable_prefix);
        assert_eq!(hit.prefix_messages, 2);
        let changed = vec![json!({"role":"system","content":"changed"})];
        let miss = tracker.observe("m", &tools, &changed);
        assert!(miss.has_previous);
        assert!(!miss.stable_prefix);
    }
    #[test]
    fn prompt_estimate_counts_all_message_fields_and_native_tools() {
        let mut assistant = Msg::new("assistant", "调用工具");
        assistant.reasoning_content = Some("内部推理".repeat(100));
        assistant.tool_calls.push(ToolCall {
            id: "call_1".into(),
            name: "execute_command".into(),
            args: r#"{"cmd":"printf very-long-argument"}"#.repeat(100),
        });
        let messages = vec![Msg::new("system", "系统提示"), assistant];
        let without_tools = ChatRequest {
            messages: messages.clone(),
            tools: None,
            max_tokens: None,
            stream: true,
        };
        let with_tools = ChatRequest {
            messages,
            tools: Some(json!([{
                "type": "function",
                "function": {
                    "name": "execute_command",
                    "description": "执行命令".repeat(100),
                    "parameters": {"type": "object"}
                }
            }])),
            max_tokens: None,
            stream: true,
        };
        let llm = Llm::mock();
        let content_only = compress::approx_total_tokens(&without_tools.messages);
        let full_without_tools = llm.estimate_prompt_tokens(&without_tools);
        let full_with_tools = llm.estimate_prompt_tokens(&with_tools);
        assert!(full_without_tools > content_only, "应计入角色、推理和工具参数");
        assert!(full_with_tools > full_without_tools, "应计入原生工具定义");
    }
    #[test]
    fn deepseek_prompt_estimate_learns_from_server_usage() {
        let mut calibration = PromptCalibration::default();
        assert_eq!(calibration.factor("deepseek-v4-flash"), 1.6);
        calibration.observe("deepseek-v4-flash", 400, 660);
        assert!((calibration.factor("deepseek-v4-flash") - 1.65).abs() < 1e-9);
        assert_eq!(calibration.factor("other-model"), 1.0);
    }
    #[test]
    fn local_llamacpp_router_backend_is_discovered_safely() {
        let models = json!({
            "data": [{
                "id": "other",
                "status": {"value": "unloaded", "args": ["--port", "0"]}
            }, {
                "id": "wanted",
                "status": {
                    "value": "loaded",
                    "args": ["llama-server", "--host", "127.0.0.1", "--port", "42559"]
                }
            }]
        });
        assert_eq!(
            backend_chat_url_from_models("http://127.0.0.1:8080", "wanted", &models)
                .as_deref(),
            Some("http://127.0.0.1:42559/v1/chat/completions")
        );
        assert_eq!(
            backend_chat_url_from_models("http://localhost:8080", "wanted", &models)
                .as_deref(),
            Some("http://127.0.0.1:42559/v1/chat/completions")
        );
        assert!(
            backend_chat_url_from_models("http://192.168.1.86:8080", "wanted", &models)
                .is_none(),
            "远程路由器返回的 loopback 子端口不能在客户端盲连"
        );
        assert!(
            backend_chat_url_from_models("http://127.0.0.1:8080", "other", &models)
                .is_none(),
            "未加载模型不能生成动态后端 URL"
        );
    }
    #[test]
    fn parse_full_response_content_and_tools() {
        let body = r#"{
          "choices": [{
            "message": {
              "role": "assistant",
              "content": "我需要查看工具",
              "reasoning_content": "thinking...",
              "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": { "name": "list_tools", "arguments": "{\"category\":\"file\"}" }
              }]
            }
          }]
        }"#;
        let mut deltas = 0usize;
        let r = parse_full_response(body, &mut |ev| {
            if let StreamEvent::Delta(d) = ev {
                deltas += d.chars().count();
            }
        })
        .unwrap();
        assert_eq!(r.text, "我需要查看工具");
        assert_eq!(r.reasoning, "thinking...");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].id, "call_1");
        assert_eq!(r.tool_calls[0].name, "list_tools");
        assert_eq!(r.tool_calls[0].args, r#"{"category":"file"}"#);
        assert_eq!(deltas, r.text.chars().count(), "非流式也应以增量回调交付");
    }
    #[test]
    fn parse_full_response_api_error() {
        let r = parse_full_response(r#"{"error":{"message":"model not found"}}"#, &mut |_| {});
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("model not found"));
    }
    #[test]
    fn mock_scripts_two_calls() {
        let llm = Llm::mock();
        let cancel = AtomicBool::new(false);
        let r1 = llm
            .stream(
                &ChatRequest { messages: vec![Msg::new("user", "hi")], tools: None, max_tokens: None, stream: true },
                &cancel,
                |_| {},
            )
            .unwrap();
        assert!(r1.text.contains("```tool"));
        let r2 = llm
            .stream(
                &ChatRequest { messages: vec![Msg::new("user", "hi")], tools: None, max_tokens: None, stream: true },
                &cancel,
                |_| {},
            )
            .unwrap();
        assert!(!r2.text.contains("```tool"));
    }
    #[test]
    fn to_openai_msg_native() {
        let mut m = Msg::new("assistant", "");
        m.reasoning_content = Some("exact thought".into());
        m.tool_calls.push(ToolCall {
            id: "c1".into(),
            name: "execute_command".into(),
            args: r#"{"cmd":"ls"}"#.into(),
        });
        let v = to_openai_msg(&m);
        assert_eq!(v["tool_calls"][0]["function"]["name"], "execute_command");
        assert_eq!(v["reasoning_content"], "exact thought");
    }
    #[test]
    fn old_session_without_image_path_still_deserializes() {
        let message: Msg = serde_json::from_str(r#"{"role":"user","content":"hello"}"#).unwrap();
        assert_eq!(message.content, "hello");
        assert!(message.image_path.is_none());
    }
    #[test]
    fn newest_live_screenshot_is_attached_once_for_vision() {
        let path = std::env::temp_dir().join(format!(
            "yjlcoder_llm_image_{}_{}.png",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&path, b"png-bytes").unwrap();
        let mut tool = Msg::new("tool", "frame ready");
        tool.image_path = Some(path.to_string_lossy().into_owned());
        tool.tool_call_id = Some("call-1".into());
        let messages = vec![Msg::new("assistant", ""), tool];

        let plain = to_openai_messages(&messages, false);
        assert_eq!(plain.len(), 2, "纯文本模型不应收到图片消息");

        let vision = to_openai_messages(&messages, true);
        assert_eq!(vision.len(), 3);
        assert_eq!(vision[2]["content"][1]["type"], "image_url");
        assert!(vision[2]["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));

        let mut superseded = messages;
        superseded.push(Msg::new("assistant", "已经看过"));
        assert_eq!(to_openai_messages(&superseded, true).len(), 3);
        let _ = std::fs::remove_file(path);
    }
    fn serve_sse(chunks: Vec<String>) -> u16 {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Some(conn) = listener.incoming().next() {
                let Ok(mut s) = conn else { return };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
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
    fn cancel_while_waiting_for_headers_closes_server_connection() {
        use std::io::{ErrorKind, Read};
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (closed_tx, closed_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
            let mut buf = [0u8; 8192];
            loop {
                match socket.read(&mut buf) {
                    Ok(0) => {
                        let _ = closed_tx.send(());
                        return;
                    }
                    Ok(_) => {}
                    Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                    Err(_) => {
                        let _ = closed_tx.send(());
                        return;
                    }
                }
            }
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let request = std::thread::spawn(move || {
            let llm = Llm::remote(
                &format!("http://127.0.0.1:{port}/v1"),
                "k",
                "m",
                1000,
                1024,
            );
            llm.stream(
                &ChatRequest {
                    messages: vec![Msg::new("user", "hi")],
                    tools: None,
                    max_tokens: Some(16_384),
                    stream: true,
                },
                &worker_cancel,
                |_| {},
            )
        });
        std::thread::sleep(Duration::from_millis(150));
        let started = Instant::now();
        cancel.store(true, Ordering::Relaxed);
        let result = request.join().unwrap();
        assert_eq!(result.unwrap_err(), "已取消");
        assert!(started.elapsed() < Duration::from_secs(1), "取消返回过慢");
        closed_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("取消后服务端应观察到 TCP 断连并释放 slot");
    }
    #[test]
    fn stream_garbage_content_reported_and_aborted() {
        let mut chunks: Vec<String> = vec![r#"{"choices":[{"delta":{"content":"你好"}}]}"#.into()];
        for _ in 0..9 {
            chunks.push(r#"{"choices":[{"delta":{"content":"<unused42>"}}]}"#.into());
        }
        let port = serve_sse(chunks);
        let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k", "m", 10, 1024);
        let cancel = AtomicBool::new(false);
        let mut events = Vec::new();
        let r = llm.stream(
            &ChatRequest { messages: vec![Msg::new("user", "hi")], tools: None, max_tokens: None, stream: true },
            &cancel,
            |ev| events.push(ev),
        );
        assert!(r.is_err(), "连续垃圾应中止请求: {r:?}");
        assert!(r.unwrap_err().contains("垃圾 token"));
        let garb: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Garbage { kind, sample, run, total, limit } => {
                    Some((*kind, sample.clone(), *run, *total, *limit))
                }
                _ => None,
            })
            .collect();
        assert_eq!(garb.len(), 8, "应实时上报 8 次垃圾事件");
        let (kind, sample, run, total, limit) = &garb[0];
        assert_eq!(*kind, "正文");
        assert_eq!(sample, "<unused42>");
        assert_eq!(*run, 1, "连续计数从 1 递增");
        assert_eq!(*total, 1, "累计计数从 1 递增");
        assert_eq!(*limit, 8);
        assert_eq!(garb[7].2, 8, "最后一次连续计数为 8");
        assert_eq!(garb[7].3, 8, "累计计数与连续一致（全程未被打断）");
        assert!(
            events.iter().any(|e| matches!(e, StreamEvent::Delta(d) if d == "你好")),
            "正常 chunk 仍应送达 Delta"
        );
    }
    #[test]
    fn stream_reasoning_symbols_never_abort() {
        let mut chunks: Vec<String> = Vec::new();
        for _ in 0..9 {
            chunks.push(r#"{"choices":[{"delta":{"reasoning_content":"/////"}}]}"#.to_string());
        }
        for _ in 0..9 {
            chunks.push(r#"{"choices":[{"delta":{"reasoning_content":"-"}}]}"#.to_string());
        }
        let port = serve_sse(chunks);
        let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k", "m", 10, 1024);
        let cancel = AtomicBool::new(false);
        let mut events = Vec::new();
        let r = llm.stream(
            &ChatRequest { messages: vec![Msg::new("user", "hi")], tools: None, max_tokens: None, stream: true },
            &cancel,
            |ev| events.push(ev),
        );
        let result = r.expect("思考流垃圾永不中止（limit=0）");
        let garb: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Garbage { kind, sample, run, total, limit } => {
                    Some((*kind, sample.clone(), *run, *total, *limit))
                }
                _ => None,
            })
            .collect();
        assert_eq!(garb.len(), 9, "5 连 / 的 chunk 各上报一次（仅记录）");
        let (kind, sample, run, total, limit) = &garb[0];
        assert_eq!(*kind, "思考流");
        assert_eq!(sample, "/////");
        assert_eq!(*run, 1);
        assert_eq!(*total, 1);
        assert_eq!(*limit, 0, "思考流 limit=0 = 仅记录不中止");
        assert_eq!(garb[8].2, 9, "9 连 / 的连续计数到 9 也不中止");
        assert!(events.iter().any(|e| matches!(e, StreamEvent::Reasoning(s) if s == "-")), "单符号思考增量照常送达");
        assert_eq!(result.reasoning, "/////".repeat(9) + &"-".repeat(9), "思考流全文收集");
    }
    #[test]
    fn stream_reasoning_loop_breaks_stream() {
        let mut chunks: Vec<String> = Vec::new();
        for i in 0..400 {
            let unit = format!("详细思考步骤 {i}：假设情形 A 成立，则处理方式为 B，结果 C 满足预期。\n");
            chunks.push(
                json!({"choices":[{"delta":{"reasoning_content": unit}}]}).to_string(),
            );
        }
        let port = serve_sse(chunks);
        let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k", "m", 10, 1024);
        let cancel = AtomicBool::new(false);
        let mut events = Vec::new();
        let r = llm.stream(
            &ChatRequest { messages: vec![Msg::new("user", "hi")], tools: None, max_tokens: None, stream: true },
            &cancel,
            |ev| events.push(ev),
        );
        let result = r.expect("思考流超限应正常返回（不 Err）");
        assert!(result.text.is_empty(), "死循环时正文应为空");
        let n = result.reasoning.chars().count();
        assert!(n > 12_000, "超限后应停止收集，实际 {n}");
        let reasoning_events = events.iter().filter(|e| matches!(e, StreamEvent::Reasoning(_))).count();
        assert!(reasoning_events < 400, "超限后剩余 chunk 不再收集，实际 {reasoning_events}");
    }
    #[test]
    fn stream_end_phrase_loop_breaks_stream() {
        let unit = "好的。\n...\n（结束）\n...\n回复。\n...\n";
        assert_eq!(unit.chars().count(), 25, "复刻实测循环单元");
        let mut chunks: Vec<String> = Vec::new();
        for _ in 0..100 {
            chunks.push(
                json!({"choices":[{"delta":{"reasoning_content": unit}}]}).to_string(),
            );
        }
        let port = serve_sse(chunks);
        let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k", "m", 10, 1024);
        let cancel = AtomicBool::new(false);
        let mut events = Vec::new();
        let r = llm.stream(
            &ChatRequest { messages: vec![Msg::new("user", "hi")], tools: None, max_tokens: None, stream: true },
            &cancel,
            |ev| events.push(ev),
        );
        let result = r.expect("尾部重复应正常返回（不 Err）");
        assert!(result.text.is_empty(), "死循环时正文应为空");
        let n = result.reasoning.chars().count();
        assert!(n < 12_000, "应远早于字符阈值截断，实际 {n}");
        assert!(n < 1000, "尾部重复应在少量循环后就截断，实际 {n}");
        let reasoning_events = events.iter().filter(|e| matches!(e, StreamEvent::Reasoning(_))).count();
        assert!(reasoning_events < 100, "剩余 chunk 不再收集，实际 {reasoning_events}");
        assert!(reasoning_events > 3, "至少收集了几个单元才判定循环，实际 {reasoning_events}");
    }
    #[test]
    fn stream_plan_loop_breaks_on_tool_blocks() {
        let blocks = [
            "```tool {\"op\":\"execute_command\",\"args\":{\"cmd\":\"ls ~/.config/noctalia/\"}}\n```\n先执行这个，最简单直接。\n\n",
            "```tool {\"op\":\"execute_command\",\"args\":{\"cmd\":\"find ~/.config -name '*noctalia*'\"}}\n```\n如果没结果，再搜索。\n\n",
            "```tool {\"op\":\"execute_command\",\"args\":{\"cmd\":\"grep -r brightness ~/.config/noctalia/\"}}\n```\n如果找不到，说明路径可能不对。\n\n",
            "```tool {\"op\":\"execute_command\",\"args\":{\"cmd\":\"echo should-not-be-reached\"}}\n```\n（第 4 块不应被收集）\n\n",
        ];
        let chunks: Vec<String> = blocks
            .iter()
            .map(|b| json!({"choices":[{"delta":{"reasoning_content": b}}]}).to_string())
            .collect();
        let port = serve_sse(chunks);
        let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k", "m", 10, 1024);
        let cancel = AtomicBool::new(false);
        let mut events = Vec::new();
        let r = llm.stream(
            &ChatRequest { messages: vec![Msg::new("user", "hi")], tools: None, max_tokens: None, stream: true },
            &cancel,
            |ev| events.push(ev),
        );
        let result = r.expect("规划循环应正常返回（不 Err）");
        assert!(result.text.is_empty(), "死循环时正文应为空");
        let n = result.reasoning.chars().count();
        assert!(n < 1000, "应远早于 12k 阈值截断（3 块即触发），实际 {n}");
        assert!(!result.reasoning.contains("echo should-not-be-reached"), "第 4 块不应被收集");
        let reasoning_events = events.iter().filter(|e| matches!(e, StreamEvent::Reasoning(_))).count();
        assert_eq!(reasoning_events, 3, "恰好收集 3 个块后截断，实际 {reasoning_events}");
        assert!(result.reasoning.contains("ls ~/.config/noctalia/"));
        assert!(result.reasoning.contains("grep -r brightness"));
    }
    #[test]
    fn count_tool_blocks_counts_closed_blocks() {
        assert_eq!(count_complete_tool_blocks(""), 0);
        assert_eq!(count_complete_tool_blocks("普通文本没有工具"), 0);
        assert_eq!(
            count_complete_tool_blocks("```tool {\"op\":\"ls\"}\n```"),
            1,
            "单个完整块"
        );
        let three = "先看这里。```tool {\"op\":\"ls\"}\n```\n如果不行：```Tool {\"op\":\"find\"}\n```\n再试：```tool {\"op\":\"grep\"}\n```";
        assert_eq!(count_complete_tool_blocks(three), 3, "3 个完整块（含大小写混用）");
        assert_eq!(
            count_complete_tool_blocks("```tool {\"op\":\"ls\"}"),
            0,
            "未闭合的块不计"
        );
        assert_eq!(
            count_complete_tool_blocks("```tool 这不是 JSON\n```"),
            0,
            "JSON 解析失败的块不计"
        );
        assert_eq!(count_complete_tool_blocks("```python print(1)\n```"), 0, "非 tool 围栏不计");
    }
    #[test]
    fn tail_repeats_detects_loops_only() {
        assert!(!tail_repeats("", 40, 3), "空文本不循环");
        assert!(!tail_repeats("短文本", 40, 3), "长度不足不检测");
        let unit = "好的。\n...\n（结束）\n...\n回复。\n...\n";
        let looped: String = unit.repeat(10);
        assert!(tail_repeats(&looped, 40, 3), "循环单元重复应命中");
        let coherent: String = (0..200)
            .map(|i| format!("详细思考第 {i} 点：假设 A 成立则处理为 B。\n"))
            .collect();
        assert!(!tail_repeats(&coherent, 40, 3), "连贯长文不应命中");
        assert!(tail_repeats(&unit.repeat(8), 40, 3), "8 个 25 字符单元应命中");
        let twice = format!("{unit}{unit}然后正常收尾结束。\n");
        assert!(!tail_repeats(&twice, 40, 3), "仅尾部自身 1 次命中不应触发");
    }
    #[test]
    fn stream_uniform_newlines_reported_and_aborted() {
        let mut chunks: Vec<String> = Vec::new();
        for _ in 0..9 {
            chunks.push(r#"{"choices":[{"delta":{"content":"\n\n\n\n"}}]}"#.to_string());
        }
        let port = serve_sse(chunks);
        let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k", "m", 10, 1024);
        let cancel = AtomicBool::new(false);
        let mut events = Vec::new();
        let r = llm.stream(
            &ChatRequest { messages: vec![Msg::new("user", "hi")], tools: None, max_tokens: None, stream: true },
            &cancel,
            |ev| events.push(ev),
        );
        assert!(r.is_err(), "换行刷屏应中止: {r:?}");
        let garb: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Garbage { kind, sample, run, .. } => Some((*kind, sample.clone(), *run)),
                _ => None,
            })
            .collect();
        assert_eq!(garb.len(), 8, "应上报 8 次");
        assert_eq!(garb[0].0, "正文");
        assert_eq!(garb[0].1, "\n\n\n\n");
        assert_eq!(garb[0].2, 1);
    }
    #[test]
    fn stream_uniform_letters_reported() {
        let mut chunks: Vec<String> = Vec::new();
        for _ in 0..9 {
            chunks.push(r#"{"choices":[{"delta":{"content":"GGGGGGGG"}}]}"#.to_string());
        }
        let port = serve_sse(chunks);
        let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k", "m", 10, 1024);
        let cancel = AtomicBool::new(false);
        let mut events = Vec::new();
        let r = llm.stream(
            &ChatRequest { messages: vec![Msg::new("user", "hi")], tools: None, max_tokens: None, stream: true },
            &cancel,
            |ev| events.push(ev),
        );
        assert!(r.is_err(), "字母重复刷屏应中止: {r:?}");
    }
    #[test]
    fn stream_garbage_total_accumulates_across_interruptions() {
        let mut chunks: Vec<String> = Vec::new();
        for _ in 0..6 {
            chunks.push(r#"{"choices":[{"delta":{"content":"<unused42>"}}]}"#.to_string());
            chunks.push(r#"{"choices":[{"delta":{"content":"正常回复内容"}}]}"#.to_string());
        }
        let port = serve_sse(chunks);
        let llm = Llm::remote(&format!("http://127.0.0.1:{port}/v1"), "k", "m", 10, 1024);
        let cancel = AtomicBool::new(false);
        let mut events = Vec::new();
        let r = llm.stream(
            &ChatRequest { messages: vec![Msg::new("user", "hi")], tools: None, max_tokens: None, stream: true },
            &cancel,
            |ev| events.push(ev),
        );
        assert!(r.is_ok(), "被打断的垃圾不应中止: {r:?}");
        let garb: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Garbage { run, total, .. } => Some((*run, *total)),
                _ => None,
            })
            .collect();
        assert_eq!(garb.len(), 6, "每次垃圾都上报");
        assert_eq!(garb[0], (1, 1), "第一次: 连续 1 累计 1");
        assert_eq!(garb[1], (1, 2), "被打断后: 连续重置为 1，累计到 2");
        assert_eq!(garb[5], (1, 6), "最后一次: 连续 1 累计 6");
    }
}
