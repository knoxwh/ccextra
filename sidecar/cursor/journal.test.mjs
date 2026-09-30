import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, readFile, writeFile, stat, chmod } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Journal } from "./journal.mjs";

const snapshot = (sessionKey, state = "clean") => ({
  sessionKey,
  agentId: `agent-${sessionKey}`,
  model: "auto",
  modelParams: [],
  workspaceDir: "/tmp",
  systemPromptHash: "h0",
  transcript: [{ role: "user", content: "hi" }],
  turnHashes: ["t0"],
  state,
});

async function withJournal(run) {
  const dir = await mkdtemp(join(tmpdir(), "ccextra-journal-"));
  try {
    await run(new Journal(join(dir, "sessions.jsonl")), dir);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

test("append then replay round-trips snapshots in order", async () => {
  await withJournal(async (journal) => {
    await journal.append(snapshot("s1"));
    await journal.append(snapshot("s2"));
    const records = await journal.replay();
    assert.equal(records.length, 2);
    assert.equal(records[0].sessionKey, "s1");
    assert.equal(records[1].sessionKey, "s2");
  });
});

test("same session key keeps only the last line effective", async () => {
  await withJournal(async (journal) => {
    await journal.append(snapshot("s1", "awaiting_tool_results"));
    await journal.append(snapshot("s1", "clean"));
    const records = await journal.replay();
    // append-only:调用方按 sessionKey 取最后一行生效
    const last = records.filter((r) => r.sessionKey === "s1").at(-1);
    assert.equal(last.state, "clean");
  });
});

test("replay drops a truncated trailing line", async () => {
  await withJournal(async (journal, dir) => {
    await journal.append(snapshot("s1"));
    const path = join(dir, "sessions.jsonl");
    await writeFile(path, (await readFile(path, "utf8")) + '{"sessionKey":"s2","trunc');
    const records = await journal.replay();
    assert.equal(records.length, 1);
    assert.equal(records[0].sessionKey, "s1");
  });
});

test("replay fails on a corrupted non-trailing line", async () => {
  await withJournal(async (journal, dir) => {
    await journal.append(snapshot("s1"));
    await journal.append(snapshot("s2"));
    const path = join(dir, "sessions.jsonl");
    const lines = (await readFile(path, "utf8")).split("\n");
    lines.splice(1, 0, "not-json");
    await writeFile(path, lines.join("\n"));
    await assert.rejects(() => journal.replay(), /corrupt/);
  });
});

test("compact atomically replaces the file with retained records", async () => {
  await withJournal(async (journal, dir) => {
    await journal.append(snapshot("s1"));
    await journal.append(snapshot("s2"));
    await journal.compact([snapshot("s2")]);
    const records = await journal.replay();
    assert.equal(records.length, 1);
    assert.equal(records[0].sessionKey, "s2");
    // compact 后仍可继续追加
    await journal.append(snapshot("s3"));
    assert.equal((await journal.replay()).length, 2);
  });
});

test("journal file is created with 0600 permissions", async () => {
  await withJournal(async (journal, dir) => {
    await journal.append(snapshot("s1"));
    const mode = (await stat(join(dir, "sessions.jsonl"))).mode & 0o777;
    assert.equal(mode, 0o600);
  });
});

test("replay on missing file returns empty list", async () => {
  await withJournal(async (journal) => {
    assert.deepEqual(await journal.replay(), []);
  });
});

test("compact creates missing auth directory with 0700 permissions", async () => {
  const root = await mkdtemp(join(tmpdir(), "ccextra-journal-"));
  try {
    const journal = new Journal(join(root, "missing", "sessions.jsonl"));
    await journal.compact([snapshot("s1")]);
    assert.deepEqual(await journal.replay(), [snapshot("s1")]);
    const mode = (await stat(join(root, "missing"))).mode & 0o777;
    assert.equal(mode, 0o700);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("append and compact both fsync the journal file", async () => {
  // patch FileHandle.prototype.sync 计数,断言 append/compact 落盘前 fsync
  const { open } = await import("node:fs/promises");
  const probe = await open(join(tmpdir(), `probe-${process.pid}`), "a");
  const proto = Object.getPrototypeOf(probe);
  await probe.close();
  const original = proto.sync;
  let syncCalls = 0;
  proto.sync = async function syncPatched() {
    syncCalls += 1;
    return original.call(this);
  };
  try {
    await withJournal(async (journal) => {
      await journal.append(snapshot("s1"));
      assert.equal(syncCalls, 1);
      await journal.compact([snapshot("s1")]);
      // compact:文件 fsync + 父目录 fsync
      assert.equal(syncCalls, 3);
    });
  } finally {
    proto.sync = original;
  }
});
