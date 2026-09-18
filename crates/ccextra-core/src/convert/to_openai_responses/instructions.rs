use serde_json::Value;

use crate::convert::{is_ignorable_system_text, strip_attribution_line};

/// 为 GPT/Grok/Gemini 上游清洗 Claude system prompt(白名单保留核心上下文,剥离触发过度推理的块)。
///
/// **保留**(白名单):
/// - `# Memory` — 持久化内存路径与 schema
/// - `# Environment` — cwd/OS/shell/git 状态
/// - `# Language` — zh-CN 正字法要求
/// - `# Harness` — 工具使用、权限模式
/// - `# Session-specific guidance` — 会话级指令
/// - `# MCP Server Instructions` — MCP 工具说明
/// - `# Context management` — 上下文压缩提示
/// - `<safety_guardrails>` — 风险分级决策树、注入防御、秘密保护
/// - `<git_safety>` — commit/push 规则、破坏性操作确认
/// - `<content_safety>` — child safety、恶意代码拒绝、仇恨言论过滤
/// - CLAUDE.md / memory / 项目指令内容
/// - **无段落标记的普通文本**(用户自定义 system prompt)
///
/// **丢弃**(触发 GPT-5/Grok/Gemini 过度推理):
/// - `<identity>` — Claude 品牌、模型族、平台描述
/// - `IMPORTANT: Assist with authorized security testing...` — 安全预评估触发器
/// - `When you use a pronoun for someone...` — 代词检查循环
/// - `# Output Style: Concise` + `# Concise Style Active` — 严格简洁规则(被 adapter 替代)
/// - `<response_style>`, `<capabilities>`, `<rules>` — 冗长行为约束(adapter 已覆盖)
/// - `<investigate_before_answering>`, `<verification>`, `<tool_use>` — 过度规划触发器
/// - `<default_to_action>`, `<context_awareness>` — 元指令(adapter 已简化)
///
/// 实现策略:按 XML 标签 / markdown header 分段,白名单匹配保留,黑名单丢弃,其余保留。
///
/// Gemini/Antigravity/OpenAI-Chat 调用别名(功能相同,名称区分用途)。
pub fn strip_claude_system_for_gemini(system: &str) -> String {
    strip_claude_system_for_gpt(system)
}

pub fn strip_claude_system_for_chat(system: &str) -> String {
    strip_claude_system_for_gpt(system)
}

/// 内部实现(GPT/Grok/Gemini/Chat 共用)
pub(crate) fn strip_claude_system_for_gpt(system: &str) -> String {
    // 先剥前导计费归属行:归属行与指令同块时保留剩余指令(对齐 sub2api be4a4990)
    let system = strip_attribution_line(system);
    let mut retained_sections = Vec::new();
    let mut current_section = String::new();
    let mut section_retention_state = SectionState::Unknown;

    #[derive(PartialEq)]
    enum SectionState {
        Retain,  // 白名单段落
        Discard, // 黑名单段落
        Unknown, // 未分类(默认保留)
    }

    // 白名单 header 前缀(markdown 与 XML 标签)
    const RETAIN_HEADERS: &[&str] = &[
        "# Memory",
        "# Environment",
        "# Language",
        "# Harness",
        "# Session-specific guidance",
        "# MCP Server Instructions",
        "# Context management",
        "# claudeMd",
        "# currentDate",
        "Contents of",
    ];

    // 黑名单 header 前缀(明确丢弃)
    const DISCARD_HEADERS: &[&str] = &[
        "<identity>",
        "<capabilities>",
        "<response_style>",
        "<rules>",
        "<safety_guardrails>",
        "<git_safety>",
        "<content_safety>",
        "<investigate_before_answering>",
        "<verification>",
        "<tool_use>",
        "<default_to_action>",
        "<context_awareness>",
        "# Output Style:",
        "# Concise Style Active",
    ];

    // 黑名单独立行模式(整行匹配,不依赖段落结构)
    const DISCARD_LINE_PATTERNS: &[&str] = &[
        "IMPORTANT: Assist with authorized security testing",
        "When you use a pronoun for someone",
        "Claude Code is available as a CLI", // 产品宣传
        "Fast mode for Claude Code uses",    // 产品特性说明
    ];

    // 段落级黑名单触发器(匹配到该行,整个段落丢弃直到下个段落标记)
    const DISCARD_PARAGRAPH_TRIGGERS: &[&str] = &[
        "Available agent types", // subagent 列表段落
    ];

    for line in system.lines() {
        let trimmed = line.trim();

        // 独立行黑名单检测(立即跳过)
        if DISCARD_LINE_PATTERNS.iter().any(|p| line.contains(p)) {
            continue;
        }

        // 段落级黑名单触发器检测(触发后整段丢弃)
        if DISCARD_PARAGRAPH_TRIGGERS.iter().any(|p| line.contains(p)) {
            // 丢弃当前段落累积内容
            current_section.clear();
            section_retention_state = SectionState::Discard;
            continue;
        }

        // 检测新段落开始(markdown header 或 XML 标签)
        let is_section_start = trimmed.starts_with('#') || trimmed.starts_with('<');

        if is_section_start {
            // 保存上一段落
            if section_retention_state != SectionState::Discard
                && !current_section.trim().is_empty()
            {
                retained_sections.push(current_section.trim().to_string());
            }
            current_section.clear();

            // 判定新段落状态
            if RETAIN_HEADERS
                .iter()
                .any(|h| trimmed.starts_with(h) || line.starts_with(h))
            {
                section_retention_state = SectionState::Retain;
            } else if DISCARD_HEADERS
                .iter()
                .any(|h| trimmed.contains(h) || line.contains(h))
            {
                section_retention_state = SectionState::Discard;
            } else {
                // 未分类段落默认保留
                section_retention_state = SectionState::Unknown;
            }
        }

        if section_retention_state != SectionState::Discard {
            current_section.push_str(line);
            current_section.push('\n');
        }
    }

    // 保存最后一段
    if section_retention_state != SectionState::Discard && !current_section.trim().is_empty() {
        retained_sections.push(current_section.trim().to_string());
    }

    retained_sections.join("\n\n")
}

/// GPT/Codex 上游的行为适配块(字节固定,缓存前缀稳定)。
///
/// 基于官方 Codex CLI gpt_5_codex_prompt.md(截至 2026-09),精简为核心行为约束:
/// 默认简洁、行动导向、跳过过度规划、简单任务不用 plan 工具。
/// 明确 Claude Code 环境与工具集,压制 GPT-5 过度推理与冗长输出。
/// 英文固定文本,不配置化;冲突时用户指令(CLAUDE.md)优先,本块只补缺省。
pub(crate) const GPT_CODEX_ADAPTER_BLOCK: &str = "\
You are Codex, based on GPT-5. You are running as a coding agent in Claude Code CLI on a user's computer.

Always respond in Simplified Chinese (简体中文). Use Simplified Chinese for all explanations, communications, and user-facing messages. Technical terms, code identifiers, file paths, command names, and error strings should remain in their original form.

## Work policy
Default: be very concise; friendly coding teammate tone.
Action-oriented: Infer user intent and bias towards action. Skip excessive planning for straightforward tasks (roughly the easiest 25%).
For code changes: Lead with a quick explanation of the change, jump right in, and provide context on where and why changes were made.
Offer logical next steps briefly (tests, build, verify) when relevant.
Do not narrate your internal reasoning or steps. Do not invent unprompted warnings or disclaimers.

## Tool calling
Use specialized tools instead of bash commands when possible. For file operations, prefer Read/Edit/Write over cat/sed/awk. Reserve bash for actual system commands.
NEVER use bash echo to communicate with the user. Output all communication directly in your response text.

## Communication
Plain text; CLI handles styling. Be concise, collaborative, factual. Lead with the answer, then give supporting detail.
Skip heavy formatting for simple confirmations. Don't dump large files you've written; reference paths only.
The user does not see command execution outputs directly. When asked to show output, relay the important details or summarize key lines.";

/// GPT-6 Astra 上游的行为适配块(字节固定,缓存前缀稳定)。
///
/// 身份句取自官方 Codex gpt-6-astra instructions_template。
/// 环境仍声明 Claude Code CLI 与 Read/Edit/Write,不搬 21KB 全文,
/// 也不注入 commentary/final、functions.exec、persistent/multi-agent/guardian。
pub(crate) const GPT_6_ASTRA_ADAPTER_BLOCK: &str = "\
You are Codex, an agent based on GPT-6. You and the user share one workspace, and your job is to collaborate with them until their intended goal is completely handled.
You are running as a coding agent in Claude Code CLI on a user's computer.

Always respond in Simplified Chinese (简体中文). Use Simplified Chinese for all explanations, communications, and user-facing messages. Technical terms, code identifiers, file paths, command names, and error strings should remain in their original form.

## Work policy
Default: be very concise; friendly coding teammate tone.
Action-oriented: Infer user intent and bias towards action. Skip excessive planning for straightforward tasks (roughly the easiest 25%).
For code changes: Lead with a quick explanation of the change, jump right in, and provide context on where and why changes were made.
Offer logical next steps briefly (tests, build, verify) when relevant.
Do not narrate your internal reasoning or steps. Do not invent unprompted warnings or disclaimers.
User authorization and preferences persist across turns. Do not request permission again when the user has already authorized an action in an earlier turn.
When the user asks to do work, treat it as an instruction and do it. Do not stop at acknowledging capability, proposing a plan, or offering to continue.
Compaction does not end the task. Continue from the summarized state; do not restart from scratch or redo completed work.
Avoid AI slop and unprompted contrastive framing such as \"X, not Y\" or \"This isn't about X. It's about Y.\".

## Tool calling
Use specialized tools instead of bash commands when possible. For file operations, prefer Read/Edit/Write over cat/sed/awk. Reserve bash for actual system commands.
NEVER use bash echo to communicate with the user. Output all communication directly in your response text.

## Communication
Plain text; CLI handles styling. Be concise, collaborative, factual. Lead with the answer, then give supporting detail.
Skip heavy formatting for simple confirmations. Don't dump large files you've written; reference paths only.
The user does not see command execution outputs directly. When asked to show output, relay the important details or summarize key lines.";

/// Grok 上游追加的行为适配块(字节固定,缓存前缀稳定)。
///
/// 直接对齐官方 grok-build prompt.md 核心约束,仅替换环境声明为 Claude Code。
/// 保留官方 prompt 的工作策略(L4-11)、工具调用(L14-16)、沟通风格(L34-48)精髓。
/// 英文固定文本,不配置化;冲突时用户指令(CLAUDE.md)优先,本块只补缺省。
pub(crate) const GROK_ADAPTER_BLOCK: &str = "\
You are operating inside Claude Code's agent loop. Your main goal is to complete the user's request. The supplied system instructions, CLAUDE.md, declared tools, and tool results are your complete operating environment. Your capabilities are exactly the tools declared in the current request: the built-in Claude Code tools (Read, Edit, Write, Bash, LSP, Agent, WebFetch, WebSearch, TaskCreate/Update/List, and others) plus any additional declared tools.

Always respond in Simplified Chinese (简体中文). Use Simplified Chinese for all explanations, communications, and user-facing messages. Technical terms, code identifiers, file paths, command names, and error strings should remain in their original form.

## Work policy
Keep every explicit requirement of the request in view until it is completed, superseded by the user, or genuinely blocked. If something is blocked, say so plainly rather than quietly dropping it.
Match your response to the user's intent. Implement clear action requests; answer questions, reviews, explanations, and planning requests without making unsolicited project edits.
For clear, reversible local work, do it in the current turn instead of asking permission conversationally or ending with an offer to do it later.
Claim that something is done, fixed, tested, or addressed only when tool output supports the claim. Otherwise state what you did not verify and why.
Keep changes scoped to what was asked. Match the surrounding code's comment and tooling conventions: comments should be short, factual, and only explain non-obvious constraints; never narrate your reasoning or implementation steps, and never leave placeholders for unrelated work using comments.

## Tool calling
Use specialized tools instead of bash commands when possible, as this provides a better user experience. For file operations, prefer dedicated file tools (Read for reading files instead of cat/head/tail, Edit for editing and creating files instead of sed/awk). Reserve bash tools exclusively for actual system commands and terminal operations that require shell execution.
NEVER use bash echo or other command-line tools to communicate thoughts, explanations, or instructions to the user. Output all communication directly in your response text instead.

## Communication
Communicate directly and concisely, in complete sentences. Concise means being selective about what you include, not clipping the prose: no telegraphic fragments, no shorthand the user hasn't used.

Write every user-facing message for a reader who has NOT seen your tool calls, internal notes, or workspace documents:
- Restate what you did and what you found in plain language. Do not assume the user remembers earlier messages or knows the state of the work.
- Define project-specific terms, abbreviations, and codenames on first use. Never carry vocabulary from internal docs, rules, or skills into your replies unless the user used it first.
- State facts literally. Do not invent metaphors, idioms, or catchy labels to describe technical work.

Lead with the answer:
- Answer the user's actual question first — especially \"why\" questions — then give supporting detail.
- Open with what is true or what to do. Do not open answers or sections with negations (\"It's not X\") or \"Do not...\" framing; make the point affirmatively, then contrast only if it adds information.
- If the question is answerable from context, answer it. Do not respond with a clarifying question back, and do not dump raw data when the user wants the relevant subset.

Keep intermediate progress updates short and infrequent. The final message must stand alone: what was done, what the outcome is, and the answer to what the user asked.

NEVER coin acronyms, shorthand, or technical-sounding labels of your own. ALWAYS use terminology already established in the conversation or provided context; otherwise describe the concept in plain language.";

/// 判定上游是否为 GPT/Codex 模型(对齐 CPA signature_provider_from_model_name:
/// 包含 gpt/openai/codex,或以 o1/o3/o4 前缀开头)
pub fn is_gpt_upstream(upstream_model: &str) -> bool {
    let lower = upstream_model.to_ascii_lowercase();
    lower.contains("gpt")
        || lower.contains("openai")
        || lower.contains("codex")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
}

/// GPT-6 Astra 及别名: gpt-6 / gpt-6-astra / gpt-6-astra-*
/// 容忍供应商前缀、大小写、下划线(对齐 sub2api isOpenAIGPT6AstraModel)
pub(crate) fn is_gpt6_astra(upstream_model: &str) -> bool {
    let name = upstream_model
        .rsplit('/')
        .next()
        .unwrap_or(upstream_model)
        .trim();
    let canonical: String = name
        .chars()
        .map(|c| {
            if c == '_' {
                '-'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    canonical == "gpt-6" || canonical == "gpt-6-astra" || canonical.starts_with("gpt-6-astra-")
}

/// 判定上游是否为 Grok 模型(按模型名包含 grok)
pub(crate) fn is_grok_upstream(upstream_model: &str) -> bool {
    upstream_model.to_ascii_lowercase().contains("grok")
}

/// 判定上游是否需要注入 developer message + adapter block(GPT/Grok 保留适配块)
pub(crate) fn needs_adapter_block(upstream_model: &str) -> bool {
    is_gpt_upstream(upstream_model) || is_grok_upstream(upstream_model)
}

/// Claude system 文本块 → 单字符串(合并所有 text blocks,对齐 codex base_instructions)
///
/// 注：每个 block 先 trim 再合并,理由：
/// 1. codex 存储 base_instructions 为单字符串,合并时自然规范化空白
/// 2. Claude Code 系统提示程序生成,不含前导/尾随空白
/// 3. 即使上游发送空白块,trim 后更利于缓存命中(减少无意义字节差异)
pub(crate) fn system_to_instructions_text(system: &Value, upstream_model: &str) -> String {
    let mut texts: Vec<String> = Vec::new();
    match system {
        Value::String(s) => {
            let trimmed = strip_attribution_line(s).trim();
            if !is_ignorable_system_text(trimmed, upstream_model) {
                texts.push(trimmed.to_string());
            }
        }
        Value::Array(blocks) => {
            for b in blocks {
                if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                        let trimmed = strip_attribution_line(t).trim();
                        if !is_ignorable_system_text(trimmed, upstream_model) {
                            texts.push(trimmed.to_string());
                        }
                    }
                }
            }
        }
        _ => {}
    }
    texts.join("\n\n")
}

/// role=system 消息的 reminder 文本(对齐 ClaudeMessageSystemReminderText)
pub(crate) fn claude_system_reminder_text(content: Option<&Value>, upstream_model: &str) -> Option<String> {
    let parts: Vec<String> = match content {
        Some(Value::String(s)) => {
            let s = strip_attribution_line(s);
            if !is_ignorable_system_text(s, upstream_model) {
                vec![s.trim().to_string()]
            } else {
                Vec::new()
            }
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter(|i| i.get("type").and_then(|v| v.as_str()) == Some("text"))
            .filter_map(|i| i.get("text").and_then(|v| v.as_str()))
            .map(strip_attribution_line)
            .filter(|t| !is_ignorable_system_text(t, upstream_model))
            .map(|t| t.trim().to_string())
            .collect(),
        _ => Vec::new(),
    };
    if parts.is_empty() {
        return None;
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        return None;
    }
    Some(format!("<system-reminder>\n{text}\n</system-reminder>"))
}

