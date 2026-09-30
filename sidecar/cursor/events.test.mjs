import test from "node:test";
import assert from "node:assert/strict";
import { normalizeSdkEvent, pumpRunEvents } from "./events.mjs";

test("unknown SDK event fails closed", () => {
  assert.throws(() => normalizeSdkEvent({ type: "future_event" }), /unknown SDK event/);
});

test("assistant text blocks map to text_delta events", () => {
  const events = normalizeSdkEvent({
    type: "assistant",
    message: { content: [{ type: "text", text: "hello" }, { type: "text", text: "world" }] },
  });
  assert.deepEqual(events, [
    { type: "text_delta", text: "hello" },
    { type: "text_delta", text: "world" },
  ]);
});

test("assistant tool_use blocks map to tool_use events", () => {
  const events = normalizeSdkEvent({
    type: "assistant",
    message: { content: [{ type: "tool_use", id: "call-1", name: "Read", input: { path: "a" } }] },
  });
  assert.deepEqual(events, [{ type: "tool_use", id: "call-1", name: "Read", input: { path: "a" } }]);
});

test("assistant with invalid content fails closed", () => {
  assert.throws(() => normalizeSdkEvent({ type: "assistant", message: { content: "not-array" } }), /unknown SDK event/);
  assert.throws(
    () => normalizeSdkEvent({ type: "assistant", message: { content: [{ type: "text", text: 1 }] } }),
    /unknown SDK event/
  );
});

test("thinking maps to thinking_delta", () => {
  assert.deepEqual(normalizeSdkEvent({ type: "thinking", text: "reason" }), [
    { type: "thinking_delta", text: "reason" },
  ]);
  assert.throws(() => normalizeSdkEvent({ type: "thinking", text: 5 }), /unknown SDK event/);
});

test("tool_call running maps to tool_use with full args", () => {
  const events = normalizeSdkEvent({
    type: "tool_call",
    call_id: "call-1",
    name: "Bash",
    status: "running",
    args: { command: "ls" },
  });
  assert.deepEqual(events, [{ type: "tool_use", id: "call-1", name: "Bash", input: { command: "ls" } }]);
});

test("tool_call without id or name fails closed", () => {
  assert.throws(() => normalizeSdkEvent({ type: "tool_call", name: "Bash", status: "running" }), /unknown SDK event/);
  assert.throws(() => normalizeSdkEvent({ type: "tool_call", call_id: "c", status: "running" }), /unknown SDK event/);
});

test("tool_call completed and error statuses are diagnostics", () => {
  assert.deepEqual(
    normalizeSdkEvent({ type: "tool_call", call_id: "c", name: "Bash", status: "completed" }),
    []
  );
  assert.deepEqual(
    normalizeSdkEvent({ type: "tool_call", call_id: "c", name: "Bash", status: "error" }),
    []
  );
});

test("tool_call wrapped as mcp is unwrapped", () => {
  const events = normalizeSdkEvent({
    type: "tool_call",
    call_id: "call-1",
    name: "mcp",
    status: "running",
    args: { toolName: "Read", args: { path: "a" } },
  });
  assert.deepEqual(events, [{ type: "tool_use", id: "call-1", name: "Read", input: { path: "a" } }]);
});

test("usage maps to normalized usage fields", () => {
  const events = normalizeSdkEvent({
    type: "usage",
    usage: { inputTokens: 10, outputTokens: 5, cacheReadTokens: 3 },
  });
  assert.deepEqual(events, [
    { type: "usage", input_tokens: 10, output_tokens: 5, cache_read_input_tokens: 3 },
  ]);
  assert.throws(() => normalizeSdkEvent({ type: "usage", usage: { inputTokens: "x" } }), /unknown SDK event/);
});

test("status and diagnostic events emit nothing", () => {
  for (const raw of [
    { type: "status", status: "CREATING" },
    { type: "status", status: "RUNNING" },
    { type: "status", status: "FINISHED" },
    { type: "system" },
    { type: "user" },
    { type: "request", request_id: "r" },
    { type: "task", status: "running" },
  ]) {
    assert.deepEqual(normalizeSdkEvent(raw), []);
  }
  assert.throws(() => normalizeSdkEvent({ type: "status", status: "WEIRD" }), /unknown SDK event/);
});

function fakeStream(events) {
  let index = 0;
  return {
    async *[Symbol.asyncIterator]() {
      while (index < events.length) {
        yield events[index];
        index += 1;
      }
    },
  };
}

test("pump emits text, usage and terminal turn_end after wait", async () => {
  const emitted = [];
  const result = await pumpRunEvents({
    stream: fakeStream([
      { type: "assistant", message: { content: [{ type: "text", text: "hi" }] } },
      { type: "usage", usage: { inputTokens: 1, outputTokens: 2 } },
    ]),
    wait: async () => ({ status: "finished", usage: { inputTokens: 1, outputTokens: 2 } }),
    emit: (event) => emitted.push(event),
  });
  assert.equal(result.status, "finished");
  assert.deepEqual(emitted, [
    { type: "text_delta", text: "hi" },
    { type: "usage", input_tokens: 1, output_tokens: 2 },
    { type: "turn_end", stop_reason: "end_turn" },
  ]);
});

test("pump routes stream tool_use to onToolCall, not emit", async () => {
  const emitted = [];
  const toolCalls = [];
  await pumpRunEvents({
    stream: fakeStream([
      { type: "tool_call", call_id: "call-1", name: "Bash", status: "running", args: { command: "ls" } },
    ]),
    wait: async () => ({ status: "finished" }),
    emit: (event) => emitted.push(event),
    onToolCall: (event) => toolCalls.push(event),
  });
  assert.equal(emitted.length, 1);
  assert.equal(emitted[0].type, "turn_end");
  assert.deepEqual(toolCalls, [{ type: "tool_use", id: "call-1", name: "Bash", input: { command: "ls" } }]);
});

test("pump maps wait error to error event", async () => {
  const emitted = [];
  const result = await pumpRunEvents({
    stream: fakeStream([]),
    wait: async () => ({ status: "error", error: { code: "upstream", message: "boom" } }),
    emit: (event) => emitted.push(event),
  });
  assert.equal(result.status, "error");
  assert.equal(emitted.length, 1);
  assert.equal(emitted[0].type, "error");
  assert.equal(emitted[0].retryable, false);
});

test("pump maps cancelled wait to error event", async () => {
  const emitted = [];
  await pumpRunEvents({
    stream: fakeStream([]),
    wait: async () => ({ status: "cancelled" }),
    emit: (event) => emitted.push(event),
  });
  assert.equal(emitted[0].type, "error");
  assert.match(emitted[0].code, /cancel/);
});

test("pump fails on conflicting stream terminal statuses", async () => {
  const emitted = [];
  await pumpRunEvents({
    stream: fakeStream([
      { type: "status", status: "FINISHED" },
      { type: "status", status: "ERROR" },
    ]),
    wait: async () => ({ status: "finished" }),
    emit: (event) => emitted.push(event),
  });
  assert.equal(emitted.length, 1);
  assert.equal(emitted[0].type, "error");
  assert.match(emitted[0].code, /terminal/);
});

test("pump fails when wait status contradicts stream terminal", async () => {
  const emitted = [];
  await pumpRunEvents({
    stream: fakeStream([{ type: "status", status: "ERROR" }]),
    wait: async () => ({ status: "finished" }),
    emit: (event) => emitted.push(event),
  });
  assert.equal(emitted.length, 1);
  assert.equal(emitted[0].type, "error");
  assert.match(emitted[0].code, /terminal/);
});

test("pump tolerates repeated identical terminal statuses", async () => {
  const emitted = [];
  const result = await pumpRunEvents({
    stream: fakeStream([
      { type: "status", status: "FINISHED" },
      { type: "status", status: "FINISHED" },
    ]),
    wait: async () => ({ status: "finished" }),
    emit: (event) => emitted.push(event),
  });
  assert.equal(result.status, "finished");
  assert.deepEqual(emitted, [{ type: "turn_end", stop_reason: "end_turn" }]);
});
