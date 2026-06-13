//! Provider Adapter Layer: a unified interface mapping the kernel's strict
//! payload contract onto each platform's native API. Adapters are
//! deliberately dumb translators — all intelligence lives in the
//! orchestration layer.
//!
//! Security note: the Gemini adapter sends the API key
//! in the `X-Goog-Api-Key` request header — never in the URL query string —
//! so the secret cannot leak into access logs, proxy logs, or referrers.

use crate::config;
use crate::tokenizer;
use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

/// Kernel→adapter execution contract.
#[derive(Debug, Clone)]
pub struct Request {
    /// Kernel route (DIRECT, PATCH, ...).
    pub route: String,
    /// Fully serialized static→dynamic payload.
    pub prompt: String,
    /// Resolved model ID (already filter-approved).
    pub model: String,
    /// Output token cap (0 = provider default).
    pub max_out: i64,
    pub timeout: Duration,
}

/// Adapter→kernel result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub text: String,
    /// Provider-reported when available, else 0.
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub model: String,
}

/// Deterministic error classes so the scheduler can react.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider rate limited (429)")]
    RateLimited { retry_after: Option<Duration> },
    #[error("provider authentication failed")]
    Auth,
    #[error("provider unavailable")]
    Unavailable(#[source] Option<anyhow::Error>),
    #[error("provider returned HTTP {0}")]
    Http(u16),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

fn classify_http(status: u16) -> ProviderError {
    match status {
        429 => ProviderError::RateLimited { retry_after: None },
        401 | 403 => ProviderError::Auth,
        s if s >= 500 => ProviderError::Unavailable(None),
        s => ProviderError::Http(s),
    }
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?;
    let val_str = value.to_str().ok()?;
    if let Ok(secs) = val_str.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    if let Ok(date) = chrono::DateTime::parse_from_rfc2822(val_str) {
        let now = chrono::Utc::now();
        let diff = date.with_timezone(&chrono::Utc) - now;
        if diff.num_seconds() > 0 {
            return Some(Duration::from_secs(diff.num_seconds() as u64));
        }
    }
    None
}

/// Pooled HTTP client: keep-alives + HTTP/2 multiplexing keep upstream
/// connections warm across consecutive kernel turns.
static SHARED_CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .tcp_keepalive(Duration::from_secs(60))
        .pool_max_idle_per_host(16)
        .pool_idle_timeout(Duration::from_secs(120))
        .build()
        .expect("build http client")
});

/// Unified provider adapter.
pub enum Adapter {
    Mock(Mock),
    OpenAi(HttpAdapter),
    Anthropic(HttpAdapter),
    Gemini(HttpAdapter),
}

/// Shared fields for HTTP-backed adapters.
pub struct HttpAdapter {
    pub name: String,
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
}

impl Adapter {
    /// Construct an adapter from a provider profile.
    pub fn new(name: &str, p: &config::Provider) -> Result<Self> {
        let api_key = if p.api_key_env.is_empty() {
            String::new()
        } else {
            std::env::var(&p.api_key_env).unwrap_or_default()
        };
        if matches!(p.adapter.as_str(), "openai" | "anthropic" | "gemini")
            && api_key.trim().is_empty()
        {
            return Err(anyhow!(
                "provider {:?}: adapter {:?} requires non-empty environment variable {:?}",
                name,
                p.adapter,
                p.api_key_env
            ));
        }
        let mk = |endpoint_default: &str| HttpAdapter {
            name: name.to_string(),
            endpoint: if p.endpoint.is_empty() {
                endpoint_default.to_string()
            } else {
                p.endpoint.clone()
            },
            api_key,
            model: p.model.clone(),
        };
        match p.adapter.as_str() {
            "mock" => Ok(Adapter::Mock(Mock::new(name))),
            "openai" => Ok(Adapter::OpenAi(mk("https://api.openai.com/v1"))),
            "anthropic" => Ok(Adapter::Anthropic(mk("https://api.anthropic.com/v1"))),
            "gemini" => Ok(Adapter::Gemini(mk(
                "https://generativelanguage.googleapis.com/v1beta",
            ))),
            // OpenAI-compatible local bridge (Cursor/Windsurf/ollama/llama.cpp...).
            "proxy" | "proxy_ide" => {
                if p.endpoint.is_empty() {
                    return Err(anyhow!(
                        "provider {:?}: proxy adapter requires endpoint",
                        name
                    ));
                }
                Ok(Adapter::OpenAi(mk("")))
            }
            other => Err(anyhow!("provider {:?}: unknown adapter {:?}", name, other)),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Adapter::Mock(m) => &m.name,
            Adapter::OpenAi(h) | Adapter::Anthropic(h) | Adapter::Gemini(h) => &h.name,
        }
    }

    /// Model IDs the adapter exposes (pre-filter; static manifest).
    pub fn models(&self) -> Vec<String> {
        match self {
            Adapter::Mock(_) => vec!["mock-1".into(), "mock-large".into()],
            Adapter::OpenAi(h) => manifest(&h.model, "gpt-4o-mini"),
            Adapter::Anthropic(h) => manifest(&h.model, "claude-sonnet-4-20250514"),
            Adapter::Gemini(h) => manifest(&h.model, "gemini-2.0-flash"),
        }
    }

    pub async fn execute(&self, req: &Request) -> Result<Response, ProviderError> {
        match self {
            Adapter::Mock(m) => m.execute(req).await,
            Adapter::OpenAi(h) => execute_openai(h, req).await,
            Adapter::Anthropic(h) => execute_anthropic(h, req).await,
            Adapter::Gemini(h) => execute_gemini(h, req).await,
        }
    }
}

fn manifest(configured: &str, default: &str) -> Vec<String> {
    if configured.is_empty() {
        vec![default.to_string()]
    } else {
        vec![configured.to_string()]
    }
}

// ---------------------------------------------------------------------------
// Mock adapter
// ---------------------------------------------------------------------------

/// Offline short-circuit adapter: deterministic responses for smoke-testing
/// routing, failover, quota tracking and telemetry without burning a single
/// live token. Supports scripted fault injection.
pub struct Mock {
    pub name: String,
    calls: AtomicI64,
    /// Every Nth call returns RateLimited (0 = never).
    pub fail_every_n: i64,
    /// Artificial latency.
    pub latency: Duration,
    /// Fixed response body (otherwise synthesized).
    pub canned: String,
}

impl Mock {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            calls: AtomicI64::new(0),
            fail_every_n: 0,
            latency: Duration::ZERO,
            canned: String::new(),
        }
    }

    async fn execute(&self, req: &Request) -> Result<Response, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if !self.latency.is_zero() {
            tokio::time::sleep(self.latency).await;
        }
        if self.fail_every_n > 0 && n % self.fail_every_n == 0 {
            return Err(ProviderError::RateLimited { retry_after: None });
        }
        let body = if !self.canned.is_empty() {
            self.canned.clone()
        } else {
            let goal = extract_line(&req.prompt, "GOAL: ");
            match req.route.as_str() {
                "ASK" => format!(
                    "What is the single most critical unspecified detail required to complete: {:?}?",
                    goal
                ),
                "PATCH" => format!(
                    "--- a/target\n+++ b/target\n@@ -1,1 +1,1 @@\n-// before\n+// after (mock patch for: {})",
                    goal
                ),
                "VERIFY" => "VERIFICATION: PASS (mock static checks + targeted tests)".to_string(),
                _ => format!("[mock:{}] completed route {} for goal: {}", self.name, req.route, goal),
            }
        };
        Ok(Response {
            tokens_in: tokenizer::estimate(&req.prompt) as i64,
            tokens_out: tokenizer::estimate(&body) as i64,
            text: body,
            model: "mock-1".to_string(),
        })
    }
}

fn extract_line(s: &str, prefix: &str) -> String {
    for line in s.split('\n') {
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest.trim().to_string();
        }
    }
    "(unspecified)".to_string()
}

// ---------------------------------------------------------------------------
// OpenAI (/chat/completions; also serves any OpenAI-compatible endpoint)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct OaMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct OaResponse {
    #[serde(default)]
    choices: Vec<OaChoice>,
    #[serde(default)]
    usage: OaUsage,
    #[serde(default)]
    model: String,
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct OaChoice {
    message: OaMsgContent,
}

#[derive(Deserialize)]
struct OaMsgContent {
    #[serde(default)]
    content: String,
}

#[derive(Deserialize, Default)]
struct OaUsage {
    #[serde(default)]
    prompt_tokens: i64,
    #[serde(default)]
    completion_tokens: i64,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    message: String,
}

async fn read_response_limit(mut resp: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| anyhow!("read error: {}", e))?
    {
        if body.len() + chunk.len() > max_bytes {
            return Err(anyhow!(
                "response body size exceeded limit of {} bytes",
                max_bytes
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Serialize)]
struct OaRequest<'a> {
    model: &'a str,
    messages: Vec<OaMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<i64>,
}

async fn execute_openai(h: &HttpAdapter, req: &Request) -> Result<Response, ProviderError> {
    let model = if req.model.is_empty() {
        h.model.clone()
    } else {
        req.model.clone()
    };
    let body = OaRequest {
        model: &model,
        messages: vec![OaMessage {
            role: "user",
            content: &req.prompt,
        }],
        max_tokens: if req.max_out > 0 {
            Some(req.max_out)
        } else {
            None
        },
    };
    let mut rb = SHARED_CLIENT
        .post(format!("{}/chat/completions", h.endpoint))
        .timeout(req.timeout)
        .json(&body);
    if !h.api_key.is_empty() {
        rb = rb.bearer_auth(&h.api_key);
    }
    let resp = rb
        .send()
        .await
        .map_err(|e| ProviderError::Unavailable(Some(e.into())))?;
    let status = resp.status().as_u16();
    if status != 200 {
        if status == 429 {
            let retry_after = parse_retry_after(resp.headers());
            return Err(ProviderError::RateLimited { retry_after });
        }
        return Err(classify_http(status));
    }
    let body_bytes = read_response_limit(resp, 10 * 1024 * 1024)
        .await
        .map_err(|e| ProviderError::Other(anyhow!("read response: {}", e)))?;
    let out: OaResponse = serde_json::from_slice(&body_bytes)
        .map_err(|e| ProviderError::Other(anyhow!("decode response: {}", e)))?;
    if let Some(e) = out.error {
        return Err(ProviderError::Other(anyhow!("api error: {}", e.message)));
    }
    let first = out
        .choices
        .first()
        .ok_or_else(|| ProviderError::Other(anyhow!("empty response")))?;
    Ok(Response {
        text: first.message.content.clone(),
        tokens_in: out.usage.prompt_tokens,
        tokens_out: out.usage.completion_tokens,
        model: out.model,
    })
}

// ---------------------------------------------------------------------------
// Anthropic (Messages API)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AnResponse {
    #[serde(default)]
    content: Vec<AnContent>,
    #[serde(default)]
    usage: AnUsage,
    #[serde(default)]
    model: String,
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct AnContent {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize, Default)]
struct AnUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
}

#[derive(Serialize)]
struct AnRequest<'a> {
    model: &'a str,
    max_tokens: i64,
    messages: Vec<OaMessage<'a>>,
}

async fn execute_anthropic(h: &HttpAdapter, req: &Request) -> Result<Response, ProviderError> {
    let model = if req.model.is_empty() {
        h.model.clone()
    } else {
        req.model.clone()
    };
    let max_out = if req.max_out > 0 { req.max_out } else { 4096 };
    let body = AnRequest {
        model: &model,
        max_tokens: max_out,
        messages: vec![OaMessage {
            role: "user",
            content: &req.prompt,
        }],
    };
    let resp = SHARED_CLIENT
        .post(format!("{}/messages", h.endpoint))
        .timeout(req.timeout)
        .header("x-api-key", &h.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .map_err(|e| ProviderError::Unavailable(Some(e.into())))?;
    let status = resp.status().as_u16();
    if status != 200 {
        if status == 429 {
            let retry_after = parse_retry_after(resp.headers());
            return Err(ProviderError::RateLimited { retry_after });
        }
        return Err(classify_http(status));
    }
    let body_bytes = read_response_limit(resp, 10 * 1024 * 1024)
        .await
        .map_err(|e| ProviderError::Other(anyhow!("read response: {}", e)))?;
    let out: AnResponse = serde_json::from_slice(&body_bytes)
        .map_err(|e| ProviderError::Other(anyhow!("decode response: {}", e)))?;
    if let Some(e) = out.error {
        return Err(ProviderError::Other(anyhow!("api error: {}", e.message)));
    }
    let text: String = out
        .content
        .iter()
        .filter(|c| c.kind == "text")
        .map(|c| c.text.as_str())
        .collect();
    if text.is_empty() {
        return Err(ProviderError::Other(anyhow!("empty response")));
    }
    Ok(Response {
        text,
        tokens_in: out.usage.input_tokens,
        tokens_out: out.usage.output_tokens,
        model: out.model,
    })
}

// ---------------------------------------------------------------------------
// Gemini (generateContent)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GmResponse {
    #[serde(default)]
    candidates: Vec<GmCandidate>,
    #[serde(rename = "usageMetadata", default)]
    usage_metadata: GmUsage,
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct GmCandidate {
    #[serde(default)]
    content: GmContent,
}

#[derive(Deserialize, Default)]
struct GmContent {
    #[serde(default)]
    parts: Vec<GmPart>,
}

#[derive(Deserialize)]
struct GmPart {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize, Default)]
struct GmUsage {
    #[serde(rename = "promptTokenCount", default)]
    prompt_token_count: i64,
    #[serde(rename = "candidatesTokenCount", default)]
    candidates_token_count: i64,
}

#[derive(Serialize)]
struct GmRequest<'a> {
    contents: Vec<GmRequestContent<'a>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "generationConfig")]
    generation_config: Option<GmGenerationConfig>,
}

#[derive(Serialize)]
struct GmRequestContent<'a> {
    role: &'a str,
    parts: Vec<GmRequestPart<'a>>,
}

#[derive(Serialize)]
struct GmRequestPart<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct GmGenerationConfig {
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: i64,
}

async fn execute_gemini(h: &HttpAdapter, req: &Request) -> Result<Response, ProviderError> {
    let model = if req.model.is_empty() {
        h.model.clone()
    } else {
        req.model.clone()
    };
    let body = GmRequest {
        contents: vec![GmRequestContent {
            role: "user",
            parts: vec![GmRequestPart { text: &req.prompt }],
        }],
        generation_config: if req.max_out > 0 {
            Some(GmGenerationConfig {
                max_output_tokens: req.max_out,
            })
        } else {
            None
        },
    };
    // Security: API key travels in the X-Goog-Api-Key header,
    // NOT the URL query string. URLs are routinely captured by access logs,
    // proxies and tracing systems; headers are not.
    let resp = SHARED_CLIENT
        .post(format!("{}/models/{}:generateContent", h.endpoint, model))
        .timeout(req.timeout)
        .header("X-Goog-Api-Key", &h.api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| ProviderError::Unavailable(Some(e.into())))?;
    let status = resp.status().as_u16();
    if status != 200 {
        if status == 429 {
            let retry_after = parse_retry_after(resp.headers());
            return Err(ProviderError::RateLimited { retry_after });
        }
        return Err(classify_http(status));
    }
    let body_bytes = read_response_limit(resp, 10 * 1024 * 1024)
        .await
        .map_err(|e| ProviderError::Other(anyhow!("read response: {}", e)))?;
    let out: GmResponse = serde_json::from_slice(&body_bytes)
        .map_err(|e| ProviderError::Other(anyhow!("decode response: {}", e)))?;
    if let Some(e) = out.error {
        return Err(ProviderError::Other(anyhow!("api error: {}", e.message)));
    }
    let first = out
        .candidates
        .first()
        .ok_or_else(|| ProviderError::Other(anyhow!("empty response")))?;
    if first.content.parts.is_empty() {
        return Err(ProviderError::Other(anyhow!("empty response")));
    }
    let text: String = first
        .content
        .parts
        .iter()
        .map(|p| p.text.as_str())
        .collect();
    Ok(Response {
        text,
        tokens_in: out.usage_metadata.prompt_token_count,
        tokens_out: out.usage_metadata.candidates_token_count,
        model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(route: &str, prompt: &str) -> Request {
        Request {
            route: route.to_string(),
            prompt: prompt.to_string(),
            model: String::new(),
            max_out: 0,
            timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn mock_synthesizes_route_specific_output() {
        let m = Mock::new("mock");
        let r = m
            .execute(&req("ASK", "GOAL: build the thing\nother"))
            .await
            .unwrap();
        assert!(r.text.contains("build the thing"));
        assert!(r.text.contains('?'));

        let r = m.execute(&req("PATCH", "GOAL: fix bug")).await.unwrap();
        assert!(r.text.starts_with("--- a/"));

        let r = m
            .execute(&req("IMPLEMENT", "GOAL: write feature"))
            .await
            .unwrap();
        assert!(r.text.contains("write feature"));
    }

    #[tokio::test]
    async fn mock_fault_injection() {
        let mut m = Mock::new("flaky");
        m.fail_every_n = 2;
        assert!(m.execute(&req("DIRECT", "GOAL: a")).await.is_ok()); // call 1
        assert!(matches!(
            m.execute(&req("DIRECT", "GOAL: b")).await,
            Err(ProviderError::RateLimited { .. })
        )); // call 2
        assert!(m.execute(&req("DIRECT", "GOAL: c")).await.is_ok()); // call 3
    }

    #[test]
    fn classify_http_codes() {
        assert!(matches!(
            classify_http(429),
            ProviderError::RateLimited { .. }
        ));
        assert!(matches!(classify_http(401), ProviderError::Auth));
        assert!(matches!(classify_http(403), ProviderError::Auth));
        assert!(matches!(classify_http(500), ProviderError::Unavailable(_)));
        assert!(matches!(classify_http(404), ProviderError::Http(404)));
    }

    #[test]
    fn test_parse_retry_after() {
        use reqwest::header::{HeaderMap, RETRY_AFTER};

        // Test seconds format
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, "120".parse().unwrap());
        let dur = parse_retry_after(&headers).unwrap();
        assert_eq!(dur, Duration::from_secs(120));

        // Test HTTP date format
        let mut headers = HeaderMap::new();
        let future_time = chrono::Utc::now() + chrono::Duration::seconds(60);
        let future_time_str = future_time.to_rfc2822();
        headers.insert(RETRY_AFTER, future_time_str.parse().unwrap());
        let dur = parse_retry_after(&headers).unwrap();
        assert!(dur.as_secs() >= 55 && dur.as_secs() <= 65);
    }

    #[test]
    fn adapter_factory() {
        let p = config::Provider {
            adapter: "mock".into(),
            ..Default::default()
        };
        let a = Adapter::new("m", &p).unwrap();
        assert_eq!(a.name(), "m");
        assert_eq!(
            a.models(),
            vec!["mock-1".to_string(), "mock-large".to_string()]
        );

        let p = config::Provider {
            adapter: "proxy".into(),
            ..Default::default()
        };
        assert!(Adapter::new("p", &p).is_err()); // proxy requires endpoint

        let p = config::Provider {
            adapter: "openai".into(),
            api_key_env: String::new(),
            ..Default::default()
        };
        assert!(Adapter::new("openai", &p).is_err()); // live adapters require credentials

        let p = config::Provider {
            adapter: "bogus".into(),
            ..Default::default()
        };
        assert!(Adapter::new("b", &p).is_err());
    }

    #[test]
    fn extract_line_finds_goal() {
        assert_eq!(extract_line("X\nGOAL: do it \nY", "GOAL: "), "do it");
        assert_eq!(extract_line("no goal here", "GOAL: "), "(unspecified)");
    }
}
