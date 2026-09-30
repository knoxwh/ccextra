// sdk.mjs 是唯一直接 import @cursor/sdk 的模块。
// 业务模块通过 loadSdk 注入边界获取 Agent 绑定;测试注入假实现。
// 注释使用中文,对齐 ccextra 仓库约定。

/** 校验注入的 Agent 绑定形状并原样返回。 */
export function loadSdk({ Agent, Cursor }) {
  if (!Agent?.create || !Agent?.resume) {
    throw new Error("Cursor SDK Agent bindings are incomplete");
  }
  return { Agent, Cursor };
}

/** Agent.create / Agent.resume 共享 options(对齐 cursor2response localAgentCreateOptions)。 */
function sharedOptions({ apiKey, modelId, modelParams, workspaceDir }) {
  // systemPrompt 不传 SDK:该选项账号级门控,无权限账号 send 即报
  // invalid_argument unknown option '--system-prompt'(官方 forum 确认)。
  // system 文本由 sessions 层拼进 run text 前缀,绕开门控。
  return {
    apiKey,
    model: { id: modelId, params: modelParams },
    mode: "agent",
    tools: ["mcp"],
    local: { cwd: workspaceDir, settingSources: [] },
  };
}

/**
 * createSdkAdapter:基于注入 bindings 的 SDK 适配器。
 * 生产代码传 loadSdk(await import("@cursor/sdk"));测试传假实现。
 */
export function createSdkAdapter(bindings) {
  const { Agent, Cursor } = bindings;
  return {
    /** 唯一模型发现入口:Cursor.models.list,字段映射对齐 cursor-runtime.ts::listModels。 */
    async listModels(apiKey) {
      if (!Cursor?.models?.list) {
        throw new Error("Cursor SDK models bindings are incomplete");
      }
      const models = await Cursor.models.list({ apiKey });
      return models.map((model) => ({ id: model.id }));
    },

    /** 新建 Agent;systemPrompt 仅此处生效(SDK 创建后不可变)。 */
    async createAgent(input) {
      return Agent.create(sharedOptions(input));
    },

    /** 恢复 Agent;仅 clean 状态允许,awaiting/dirty 抛恢复错误。 */
    async resumeAgent(input) {
      if (input.state !== "clean") {
        throw new Error(`agent ${input.agentId} state ${input.state}; resume not allowed`);
      }
      const { agentId, ...rest } = input;
      return Agent.resume(agentId, sharedOptions(rest));
    },

    /**
     * sendRun:SDK 1.0.34 raw API 两参数形式 agent.send(text, options)。
     * runId 由调用方生成(随机 UUID,只绑定当前 Run 的事件与回调);
     * idempotencyKey 为当前完整 transcript 的稳定 turn_hash,不得使用 runId。
     * customTools 只属于当前 send,不累积上一请求工具。
     */
    async sendRun(agent, { text, images, modelId, modelParams, customTools, force, onDelta, runId, idempotencyKey }) {
      if (!runId) throw new Error("runId is required for stale event binding");
      if (!idempotencyKey) throw new Error("idempotencyKey is required for request deduplication");
      const message = images?.length > 0 ? { text, images } : text;
      const run = await agent.send(message, {
        model: { id: modelId, params: modelParams },
        mode: "agent",
        local: { customTools, force: force === true },
        onDelta,
        idempotencyKey,
      });
      return {
        runId,
        stream: run.stream(),
        wait: async () => run.wait(),
        cancel: () => run.cancel(),
      };
    },
  };
}
