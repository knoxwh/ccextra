// proxy-tunnel.mjs:http2 路径代理隧道补丁(池化)。
// NODE_USE_ENV_PROXY=1 覆盖 fetch 与 http(s).Agent;但 SDK 的 run 主流量走
// connect-node 的 http2 会话(node:http2 直连,无 Agent 概念)。SDK bundle 以
// ESM 命名空间导入 http2(属性快照),patch http2.connect 对其不可见;但
// http2 会话建连在 Node 内部经 tls.connect(port, host, options) 位置参数
// 调用(CJS 活引用),故在 tls.connect 层拦截。命中预建 CONNECT 隧道池则
// 把 TLS 建在隧道上;池空该次直连并异步补池(仅首次或耗尽时泄漏一次);
// 建池失败线性退避,池内死隧道自动剔除。
// 必须在 @cursor/sdk 首次 import 前加载(side-effect 模块)。
import net from "node:net";
import tls from "node:tls";

const proxyRaw = process.env.https_proxy || process.env.HTTPS_PROXY || "";
const noProxy = (process.env.no_proxy || process.env.NO_PROXY || "")
  .split(",")
  .map((s) => s.trim())
  .filter(Boolean);
// SDK 会为 privacy/server-config/run 等并发建多个 h2 传输,池需覆盖突发
const POOL_SIZE = 8;
// 隧道最大复用年龄:代理(clash 等)会静默丢弃空闲 CONNECT 隧道(无 FIN,
// 半开连接写缓冲成功但永无响应,SDK run 挂死到空闲超时),超龄隧道直接弃用
const TUNNEL_TTL_MS = Number(process.env.CCEXTRA_TUNNEL_TTL_MS || 60_000);

function shouldProxy(host) {
  if (!proxyRaw || !host) return false;
  return !noProxy.some((entry) => host === entry || host.endsWith(entry));
}

function proxyTarget() {
  const url = new URL(proxyRaw);
  const auth =
    url.username || url.password
      ? "Proxy-Authorization: Basic " +
        Buffer.from(
          `${decodeURIComponent(url.username)}:${decodeURIComponent(url.password)}`,
        ).toString("base64") + "\r\n"
      : "";
  return { host: url.hostname, port: Number(url.port) || 80, auth };
}

/** 建一条已完成 CONNECT 握手的隧道 socket */
function buildTunnel(host, port) {
  return new Promise((resolve, reject) => {
    const { host: pHost, port: pPort, auth } = proxyTarget();
    const sock = net.connect({ host: pHost, port: pPort });
    let header = "";
    const onReadable = () => {
      for (;;) {
        const chunk = sock.read();
        if (!chunk) return;
        header += chunk.toString("latin1");
        const idx = header.indexOf("\r\n\r\n");
        if (idx === -1) continue;
        cleanup();
        const status = header.slice(0, header.indexOf("\r\n"));
        if (!/ 200 /.test(status)) {
          fail(new Error(`proxy CONNECT failed: ${status}`));
          return;
        }
        const rest = header.slice(idx + 4);
        if (rest) sock.unshift(Buffer.from(rest, "latin1"));
        sock.pause();
        resolve(sock);
        return;
      }
    };
    const fail = (err) => {
      cleanup();
      sock.destroy();
      reject(err);
    };
    // 握手完成前代理断连(FIN/RST 均触发 close)必须 reject,否则 filling 永久卡死
    const onHandshakeClose = () => fail(new Error("proxy closed connection during CONNECT handshake"));
    function cleanup() {
      sock.off("readable", onReadable);
      sock.off("error", fail);
      sock.off("close", onHandshakeClose);
    }
    sock.once("error", fail);
    sock.once("close", onHandshakeClose);
    sock.once("connect", () => {
      sock.write(`CONNECT ${host}:${port} HTTP/1.1\r\nHost: ${host}:${port}\r\n${auth}\r\n`);
      sock.on("readable", onReadable);
    });
  });
}

// 隧道池:host:port → 就绪 socket 队列 + 补池任务 + 连续失败计数
const pool = new Map();
// 建池失败退避基数:连续失败线性放大,封顶 30s,避免代理不可达时热循环
const RETRY_DELAY_MS = 1000;

function refill(host, port) {
  const key = `${host}:${port}`;
  let entry = pool.get(key);
  if (!entry) {
    entry = { ready: [], filling: false, failures: 0 };
    pool.set(key, entry);
  }
  if (entry.filling || entry.ready.length >= POOL_SIZE) return;
  entry.filling = true;
  buildTunnel(host, port)
    .then((sock) => {
      entry.ready.push({ sock, born: Date.now() });
      entry.failures = 0;
      // 池内隧道被代理空闲超时关闭时移出队列,避免取到死隧道
      sock.once("close", () => {
        const index = entry.ready.findIndex((item) => item.sock === sock);
        if (index !== -1) entry.ready.splice(index, 1);
      });
    })
    .catch(() => {
      entry.failures += 1;
    })
    .finally(() => {
      entry.filling = false;
      if (entry.ready.length < POOL_SIZE) {
        const delay = entry.failures === 0 ? 0 : Math.min(RETRY_DELAY_MS * entry.failures, 30_000);
        if (delay === 0) refill(host, port);
        else setTimeout(() => refill(host, port), delay).unref();
      }
    });
}

function takeTunnel(host, port) {
  const entry = pool.get(`${host}:${port}`);
  // 跳过已被对端关闭或超龄的隧道(close 事件可能尚未派发)
  let sock = null;
  let item;
  while ((item = entry?.ready.shift())) {
    const candidate = item.sock;
    if (candidate.destroyed || candidate.readableEnded) continue;
    if (Date.now() - item.born > TUNNEL_TTL_MS) {
      candidate.destroy();
      continue;
    }
    sock = candidate;
    break;
  }
  refill(host, port);
  return sock;
}

// 预热默认后端(SDK 未设 CURSOR_BACKEND_URL 时为 api2.cursor.sh)
export function warmProxyPool() {
  const backend = process.env.CURSOR_BACKEND_URL || "https://api2.cursor.sh";
  const url = new URL(backend);
  if (shouldProxy(url.hostname)) refill(url.hostname, url.port || 443);
}

const origTlsConnect = tls.connect;
tls.connect = function patchedTlsConnect(...args) {
  try {
    const [port, host, options] = args;
    // 只拦 http2 会话建连:位置参数 + ALPN h2;https.Agent 走对象形式或
    // 自带 proxyEnv(Node 24 原生代理),均不匹配
    if (
      typeof host === "string" &&
      (typeof port === "number" || typeof port === "string") &&
      options &&
      typeof options === "object" &&
      Array.isArray(options.ALPNProtocols) &&
      options.ALPNProtocols.includes("h2") &&
      !options.createConnection &&
      !options.proxyEnv &&
      shouldProxy(host)
    ) {
      const numericPort = Number(port) || 443;
      const tunnel = takeTunnel(host, numericPort);
      console.error(`[proxy-tunnel] h2 ${host}:${numericPort} ${tunnel ? "pool-hit" : "pool-miss"}`);
      if (tunnel) {
        const { port: _p, host: _h, ...rest } = options;
        return origTlsConnect.call(tls, {
          ...rest,
          servername: options.servername ?? host,
          socket: tunnel,
        });
      }
      refill(host, numericPort);
    }
  } catch {
    // 解析失败按原样直连
  }
  return origTlsConnect.apply(tls, args);
};
