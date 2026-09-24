// 非流响应 body 的统一读取边界(大小 + idle);入站请求大小上限同处集中定义。
// SSE 流式路径不走这里(流 chunk idle 见 sse::STREAM_IDLE_TIMEOUT)。

use crate::http::error::AppError;
use axum::http::StatusCode;
use bytes::Bytes;
use futures::Stream;

/// 入站请求 body 上限(原 10 MiB 内联值,数值不变,仅集中定义)
pub(crate) const INBOUND_BODY_LIMIT: usize = 10 * 1024 * 1024;
/// 成功非流响应 body 上限:恰好上限可读,多 1 字节拒绝
pub(crate) const SUCCESS_BODY_LIMIT: usize = 16 * 1024 * 1024;
/// 上游错误 body 上限:超出停止读取并标记截断
pub(crate) const ERROR_BODY_LIMIT: usize = 256 * 1024;
/// body 读取 idle:与 SSE chunk idle 共用一份常量;每次非空数据后重置,空 chunk 不续期
pub(crate) const BODY_READ_IDLE: std::time::Duration = crate::sse::STREAM_IDLE_TIMEOUT;

/// 有界读取结果:truncated 表示超过上限,已停止读取并保留前 limit 字节
#[derive(Debug)]
pub(crate) struct BoundedBody {
    pub(crate) bytes: Bytes,
    pub(crate) truncated: bool,
}

#[derive(Debug)]
pub(crate) enum BodyReadError<E> {
    /// 距上次非空数据停顿超过 BODY_READ_IDLE
    Idle,
    /// 超过大小上限(成功路径语义:拒绝)
    OverLimit { limit: usize },
    /// 底层传输错误
    Transport(E),
}

impl<E: std::fmt::Display> std::fmt::Display for BodyReadError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "body 读取停顿超 {}s", BODY_READ_IDLE.as_secs()),
            Self::OverLimit { limit } => write!(f, "body 超过 {limit} 字节上限"),
            Self::Transport(e) => write!(f, "body 读取失败: {e}"),
        }
    }
}

impl<E: std::error::Error + Send + Sync + 'static> std::error::Error for BodyReadError<E> {}

/// 流式累积并在过程中限制大小与停顿,不能等 .bytes() 完成后才检查。
/// Content-Length 不作提前拒绝依据,按实际字节计数。
async fn read_stream_bounded<S, E>(stream: S, limit: usize) -> Result<BoundedBody, BodyReadError<E>>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    use futures::StreamExt;
    let mut stream = stream;
    let mut buf: Vec<u8> = Vec::new();
    let mut deadline = tokio::time::Instant::now() + BODY_READ_IDLE;
    loop {
        let now = tokio::time::Instant::now();
        if deadline <= now {
            return Err(BodyReadError::Idle);
        }
        match tokio::time::timeout(deadline - now, stream.next()).await {
            Err(_) => return Err(BodyReadError::Idle),
            Ok(None) => {
                return Ok(BoundedBody {
                    bytes: Bytes::from(buf),
                    truncated: false,
                })
            }
            Ok(Some(Err(e))) => return Err(BodyReadError::Transport(e)),
            Ok(Some(Ok(chunk))) => {
                if chunk.is_empty() {
                    continue; // 空 chunk 不续期
                }
                deadline = tokio::time::Instant::now() + BODY_READ_IDLE;
                // 复制前比较剩余容量:只复制允许的前缀,避免超限字节的分配与复制。
                // Vec 容量按摊销增长,上限约 2×limit;底层已收到的 chunk 内存
                // 不在本 helper 控制范围,不宣称进程内存有严格上限。
                let remaining = limit - buf.len();
                if chunk.len() > remaining {
                    buf.extend_from_slice(&chunk[..remaining]);
                    return Ok(BoundedBody {
                        bytes: Bytes::from(buf),
                        truncated: true,
                    });
                }
                buf.extend_from_slice(&chunk);
            }
        }
    }
}

async fn read_body_bounded(
    resp: reqwest::Response,
    limit: usize,
) -> Result<BoundedBody, BodyReadError<reqwest::Error>> {
    read_stream_bounded(resp.bytes_stream(), limit).await
}

/// 成功 body:恰好上限可读,多 1 字节拒绝(OverLimit)
pub(crate) async fn read_success_body(
    resp: reqwest::Response,
) -> Result<Bytes, BodyReadError<reqwest::Error>> {
    match read_body_bounded(resp, SUCCESS_BODY_LIMIT).await {
        Ok(body) if body.truncated => Err(BodyReadError::OverLimit {
            limit: SUCCESS_BODY_LIMIT,
        }),
        Ok(body) => Ok(body.bytes),
        Err(e) => Err(e),
    }
}

/// 错误 body:最多保留 256 KiB,超出停止读取并标记截断
pub(crate) async fn read_error_body(
    resp: reqwest::Response,
) -> Result<BoundedBody, BodyReadError<reqwest::Error>> {
    read_body_bounded(resp, ERROR_BODY_LIMIT).await
}

/// 按状态选上限:成功 16 MiB(截断即 OverLimit),错误 256 KiB(截断保留)。
/// OAuth/project/model 路径用;调用层保留各自 30s 总超时与错误策略。
pub(crate) async fn read_body_by_status(
    resp: reqwest::Response,
) -> Result<BoundedBody, BodyReadError<reqwest::Error>> {
    if resp.status().is_success() {
        read_success_body(resp).await.map(|bytes| BoundedBody {
            bytes,
            truncated: false,
        })
    } else {
        read_error_body(resp).await
    }
}

/// Messages/count_tokens 成功 body:超限 502、停顿 504(Anthropic api_error),
/// 传输错误维持 500(原 .bytes() 错误语义)
pub(crate) async fn read_success_body_or_anthropic(
    resp: reqwest::Response,
) -> Result<Bytes, AppError> {
    read_success_body(resp).await.map_err(|e| match e {
        BodyReadError::OverLimit { .. } => AppError::with_status(
            StatusCode::BAD_GATEWAY,
            format!("上游响应超过大小上限: {e}"),
        ),
        BodyReadError::Idle => AppError::with_status(
            StatusCode::GATEWAY_TIMEOUT,
            format!("上游响应读取停顿: {e}"),
        ),
        BodyReadError::Transport(e) => AppError::new(anyhow::anyhow!("读上游响应失败: {e}")),
    })
}

/// Messages/count_tokens 错误 body:读取失败保留已知上游状态(不重新获得
/// 重试预算)。返回 (bytes, truncated):截断标志必须传到调用层,截断前缀
/// 不得交给完整 JSON 解析或触发依赖其字段的恢复逻辑。
pub(crate) async fn read_error_body_or_anthropic(
    resp: reqwest::Response,
    status: StatusCode,
) -> Result<(Bytes, bool), AppError> {
    match read_error_body(resp).await {
        Ok(body) => {
            if body.truncated {
                tracing::warn!(
                    status = status.as_u16(),
                    "上游错误 body 超过 256 KiB,已截断"
                );
            }
            Ok((body.bytes, body.truncated))
        }
        Err(e) => Err(AppError::with_status(
            status,
            format!("读取上游错误响应失败: {e}"),
        )),
    }
}

/// OAuth 错误正文:256 KiB 上限,截断加标记;读取失败返回空(对齐原 unwrap_or_default)
pub(crate) async fn read_error_text(resp: reqwest::Response) -> String {
    match read_error_body(resp).await {
        Ok(body) => error_text_with_marker(&body.bytes, body.truncated),
        Err(_) => String::new(),
    }
}

/// 截断标记纯函数(便于单测)
pub(crate) fn error_text_with_marker(bytes: &[u8], truncated: bool) -> String {
    let mut text = String::from_utf8_lossy(bytes).trim().to_string();
    if truncated {
        text.push_str("…(超过 256 KiB 已截断)");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[derive(Debug)]
    struct TestErr;
    impl std::fmt::Display for TestErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("test transport error")
        }
    }
    impl std::error::Error for TestErr {}

    fn chunks(parts: Vec<Bytes>) -> impl Stream<Item = Result<Bytes, TestErr>> + Unpin {
        futures::stream::iter(parts.into_iter().map(Ok))
    }

    #[tokio::test]
    async fn exact_limit_readable_one_more_byte_truncated() {
        let limit = 8;
        // 恰好 8 字节:可读,不截断
        let body = read_stream_bounded(chunks(vec![Bytes::from_static(b"12345678")]), limit)
            .await
            .unwrap();
        assert!(!body.truncated);
        assert_eq!(body.bytes.len(), 8);
        // 多 1 字节:停止读取,保留前 limit 字节并标记截断
        let body = read_stream_bounded(
            chunks(vec![
                Bytes::from_static(b"12345678"),
                Bytes::from_static(b"9"),
            ]),
            limit,
        )
        .await
        .unwrap();
        assert!(body.truncated);
        assert_eq!(&body.bytes[..], b"12345678");
    }

    /// 大单 chunk 越过剩余容量:已有一部分累积,后续 chunk 大小远超剩余容量,
    /// 仅切取允许的前缀,剩余字节不拷贝,正确标记 truncated
    #[tokio::test]
    async fn large_single_chunk_exceeding_remaining_capacity_truncated() {
        let limit = 10;
        let initial = Bytes::from_static(b"1234");
        let large_overflow = Bytes::from(vec![b'x'; 1024 * 1024]); // 1 MiB chunk
        let body = read_stream_bounded(chunks(vec![initial, large_overflow]), limit)
            .await
            .unwrap();
        assert!(body.truncated);
        assert_eq!(body.bytes.len(), limit);
        assert_eq!(&body.bytes[..4], b"1234");
        assert_eq!(&body.bytes[4..], &[b'x'; 6]);
    }

    #[tokio::test]
    async fn eof_returns_buffered_bytes() {
        let body =
            read_stream_bounded(chunks(vec![Bytes::from_static(b"abc"), Bytes::new()]), 1024)
                .await
                .unwrap();
        assert!(!body.truncated);
        assert_eq!(&body.bytes[..], b"abc");
    }

    #[tokio::test]
    async fn transport_error_propagates() {
        let stream = futures::stream::iter(vec![Ok(Bytes::from_static(b"abc")), Err(TestErr)]);
        let err = read_stream_bounded(stream, 1024).await.unwrap_err();
        assert!(matches!(err, BodyReadError::Transport(_)));
    }

    /// 停顿:首 chunk 后挂起超过 idle 返回 Idle(paused time 下不耗真实时间)
    #[tokio::test(start_paused = true)]
    async fn stall_after_first_chunk_times_out() {
        let stream = futures::stream::once(async { Ok(Bytes::from_static(b"abc")) })
            .chain(futures::stream::pending())
            .boxed();
        let err: BodyReadError<TestErr> = read_stream_bounded(stream, 1024).await.unwrap_err();
        assert!(matches!(err, BodyReadError::Idle));
    }

    /// 空 chunk 不续期:100s 时收到空 chunk,deadline 仍从起点算(120s 而非 220s)
    #[tokio::test(start_paused = true)]
    async fn empty_chunk_does_not_renew_idle_deadline() {
        let stream = futures::stream::unfold((), |()| async {
            tokio::time::sleep(std::time::Duration::from_secs(100)).await;
            Some((Ok(Bytes::new()), ()))
        })
        .take(1)
        .chain(futures::stream::pending())
        .boxed();
        let start = tokio::time::Instant::now();
        let err: BodyReadError<TestErr> = read_stream_bounded(stream, 1024).await.unwrap_err();
        assert!(matches!(err, BodyReadError::Idle));
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(150),
            "空 chunk 不得续期(实际 {elapsed:?})"
        );
    }

    #[test]
    fn error_text_marker_only_when_truncated() {
        assert_eq!(error_text_with_marker(b"boom", false), "boom");
        let marked = error_text_with_marker(b"boom", true);
        assert!(marked.starts_with("boom"));
        assert!(marked.contains("已截断"));
    }

    #[tokio::test(start_paused = true)]
    async fn read_error_body_or_anthropic_idle_preserves_status_code() {
        use axum::response::IntoResponse;
        for status in [StatusCode::TOO_MANY_REQUESTS, StatusCode::UNAUTHORIZED] {
            let stream = chunks(vec![Bytes::from_static(b"partial error")])
                .chain(futures::stream::pending());
            let resp = crate::test_support::response(status, reqwest::Body::wrap_stream(stream));
            let err = read_error_body_or_anthropic(resp, status)
                .await
                .unwrap_err();
            assert!(err.err.to_string().contains("停顿"));
            assert_eq!(err.into_response().status(), status);
        }
    }
}
