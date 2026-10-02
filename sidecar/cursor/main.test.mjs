import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawn } from "node:child_process";
import { createServer } from "./main.mjs";
import { loadModelCache, saveModelCache, discoverModels } from "./models.mjs";

const sessionRegistry = {
  health: () => ({ agents: 0, pendingCallbacks: 0 }),
  run: async () => { throw new Error("not used"); }
};

async function withServer(handler, run) {
  const authDir = await mkdtemp(join(tmpdir(), "ccextra-cursor-"));
  const server = createServer({ token: "secret", authDir, sessionRegistry, modelCatalog: handler?.modelCatalog });
  try {
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    await run(`http://127.0.0.1:${server.address().port}`, authDir);
  } finally {
    await new Promise((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())));
    await rm(authDir, { recursive: true, force: true });
  }
}

test("requests without sidecar token return 401", async () => {
  await withServer(null, async (base) => {
    const response = await fetch(`${base}/health`);
    assert.equal(response.status, 401);
  });
});

test("requests with wrong token return 401", async () => {
  await withServer(null, async (base) => {
    const response = await fetch(`${base}/health`, { headers: { authorization: "Bearer nope" } });
    assert.equal(response.status, 401);
  });
});

test("health returns ok, sdkVersion and registry counters", async () => {
  await withServer(null, async (base) => {
    const response = await fetch(`${base}/health`, { headers: { authorization: "Bearer secret" } });
    assert.equal(response.status, 200);
    const body = await response.json();
    assert.equal(body.ok, true);
    assert.equal(body.sdkVersion, "1.0.34");
    assert.equal(body.agents, 0);
    assert.equal(body.pendingCallbacks, 0);
  });
});

test("unknown route returns 404", async () => {
  await withServer(null, async (base) => {
    const response = await fetch(`${base}/nope`, { headers: { authorization: "Bearer secret" } });
    assert.equal(response.status, 404);
  });
});

test("run with invalid JSON returns 400", async () => {
  await withServer(null, async (base) => {
    const response = await fetch(`${base}/run`, {
      method: "POST",
      headers: { authorization: "Bearer secret", "content-type": "application/json" },
      body: "{not json",
    });
    assert.equal(response.status, 400);
  });
});

test("run rejects invalid image before touching session registry", async () => {
  let calls = 0;
  const registry = {
    health: () => ({ agents: 0, pendingCallbacks: 0 }),
    run: async () => { calls += 1; throw new Error("must not run"); },
  };
  const authDir = await mkdtemp(join(tmpdir(), "ccextra-cursor-image-"));
  const server = createServer({ token: "secret", authDir, sessionRegistry: registry, modelCatalog: null });
  try {
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    const response = await fetch(`http://127.0.0.1:${server.address().port}/run`, {
      method: "POST",
      headers: { authorization: "Bearer secret", "content-type": "application/json" },
      body: JSON.stringify({
        apiKey: "key",
        model: "auto",
        workspaceDir: process.cwd(),
        messages: [{ role: "user", content: [{ type: "image", source: { type: "base64", media_type: "image/png", data: "bad!" } }] }],
      }),
    });
    assert.equal(response.status, 400);
    const body = await response.json();
    assert.equal(body.error.type, "invalid_request_error");
    assert.equal(calls, 0);
  } finally {
    await new Promise((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())));
    await rm(authDir, { recursive: true, force: true });
  }
});

test("run streams normalized SSE events", async () => {
  const registry = {
    health: () => ({ agents: 1, pendingCallbacks: 0 }),
    run: async (request, subscriber) => {
      assert.equal(request.model, "auto");
      assert.equal(request.workspaceDir, process.cwd());
      subscriber.emit({ type: "text_delta", text: "hi" });
      subscriber.emit({ type: "turn_end", stop_reason: "end_turn" });
      subscriber.close();
      return { runId: "r1", abort: async () => {} };
    },
  };
  const authDir = await mkdtemp(join(tmpdir(), "ccextra-cursor-"));
  const server = createServer({ token: "secret", authDir, sessionRegistry: registry, modelCatalog: null });
  try {
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    const response = await fetch(`http://127.0.0.1:${server.address().port}/run`, {
      method: "POST",
      headers: { authorization: "Bearer secret", "content-type": "application/json" },
      body: JSON.stringify({
        apiKey: "key",
        model: "auto",
        systemPrompt: "s",
        workspaceDir: process.cwd(),
        messages: [{ role: "user", content: "hi" }],
        tools: [],
      }),
    });
    assert.equal(response.status, 200);
    assert.match(response.headers.get("content-type"), /text\/event-stream/);
    const text = await response.text();
    assert.match(text, /data: \{"type":"text_delta","text":"hi"\}/);
    assert.match(text, /data: \{"type":"turn_end","stop_reason":"end_turn"\}/);
  } finally {
    await new Promise((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())));
    await rm(authDir, { recursive: true, force: true });
  }
});

test("run busy maps to 503 with Retry-After", async () => {
  const registry = {
    health: () => ({ agents: 1, pendingCallbacks: 1 }),
    run: async () => {
      const error = new Error("cursor_sdk_session_busy");
      error.statusCode = 503;
      error.retryAfter = 1;
      throw error;
    },
  };
  const authDir = await mkdtemp(join(tmpdir(), "ccextra-cursor-"));
  const server = createServer({ token: "secret", authDir, sessionRegistry: registry, modelCatalog: null });
  try {
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    const response = await fetch(`http://127.0.0.1:${server.address().port}/run`, {
      method: "POST",
      headers: { authorization: "Bearer secret", "content-type": "application/json" },
      body: JSON.stringify({ apiKey: "k", model: "auto", workspaceDir: process.cwd(), messages: [], tools: [] }),
    });
    assert.equal(response.status, 503);
    assert.equal(response.headers.get("retry-after"), "1");
    const body = await response.json();
    assert.equal(body.type, "error");
  } finally {
    await new Promise((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())));
    await rm(authDir, { recursive: true, force: true });
  }
});

test("run first-frame error returns HTTP error without committing 200", async () => {
  const registry = {
    health: () => ({ agents: 0, pendingCallbacks: 0 }),
    run: async (request, subscriber) => {
      subscriber.emit({ type: "error", code: "cursor_sdk_run_error", message: "boom", retryable: false });
      subscriber.close();
      return { runId: "r1", abort: async () => {} };
    },
  };
  const authDir = await mkdtemp(join(tmpdir(), "ccextra-cursor-"));
  const server = createServer({ token: "secret", authDir, sessionRegistry: registry, modelCatalog: null });
  try {
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    const response = await fetch(`http://127.0.0.1:${server.address().port}/run`, {
      method: "POST",
      headers: { authorization: "Bearer secret", "content-type": "application/json" },
      body: JSON.stringify({ apiKey: "k", model: "auto", workspaceDir: process.cwd(), messages: [], tools: [] }),
    });
    assert.equal(response.status, 502);
    const body = await response.json();
    assert.equal(body.type, "error");
    assert.equal(body.error.type, "cursor_sdk_run_error");
  } finally {
    await new Promise((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())));
    await rm(authDir, { recursive: true, force: true });
  }
});

test("models endpoint returns catalog from apiKey", async () => {
  const modelCatalog = async (apiKey) => {
    assert.equal(apiKey, "key");
    return { models: [{ id: "auto" }, { id: "composer-2.5" }] };
  };
  await withServer({ modelCatalog }, async (base) => {
    const response = await fetch(`${base}/models`, {
      method: "POST",
      headers: { authorization: "Bearer secret", "content-type": "application/json" },
      body: JSON.stringify({ apiKey: "key" }),
    });
    assert.equal(response.status, 200);
    const body = await response.json();
    assert.deepEqual(body.models, [{ id: "auto" }, { id: "composer-2.5" }]);
  });
});

test("models endpoint without apiKey returns 400", async () => {
  await withServer({ modelCatalog: async () => ({ models: [] }) }, async (base) => {
    const response = await fetch(`${base}/models`, {
      method: "POST",
      headers: { authorization: "Bearer secret", "content-type": "application/json" },
      body: JSON.stringify({}),
    });
    assert.equal(response.status, 400);
  });
});

test("process entry emits a single READY frame on stdout", async () => {
  const authDir = await mkdtemp(join(tmpdir(), "ccextra-cursor-ready-"));
  const child = spawn(process.execPath, [join(import.meta.dirname, "main.mjs")], {
    env: {
      ...process.env,
      CCEXTRA_CURSOR_TOKEN: "ready-token",
      CCEXTRA_CURSOR_AUTH_DIR: authDir,
      CCEXTRA_CURSOR_PORT: "0",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  try {
    const firstLine = await new Promise((resolve, reject) => {
      let buffer = "";
      const timer = setTimeout(() => reject(new Error("READY 超时")), 10_000);
      child.stdout.setEncoding("utf8");
      child.stdout.on("data", (chunk) => {
        buffer += chunk;
        const newline = buffer.indexOf("\n");
        if (newline >= 0) {
          clearTimeout(timer);
          resolve(buffer.slice(0, newline));
        }
      });
      child.on("exit", (code) => reject(new Error(`进程提前退出: ${code}`)));
    });
    const frame = JSON.parse(firstLine);
    assert.equal(frame.event, "ready");
    assert.ok(frame.port > 0);
    assert.notEqual(frame.port, 8223);
    // READY 后 /health 可用且要求 token
    const response = await fetch(`http://127.0.0.1:${frame.port}/health`, {
      headers: { authorization: "Bearer ready-token" },
    });
    assert.equal(response.status, 200);
  } finally {
    if (child.exitCode === null && child.signalCode === null) {
      const exited = new Promise((resolve) => child.once("exit", resolve));
      child.kill("SIGTERM");
      await exited;
    }
    await rm(authDir, { recursive: true, force: true });
  }
});

test("startup compacts journal to 32 sessions before READY", async () => {
  const authDir = await mkdtemp(join(tmpdir(), "ccextra-cursor-compact-"));
  // 预写 40 个 session:READY 前应 compact 到最近 32 个
  const lines = [];
  for (let i = 0; i < 40; i += 1) {
    lines.push(JSON.stringify({ sessionKey: `s-${i}`, agentId: `a-${i}`, state: "clean" }));
  }
  await (await import("node:fs/promises")).writeFile(
    join(authDir, "sessions.jsonl"),
    `${lines.join("\n")}\n`,
  );
  const child = spawn(process.execPath, [join(import.meta.dirname, "main.mjs")], {
    env: {
      ...process.env,
      CCEXTRA_CURSOR_TOKEN: "compact-token",
      CCEXTRA_CURSOR_AUTH_DIR: authDir,
      CCEXTRA_CURSOR_PORT: "0",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  try {
    const firstLine = await new Promise((resolve, reject) => {
      let buffer = "";
      const timer = setTimeout(() => reject(new Error("READY 超时")), 10_000);
      child.stdout.setEncoding("utf8");
      child.stdout.on("data", (chunk) => {
        buffer += chunk;
        const newline = buffer.indexOf("\n");
        if (newline >= 0) {
          clearTimeout(timer);
          resolve(buffer.slice(0, newline));
        }
      });
      child.on("exit", (code) => reject(new Error(`进程提前退出: ${code}`)));
    });
    assert.equal(JSON.parse(firstLine).event, "ready");
    // READY 已发出:compact 必须在 HTTP 服务就绪前完成
    const text = await (await import("node:fs/promises")).readFile(
      join(authDir, "sessions.jsonl"),
      "utf8",
    );
    const remaining = text.trim().split("\n");
    assert.equal(remaining.length, 32);
    // 保留最近 32 个(尾部 session)
    const keys = remaining.map((line) => JSON.parse(line).sessionKey);
    assert.equal(keys[0], "s-8");
    assert.equal(keys[31], "s-39");
  } finally {
    if (child.exitCode === null && child.signalCode === null) {
      const exited = new Promise((resolve) => child.once("exit", resolve));
      child.kill("SIGTERM");
      await exited;
    }
    await rm(authDir, { recursive: true, force: true });
  }
});

import { SessionRegistry } from "./sessions.mjs";

/** 受控 Run:测试手动推进事件流。 */
function controlledRun() {
  const queue = [];
  let ended = false;
  let notify = () => {};
  let waitResolve;
  const waitPromise = new Promise((resolve) => { waitResolve = resolve; });
  const raw = {
    id: "run-raw",
    async *stream() {
      while (true) {
        while (queue.length > 0) yield queue.shift();
        if (ended) return;
        await new Promise((resolve) => { notify = resolve; });
      }
    },
    wait: () => waitPromise,
    cancel: () => {},
  };
  return {
    raw,
    push: (event) => { queue.push(event); notify(); },
    end: () => { ended = true; notify(); },
    finish: (result) => waitResolve(result),
  };
}

/** fake SDK:记录 send 调用,返回受控 Run。 */
function fakeSdk() {
  const state = { sends: [] };
  let agentSeq = 0;
  const makeAgent = () => {
    agentSeq += 1;
    const agent = {
      agentId: `agent-${agentSeq}`,
      send: async (text, options) => {
        const run = controlledRun();
        state.sends.push({ text, options, run });
        return run.raw;
      },
    };
    return agent;
  };
  return {
    state,
    async createAgent() { return makeAgent(); },
    async resumeAgent() { return makeAgent(); },
    async sendRun(agent, { text, modelId, modelParams, customTools, force, onDelta, runId, idempotencyKey }) {
      const raw = await agent.send(text, {
        model: { id: modelId, params: modelParams },
        mode: "agent",
        local: { customTools, force },
        onDelta,
        idempotencyKey,
      });
      return { runId, stream: raw.stream(), wait: () => raw.wait(), cancel: () => raw.cancel() };
    },
  };
}

const settle = (ms = 10) => new Promise((resolve) => setTimeout(resolve, ms));

/** 轮询等待条件成立(测试推进受控 Run 用)。 */
async function waitFor(predicate, ms = 5000) {
  const deadline = Date.now() + ms;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await settle(5);
  }
  throw new Error("waitFor timeout");
}

/** 搭真实 registry HTTP server,返回 fetch 封装。 */
async function withRegistryServer(run) {
  const sdk = fakeSdk();
  const journal = { append: async () => {}, replay: async () => [], compact: async () => {} };
  const registry = new SessionRegistry({ journal, sdk, config: { toolBatchGraceMs: 5 } });
  const authDir = await mkdtemp(join(tmpdir(), "ccextra-cursor-"));
  const server = createServer({ token: "secret", authDir, sessionRegistry: registry, modelCatalog: null });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const base = `http://127.0.0.1:${server.address().port}`;
  const post = (body) => fetch(`${base}/run`, {
    method: "POST",
    headers: { authorization: "Bearer secret", "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  try {
    await run({ sdk, registry, post });
  } finally {
    await new Promise((resolve, reject) => server.close((error) => (error ? reject(error) : resolve())));
    await rm(authDir, { recursive: true, force: true });
  }
}

const runRequest = (messages, tools) => ({
  apiKey: "key",
  model: "auto",
  modelParams: [],
  systemPrompt: "s",
  workspaceDir: process.cwd(),
  messages,
  tools,
});

const readTool = () => [{ name: "Read", description: "read", input_schema: { type: "object" } }];

test("tool_result with unknown id on active run returns HTTP 400", async () => {
  await withRegistryServer(async ({ sdk, post }) => {
    // park call-1:session 进入 awaiting_tool_results
    const firstPromise = post(runRequest([{ role: "user", content: "read a" }], readTool()));
    await waitFor(() => sdk.state.sends.length === 1);
    sdk.state.sends[0].options.local.customTools.Read.execute({ path: "a" }, { toolCallId: "call-1" });
    const first = await firstPromise;
    assert.equal(first.status, 200);
    await first.text();
    await settle(20);

    // 续接请求带未知 id:active Run 的 batch.resolve 报 unmatched → HTTP 400
    const response = await post(runRequest([
      { role: "user", content: "read a" },
      { role: "assistant", content: [{ type: "tool_use", id: "call-unknown", name: "Read", input: { path: "a" } }] },
      { role: "user", content: [{ type: "tool_result", tool_use_id: "call-unknown", content: "ok" }] },
    ], readTool()));
    assert.equal(response.status, 400);
    const body = await response.json();
    assert.equal(body.type, "error");
    assert.match(body.error.message, /cursor_sdk_tool_result_unmatched/);
  });
});

test("tool_result with settled id from an old batch returns HTTP 400", async () => {
  await withRegistryServer(async ({ sdk, post }) => {
    // 第一轮:park call-1 并正常 resolve,Run 完成
    const firstPromise = post(runRequest([{ role: "user", content: "read a" }], readTool()));
    await waitFor(() => sdk.state.sends.length === 1);
    sdk.state.sends[0].options.local.customTools.Read.execute({ path: "a" }, { toolCallId: "call-1" });
    const first = await firstPromise;
    assert.equal(first.status, 200);
    await first.text();
    await settle(20);

    // resolve call-1:续接当前 Run(不新建 send),推进到终态
    const secondPromise = post(runRequest([
      { role: "user", content: "read a" },
      { role: "assistant", content: [{ type: "tool_use", id: "call-1", name: "Read", input: { path: "a" } }] },
      { role: "user", content: [{ type: "tool_result", tool_use_id: "call-1", content: "ok" }] },
    ], readTool()));
    await settle(20);
    sdk.state.sends[0].run.push({ type: "assistant", message: { content: [{ type: "text", text: "done" }] } });
    sdk.state.sends[0].run.end();
    sdk.state.sends[0].run.finish({ status: "finished" });
    const second = await secondPromise;
    assert.equal(second.status, 200);
    await second.text();
    await settle(20);

    // 第二轮:park call-2 后,续接带旧 batch 的 call-1 → unmatched 400
    const thirdPromise = post(runRequest([
      { role: "user", content: "read a" },
      { role: "assistant", content: [{ type: "tool_use", id: "call-1", name: "Read", input: { path: "a" } }] },
      { role: "user", content: [{ type: "tool_result", tool_use_id: "call-1", content: "ok" }] },
      { role: "assistant", content: [{ type: "text", text: "done" }] },
      { role: "user", content: "read b" },
    ], readTool()));
    await waitFor(() => sdk.state.sends.length === 2);
    sdk.state.sends[1].options.local.customTools.Read.execute({ path: "b" }, { toolCallId: "call-2" });
    const third = await thirdPromise;
    assert.equal(third.status, 200);
    await third.text();
    await settle(20);

    const response = await post(runRequest([
      { role: "user", content: "read a" },
      { role: "assistant", content: [{ type: "tool_use", id: "call-1", name: "Read", input: { path: "a" } }] },
      { role: "user", content: [{ type: "tool_result", tool_use_id: "call-1", content: "ok" }] },
      { role: "assistant", content: [{ type: "text", text: "done" }] },
      { role: "user", content: "read b" },
      { role: "assistant", content: [{ type: "tool_use", id: "call-2", name: "Read", input: { path: "b" } }] },
      { role: "user", content: [{ type: "tool_result", tool_use_id: "call-1", content: "stale" }] },
    ], readTool()));
    assert.equal(response.status, 400);
    const body = await response.json();
    assert.match(body.error.message, /cursor_sdk_tool_result_unmatched/);
  });
});
