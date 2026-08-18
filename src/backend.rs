use serde_json::Value;
use std::time::Duration;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    #[default]
    Other,
    LlamaCpp,
}
#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub kind: BackendKind,
    pub n_ctx: Option<usize>,
    pub default_max_tokens: Option<usize>,
    pub n_ctx_train: Option<usize>,
    pub model_id: Option<String>,
    pub supports_tools: Option<bool>,
    pub modalities: Vec<String>,
    pub router: bool,
    pub auth_required: bool,
}
impl Capabilities {
    pub fn unknown() -> Self {
        Self::default()
    }
    pub fn is_llamacpp(&self) -> bool {
        matches!(self.kind, BackendKind::LlamaCpp)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenBudget {
    TuiMain,
    QqMain,
    QqFinalize,
    TuiFinalize,
    MemorySummary,
}
pub fn derive_max_tokens(caps: &Capabilities, kind: TokenBudget, qq_max: usize) -> Option<usize> {
    match (kind, caps.n_ctx) {
        (TokenBudget::TuiMain, Some(n)) => Some(n),
        (TokenBudget::TuiMain, None) => None,
        (TokenBudget::QqMain, Some(n)) => Some(qq_max.min(n)),
        (TokenBudget::QqMain, None) => Some(qq_max),
        (TokenBudget::QqFinalize, Some(n)) => Some(qq_max.min(n / 64)),
        (TokenBudget::QqFinalize, None) => Some(qq_max),
        (TokenBudget::TuiFinalize, Some(n)) => Some(n / 256),
        (TokenBudget::TuiFinalize, None) => None,
        (TokenBudget::MemorySummary, Some(n)) => Some(n / 128),
        (TokenBudget::MemorySummary, None) => Some(1024),
    }
}
pub fn effective_window(ctx_override: Option<usize>, n_ctx: Option<usize>, fallback: usize) -> usize {
    ctx_override.or(n_ctx).unwrap_or(fallback)
}
fn get_json(url: &str, api_key: &str) -> Option<(u16, Value)> {
    let mut request = ureq::get(url).timeout(Duration::from_secs(2));
    if !api_key.is_empty() {
        request = request.set("Authorization", &format!("Bearer {api_key}"));
    }
    let response = match request.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(status, r)) if status == 401 => r,
        Err(_) => return None,
    };
    let status = response.status();
    let body = response.into_string().ok()?;                         
    let value = serde_json::from_str(&body).ok()?;                                 
    Some((status, value))
}
pub fn discover(base_url: &str, api_key: &str) -> Capabilities {
    let origin = crate::llm::api_origin(base_url);
    let mut caps = Capabilities::unknown();                     
    if let Some((status, props)) = get_json(&format!("{origin}/props"), api_key) {
        if status == 200 {
            caps.kind = BackendKind::LlamaCpp;
            caps.n_ctx = parse_n_ctx(&props);
            caps.default_max_tokens = parse_default_max_tokens(&props);
            caps.supports_tools = props
                .pointer("/chat_template_caps/supports_tools")
                .and_then(Value::as_bool);
            caps.modalities = parse_modalities(props.get("modalities"));
        } else if status == 401 {
            caps.kind = BackendKind::LlamaCpp;
            caps.auth_required = true;
        }
    }
    if let Some((status, models)) = get_json(&format!("{origin}/v1/models"), api_key) {
        if status == 200 {
            if let Some((id, n_ctx, n_ctx_train)) = parse_models(&models) {
                if caps.model_id.is_none() {
                    caps.model_id = Some(id);
                }
                if caps.n_ctx.is_none() {
                    caps.n_ctx = n_ctx;
                }
                if caps.n_ctx_train.is_none() {
                    caps.n_ctx_train = n_ctx_train;
                }
            }
        }
    }
    if caps.is_llamacpp() {
        if let Some((status, models)) = get_json(&format!("{origin}/models"), api_key) {
            if status == 200 {
                caps.router = is_router_models(&models);
            }
        }
    }
    caps
}
pub(crate) fn parse_n_ctx(props: &Value) -> Option<usize> {
    props
        .pointer("/default_generation_settings/n_ctx")
        .and_then(Value::as_u64)
        .map(|v| v as usize)
}
pub(crate) fn parse_default_max_tokens(props: &Value) -> Option<usize> {
    props
        .pointer("/default_generation_settings/params/max_tokens")
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
        .map(|v| v as usize)
}
pub(crate) fn parse_models(models: &Value) -> Option<(String, Option<usize>, Option<usize>)> {
    let first = models.get("data").and_then(|d| d.as_array())?.first()?;
    let id = first.get("id").and_then(Value::as_str)?.to_string();
    let n_ctx = first.pointer("/meta/n_ctx").and_then(Value::as_u64).map(|v| v as usize);
    let n_ctx_train = first.pointer("/meta/n_ctx_train").and_then(Value::as_u64).map(|v| v as usize);
    Some((id, n_ctx, n_ctx_train))
}
pub(crate) fn is_router_models(models: &Value) -> bool {
    models
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .any(|m| m.pointer("/status/value").and_then(Value::as_str).is_some())
        })
        .unwrap_or(false)
}
pub(crate) fn parse_modalities(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Object(map)) => map
            .iter()
            .filter(|(_, val)| val.as_bool().unwrap_or(false))
            .map(|(k, _)| k.clone())
            .collect(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|m| m.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn sample_props() -> Value {
        json!({
            "default_generation_settings": {
                "n_ctx": 131072,
                "params": {
                    "max_tokens": -1,
                    "n_predict": -1,
                    "temperature": 1.0
                }
            },
            "model_alias": "Qwen3.8-27B-Q4_0_ROCMFP4_STRIX.gguf",
            "chat_template_caps": {
                "supports_tools": true,
                "supports_parallel_tool_calls": true
            },
            "modalities": { "vision": true, "audio": false },
            "total_slots": 1
        })
    }
    fn sample_models() -> Value {
        json!({
            "object": "list",
            "data": [{
                "id": "Qwen3.8-27B-Q4_0_ROCMFP4_STRIX.gguf",
                "object": "model",
                "owned_by": "llamacpp",
                "meta": {
                    "n_vocab": 248320,
                    "n_ctx": 131072,
                    "n_ctx_train": 262144,
                    "n_embd": 5120
                }
            }]
        })
    }
    #[test]
    fn props_parsing_full() {
        let props = sample_props();
        assert_eq!(parse_n_ctx(&props), Some(131072));
        assert_eq!(parse_default_max_tokens(&props), None);
        assert_eq!(
            props.pointer("/chat_template_caps/supports_tools").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(parse_modalities(props.get("modalities")), vec!["vision"]);
        assert_eq!(parse_modalities(Some(&json!(["text", "vision"]))), vec!["text", "vision"]);
        assert_eq!(parse_modalities(None), Vec::<String>::new());
    }
    #[test]
    fn props_parsing_positive_max_tokens() {
        let props = json!({
            "default_generation_settings": {
                "n_ctx": 32768,
                "params": { "max_tokens": 16384 }
            }
        });
        assert_eq!(parse_default_max_tokens(&props), Some(16384));
    }
    #[test]
    fn models_parsing_full() {
        let (id, n_ctx, n_ctx_train) = parse_models(&sample_models()).unwrap();
        assert_eq!(id, "Qwen3.8-27B-Q4_0_ROCMFP4_STRIX.gguf");
        assert_eq!(n_ctx, Some(131072));
        assert_eq!(n_ctx_train, Some(262144));
        assert!(parse_models(&json!({"data": []})).is_none());
        assert!(parse_models(&json!({"object": "list"})).is_none());
    }
    #[test]
    fn router_detection_needs_status_value() {
        let router = json!({
            "data": [{
                "id": "m1",
                "status": {"value": "loaded", "args": ["--port", "42559"]}
            }]
        });
        assert!(is_router_models(&router));
        let single = json!({
            "data": [{"id": "m1", "object": "model"}]
        });
        assert!(!is_router_models(&single));
        assert!(!is_router_models(&json!({"data": []})));
    }
    #[test]
    fn derive_rules_llamacpp_n_ctx_131072() {
        let caps = Capabilities { kind: BackendKind::LlamaCpp, n_ctx: Some(131072), ..Default::default() };
        assert_eq!(derive_max_tokens(&caps, TokenBudget::TuiMain, 8192), Some(131072));
        assert_eq!(derive_max_tokens(&caps, TokenBudget::QqMain, 16384), Some(16384));
        assert_eq!(derive_max_tokens(&caps, TokenBudget::QqMain, 131072), Some(131072));
        assert_eq!(derive_max_tokens(&caps, TokenBudget::QqMain, 262144), Some(131072));
        assert_eq!(derive_max_tokens(&caps, TokenBudget::QqFinalize, 8192), Some(2048));
        assert_eq!(derive_max_tokens(&caps, TokenBudget::QqFinalize, 1024), Some(1024));
        assert_eq!(derive_max_tokens(&caps, TokenBudget::TuiFinalize, 8192), Some(512));
        assert_eq!(derive_max_tokens(&caps, TokenBudget::MemorySummary, 8192), Some(1024));
    }
    #[test]
    fn derive_rules_other_backend_fallback() {
        let caps = Capabilities::unknown();
        assert_eq!(derive_max_tokens(&caps, TokenBudget::TuiMain, 8192), None);
        assert_eq!(derive_max_tokens(&caps, TokenBudget::QqMain, 16384), Some(16384));
        assert_eq!(derive_max_tokens(&caps, TokenBudget::QqFinalize, 16384), Some(16384));
        assert_eq!(derive_max_tokens(&caps, TokenBudget::TuiFinalize, 8192), None);
        assert_eq!(derive_max_tokens(&caps, TokenBudget::MemorySummary, 8192), Some(1024));
    }
    #[test]
    fn effective_window_priority() {
        assert_eq!(effective_window(Some(65536), Some(131072), 32768), 65536);
        assert_eq!(effective_window(None, Some(131072), 32768), 131072);
        assert_eq!(effective_window(None, None, 32768), 32768);
        assert_eq!(effective_window(None, Some(4096), 32768), 4096);
    }
    fn serve_discover_responses(responses: Vec<(String, u16, String)>) -> u16 {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for (path, status, body) in responses {
                let (mut socket, _) = listener.accept().unwrap();
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    if status == 200 { "OK" } else { "ERROR" },
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes());
                let _ = path;            
            }
        });
        port
    }
    #[test]
    fn discover_full_llamacpp() {
        let port = serve_discover_responses(vec![
            ("/props".to_string(), 200, sample_props().to_string()),
            ("/v1/models".to_string(), 200, sample_models().to_string()),
            ("/models".to_string(), 200, json!({"data": [{"id": "m", "status": {"value": "loaded"}}]}).to_string()),
        ]);
        let caps = discover(&format!("http://127.0.0.1:{port}/v1"), "");
        assert!(caps.is_llamacpp());
        assert_eq!(caps.n_ctx, Some(131072));
        assert_eq!(caps.default_max_tokens, None);
        assert_eq!(caps.supports_tools, Some(true));
        assert_eq!(caps.modalities, vec!["vision"]);
        assert_eq!(caps.model_id.as_deref(), Some("Qwen3.8-27B-Q4_0_ROCMFP4_STRIX.gguf"));
        assert_eq!(caps.n_ctx_train, Some(262144));
        assert!(caps.router);
    }
    #[test]
    fn discover_auth_required_llamacpp() {
        let port = serve_discover_responses(vec![
            ("/props".to_string(), 401, r#"{"error":{"message":"Invalid API Key"}}"#.to_string()),
            ("/v1/models".to_string(), 200, sample_models().to_string()),
        ]);
        let caps = discover(&format!("http://127.0.0.1:{port}"), "");
        assert!(caps.is_llamacpp());
        assert!(caps.auth_required, "401 应标记服务端要求鉴权");
        assert_eq!(caps.n_ctx, Some(131072));
        assert_eq!(caps.model_id.as_deref(), Some("Qwen3.8-27B-Q4_0_ROCMFP4_STRIX.gguf"));
        assert_eq!(caps.n_ctx_train, Some(262144));
        assert!(!caps.router);
    }
    #[test]
    fn discover_non_llamacpp_other() {
        let port = serve_discover_responses(vec![
            ("/props".to_string(), 404, "not found".to_string()),
            ("/v1/models".to_string(), 200, sample_models().to_string()),
        ]);
        let caps = discover(&format!("http://127.0.0.1:{port}/v1"), "k");
        assert!(!caps.is_llamacpp());
        assert_eq!(caps.n_ctx, Some(131072));
        assert_eq!(caps.model_id.as_deref(), Some("Qwen3.8-27B-Q4_0_ROCMFP4_STRIX.gguf"));
        assert_eq!(caps.n_ctx_train, Some(262144));
    }
    #[test]
    fn discover_unreachable_other() {
        let caps = discover("http://127.0.0.1:1/v1", "");
        assert!(!caps.is_llamacpp());
        assert_eq!(caps.n_ctx, None);
        assert_eq!(caps.model_id, None);
        assert_eq!(caps.modalities, Vec::<String>::new());
    }
}
