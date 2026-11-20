//! 官方 Anthropic 订阅透传路径(`Protocol::AnthropicOfficial`)。
//!
//! 隔离于通用转换/中转路径,确保红线 R1-R7 可单文件审计:
//! - 不落盘/缓存/记录任何订阅凭证与请求体
//! - 不改写请求体字节、不改写 model、不追加 beta、不调用 scrub_*
//! - 走 Claude 指纹车道(wreq/BoringSSL,HTTP/1.1)独立池,禁重定向,不做 stale/400 重试
//! - 响应状态码、非 hop-by-hop 头、错误体与 SSE 字节流原样回传,不加 180s 空闲超时

use crate::http::{ConfigSnapshot, error::AppError};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use ccextra_core::route::RouteDecision;
use serde::de::{self, Deserialize, Deserializer, IgnoredAny, MapAccess, Visitor};
use serde_json::Value;
use std::fmt;
use std::time::{Duration, Instant};

/// 轻量顶层 JSON 探针结果(与 `serde_json::from_slice::<Value>` 后者覆盖语义完全等价)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfficialProbe {
    pub model: String,
    pub stream: bool,
}

enum ProbeField {
    Model,
    Stream,
    Other,
}

impl<'de> Deserialize<'de> for ProbeField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldVisitor;
        impl Visitor<'_> for FieldVisitor {
            type Value = ProbeField;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("JSON object key")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(match v {
                    "model" => ProbeField::Model,
                    "stream" => ProbeField::Stream,
                    _ => ProbeField::Other,
                })
            }
        }
        deserializer.deserialize_str(FieldVisitor)
    }
}

struct RawProbe {
    model: Option<Value>,
    stream: Option<Value>,
}

impl<'de> Deserialize<'de> for RawProbe {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ProbeVisitor;
        impl<'de> Visitor<'de> for ProbeVisitor {
            type Value = RawProbe;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("top-level JSON object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut model: Option<Value> = None;
                let mut stream: Option<Value> = None;
                while let Some(key) = map.next_key::<ProbeField>()? {
                    match key {
                        ProbeField::Model => model = Some(map.next_value::<Value>()?),
                        ProbeField::Stream => stream = Some(map.next_value::<Value>()?),
                        ProbeField::Other => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(RawProbe { model, stream })
            }
        }
        deserializer.deserialize_map(ProbeVisitor)
    }
}

/// 解析顶层 JSON 对象中的 `model` 与 `stream`,不分配整棵 body 树。
/// 任一语法错、非对象、缺 `model` 或 `model` 非字符串均返回 `None`,交调用方回落旧路径返回既有 4xx。
pub(crate) fn probe_inbound_body(bytes: &[u8]) -> Option<OfficialProbe> {
    let raw: RawProbe = serde_json::from_slice(bytes).ok()?;
    let model = raw.model.as_ref()?.as_str()?.to_string();
    let stream = raw
        .stream
        .as_ref()
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(OfficialProbe { model, stream })
}

/// 读 body 前检查入站 `Content-Encoding`:含非 UTF-8 值或任一非 `identity` 项即返回 415。
pub(crate) fn reject_compressed_inbound(headers: &HeaderMap) -> Result<(), AppError> {
    for value in headers.get_all(header::CONTENT_ENCODING) {
        let Ok(raw) = value.to_str() else {
            return Err(AppError::with_status(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "官方透传路径不支持非 UTF-8 的 Content-Encoding 请求头",
            ));
        };
        for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if !item.eq_ignore_ascii_case("identity") {
                return Err(AppError::with_status(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    format!("官方透传路径不支持压缩请求体(Content-Encoding: {item})"),
                ));
            }
        }
    }
    Ok(())
}

/// 静态 hop-by-hop 头表(请求与响应共用;对齐 claude_relay.rs L54-63)
pub(crate) fn is_static_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "http2-settings"
    )
}

/// 提取 `Connection` 头里声明的动态 hop-by-hop 头名(小写)
fn connection_header_names(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// 校验入站是否带有非空且非纯空白的 `Authorization` 头
fn has_valid_authorization(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::AUTHORIZATION)
        .iter()
        .any(|v| v.as_bytes().iter().any(|b| !b.is_ascii_whitespace()))
}

/// 构造官方出站请求头:
/// - 原样透传 `Authorization`、`anthropic-*`、`x-stainless-*`、`x-app`、`x-claude-code-*`、`User-Agent`、`Accept`、`Accept-Encoding`、`Content-Type` 等
/// - 剔除 `host`、`content-length`、静/动态 hop-by-hop、`x-api-key`、`x-ccextra-key`
/// - 缺 `Accept-Encoding` 时本函数不补(wreq 解压层会自报 `zstd,gzip,deflate,br`);响应仍由 wreq 按 `Content-Encoding` 透明解压
/// - 不追加 beta、不写默认 UA、不写 `Accept`、不调用 `scrub_*`
pub(crate) fn official_request_headers(inbound: &HeaderMap) -> HeaderMap {
    let conn_names = connection_header_names(inbound);
    let mut out = HeaderMap::new();
    for (name, value) in inbound.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if is_static_hop_by_hop(&lower)
            || conn_names.iter().any(|c| c == &lower)
            || matches!(
                lower.as_str(),
                "host" | "content-length" | "x-api-key" | "x-ccextra-key"
            )
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    if !out.contains_key(header::CONTENT_TYPE) {
        out.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    out
}

/// 转发上游响应头:剔除静/动态 hop-by-hop 与 `set-cookie`;不剔除 `content-length`/`content-encoding`
pub(crate) fn forward_response_headers(upstream: &HeaderMap, dst: &mut HeaderMap) {
    let conn_names = connection_header_names(upstream);
    for (name, value) in upstream.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if is_static_hop_by_hop(&lower)
            || conn_names.iter().any(|c| c == &lower)
            || lower == "set-cookie"
        {
            continue;
        }
        dst.append(name.clone(), value.clone());
    }
}

/// 生产路径 `/v1/messages` 响应头等待上限
pub(crate) fn messages_header_wait(is_stream: bool) -> Duration {
    crate::upstream::header_wait(is_stream)
}

/// 生产路径 `/v1/messages/count_tokens` 响应头等待上限(对齐现有 count_tokens 的 SEND_TIMEOUT)
pub(crate) fn count_tokens_header_wait() -> Duration {
    crate::upstream::SEND_TIMEOUT
}

pub(crate) async fn relay_messages(
    snapshot: &ConfigSnapshot,
    route: &RouteDecision,
    inbound_headers: &HeaderMap,
    query: Option<&str>,
    body_bytes: Bytes,
    is_stream: bool,
) -> Result<Response, AppError> {
    relay_official_with_timeout(
        snapshot,
        route,
        "/v1/messages",
        inbound_headers,
        query,
        body_bytes,
        messages_header_wait(is_stream),
        false,
    )
    .await
}

pub(crate) async fn relay_count_tokens(
    snapshot: &ConfigSnapshot,
    route: &RouteDecision,
    inbound_headers: &HeaderMap,
    query: Option<&str>,
    body_bytes: Bytes,
) -> Result<Response, AppError> {
    relay_official_with_timeout(
        snapshot,
        route,
        "/v1/messages/count_tokens",
        inbound_headers,
        query,
        body_bytes,
        count_tokens_header_wait(),
        true,
    )
    .await
}

#[expect(clippy::too_many_arguments)]
pub(crate) async fn relay_official_with_timeout(
    snapshot: &ConfigSnapshot,
    route: &RouteDecision,
    path: &str,
    inbound_headers: &HeaderMap,
    query: Option<&str>,
    body_bytes: Bytes,
    wait: Duration,
    log_timing: bool,
) -> Result<Response, AppError> {
    let outbound_headers = official_request_headers(inbound_headers);
    if !has_valid_authorization(&outbound_headers) {
        return Err(AppError::unauthorized(
            "官方路径需要 CC 订阅登录的 Authorization;不支持 x-api-key",
        ));
    }

    let provider = route.provider_config(&snapshot.providers).ok_or_else(|| {
        AppError::new(anyhow::anyhow!("官方 provider 未找到: {}", route.provider))
    })?;
    let base_url = provider.base_urls().first().ok_or_else(|| {
        AppError::new(anyhow::anyhow!(
            "官方 provider {} 未配置 base_url",
            route.provider
        ))
    })?;
    let proxy_key = snapshot
        .runtime
        .upstream
        .resolve_proxy(provider.proxy_url.as_deref())
        .map_err(|e| {
            AppError::with_status(
                StatusCode::BAD_GATEWAY,
                format!("官方上游代理配置错误: {e}"),
            )
        })?;
    let client = snapshot
        .runtime
        .upstream
        .official_client_for(&proxy_key)
        .map_err(|e| {
            AppError::with_status(
                StatusCode::BAD_GATEWAY,
                format!("构建官方上游客户端失败: {e}"),
            )
        })?;

    let endpoint = crate::upstream::append_inbound_query(path, query);
    let url = format!("{}{}", base_url.trim_end_matches('/'), endpoint);

    let mut built = client.post(&url).body(body_bytes).build().map_err(|e| {
        AppError::with_status(
            StatusCode::BAD_GATEWAY,
            format!("构造官方上游请求失败: {e}"),
        )
    })?;
    *built.headers_mut() = outbound_headers;

    let started = Instant::now();
    // 仅对等待响应头设上限;不设 Client::timeout、不做 stale/400 重试、不调用 drop_pooled_client
    let resp = match tokio::time::timeout(wait, client.execute(built)).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            return Err(AppError::with_status(
                StatusCode::BAD_GATEWAY,
                format!("官方上游连接失败: {e}"),
            ));
        }
        Err(_) => {
            return Err(AppError::with_status(
                StatusCode::REQUEST_TIMEOUT,
                format!("官方上游响应头超时({}s)", wait.as_secs()),
            ));
        }
    };

    let status = resp.status();
    let request_id = resp
        .headers()
        .get("request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    if log_timing {
        tracing::info!(
            provider = %route.provider,
            path,
            status = status.as_u16(),
            request_id,
            elapsed_ms = started.elapsed().as_millis(),
            "official count_tokens"
        );
    } else {
        tracing::info!(
            provider = %route.provider,
            status = status.as_u16(),
            request_id,
            "official messages response"
        );
    }

    let mut builder = Response::builder().status(status);
    if let Some(dst) = builder.headers_mut() {
        forward_response_headers(resp.headers(), dst);
    }
    // 原始字节流直通:不加 180s 空闲超时、不插 keepalive、不补 timeout_error/message_stop
    builder
        .body(Body::from_stream(resp.bytes_stream()))
        .map_err(|e| AppError::new(anyhow::anyhow!("构造官方透传响应失败: {e}")))
}

#[cfg(test)]
#[path = "official_tests.rs"]
mod tests;
