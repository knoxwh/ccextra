// models.mjs:账户模型目录发现与 authDir/models.json 缓存。
// 发现入口唯一走 sdk adapter 的 listModels,不直接拼接 Cursor API URL。
import { open, readFile, rename } from "node:fs/promises";
import { basename, dirname, join } from "node:path";

const cachePath = (authDir) => join(authDir, "models.json");

/** 读取缓存目录;缺失或损坏返回 null(离线兜底由调用方处理)。 */
export async function loadModelCache(authDir) {
  try {
    const text = await readFile(cachePath(authDir), "utf8");
    const parsed = JSON.parse(text);
    return Array.isArray(parsed?.models) ? parsed.models : null;
  } catch {
    return null;
  }
}

/** 写缓存:临时文件 + 原子 rename,权限 0600。 */
export async function saveModelCache(authDir, models) {
  const target = cachePath(authDir);
  const dir = dirname(target);
  const tmp = join(dir, `.models.json.tmp-${process.pid}-${Date.now()}`);
  const handle = await open(tmp, "w", 0o600);
  try {
    await handle.write(`${JSON.stringify({ models })}\n`);
    await handle.sync();
  } finally {
    await handle.close();
  }
  await rename(tmp, target);
}

/**
 * 发现模型目录:adapter.listModels(apiKey) → 扁平 ID 列表并写缓存。
 * 失败抛出原始错误(调用方决定保留旧目录)。
 */
export async function discoverModels(adapter, apiKey) {
  if (typeof apiKey !== "string" || !apiKey.trim()) {
    throw new Error("apiKey is required for model discovery");
  }
  const models = await adapter.listModels(apiKey);
  const ids = models
    .map((model) => (typeof model?.id === "string" ? { id: model.id } : null))
    .filter(Boolean);
  return ids;
}
