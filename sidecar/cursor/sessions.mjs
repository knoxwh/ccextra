// sessions.mjs:canonical transcript、前缀哈希链、会话亲和判定。
// 纯逻辑模块,不 import @cursor/sdk,不做 IO(realpath 除外)。
import { createHash } from "node:crypto";
import { realpath } from "node:fs/promises";
import { isAbsolute } from "node:path";

const sha256 = (value) => createHash("sha256").update(value).digest("hex");

/** 递归剥除 thinking block;其余结构原样保留。 */
function stripThinkingBlocks(value) {
  if (Array.isArray(value)) {
    return value
      .filter((block) => !(block && typeof block === "object" && block.type === "thinking"))
      .map(stripThinkingBlocks);
  }
  if (value && typeof value === "object") {
    const out = {};
    for (const [key, child] of Object.entries(value)) out[key] = stripThinkingBlocks(child);
    return out;
  }
  return value;
}

/** 递归排序对象键并剥除 null/undefined 字段;数组顺序保持。 */
function canonicalizeValue(value) {
  if (Array.isArray(value)) return value.map(canonicalizeValue);
  if (value && typeof value === "object") {
    const out = {};
    for (const key of Object.keys(value).sort()) {
      const child = value[key];
      if (child === undefined || child === null) continue;
      out[key] = canonicalizeValue(child);
    }
    return out;
  }
  return value;
}

/** content 字符串归一为 text block 数组(请求与 Run 输出哈希一致)。 */
function normalizeTurnShape(turn) {
  if (!turn || typeof turn !== "object" || typeof turn.content !== "string") return turn;
  return { ...turn, content: turn.content ? [{ type: "text", text: turn.content }] : [] };
}

/**
 * canonicalizeTurn:回合稳定序列化。
 * 剥 thinking block 与 null/undefined 字段;保留 role、content、type、id、
 * name、input、tool_use_id、错误标志及空 tool_result 内容。
 */
export function canonicalizeTurn(turn) {
  return JSON.stringify(canonicalizeValue(stripThinkingBlocks(normalizeTurnShape(turn))));
}

/**
 * computeTurnHashes:滚动哈希链。
 * turn_hash[0] = sha256(sha256(systemPrompt)+sha256(model)+sha256(canonical modelParams)
 *   +sha256(workspaceDir)+canonical(turn_0))
 * turn_hash[i] = sha256(turn_hash[i-1]+canonical(turn_i))
 * workspaceDir 必须已由 normalizeWorkspaceDir 规范化。
 */
export function computeTurnHashes({ systemPrompt, model, modelParams, workspaceDir, turns }) {
  const hashes = [];
  let previous = null;
  for (let index = 0; index < turns.length; index += 1) {
    const canonicalTurn = canonicalizeTurn(turns[index]);
    if (index === 0) {
      const salt = [
        sha256(systemPrompt ?? ""),
        sha256(model ?? ""),
        sha256(JSON.stringify(canonicalizeValue(modelParams ?? []))),
        sha256(workspaceDir ?? ""),
        canonicalTurn,
      ].join("");
      previous = sha256(salt);
    } else {
      previous = sha256(previous + canonicalTurn);
    }
    hashes.push(previous);
  }
  return hashes;
}

/** normalizeWorkspaceDir:要求绝对路径,realpath 解析 symlink;失败抛出原始错误。 */
export async function normalizeWorkspaceDir(input) {
  if (typeof input !== "string" || !isAbsolute(input)) {
    throw new Error(`workspaceDir must be an absolute path, got: ${input}`);
  }
  return realpath(input);
}

/** computeIdempotencyKey:完整 transcript 最后一个 turn_hash;空 transcript 返回 null。 */
export function computeIdempotencyKey(input) {
  const hashes = computeTurnHashes(input);
  return hashes.length > 0 ? hashes[hashes.length - 1] : null;
}

/**
 * matchTurnHashes:入站哈希链对已记录链的亲和判定。
 * - new:无公共前缀,新建 Agent
 * - suffix:覆盖已记录全部回合,只发缺失后缀
 * - replay:入站短于已记录 transcript 或中途分歧,full replay
 */
export function matchTurnHashes(recorded, incoming) {
  const limit = Math.min(recorded.length, incoming.length);
  let matched = 0;
  while (matched < limit && recorded[matched] === incoming[matched]) matched += 1;
  if (matched === 0) return { mode: "new", matchedCount: 0 };
  if (matched === recorded.length) return { mode: "suffix", matchedCount: matched };
  return { mode: "replay", matchedCount: matched };
}

/** isResumable:仅 clean 状态允许 Agent.resume。 */
export function isResumable(snapshot) {
  return snapshot?.state === "clean";
}

// ---------------------------------------------------------------------------
// SessionActor / SessionRegistry:会话亲和、Run 生命周期与工具回调协调。
// ---------------------------------------------------------------------------

import { randomUUID } from "node:crypto";
import {
  ToolBatch,
  ToolUseIndex,
  mapCustomTools,
  buildCompletedResults,
  normalizeToolResult,
  extractMessageImages,
} from "./tools.mjs";
import { pumpRunEvents } from "./events.mjs";

/** 回合文本渲染(对齐 cursor-sdk2api renderPrompt 的 block 格式)。 */
export function renderTurnsText(turns) {
  const parts = [];
  for (const turn of turns ?? []) {
    const blocks = Array.isArray(turn?.content) ? turn.content : [{ type: "text", text: turn?.content ?? "" }];
    const text = blocks
      .map((block) => {
        if (block?.type === "text") return typeof block.text === "string" ? block.text : "";
        if (block?.type === "tool_use") {
          return `[tool_use ${block.name} ${block.id}] input=${JSON.stringify(block.input ?? {})}`;
        }
        if (block?.type === "tool_result") {
          const content = typeof block.content === "string"
            ? block.content
            : Array.isArray(block.content)
              ? block.content.map((b) => (b?.type === "text" ? b.text : b?.type === "image" ? "[image]" : JSON.stringify(b ?? null))).join("\n")
              : "";
          return `[tool_result ${block.tool_use_id} is_error=${block.is_error === true}]\n${content}`;
        }
        if (block?.type === "image") return "[image]";
      })
      .filter(Boolean)
      .join("\n");
    if (text) parts.push(`${turn.role}:\n${text}`);
  }
  return parts.join("\n\n") || " ";
}

/** 提取最后一条 user 消息中的 tool_result blocks(当前待提交批次)。 */
export function extractCurrentToolResults(messages) {
  const lastUser = [...(messages ?? [])].reverse().find((m) => m?.role === "user");
  if (!lastUser || !Array.isArray(lastUser.content)) return [];
  return lastUser.content.filter((block) => block?.type === "tool_result");
}

/** 哈希输入组装:workspaceDir 必须已规范化。 */
function hashInput(request) {
  return {
    systemPrompt: request.systemPrompt,
    model: request.model,
    modelParams: request.modelParams,
    workspaceDir: request.workspaceDir,
    turns: request.messages,
  };
}

function busyError() {
  const error = new Error("cursor_sdk_session_busy");
  error.statusCode = 503;
  error.retryAfter = 1;
  return error;
}

/**
 * SessionActor:单会话的 Agent、active Run、subscriber 与 pending 回调持有者。
 * 状态机:active(流进行)→ awaiting_results(tool_use 后等 tool_result)→ terminal;
 * 异常中断标 dirty。同一 Actor 同时只允许一个活跃 Run。
 */
export class SessionActor {
  constructor({ sessionKey, journal, sdk, index, config = {} }) {
    this.sessionKey = sessionKey;
    this.journal = journal;
    this.sdk = sdk;
    this.index = index;
    this.agent = null;
    this.agentId = null;
    this.model = null;
    this.modelParams = null;
    this.workspaceDir = null;
    this.systemPrompt = null;
    this.transcript = [];
    this.turnHashes = [];
    this.state = "clean";
    this.activeRun = null;
    this.lastRequestAt = Date.now();
    this.toolBatchGraceMs = config.toolBatchGraceMs ?? 50;
    this.runIdleTimeoutMs = config.runIdleTimeoutMs ?? 180_000;
    this.batchTimer = null;
    this.idleTimer = null;
  }

  touch() {
    this.lastRequestAt = Date.now();
  }

  /** journal 快照:Agent id、transcript、哈希链与状态。 */
  snapshot(state) {
    return {
      sessionKey: this.sessionKey,
      agentId: this.agentId,
      model: this.model,
      modelParams: this.modelParams,
      workspaceDir: this.workspaceDir,
      systemPromptHash: sha256(this.systemPrompt ?? ""),
      transcript: this.transcript,
      turnHashes: this.turnHashes,
      state,
    };
  }

  /**
   * run:统一入口,返回 { runId, abort } 句柄供 HTTP 层断连取消。
   * - active Run 存在且请求带当前 tool_result → submitToolResults 并切换 subscriber
   * - active Run 存在且无 tool_result → 503 busy(不排队、不并跑)
   * - 否则启动新 Run(suffix / replay / new)
   */
  async run(request, subscriber) {
    const active = this.activeRun;
    const currentResults = extractCurrentToolResults(request.messages);
    if (active && !active.finished) {
      if (currentResults.length > 0) {
        await this.submitToolResults(active.runId, active.batch.batchId, currentResults);
        // 请求历史权威:对齐 transcript 与哈希链(含 tool_result 回合)
        this.transcript = request.messages;
        this.turnHashes = computeTurnHashes(hashInput(request));
        active.subscriber = subscriber;
        this.touch();
        return { runId: active.runId, abort: (reason) => this.abort(active.runId, reason) };
      }
      throw busyError();
    }
    const runId = await this.startRun(request, subscriber);
    this.touch();
    return { runId, abort: (reason) => this.abort(runId, reason) };
  }

  /** 启动新 Run:前缀匹配决定 suffix-only 或 full replay;返回 runId。 */
  async startRun(request, subscriber) {
    const runId = randomUUID();
    const batch = new ToolBatch(runId);
    const incoming = computeTurnHashes(hashInput(request));
    const match = this.agent || this.turnHashes.length > 0
      ? matchTurnHashes(this.turnHashes, incoming)
      : { mode: "new", matchedCount: 0 };
    let sendTurns;
    let completedResults;
    if (match.mode === "suffix" && this.agent) {
      sendTurns = request.messages.slice(match.matchedCount);
      completedResults = undefined;
    } else {
      // full replay:部分前缀、历史改写、新建、journal 恢复后失配
      completedResults = buildCompletedResults(request.messages);
      if (this.agent) {
        this.agent = await this.sdk.createAgent(this.createInput(request));
      } else if (this.agentId && match.mode === "suffix" && isResumable(this)) {
        // journal 恢复的 clean 会话,首个匹配请求惰性 resume
        this.agent = await this.sdk.resumeAgent({ agentId: this.agentId, state: this.state, ...this.createInput(request) });
      } else {
        this.agent = await this.sdk.createAgent(this.createInput(request));
      }
      sendTurns = request.messages;
    }
    if (sendTurns.length === 0) {
      throw new Error("cursor_sdk_transcript_not_extended");
    }
    this.model = request.model;
    this.modelParams = request.modelParams ?? [];
    this.workspaceDir = request.workspaceDir;
    this.systemPrompt = request.systemPrompt;
    this.transcript = request.messages;
    this.turnHashes = incoming;
    this.agentId = this.agent?.agentId ?? this.agentId;
    const customTools = mapCustomTools(
      request.tools,
      (event) => this.parkToolCall(runId, batch, event),
      completedResults
    );
    const wrapped = await this.sdk.sendRun(this.agent, {
      text: renderTurnsText(sendTurns),
      images: extractMessageImages(sendTurns),
      modelId: request.model,
      modelParams: request.modelParams ?? [],
      customTools,
      force: false,
      runId,
      idempotencyKey: incoming[incoming.length - 1],
    });
    this.activeRun = { runId, batch, subscriber, wrapped, finished: false, frozen: false, textBuffer: "", outputBlocks: [] };
    this.state = "active";
    this.scheduleRunIdleTimeout(runId);
    this.pump(runId, wrapped).catch((error) => {
      this.emitRunEvent(runId, { type: "error", code: "cursor_sdk_pump_failed", message: String(error?.message ?? error), retryable: false });
      this.finishRun(runId, "dirty");
    });
    return runId;
  }

  createInput(request) {
    return {
      apiKey: request.apiKey,
      modelId: request.model,
      modelParams: request.modelParams ?? [],
      systemPrompt: request.systemPrompt,
      workspaceDir: request.workspaceDir,
    };
  }

  /** journal 快照恢复:哈希链/transcript/agentId/state,不建 SDK 对象。 */
  restore(snapshot) {
    this.agentId = snapshot.agentId ?? null;
    this.model = snapshot.model ?? null;
    this.modelParams = snapshot.modelParams ?? [];
    this.workspaceDir = snapshot.workspaceDir ?? null;
    this.transcript = Array.isArray(snapshot.transcript) ? snapshot.transcript : [];
    this.turnHashes = Array.isArray(snapshot.turnHashes) ? snapshot.turnHashes : [];
    this.state = snapshot.state ?? "clean";
  }

  /** 事件泵:stream + wait 终态协调;旧 Run 事件按 runId 丢弃。 */
  async pump(runId, wrapped) {
    const result = await pumpRunEvents({
      stream: wrapped.stream,
      wait: wrapped.wait,
      emit: (event) => this.emitRunEvent(runId, event),
      onToolCall: () => {},
    });
    if (result.status === "finished") {
      this.finishRun(runId, "clean");
    } else {
      this.finishRun(runId, "dirty");
    }
  }

  /** 事件下发:runId 不匹配的旧 Run 事件直接丢弃;同时收集 assistant 输出。 */
  emitRunEvent(runId, event) {
    const active = this.activeRun;
    if (!active || active.runId !== runId) return;
    if (event.type === "text_delta") {
      active.textBuffer += event.text;
    } else if (event.type === "tool_use") {
      active.outputBlocks.push({ type: "tool_use", id: event.id, name: event.name, input: event.input });
    } else if (event.type === "turn_end" && event.stop_reason === "end_turn") {
      this.flushAssistantOutput(runId);
    }
    active.subscriber?.emit(event);
    this.touch();
    this.scheduleRunIdleTimeout(runId);
  }

  /**
   * Run 的 assistant 输出追加进 transcript 与哈希链。
   * 请求间隙的前缀匹配依赖该记录;thinking 不入(CC 请求历史已剥)。
   */
  flushAssistantOutput(runId) {
    const active = this.activeRun;
    if (!active || active.runId !== runId) return;
    const blocks = [];
    if (active.textBuffer) blocks.push({ type: "text", text: active.textBuffer });
    blocks.push(...active.outputBlocks);
    if (blocks.length === 0) return;
    const turn = { role: "assistant", content: blocks };
    this.transcript.push(turn);
    const previous = this.turnHashes.length > 0 ? this.turnHashes[this.turnHashes.length - 1] : "";
    this.turnHashes.push(sha256(previous + canonicalizeTurn(turn)));
    active.textBuffer = "";
    active.outputBlocks = [];
  }

  /** Run 终态收尾:retire 索引、落 journal、清 activeRun。 */
  async finishRun(runId, state) {
    const active = this.activeRun;
    if (!active || active.runId !== runId) return;
    clearTimeout(this.batchTimer);
    clearTimeout(this.idleTimer);
    this.batchTimer = null;
    this.idleTimer = null;
    this.index.retire(runId);
    this.state = state;
    this.activeRun = null;
    active.subscriber?.close?.();
    await this.journal.append(this.snapshot(state));
  }

  /**
   * parkToolCall:customTools execute 挂起。
   * 登记 batch 与全局索引,发 tool_use 事件;并行 sibling 由 batch 冻结定时器聚齐。
   */
  parkToolCall(runId, batch, event) {
    return new Promise((resolve, reject) => {
      batch.register({ id: event.id, name: event.name, resolve, reject });
      this.index.register(event.id, { sessionKey: this.sessionKey, runId, batchId: batch.batchId });
      this.emitRunEvent(runId, { type: "tool_use", id: event.id, name: event.name, input: event.input });
      this.scheduleBatchFreeze(runId, batch);
    });
  }

  /** 并行工具收集窗口:重置定时器,到期冻结当前批次。 */
  scheduleBatchFreeze(runId, batch) {
    clearTimeout(this.batchTimer);
    this.batchTimer = setTimeout(() => this.freezeBatch(runId, batch), this.toolBatchGraceMs);
    this.batchTimer.unref?.();
  }

  /** 冻结批次:发 turn_end(tool_use)、关流、落 awaiting_tool_results 快照。 */
  freezeBatch(runId, batch) {
    const active = this.activeRun;
    if (!active || active.runId !== runId || active.frozen) return;
    if (batch.pendingCallbacks === 0) return;
    active.frozen = true;
    this.flushAssistantOutput(runId);
    this.state = "awaiting_tool_results";
    this.emitRunEvent(runId, { type: "turn_end", stop_reason: "tool_use" });
    active.subscriber?.close?.();
    this.journal.append(this.snapshot("awaiting_tool_results")).catch(() => {});
  }

  /**
   * submitToolResults:校验 runId/batchId,整批 resolve。
   * 未知 ID、异 payload、跨批次由 ToolBatch 与调用方校验报错。
   */
  async submitToolResults(runId, batchId, results) {
    const active = this.activeRun;
    if (!active || active.runId !== runId || active.batch.batchId !== batchId) {
      // 跨 run/batch 提交:HTTP 400 契约错误
      throw Object.assign(new Error("cursor_sdk_tool_result_batch_mismatch"), { statusCode: 400 });
    }
    for (const result of results) {
      const payload = normalizeToolResult(result);
      await active.batch.resolve(result.tool_use_id, payload);
      this.index.settle(result.tool_use_id, payload);
    }
    this.touch();
    this.scheduleRunIdleTimeout(runId);
  }

  /** abort:断连/超时/取消;cancel Run、reject pending、落 dirty。 */
  async abort(runId, reason) {
    const active = this.activeRun;
    if (!active || active.runId !== runId) return;
    clearTimeout(this.batchTimer);
    clearTimeout(this.idleTimer);
    this.batchTimer = null;
    this.idleTimer = null;
    try {
      active.wrapped.cancel();
    } catch {
      // cancel 失败不阻断 dirty 落盘
    }
    active.batch.rejectAll(new Error(`run aborted: ${reason}`));
    this.index.retire(runId);
    this.state = "dirty";
    this.activeRun = null;
    active.subscriber?.close?.();
    await this.journal.append(this.snapshot("dirty"));
  }

  /** Run 空闲超时(默认 180s):走 abort 路径。 */
  scheduleRunIdleTimeout(runId) {
    clearTimeout(this.idleTimer);
    this.idleTimer = setTimeout(() => {
      this.abort(runId, "run_idle_timeout").catch(() => {});
    }, this.runIdleTimeoutMs);
    this.idleTimer.unref?.();
  }
}

/**
 * SessionRegistry:sessionKey → SessionActor 注册表 + 全局 ToolUseIndex。
 * sessionKey = 首回合哈希(system/model/modelParams/workspace/turn_0 入盐)。
 */
export class SessionRegistry {
  constructor({ journal, sdk, config = {}, journalIndex = new Map() }) {
    this.actors = new Map();
    this.index = new ToolUseIndex();
    this.journal = journal;
    this.sdk = sdk;
    this.config = config;
    this.journalIndex = journalIndex;
  }

  /** 延续解析:全部 toolUseId 必须指向同一 actor、Run 与 batch。 */
  resolveContinuation(toolUseIds) {
    if (!Array.isArray(toolUseIds) || toolUseIds.length === 0) {
      throw Object.assign(new Error("cursor_sdk_tool_result_unmatched: empty tool_use_ids"), { statusCode: 400 });
    }
    const bindings = toolUseIds.map((id) => this.index.lookup(id));
    const missingIndex = bindings.findIndex((binding) => !binding);
    if (missingIndex >= 0) {
      throw Object.assign(
        new Error(`cursor_sdk_tool_result_unmatched: ${toolUseIds[missingIndex]}`),
        { statusCode: 400 },
      );
    }
    const first = bindings[0];
    for (const binding of bindings) {
      if (
        binding.sessionKey !== first.sessionKey
        || binding.runId !== first.runId
        || binding.batchId !== first.batchId
      ) {
        // 跨 session/run/batch 混批:HTTP 400 契约错误
        throw Object.assign(new Error("cursor_sdk_tool_result_batch_mismatch"), { statusCode: 400 });
      }
    }
    return first;
  }

  /** 延续尝试:失败返回 null(调用方回落 full replay)。 */
  tryResolveContinuation(toolUseIds) {
    try {
      return this.resolveContinuation(toolUseIds);
    } catch {
      return null;
    }
  }

  /**
   * run:请求分派。
   * - 最后 user turn 带 tool_result 且 ID 在 active 索引 → 延续当前 Run
   * - 否则按 sessionKey(首回合哈希)定位或新建 actor,内部做前缀匹配
   * 返回 { runId, abort } 句柄。
   */
  async run(request, subscriber) {
    const currentResults = extractCurrentToolResults(request.messages);
    if (currentResults.length > 0) {
      const binding = this.tryResolveContinuation(currentResults.map((result) => result.tool_use_id));
      if (binding) {
        const actor = this.actors.get(binding.sessionKey);
        if (actor) return actor.run(request, subscriber);
      }
      // active 索引无匹配:full replay 回落(历史含已完成工具结果)
    }
    const incoming = computeTurnHashes(hashInput(request));
    const sessionKey = incoming[0];
    let actor = this.actors.get(sessionKey);
    if (!actor) {
      actor = new SessionActor({
        sessionKey,
        journal: this.journal,
        sdk: this.sdk,
        index: this.index,
        config: this.config,
      });
      // journal 恢复:哈希链/transcript 载入,不建 SDK 对象(惰性 resume)
      const snapshot = this.journalIndex?.get(sessionKey);
      if (snapshot) actor.restore(snapshot);
      this.actors.set(sessionKey, actor);
    }
    return actor.run(request, subscriber);
  }

  /** /health 聚合:Agent 数与 pending callback 数。 */
  health() {
    let agents = 0;
    let pendingCallbacks = 0;
    for (const actor of this.actors.values()) {
      if (actor.agent) agents += 1;
      if (actor.activeRun) pendingCallbacks += actor.activeRun.batch.pendingCallbacks;
    }
    return { agents, pendingCallbacks };
  }

  /**
   * GC:空闲超时与超上限 LRU 淘汰。
   * 淘汰前 journal 已有最近 terminal snapshot;journalIndex 同步最新状态,
   * 后续请求经 restore 走 resume 路径。
   */
  sweep() {
    const now = Date.now();
    const idleMs = (this.config.idleSecs ?? 1800) * 1000;
    const maxAgents = this.config.maxAgents ?? 16;
    for (const [sessionKey, actor] of [...this.actors]) {
      if (actor.activeRun) continue; // 活跃 Run 不淘汰
      if (now - actor.lastRequestAt > idleMs) this.evict(sessionKey, actor);
    }
    if (this.actors.size > maxAgents) {
      const sorted = [...this.actors.entries()].sort((a, b) => a[1].lastRequestAt - b[1].lastRequestAt);
      for (const [sessionKey, actor] of sorted.slice(0, this.actors.size - maxAgents)) {
        if (!actor.activeRun) this.evict(sessionKey, actor);
      }
    }
  }

  evict(sessionKey, actor) {
    this.journalIndex?.set(sessionKey, actor.snapshot(actor.state));
    this.actors.delete(sessionKey);
  }
}
