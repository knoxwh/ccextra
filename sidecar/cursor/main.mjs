// main.mjs:sidecar 入口。HTTP server + Bearer 鉴权 + 请求分派。
// stdout 只允许一行 READY 帧;日志全部 stderr。
import "./proxy-tunnel.mjs";
import http from "node:http";
import { join } from "node:path";
import { isAbsolute } from "node:path";
import { Journal } from "./journal.mjs";
import { SessionRegistry } from "./sessions.mjs";
import { normalizeWorkspaceDir } from "./sessions.mjs";
import { validateMessageImages } from "./tools.mjs";
import { createSdkAdapter, loadSdk } from "./sdk.mjs";
import { discoverModels, loadModelCache, saveModelCache } from "./models.mjs";

const SDK_VERSION = "1.0.34";

/** Anthropic error JSON 形状。 */
function errorBody(type, message) {
  return JSON.stringify({ type: "error", error: { type, message } });
}

/** 读取请求 JSON body;解析失败抛 400。 */
async function readJsonBody(req, limit = 64 * 1024 * 1024) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > limit) throw Object.assign(new Error("request body too large"), { statusCode: 400 });
    chunks.push(chunk);
  }
  try {
    return JSON.parse(Buffer.concat(chunks).toString("utf8"));
  } catch {
    throw Object.assign(new Error("invalid JSON body"), { statusCode: 400 });
  }
}

function respondJson(res, status, body, extraHeaders = {}) {
  res.writeHead(status, { "content-type": "application/json", ...extraHeaders });
  res.end(body);
}

/**
 * createServer:sidecar HTTP server。
 * @param token Bearer 鉴权 token
 * @param authDir 绝对路径;journal 与模型缓存固定位于该目录
 * @param sessionRegistry SessionRegistry(测试注入 fake)
 * @param modelCatalog async (apiKey) => { models: [{ id }] }
 */
export function createServer({ token, authDir, sessionRegistry, modelCatalog, sdkVersion = SDK_VERSION }) {
  if (!token || typeof token !== "string") throw new Error("sidecar token is required");
  if (!isAbsolute(authDir)) throw new Error("authDir must be an absolute path");

  const server = http.createServer(async (req, res) => {
    try {
      const authorization = req.headers.authorization ?? "";
      if (authorization !== `Bearer ${token}`) {
        respondJson(res, 401, errorBody("authentication_error", "invalid sidecar token"));
        return;
      }
      const url = new URL(req.url, "http://sidecar.local");
      if (req.method === "GET" && url.pathname === "/health") {
        const health = sessionRegistry.health();
        respondJson(res, 200, JSON.stringify({ ok: true, sdkVersion, ...health }));
        return;
      }
      if (req.method === "POST" && url.pathname === "/models") {
        await handleModels(req, res, modelCatalog);
        return;
      }
      if (req.method === "POST" && url.pathname === "/run") {
        await handleRun(req, res, sessionRegistry);
        return;
      }
      respondJson(res, 404, errorBody("not_found_error", `unknown route ${req.method} ${url.pathname}`));
    } catch (error) {
      const status = error?.statusCode ?? 502;
      const headers = error?.retryAfter !== undefined ? { "retry-after": String(error.retryAfter) } : {};
      if (!res.headersSent) {
        respondJson(res, status, errorBody(error?.code ?? "cursor_sdk_error", String(error?.message ?? error)), headers);
      } else {
        res.end();
      }
    }
  });
  return server;
}

/** POST /models:apiKey 发现模型目录。 */
async function handleModels(req, res, modelCatalog) {
  const body = await readJsonBody(req);
  const apiKey = typeof body?.apiKey === "string" ? body.apiKey : "";
  if (!apiKey.trim()) {
    respondJson(res, 400, errorBody("invalid_request_error", "apiKey is required"));
    return;
  }
  try {
    const catalog = await modelCatalog(apiKey);
    respondJson(res, 200, JSON.stringify(catalog));
  } catch (error) {
    respondJson(res, 502, errorBody("cursor_sdk_models_unavailable", String(error?.message ?? error)));
  }
}

/**
 * POST /run:主入口。
 * 首业务帧前错误返回 HTTP 错误(不提交 200);流中错误写 SSE error 事件。
 * 断连(响应未正常结束)触发 SessionActor.abort。
 */
async function handleRun(req, res, sessionRegistry) {
  const body = await readJsonBody(req);
  const request = {
    apiKey: typeof body?.apiKey === "string" ? body.apiKey : "",
    model: typeof body?.model === "string" ? body.model : "",
    modelParams: Array.isArray(body?.modelParams) ? body.modelParams : [],
    systemPrompt: typeof body?.systemPrompt === "string" ? body.systemPrompt : "",
    workspaceDir: body?.workspaceDir,
    messages: Array.isArray(body?.messages) ? body.messages : [],
    tools: Array.isArray(body?.tools) ? body.tools : [],
  };
  if (!request.apiKey || !request.model) {
    respondJson(res, 400, errorBody("invalid_request_error", "apiKey and model are required"));
    return;
  }
  validateMessageImages(request.messages);
  try {
    request.workspaceDir = await normalizeWorkspaceDir(request.workspaceDir);
  } catch (error) {
    respondJson(res, 400, errorBody("invalid_request_error", String(error?.message ?? error)));
    return;
  }

  let headersCommitted = false;
  let finished = false;
  const subscriber = {
    emit: (event) => {
      if (finished) return;
      if (!headersCommitted) {
        if (event.type === "error") {
          // 首业务帧前错误:HTTP 错误响应,不提交 200
          finished = true;
          respondJson(res, 502, errorBody(event.code ?? "cursor_sdk_run_error", String(event.message ?? "run failed")));
          return;
        }
        res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
        headersCommitted = true;
      }
      res.write(`data: ${JSON.stringify(event)}\n\n`);
    },
    close: () => {
      if (finished) return;
      finished = true;
      if (headersCommitted) res.end();
    },
  };

  const handle = await sessionRegistry.run(request, subscriber);
  // 断连:响应未正常结束时取消 Run
  res.once("close", () => {
    if (!finished) {
      finished = true;
      Promise.resolve(handle.abort("client_disconnect")).catch(() => {});
    }
  });
}

// ---------------------------------------------------------------------------
// 进程入口:环境变量读取、journal 重放、registry 装配、READY 帧。
// ---------------------------------------------------------------------------

import { pathToFileURL } from "node:url";

/** journal 重放:尾部向前按 sessionKey 去重,保留最近 32 个会话最新快照。 */
export async function restoreJournalIndex(journal, maxSessions = 32) {
  const records = await journal.replay();
  const journalIndex = new Map();
  const retained = [];
  for (let index = records.length - 1; index >= 0; index -= 1) {
    const record = records[index];
    if (!record?.sessionKey || journalIndex.has(record.sessionKey)) continue;
    journalIndex.set(record.sessionKey, record);
    retained.push(record);
    if (retained.length >= maxSessions) break;
  }
  await journal.compact(retained.slice().reverse());
  return journalIndex;
}

/** 模型目录:发现优先,失败回落缓存(离线兜底)。 */
function makeModelCatalog(adapter, authDir) {
  return async (apiKey) => {
    try {
      const models = await discoverModels(adapter, apiKey);
      await saveModelCache(authDir, models);
      return { models };
    } catch (error) {
      const cached = await loadModelCache(authDir);
      if (cached) return { models: cached };
      throw error;
    }
  };
}

function sidecarPort() {
  const raw = process.env.CCEXTRA_CURSOR_PORT ?? "8223";
  const port = Number(raw);
  if (!Number.isInteger(port) || port < 0 || port > 65535) {
    throw new Error(`invalid CCEXTRA_CURSOR_PORT: ${raw}`);
  }
  return port;
}

/** 进程入口:stdout 只写一行 READY,日志全部 stderr。 */
export async function main() {
  const token = process.env.CCEXTRA_CURSOR_TOKEN;
  const authDir = process.env.CCEXTRA_CURSOR_AUTH_DIR;
  if (!token) {
    console.error("CCEXTRA_CURSOR_TOKEN is required");
    process.exit(1);
  }
  if (!authDir || !isAbsolute(authDir)) {
    console.error("CCEXTRA_CURSOR_AUTH_DIR must be an absolute path");
    process.exit(1);
  }
  const journal = new Journal(join(authDir, "sessions.jsonl"));
  // compact 必须在 HTTP server 接收请求前完成
  const journalIndex = await restoreJournalIndex(journal);
  const sdkModule = await import("@cursor/sdk");
  const adapter = createSdkAdapter(loadSdk(sdkModule));
  const registry = new SessionRegistry({
    journal,
    sdk: adapter,
    config: {
      idleSecs: Number(process.env.CCEXTRA_CURSOR_IDLE_SECS ?? 1800),
      maxAgents: Number(process.env.CCEXTRA_CURSOR_MAX_AGENTS ?? 16),
    },
    journalIndex,
  });
  const server = createServer({
    token,
    authDir,
    sessionRegistry: registry,
    modelCatalog: makeModelCatalog(adapter, authDir),
  });
  await new Promise((resolve, reject) => {
    const onListening = () => {
      server.off("error", onError);
      resolve();
    };
    const onError = (error) => {
      server.off("listening", onListening);
      reject(error);
    };
    server.once("listening", onListening);
    server.once("error", onError);
    server.listen(sidecarPort(), "127.0.0.1");
  });
  process.stdout.write(`${JSON.stringify({ event: "ready", port: server.address().port })}\n`);
  const gcTimer = setInterval(() => registry.sweep(), 60_000);
  gcTimer.unref();
}

const invokedAsMain = process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
if (invokedAsMain) {
  main().catch((error) => {
    console.error(`sidecar failed to start: ${error?.message ?? error}`);
    process.exit(1);
  });
}
