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

/// 按协议取上游请求路径
///
/// 版本前缀约定(与 参考实现/OpenAI 一致):anthropic 协议 base_url 不含版本,路径带 /v1;
/// openai 协议 base_url 已含版本前缀(/v1 或 /v3 等),路径不带版本。
fn endpoint_path(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Claude => "/v1/messages",
        Protocol::OpenAiChat => "/chat/completions",
        Protocol::OpenAiResponses => "/responses",
    }
}

/// 按协议取 User-Agent(对齐上游期望的客户端标识)
///
/// openai chat → claude-cli;responses → codex_cli_rs。部分上游按 UA
/// 分流缓存/特性,reqwest 默认 UA 会被识别为非官方客户端。
fn user_agent(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Claude => "claude-cli/2.1.221",
        Protocol::OpenAiChat => "claude-cli/2.1.221",
        Protocol::OpenAiResponses => {
            "codex_cli_rs/0.146.0 (Mac OS 26.5.1; aarch64) iTerm.app/3.6.10"
        }
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
    ///   `Cache-Control: no-cache`(对齐 openai_compat_executor)
    /// - `session_id`:responses 链路发 `Session_id` 头(对齐 cacheHelper,
    ///   值为 prompt_cache_key,上游按它做缓存亲和)
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
        extra_headers: &[(String, String)],
    ) -> anyhow::Result<UpstreamResponse> {
        let proxy_key = self.resolve_proxy(provider_proxy);
        let client = self.client_for(&proxy_key);
        let url = format!(
            "{}{}",
            base_url.trim_end_matches('/'),
            endpoint_path(protocol)
        );

        let mut req = client
            .post(&url)
            .bearer_auth(api_key)
            .header(reqwest::header::USER_AGENT, user_agent(protocol));
        if is_stream && matches!(protocol, Protocol::OpenAiChat) {
            req = req
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .header(reqwest::header::CACHE_CONTROL, "no-cache");
        }
        if let Some(sid) = session_id {
            if matches!(protocol, Protocol::OpenAiResponses) {
                req = req.header("Session_id", sid);
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
        assert_eq!(endpoint_path(Protocol::Claude), "/v1/messages");
        assert_eq!(endpoint_path(Protocol::OpenAiChat), "/chat/completions");
        assert_eq!(endpoint_path(Protocol::OpenAiResponses), "/responses");
    }

    #[test]
    fn test_user_agent_per_protocol() {
        assert_eq!(user_agent(Protocol::OpenAiChat), "claude-cli/2.1.221");
        assert!(user_agent(Protocol::OpenAiResponses).starts_with("codex_cli_rs/0.146.0"));
        assert_eq!(user_agent(Protocol::Claude), "claude-cli/2.1.221");
    }

    #[test]
    fn test_url_join_no_double_version() {
        // openai 协议:base_url 已含版本前缀,路径不再重复 /v1
        let base = "https://dashscope.aliyuncs.com/compatible-mode/v1";
        let url = format!(
            "{}{}",
            base.trim_end_matches('/'),
            endpoint_path(Protocol::OpenAiChat)
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
            endpoint_path(Protocol::OpenAiChat)
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
            endpoint_path(Protocol::Claude)
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
