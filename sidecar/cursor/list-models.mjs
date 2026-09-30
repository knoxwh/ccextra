// list-models.mjs:独立 CLI,列出 SDK 模型目录(cursor_models 白名单配置依据)。
// 不经 sidecar HTTP;apiKey 从 CCEXTRA_CURSOR_API_KEY 环境变量读取,
// 避免命令行参数泄漏。输出 JSON:{ models: [{ id, displayName, parameters }] }
// 与 main.mjs 同属入口边界,直接 import @cursor/sdk。
import "./proxy-tunnel.mjs";

const apiKey = process.env.CCEXTRA_CURSOR_API_KEY;
if (!apiKey) {
  console.error("missing CCEXTRA_CURSOR_API_KEY");
  process.exit(1);
}

const { Cursor } = await import("@cursor/sdk");
if (!Cursor?.models?.list) {
  console.error("Cursor SDK models bindings are incomplete");
  process.exit(1);
}

let models;
try {
  models = await Cursor.models.list({ apiKey });
} catch (err) {
  // 上游错误(无效 key、网络故障)收敛为单行提示,避免倾倒 SDK 内部堆栈
  console.error(`list models failed: ${err?.message ?? err}`);
  process.exit(1);
}
const out = models.map((m) => ({
  id: m.id,
  displayName: m.displayName,
  parameters: (m.parameters ?? []).map((p) => ({
    id: p.id,
    values: (p.values ?? []).map((v) => v.value),
  })),
}));
process.stdout.write(JSON.stringify({ models: out }, null, 2) + "\n");
