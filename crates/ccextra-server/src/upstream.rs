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

/// Grok CLI 身份头常量
/// Token-Auth 对齐 grok-build GrokAuthCredentials (`xai-grok-cli`);
/// version/identifier 对齐 grok-shell,不在 sampler GrokRequestHeaders 里
const GROK_TOKEN_AUTH: &str = "xai-grok-cli";
const GROK_CLIENT_IDENTIFIER: &str = "grok-shell";

/// 模型名是否为 GPT/Codex 模型(对齐 ccextra_core::convert::to_openai_responses::is_gpt_upstream)
pub(crate) fn is_gpt_model(upstream_model: &str) -> bool {
    ccextra_core::convert::to_openai_responses::is_gpt_upstream(upstream_model)
}

/// 模型名是否按 *grok* 匹配(大小写不敏感)
pub(crate) fn is_grok_model(upstream_model: &str) -> bool {
    upstream_model.to_ascii_lowercase().contains("grok")
}

/// grok chat/responses 出站头(手术对齐 grok-build)
/// Token-Auth/version/identifier 来自 GrokAuthCredentials + grok-shell;
/// conv-id/model-override 来自 GrokRequestHeaders(不发 req-id/session-id/agent-id/turn-idx)
/// 非 grok 或非 chat/responses 返回空,conv-id 仅 session trim 非空才带,doom-loop 仅 responses
fn grok_cli_headers(
    protocol: Protocol,
    upstream_model: &str,
    session_id: Option<&str>,
    grok_version: &str,
) -> Vec<(&'static str, String)> {
    if !matches!(protocol, Protocol::OpenAiChat | Protocol::OpenAiResponses)
        || !is_grok_model(upstream_model)
    {
        return Vec::new();
    }
    let mut headers = vec![
        ("X-XAI-Token-Auth", GROK_TOKEN_AUTH.to_string()),
        ("x-grok-client-version", grok_version.to_string()),
        (
            "x-grok-client-identifier",
            GROK_CLIENT_IDENTIFIER.to_string(),
        ),
        ("x-grok-model-override", upstream_model.to_string()),
    ];
    if let Some(sid) = session_id {
        if !sid.trim().is_empty() {
            // 原样发出;trim 只判断空,不改值
            headers.push(("x-grok-conv-id", sid.to_string()));
        }
    }
    if matches!(protocol, Protocol::OpenAiResponses) {
        headers.push(("x-grok-doom-loop-check", "1024".to_string()));
        headers.push(("x-grok-exact-repetition-check", "64".to_string()));
    }
    headers
}

/// 按协议+模型取 User-Agent(对齐上游期望的客户端标识)
///
/// 仅 responses + *gpt* 用 Codex UA;chat 或 responses + *grok* 用 Grok CLI UA
/// (对齐 grok-shell `{name}/{ver} ({os}; {arch})`);其余用 claude-cli
/// 部分上游按 UA 分流缓存/特性,reqwest 默认 UA 会被识别为非官方客户端。
fn user_agent(
    protocol: Protocol,
    upstream_model: &str,
    user_agents: &crate::http::UserAgentSet,
    inbound_user_agent: Option<&str>,
) -> String {
    if matches!(protocol, Protocol::Claude) {
        if let Some(value) = inbound_user_agent.filter(|value| !value.is_empty()) {
            return value.to_string();
        }
        return user_agents.claude_cli.to_string();
    }

    match protocol {
        // Antigravity 上游按 UA 识别客户端,非 antigravity UA 直接 404
        Protocol::Antigravity => user_agents.antigravity.to_string(),
        Protocol::OpenAiResponses if is_gpt_model(upstream_model) => {
            user_agents.codex_tui.to_string()
        }
        Protocol::OpenAiChat | Protocol::OpenAiResponses if is_grok_model(upstream_model) => {
            format!(
                "grok-shell/{} ({}; {})",
                user_agents.grok_version,
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        }
        _ => user_agents.claude_cli.to_string(),
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
    pub(crate) fn resolve_proxy<'a>(&'a self, provider_proxy: Option<&'a str>) -> String {
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
    pub(crate) fn client_for(&self, proxy_key: &str) -> Client {
        if let Some(c) = self.clients.lock().unwrap().get(proxy_key) {
            return c.clone();
        }
        let mut builder = Client::builder()
            // 限制单个 host 最大空闲连接数，防毒化池
            .pool_max_idle_per_host(4)
            // 连接驻留 90s 空闲淘汰(对齐 grok/codex 默认池策略)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            // 建连超时 10s，防 DNS/TLS 握手卡死
            .connect_timeout(std::time::Duration::from_secs(10))
            // TCP 层探活,防死连接滞留/中间设备静默断
            .tcp_keepalive(std::time::Duration::from_secs(60))
            // 禁 Nagle:SSE 首帧/心跳/delta 都是小包,不等积压直接发,压 TTFT
            .tcp_nodelay(true)
            // HTTP/2 探活与空闲 Ping，防静默掉线(对齐 grok shared_http)
            .http2_keep_alive_interval(std::time::Duration::from_secs(15))
            .http2_keep_alive_timeout(std::time::Duration::from_secs(5))
            .http2_keep_alive_while_idle(true);
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
    /// - `extra_headers`:Claude 入站头;已排除入站认证、User-Agent、传输与连接管理头
    /// - `inbound_user_agent`:Claude 协议优先使用入站值,缺失时回退配置值
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
        extra_headers: &axum::http::HeaderMap,
        user_agents: &crate::http::UserAgentSet,
        inbound_user_agent: Option<&str>,
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
            user_agent(protocol, upstream_model, user_agents, inbound_user_agent),
        );
        if matches!(protocol, Protocol::Gemini) {
            req = req.header("x-goog-api-key", api_key);
        } else {
            req = req.bearer_auth(api_key);
        }
        for (name, value) in stream_headers(protocol, is_stream) {
            req = req.header(name, value);
        }
        // responses 协议:Session-Id/Thread-Id 始终带;Originator 仅 *gpt*
        if matches!(protocol, Protocol::OpenAiResponses) {
            if let Some(sid) = session_id {
                req = req.header("Session-Id", sid);
                if is_gpt_model(upstream_model) {
                    req = req.header("X-Codex-Window-Id", format!("{sid}:0"));
                }
            }
            if let Some(tid) = thread_id {
                req = req.header("Thread-Id", tid);
            }
            if is_gpt_model(upstream_model) {
                req = req.header("Originator", "codex_cli_rs");
            }
        }

        // grok 模型:CLI 身份头 + conv-id +(仅 responses) doom-loop
        let grok_headers = grok_cli_headers(
            protocol,
            upstream_model,
            session_id,
            &user_agents.grok_version,
        );
        if !grok_headers.is_empty() {
            tracing::debug!(
                session_id = ?session_id,
                upstream_model = upstream_model,
                protocol = ?protocol,
                "upstream.rs grok 头注入"
            );
        }
        for (name, value) in grok_headers {
            req = req.header(name, value);
        }

        for (name, value) in extra_headers {
            req = req.header(name, value);
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
    fn test_claude_user_agent_prefers_inbound_value() {
        use std::sync::Arc;
        let uas = crate::http::UserAgentSet {
            claude_cli: Arc::new("configured-agent".to_string()),
            codex_tui: Arc::new("codex".to_string()),
            grok_version: Arc::new("1.0.5".to_string()),
            antigravity: Arc::new("antigravity".to_string()),
        };
        assert_eq!(
            user_agent(
                Protocol::Claude,
                "claude-opus-5",
                &uas,
                Some("inbound-agent")
            ),
            "inbound-agent"
        );
        assert_eq!(
            user_agent(Protocol::Claude, "claude-opus-5", &uas, Some("")),
            "configured-agent"
        );
        assert_eq!(
            user_agent(Protocol::Claude, "claude-opus-5", &uas, None),
            "configured-agent"
        );
    }

    #[test]
    fn test_user_agent_per_protocol() {
        use std::sync::Arc;
        let uas = crate::http::UserAgentSet {
            claude_cli: Arc::new("claude-cli/2.1.246".to_string()),
            codex_tui: Arc::new(
                "codex-tui/0.149.1 (Mac OS 26.6.2; arm64) ghostty/1.3.1 (codex-tui; 0.149.1)"
                    .to_string(),
            ),
            grok_version: Arc::new("1.0.5".to_string()),
            antigravity: Arc::new("antigravity/hub/2.10.0 darwin/arm64".to_string()),
        };
        const CLAUDE_CLI: &str = "claude-cli/2.1.246";
        const CODEX_TUI: &str =
            "codex-tui/0.149.1 (Mac OS 26.6.2; arm64) ghostty/1.3.1 (codex-tui; 0.149.1)";
        assert_eq!(
            user_agent(Protocol::OpenAiChat, "gpt-5.6-terra", &uas, None),
            CLAUDE_CLI
        );
        assert_eq!(
            user_agent(Protocol::OpenAiResponses, "gpt-5.6-terra", &uas, None),
            CODEX_TUI
        );
        assert_eq!(
            user_agent(Protocol::OpenAiResponses, "GPT-5.6-sol", &uas, None),
            CODEX_TUI
        );
        assert_eq!(
            user_agent(Protocol::OpenAiResponses, "openai/gpt-5.6", &uas, None),
            CODEX_TUI
        );
        let grok_ua = user_agent(Protocol::OpenAiResponses, "grok-4.6", &uas, None);
        assert!(grok_ua.starts_with("grok-shell/1.0.5 ("));
        assert_eq!(
            user_agent(Protocol::OpenAiChat, "grok-4.6", &uas, None),
            grok_ua
        );
        assert_eq!(
            user_agent(Protocol::OpenAiChat, "Grok-4.6", &uas, None),
            grok_ua
        );
        assert_eq!(
            user_agent(Protocol::OpenAiChat, "GPT-4", &uas, None),
            CLAUDE_CLI
        );
        assert!(grok_ua.contains(std::env::consts::OS));
        assert!(grok_ua.contains(std::env::consts::ARCH));
        assert_eq!(
            user_agent(Protocol::Claude, "claude-opus-5", &uas, None),
            CLAUDE_CLI
        );
        assert!(is_gpt_model("gpt-5.6-terra"));
        assert!(is_gpt_model("ck-gpt-5.6"));
        assert!(is_gpt_model("openai/GPT-5"));
        assert!(is_gpt_model("codex-mini"));
        assert!(is_gpt_model("o3-mini"));
        assert!(is_gpt_model("o1-preview"));
        assert!(!is_gpt_model("grok-4.6"));
        assert!(!is_gpt_model("claude-opus-5"));
        assert!(is_grok_model("grok-4.6"));
        assert!(is_grok_model("Grok-4.6"));
        assert!(!is_grok_model("gpt-5.6"));
    }

    fn grok_header_map(
        protocol: Protocol,
        model: &str,
        session: Option<&str>,
    ) -> std::collections::HashMap<String, String> {
        grok_cli_headers(protocol, model, session, "1.0.5")
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    fn assert_identity(h: &std::collections::HashMap<String, String>, model: &str) {
        assert_eq!(
            h.get("X-XAI-Token-Auth").map(String::as_str),
            Some("xai-grok-cli")
        );
        assert_eq!(
            h.get("x-grok-client-version").map(String::as_str),
            Some("1.0.5")
        );
        assert_eq!(
            h.get("x-grok-client-identifier").map(String::as_str),
            Some("grok-shell")
        );
        assert_eq!(
            h.get("x-grok-model-override").map(String::as_str),
            Some(model)
        );
        assert!(!h.contains_key("x-grok-req-id"));
        assert!(!h.contains_key("x-grok-session-id"));
        assert!(!h.contains_key("x-grok-agent-id"));
        assert!(!h.contains_key("x-grok-turn-idx"));
    }

    #[test]
    fn test_grok_cli_headers_chat_has_identity_no_doom_loop() {
        let h = grok_header_map(Protocol::OpenAiChat, "grok-4.6", Some("sess-abc"));
        assert_identity(&h, "grok-4.6");
        assert_eq!(
            h.get("x-grok-conv-id").map(String::as_str),
            Some("sess-abc")
        );
        assert!(!h.contains_key("x-grok-doom-loop-check"));
    }

    #[test]
    fn test_grok_cli_headers_responses_has_doom_loop() {
        let h = grok_header_map(Protocol::OpenAiResponses, "grok-4.6", Some("sess-abc"));
        assert_identity(&h, "grok-4.6");
        assert_eq!(
            h.get("x-grok-conv-id").map(String::as_str),
            Some("sess-abc")
        );
        assert_eq!(
            h.get("x-grok-doom-loop-check").map(String::as_str),
            Some("1024")
        );
        assert_eq!(
            h.get("x-grok-exact-repetition-check").map(String::as_str),
            Some("64")
        );
    }

    #[test]
    fn test_grok_cli_headers_chat_gpt_empty() {
        assert!(grok_cli_headers(Protocol::OpenAiChat, "gpt-4", Some("sess"), "1.0.5").is_empty());
        assert!(grok_cli_headers(
            Protocol::OpenAiResponses,
            "gpt-5.6-terra",
            Some("sess"),
            "1.0.5"
        )
        .is_empty());
        assert!(grok_cli_headers(Protocol::Claude, "grok-4.6", Some("sess"), "1.0.5").is_empty());
        assert!(grok_cli_headers(Protocol::Gemini, "grok-4.6", Some("sess"), "1.0.5").is_empty());
    }

    #[test]
    fn test_grok_cli_headers_empty_session_omits_conv_id() {
        for session in [None, Some(""), Some("   ")] {
            let h = grok_header_map(Protocol::OpenAiChat, "grok-4.6", session);
            assert_identity(&h, "grok-4.6");
            assert!(!h.contains_key("x-grok-conv-id"));
            assert!(h.contains_key("X-XAI-Token-Auth"));
        }
    }

    #[test]
    fn test_grok_cli_headers_conv_id_is_raw_not_uuid() {
        let raw = "user_session_not-a-uuid";
        let h = grok_header_map(Protocol::OpenAiChat, "Grok-4.6", Some(raw));
        assert_eq!(h.get("x-grok-conv-id").map(String::as_str), Some(raw));
        let padded = "  sess-abc  ";
        let h = grok_header_map(Protocol::OpenAiChat, "grok-4.6", Some(padded));
        assert_eq!(h.get("x-grok-conv-id").map(String::as_str), Some(padded));
    }

    #[test]
    fn test_grok_cli_headers_empty_model_skips_override() {
        // 空模型名不含 grok,整组头都不发(override 无从谈起)
        assert!(grok_cli_headers(Protocol::OpenAiChat, "", Some("sess"), "1.0.5").is_empty());
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
