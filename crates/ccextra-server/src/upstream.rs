// 上游请求客户端 (reqwest)
//
// 支持代理:全局 proxy 兜底 + 每 provider 覆盖。
// 按"最终代理"缓存 client,避免每次请求重建。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use ccextra_core::route::Protocol;
use reqwest::Client;
use serde::Deserialize;

/// 连接池空闲淘汰(对齐 grok `GROK_POOL_IDLE_TIMEOUT_SECS` 默认 90s)。
/// 当前服务流 chunk idle 为 180s,不是池寿命。
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// send() 上限(等到响应头)。60s:上游收连不回头时快速失败交客户端退避;
/// 流式之后的 chunk idle 走 STREAM_IDLE_TIMEOUT。
/// 无 Client::timeout,避免掐整条 SSE。
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// 上游请求结果
#[derive(Debug)]
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

/// 对齐 codex rust-v0.155.0 build_routing_hint_header 与 CPA b97f71da/dd3b657b:
/// ChatGPT 后端(Codex OAuth 订阅)Responses 请求携带 `model=<解析后模型>`,
/// body 存在非空 `service_tier` 时追加 `;tier=<值>`。必须在压缩前从最终
/// body 构造;入站同名头(操作员覆盖)由 extra_headers 后置覆盖。
fn codex_routing_hint(upstream_model: &str, body: &serde_json::Value) -> Option<String> {
    if !is_gpt_model(upstream_model) {
        return None;
    }
    let mut hint = format!("model={upstream_model}");
    if let Some(tier) = body.get("service_tier").and_then(|v| v.as_str()) {
        let tier = tier.trim();
        if !tier.is_empty() {
            hint.push_str(";tier=");
            hint.push_str(tier);
        }
    }
    Some(hint)
}

/// Responses 路由字段先发(对齐 codex ed0cc1a4ab)，不克隆大 input 或改动其余键序。
fn serialize_request_body(
    protocol: Protocol,
    body: &serde_json::Value,
) -> serde_json::Result<Vec<u8>> {
    use serde::ser::{SerializeMap, Serializer};
    let Some(object) = body
        .as_object()
        .filter(|_| matches!(protocol, Protocol::OpenAiResponses))
    else {
        return serde_json::to_vec(body);
    };
    let mut serializer = serde_json::Serializer::new(Vec::new());
    let mut map = serializer.serialize_map(Some(object.len()))?;
    for key in ["model", "stream", "service_tier"] {
        if let Some(value) = object.get(key) {
            map.serialize_entry(key, value)?;
        }
    }
    for (key, value) in object {
        if !matches!(key.as_str(), "model" | "stream" | "service_tier") {
            map.serialize_entry(key, value)?;
        }
    }
    map.end()?;
    Ok(serializer.into_inner())
}

/// codex 订阅请求体 zstd 压缩(对齐 codex prepare_encoded_json:level 3,
/// debug 记录压缩前后字节与耗时)。ccextra 的 codex extra_headers 由
/// messages.rs 新建仅含 chatgpt-account-id,不存在 Content-Encoding
/// 冲突路径,故省略 codex 的冲突报错守卫。
fn compress_request_body(body: bytes::Bytes) -> anyhow::Result<bytes::Bytes> {
    let started = std::time::Instant::now();
    let compressed = zstd::stream::encode_all(&body[..], 3)
        .map_err(|e| anyhow::anyhow!("zstd 压缩请求体失败: {e}"))?;
    tracing::debug!(
        pre_bytes = body.len(),
        post_bytes = compressed.len(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "codex 请求体 zstd 压缩"
    );
    Ok(bytes::Bytes::from(compressed))
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
        // Cursor SDK 走 sidecar 专用通道,不经通用 upstream
        Protocol::CursorSdk => {
            unreachable!("CursorSdk must bypass generic upstream")
        }
    }
}

/// Grok CLI 身份头常量
/// Token-Auth 对齐 grok-build GrokAuthCredentials (`xai-grok-cli`);
/// identifier/mode 对齐 sub2api ad05eda15 的官方交互式 CLI 抓包。
const GROK_TOKEN_AUTH: &str = "xai-grok-cli";
const GROK_CLIENT_IDENTIFIER: &str = "grok-pager";
const GROK_CLIENT_MODE: &str = "interactive";

/// 模型名是否为 GPT/Codex 模型(对齐 ccextra_core::convert::to_openai_responses::is_gpt_upstream)
pub(crate) fn is_gpt_model(upstream_model: &str) -> bool {
    ccextra_core::convert::to_openai_responses::is_gpt_upstream(upstream_model)
}

/// 模型名是否按 *grok* 匹配(大小写不敏感)
pub(crate) fn is_grok_model(upstream_model: &str) -> bool {
    upstream_model.to_ascii_lowercase().contains("grok")
}

/// grok chat/responses 出站头(会话语义对齐 grok-build，交互身份对齐 sub2api)
/// Token-Auth 来自 GrokAuthCredentials，version/identifier/mode 来自 CLI 身份;
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
        ("x-grok-client-mode", GROK_CLIENT_MODE.to_string()),
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
/// (对齐 sub2api CLIUserAgent 的双组件 UA);其余用 claude-cli
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
                "grok-pager/{version} grok-shell/{version} ({}; {})",
                std::env::consts::OS,
                std::env::consts::ARCH,
                version = user_agents.grok_version
            )
        }
        _ => user_agents.claude_cli.to_string(),
    }
}

/// Antigravity 连接池配置(对齐 CPA AntigravityConnectionPoolConfig)
///
/// 默认短连接:空闲连接响应结束即关闭,防止凭证轮换下 socket 堆积与
/// 陈旧连接错误(CPA #5494);仅在显式启用时保留连接池。
#[derive(Debug, Deserialize, Clone, Default)]
pub struct AntigravityConfig {
    /// 连接池子配置;字段缺失按默认值处理
    #[serde(default, rename = "connection-pool")]
    pub connection_pool: AntigravityConnectionPoolConfig,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct AntigravityConnectionPoolConfig {
    /// 是否启用连接池;默认 false(短连接模式)
    #[serde(default)]
    pub enabled: Option<bool>,
    /// 空闲连接存活时长,如 "30s";默认 30s,上限 210s(防超 GFE 240s cutoff)
    #[serde(default, rename = "idle-conn-timeout")]
    pub idle_conn_timeout: Option<String>,
    /// 每凭证每 host 最大空闲连接数;默认 2,上限 100;负值回退短连接
    #[serde(default, rename = "max-idle-conns-per-host")]
    pub max_idle_conns_per_host: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AntigravityPoolSettings {
    /// 短连接模式:不保留任何空闲连接(其余字段无意义)
    pub short_mode: bool,
    pub idle_conn_timeout: Duration,
    pub max_idle_conns_per_host: usize,
}

pub(crate) const ANTIGRAVITY_DEFAULT_IDLE_CONN_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const ANTIGRAVITY_MAX_ALLOWED_IDLE_CONN_TIMEOUT: Duration = Duration::from_secs(210);
pub(crate) const ANTIGRAVITY_DEFAULT_MAX_IDLE_CONNS_PER_HOST: usize = 2;
pub(crate) const ANTIGRAVITY_MAX_ALLOWED_MAX_IDLE_CONNS_PER_HOST: usize = 100;

impl AntigravityPoolSettings {
    /// 按配置解析连接池设置(对齐 CPA resolveAntigravityPoolSettings)
    pub fn resolve(cfg: Option<&AntigravityConfig>) -> Self {
        let defaults = Self {
            short_mode: true,
            idle_conn_timeout: ANTIGRAVITY_DEFAULT_IDLE_CONN_TIMEOUT,
            max_idle_conns_per_host: ANTIGRAVITY_DEFAULT_MAX_IDLE_CONNS_PER_HOST,
        };
        let Some(cfg) = cfg else {
            return defaults;
        };
        let pool = &cfg.connection_pool;
        // 仅显式 enabled: true 才启用连接池,其余一律短连接
        if !pool.enabled.unwrap_or(false) {
            return defaults;
        }
        let mut settings = Self {
            short_mode: false,
            ..defaults
        };
        if let Some(raw) = pool
            .idle_conn_timeout
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match parse_go_duration(raw) {
                // 0/负值(归一为 ZERO)回退短连接(对齐 CPA d <= 0)
                Ok(d) if d.is_zero() => return defaults,
                Ok(d) => {
                    settings.idle_conn_timeout = d.min(ANTIGRAVITY_MAX_ALLOWED_IDLE_CONN_TIMEOUT)
                }
                Err(e) => {
                    tracing::warn!("antigravity 非法 idle-conn-timeout {raw:?}: {e},沿用默认值");
                }
            }
        }
        if let Some(val) = pool.max_idle_conns_per_host {
            if val < 0 {
                return defaults;
            }
            settings.max_idle_conns_per_host =
                (val as usize).min(ANTIGRAVITY_MAX_ALLOWED_MAX_IDLE_CONNS_PER_HOST);
        }
        settings
    }
}

/// 解析 Go time.Duration 字符串子集:ns/us/ms/s/m/h,支持十进制小数
/// (如 "1.5s",对齐 Go time.ParseDuration 浮点形式);负值或 0 归一为
/// ZERO(调用方据此回退短连接,对齐 CPA d <= 0),其余非法输入返回 Err
fn parse_go_duration(raw: &str) -> anyhow::Result<Duration> {
    let raw = raw.trim();
    let split = raw
        .find(|c: char| c.is_ascii_alphabetic() || c == 'µ')
        .unwrap_or(raw.len());
    let (value, unit) = raw.split_at(split);
    let value: f64 = value
        .parse()
        .map_err(|_| anyhow::anyhow!("非法时长数值 {value:?}"))?;
    let secs = match unit {
        "ns" => value / 1e9,
        "us" | "µs" => value / 1e6,
        "ms" => value / 1e3,
        "s" => value,
        "m" => value * 60.0,
        "h" => value * 3600.0,
        "" => return Err(anyhow::anyhow!("缺少时间单位")),
        other => return Err(anyhow::anyhow!("不支持的时间单位 {other:?}")),
    };
    if !secs.is_finite() || secs > u64::MAX as f64 {
        return Err(anyhow::anyhow!("时长溢出"));
    }
    if secs <= 0.0 {
        return Ok(Duration::ZERO);
    }
    Ok(Duration::from_secs_f64(secs))
}

/// client 缓存键:(最终代理, 是否 antigravity, 是否禁 redirect)
type ClientCacheKey = (String, bool, bool);

#[derive(Clone)]
pub struct UpstreamClient {
    global_proxy: Option<String>,
    clients: std::sync::Arc<Mutex<HashMap<ClientCacheKey, Client>>>,
    /// Antigravity 连接池设置(短连接默认,对齐 CPA antigravity.executor)
    ant_pool: AntigravityPoolSettings,
    #[cfg(test)]
    pub(crate) attempts: std::sync::Arc<Mutex<Vec<String>>>,
}

impl UpstreamClient {
    pub fn new(global_proxy: Option<String>) -> Self {
        Self::with_ant_pool(global_proxy, None)
    }

    pub fn with_ant_pool(
        global_proxy: Option<String>,
        ant_cfg: Option<&AntigravityConfig>,
    ) -> Self {
        Self {
            global_proxy,
            clients: std::sync::Arc::new(Mutex::new(HashMap::new())),
            ant_pool: AntigravityPoolSettings::resolve(ant_cfg),
            #[cfg(test)]
            attempts: Default::default(),
        }
    }

    /// 全局代理(供 cursor 目录刷新等不经 upstream 的路径复用)
    pub fn global_proxy(&self) -> Option<&str> {
        self.global_proxy.as_deref()
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

    /// 按最终代理 + 协议取(或构建)client
    ///
    /// Antigravity 连接池设置独立生效(对齐 CPA antigravity executor transport):
    /// 缓存键为 (代理, 是否 antigravity, 是否禁 redirect) 元组,专属 transport 不影响其他协议共享池。
    ///
    /// `no_redirect`:Codex 订阅请求(带 chatgpt-account-id 头)禁跟随 redirect。
    /// reqwest 跨 host redirect 只剥 Authorization/Cookie 类敏感头,自定义身份头
    /// (chatgpt-account-id / Session-Id / Originator)会原样转发到重定向目标,
    /// 泄漏订阅身份。对齐 codex CLI:带账号路由头的请求 redirect Policy::none()。
    pub(crate) fn client_for(
        &self,
        proxy_key: &str,
        protocol: Protocol,
        no_redirect: bool,
    ) -> anyhow::Result<Client> {
        let ant = matches!(protocol, Protocol::Antigravity);
        let cache_key: ClientCacheKey = (proxy_key.to_string(), ant, no_redirect);
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(c) = clients.get(&cache_key) {
            return Ok(c.clone());
        }
        let mut builder = Client::builder()
            // 限制单个 host 最大空闲连接数，防毒化池
            .pool_max_idle_per_host(4)
            // 池空闲 90s(对齐 grok);流 chunk idle 为服务当前设置 180s,不是池寿命。
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            // 建连超时 10s，防 DNS/TLS 握手卡死
            .connect_timeout(CONNECT_TIMEOUT)
            // TCP 层探活,防死连接滞留/中间设备静默断
            .tcp_keepalive(std::time::Duration::from_secs(60))
            // 禁 Nagle:SSE 首帧/心跳/delta 都是小包,不等积压直接发,压 TTFT
            .tcp_nodelay(true)
            // HTTP/2 探活与空闲 Ping，防静默掉线(对齐 grok shared_http)
            .http2_keep_alive_interval(std::time::Duration::from_secs(15))
            .http2_keep_alive_timeout(std::time::Duration::from_secs(5))
            .http2_keep_alive_while_idle(true);
        if no_redirect {
            builder = builder.redirect(reqwest::redirect::Policy::none());
        }
        if ant {
            // Antigravity 专属池:默认短连接(不保留空闲连接,响应结束即关,
            // 不发 Connection: close);显式启用时按配置保留池
            let pool = self.ant_pool;
            if pool.short_mode {
                builder = builder.pool_max_idle_per_host(0);
            } else {
                builder = builder
                    .pool_max_idle_per_host(pool.max_idle_conns_per_host)
                    .pool_idle_timeout(pool.idle_conn_timeout);
            }
        }
        if proxy_key == "direct" {
            builder = builder.no_proxy();
        } else if let Ok(proxy) = reqwest::Proxy::all(proxy_key) {
            builder = builder.proxy(proxy);
        }
        let client = builder
            .build()
            .map_err(|e| anyhow::anyhow!("构建上游 HTTP client 失败: {e}"))?;
        clients.insert(cache_key, client.clone());
        Ok(client)
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
        // Cursor SDK 走 sidecar 专用通道,禁止进入通用 upstream
        if matches!(protocol, Protocol::CursorSdk) {
            anyhow::bail!("cursor_sdk must bypass generic upstream");
        }
        // 对齐 codex EncodedJsonBody:一次序列化,Bytes 共享分配;
        // stale-connection 重试复用同一份字节,不重复序列化
        let body_bytes = bytes::Bytes::from(
            serialize_request_body(protocol, body)
                .map_err(|e| anyhow::anyhow!("序列化请求体失败: {e}"))?,
        );
        // 对齐 codex responses_request_compression:仅 codex OAuth 订阅请求
        // (uses_codex_backend + openai provider 的等价标记 chatgpt-account-id)
        // 压缩;压缩一次,stale 重试共享压缩字节(into_prepared 语义)
        let body_bytes = if extra_headers.contains_key("chatgpt-account-id") {
            compress_request_body(body_bytes)?
        } else {
            body_bytes
        };
        let upstream_model = body.get("model").and_then(|v| v.as_str()).unwrap_or("");
        // 路由提示从压缩前的最终 body 构造(对齐 codex build_routing_hint_header)
        let routing_hint = if extra_headers.contains_key("chatgpt-account-id")
            && matches!(protocol, Protocol::OpenAiResponses)
        {
            codex_routing_hint(upstream_model, body)
        } else {
            None
        };
        match self
            .request_once(
                base_url,
                api_key,
                protocol,
                provider_proxy,
                &body_bytes,
                upstream_model,
                routing_hint.as_deref(),
                is_stream,
                session_id,
                thread_id,
                extra_headers,
                user_agents,
                inbound_user_agent,
            )
            .await
        {
            Ok(resp) => Ok(resp),
            Err(e) if is_stale_connection(&e) => {
                tracing::warn!(error = %e, "上游连接失效,重试一次");
                self.request_once(
                    base_url,
                    api_key,
                    protocol,
                    provider_proxy,
                    &body_bytes,
                    upstream_model,
                    routing_hint.as_deref(),
                    is_stream,
                    session_id,
                    thread_id,
                    extra_headers,
                    user_agents,
                    inbound_user_agent,
                )
                .await
            }
            Err(e) => Err(e),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn request_once(
        &self,
        base_url: &str,
        api_key: &str,
        protocol: Protocol,
        provider_proxy: Option<&str>,
        body: &bytes::Bytes,
        upstream_model: &str,
        routing_hint: Option<&str>,
        is_stream: bool,
        session_id: Option<&str>,
        thread_id: Option<&str>,
        extra_headers: &axum::http::HeaderMap,
        user_agents: &crate::http::UserAgentSet,
        inbound_user_agent: Option<&str>,
    ) -> anyhow::Result<UpstreamResponse> {
        #[cfg(test)]
        self.attempts.lock().unwrap().push(base_url.to_owned());
        let proxy_key = self.resolve_proxy(provider_proxy);
        // chatgpt-account-id 仅 Codex OAuth 订阅请求携带:禁 redirect + zstd 压缩标记
        let codex_subscription = extra_headers.contains_key("chatgpt-account-id");
        let client = self.client_for(&proxy_key, protocol, codex_subscription)?;

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
                if let Some(hint) = routing_hint {
                    req = req.header("X-Codex-Routing-Hint", hint);
                }
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
            // 对齐 sub2api ApplyCLIProxyHeaders:仅官方 host 请求携带响应认证标记。
            if reqwest::Url::parse(&url).is_ok_and(|parsed| {
                parsed
                    .host_str()
                    .is_some_and(|host| host.eq_ignore_ascii_case("cli-chat-proxy.grok.com"))
            }) {
                req = req.header("x-authenticateresponse", "authenticate-response");
            }
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

        // Gemini/Antigravity:从 session_id 参数派生确定性 UUID,注入 x-vscode-sessionid
        // (对齐 CPA 5907285:缓存正交隔离;Responses 已在上方 Session-Id 注入)
        if matches!(protocol, Protocol::Gemini | Protocol::Antigravity) {
            if let Some(sid) = session_id {
                let session_uuid = derive_session_uuid(sid);
                req = req.header("x-vscode-sessionid", session_uuid);
            }
        }

        for (name, value) in extra_headers {
            req = req.header(name, value);
        }
        // 已编码字节直接发送(对齐 codex prepare_body_for_send:补 Content-Type,
        // Bytes clone 零拷贝);仅 Responses 顶层路由键前置，字段值不变
        // codex 订阅请求 body 已在 request() 内 zstd 压缩,声明编码
        if codex_subscription {
            req = req.header(reqwest::header::CONTENT_ENCODING, "zstd");
        }
        let resp = send_with_timeout(
            req.header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(bytes::Bytes::clone(body)),
        )
        .await?;

        let status = resp.status();
        Ok(UpstreamResponse { status, body: resp })
    }
}

/// send() 等到响应头。流式之后读 body 走 chunk idle,不套 Client::timeout。
pub(crate) async fn send_with_timeout(
    request: reqwest::RequestBuilder,
) -> anyhow::Result<reqwest::Response> {
    match tokio::time::timeout(SEND_TIMEOUT, request.send()).await {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => Err(anyhow::anyhow!(
            "上游请求超时 ({}s)",
            SEND_TIMEOUT.as_secs()
        )),
    }
}

/// 连接失效:立即额外尝试一次,不耗 3s 重试预算。
/// send() 超时不走这里,避免重复等待。
/// 普通建连失败/建连超时不是死连接:不得进入内部快速重试(否则单 URL
/// 建连超时被放大成约 20s),交给外层 URL fallback 与退避预算。
fn is_stale_connection(err: &anyhow::Error) -> bool {
    let (is_connect, is_timeout) = match err.downcast_ref::<reqwest::Error>() {
        Some(e) => (e.is_connect(), e.is_timeout()),
        None => (false, false),
    };
    stale_by_classification(is_connect, is_timeout, err)
}

/// 从 Claude 入站头 x-session-id 派生确定性 UUID v4(对齐 CPA 5907285)
fn derive_session_uuid(seed: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"antigravity-session-v1:");
    hasher.update(seed.as_bytes());
    let hash = hasher.finalize();
    // RFC 4122 v4 格式
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-4{:01x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        hash[0], hash[1], hash[2], hash[3],
        hash[4], hash[5],
        hash[6] & 0x0f, hash[7],
        (hash[8] & 0x3f) | 0x80, hash[9],
        hash[10], hash[11], hash[12], hash[13], hash[14], hash[15]
    )
}

/// 分类优先级:显式 connect/timeout 标志先判——建连失败/建连超时一律非
/// stale,即使消息含 reset 字样;其余沿错误链匹配复用连接死亡特征。
/// reqwest 的 Display 不含 source,须遍历整条错误链找 hyper/io 层消息。
fn stale_by_classification(is_connect: bool, is_timeout: bool, err: &anyhow::Error) -> bool {
    if is_connect || is_timeout {
        return false;
    }
    err.chain().any(|cause| {
        let msg = cause.to_string().to_ascii_lowercase();
        msg.contains("connection reset")
            || msg.contains("broken pipe")
            || msg.contains("connection closed")
            || msg.contains("error 54")
            || msg.contains("error 32")
            || msg.contains("error 104")
    })
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
            claude_cli: Arc::new("claude-cli/2.1.258".to_string()),
            codex_tui: Arc::new("codex_cli_rs/0.153.3 (Mac OS 26.6.2; arm64)".to_string()),
            grok_version: Arc::new("1.0.5".to_string()),
            antigravity: Arc::new("antigravity/hub/2.10.0 darwin/arm64".to_string()),
        };
        const CLAUDE_CLI: &str = "claude-cli/2.1.258";
        const CODEX_CLI: &str = "codex_cli_rs/0.153.3 (Mac OS 26.6.2; arm64)";
        assert_eq!(
            user_agent(Protocol::OpenAiChat, "gpt-5.6-terra", &uas, None),
            CLAUDE_CLI
        );
        assert_eq!(
            user_agent(Protocol::OpenAiResponses, "gpt-5.6-terra", &uas, None),
            CODEX_CLI
        );
        assert_eq!(
            user_agent(Protocol::OpenAiResponses, "GPT-5.6-sol", &uas, None),
            CODEX_CLI
        );
        assert_eq!(
            user_agent(Protocol::OpenAiResponses, "openai/gpt-5.6", &uas, None),
            CODEX_CLI
        );
        let grok_ua = user_agent(Protocol::OpenAiResponses, "grok-4.6", &uas, None);
        assert!(grok_ua.starts_with("grok-pager/1.0.5 grok-shell/1.0.5 ("));
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
            Some("grok-pager")
        );
        assert_eq!(
            h.get("x-grok-client-mode").map(String::as_str),
            Some("interactive")
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
    fn test_derive_session_uuid_deterministic() {
        // 对齐 CPA 5907285:从 x-session-id 派生确定性 UUID v4
        let uuid1 = derive_session_uuid("test-session-123");
        let uuid2 = derive_session_uuid("test-session-123");
        assert_eq!(uuid1, uuid2, "相同 session ID 必须派生相同 UUID");
        assert_eq!(uuid1.len(), 36, "UUID 格式");
        assert!(uuid1.contains('-'), "UUID 包含短横线");

        // 验证 v4 格式（第3组第1字节高4位为4）
        let parts: Vec<&str> = uuid1.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert!(parts[2].starts_with('4'), "UUID v4 格式");

        // 不同 session ID 派生不同 UUID
        let uuid3 = derive_session_uuid("different");
        assert_ne!(uuid1, uuid3);

        // 验证盐前缀影响结果
        let no_prefix = {
            use sha2::{Digest, Sha256};
            let hash = Sha256::digest(b"test-session-123");
            format!(
                "{:02x}{:02x}{:02x}{:02x}",
                hash[0], hash[1], hash[2], hash[3]
            )
        };
        assert!(!uuid1.starts_with(&no_prefix), "必须使用盐前缀");
    }

    #[test]
    fn test_grok_cli_headers_empty_model_skips_override() {
        // 空模型名不含 grok,整组头都不发(override 无从谈起)
        assert!(grok_cli_headers(Protocol::OpenAiChat, "", Some("sess"), "1.0.5").is_empty());
    }

    #[tokio::test]
    async fn grok_authenticate_response_is_limited_to_official_host() {
        use crate::test_support::spawn_captured_server;
        for (host, protocol, model, expected) in [
            (
                "cli-chat-proxy.grok.com",
                Protocol::OpenAiChat,
                "grok-4.7",
                Some("authenticate-response"),
            ),
            (
                "CLI-CHAT-PROXY.GROK.COM",
                Protocol::OpenAiResponses,
                "grok-4.7",
                Some("authenticate-response"),
            ),
            ("api.x.ai", Protocol::OpenAiResponses, "grok-4.7", None),
            (
                "cli-chat-proxy.grok.com.example.com",
                Protocol::OpenAiResponses,
                "grok-4.7",
                None,
            ),
            (
                "cli-chat-proxy.grok.com",
                Protocol::OpenAiResponses,
                "gpt-5",
                None,
            ),
            (
                "cli-chat-proxy.grok.com",
                Protocol::Claude,
                "grok-4.7",
                None,
            ),
        ] {
            let (server, captured) = spawn_captured_server(
                &endpoint_path(protocol, false),
                axum::http::StatusCode::OK,
                "{}",
            )
            .await;
            let addr: std::net::SocketAddr =
                server.url.trim_start_matches("http://").parse().unwrap();
            let client = UpstreamClient::new(None);
            client.clients.lock().unwrap().insert(
                ("direct".into(), false, false),
                Client::builder()
                    .no_proxy()
                    .resolve(&host.to_ascii_lowercase(), addr)
                    .build()
                    .unwrap(),
            );
            let result = client
                .request(
                    &format!("http://{host}:{}", addr.port()),
                    "sk-test",
                    protocol,
                    Some("direct"),
                    &serde_json::json!({"model": model}),
                    false,
                    None,
                    None,
                    &axum::http::HeaderMap::new(),
                    &mock_user_agents(),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(result.status, axum::http::StatusCode::OK);
            assert_eq!(
                captured.header("x-authenticateresponse").as_deref(),
                expected,
                "{host} {protocol:?} {model}"
            );
        }
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
    fn test_proxy_priority() {
        // provider 覆盖 > 全局 > 直连
        let cases = [
            (
                Some("http://global-proxy:8080"),
                Some("http://provider-proxy:9090"),
                "http://provider-proxy:9090",
            ),
            (Some("http://global-proxy:8080"), Some("direct"), "direct"),
            (Some("http://global-proxy:8080"), Some(""), "direct"),
            (
                Some("http://global-proxy:8080"),
                None,
                "http://global-proxy:8080",
            ),
            (None, None, "direct"),
        ];
        for (global, provider, expected) in cases {
            let client = UpstreamClient::new(global.map(Into::into));
            assert_eq!(
                client.resolve_proxy(provider),
                expected,
                "global={global:?}, provider={provider:?}"
            );
        }
    }

    #[test]
    fn test_stale_connection_detects_reset_not_timeout_message() {
        assert!(is_stale_connection(&anyhow::anyhow!(
            "connection reset by peer"
        )));
        assert!(is_stale_connection(&anyhow::anyhow!("Broken pipe")));
        assert!(
            !is_stale_connection(&anyhow::anyhow!("上游请求超时 (60s)")),
            "send 超时不得进入死连接重试"
        );
        assert!(!is_stale_connection(&anyhow::anyhow!("invalid api key")));
    }

    // ── B4:死连接重试分类 ─────────────────────────────────────────────

    #[test]
    fn test_stale_classification_flags_beat_string_match() {
        // 建连失败/建连超时标志优先:即使消息含 reset 字样也不算 stale
        assert!(!stale_by_classification(
            true,
            false,
            &anyhow::anyhow!("connection reset by peer")
        ));
        assert!(!stale_by_classification(
            false,
            true,
            &anyhow::anyhow!("connection reset by peer")
        ));
        // 无标志时按消息判 stale
        assert!(stale_by_classification(
            false,
            false,
            &anyhow::anyhow!("connection reset by peer")
        ));
        // 错误链深层消息也要能匹配(reqwest Display 不含 source)
        let chained = anyhow::anyhow!("connection closed before message completed")
            .context("error sending request");
        assert!(stale_by_classification(false, false, &chained));
    }

    fn mock_user_agents() -> crate::http::UserAgentSet {
        crate::http::UserAgentSet {
            claude_cli: std::sync::Arc::new("claude-cli/2.1.258".into()),
            codex_tui: std::sync::Arc::new("codex_cli_rs/0.153.3".into()),
            grok_version: std::sync::Arc::new("1.0.5".into()),
            antigravity: std::sync::Arc::new("antigravity/hub/2.10.0".into()),
        }
    }

    async fn send_test_request(
        client: &UpstreamClient,
        addr: std::net::SocketAddr,
        user_agents: &crate::http::UserAgentSet,
    ) -> anyhow::Result<crate::upstream::UpstreamResponse> {
        let body = serde_json::json!({"model": "gpt-4", "messages": []});
        client
            .request(
                &format!("http://{addr}"),
                "sk-test",
                Protocol::OpenAiChat,
                None,
                &body,
                false,
                None,
                None,
                &axum::http::HeaderMap::new(),
                user_agents,
                None,
            )
            .await
    }

    #[tokio::test]
    async fn connect_and_timeout_do_not_retry() {
        for timeout in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            // 保留监听端口但不返回响应,产生真实 reqwest timeout。
            let _listener = if timeout {
                Some(listener)
            } else {
                drop(listener);
                None
            };
            let client = UpstreamClient::new(None);
            client.clients.lock().unwrap().insert(
                ("direct".into(), false, false),
                Client::builder()
                    .no_proxy()
                    .timeout(std::time::Duration::from_millis(100))
                    .build()
                    .unwrap(),
            );
            let err = send_test_request(&client, addr, &mock_user_agents())
                .await
                .unwrap_err();
            let transport = err.downcast_ref::<reqwest::Error>().unwrap();
            assert_eq!(transport.is_timeout(), timeout);
            if !timeout {
                assert!(transport.is_connect());
            }
            assert!(!is_stale_connection(&err));
            assert_eq!(*client.attempts.lock().unwrap(), [format!("http://{addr}")]);
        }
    }

    #[tokio::test]
    async fn stale_connection_retries_at_most_once() {
        use tokio::io::AsyncReadExt;
        for recover in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                for _ in 0..if recover { 1 } else { 2 } {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let _ = socket.read(&mut [0; 4096]).await.unwrap();
                }
                if recover {
                    axum::serve(listener, axum::Router::new().fallback(|| async { "ok" }))
                        .await
                        .unwrap();
                }
            });
            let client = UpstreamClient::new(None);
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                send_test_request(&client, addr, &mock_user_agents()),
            )
            .await;
            server.abort();
            let result = result.expect("重试必须有界完成");
            assert_eq!(result.is_ok(), recover);
            assert_eq!(
                *client.attempts.lock().unwrap(),
                vec![format!("http://{addr}"); 2]
            );
        }
    }

    /// 307 上游 + Codex 订阅身份头:禁跟随,身份头不外泄到重定向目标
    #[tokio::test]
    async fn codex_identity_header_disables_redirect() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target_listener.local_addr().unwrap();
        let target_hit = std::sync::Arc::new(AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&target_hit);
        let target_server = tokio::spawn(async move {
            let app = axum::Router::new().fallback(move || {
                let flag = std::sync::Arc::clone(&flag);
                async move {
                    flag.store(true, Ordering::SeqCst);
                    "ok"
                }
            });
            axum::serve(target_listener, app).await.unwrap();
        });

        let source_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = source_listener.local_addr().unwrap();
        let location = format!("http://{target_addr}/responses");
        let source_server = tokio::spawn(async move {
            let app = axum::Router::new().fallback(move || {
                let location = location.clone();
                async move { axum::response::Redirect::temporary(&location) }
            });
            axum::serve(source_listener, app).await.unwrap();
        });

        let client = UpstreamClient::new(None);
        let mut extra = axum::http::HeaderMap::new();
        extra.insert(
            axum::http::HeaderName::from_static("chatgpt-account-id"),
            "acct-123".parse().unwrap(),
        );
        let body = serde_json::json!({"model": "gpt-5.6-terra", "input": []});
        let resp = client
            .request(
                &format!("http://{source_addr}"),
                "codex-access-token",
                Protocol::OpenAiResponses,
                None,
                &body,
                false,
                None,
                None,
                &extra,
                &mock_user_agents(),
                None,
            )
            .await
            .unwrap();

        // 307 原样返回,不跟随,重定向目标无请求
        assert_eq!(resp.status.as_u16(), 307);
        assert!(!target_hit.load(Ordering::SeqCst), "重定向目标不应收到请求");

        source_server.abort();
        target_server.abort();
    }

    /// 无 Codex 身份头:redirect 行为不变(默认跟随)
    #[tokio::test]
    async fn non_codex_request_follows_redirect() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target_listener.local_addr().unwrap();
        let target_hit = std::sync::Arc::new(AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&target_hit);
        let target_server = tokio::spawn(async move {
            let app = axum::Router::new().fallback(move || {
                let flag = std::sync::Arc::clone(&flag);
                async move {
                    flag.store(true, Ordering::SeqCst);
                    "ok"
                }
            });
            axum::serve(target_listener, app).await.unwrap();
        });

        let source_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = source_listener.local_addr().unwrap();
        let location = format!("http://{target_addr}/responses");
        let source_server = tokio::spawn(async move {
            let app = axum::Router::new().fallback(move || {
                let location = location.clone();
                async move { axum::response::Redirect::temporary(&location) }
            });
            axum::serve(source_listener, app).await.unwrap();
        });

        let client = UpstreamClient::new(None);
        let body = serde_json::json!({"model": "gpt-5.6-terra", "input": []});
        let resp = client
            .request(
                &format!("http://{source_addr}"),
                "sk-test",
                Protocol::OpenAiResponses,
                None,
                &body,
                false,
                None,
                None,
                &axum::http::HeaderMap::new(),
                &mock_user_agents(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(resp.status.as_u16(), 200);
        assert!(target_hit.load(Ordering::SeqCst), "默认应跟随 redirect");

        source_server.abort();
        target_server.abort();
    }

    #[test]
    fn test_client_caching() {
        let client = UpstreamClient::new(None);
        let _c1 = client
            .client_for("direct", Protocol::OpenAiChat, false)
            .unwrap();
        let _c2 = client
            .client_for("direct", Protocol::OpenAiChat, false)
            .unwrap();
        // 同一 proxy_key 应返回相同 client(Arc clone)
        // 通过计数验证缓存命中
        let count_before = client.clients.lock().unwrap().len();
        let _c3 = client
            .client_for("direct", Protocol::OpenAiChat, false)
            .unwrap();
        let count_after = client.clients.lock().unwrap().len();
        assert_eq!(count_before, count_after, "缓存应命中,不应重建 client");
    }

    #[test]
    fn test_client_different_proxies() {
        let client = UpstreamClient::new(None);
        let _c1 = client
            .client_for("direct", Protocol::OpenAiChat, false)
            .unwrap();
        let _c2 = client
            .client_for("http://proxy1:8080", Protocol::OpenAiChat, false)
            .unwrap();
        let _c3 = client
            .client_for("http://proxy2:9090", Protocol::OpenAiChat, false)
            .unwrap();
        assert_eq!(client.clients.lock().unwrap().len(), 3);
    }

    #[test]
    fn test_antigravity_short_connection_isolation() {
        // Antigravity 默认短连接:缓存键独立于其他协议(#ant 后缀)
        let client = UpstreamClient::new(None);
        let _c1 = client
            .client_for("direct", Protocol::Antigravity, false)
            .unwrap();
        let _c2 = client
            .client_for("direct", Protocol::OpenAiChat, false)
            .unwrap();
        assert_eq!(client.clients.lock().unwrap().len(), 2);

        // 默认(未启用连接池)解析为短连接
        assert!(client.ant_pool.short_mode);
    }

    #[test]
    fn test_antigravity_pool_settings_resolution() {
        use super::{AntigravityConfig, AntigravityPoolSettings};

        // 缺省:短连接
        assert!(AntigravityPoolSettings::resolve(None).short_mode);

        // enabled: true → 默认 2 连接 / 30s
        let cfg: AntigravityConfig =
            serde_yaml::from_str("connection-pool:\n  enabled: true").unwrap();
        let s = AntigravityPoolSettings::resolve(Some(&cfg));
        assert!(!s.short_mode);
        assert_eq!(s.max_idle_conns_per_host, 2);
        assert_eq!(s.idle_conn_timeout, Duration::from_secs(30));

        // 超上限钳制:timeout ≤210s,idle ≤100
        let cfg: AntigravityConfig =
            serde_yaml::from_str("connection-pool:\n  enabled: true\n  idle-conn-timeout: \"600s\"\n  max-idle-conns-per-host: 500").unwrap();
        let s = AntigravityPoolSettings::resolve(Some(&cfg));
        assert_eq!(s.idle_conn_timeout, Duration::from_secs(210));
        assert_eq!(s.max_idle_conns_per_host, 100);

        // 负值 / 0 timeout 回退短连接
        let cfg: AntigravityConfig = serde_yaml::from_str(
            "connection-pool:\n  enabled: true\n  max-idle-conns-per-host: -1",
        )
        .unwrap();
        assert!(AntigravityPoolSettings::resolve(Some(&cfg)).short_mode);
        let cfg: AntigravityConfig =
            serde_yaml::from_str("connection-pool:\n  enabled: true\n  idle-conn-timeout: \"0s\"")
                .unwrap();
        assert!(AntigravityPoolSettings::resolve(Some(&cfg)).short_mode);
    }

    #[tokio::test]
    async fn responses_routing_fields_precede_large_input_on_the_wire() {
        use crate::test_support::{CapturedUpstream, TestServer};
        for (tier, subscription) in [
            (None, false),
            (Some("priority"), false),
            (Some("priority"), true),
        ] {
            let captured = CapturedUpstream::default();
            let cap = captured.clone();
            let server = TestServer::spawn(
                axum::Router::new()
                    .route(
                        "/responses",
                        axum::routing::post(
                            move |headers: axum::http::HeaderMap, body: bytes::Bytes| {
                                let cap = cap.clone();
                                async move {
                                    cap.record(headers, body);
                                    "{}"
                                }
                            },
                        ),
                    )
                    .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024)),
            )
            .await;
            let mut body = serde_json::json!({
                "instructions": "Say hi",
                "input": [{"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "x".repeat(2 * 1024 * 1024)}
                ]}],
                "tools": [{"z": 1, "a": 2}],
                "model": "gpt-test",
                "stream": true
            });
            if let Some(tier) = tier {
                body["service_tier"] = tier.into();
            }
            let original = serde_json::to_vec(&body).unwrap();
            let mut extra = axum::http::HeaderMap::new();
            if subscription {
                extra.insert("chatgpt-account-id", "acct-123".parse().unwrap());
            }
            let response = UpstreamClient::new(None)
                .request(
                    &server.url,
                    "sk-test",
                    Protocol::OpenAiResponses,
                    Some("direct"),
                    &body,
                    true,
                    None,
                    None,
                    &extra,
                    &mock_user_agents(),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(response.status, axum::http::StatusCode::OK);
            assert_eq!(
                captured.header("content-encoding").as_deref(),
                subscription.then_some("zstd")
            );
            let raw = captured.raw_body.lock().unwrap().clone().unwrap();
            let decoded = if subscription {
                zstd::stream::decode_all(&raw[..]).unwrap()
            } else {
                raw.to_vec()
            };
            let wire = std::str::from_utf8(&decoded).unwrap();
            let prefix = if tier.is_some() {
                r#"{"model":"gpt-test","stream":true,"service_tier":"priority","instructions":"Say hi","input":"#
            } else {
                r#"{"model":"gpt-test","stream":true,"instructions":"Say hi","input":"#
            };
            assert!(
                wire.starts_with(prefix),
                "路由字段必须在 input 前，subscription={subscription}, tier={tier:?}"
            );
            assert!(wire.ends_with(r#""tools":[{"z":1,"a":2}]}"#));
            assert!(decoded.len() > 2 * 1024 * 1024);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&decoded).unwrap(),
                body
            );
            assert_eq!(serde_json::to_vec(&body).unwrap(), original);
        }
    }

    #[test]
    fn request_serialization_preserves_other_protocols_and_missing_fields() {
        let body = serde_json::json!({
            "input": [{"z": 1, "a": 2}], "service_tier": null, "stream": false, "model": "test"
        });
        let original = serde_json::to_vec(&body).unwrap();
        for protocol in [
            Protocol::Claude,
            Protocol::OpenAiChat,
            Protocol::Gemini,
            Protocol::Antigravity,
        ] {
            assert_eq!(serialize_request_body(protocol, &body).unwrap(), original);
        }
        assert_eq!(
            std::str::from_utf8(&serialize_request_body(Protocol::OpenAiResponses, &body).unwrap())
                .unwrap(),
            r#"{"model":"test","stream":false,"service_tier":null,"input":[{"z":1,"a":2}]}"#
        );
        for body in [
            serde_json::json!({"input": []}),
            serde_json::json!({}),
            serde_json::json!(null),
        ] {
            assert_eq!(
                serialize_request_body(Protocol::OpenAiResponses, &body).unwrap(),
                serde_json::to_vec(&body).unwrap()
            );
        }
    }

    #[test]
    fn test_codex_routing_hint_includes_service_tier() {
        // 对齐 CPA applyCodexRoutingHint:任意非空 service_tier 原样追加
        assert_eq!(
            codex_routing_hint("gpt-5", &serde_json::json!({"service_tier": "priority"})),
            Some("model=gpt-5;tier=priority".to_string())
        );
        assert_eq!(
            codex_routing_hint("gpt-5", &serde_json::json!({"service_tier": "ultrafast"})),
            Some("model=gpt-5;tier=ultrafast".to_string())
        );
        assert_eq!(
            codex_routing_hint("gpt-5", &serde_json::json!({"service_tier": "  "})),
            Some("model=gpt-5".to_string())
        );
        assert_eq!(
            codex_routing_hint("gpt-5", &serde_json::json!({})),
            Some("model=gpt-5".to_string())
        );
    }

    #[test]
    fn test_codex_routing_hint_rejects_non_gpt_model() {
        assert_eq!(codex_routing_hint("grok-4", &serde_json::json!({})), None);
    }
}
