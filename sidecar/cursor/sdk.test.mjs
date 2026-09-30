import test from "node:test";
import assert from "node:assert/strict";
import { loadSdk, createSdkAdapter } from "./sdk.mjs";

function fakeAgentBindings() {
  const calls = { create: [], resume: [], send: [] };
  const Agent = {
    create: async (options) => {
      calls.create.push(options);
      return { agentId: "agent-1", send: undefined };
    },
    resume: async (agentId, options) => {
      calls.resume.push({ agentId, options });
      return { agentId };
    },
  };
  return { Agent, calls };
}

test("loadSdk rejects incomplete Agent bindings", () => {
  assert.throws(() => loadSdk({ Agent: { create: async () => {} } }), /incomplete/);
  assert.throws(() => loadSdk({}), /incomplete/);
});

test("createAgent passes exact shared options", async () => {
  const { Agent, calls } = fakeAgentBindings();
  const adapter = createSdkAdapter(loadSdk({ Agent }));
  await adapter.createAgent({
    apiKey: "key",
    modelId: "auto",
    modelParams: [],
    systemPrompt: "be brief",
    workspaceDir: "/tmp",
  });
  assert.equal(calls.create.length, 1);
  assert.deepEqual(calls.create[0], {
    apiKey: "key",
    model: { id: "auto", params: [] },
    mode: "agent",
    tools: ["mcp"],
    local: { cwd: "/tmp", settingSources: [] },
  });
});

test("resumeAgent passes agentId and identical shared options", async () => {
  const { Agent, calls } = fakeAgentBindings();
  const adapter = createSdkAdapter(loadSdk({ Agent }));
  await adapter.resumeAgent({
    agentId: "agent-1",
    state: "clean",
    apiKey: "key",
    modelId: "auto",
    modelParams: [],
    systemPrompt: "be brief",
    workspaceDir: "/tmp",
  });
  assert.equal(calls.resume.length, 1);
  assert.equal(calls.resume[0].agentId, "agent-1");
  assert.deepEqual(calls.resume[0].options, {
    apiKey: "key",
    model: { id: "auto", params: [] },
    mode: "agent",
    tools: ["mcp"],
    local: { cwd: "/tmp", settingSources: [] },
  });
});

test("resumeAgent rejects non-clean state", async () => {
  const { Agent } = fakeAgentBindings();
  const adapter = createSdkAdapter(loadSdk({ Agent }));
  for (const state of ["awaiting_tool_results", "dirty"]) {
    await assert.rejects(
      () => adapter.resumeAgent({ agentId: "a", state, apiKey: "k", modelId: "auto", modelParams: [], workspaceDir: "/tmp" }),
      /resume/
    );
  }
});

function fakeSendAgent() {
  const sent = [];
  const rawRun = {
    id: "run-raw",
    stream: async function* () {
      yield { type: "assistant", message: { content: [{ type: "text", text: "hi" }] } };
    },
    wait: async () => ({ status: "finished", result: "hi" }),
    cancel: () => {},
  };
  return {
    sent,
    agent: {
      agentId: "agent-1",
      send: async (text, options) => {
        sent.push({ text, options });
        return rawRun;
      },
    },
    rawRun,
  };
}

test("sendRun passes two-argument send with exact options", async () => {
  const { agent, sent } = fakeSendAgent();
  const adapter = createSdkAdapter(loadSdk({ Agent: { create: async () => agent, resume: async () => agent } }));
  const customTools = { Read: { description: "d", inputSchema: {}, execute: async () => "ok" } };
  const onDelta = async () => {};
  const wrapped = await adapter.sendRun(agent, {
    text: "hello",
    modelId: "auto",
    modelParams: [],
    customTools,
    force: true,
    onDelta,
    runId: "run-1",
    idempotencyKey: "hash-1",
  });
  assert.equal(sent.length, 1);
  assert.equal(sent[0].text, "hello");
  assert.deepEqual(sent[0].options, {
    model: { id: "auto", params: [] },
    mode: "agent",
    local: { customTools, force: true },
    onDelta,
    idempotencyKey: "hash-1",
  });
  assert.equal(wrapped.runId, "run-1");
  assert.equal(typeof wrapped.stream[Symbol.asyncIterator], "function");
  assert.equal(typeof wrapped.wait, "function");
  assert.equal(typeof wrapped.cancel, "function");
});

test("sendRun passes base64 images as SDK user message", async () => {
  const { agent, sent } = fakeSendAgent();
  const adapter = createSdkAdapter(loadSdk({ Agent: { create: async () => agent, resume: async () => agent } }));
  const images = [{ data: "AQID", mimeType: "image/png" }];
  await adapter.sendRun(agent, {
    text: "inspect",
    images,
    modelId: "auto",
    modelParams: [],
    runId: "run-image",
    idempotencyKey: "hash-image",
  });
  assert.deepEqual(sent[0].text, { text: "inspect", images });
});

test("sendRun wait normalizes status and usage", async () => {
  const { agent } = fakeSendAgent();
  const adapter = createSdkAdapter(loadSdk({ Agent: { create: async () => agent, resume: async () => agent } }));
  const wrapped = await adapter.sendRun(agent, {
    text: "hello",
    modelId: "auto",
    modelParams: [],
    runId: "run-1",
    idempotencyKey: "hash-1",
  });
  const result = await wrapped.wait();
  assert.equal(result.status, "finished");
  assert.equal(result.result, "hi");
});

test("sharedOptions never passes systemPrompt (account-gated option)", async () => {
  const { createSdkAdapter } = await import("./sdk.mjs");
  const created = [];
  const adapter = createSdkAdapter({
    Agent: {
      create: async (options) => {
        created.push(options);
        return { agentId: "a1", send: async () => ({ stream: () => ({}) }) };
      },
      resume: async () => {},
    },
    Cursor: { models: { list: async () => [] } },
  });
  await adapter.createAgent({
    apiKey: "k",
    modelId: "auto",
    modelParams: [],
    systemPrompt: "be brief",
    workspaceDir: "/tmp",
  });
  assert.equal("systemPrompt" in created[0], false);
});
