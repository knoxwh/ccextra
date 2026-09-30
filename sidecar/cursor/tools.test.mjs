import test from "node:test";
import assert from "node:assert/strict";
import {
  ToolUseIndex,
  completedToolSignature,
  mapCustomTools,
  normalizeToolResult,
  buildCompletedResults,
} from "./tools.mjs";

test("completedToolSignature is stable and input-sensitive", () => {
  assert.equal(completedToolSignature("Read", { path: "a" }), completedToolSignature("Read", { path: "a" }));
  assert.notEqual(completedToolSignature("Read", { path: "a" }), completedToolSignature("Read", { path: "b" }));
  assert.notEqual(completedToolSignature("Read", { path: "a" }), completedToolSignature("Bash", { path: "a" }));
  assert.equal(completedToolSignature("Read", undefined), completedToolSignature("Read", {}));
});

test("normalizeToolResult maps Anthropic shapes to SDK results", () => {
  assert.equal(normalizeToolResult({ content: "plain" }), "plain");
  assert.equal(
    normalizeToolResult({ content: [{ type: "text", text: "a" }, { type: "text", text: "b" }] }),
    "ab"
  );
  assert.deepEqual(
    normalizeToolResult({ content: "boom", is_error: true }),
    { content: [{ type: "text", text: "boom" }], isError: true }
  );
});

test("normalizeToolResult preserves base64 images as SDK content", () => {
  assert.deepEqual(
    normalizeToolResult({
      content: [
        { type: "text", text: "diagram" },
        { type: "image", source: { type: "base64", media_type: "image/png", data: "AQID" } },
      ],
    }),
    {
      content: [
        { type: "text", text: "diagram" },
        { type: "image", data: "AQID", mimeType: "image/png" },
      ],
    }
  );
});

test("normalizeToolResult rejects remote image URLs", () => {
  assert.throws(
    () => normalizeToolResult({ content: [{ type: "image", source: { type: "url", url: "https://example.com/a.png" } }] }),
    (error) => error?.statusCode === 400 && error?.code === "unsupported_parameter"
  );
});

test("normalizeToolResult rejects invalid base64 and MIME", () => {
  for (const source of [
    { type: "base64", media_type: "image/png", data: "not base64!" },
    { type: "base64", media_type: "image/tiff", data: "AQID" },
    "malformed source",
  ]) {
    assert.throws(
      () => normalizeToolResult({ content: [{ type: "image", source }] }),
      (error) => error?.statusCode === 400 && error?.code === "invalid_request_error"
    );
  }
});
test("mapCustomTools produces SDK customTools shape", async () => {
  const tools = [{ name: "Read", description: "read a file", input_schema: { type: "object" } }];
  const customTools = mapCustomTools(tools, () => {}, undefined);
  assert.deepEqual(Object.keys(customTools), ["Read"]);
  assert.equal(customTools.Read.description, "read a file");
  assert.equal(customTools.Read.inputSchema, tools[0].input_schema);
  assert.equal(typeof customTools.Read.execute, "function");
});

test("mapCustomTools execute emits tool_use and awaits pending result", async () => {
  const emitted = [];
  const tools = [{ name: "Read", input_schema: {} }];
  const customTools = mapCustomTools(
    tools,
    (event) => {
      emitted.push(event);
      return Promise.resolve("tool output");
    },
    undefined
  );
  const result = await customTools.Read.execute({ path: "a" }, { toolCallId: "call-1" });
  assert.equal(result, "tool output");
  assert.deepEqual(emitted, [{ id: "call-1", name: "Read", input: { path: "a" } }]);
});

test("mapCustomTools completedResults hit returns recorded result without tool_use", async () => {
  const emitted = [];
  const tools = [{ name: "Read", input_schema: {} }];
  const completed = { [completedToolSignature("Read", { path: "a" })]: ["recorded"] };
  const customTools = mapCustomTools(
    tools,
    (event) => {
      emitted.push(event);
      return Promise.resolve("should not happen");
    },
    completed
  );
  const result = await customTools.Read.execute({ path: "a" }, { toolCallId: "call-1" });
  assert.equal(result, "recorded");
  assert.equal(emitted.length, 0);
});

test("completedResults queue drains in order per signature", async () => {
  const tools = [{ name: "Read", input_schema: {} }];
  const signature = completedToolSignature("Read", { path: "a" });
  const completed = { [signature]: ["first", "second"] };
  const customTools = mapCustomTools(tools, () => {}, completed);
  assert.equal(await customTools.Read.execute({ path: "a" }, { toolCallId: "c1" }), "first");
  assert.equal(await customTools.Read.execute({ path: "a" }, { toolCallId: "c2" }), "second");
  // 队列耗尽后回落到 pending 路径
  const pending = customTools.Read.execute({ path: "a" }, { toolCallId: "c3" });
  assert.equal(typeof pending.then, "function");
});

test("buildCompletedResults pairs assistant tool_use with user tool_result", () => {
  const transcript = [
    { role: "user", content: "hi" },
    {
      role: "assistant",
      content: [{ type: "tool_use", id: "call-1", name: "Read", input: { path: "a" } }],
    },
    {
      role: "user",
      content: [{ type: "tool_result", tool_use_id: "call-1", content: "file body" }],
    },
  ];
  const completed = buildCompletedResults(transcript);
  const signature = completedToolSignature("Read", { path: "a" });
  assert.deepEqual(completed[signature], ["file body"]);
});

test("buildCompletedResults keeps duplicate signatures in order", () => {
  const transcript = [
    {
      role: "assistant",
      content: [
        { type: "tool_use", id: "call-1", name: "Read", input: { path: "a" } },
        { type: "tool_use", id: "call-2", name: "Read", input: { path: "a" } },
      ],
    },
    {
      role: "user",
      content: [
        { type: "tool_result", tool_use_id: "call-1", content: "first" },
        { type: "tool_result", tool_use_id: "call-2", content: "second" },
      ],
    },
  ];
  const completed = buildCompletedResults(transcript);
  const signature = completedToolSignature("Read", { path: "a" });
  assert.deepEqual(completed[signature], ["first", "second"]);
});

test("ToolUseIndex registers, retires and tombstones", async () => {
  const index = new ToolUseIndex();
  index.register("call-1", { sessionKey: "s1", runId: "run-1", batchId: "batch-1" });
  assert.deepEqual(index.lookup("call-1"), { sessionKey: "s1", runId: "run-1", batchId: "batch-1" });
  // 结算后 retire:active 索引删除,tombstone 保留 payload digest
  await index.settle("call-1", "payload");
  index.retire("run-1");
  assert.equal(index.lookup("call-1"), undefined);
  assert.equal(index.tombstoneMatches("call-1", "payload"), true);
  assert.equal(index.tombstoneMatches("call-1", "other"), false);
  assert.equal(index.tombstoneMatches("call-9", "payload"), false);
});
