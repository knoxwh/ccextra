// events.mjs:SDK raw 事件归一化与 Run 事件泵终态协调。
// 归一化事件类型:text_delta、thinking_delta、tool_use、usage、turn_end、error。
// 未知事件、缺工具 ID、terminal 冲突一律 fail closed。

const TERMINAL_STATUSES = new Set(["FINISHED", "ERROR", "CANCELLED", "EXPIRED"]);
const TOOL_CALL_STATUSES = new Set(["running", "completed", "error"]);
const DIAGNOSTIC_TYPES = new Set(["system", "user", "request", "task"]);

function unknown(reason) {
  return new Error(`unknown SDK event: ${reason}`);
}

/** usage 归一化:inputTokens/outputTokens 必填数字,cacheReadTokens 可选。 */
export function normalizeUsage(raw) {
  if (!raw || typeof raw !== "object") throw unknown("invalid_usage");
  const { inputTokens, outputTokens, cacheReadTokens } = raw;
  if (typeof inputTokens !== "number" || typeof outputTokens !== "number") {
    throw unknown("invalid_usage");
  }
  const usage = { input_tokens: inputTokens, output_tokens: outputTokens };
  if (typeof cacheReadTokens === "number") usage.cache_read_input_tokens = cacheReadTokens;
  return usage;
}

/** assistant message.content 文本块提取(对齐 cursor2response asText 语义)。 */
function assistantTextBlocks(event) {
  const content = event.message?.content;
  if (!Array.isArray(content)) throw unknown("invalid_assistant_content");
  return content;
}

/**
 * normalizeSdkEvent:单个 SDK raw 事件转归一化事件数组(空数组 = 诊断跳过)。
 * - assistant:text 块转 text_delta,tool_use 块转 tool_use
 * - thinking:转 thinking_delta
 * - tool_call:running 转 tool_use(mcp 包装解包);completed/error 为诊断
 * - usage:转 usage
 * - status/system/user/request/task:诊断,不下发
 */
export function normalizeSdkEvent(raw) {
  if (!raw || typeof raw !== "object") throw unknown("invalid_event");
  switch (raw.type) {
    case "assistant": {
      const blocks = assistantTextBlocks(raw);
      const events = [];
      for (const block of blocks) {
        if (!block || typeof block !== "object") throw unknown("invalid_assistant_block");
        if (block.type === "text") {
          if (typeof block.text !== "string") throw unknown("invalid_assistant_text");
          events.push({ type: "text_delta", text: block.text });
          continue;
        }
        if (block.type === "tool_use") {
          if (typeof block.id !== "string" || !block.id.trim() || typeof block.name !== "string" || !block.name.trim()) {
            throw unknown("invalid_assistant_tool_use");
          }
          events.push({ type: "tool_use", id: block.id, name: block.name, input: block.input ?? {} });
          continue;
        }
        throw unknown(`unknown_assistant_block:${block.type}`);
      }
      return events;
    }
    case "thinking": {
      if (typeof raw.text !== "string") throw unknown("invalid_reasoning_text");
      return [{ type: "thinking_delta", text: raw.text }];
    }
    case "tool_call": {
      const callId = typeof raw.call_id === "string" ? raw.call_id.trim() : "";
      const name = typeof raw.name === "string" ? raw.name.trim() : "";
      if (!callId) throw unknown("invalid_tool_call_id");
      if (!name) throw unknown("invalid_tool_name");
      if (!TOOL_CALL_STATUSES.has(raw.status)) throw unknown(`unknown_tool_status:${raw.status}`);
      if (raw.status !== "running") return [];
      // mcp 包装解包:SDK 可能把 customTools 调用报成 name "mcp" + args { toolName, args }
      if (name === "mcp" && typeof raw.args?.toolName === "string" && raw.args.toolName.trim()) {
        const inner = raw.args.args && typeof raw.args.args === "object" && !Array.isArray(raw.args.args)
          ? raw.args.args
          : {};
        return [{ type: "tool_use", id: callId, name: raw.args.toolName.trim(), input: inner }];
      }
      const input = raw.args && typeof raw.args === "object" && !Array.isArray(raw.args) ? raw.args : {};
      return [{ type: "tool_use", id: callId, name, input }];
    }
    case "usage": {
      return [{ type: "usage", ...normalizeUsage(raw.usage) }];
    }
    case "status": {
      if (raw.status !== "CREATING" && raw.status !== "RUNNING" && !TERMINAL_STATUSES.has(raw.status)) {
        throw unknown(`unknown_run_status:${raw.status}`);
      }
      return [];
    }
    default: {
      if (DIAGNOSTIC_TYPES.has(raw.type)) return [];
      throw unknown(`unknown_event_type:${raw.type}`);
    }
  }
}

/** stream 侧 terminal status 与 wait() status 的统一映射。 */
function waitStatusFor(streamStatus) {
  if (streamStatus === "FINISHED") return "finished";
  if (streamStatus === "CANCELLED") return "cancelled";
  return "error";
}

function errorEvent(code, message) {
  return { type: "error", code, message, retryable: false };
}

/**
 * pumpRunEvents:事件泵同时消费 Run.stream() 与 Run.wait()。
 * stream EOF 不代表成功;以 wait() terminal status 校验后发 turn_end。
 * stream 侧 tool_use 不直接下发(工具下发走 customTools execute 回调),
 * 转交 onToolCall 做状态记录;terminal 冲突与 wait 失败发 error 事件。
 * @returns wait() 归一化结果 { status, result?, error?, usage? }
 */
export async function pumpRunEvents({ stream, wait, emit, onToolCall }) {
  let streamTerminal = null;
  for await (const raw of stream) {
    if (raw?.type === "status" && TERMINAL_STATUSES.has(raw.status)) {
      if (streamTerminal !== null && streamTerminal !== raw.status) {
        emit(errorEvent("cursor_sdk_terminal_conflict", `stream terminal changed from ${streamTerminal} to ${raw.status}`));
        return { status: "error" };
      }
      streamTerminal = raw.status;
    }
    for (const event of normalizeSdkEvent(raw)) {
      if (event.type === "tool_use") {
        onToolCall?.(event);
        continue;
      }
      emit(event);
    }
  }
  const result = await wait();
  if (streamTerminal !== null && waitStatusFor(streamTerminal) !== result.status) {
    emit(errorEvent(
      "cursor_sdk_terminal_conflict",
      `stream terminal ${streamTerminal} contradicts wait status ${result.status}`
    ));
    return { status: "error" };
  }
  if (result.status === "finished") {
    emit({ type: "turn_end", stop_reason: "end_turn" });
    return result;
  }
  if (result.status === "cancelled") {
    emit(errorEvent("cursor_sdk_run_cancelled", "run cancelled"));
    return result;
  }
  const message = result.error?.message ?? "run failed";
  const code = result.error?.code ? String(result.error.code) : "cursor_sdk_run_error";
  emit(errorEvent(code, message));
  return result;
}
