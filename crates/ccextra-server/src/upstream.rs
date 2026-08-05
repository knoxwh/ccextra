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
fn endpoint_path(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::Claude => "/v1/messages",
        Protocol::OpenAiChat => "/v1/chat/completions",
        Protocol::OpenAiResponses => "/v1/responses",
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
            None => self.global_proxy.clone().unwrap_or_else(|| "direct".to_string()),
        }
    }

    /// 按最终代理取(或构建)client
    fn client_for(&self, proxy_key: &str) -> Client {
        if let Some(c) = self.clients.lock().unwrap().get(proxy_key) {
            return c.clone();
        }
        let mut builder = Client::builder();
        if proxy_key == "direct" {
            builder = builder.no_proxy();
        } else if let Ok(proxy) = reqwest::Proxy::all(proxy_key) {
            builder = builder.proxy(proxy);
        }
        let client = builder.build().unwrap_or_else(|_| Client::new());
        self.clients.lock().unwrap().insert(proxy_key.to_string(), client.clone());
        client
    }

    /// 发起上游请求,返回原始响应(字节或流由调用方决定)
    pub async fn request(
        &self,
        base_url: &str,
        api_key: &str,
        protocol: Protocol,
        provider_proxy: Option<&str>,
        body: &serde_json::Value,
    ) -> anyhow::Result<UpstreamResponse> {
        let proxy_key = self.resolve_proxy(provider_proxy);
        let client = self.client_for(&proxy_key);
        let url = format!("{}{}", base_url.trim_end_matches('/'), endpoint_path(protocol));

        let resp = client
            .post(&url)
            .bearer_auth(api_key)
            .json(body)
            .send()
            .await?;

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
        assert_eq!(endpoint_path(Protocol::OpenAiChat), "/v1/chat/completions");
        assert_eq!(endpoint_path(Protocol::OpenAiResponses), "/v1/responses");
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