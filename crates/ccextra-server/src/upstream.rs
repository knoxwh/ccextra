// 上游请求客户端 (reqwest)
//
// 支持代理:全局 proxy 兜底 + 每 provider 覆盖。
// 按"最终代理"缓存 client,避免每次请求重建。

use std::collections::HashMap;
use std::sync::Mutex;

use ccextra_core::route::Protocol;
use reqwest::Client;

/// 上游请求结果
pub struct UpstreamResponse {
    pub status: reqwest::StatusCode,
    pub body: reqwest::Response,
}

/// 每个请求都要用到的流式头集合,避免在 request 里内联判断
/// (chat / responses 流式声明 SSE;claude 直通不掺头)
///
/// 流式请求显式声明 SSE 并禁止中间缓存复用响应。
fn stream_headers(protocol: Protocol, is_stream: bool) -> Vec<(&'static str, &'static str)> {
    if !is_stream || !matches!(protocol, Protocol::OpenAiChat | Protocol::OpenAiResponses) {
        return Vec::new();
    }
    vec![
        (reqwest::header::ACCEPT.as_str(), "text/event-stream"),
        (reqwest::header::CACHE_CONTROL.as_str(), "no-cache"),
    ]
}

/// 按协议取上游请求路径
///
/// 版本前缀约定(与 参考实现/OpenAI 一致):anthropic 协议 base_url 不含版本,路径带 /v1;
/// openai 协议 base_url 已含版本前缀(/v1 或 /v3 等),路径不带版本。
fn endpoint_path(protocol: Protocol, is_stream: bool) -> String {
    match protocol {
        Protocol::Claude => "/v1/messages".to_string(),
        Protocol::OpenAiChat => "/chat/completions".to_string(),
        Protocol::OpenAiResponses => "/responses".to_string(),
        Protocol::Gemini => {
            if is_stream {
                "/v1beta/models/{model}:streamGenerateContent".to_string()
            } else {
                "/v1beta/models/{model}:generateContent".to_string()
            }
        }
        Protocol::Antigravity => {
            if is_stream {
                // 对齐 CLIProxyAPI:流式必须 ?alt=sse
                "/v1internal:streamGenerateContent?alt=sse".to_string()
            } else {
                "/v1internal:generateContent".to_string()
            }
        }
    }
}

/// Grok CLI 身份头常量(对齐 grok-build xai-grok-sampler client.rs)
const GROK_TOKEN_AUTH: &str = "xai-grok-cli";
const GROK_CLIENT_VERSION: &str = "1.0.5";
const GROK_CLIENT_IDENTIFIER: &str = "grok-shell";

/// Grok CLI User-Agent(对齐 grok-build 默认分支:
/// grok-shell/{version} ({os}; {arch}),platform 运行时取真实值)
static GROK_CLI_UA: std::sync::OnceLock<String> = std::sync::OnceLock::new();
fn grok_cli_ua() -> &'static str {
    GROK_CLI_UA
        .get_or_init(|| {
            format!(
                "grok-shell/{} ({}; {})",
                GROK_CLIENT_VERSION,
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })
        .as_str()
}

/// 模型名是否按 *gpt* 匹配(大小写不敏感,含前缀/中缀/后缀)
fn is_gpt_model(upstream_model: &str) -> bool {
    upstream_model.to_ascii_lowercase().contains("gpt")
}

/// 模型名是否按 *grok* 匹配(大小写不敏感)
fn is_grok_model(upstream_model: &str) -> bool {
    upstream_model.to_ascii_lowercase().contains("grok")
}

/// 按协议+模型取 User-Agent(对齐上游期望的客户端标识)
///
/// 仅 responses + *gpt* 用 Codex UA;responses + *grok* 用 Grok CLI UA
/// (对齐 grokbuild-proxy DefaultUserAgent);其余(含 responses 上的其他模型)
/// 用 claude-cli。部分上游按 UA 分流缓存/特性,reqwest 默认 UA
/// 会被识别为非官方客户端。
fn user_agent(protocol: Protocol, upstream_model: &str) -> &'static str {
    const CLAUDE_CLI: &str = "claude-cli/2.1.234";
    const CODEX_TUI: &str =
        "codex-tui/0.147.0 (Mac OS 26.6.2; arm64) ghostty/1.3.1 (codex-tui; 0.147.0)";
    match protocol {
        // Antigravity 上游按 UA 识别客户端,非 antigravity UA 直接 404
        Protocol::Antigravity => crate::antigravity::constants::REQUEST_UA,
        Protocol::OpenAiResponses if is_gpt_model(upstream_model) => CODEX_TUI,
        Protocol::OpenAiResponses if is_grok_model(upstream_model) => grok_cli_ua(),
        _ => CLAUDE_CLI,
    }
}

#[derive(Clone)]
pub struct UpstreamClient {
    global_proxy: Option<String>,
    /// key(最终代理) → client
    clients: std::sync::Arc<Mutex<HashMap<String, Client>>>,
}

impl UpstreamClient {
    pub fn new(global_proxy: Option<String>) -> Self {
        Self {
            global_proxy,
            clients: std::sync::Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 解析最终代理:provider 覆盖 > 全局 > 直连
    fn resolve_proxy<'a>(&'a self, provider_proxy: Option<&'a str>) -> String {
        match provider_proxy {
            Some(p) if !p.is_empty() && p != "direct" => p.to_string(),
            Some(_) => "direct".to_string(), // "direct"/"" → 直连
            None => self
                .global_proxy
                .clone()
                .unwrap_or_else(|| "direct".to_string()),
        }
    }

    /// 暴露 resolve_proxy 供 crate 内测试断言(如 /reload 后全局代理是否生效)
    #[cfg(test)]
    pub(crate) fn resolve_proxy_for_test(&self, provider_proxy: Option<&str>) -> String {
        self.resolve_proxy(provider_proxy)
    }

    /// 按最终代理取(或构建)client
    fn client_for(&self, proxy_key: &str) -> Client {
        if let Some(c) = self.clients.lock().unwrap().get(proxy_key) {
            return c.clone();
        }
        let mut builder = Client::builder()
            // 连接驻留对齐缓存 TTL(5min),防空闲回收重连导致上游节点切换
            .pool_idle_timeout(std::time::Duration::from_secs(300))
            // TCP 层探活,防死连接滞留/中间设备静默断
            .tcp_keepalive(std::time::Duration::from_secs(60));
        if proxy_key == "direct" {
            builder = builder.no_proxy();
        } else if let Ok(proxy) = reqwest::Proxy::all(proxy_key) {
            builder = builder.proxy(proxy);
        }
        let client = builder.build().unwrap_or_else(|_| Client::new());
        self.clients
            .lock()
            .unwrap()
            .insert(proxy_key.to_string(), client.clone());
        client
    }

    /// 发起上游请求,返回原始响应(字节或流由调用方决定)
    ///
    /// - `is_stream`:chat 链路流式时补 `Accept: text/event-stream` /
    ///   `Cache-Control: no-cache`
    /// - `session_id`:responses 链路发 `session-id` 头(对齐 cacheHelper,
    ///   值为 prompt_cache_key,上游按它做缓存亲和);grok 模型(chat/responses)
    ///   发 `x-grok-conv-id` 会话路由头(xAI 服务器缓存亲和)
    /// - `extra_headers`:claude 直通的透传/重建头(对齐 applyClaudeHeaders
    ///   的中转场景:anthropic-beta 按 body 条件重建 + caller beta 追加,
    ///   anthropic-version / x-app / stainless 系列透传)
    #[allow(clippy::too_many_arguments)]
    pub async fn request(
        &self,
        base_url: &str,
        api_key: &str,
        protocol: Protocol,
        provider_proxy: Option<&str>,
        body: &serde_json::Value,
        is_stream: bool,
        session_id: Option<&str>,
        thread_id: Option<&str>,
        extra_headers: &[(String, String)],
    ) -> anyhow::Result<UpstreamResponse> {
        let proxy_key = self.resolve_proxy(provider_proxy);
        let client = self.client_for(&proxy_key);

        let upstream_model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");

        // Gemini 端点需要替换 {model} 占位符
        let endpoint = endpoint_path(protocol, is_stream);
        let endpoint = if matches!(protocol, Protocol::Gemini) {
            endpoint.replace("{model}", upstream_model)
        } else {
            endpoint
        };

        let url = format!("{}{}", base_url.trim_end_matches('/'), endpoint);
        // 认证头按协议:Gemini 直连用 x-goog-api-key(对齐 CPA gemini_executor,
        // generativelanguage 不收 Bearer);其余 Bearer
        let mut req = client.post(&url).header(
            reqwest::header::USER_AGENT,
            user_agent(protocol, upstream_model),
        );
        if matches!(protocol, Protocol::Gemini) {
            req = req.header("x-goog-api-key", api_key);
        } else {
            req = req.bearer_auth(api_key);
        }
        for (name, value) in stream_headers(protocol, is_stream) {
            req = req.header(name, value);
        }
        // responses 协议:session-id/thread-id 始终带;Originator 仅 *gpt*
        if matches!(protocol, Protocol::OpenAiResponses) {
            if let Some(sid) = session_id {
                req = req.header("session-id", sid);
            }
            if let Some(tid) = thread_id {
                req = req.header("thread-id", tid);
            }
            if is_gpt_model(upstream_model) {
                req = req.header("Originator", "codex_cli_rs");
            }
        }

        // grok 模型会话路由 + CLI 身份头(对齐 grokbuild-proxy headers.go)
        // - x-grok-conv-id:同一会话路由到同一服务器(缓存亲和)
        // - X-XAI-Token-Auth / x-grok-client-version / x-grok-client-identifier /
        //   x-grok-model-override:Grok Build CLI 身份,网关按此识别合法客户端
        // 仅 responses 协议补完整身份头(chat 协议的 c-grok 保持原行为)
        if matches!(protocol, Protocol::OpenAiChat | Protocol::OpenAiResponses)
            && is_grok_model(upstream_model)
        {
            tracing::debug!(
                session_id = ?session_id,
                upstream_model = upstream_model,
                protocol = ?protocol,
                "upstream.rs grok 头注入"
            );
            if let Some(sid) = session_id {
                if !sid.trim().is_empty() {
                    req = req.header("x-grok-conv-id", sid);
                    tracing::debug!(sid = sid, "发送 x-grok-conv-id");
                }
            }
            if matches!(protocol, Protocol::OpenAiResponses) {
                req = req.header("X-XAI-Token-Auth", GROK_TOKEN_AUTH);
                req = req.header("x-grok-client-version", GROK_CLIENT_VERSION);
                req = req.header("x-grok-client-identifier", GROK_CLIENT_IDENTIFIER);
                if !upstream_model.is_empty() {
                    req = req.header("x-grok-model-override", upstream_model);
                }
            }
        }

        for (name, value) in extra_headers {
            req = req.header(name.as_str(), value.as_str());
        }
        let resp = req.json(body).send().await?;

        let status = resp.status();
        Ok(UpstreamResponse { status, body: resp })
    }
}

impl Default for UpstreamClient {
    fn default() -> Self {
        Self::new(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_endpoint_path_routing() {
        assert_eq!(endpoint_path(Protocol::Claude, false), "/v1/messages");
        assert_eq!(
            endpoint_path(Protocol::OpenAiChat, false),
            "/chat/completions"
        );
        assert_eq!(
            endpoint_path(Protocol::OpenAiResponses, false),
            "/responses"
        );
        assert_eq!(
            endpoint_path(Protocol::Gemini, false),
            "/v1beta/models/{model}:generateContent"
        );
        assert_eq!(
            endpoint_path(Protocol::Gemini, true),
            "/v1beta/models/{model}:streamGenerateContent"
        );
    }

    #[test]
    fn test_user_agent_per_protocol() {
        const CLAUDE_CLI: &str = "claude-cli/2.1.234";
        const CODEX_TUI: &str =
            "codex-tui/0.147.0 (Mac OS 26.6.2; arm64) ghostty/1.3.1 (codex-tui; 0.147.0)";
        assert_eq!(
            user_agent(Protocol::OpenAiChat, "gpt-5.6-terra"),
            CLAUDE_CLI
        );
        assert_eq!(
            user_agent(Protocol::OpenAiResponses, "gpt-5.6-terra"),
            CODEX_TUI
        );
        assert_eq!(
            user_agent(Protocol::OpenAiResponses, "GPT-5.6-sol"),
            CODEX_TUI
        );
        assert_eq!(
            user_agent(Protocol::OpenAiResponses, "openai/gpt-5.6"),
            CODEX_TUI
        );
        assert_eq!(
            user_agent(Protocol::OpenAiResponses, "grok-4.6"),
            grok_cli_ua()
        );
        // UA 格式: grok-shell/{version} ({os}; {arch}),platform 运行时取真实值
        let ua = grok_cli_ua();
        assert!(ua.starts_with("grok-shell/1.0.5 ("));
        assert!(ua.contains(std::env::consts::OS));
        assert!(ua.contains(std::env::consts::ARCH));
        assert_eq!(user_agent(Protocol::Claude, "claude-opus-5"), CLAUDE_CLI);
        assert!(is_gpt_model("gpt-5.6-terra"));
        assert!(is_gpt_model("ck-gpt-5.6"));
        assert!(is_gpt_model("openai/GPT-5"));
        assert!(!is_gpt_model("grok-4.6"));
        assert!(!is_gpt_model("claude-opus-5"));
        assert!(is_grok_model("grok-4.6"));
        assert!(is_grok_model("Grok-4.6"));
        assert!(!is_grok_model("gpt-5.6"));
    }

    #[test]
    fn test_stream_headers_chat_and_responses() {
        let chat = stream_headers(Protocol::OpenAiChat, true);
        assert_eq!(
            chat,
            vec![
                (reqwest::header::ACCEPT.as_str(), "text/event-stream"),
                (reqwest::header::CACHE_CONTROL.as_str(), "no-cache"),
            ]
        );
        assert_eq!(stream_headers(Protocol::OpenAiResponses, true), chat);
    }

    #[test]
    fn test_stream_headers_absent_for_non_stream_and_claude() {
        assert!(stream_headers(Protocol::OpenAiChat, false).is_empty());
        assert!(stream_headers(Protocol::OpenAiResponses, false).is_empty());
        // claude 直通字节原样转发,不掺头
        assert!(stream_headers(Protocol::Claude, true).is_empty());
    }

    #[test]
    fn test_url_join_no_double_version() {
        // openai 协议:base_url 已含版本前缀,路径不再重复 /v1
        let base = "https://dashscope.aliyuncs.com/compatible-mode/v1";
        let url = format!(
            "{}{}",
            base.trim_end_matches('/'),
            endpoint_path(Protocol::OpenAiChat, false)
        );
        assert_eq!(
            url,
            "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions"
        );

        // 自定义版本前缀(如 /v3):同样不重复
        let base = "https://ark.cn-beijing.volces.com/api/v3";
        let url = format!(
            "{}{}",
            base.trim_end_matches('/'),
            endpoint_path(Protocol::OpenAiChat, false)
        );
        assert_eq!(
            url,
            "https://ark.cn-beijing.volces.com/api/v3/chat/completions"
        );

        // claude 协议:base_url 不含版本,路径带 /v1
        let base = "https://example.com/claude-proxy";
        let url = format!(
            "{}{}",
            base.trim_end_matches('/'),
            endpoint_path(Protocol::Claude, false)
        );
        assert_eq!(url, "https://example.com/claude-proxy/v1/messages");
    }

    #[test]
    fn test_proxy_priority_provider_overrides_global() {
        let client = UpstreamClient::new(Some("http://global-proxy:8080".into()));
        let resolved = client.resolve_proxy(Some("http://provider-proxy:9090"));
        assert_eq!(resolved, "http://provider-proxy:9090");
    }

    #[test]
    fn test_proxy_priority_direct_overrides_global() {
        let client = UpstreamClient::new(Some("http://global-proxy:8080".into()));
        let resolved = client.resolve_proxy(Some("direct"));
        assert_eq!(resolved, "direct");
    }

    #[test]
    fn test_proxy_priority_empty_string_is_direct() {
        let client = UpstreamClient::new(Some("http://global-proxy:8080".into()));
        let resolved = client.resolve_proxy(Some(""));
        assert_eq!(resolved, "direct");
    }

    #[test]
    fn test_proxy_priority_none_uses_global() {
        let client = UpstreamClient::new(Some("http://global-proxy:8080".into()));
        let resolved = client.resolve_proxy(None);
        assert_eq!(resolved, "http://global-proxy:8080");
    }

    #[test]
    fn test_proxy_priority_none_and_no_global_is_direct() {
        let client = UpstreamClient::new(None);
        let resolved = client.resolve_proxy(None);
        assert_eq!(resolved, "direct");
    }

    #[test]
    fn test_client_caching() {
        let client = UpstreamClient::new(None);
        let _c1 = client.client_for("direct");
        let _c2 = client.client_for("direct");
        // 同一 proxy_key 应返回相同 client(Arc clone)
        // 通过计数验证缓存命中
        let count_before = client.clients.lock().unwrap().len();
        let _c3 = client.client_for("direct");
        let count_after = client.clients.lock().unwrap().len();
        assert_eq!(count_before, count_after, "缓存应命中,不应重建 client");
    }

    #[test]
    fn test_client_different_proxies() {
        let client = UpstreamClient::new(None);
        let _c1 = client.client_for("direct");
        let _c2 = client.client_for("http://proxy1:8080");
        let _c3 = client.client_for("http://proxy2:9090");
        assert_eq!(client.clients.lock().unwrap().len(), 3);
    }
}
