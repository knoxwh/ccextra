// tools.mjs:custom tool 桥、工具批次、toolUseId 索引与已完成结果配对。
// 工具下发走 customTools execute 回调(对齐 cursor2response parked 模式);
// stream 侧 tool_call 事件只做状态记录,不重复下发。
import { createHash, randomUUID } from "node:crypto";

/** 稳定 JSON 序列化(排序键)后取 sha256。 */
export function digestJson(value) {
  const canonical = JSON.stringify(value, (key, child) => {
    if (child && typeof child === "object" && !Array.isArray(child)) {
      return Object.fromEntries(Object.keys(child).sort().map((k) => [k, child[k]]));
    }
    return child;
  });
  return createHash("sha256").update(canonical).digest("hex");
}

/**
 * 合成外部 tool_use id(对齐 cursor2response externalToolCallId):
 * 原始 SDK toolCallId 可含换行等控制字符,只做哈希输入,永不下发客户端。
 * 同 sessionKey + 同原始 id 稳定复现,幂等重放时客户端重试仍可匹配。
 */
export function externalToolCallId(sessionKey, rawCallId, toolName) {
  const hash = createHash("sha256").update(`${sessionKey}\0${rawCallId}`).digest("hex").slice(0, 32);
  const suffix = String(toolName || "tool").replace(/[^A-Za-z0-9_-]/g, "_").slice(0, 16) || "tool";
  return `call_sdk_${hash}_${suffix}`;
}

/** 工具签名:name + input 的稳定摘要;同签名按出现顺序共享结果队列。 */
export function completedToolSignature(toolName, input) {
  return digestJson({ name: toolName, input: input ?? {} });
}

/**
 * ToolBatch:一次 Run 内的工具调用批次。
 - expected:已登记调用(id → { name, resolve, reject, settled })
 * - resolve 幂等(字节等价 payload 忽略)、异 payload 冲突、未知 ID unmatched
 * - rejectAll 用于断连/超时/取消
 */
export class ToolBatch {
  constructor(runId, calls = []) {
    this.runId = runId;
    this.batchId = randomUUID();
    this.expected = new Map();
    for (const call of calls) {
      this.expected.set(call.id, { name: call.name, resolve: undefined, reject: undefined, settled: null });
    }
  }

  /** execute 挂起时登记 pending 回调。 */
  register({ id, name, resolve, reject }) {
    const entry = this.expected.get(id);
    if (entry) {
      entry.resolve = resolve;
      entry.reject = reject;
      return;
    }
    this.expected.set(id, { name, resolve, reject, settled: null });
  }

  /** tool_result 到达:未知 ID 报 unmatched(HTTP 400);重复按 payload 字节等价幂等或冲突。 */
  async resolve(id, payload) {
    const entry = this.expected.get(id);
    if (!entry) {
      throw Object.assign(new Error(`cursor_sdk_tool_result_unmatched: ${id}`), { statusCode: 400 });
    }
    const serialized = JSON.stringify(payload);
    if (entry.settled !== null) {
      if (JSON.stringify(entry.settled) === serialized) return;
      throw new Error(`tool result ${id} different payload`);
    }
    entry.settled = payload;
    entry.resolve?.(payload);
  }

  /** 断连/超时/取消:reject 全部 pending 回调并终结批次。 */
  rejectAll(reason) {
    for (const entry of this.expected.values()) {
      if (entry.settled === null) {
        entry.reject?.(reason);
        entry.terminated = true;
      }
      entry.resolve = undefined;
      entry.reject = undefined;
    }
  }

  get pendingCallbacks() {
    let count = 0;
    for (const entry of this.expected.values()) {
      if (entry.settled === null && !entry.terminated) count += 1;
    }
    return count;
  }

  /** 全部 sibling 结果到齐才允许释放 Run 并落 clean 快照。 */
  allSettled() {
    for (const entry of this.expected.values()) {
      if (entry.settled === null && !entry.terminated) return false;
    }
    return true;
  }
}

/**
 * ToolUseIndex:toolUseId → { sessionKey, runId, batchId } 全局索引。
 * Run 终态、cancel、timeout 时 retire:删除 active 索引并写 stale tombstone
 * (保存已结算 payload digest,供重复 tool_result 幂等判定)。
 */
export class ToolUseIndex {
  constructor() {
    this.active = new Map();
    this.tombstones = new Map();
  }

  register(toolUseId, binding) {
    this.active.set(toolUseId, binding);
  }

  lookup(toolUseId) {
    return this.active.get(toolUseId);
  }

  /** 结算时记录 payload digest,retire 后转 tombstone。 */
  settle(toolUseId, payload) {
    const entry = this.active.get(toolUseId);
    if (entry) entry.payloadDigest = digestJson(payload);
  }

  /** Run 结束:该 run 的 active 索引转 tombstone。 */
  retire(runId) {
    for (const [toolUseId, binding] of this.active) {
      if (binding.runId !== runId) continue;
      if (binding.payloadDigest !== undefined) {
        this.tombstones.set(toolUseId, binding.payloadDigest);
      }
      this.active.delete(toolUseId);
    }
  }

  /** tombstone 命中:同 ID 且 payload 字节等价才幂等放行。 */
  tombstoneMatches(toolUseId, payload) {
    const digest = this.tombstones.get(toolUseId);
    return digest !== undefined && digest === digestJson(payload);
  }
}

const IMAGE_MIME_TYPES = new Set(["image/png", "image/jpeg", "image/webp", "image/gif"]);

function imageError(message, code = "invalid_request_error") {
  return Object.assign(new Error(message), { statusCode: 400, code });
}

/** Anthropic image block → Cursor SDK image;仅允许 base64 与白名单 MIME。 */
export function toSdkImage(block) {
  const source = block?.source;
  const sourceIsObject = source !== null && typeof source === "object";
  if (sourceIsObject && (source.type === "url" || "url" in source)) {
    throw imageError(
      "Local Cursor SDK image inputs require a base64 data URL; remote image URLs are unsupported.",
      "unsupported_parameter",
    );
  }
  const data = source?.data;
  const mimeType = source?.media_type;
  const validBase64 = typeof data === "string"
    && /^[A-Za-z0-9+/]+={0,2}$/.test(data)
    && Buffer.from(data, "base64").toString("base64").replace(/=+$/, "") === data.replace(/=+$/, "");
  if (source?.type !== "base64" || !validBase64 || !IMAGE_MIME_TYPES.has(mimeType)) {
    throw imageError("Image inputs must contain valid base64 PNG, JPEG, WebP or GIF data.");
  }
  return { data, mimeType };
}

/** 校验 messages 内全部 image block;必须在会话状态变更前调用。 */
export function validateMessageImages(value) {
  if (Array.isArray(value)) {
    for (const child of value) validateMessageImages(child);
  } else if (value && typeof value === "object") {
    if (value.type === "image") toSdkImage(value);
    for (const child of Object.values(value)) validateMessageImages(child);
  }
}

/** 提取本次发送回合的顶层图片;tool_result 图片走工具结果回调。 */
export function extractMessageImages(turns) {
  const images = [];
  for (const turn of turns ?? []) {
    if (turn?.role !== "user" || !Array.isArray(turn?.content)) continue;
    for (const block of turn.content) {
      if (block?.type === "image") images.push(toSdkImage(block));
    }
  }
  return images;
}
/**
 * normalizeToolResult:Anthropic tool_result → SDK tool result(对齐 cursor2response toSdkToolResult)。
 * content 字符串或 text/image block;is_error 保留为 isError。
 */
export function normalizeToolResult(block) {
  const { text, images } = normalizeToolContent(block?.content);
  if (images.length > 0 || block?.is_error) {
    return {
      content: [
        { type: "text", text },
        ...images.map(({ data, mimeType }) => ({ type: "image", data, mimeType })),
      ],
      ...(block?.is_error ? { isError: true } : {}),
    };
  }
  return text;
}

function normalizeToolContent(content) {
  if (typeof content === "string") return { text: content, images: [] };
  if (!Array.isArray(content)) return { text: "", images: [] };
  let text = "";
  const images = [];
  for (const block of content) {
    if (block?.type === "text" && typeof block.text === "string") {
      text += block.text;
    } else if (block?.type === "image") {
      images.push(toSdkImage(block));
    } else {
      text += JSON.stringify(block ?? null);
    }
  }
  return { text, images };
}

/**
 * buildCompletedResults:full replay 前从 transcript 配对构造已完成结果表。
 * assistant tool_use 与 user tool_result 按 tool_use_id 配对;
 * 同 signature 按出现顺序保存多个结果(队列语义)。
 */
export function buildCompletedResults(transcript) {
  const callsById = new Map();
  for (const turn of transcript) {
    if (turn?.role !== "assistant" || !Array.isArray(turn.content)) continue;
    for (const block of turn.content) {
      if (block?.type === "tool_use") callsById.set(block.id, { name: block.name, input: block.input });
    }
  }
  const completed = {};
  for (const turn of transcript) {
    if (turn?.role !== "user" || !Array.isArray(turn.content)) continue;
    for (const block of turn.content) {
      if (block?.type !== "tool_result") continue;
      const call = callsById.get(block.tool_use_id);
      if (!call) continue;
      const signature = completedToolSignature(call.name, call.input);
      (completed[signature] ??= []).push(normalizeToolResult(block));
    }
  }
  return completed;
}

/**
 * mapCustomTools:每请求生成 SDK customTools(挂 send 不挂 create)。
 * - completedResults 命中:直接返回记录结果,不登记 pending、不发 tool_use
 * - 未命中:emitToolUse({ id, name, input }) 返回 pending promise
 * @param tools Anthropic 工具定义 [{ name, description, input_schema }]
 * @param emitToolUse (event) => Promise<SDK tool result>
 * @param completedResults { [signature]: [result, ...] } 或 undefined
 */
export function mapCustomTools(tools, emitToolUse, completedResults) {
  const customTools = {};
  for (const tool of tools ?? []) {
    const name = typeof tool?.name === "string" ? tool.name.trim() : "";
    if (!name) continue;
    customTools[name] = {
      description: typeof tool.description === "string" ? tool.description : "",
      inputSchema: tool.input_schema ?? { type: "object", properties: {} },
      execute: async (args, context) => {
        const signature = completedToolSignature(name, args);
        const queue = completedResults?.[signature];
        if (queue && queue.length > 0) {
          return queue.shift();
        }
        const toolCallId = typeof context?.toolCallId === "string" ? context.toolCallId : undefined;
        return emitToolUse({ id: toolCallId, name, input: args ?? {} });
      },
    };
  }
  return customTools;
}
