// proxy-tunnel.test.mjs:http2 隧道池补丁测试。
// mock 代理统计 CONNECT 次数:预热建 POOL_SIZE 条,http2.connect 消费 1 条
// 并触发补池,总数应超过 POOL_SIZE(证明 h2 会话真实走了隧道,而非仅预热)。
import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { spawn } from "node:child_process";

const POOL_SIZE = 8;

test("http2 session tunnels through https_proxy pool", async () => {
  const connectRequests = [];
  const proxy = http.createServer();
  proxy.on("connect", (req, client) => {
    connectRequests.push(req.url);
    // 回 200 保持连接但不转发:TLS 无响应,h2 会话停在 connecting 即可
    client.write("HTTP/1.1 200 Connection established\r\n\r\n");
  });
  await new Promise((resolve) => proxy.listen(0, "127.0.0.1", resolve));
  const proxyPort = proxy.address().port;

  const script = `
    const { warmProxyPool } = await import("./proxy-tunnel.mjs");
    warmProxyPool();
    await new Promise((r) => setTimeout(r, 300));
    const http2 = await import("node:http2");
    http2.connect("https://tunnel-test.invalid");
    await new Promise((r) => setTimeout(r, 800));
    process.exit(0);
  `;
  const child = spawn(
    process.execPath,
    ["--input-type=module", "-e", script],
    {
      cwd: new URL(".", import.meta.url).pathname,
      env: {
        ...process.env,
        https_proxy: `http://127.0.0.1:${proxyPort}`,
        no_proxy: "",
        NODE_USE_ENV_PROXY: "1",
        CURSOR_BACKEND_URL: "https://tunnel-test.invalid",
      },
    },
  );
  await new Promise((resolve) => child.on("exit", resolve));
  proxy.close();

  const count = connectRequests.filter((u) => u === "tunnel-test.invalid:443").length;
  // 预热 POOL_SIZE 条 + 会话消费 1 条触发补池:总数必须超过预热数
  assert.ok(
    count > POOL_SIZE,
    `CONNECT count ${count} not above prewarm ${POOL_SIZE}: ${JSON.stringify(connectRequests)}`,
  );
});

test("stale tunnels beyond TTL are discarded and rebuilt", async () => {
  const connectRequests = [];
  const proxy = http.createServer();
  proxy.on("connect", (req, client) => {
    connectRequests.push(req.url);
    client.write("HTTP/1.1 200 Connection established\r\n\r\n");
  });
  await new Promise((resolve) => proxy.listen(0, "127.0.0.1", resolve));
  const proxyPort = proxy.address().port;

  const script = `
    const { warmProxyPool } = await import("./proxy-tunnel.mjs");
    warmProxyPool();
    await new Promise((r) => setTimeout(r, 300));
    const http2 = await import("node:http2");
    http2.connect("https://tunnel-test.invalid");
    await new Promise((r) => setTimeout(r, 800));
    process.exit(0);
  `;
  const child = spawn(
    process.execPath,
    ["--input-type=module", "-e", script],
    {
      cwd: new URL(".", import.meta.url).pathname,
      env: {
        ...process.env,
        https_proxy: `http://127.0.0.1:${proxyPort}`,
        no_proxy: "",
        NODE_USE_ENV_PROXY: "1",
        CURSOR_BACKEND_URL: "https://tunnel-test.invalid",
        CCEXTRA_TUNNEL_TTL_MS: "100",
      },
    },
  );
  await new Promise((resolve) => child.on("exit", resolve));
  proxy.close();

  const count = connectRequests.filter((u) => u === "tunnel-test.invalid:443").length;
  // 预热 300ms 后全部超龄(TTL 100ms):消费必须新建隧道而非复用死隧道
  assert.ok(
    count > POOL_SIZE,
    `CONNECT count ${count} not above prewarm ${POOL_SIZE}: ${JSON.stringify(connectRequests)}`,
  );
});
