//! 双通道传输(2026-10-09 探针 D/E/G 实证):读 `AgentService/RunSSE`
//! (connect+json,h2 服务端流),写 `BidiService/BidiAppend`(unary JSON,
//! `data` 为 AgentClientMessage proto hex,`appendSeqno` 从 0 单调递增,
//! 首条必须是 runRequest)。两条独立 h2 连接,requestId 关联读写。
//! 适用于双向流被代理/防火墙掐断的环境;协议消息层与 BiDi Run 完全一致,
//! 仅读侧载荷是 JSON(见 raw_wire::json)。

use super::stream::{connect, send_bytes};
use anyhow::{anyhow, Context, Result};
use axum::http::{HeaderMap, Request, StatusCode};
use bytes::Bytes;
use ccextra_core::convert::cursor::proto::{ConnectFrame, DEFAULT_MAX_FRAME_SIZE};
use h2::{client, RecvStream};
use reqwest::Url;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{interval_at, timeout, Instant, MissedTickBehavior};

const RUN_SSE_PATH: &str = "/agent.v1.AgentService/RunSSE";
const BIDI_APPEND_PATH: &str = "/aiserver.v1.BidiService/BidiAppend";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const APPEND_TIMEOUT: Duration = Duration::from_secs(30);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// BidiAppend 写侧:h2 连接上的串行 unary POST(seqno 单调,首条 runRequest)
struct DualWriter {
    client: client::SendRequest<Bytes>,
    seqno: u64,
    url: Url,
    token: String,
    client_version: String,
    request_id: String,
}

impl DualWriter {
    async fn append(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() > DEFAULT_MAX_FRAME_SIZE {
            return Err(anyhow!("Cursor request frame exceeds limit"));
        }
        let body = json!({
            "requestId": { "requestId": self.request_id },
            "appendSeqno": self.seqno.to_string(),
            "data": hex::encode(payload),
        })
        .to_string()
        .into_bytes();
        let request = Request::builder()
            .method("POST")
            .uri(self.url.as_str())
            .header("content-type", "application/json")
            .header("connect-protocol-version", "1")
            .header("authorization", format!("Bearer {}", self.token))
            .header("x-ghost-mode", "true")
            .header("x-cursor-client-type", "cli")
            .header("x-cursor-client-version", &self.client_version)
            .header("x-request-id", &self.request_id)
            .body(())?;
        let (response, mut sender) = self.client.send_request(request, false)?;
        send_bytes(&mut sender, &body).await?;
        sender.send_data(Bytes::new(), true)?;
        let response = timeout(APPEND_TIMEOUT, response)
            .await
            .context("Cursor BidiAppend response timed out")??;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow!("Cursor BidiAppend failed: {status}"));
        }
        // BidiAppendResponse 是空确认;读完释放流
        let mut body = response.into_body();
        while let Some(chunk) = timeout(APPEND_TIMEOUT, body.data())
            .await
            .context("Cursor BidiAppend body timed out")?
        {
            let chunk = chunk?;
            let _ = body.flow_control().release_capacity(chunk.len());
        }
        self.seqno += 1;
        Ok(())
    }
}

pub struct DualChannel {
    status: StatusCode,
    headers: HeaderMap,
    recv: RecvStream,
    writer: Arc<Mutex<DualWriter>>,
    connection: JoinHandle<()>,
    writer_connection: JoinHandle<()>,
    heartbeat: JoinHandle<()>,
}

impl DualChannel {
    pub async fn open(
        base_url: &str,
        token: &str,
        client_version: &str,
        request_id: &str,
        proxy_url: Option<&str>,
        initial_message: &[u8],
    ) -> Result<Self> {
        let url = Url::parse(base_url).context("invalid Cursor base_url")?;
        // 读侧:RunSSE 服务端流,请求体是 Connect 信封包 JSON requestId
        let payload = json!({ "requestId": request_id }).to_string().into_bytes();
        let frame = ConnectFrame::encode(&payload, 0)?;
        let mut run_url = url.clone();
        run_url.set_path(&format!(
            "{}{RUN_SSE_PATH}",
            url.path().trim_end_matches('/')
        ));
        run_url.set_query(None);
        run_url.set_fragment(None);
        let request = Request::builder()
            .method("POST")
            .uri(run_url.as_str())
            .header("content-type", "application/connect+json")
            .header("connect-protocol-version", "1")
            .header("authorization", format!("Bearer {token}"))
            .header("x-ghost-mode", "true")
            .header("x-cursor-client-type", "cli")
            .header("x-cursor-client-version", client_version)
            .header("x-request-id", request_id)
            .body(())?;
        let transport = connect(&url, proxy_url).await?;
        let (mut client, connection) = timeout(CONNECT_TIMEOUT, client::handshake(transport))
            .await
            .context("Cursor H2 handshake timed out")??;
        let connection = tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!("Cursor RunSSE H2 connection closed: {error}");
            }
        });
        let (response, mut sender) = match client.send_request(request, false) {
            Ok(stream) => stream,
            Err(error) => {
                connection.abort();
                return Err(error.into());
            }
        };
        if let Err(error) = send_bytes(&mut sender, &frame).await {
            connection.abort();
            return Err(error);
        }
        sender.send_data(Bytes::new(), true)?;
        // 等响应头:服务端已受理读流,再发首条 append(runRequest)保证
        // BidiAppend 到达时 requestId 已注册(探针以固定延时保证,此处
        // 以响应头到达为确定性信号)
        let response = match timeout(READ_IDLE_TIMEOUT, response).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                connection.abort();
                return Err(error.into());
            }
            Err(error) => {
                connection.abort();
                return Err(anyhow!("Cursor RunSSE response headers timed out: {error}"));
            }
        };
        // 写侧:独立 h2 连接承载 BidiAppend;任一步失败须清理已建连接,
        // 避免读连接悬挂泄漏(读连接 abort,写连接在块内自行 abort)
        let mut append_url = url.clone();
        append_url.set_path(&format!(
            "{}{BIDI_APPEND_PATH}",
            url.path().trim_end_matches('/')
        ));
        append_url.set_query(None);
        append_url.set_fragment(None);
        let write_side = async {
            let write_transport = connect(&url, proxy_url).await?;
            let (write_client, writer_connection) =
                timeout(CONNECT_TIMEOUT, client::handshake(write_transport))
                    .await
                    .context("Cursor H2 handshake timed out")??;
            let writer_connection = tokio::spawn(async move {
                if let Err(error) = writer_connection.await {
                    tracing::debug!("Cursor BidiAppend H2 connection closed: {error}");
                }
            });
            let mut writer = DualWriter {
                client: write_client,
                seqno: 0,
                url: append_url,
                token: token.to_string(),
                client_version: client_version.to_string(),
                request_id: request_id.to_string(),
            };
            // 首条 append 必须是 runRequest(seqno 0)
            if let Err(error) = writer.append(initial_message).await {
                writer_connection.abort();
                return Err(error);
            }
            Ok((writer, writer_connection))
        };
        let (writer, writer_connection) = match write_side.await {
            Ok(pair) => pair,
            Err(error) => {
                connection.abort();
                return Err(error);
            }
        };
        let writer = Arc::new(Mutex::new(writer));
        let heartbeat_writer = writer.clone();
        let heartbeat = tokio::spawn(async move {
            let mut timer = interval_at(Instant::now() + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL);
            timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                timer.tick().await;
                // AgentClientMessage.client_heartbeat = field 7, empty submessage
                let mut guard = heartbeat_writer.lock().await;
                if guard.append(b"\x3a\x00").await.is_err() {
                    // 写通道断开:读流只能等 90s idle 超时,记日志便于排查
                    tracing::warn!("Cursor dual-channel heartbeat failed; write path closed");
                    break;
                }
            }
        });
        Ok(Self {
            status: response.status(),
            headers: response.headers().clone(),
            recv: response.into_body(),
            writer,
            connection,
            writer_connection,
            heartbeat,
        })
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub async fn send_message(&self, payload: &[u8]) -> Result<()> {
        self.writer.lock().await.append(payload).await
    }

    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>> {
        let chunk = timeout(READ_IDLE_TIMEOUT, self.recv.data())
            .await
            .context("Cursor response idle timeout")?;
        match chunk {
            Some(Ok(chunk)) => {
                self.recv.flow_control().release_capacity(chunk.len())?;
                Ok(Some(chunk))
            }
            Some(Err(error)) => Err(error.into()),
            None => Ok(None),
        }
    }
}

impl Drop for DualChannel {
    fn drop(&mut self) {
        self.heartbeat.abort();
        self.connection.abort();
        self.writer_connection.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use h2::server;
    use std::time::Duration;

    /// 读连接帧体(Connect 信封)
    async fn read_body(body: &mut h2::RecvStream) -> Bytes {
        let mut data = Bytes::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.unwrap();
            let _ = body.flow_control().release_capacity(chunk.len());
            data = [data, chunk].concat().into();
        }
        data
    }

    fn envelope(payload: &[u8]) -> Bytes {
        let mut out = vec![0u8];
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        Bytes::from(out)
    }

    #[tokio::test]
    async fn dual_channel_reads_json_frames_and_appends_proto_hex() {
        // 双通道两条连接连同一 base_url:单 listener 顺序收读连接与写连接。
        // h2 server 连接 future 需持续 poll 才处理 DATA 帧,故每条连接
        // 都挂 driver 循环 accept,请求经 channel 交给断言逻辑。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            // 连接 1:RunSSE 读流
            let (socket, _) = listener.accept().await.unwrap();
            let mut connection = server::handshake(socket).await.unwrap();
            let (request, mut response) = connection.accept().await.unwrap().unwrap();
            let _driver = tokio::spawn(async move {
                while let Some(result) = connection.accept().await {
                    result.unwrap();
                }
            });
            assert_eq!(request.uri().path(), "/agent.v1.AgentService/RunSSE");
            assert_eq!(
                request.headers()["content-type"],
                "application/connect+json"
            );
            assert_eq!(request.headers()["authorization"], "Bearer access");
            assert_eq!(request.headers()["x-request-id"], "request-1");
            let mut body = request.into_body();
            let payload = read_body(&mut body).await;
            // 请求体 = Connect 信封包 JSON requestId
            assert_eq!(payload, envelope(br#"{"requestId":"request-1"}"#));
            let mut sender = response
                .send_response(axum::http::Response::new(()), false)
                .unwrap();
            sender
                .send_data(
                    envelope(br#"{"interactionUpdate":{"textDelta":{"text":"hi"}}}"#),
                    false,
                )
                .unwrap();
            sender.send_data(envelope(b"{}"), true).unwrap();
            // driver 持续 poll 连接驱动响应帧写出,随客户端断开自然结束
            // 连接 2:BidiAppend 写流(同一 h2 连接承载多条 append)
            let (socket, _) = listener.accept().await.unwrap();
            let mut connection = server::handshake(socket).await.unwrap();
            let (tx, mut rx) = tokio::sync::mpsc::channel(2);
            let _driver = tokio::spawn(async move {
                while let Some(result) = connection.accept().await {
                    if tx.send(result.unwrap()).await.is_err() {
                        break;
                    }
                }
            });
            for seqno in 0..2 {
                let (request, mut response) = rx.recv().await.unwrap();
                assert_eq!(request.uri().path(), "/aiserver.v1.BidiService/BidiAppend");
                assert_eq!(request.headers()["content-type"], "application/json");
                let mut body = request.into_body();
                let payload = read_body(&mut body).await;
                let json: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                assert_eq!(json["requestId"]["requestId"], "request-1");
                assert_eq!(json["appendSeqno"], seqno.to_string());
                if seqno == 0 {
                    // 首条 append 必须是 runRequest 原始 proto hex
                    assert_eq!(json["data"], hex::encode(b"\x08\x01"));
                } else {
                    assert_eq!(json["data"], hex::encode(b"\x10\x01"));
                }
                let mut sender = response
                    .send_response(axum::http::Response::new(()), false)
                    .unwrap();
                sender.send_data(Bytes::new(), true).unwrap();
            }
        });
        let run = async {
            let mut dual =
                DualChannel::open(&url, "access", "cli-test", "request-1", None, b"\x08\x01")
                    .await
                    .unwrap();
            assert_eq!(dual.status(), StatusCode::OK);
            dual.send_message(b"\x10\x01").await.unwrap();
            // 读侧:Connect 信封 JSON 帧
            let chunk = dual.next_chunk().await.unwrap().unwrap();
            assert_eq!(
                chunk,
                envelope(br#"{"interactionUpdate":{"textDelta":{"text":"hi"}}}"#)
            );
            let chunk = dual.next_chunk().await.unwrap().unwrap();
            assert_eq!(chunk, envelope(b"{}"));
            assert_eq!(dual.next_chunk().await.unwrap(), None);
        };
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}
