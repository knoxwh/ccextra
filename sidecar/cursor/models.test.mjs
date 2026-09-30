import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadModelCache, saveModelCache, discoverModels } from "./models.mjs";

async function withDir(run) {
  const dir = await mkdtemp(join(tmpdir(), "ccextra-models-"));
  try {
    await run(dir);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

test("discoverModels maps adapter catalog to flat ids", async () => {
  const adapter = { listModels: async (apiKey) => {
    assert.equal(apiKey, "key");
    return [{ id: "auto" }, { id: "composer-2.5" }];
  } };
  assert.deepEqual(await discoverModels(adapter, "key"), [{ id: "auto" }, { id: "composer-2.5" }]);
});

test("model cache round-trips with 0600 permissions", async () => {
  await withDir(async (dir) => {
    assert.equal(await loadModelCache(dir), null);
    await saveModelCache(dir, [{ id: "auto" }]);
    assert.deepEqual(await loadModelCache(dir), [{ id: "auto" }]);
    const mode = (await stat(join(dir, "models.json"))).mode & 0o777;
    assert.equal(mode, 0o600);
  });
});

test("model cache tolerates corrupt file by returning null", async () => {
  await withDir(async (dir) => {
    await writeFile(join(dir, "models.json"), "not json");
    assert.equal(await loadModelCache(dir), null);
  });
});
