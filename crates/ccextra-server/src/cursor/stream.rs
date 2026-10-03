use super::constants::RUN_PATH;
use anyhow::{anyhow, Context, Result};
use axum::http::{HeaderMap, Request, StatusCode};
use bytes::Bytes;
use ccextra_core::convert::cursor::proto::{ConnectFrame, DEFAULT_MAX_FRAME_SIZE};
use futures::future::poll_fn;
use h2::{client, RecvStream, SendStream};
use reqwest::Url;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{interval_at, timeout, Instant, MissedTickBehavior};
use tokio_rustls::{rustls, TlsConnector};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const FLOW_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const H2_DATA_SIZE: usize = 16 * 1024;

trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Transport for T {}
type BoxTransport = Box<dyn Transport>;

pub struct CursorStream {
    status: StatusCode,
    headers: HeaderMap,
    recv: RecvStream,
    sender: Arc<Mutex<SendStream<Bytes>>>,
    connection: JoinHandle<()>,
    heartbeat: JoinHandle<()>,
}

impl CursorStream {
    pub async fn open(
        base_url: &str,
        token: &str,
        client_version: &str,
        request_id: &str,
        proxy_url: Option<&str>,
        initial_message: &[u8],
    ) -> Result<Self> {
        let url = Url::parse(base_url).context("invalid Cursor base_url")?;
        if initial_message.len() > DEFAULT_MAX_FRAME_SIZE {
            return Err(anyhow!("Cursor request frame exceeds limit"));
        }
        let frame = ConnectFrame::encode(initial_message, 0)?;
        let mut run_url = url.clone();
        run_url.set_path(&format!("{}{RUN_PATH}", url.path().trim_end_matches('/')));
        run_url.set_query(None);
        run_url.set_fragment(None);
        let request = Request::builder()
            .method("POST")
            .uri(run_url.as_str())
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("te", "trailers")
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
                tracing::debug!("Cursor H2 connection closed: {error}");
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
        let sender = Arc::new(Mutex::new(sender));
        let heartbeat_sender = sender.clone();
        let heartbeat = tokio::spawn(async move {
            let mut timer = interval_at(Instant::now() + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL);
            timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                timer.tick().await;
                // AgentClientMessage.client_heartbeat = field 7, empty submessage.
                let frame = ConnectFrame::encode(b"\x3a\x00", 0).expect("heartbeat fits frame");
                if send_bytes(&mut *heartbeat_sender.lock().await, &frame)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let response = match timeout(READ_IDLE_TIMEOUT, response).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                heartbeat.abort();
                connection.abort();
                return Err(error.into());
            }
            Err(error) => {
                heartbeat.abort();
                connection.abort();
                return Err(error).context("Cursor H2 response headers timed out");
            }
        };
        Ok(Self {
            status: response.status(),
            headers: response.headers().clone(),
            recv: response.into_body(),
            sender,
            connection,
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
        if payload.len() > DEFAULT_MAX_FRAME_SIZE {
            return Err(anyhow!("Cursor request frame exceeds limit"));
        }
        let frame = ConnectFrame::encode(payload, 0)?;
        send_bytes(&mut *self.sender.lock().await, &frame).await
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

impl Drop for CursorStream {
    fn drop(&mut self) {
        self.heartbeat.abort();
        self.connection.abort();
    }
}

async fn send_bytes(stream: &mut SendStream<Bytes>, mut data: &[u8]) -> Result<()> {
    while !data.is_empty() {
        let wanted = data.len().min(H2_DATA_SIZE);
        stream.reserve_capacity(wanted);
        let capacity = timeout(FLOW_WAIT_TIMEOUT, poll_fn(|cx| stream.poll_capacity(cx)))
            .await
            .context("Cursor H2 flow-control timeout")?
            .ok_or_else(|| anyhow!("Cursor H2 send stream closed"))??;
        if capacity == 0 {
            continue;
        }
        let n = wanted.min(capacity);
        stream.send_data(Bytes::copy_from_slice(&data[..n]), false)?;
        data = &data[n..];
    }
    Ok(())
}

async fn connect(url: &Url, proxy_url: Option<&str>) -> Result<BoxTransport> {
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("Cursor base_url has no host"))?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow!("unsupported Cursor URL scheme"))?;
    let authority = host_port(host, port);
    let proxy = proxy_url.filter(|url| !url.is_empty() && *url != "direct");
    let mut transport: BoxTransport = if let Some(raw) = proxy {
        let proxy = Url::parse(raw).context("invalid Cursor proxy_url")?;
        let proxy_host = proxy
            .host_str()
            .ok_or_else(|| anyhow!("Cursor proxy has no host"))?;
        let proxy_port = proxy
            .port_or_known_default()
            .ok_or_else(|| anyhow!("unsupported Cursor proxy scheme"))?;
        if !matches!(proxy.scheme(), "http" | "https") {
            return Err(anyhow!("Cursor proxy requires HTTP CONNECT"));
        }
        let socket = tcp_connect(proxy_host, proxy_port).await?;
        let mut tunnel: BoxTransport = Box::new(socket);
        if proxy.scheme() == "https" {
            tunnel = tls(tunnel, proxy_host, false).await?;
        }
        let credentials = if proxy.username().is_empty() {
            None
        } else {
            use base64::Engine;
            let user = percent_encoding::percent_decode_str(proxy.username()).decode_utf8()?;
            let password = percent_encoding::percent_decode_str(proxy.password().unwrap_or(""))
                .decode_utf8()?;
            Some(base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}")))
        };
        let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
        if let Some(encoded) = credentials {
            request.push_str(&format!("Proxy-Authorization: Basic {encoded}\r\n"));
        }
        request.push_str("\r\n");
        timeout(CONNECT_TIMEOUT, async {
            tunnel.write_all(request.as_bytes()).await?;
            let mut response = Vec::new();
            loop {
                if response.len() >= 16 * 1024 {
                    return Err(anyhow!("Cursor proxy response headers too large"));
                }
                let mut byte = [0];
                tunnel.read_exact(&mut byte).await?;
                response.push(byte[0]);
                if response.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let status = String::from_utf8_lossy(&response);
            if !status.lines().next().is_some_and(|line| {
                line.starts_with("HTTP/1.1 200 ") || line.starts_with("HTTP/1.0 200 ")
            }) {
                return Err(anyhow!(
                    "Cursor proxy CONNECT refused: {}",
                    status.lines().next().unwrap_or("invalid response")
                ));
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("Cursor proxy CONNECT timed out")??;
        tunnel
    } else {
        Box::new(tcp_connect(host, port).await?)
    };
    match url.scheme() {
        "https" => transport = tls(transport, host, true).await?,
        "http" => {}
        _ => return Err(anyhow!("unsupported Cursor URL scheme")),
    }
    Ok(transport)
}

fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

async fn tcp_connect(host: &str, port: u16) -> Result<TcpStream> {
    let socket = timeout(CONNECT_TIMEOUT, TcpStream::connect(host_port(host, port)))
        .await
        .context("Cursor TCP connect timed out")??;
    socket.set_nodelay(true)?;
    Ok(socket)
}

async fn tls(socket: BoxTransport, host: &str, require_h2: bool) -> Result<BoxTransport> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for certificate in native.certs {
        roots.add(certificate)?;
    }
    if roots.is_empty() {
        return Err(anyhow!("Cursor TLS trust store is empty"));
    }
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    if require_h2 {
        config.alpn_protocols = vec![b"h2".to_vec()];
    }
    let name = rustls::pki_types::ServerName::try_from(host.to_owned())?;
    let stream = timeout(
        CONNECT_TIMEOUT,
        TlsConnector::from(Arc::new(config)).connect(name, socket),
    )
    .await
    .context("Cursor TLS handshake timed out")??;
    if require_h2 && stream.get_ref().1.alpn_protocol() != Some(b"h2".as_slice()) {
        return Err(anyhow!("Cursor TLS peer did not negotiate h2"));
    }
    Ok(Box::new(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::time::Duration;

    #[tokio::test]
    async fn run_request_and_inline_reply_keep_h2_request_open() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut connection = h2::server::handshake(socket).await.unwrap();
            let (request, mut response) = connection.accept().await.unwrap().unwrap();
            assert_eq!(request.uri().path(), "/agent.v1.AgentService/Run");
            assert_eq!(
                request.headers()["content-type"],
                "application/connect+proto"
            );
            assert_eq!(request.headers()["authorization"], "Bearer access");
            let mut body = request.into_body();
            let driver = tokio::spawn(async move {
                while let Some(result) = connection.accept().await {
                    result.unwrap();
                }
            });
            assert_eq!(
                body.data().await.unwrap().unwrap().as_ref(),
                b"\x00\x00\x00\x00\x02\x08\x01"
            );
            assert!(!body.is_end_stream());
            let mut sender = response
                .send_response(axum::http::Response::new(()), false)
                .unwrap();
            assert_eq!(
                body.data().await.unwrap().unwrap().as_ref(),
                b"\x00\x00\x00\x00\x02\x10\x01"
            );
            assert!(!body.is_end_stream());
            sender
                .send_data(Bytes::from_static(b"\x02\x00\x00\x00\x02{}"), true)
                .unwrap();
            let _ = done_rx.await;
            driver.abort();
        });
        let run = async {
            let mut stream =
                CursorStream::open(&url, "access", "cli-test", "request-1", None, b"\x08\x01")
                    .await
                    .unwrap();
            assert_eq!(stream.status(), axum::http::StatusCode::OK);
            stream.send_message(b"\x10\x01").await.unwrap();
            assert_eq!(
                stream.next_chunk().await.unwrap().unwrap().as_ref(),
                b"\x02\x00\x00\x00\x02{}"
            );
            done_tx.send(()).unwrap();
            server.await.unwrap();
        };
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .unwrap();
    }
}
