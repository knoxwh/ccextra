//! 每请求一行 INFO 访问日志:只记路由元数据、状态与耗时,不含 header / body / key。
//! 流式成功响应在 body 流结束或客户端断开(drop)时记总耗时;其余在 handler 返回时记。

use crate::http::handlers::messages::PreparedMessageRequest;
use axum::body::Body;
use axum::response::Response;
use ccextra_core::route::Protocol;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Instant;

/// 路由确定后的日志字段(全部来自路由决策,不触碰凭证与请求内容)
#[derive(Debug, Clone)]
pub(crate) struct AccessFields {
    model: Arc<str>,
    provider: String,
    protocol: Protocol,
    upstream_model: String,
    stream: bool,
}

impl AccessFields {
    pub(crate) fn from_prepared(prepared: &PreparedMessageRequest) -> Self {
        Self {
            model: prepared.inbound_model.clone(),
            provider: prepared.route.provider.clone(),
            protocol: prepared.route.protocol,
            upstream_model: prepared.route.upstream_model.clone(),
            stream: prepared.is_stream,
        }
    }
}

/// 单行内容;`fields` 为 None 表示路由前即失败(认证、读 body、解析等)。
/// 首字节耗时 = handler 返回(流式为首帧预读后提交 200)的时间。
pub(crate) fn access_line(
    fields: Option<&AccessFields>,
    status: u16,
    first_byte_ms: u128,
    total_ms: u128,
) -> String {
    let route = fields.map_or_else(String::new, |f| {
        format!(
            "model={} provider={} protocol={:?} upstream_model={} stream={} ",
            f.model, f.provider, f.protocol, f.upstream_model, f.stream
        )
    });
    format!("access {route}status={status} first_byte_ms={first_byte_ms} total_ms={total_ms}")
}

/// 记录成功返回的响应;流式 2xx 包装 body,在流结束/断开时补记总耗时。
pub(crate) fn finish(resp: Response, fields: Option<AccessFields>, started: Instant) -> Response {
    let status = resp.status().as_u16();
    let first_byte_ms = started.elapsed().as_millis();
    let streaming = resp.status().is_success() && fields.as_ref().is_some_and(|f| f.stream);
    if !streaming {
        tracing::info!(
            "{}",
            access_line(fields.as_ref(), status, first_byte_ms, first_byte_ms)
        );
        return resp;
    }
    let guard = StreamGuard {
        fields,
        status,
        first_byte_ms,
        started,
    };
    let (parts, body) = resp.into_parts();
    let body = Body::from_stream(body.into_data_stream().map(move |chunk| {
        let _keep = &guard;
        chunk
    }));
    Response::from_parts(parts, body)
}

/// handler 返回错误时记录(status 为最终返回给客户端的状态码)。
pub(crate) fn log_error(fields: Option<&AccessFields>, status: u16, started: Instant) {
    let elapsed = started.elapsed().as_millis();
    tracing::info!("{}", access_line(fields, status, elapsed, elapsed));
}

struct StreamGuard {
    fields: Option<AccessFields>,
    status: u16,
    first_byte_ms: u128,
    started: Instant,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        tracing::info!(
            "{}",
            access_line(
                self.fields.as_ref(),
                self.status,
                self.first_byte_ms,
                self.started.elapsed().as_millis(),
            )
        );
    }
}
