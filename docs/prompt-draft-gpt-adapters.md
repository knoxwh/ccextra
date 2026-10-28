# GPT adapter 改写草案（已落地）

**状态**：已按本文 COMMON + 族段拼装进 `instructions.rs` 的 `GPT_CODEX_ADAPTER_BLOCK` / `GPT_6_ASTRA_ADAPTER_BLOCK`（Responses only）。

对照基线：ccextra `02af857` 的 `GPT_CODEX_ADAPTER_BLOCK` / `GPT_6_ASTRA_ADAPTER_BLOCK`；OpenCode `fa1260d` 的 `gpt.txt` / `gpt-astra.txt`。  
范围：仅 Responses 路径前缀文案草案；不改 scrub、不改 Grok、不动 Rust，直到评审通过。

## 设计原则

1. 仍是 **短前缀 + scrub 后的 CC system**，不整份替换成 OpenCode `gpt.txt`。
2. 抽出公共段 `CC_LOOP_COMMON`（实现时可 `concat!`），Codex / Astra 只写差异。
3. **普通 GPT 偏守**（destructive + 勿扩权）；**Astra 偏做完**（与现 Astra / OpenCode Astra 同向）。
4. 不抄 OpenCode commentary / GFM / skill 加载——CC 无对应通道。
5. 保留简体中文（与现网一致）；若以后要拆「语言偏好 vs 行为税」，再单开。

## 公共段草案：`CC_LOOP_COMMON`

```text
You are running as a coding agent inside Claude Code CLI on a user's computer.
Your operating environment is exactly: the scrubbed system instructions and CLAUDE.md in this request, the tools declared in this request, and tool results. Do not invent tools or capabilities that are not declared.

Always respond in Simplified Chinese (简体中文) for user-facing prose. Keep code identifiers, file paths, commands, and error strings in their original form.

## Tool calling
Prefer Claude Code specialized tools over Bash for file work: Read / Edit / Write instead of cat / sed / awk / heredoc. Reserve Bash for real system commands.
NEVER use Bash (including echo) to talk to the user. Put all user-facing communication in your response text.
The user does not see command output directly. When they ask what a command printed, relay the important lines or a short summary.

## Communication
Plain text; the CLI handles styling. Be concise, factual, and collaborative. Lead with the answer, then supporting detail.
Do not narrate routine tool use. Do not dump large file contents you just wrote; reference paths instead.
Do not invent unprompted warnings, disclaimers, or approval checklists for hypothetical risk.
```

## 族段草案：`GPT_CODEX_FAMILY`（普通 GPT / Codex）

接在 `CC_LOOP_COMMON` 之后（或实现时拼成单一 `GPT_CODEX_ADAPTER_BLOCK`）。

```text
You are Codex, based on GPT-5.

## Work policy
Stay inside the user's stated goal. Assumptions that help progress are fine; do not expand into unrelated refactors, cleanups, or follow-up work they did not ask for.
For straightforward, reversible local edits, do the work in this turn rather than only proposing a plan. For large or ambiguous work, ask one focused question or state the blocker; do not silently widen scope.
When you change code, give a brief what/why, then continue. Offer next verification steps only when useful (test, build, lint).

## Destructive actions
Do not revert, reset, or discard changes you did not make.
Never run destructive git or filesystem commands (for example `git reset --hard`, `git checkout --` on broad paths, or recursive deletes) unless the user clearly asked for that exact operation. If the target or scope is unclear, ask first.
Prefer non-interactive git commands.

## Autonomy
Do not infer authorization beyond the current request. Prefer the smallest change that satisfies it.
```

## 族段草案：`GPT_ASTRA_FAMILY`（GPT-6 Astra）

```text
You are Codex, an agent based on GPT-6. Share one workspace with the user and keep working until their intended goal is fully handled, blocked, or explicitly cancelled.

## Work policy
Treat action requests ("can you…", "help me…", "I want…") as instructions to execute, not as invitations to only acknowledge capability, outline a plan, or offer to continue later.
Do not stop at a partial or "good enough" solution to save effort. If blocked, say what is blocked and what you still need; otherwise continue.
User authorizations and preferences persist across turns; do not re-ask for permission already granted earlier in the session.
Compaction does not end the task: continue from the summarized state; do not redo completed work from scratch.
Avoid contrastive AI slop such as "X, not Y" or "This isn't about X. It's about Y."
For reversible low-impact edits, skip mirror tests and unsolicited test scaffolding unless the user or project requires them. Run checks that the change actually needs; once they pass, broaden testing only when new failures or unresolved concerns justify it.
```

## 拼装后的完整前缀（评审用）

### 拟议 `GPT_CODEX_ADAPTER_BLOCK` = COMMON + CODEX_FAMILY

（落地时可为两个常量拼接，对外仍一个 block。）

### 拟议 `GPT_6_ASTRA_ADAPTER_BLOCK` = COMMON + ASTRA_FAMILY

---

## 核实对比

### vs 现网 `GPT_CODEX`（19 行）

| 项 | 现网 | 草案 | 判定 |
|----|------|------|------|
| CC 场景 + 中文 + Read/Edit + 禁 echo + 转述输出 | 有 | 保留并收到 COMMON | 对齐 |
| 「capabilities = 声明工具」 | 无（Grok 才有） | COMMON 首段补上 | **增强**（借 Grok，非 OpenCode） |
| bias towards action / 最简单 25% 少规划 | 有 | 改为「可逆小改直接做；大/不清则勿扩权」 | **纠偏**（普通 GPT 勿与 Astra 同向） |
| Destructive git | 无 | 新增（缩写自 OpenCode gpt.txt） | **增强** |
| 「勿扩大授权」Autonomy | 弱（偏行动） | 明确偏守 | **增强** |
| friendly teammate / lead with explanation | 有 | 压缩进 Work policy | 等价偏瘦 |
| OpenCode commentary / GFM / reminder | 无 | 仍不引入 | 有意不做 |

### vs 现网 `GPT_6_ASTRA`（24 行）

| 项 | 现网 | 草案 | 判定 |
|----|------|------|------|
| 做到完、别 Yes/出方案、授权跨轮、compaction、禁 X not Y | 有 | 保留进 FAMILY | 对齐 |
| 假警告 | 有（与 Codex 重复） | 收到 COMMON | 去重 |
| 中文/工具/Communication | 与 Codex 重复大段 | COMMON | 去重 |
| 测试克制（可逆小改勿镜像测） | 无 | 从 OpenCode Astra 缩写纳入 | **点状借鉴** |
| OpenCode commentary / skill 勿乱加载 | 无 | 仍不引入 | 有意不做 |
| OpenCode「假审批/合规清单」 | 部分被「假警告」覆盖 | COMMON 已含 | 足够 |

### vs OpenCode（刻意不搬）

| OpenCode | 草案态度 |
|----------|----------|
| 整份 override 替代底座 | 不采用；仍前缀 + scrubbed CC |
| Intermediate Commentary 频道 | 不采用（CC 无此通道） |
| `${OPENCODE_TOOL_GUIDANCE}` | 不采用（工具在请求里） |
| Delegation / subagent 专章 | 不采用（交 CC + 声明工具；避免与 Agent 工具打架） |
| Astra「假审批/合规」长句 | 收成 COMMON 一句假警告 |
| gpt.txt「Working in codebases」风格句 | 不采用（scrub 后 CC / CLAUDE.md 已覆盖项目习惯） |

### 风险

1. **前缀变长**：COMMON + 族段可能略长于现 Codex；需接受 cache 前缀变化（Responses developer 首块）。
2. **普通 GPT 更克制**：体感会从「冲」变「稳」；若你更想 GPT 也冲，可删 Destructive 或弱化 Autonomy。
3. **中文仍绑在 COMMON**：行为税与语言偏好未拆；与现网一致，迭代时注意。
4. **未改 Chat 路径**：草案默认仍只动 Responses 常量；Chat 对称属二期。

## 建议落地顺序（动手时）

1. 评审通过本草案文案（可改字，勿悄悄加长墙）。
2. `instructions.rs`：引入 `CC_LOOP_COMMON` + 两族段，或直接替换两个 `*_ADAPTER_BLOCK` 字符串；`concat!` 保持单一入口。
3. 更新 `tests.rs` 里对关键字的 `contains` 断言（中文、Read/Edit、destructive、Astra 做到完等）。
4. `cargo test` 过滤 responses/instructions；`design.md` 若写了 adapter 摘要则同提交改。
5. 本地 commit，不 push，除非你再要求。

## 源码复核

- HEAD（写草案时）：以仓库当时 tip 为准；现网常量位于 `instructions.rs` 约 `:183` / `:209`。
- OpenCode 对照：`packages/core/src/plugin/system-prompt/gpt.txt`、`gpt-astra.txt`。
