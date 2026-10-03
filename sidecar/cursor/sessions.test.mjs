import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, symlink, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  canonicalizeTurn,
  computeTurnHashes,
  computeIdempotencyKey,
  normalizeWorkspaceDir,
  matchTurnHashes,
  isResumable,
  renderTurnsText,
  renderRunText,
} from "./sessions.mjs";

const base = {
  systemPrompt: "s",
  model: "auto",
  modelParams: [],
  workspaceDir: process.cwd(),
  turns: [{ role: "user", content: "hi" }],
};

test("model params and workspace change first hash", () => {
  const a = computeTurnHashes(base)[0];
  const b = computeTurnHashes({ ...base, modelParams: [{ id: "effort", value: "high" }] })[0];
  const c = computeTurnHashes({ ...base, workspaceDir: "/tmp" })[0];
  assert.notEqual(a, b);
  assert.notEqual(a, c);
});

test("same tuple yields same hash chain", () => {
  assert.deepEqual(computeTurnHashes(base), computeTurnHashes(base));
});

test("canonicalizeTurn strips thinking blocks and sorts keys", () => {
  const turn = {
    role: "assistant",
    content: [
      { type: "thinking", thinking: "secret" },
      { type: "text", text: "answer" },
    ],
  };
  const canonical = JSON.parse(canonicalizeTurn(turn));
  assert.equal(canonical.content.length, 1);
  assert.equal(canonical.content[0].type, "text");
  // 键排序稳定:序列化后 content 内块键按字母序
  const keys = Object.keys(canonical.content[0]);
  assert.deepEqual(keys, [...keys].sort());
});

test("renderTurnsText marks image blocks for separate SDK attachments", () => {
  const text = renderTurnsText([
    {
      role: "user",
      content: [
        { type: "text", text: "inspect" },
        { type: "image", source: { type: "base64", media_type: "image/png", data: "AQID" } },
      ],
    },
  ]);
  assert.equal(text, "user:\ninspect\n[image]");
});

test("renderRunText prefixes system prompt only on full send", () => {
  const request = { systemPrompt: "be brief" };
  const turns = [{ role: "user", content: "hi" }];
  // 全量发送(new/replay):system 拼正文前缀
  assert.equal(renderRunText(request, turns, true), "system:\nbe brief\n\nuser:\nhi");
  // suffix 续跑:不拼
  assert.equal(renderRunText(request, turns, false), "user:\nhi");
  // 空 systemPrompt:不拼
  assert.equal(renderRunText({ systemPrompt: "" }, turns, true), "user:\nhi");
});

test("canonicalizeTurn keeps empty tool_result content and error flag", () => {
  const turn = {
    role: "user",
    content: [
      { type: "tool_result", tool_use_id: "call-1", content: "", is_error: true },
    ],
  };
  const canonical = JSON.parse(canonicalizeTurn(turn));
  assert.equal(canonical.content[0].content, "");
  assert.equal(canonical.content[0].is_error, true);
  assert.equal(canonical.content[0].tool_use_id, "call-1");
});

test("canonicalizeTurn drops null and undefined fields", () => {
  const turn = { role: "user", content: "hi", cache_control: null };
  const canonical = JSON.parse(canonicalizeTurn(turn));
  assert.equal("cache_control" in canonical, false);
});

test("canonicalizeTurn ignores cache_control markers", () => {
  // Claude Code 在当前消息打 cache_control 断点,历史回显时剥标;
  // 两种形状必须同哈希,否则前缀链每轮断裂触发 full replay
  const marked = { role: "user", content: [{ type: "text", text: "hi", cache_control: { type: "ephemeral" } }] };
  const plain = { role: "user", content: [{ type: "text", text: "hi" }] };
  assert.equal(canonicalizeTurn(marked), canonicalizeTurn(plain));
});

test("canonicalizeTurn is idempotent", () => {
  const turn = { role: "assistant", content: [{ type: "text", text: "a" }] };
  assert.equal(canonicalizeTurn(turn), canonicalizeTurn(JSON.parse(canonicalizeTurn(turn))));
});

test("computeIdempotencyKey is stable across retries and changes with last turn", () => {
  const twoTurns = {
    ...base,
    turns: [
      { role: "user", content: "hi" },
      { role: "assistant", content: "hello" },
    ],
  };
  const first = computeIdempotencyKey(twoTurns);
  assert.equal(first, computeIdempotencyKey(twoTurns));
  assert.equal(first, computeTurnHashes(twoTurns).at(-1));
  const changed = computeIdempotencyKey({
    ...twoTurns,
    turns: [...twoTurns.turns, { role: "user", content: "again" }],
  });
  assert.notEqual(first, changed);
});

test("computeIdempotencyKey returns null for empty transcript", () => {
  assert.equal(computeIdempotencyKey({ ...base, turns: [] }), null);
});

test("matchTurnHashes classifies suffix, replay and new", () => {
  const recorded = computeTurnHashes({
    ...base,
    turns: [
      { role: "user", content: "hi" },
      { role: "assistant", content: "hello" },
    ],
  });
  // 入站覆盖全部已记录回合:只发后缀
  const longer = computeTurnHashes({
    ...base,
    turns: [
      { role: "user", content: "hi" },
      { role: "assistant", content: "hello" },
      { role: "user", content: "again" },
    ],
  });
  assert.equal(matchTurnHashes(recorded, longer).mode, "suffix");
  // 入站短于已记录 transcript:full replay
  const shorter = computeTurnHashes({ ...base, turns: [{ role: "user", content: "hi" }] });
  assert.equal(matchTurnHashes(recorded, shorter).mode, "replay");
  // 无公共前缀:新建
  const fresh = computeTurnHashes({ ...base, turns: [{ role: "user", content: "other" }] });
  assert.equal(matchTurnHashes(recorded, fresh).mode, "new");
  // 历史改写:首回合相同第二回合不同,同样 full replay
  const rewritten = computeTurnHashes({
    ...base,
    turns: [
      { role: "user", content: "hi" },
      { role: "assistant", content: "changed" },
    ],
  });
  assert.equal(matchTurnHashes(recorded, rewritten).mode, "replay");
});

test("normalizeWorkspaceDir resolves symlinks to the same real path", async () => {
  const dir = await mkdtemp(join(tmpdir(), "ccextra-hash-"));
  try {
    const target = join(dir, "target");
    await (await import("node:fs/promises")).mkdir(target);
    const link = join(dir, "link");
    await symlink(target, link);
    assert.equal(await normalizeWorkspaceDir(link), await normalizeWorkspaceDir(target));
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test("normalizeWorkspaceDir rejects relative paths", async () => {
  await assert.rejects(() => normalizeWorkspaceDir("relative/dir"), /absolute/);
});

test("isResumable only allows clean state", () => {
  assert.equal(isResumable({ state: "clean" }), true);
  assert.equal(isResumable({ state: "awaiting_tool_results" }), false);
  assert.equal(isResumable({ state: "dirty" }), false);
});

// ---------------------------------------------------------------------------
// SessionActor / SessionRegistry 生命周期测试(fake SDK 注入)。
// ---------------------------------------------------------------------------

import { SessionRegistry } from "./sessions.mjs";
import { Journal } from "./journal.mjs";
import { createSdkAdapter } from "./sdk.mjs";

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

function fakeSdk() {
  const state = { created: [], resumed: [], sends: [] };
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
    async createAgent(input) {
      const agent = makeAgent();
      state.created.push({ input, agent });
      return agent;
    },
    async resumeAgent(input) {
      const agent = makeAgent();
      state.resumed.push({ input, agent });
      return agent;
    },
    async sendRun(agent, { text, images, modelId, modelParams, customTools, force, onDelta, runId, idempotencyKey }) {
      const message = images?.length > 0 ? { text, images } : text;
      const raw = await agent.send(message, {
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

function fakeSubscriber() {
  const events = [];
  const subscriber = {
    events,
    closed: false,
    // 对齐生产 subscriber:关流后事件丢弃(main.mjs finished 守卫)
    emit: (event) => {
      if (subscriber.closed) return;
      subscriber.events.push(event);
    },
    close: () => { subscriber.closed = true; },
  };
  return subscriber;
}

function fakeJournal() {
  const appended = [];
  return {
    appended,
    append: async (snapshot) => appended.push(snapshot),
    replay: async () => [],
    compact: async () => {},
  };
}

const settle = (ms = 10) => new Promise((resolve) => setTimeout(resolve, ms));

/** 从 subscriber 事件捕获已下发的 tool_use 外部 id(客户端回显同一 id)。 */
const emittedToolId = (subscriber) => subscriber.events.find((e) => e.type === "tool_use")?.id;

function makeRegistry(config = {}) {
  const sdk = fakeSdk();
  const journal = fakeJournal();
  const registry = new SessionRegistry({
    journal,
    sdk,
    config: { toolBatchGraceMs: 5, runIdleTimeoutMs: 60_000, ...config },
  });
  return { sdk, journal, registry };
}

const baseRequest = (messages, tools = []) => ({
  apiKey: "key",
  model: "auto",
  modelParams: [],
  systemPrompt: "s",
  workspaceDir: process.cwd(),
  messages,
  tools,
});

test("text turn flows text_delta, turn_end and clean journal snapshot", async () => {
  const { sdk, journal, registry } = makeRegistry();
  const subscriber = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "hi" }]), subscriber);
  const send = sdk.state.sends[0];
  send.run.push({ type: "assistant", message: { content: [{ type: "text", text: "hello" }] } });
  send.run.end();
  send.run.finish({ status: "finished" });
  await settle();
  assert.deepEqual(subscriber.events, [
    { type: "text_delta", text: "hello" },
    { type: "turn_end", stop_reason: "end_turn" },
  ]);
  assert.equal(subscriber.closed, true);
  assert.equal(journal.appended.at(-1).state, "clean");
  assert.equal(journal.appended.length, 1);
});

test("tool round trip parks, freezes batch, resumes on tool_result", async () => {
  const { sdk, journal, registry } = makeRegistry();
  const tools = [{ name: "Read", description: "read", input_schema: { type: "object" } }];
  const subscriber = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "read a" }], tools), subscriber);
  const send = sdk.state.sends[0];
  const pending = send.options.local.customTools.Read.execute({ path: "a" }, { toolCallId: "call-1" });
  await settle(20);
  const id = emittedToolId(subscriber);
  // 外部 id 合成:原始 toolCallId 不出 sidecar
  assert.match(id, /^call_sdk_[0-9a-f]{32}_Read$/);
  assert.deepEqual(subscriber.events, [
    { type: "tool_use", id, name: "Read", input: { path: "a" } },
    { type: "turn_end", stop_reason: "tool_use" },
  ]);
  assert.equal(subscriber.closed, true);
  assert.equal(journal.appended.at(-1).state, "awaiting_tool_results");
  // 下一请求带 tool_result:延续同一 Run
  const second = fakeSubscriber();
  await registry.run(baseRequest([
    { role: "user", content: "read a" },
    { role: "assistant", content: [{ type: "tool_use", id, name: "Read", input: { path: "a" } }] },
    { role: "user", content: [{ type: "tool_result", tool_use_id: id, content: "file body" }] },
  ], tools), second);
  assert.equal(await pending, "file body");
  assert.equal(sdk.state.sends.length, 1);
  send.run.push({ type: "assistant", message: { content: [{ type: "text", text: "done" }] } });
  send.run.end();
  send.run.finish({ status: "finished" });
  await settle();
  assert.deepEqual(second.events, [
    { type: "text_delta", text: "done" },
    { type: "turn_end", stop_reason: "end_turn" },
  ]);
  assert.equal(journal.appended.at(-1).state, "clean");
});

test("second tool batch after continuation freezes again", async () => {
  // 回归:continuation 不重置 frozen 时,续跑后第二个工具批次
  // 被 freezeBatch 守卫拦截,流挂死到 idle 超时
  const { sdk, journal, registry } = makeRegistry();
  const tools = [{ name: "Read", description: "read", input_schema: { type: "object" } }];
  const subscriber = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "read a" }], tools), subscriber);
  const send = sdk.state.sends[0];
  const pending = send.options.local.customTools.Read.execute({ path: "a" }, { toolCallId: "call-1" });
  await settle(20);
  const id = emittedToolId(subscriber);
  assert.equal(subscriber.closed, true);
  // 下一请求带 tool_result:延续同一 Run
  const second = fakeSubscriber();
  await registry.run(baseRequest([
    { role: "user", content: "read a" },
    { role: "assistant", content: [{ type: "tool_use", id, name: "Read", input: { path: "a" } }] },
    { role: "user", content: [{ type: "tool_result", tool_use_id: id, content: "file body" }] },
  ], tools), second);
  assert.equal(await pending, "file body");
  // 续跑后模型再调工具:必须再次冻结并关流
  const pending2 = send.options.local.customTools.Read.execute({ path: "b" }, { toolCallId: "call-2" });
  await settle(20);
  const id2 = emittedToolId(second);
  assert.deepEqual(second.events, [
    { type: "tool_use", id: id2, name: "Read", input: { path: "b" } },
    { type: "turn_end", stop_reason: "tool_use" },
  ]);
  assert.equal(second.closed, true);
  assert.equal(journal.appended.at(-1).state, "awaiting_tool_results");
  // 收尾:abort 清挂起回调,避免 unhandled rejection
  const actor = [...registry.actors.values()][0];
  await actor.abort(actor.activeRun.runId, "test_cleanup");
  await pending2.catch(() => {});
});

test("second concurrent request on same session returns 503", async () => {
  const { registry } = makeRegistry();
  const tools = [{ name: "Read", input_schema: {} }];
  const first = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "a" }], tools), first);
  // active Run 挂起(未终态):同历史第二请求 503
  await assert.rejects(
    () => registry.run(baseRequest([{ role: "user", content: "a" }], tools), fakeSubscriber()),
    (error) => error.statusCode === 503
  );
});

test("client disconnect aborts run, rejects pending and marks dirty", async () => {
  const { sdk, journal, registry } = makeRegistry();
  const tools = [{ name: "Read", input_schema: {} }];
  const subscriber = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "a" }], tools), subscriber);
  const send = sdk.state.sends[0];
  const pending = send.options.local.customTools.Read.execute({ path: "a" }, { toolCallId: "call-1" });
  await settle(20);
  const actor = [...registry.actors.values()][0];
  await actor.abort(actor.activeRun.runId, "client_disconnect");
  await assert.rejects(() => pending, /client_disconnect/);
  assert.equal(actor.state, "dirty");
  assert.equal(journal.appended.at(-1).state, "dirty");
  assert.equal(actor.activeRun, null);
});

test("run idle timeout cancels and rejects pending callbacks", async () => {
  const { sdk, journal, registry } = makeRegistry({ runIdleTimeoutMs: 15 });
  const tools = [{ name: "Read", input_schema: {} }];
  const subscriber = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "a" }], tools), subscriber);
  const send = sdk.state.sends[0];
  const pending = send.options.local.customTools.Read.execute({ path: "a" }, { toolCallId: "call-1" });
  const observed = await pending.catch((error) => error);
  assert.match(observed.message, /run_idle_timeout/);
  const actor = [...registry.actors.values()][0];
  assert.equal(actor.state, "dirty");
  assert.equal(journal.appended.at(-1).state, "dirty");
});

test("stale events from an aborted run are dropped", async () => {
  const { sdk, registry } = makeRegistry();
  const tools = [{ name: "Read", input_schema: {} }];
  const subscriber = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "a" }], tools), subscriber);
  const send = sdk.state.sends[0];
  send.options.local.customTools.Read.execute({ path: "a" }, { toolCallId: "call-1" }).catch(() => {});
  await settle(20);
  const actor = [...registry.actors.values()][0];
  const runId = actor.activeRun.runId;
  await actor.abort(runId, "client_disconnect");
  const eventsBefore = subscriber.events.length;
  // 旧 Run 晚到事件:不进入任何流
  send.run.push({ type: "assistant", message: { content: [{ type: "text", text: "late" }] } });
  send.run.end();
  send.run.finish({ status: "finished" });
  await settle();
  assert.equal(subscriber.events.length, eventsBefore);
});

test("parallel tools keep pending until all siblings settle", async () => {
  const { sdk, journal, registry } = makeRegistry();
  const tools = [{ name: "Read", input_schema: {} }, { name: "Bash", input_schema: {} }];
  const subscriber = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "a" }], tools), subscriber);
  const send = sdk.state.sends[0];
  const read = send.options.local.customTools.Read.execute({ path: "a" }, { toolCallId: "call-1" });
  const bash = send.options.local.customTools.Bash.execute({ command: "ls" }, { toolCallId: "call-2" });
  await settle(20);
  // 两个 tool_use 都下发,单 freeze 窗口
  const ids = subscriber.events.filter((e) => e.type === "tool_use").map((e) => e.id);
  assert.equal(ids.length, 2);
  for (const id of ids) assert.match(id, /^call_sdk_[0-9a-f]{32}_(Read|Bash)$/);
  assert.equal(subscriber.events.filter((e) => e.type === "turn_end").length, 1);
  // 只提交一个结果:另一个保持 pending,Run 不释放
  const actor = [...registry.actors.values()][0];
  await actor.submitToolResults(actor.activeRun.runId, actor.activeRun.batch.batchId, [
    { type: "tool_result", tool_use_id: ids[0], content: "body" },
  ]);
  assert.equal(actor.activeRun.batch.pendingCallbacks, 1);
  // 补齐 sibling
  await actor.submitToolResults(actor.activeRun.runId, actor.activeRun.batch.batchId, [
    { type: "tool_result", tool_use_id: ids[1], content: "out" },
  ]);
  assert.equal(await read, "body");
  assert.equal(await bash, "out");
  send.run.end();
  send.run.finish({ status: "finished" });
  await settle();
  assert.equal(journal.appended.at(-1).state, "clean");
});

test("stream tool_call announcement extends the freeze window for late siblings", async () => {
  // 公告驱动(对齐 cursor2response):SDK 流先公告 tool_call,后触发 execute;
  // 公告重排冻结定时器,晚于首 park grace 的 sibling 仍入同批。
  // 时序:park t=0,公告 t≈10(原冻结 t=20 前),execute t≈25(原窗口外、
  // 公告窗口内);无公告重排时 Bash 在冻结后被 subscriber 丢弃
  const { sdk, registry } = makeRegistry({ toolBatchGraceMs: 20 });
  const tools = [{ name: "Read", input_schema: {} }, { name: "Bash", input_schema: {} }];
  const subscriber = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "a" }], tools), subscriber);
  const send = sdk.state.sends[0];
  const read = send.options.local.customTools.Read.execute({ path: "a" }, { toolCallId: "call-1" });
  await settle(10);
  send.run.push({ type: "tool_call", call_id: "call-2", name: "Bash", status: "running", args: { command: "ls" } });
  await settle(15); // t≈25:超过首 park 的 20ms grace
  const bash = send.options.local.customTools.Bash.execute({ command: "ls" }, { toolCallId: "call-2" });
  await settle(60);
  const events = subscriber.events;
  assert.deepEqual(events.filter((e) => e.type === "tool_use").map((e) => e.name), ["Read", "Bash"]);
  // 冻结即关流:turn_end 是最后一个事件,单次冻结
  assert.equal(events.at(-1).type, "turn_end");
  assert.equal(events.filter((e) => e.type === "turn_end").length, 1);
  // 收尾:abort 清挂起回调,避免 unhandled rejection
  const actor = [...registry.actors.values()][0];
  await actor.abort(actor.activeRun.runId, "test_cleanup");
  await Promise.allSettled([read, bash]);
});

test("late sibling parked after freeze emits no empty turn_end after resume", async () => {
  // 回归:冻结后晚到 sibling 的 park 不得重排定时器;重排的 stale timer 会在
  // 客户端提交结果续跑(解冻)后触发,对新 subscriber 发零 tool_use 块的 turn_end
  const { sdk, registry } = makeRegistry({ toolBatchGraceMs: 20 });
  const tools = [{ name: "Read", input_schema: {} }, { name: "Bash", input_schema: {} }];
  const subscriber = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "a" }], tools), subscriber);
  const send = sdk.state.sends[0];
  const read = send.options.local.customTools.Read.execute({ path: "a" }, { toolCallId: "call-1" });
  await settle(30); // 冻结已触发,流关闭
  const id = emittedToolId(subscriber);
  // 冻结后晚到 sibling:park 不重排定时器(choke point 守卫)
  const bash = send.options.local.customTools.Bash.execute({ command: "ls" }, { toolCallId: "call-2" });
  await settle(5);
  // 客户端提交 Read 结果续跑:新 subscriber 接管,解冻
  const subscriber2 = fakeSubscriber();
  await registry.run(
    baseRequest(
      [
        { role: "user", content: "a" },
        { role: "assistant", content: [{ type: "tool_use", id, name: "Read", input: { path: "a" } }] },
        { role: "user", content: [{ type: "tool_result", tool_use_id: id, content: "body" }] },
      ],
      tools
    ),
    subscriber2
  );
  await settle(60); // 原冻结窗口两倍以上:stale timer 若存在必已触发
  assert.equal(subscriber2.events.filter((e) => e.type === "turn_end").length, 0);
  // Bash 仍挂起(已知缺口:晚于冻结的 sibling 挂起至 idle 超时)
  const actor = [...registry.actors.values()][0];
  assert.equal(actor.activeRun.batch.pendingCallbacks, 1);
  await actor.abort(actor.activeRun.runId, "test_cleanup");
  await Promise.allSettled([read, bash]);
});

test("unknown tool result id errors at actor level", async () => {
  const { registry } = makeRegistry();
  const tools = [{ name: "Read", input_schema: {} }];
  const subscriber = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "a" }], tools), subscriber);
  const actor = [...registry.actors.values()][0];
  await actor.submitToolResults(actor.activeRun.runId, actor.activeRun.batch.batchId, [
    { type: "tool_result", tool_use_id: "call-404", content: "x" },
  ]).then(
    () => { throw new Error("expected rejection"); },
    (error) => assert.match(error.message, /cursor_sdk_tool_result_unmatched/)
  );
});

test("duplicate identical result is idempotent, different payload errors", async () => {
  const { sdk, registry } = makeRegistry();
  const tools = [{ name: "Read", input_schema: {} }];
  const subscriber = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "a" }], tools), subscriber);
  const send = sdk.state.sends[0];
  const pending = send.options.local.customTools.Read.execute({ path: "a" }, { toolCallId: "call-1" });
  await settle(20);
  const id = emittedToolId(subscriber);
  const actor = [...registry.actors.values()][0];
  const { runId, batch } = actor.activeRun;
  const result = { type: "tool_result", tool_use_id: id, content: "same" };
  await actor.submitToolResults(runId, batch.batchId, [result]);
  await actor.submitToolResults(runId, batch.batchId, [result]);
  assert.equal(await pending, "same");
  await actor.submitToolResults(runId, batch.batchId, [
    { type: "tool_result", tool_use_id: id, content: "different" },
  ]).then(
    () => { throw new Error("expected rejection"); },
    (error) => assert.match(error.message, /different payload/)
  );
});

test("suffix reuse sends only missing turns", async () => {
  const { sdk, registry } = makeRegistry();
  const first = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "hi" }]), first);
  const send1 = sdk.state.sends[0];
  send1.run.push({ type: "assistant", message: { content: [{ type: "text", text: "hello" }] } });
  send1.run.end();
  send1.run.finish({ status: "finished" });
  await settle();
  const second = fakeSubscriber();
  await registry.run(baseRequest([
    { role: "user", content: "hi" },
    { role: "assistant", content: "hello" },
    { role: "user", content: "again" },
  ]), second);
  assert.equal(sdk.state.created.length, 1);
  assert.equal(sdk.state.sends.length, 2);
  assert.equal(sdk.state.sends[1].text.includes("hi"), false);
  assert.equal(sdk.state.sends[1].text.includes("hello"), false);
  assert.equal(sdk.state.sends[1].text.includes("again"), true);
  // suffix 续跑不带 system 前缀(首轮已含)
  assert.equal(sdk.state.sends[1].text.includes("system:"), false);
});

test("suffix reuse survives cache_control marker movement", async () => {
  const { sdk, registry } = makeRegistry();
  // 首轮:当前消息带 cache_control 断点(块数组形状)
  await registry.run(baseRequest([
    { role: "user", content: [{ type: "text", text: "hi", cache_control: { type: "ephemeral" } }] },
  ]), fakeSubscriber());
  const send1 = sdk.state.sends[0];
  send1.run.push({ type: "assistant", message: { content: [{ type: "text", text: "hello" }] } });
  send1.run.end();
  send1.run.finish({ status: "finished" });
  await settle();
  // 下一轮:历史回显剥标并坍缩为裸字符串,新末轮消息重新打标;须仍走 suffix
  await registry.run(baseRequest([
    { role: "user", content: "hi" },
    { role: "assistant", content: "hello" },
    { role: "user", content: [{ type: "text", text: "again", cache_control: { type: "ephemeral" } }] },
  ]), fakeSubscriber());
  assert.equal(sdk.state.created.length, 1);
  assert.equal(sdk.state.sends[1].text.includes("again"), true);
  assert.equal(sdk.state.sends[1].text.includes("hello"), false);
});

test("empty messages rejected before agent creation", async () => {
  const { sdk, registry } = makeRegistry();
  await assert.rejects(
    () => registry.run(baseRequest([]), fakeSubscriber()),
    /cursor_sdk_empty_messages/
  );
  assert.equal(sdk.state.created.length, 0);
});

test("identical resend after failed run replays instead of erroring", async () => {
  const { sdk, registry } = makeRegistry();
  const first = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "hi" }]), first);
  const send1 = sdk.state.sends[0];
  // run 失败:无 assistant 输出,wait 返回 error → dirty;transcript 已记录 [user]
  send1.run.end();
  send1.run.finish({ status: "error", error: { message: "boom" } });
  await settle();
  // 客户端失败重试:同一 body 重发,零新回合,不再抛 transcript_not_extended
  const second = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "hi" }]), second);
  assert.equal(sdk.state.created.length, 2); // 重建 Agent full replay
  assert.equal(sdk.state.sends.length, 2);
  assert.equal(sdk.state.sends[1].text.startsWith("system:\ns\n\n"), true);
  assert.equal(sdk.state.sends[1].text.includes("hi"), true);
  // 重放 run 正常完成
  sdk.state.sends[1].run.push({ type: "assistant", message: { content: [{ type: "text", text: "hello" }] } });
  sdk.state.sends[1].run.end();
  sdk.state.sends[1].run.finish({ status: "finished" });
  await settle();
  assert.equal(second.events.some((event) => event.type === "turn_end"), true);
});

test("suffix reuse sends only missing image attachments", async () => {
  const { sdk, registry } = makeRegistry();
  const firstImage = { type: "image", source: { type: "base64", media_type: "image/png", data: "AQID" } };
  const secondImage = { type: "image", source: { type: "base64", media_type: "image/jpeg", data: "BAUG" } };
  await registry.run(baseRequest([{ role: "user", content: [{ type: "text", text: "first" }, firstImage] }]), fakeSubscriber());
  const send1 = sdk.state.sends[0];
  send1.run.push({ type: "assistant", message: { content: [{ type: "text", text: "done" }] } });
  send1.run.end();
  send1.run.finish({ status: "finished" });
  await settle();
  await registry.run(baseRequest([
    { role: "user", content: [{ type: "text", text: "first" }, firstImage] },
    { role: "assistant", content: "done" },
    { role: "user", content: [{ type: "text", text: "second" }, secondImage] },
  ]), fakeSubscriber());
  assert.deepEqual(sdk.state.sends[1].text, {
    text: "user:\nsecond\n[image]",
    images: [{ data: "BAUG", mimeType: "image/jpeg" }],
  });
});
test("history rewrite triggers full replay with new agent", async () => {
  const { sdk, registry } = makeRegistry();
  const image = { type: "image", source: { type: "base64", media_type: "image/png", data: "AQID" } };
  const first = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: [{ type: "text", text: "hi" }, image] }]), first);
  const send1 = sdk.state.sends[0];
  send1.run.push({ type: "assistant", message: { content: [{ type: "text", text: "hello" }] } });
  send1.run.end();
  send1.run.finish({ status: "finished" });
  await settle();
  const second = fakeSubscriber();
  await registry.run(baseRequest([
    { role: "user", content: [{ type: "text", text: "hi" }, image] },
    { role: "assistant", content: "changed" },
    { role: "user", content: "again" },
  ]), second);
  assert.equal(sdk.state.created.length, 2);
  assert.equal(sdk.state.sends[1].text.text.includes("hi"), true);
  assert.equal(sdk.state.sends[1].text.text.includes("changed"), true);
  // full replay 重建 Agent,system 前缀随全量 transcript 重发
  assert.equal(sdk.state.sends[1].text.text.startsWith("system:\ns\n\n"), true);
  assert.deepEqual(sdk.state.sends[1].text.images, [{ data: "AQID", mimeType: "image/png" }]);
});

test("tool result image reaches SDK tool callback as content", async () => {
  const { sdk, registry } = makeRegistry();
  const tools = [{ name: "Read", input_schema: {} }];
  const subscriber = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "read" }], tools), subscriber);
  const send = sdk.state.sends[0];
  const pending = send.options.local.customTools.Read.execute({}, { toolCallId: "call-image" });
  await settle(20);
  const id = emittedToolId(subscriber);
  const actor = [...registry.actors.values()][0];
  await actor.submitToolResults(actor.activeRun.runId, actor.activeRun.batch.batchId, [{
    type: "tool_result",
    tool_use_id: id,
    content: [
      { type: "text", text: "screenshot" },
      { type: "image", source: { type: "base64", media_type: "image/png", data: "AQID" } },
    ],
  }]);
  assert.deepEqual(await pending, {
    content: [
      { type: "text", text: "screenshot" },
      { type: "image", data: "AQID", mimeType: "image/png" },
    ],
  });
});

test("sweep evicts idle actors and restores from journal index", async () => {
  const { sdk, registry } = makeRegistry({ idleSecs: 0 });
  const first = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "hi" }]), first);
  const send1 = sdk.state.sends[0];
  send1.run.push({ type: "assistant", message: { content: [{ type: "text", text: "hello" }] } });
  send1.run.end();
  send1.run.finish({ status: "finished" });
  await settle();
  const actor = [...registry.actors.values()][0];
  actor.lastRequestAt = Date.now() - 10_000;
  registry.sweep();
  assert.equal(registry.actors.size, 0);
  assert.equal(registry.journalIndex.has(actor.sessionKey), true);
  // 下次请求:journal 恢复 → suffix 匹配 → resume 路径
  const second = fakeSubscriber();
  await registry.run(baseRequest([
    { role: "user", content: "hi" },
    { role: "assistant", content: "hello" },
    { role: "user", content: "again" },
  ]), second);
  assert.equal(sdk.state.resumed.length, 1);
  assert.equal(sdk.state.created.length, 1);
  assert.equal(sdk.state.sends[1].text.includes("again"), true);
});

test("sweep enforces max agents with LRU eviction", async () => {
  const { sdk, registry } = makeRegistry({ maxAgents: 1 });
  const requests = ["a", "b"].map((text) => baseRequest([{ role: "user", content: text }]));
  for (const request of requests) {
    const subscriber = fakeSubscriber();
    await registry.run(request, subscriber);
  }
  // 终结全部 Run 后才可 LRU 淘汰
  for (const send of sdk.state.sends) {
    send.run.end();
    send.run.finish({ status: "finished" });
  }
  await settle();
  assert.equal(registry.actors.size, 2);
  registry.sweep();
  assert.equal(registry.actors.size, 1);
  // LRU:保留最近请求的 actor
  const [remaining] = [...registry.actors.values()];
  assert.equal(remaining.transcript[0].content, "b");
});

test("sweep keeps actors with active runs", async () => {
  const { registry } = makeRegistry({ idleSecs: 0 });
  const tools = [{ name: "Read", input_schema: {} }];
  const subscriber = fakeSubscriber();
  await registry.run(baseRequest([{ role: "user", content: "a" }], tools), subscriber);
  const actor = [...registry.actors.values()][0];
  actor.lastRequestAt = Date.now() - 10_000;
  registry.sweep();
  assert.equal(registry.actors.size, 1);
});
