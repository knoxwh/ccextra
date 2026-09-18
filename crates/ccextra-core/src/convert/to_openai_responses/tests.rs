use super::*;
use super::schema::simplify_pure_const_unions;
use super::reasoning::gpt_compatible_signature;
use base64::Engine;
use serde_json::json;

fn astra_registry() -> Vec<crate::thinking::ModelCapability> {
        vec![crate::thinking::ModelCapability {
            id: "gpt-6-astra".into(),
            reasoning_levels: vec!["low".into(), "medium".into()],
            force_effort: None,
        }]
    }

    fn glm51_registry() -> Vec<crate::thinking::ModelCapability> {
        vec![crate::thinking::ModelCapability {
            id: "glm-5.1".into(),
            reasoning_levels: vec!["low".into(), "medium".into(), "high".into(), "xhigh".into()],
            force_effort: None,
        }]
    }

    #[test]
    fn test_system_goes_to_instructions() {
        // 新行为：system → instructions 字段，而非 developer message
        let mut body = json!({
            "model": "test",
            "system": "You are helpful",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let rev = convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["instructions"], "You are helpful");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["text"], "hi");
        assert!(rev.is_empty(), "无工具时反向映射为空");
    }

    #[test]
    fn test_gpt_system_goes_to_developer_message() {
        // GPT 上游将 adapter + system 作为 developer 输入，instructions 留空
        let mut body = json!({
            "model": "test",
            "system": "You are helpful",
            "messages": []
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-terra").unwrap();
        assert_eq!(body["instructions"], "");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "developer");
        let dev_text = body["input"][0]["content"][0]["text"].as_str().unwrap();
        assert!(dev_text.starts_with("You are Codex, based on GPT-5."));
        assert!(dev_text.contains("You are helpful"));
    }

    #[test]
    fn test_gpt_without_system_no_developer_message() {
        // GPT 上游空 system 时仍注入 adapter block
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-sol").unwrap();
        assert_eq!(body["instructions"], "");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2); // developer + user
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[1]["content"][0]["text"], "hi");
    }

    #[test]
    fn test_non_gpt_no_adapter_block() {
        // 非 gpt/grok 上游不注入 adapter block
        let mut body = json!({
            "model": "test",
            "system": "You are helpful",
            "messages": []
        });
        convert_to_openai_responses(&mut body, "claude-opus-5").unwrap();
        assert_eq!(body["instructions"], "You are helpful");
        assert_eq!(body["input"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_grok_system_and_adapter_go_to_developer_message() {
        // Grok 线将 adapter + 清洗后的 system 作为 developer 输入，instructions 留空。
        let mut body = json!({
            "model": "test",
            "system": "You are helpful",
            "messages": []
        });
        convert_to_openai_responses(&mut body, "grok-4.6").unwrap();
        assert_eq!(body["instructions"], "");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "developer");
        let dev_text = body["input"][0]["content"][0]["text"].as_str().unwrap();
        assert!(dev_text.starts_with("You are operating inside Claude Code"));
        assert!(dev_text.contains("You are helpful"));
    }

    #[test]
    fn test_grok_adapter_creates_developer_message_without_system() {
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_responses(&mut body, "grok-3").unwrap();
        assert_eq!(body["instructions"], "");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[0]["content"][0]["text"], GROK_ADAPTER_BLOCK);
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[1]["content"][0]["text"], "hi");
    }

    #[test]
    fn test_grok_adapter_block_contains_key_constraints() {
        // 验证 GROK_ADAPTER_BLOCK 包含官方 prompt.md 的核心约束
        assert!(GROK_ADAPTER_BLOCK.contains("inside Claude Code's agent loop"));
        assert!(GROK_ADAPTER_BLOCK.contains("Your capabilities are exactly the tools declared"));
        assert!(GROK_ADAPTER_BLOCK.contains("only when tool output supports the claim"));
        assert!(GROK_ADAPTER_BLOCK.contains("Use specialized tools instead of bash commands"));
        assert!(GROK_ADAPTER_BLOCK.contains("Communicate directly and concisely"));
        assert!(GROK_ADAPTER_BLOCK.contains("in complete sentences"));
        assert!(GROK_ADAPTER_BLOCK.contains("NEVER coin acronyms"));
        assert!(GROK_ADAPTER_BLOCK.contains("Always respond in Simplified Chinese"));
    }

    #[test]
    fn test_gpt_match_case_insensitive() {
        // 判定包含 gpt/openai/codex 或 o1/o3/o4 前缀
        assert!(is_gpt_upstream("GPT-5.6-terra"));
        assert!(is_gpt_upstream("gpt-5.6-terra"));
        assert!(is_gpt_upstream("openai/gpt-5.6"));
        assert!(is_gpt_upstream("codex-mini"));
        assert!(is_gpt_upstream("o3-mini"));
        assert!(is_gpt_upstream("o1-preview"));
        assert!(is_gpt_upstream("o4-high"));
        assert!(!is_gpt_upstream("claude-opus-5"));
        assert!(!is_gpt_upstream("grok-4.6"));
        assert!(!is_gpt_upstream("gemini-2.5-pro"));
    }

    #[test]
    fn test_gpt6_astra_match() {
        assert!(is_gpt6_astra("gpt-6-astra"));
        assert!(is_gpt6_astra("gpt-6"));
        assert!(is_gpt6_astra("openai/gpt-6-astra"));
        assert!(is_gpt6_astra("OPENAI/GPT-6_ASTRA"));
        assert!(is_gpt6_astra("gpt-6-astra-2026-09-01"));
        assert!(!is_gpt6_astra("gpt-5.6-terra"));
        assert!(!is_gpt6_astra("gpt-6-flash"));
        assert!(!is_gpt6_astra("gpt-60"));
        assert!(!is_gpt6_astra("grok-4.6"));
    }

    #[test]
    fn test_system_array_blocks_merged_to_instructions() {
        // system blocks 合并到 instructions 字段(对齐 codex base_instructions)
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "Block 1"},
                {"type": "text", "text": "Block 2"}
            ],
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["instructions"], "Block 1\n\nBlock 2");
        // input 应该为空（没有 developer message）
        assert_eq!(body["input"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_system_attribution_and_claude_identity_stripped() {
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: fp=abc"},
                {"type": "text", "text": "You are a Claude agent, built on Anthropic's Claude Agent SDK."},
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
                {"type": "text", "text": "Real"}
            ],
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        // attribution 与 Claude 身份句被过滤，只保留 "Real"
        assert_eq!(body["instructions"], "Real");
    }

    #[test]
    fn test_system_attribution_line_keeps_rest() {
        // 对齐 sub2api be4a4990:归属行与指令同块只删行(CRLF 行尾),非前导字面量不动
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: fp=abc\r\nKeep these instructions."},
                {"type": "text", "text": "Explain this metadata: x-anthropic-billing-header: fp=abc"}
            ],
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(
            body["instructions"],
            "Keep these instructions.\n\nExplain this metadata: x-anthropic-billing-header: fp=abc"
        );
    }

    #[test]
    fn test_claude_target_keeps_claude_identity() {
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}
            ],
            "messages": []
        });

        convert_to_openai_responses(&mut body, "claude-opus-5").unwrap();

        assert_eq!(
            body["instructions"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
    }

    #[test]
    fn test_system_whitespace_only_blocks_dropped() {
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "  \n  "},
                {"type": "text", "text": "Content"},
                {"type": "text", "text": ""},
            ],
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        // 纯空白块被 trim 后丢弃
        assert_eq!(body["instructions"], "Content");
    }

    #[test]
    fn test_system_blocks_trimmed_before_join() {
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "  Block 1  "},
                {"type": "text", "text": "\n\nBlock 2\n"},
            ],
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        // 每个块先 trim 再用 \n\n 连接
        assert_eq!(body["instructions"], "Block 1\n\nBlock 2");
    }

    #[test]
    fn test_tool_name_shortened_with_unique_suffix() {
        // 超长名截断;冲突加 _N(对齐 buildShortNameMap)
        let a = "a".repeat(64) + "X";
        let b = "a".repeat(64) + "Y";
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [
                {"name": a.clone(), "description": "d", "input_schema": {"type": "object"}},
                {"name": b.clone(), "description": "d", "input_schema": {"type": "object"}},
                {"name": "short_tool", "description": "d", "input_schema": {"type": "object"}}
            ]
        });
        let rev = convert_to_openai_responses(&mut body, "test-model").unwrap();
        let sa = body["tools"][0]["name"].as_str().unwrap();
        let sb = body["tools"][1]["name"].as_str().unwrap();
        assert_eq!(sa.len(), 64);
        assert_eq!(sb.len(), 64);
        assert_eq!(body["tools"][2]["name"], "short_tool");
        // 反向映射还原原名
        assert_eq!(rev[sa], a);
        assert_eq!(rev[sb], b);
    }

    #[test]
    fn test_tool_use_name_uses_short_name() {
        let long = "mcp__".to_string() + &"x".repeat(80);
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": long.clone(), "input": {}}
                ]}
            ],
            "tools": [{"name": long.clone(), "input_schema": {"type": "object"}}]
        });
        let rev = convert_to_openai_responses(&mut body, "test-model").unwrap();
        let short = body["tools"][0]["name"].as_str().unwrap();
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][0]["name"], short);
        assert_eq!(rev[short], long);
    }

    #[test]
    fn test_tool_schema_normalized() {
        // 空 schema → 默认 object;type 缺失补 object;object 无 properties 补 {};strict=false
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [
                {"name": "a", "input_schema": null},
                {"name": "b", "input_schema": {"properties": {"x": {"type": "string"}}}},
                {"name": "c", "input_schema": {"type": "object", "properties": {}}}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(
            body["tools"][0]["parameters"],
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(body["tools"][1]["parameters"]["type"], "object");
        assert_eq!(body["tools"][1]["strict"], false);
        assert!(
            body["tools"][1].get("input_schema").is_none(),
            "input_schema 应剥除"
        );
        assert_eq!(body["tools"][2]["strict"], false);
    }

    #[test]
    fn test_tool_schema_union_non_object_simplified() {
        // xAI 拒收非 object-only 的 root union:整体简化为安全 schema(对齐 CPA)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [
                {"name": "u", "input_schema": {
                    "type": "object",
                    "properties": {"v": {"type": "string"}},
                    "anyOf": [
                        {"type": "string"},
                        {"type": "object", "properties": {"a": {"type": "string"}}}
                    ]
                }}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(
            body["tools"][0]["parameters"],
            json!({"type": "object", "properties": {}, "additionalProperties": true})
        );
    }

    #[test]
    fn test_tool_schema_union_missing_type_filled() {
        // object-only union 分支缺 type → 补 "object",保留原 schema 语义(对齐 CPA)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [
                {"name": "u", "input_schema": {
                    "type": "object",
                    "properties": {"v": {"type": "string"}},
                    "oneOf": [
                        {"properties": {"a": {"type": "string"}}},
                        {"type": "object", "properties": {"b": {"type": "integer"}}}
                    ]
                }}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["tools"][0]["parameters"]["oneOf"][0]["type"], "object");
        assert_eq!(
            body["tools"][0]["parameters"]["properties"]["v"]["type"],
            "string"
        );
    }

    #[test]
    fn test_tool_schema_local_refs_inlined() {
        // 本地 $ref 内联,内联后删除 $defs(对齐 CPA normalizeXAITool InlineLocalRefs)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [
                {"name": "r", "input_schema": {
                    "type": "object",
                    "properties": {"user": {"$ref": "#/$defs/User"}},
                    "$defs": {"User": {
                        "type": "object",
                        "properties": {"name": {"type": "string"}}
                    }}
                }}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        let params = &body["tools"][0]["parameters"];
        assert!(params.get("$defs").is_none());
        assert_eq!(params["properties"]["user"]["type"], "object");
        assert_eq!(
            params["properties"]["user"]["properties"]["name"]["type"],
            "string"
        );
    }

    #[test]
    fn test_tool_schema_union_unresolved_ref_simplified() {
        // 无法内联的 $ref 分支不补 type,非 object-only → 整体简化(对齐 CPA)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [
                {"name": "r", "input_schema": {
                    "type": "object",
                    "oneOf": [
                        {"$ref": "#/$defs/Missing"},
                        {"type": "object", "properties": {"a": {"type": "string"}}}
                    ]
                }}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(
            body["tools"][0]["parameters"],
            json!({"type": "object", "properties": {}, "additionalProperties": true})
        );
    }

    #[test]
    fn test_custom_tool_round_trip() {
        // custom 工具声明保留 type;tool_use → custom_tool_call(input 解包);
        // tool_result → custom_tool_call_output
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "apply_patch",
                     "input": {"input": "patch-content"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
                ]}
            ],
            "tools": [
                {"type": "custom", "name": "apply_patch", "description": "d"}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        // 声明保持 custom,不套 input_schema
        assert_eq!(body["tools"][0]["type"], "custom");
        assert!(body["tools"][0].get("input_schema").is_none());
        // tool_use → custom_tool_call,字符串 input 解包
        assert_eq!(body["input"][0]["type"], "custom_tool_call");
        assert_eq!(body["input"][0]["name"], "apply_patch");
        assert_eq!(body["input"][0]["input"], "patch-content");
        // tool_result → custom_tool_call_output
        assert_eq!(body["input"][1]["type"], "custom_tool_call_output");
        assert_eq!(body["input"][1]["output"], "ok");
    }

    #[test]
    fn test_custom_tool_choice_mapping() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "tool", "name": "apply_patch"},
            "tools": [{"type": "custom", "name": "apply_patch", "description": "d"}]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["tool_choice"]["type"], "custom");
        assert_eq!(body["tool_choice"]["name"], "apply_patch");
    }

    #[test]
    fn test_regular_tool_unaffected_by_custom() {
        // 非 custom 工具仍走 function_call 路径
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "get_weather", "input": {"city": "bj"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "sunny"}
                ]}
            ],
            "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][1]["type"], "function_call_output");
    }

    #[test]
    fn test_tool_use_and_result() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "get_weather", "input": {"city": "beijing"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "sunny"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][0]["call_id"], "t1");
        assert_eq!(body["input"][0]["name"], "get_weather");
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][1]["output"], "sunny");
    }

    #[test]
    fn test_orphan_tool_result_becomes_user_text() {
        // 对齐 CPA 8c984672:无配对 tool_use 的 tool_result 不发孤儿
        // function_call_output,转 user text(防上游严格配对校验 400)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "gone", "content": "leftover output"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][0]["text"], "leftover output");
    }

    #[test]
    fn test_unmatched_explicit_call_id_not_rebound() {
        // 对齐 CPA 8c984672:显式 call_id 与 pending call 不匹配时,
        // 不得被改写/重绑,整体按 user text 发出
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t_other", "content": "other_result"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        let items = body["input"].as_array().unwrap();
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["call_id"], "t1");
        for item in items {
            assert_ne!(item["type"], "function_call_output", "{item}");
            if item["type"] == "message" {
                assert_eq!(item["content"][0]["text"], "other_result");
            }
        }
    }

    #[test]
    fn test_consecutive_assistant_turns_merged() {
        // 连续 assistant 消息(thinking + text/tool)合并为一条输入序列,
        // 保序:reasoning → message(output_text) → function_call。
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "t1"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "answer"},
                    {"type": "tool_use", "id": "c1", "name": "Read", "input": {"p": "a"}}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["content"], "t1");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["content"][0]["text"], "answer");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "c1");
    }

    #[test]
    fn test_gpt_unsigned_thinking_is_dropped() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "private trace"},
                    {"type": "text", "text": "answer"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-terra").unwrap();
        let input = body["input"].as_array().unwrap();
        // developer message + assistant message
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "developer"); // adapter block
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["content"][0]["text"], "answer");
        // 无签名 thinking 被丢弃(无 reasoning 项)
        assert!(!input.iter().any(|i| i["type"] == "reasoning"));
    }

    #[test]
    fn test_thinking_signature_gpt_compat_kept_else_dropped() {
        // 严格 Fernet 校验:gAAAA + 合法密文长度 → reasoning;其余首字节 → 丢弃
        const VALID: &str = "gAAAAAAAAAAAAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs8PT4_QA";
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t1", "signature": VALID},
                    {"type": "thinking", "thinking": "t2", "signature": "C4x2 weird"},
                    {"type": "text", "text": "answer"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-terra").unwrap();
        // input[0] = developer (adapter), input[1] = reasoning, input[2] = message
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][1]["type"], "reasoning");
        assert_eq!(body["input"][1]["encrypted_content"], VALID);
        assert_eq!(body["input"][1]["summary"], json!([]));
        assert_eq!(body["input"][2]["type"], "message");
        assert_eq!(body["input"][2]["content"][0]["text"], "answer");
    }

    #[test]
    fn test_thinking_grok_model_drops_foreign_envelope() {
        // grok 目标:Claude/GPT/Gemini 信封剥离,opaque 才回放
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t", "signature": "C4x2 opaque"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "grok-3").unwrap();
        let input = body["input"].as_array().unwrap();
        // grok 注入 developer message，外来信封被丢弃后只剩 developer
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "developer");
    }

    #[test]
    fn test_thinking_grok_model_drops_gpt_envelope() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t", "signature": "gAAAA-from-gpt"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "grok-3").unwrap();
        let input = body["input"].as_array().unwrap();
        // grok 注入 developer message，GPT 信封被丢弃后只剩 developer
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "developer");
    }

    #[test]
    fn test_thinking_grok_model_replays_opaque() {
        // 构造合法的高熵 Grok encrypted_content(standard base64, 无填充, 高熵)
        let mut high_entropy = vec![0u8; 64];
        for (i, byte) in high_entropy.iter_mut().enumerate() {
            *byte = (i * 13 % 256) as u8;
        }
        let valid_grok_sig = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&high_entropy);

        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t", "signature": valid_grok_sig}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "grok-3").unwrap();
        let input = body["input"].as_array().unwrap();
        // grok 注入 developer message，opaque 签名回放为 reasoning
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["encrypted_content"], valid_grok_sig);
    }

    #[test]
    fn test_grok_unsigned_thinking_is_dropped() {
        // grok 对齐 GPT:无签名 thinking 丢弃(明文 reasoning 无法缓存会导致死循环)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "private trace"},
                    {"type": "text", "text": "answer"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "grok-4.6").unwrap();
        let input = body["input"].as_array().unwrap();
        // grok 现在注入 developer message，input[0] 是 developer，input[1] 是 message
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["text"], "answer");
        // 无签名 thinking 被丢弃(无 reasoning 项)
        assert!(!input.iter().any(|i| i["type"] == "reasoning"));
    }

    #[test]
    fn test_trim_encrypted_reasoning_items() {
        let mut body = json!({
            "store": false,
            "input": [
                {"type": "reasoning", "id": "rs_1", "encrypted_content": "gAAAA-bad", "content": null},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "reasoning", "encrypted_content": "gAAAA-keep-text", "content": "think"}
            ]
        });
        assert!(trim_encrypted_reasoning_items(&mut body));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["content"], "think");
        assert!(input[1].get("encrypted_content").is_none());
        assert!(!trim_encrypted_reasoning_items(&mut body));
    }

    #[test]
    fn test_sanitize_gpt_reasoning_items() {
        const VALID: &str = "gAAAAAAAAAAAAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs8PT4_QA";
        let mut body = json!({
            "store": false,
            "input": [
                {"type": "reasoning", "id": "rs_bad", "encrypted_content": "gAAAA-replay", "content": null, "summary": []},
                {"type": "reasoning", "id": "rs_text", "encrypted_content": " ", "content": "keep"},
                {"type": "reasoning", "id": "rs_summary", "encrypted_content": null, "summary": ["keep"]},
                {"type": "reasoning", "id": "rs_valid", "encrypted_content": VALID, "content": null, "summary": []},
                {"type": "reasoning", "id": "rs_orphan", "content": null, "summary": []},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
            ]
        });

        assert!(sanitize_gpt_reasoning_items(&mut body));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["content"], "keep");
        assert!(input[0].get("encrypted_content").is_none());
        assert!(input[0].get("id").is_none());
        assert_eq!(input[1]["summary"], json!(["keep"]));
        assert!(input[1].get("encrypted_content").is_none());
        assert!(input[1].get("id").is_none());
        assert_eq!(input[2]["encrypted_content"], VALID);
        assert_eq!(input[2]["id"], "rs_valid");
        assert_eq!(input[3]["type"], "message");
        assert!(!sanitize_gpt_reasoning_items(&mut body));
    }

    #[test]
    fn test_sanitize_gpt_reasoning_promotes_content_to_summary() {
        // 对齐 CPA:reasoning.content 数组(官方 Codex schema maxItems:0)清空;
        // summary 为空时 reasoning_text 先提升为 summary_text
        let mut body = json!({
            "store": false,
            "input": [
                {"type": "reasoning", "id": "rs_a", "content": [
                    {"type": "reasoning_text", "text": "明文思考"}
                ]},
                {"type": "reasoning", "content": [
                    {"type": "reasoning_text", "text": "保留"}
                ], "summary": [{"type": "summary_text", "text": "已有"}]},
                {"type": "reasoning", "content": [
                    {"type": "input_text", "text": "非 reasoning_text 不提升"}
                ]}
            ]
        });
        assert!(sanitize_gpt_reasoning_items(&mut body));
        let input = body["input"].as_array().unwrap();
        // item0:content 提升进 summary 后强制清空
        assert_eq!(
            input[0]["summary"],
            json!([{"type": "summary_text", "text": "明文思考"}])
        );
        assert_eq!(input[0]["content"], json!([]));
        // item1:summary 非空,不提升,仅清空 content
        assert_eq!(
            input[1]["summary"],
            json!([{"type": "summary_text", "text": "已有"}])
        );
        assert_eq!(input[1]["content"], json!([]));
        // item2:无 reasoning_text,summary 不动,content 仍清空
        assert!(input[2].get("summary").is_none());
        assert_eq!(input[2]["content"], json!([]));
    }

    #[test]
    fn test_sanitize_gpt_reasoning_keeps_id_with_store_true() {
        let mut body = json!({
            "store": true,
            "input": [{
                "type": "reasoning",
                "id": "rs_stored",
                "encrypted_content": null,
                "content": "keep"
            }]
        });

        assert!(sanitize_gpt_reasoning_items(&mut body));
        assert_eq!(body["input"][0]["id"], "rs_stored");
        assert!(body["input"][0].get("encrypted_content").is_none());
        assert_eq!(body["input"][0]["content"], "keep");
    }

    #[test]
    fn test_is_thinking_signature_invalid() {
        assert!(is_thinking_signature_invalid(
            br#"{"error":{"code":"invalid_encrypted_content","message":"bad"}}"#
        ));
        assert!(is_thinking_signature_invalid(
            br#"{"error":{"message":"Invalid signature in thinking block"}}"#
        ));
        assert!(is_thinking_signature_invalid(
            br#"{"error":{"message":"Could not decrypt"}}"#
        ));
        assert!(!is_thinking_signature_invalid(
            br#"{"error":{"message":"context_length_exceeded"}}"#
        ));
    }

    #[test]
    fn test_redacted_thinking_grok_model_skipped() {
        // grok 不回放 redacted_thinking(对齐 grok-build parse-only 丢弃)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "redacted_thinking", "data": "opaque_payload_xyz"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "grok-3").unwrap();
        let input = body["input"].as_array().unwrap();
        // grok 注入 developer message，redacted_thinking 被丢弃后只剩 developer
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "developer");
    }

    #[test]
    fn test_reasoning_effort_default_medium() {
        // 无 thinking → 默认 medium(对齐 reasoningEffort 初值)
        let mut body = json!({"model": "test", "messages": []});
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["reasoning"]["effort"], "medium");
    }

    #[test]
    fn test_gpt6_astra_effort_handling() {
        // 测试 Astra default effort 和 explicit effort
        struct Case {
            name: &'static str,
            body: Value,
            expected_effort: &'static str,
        }

        let cases = vec![
            Case {
                name: "default effort low",
                body: json!({"model": "test", "messages": []}),
                expected_effort: "low",
            },
            Case {
                name: "explicit effort high clamps to medium",
                body: json!({
                    "model": "test",
                    "output_config": {"effort": "high"},
                    "messages": []
                }),
                expected_effort: "medium",
            },
        ];

        for case in cases {
            let mut body = case.body.clone();
            convert_to_openai_responses_with(&mut body, "gpt-6-astra", &astra_registry()).unwrap();
            assert_eq!(
                body["reasoning"]["effort"], case.expected_effort,
                "Failed at case: {}",
                case.name
            );
        }
    }

    #[test]
    fn test_non_astra_gpt_default_effort_medium() {
        // 锁在 GPT 模型上:误把 is_gpt6_astra 写成 is_gpt_upstream 时,gpt-5.4 会变 low
        let mut body = json!({"model": "test", "messages": []});
        convert_to_openai_responses(&mut body, "gpt-5.4").unwrap();
        assert_eq!(body["reasoning"]["effort"], "medium");
    }

    #[test]
    fn test_force_effort_overrides_inbound_and_default() {
        // force_effort 固定档:显式 effort 与默认 effort 均改写为固定值
        let registry = vec![crate::thinking::ModelCapability {
            id: "gpt-6-astra".into(),
            reasoning_levels: vec!["low".into(), "medium".into()],
            force_effort: Some("low".into()),
        }];
        let mut explicit = json!({
            "model": "test",
            "output_config": {"effort": "high"},
            "messages": []
        });
        convert_to_openai_responses_with(&mut explicit, "gpt-6-astra", &registry).unwrap();
        assert_eq!(explicit["reasoning"]["effort"], "low");

        let mut no_effort = json!({"model": "test", "messages": []});
        convert_to_openai_responses_with(&mut no_effort, "gpt-6-astra", &registry).unwrap();
        assert_eq!(no_effort["reasoning"]["effort"], "low");
    }

    #[test]
    fn test_thinking_effort_mapping() {
        let mut body = json!({
            "model": "test",
            "thinking": {"type": "enabled", "budget_tokens": 8192},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["reasoning"]["effort"], "medium");
    }

    #[test]
    fn test_adaptive_effort_uses_output_config() {
        let mut body = json!({
            "model": "test",
            "thinking": {"type": "adaptive", "output_config": {"effort": "high"}},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn test_output_config_format_maps_to_text_format() {
        // output_config.format(json_schema)→ text.format(对齐 convertClaudeRequestToCodex)
        let mut body = json!({
            "model": "test",
            "output_config": {"format": {
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "properties": {"answer": {"type": "string"}},
                    "required": ["answer"],
                    "additionalProperties": false
                }
            }},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["text"]["format"]["type"], "json_schema");
        // name 缺省 → cli_proxy_structured_output;strict 缺省 → true
        assert_eq!(
            body["text"]["format"]["name"],
            "cli_proxy_structured_output"
        );
        assert_eq!(body["text"]["format"]["strict"], true);
        assert_eq!(
            body["text"]["format"]["schema"]["properties"]["answer"]["type"],
            "string"
        );
    }

    #[test]
    fn test_output_config_format_custom_name_and_strict_false() {
        let mut body = json!({
            "model": "test",
            "output_config": {"format": {
                "type": "json_schema",
                "name": "custom_schema",
                "strict": false,
                "schema": {"type": "object"}
            }},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["text"]["format"]["name"], "custom_schema");
        assert_eq!(body["text"]["format"]["strict"], false);
    }

    #[test]
    fn test_output_config_format_optional_property_downgrades_strict() {
        let mut body = json!({
            "model": "test",
            "output_config": {"format": {
                "type": "json_schema",
                "name": "cli_proxy_structured_output",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": {
                        "answer": {"type": "string"},
                        "impossible": {"type": "string"}
                    },
                    "required": ["answer"],
                    "additionalProperties": false
                }
            }},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["text"]["format"]["strict"], false);
        assert_eq!(
            body["text"]["format"]["name"],
            "cli_proxy_structured_output"
        );
    }

    #[test]
    fn test_output_config_without_format_no_text() {
        // 仅 effort / 缺 format 且非 GPT → 不发 text(对齐 CPA effort-only 子例)
        let mut body = json!({
            "model": "test",
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert!(body.get("text").is_none());
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn test_gpt_upstream_strips_claude_triggers() {
        // GPT 上游清洗 system:保留 Memory/Environment/Language,丢弃 identity/concise-style/agent-types/产品宣传
        let mut body = json!({
            "model": "test",
            "system": r#"
Available agent types for the Agent tool:
- claude: Catch-all for any task
- Explore: Read-only search agent

<identity>
You are Claude, Anthropic's AI assistant.
</identity>

# Memory
Memory path: /home/user/.claude/memory

# Environment
Working directory: /project
Shell: bash
- Claude Code is available as a CLI in the terminal, desktop app (Mac/Windows), web app (claude.ai/code), and IDE extensions (VS Code, JetBrains).
- Fast mode for Claude Code uses Claude Opus with faster output (it does not downgrade to a smaller model). It can be toggled with /fast and is available on Opus 5/4.8.

<response_style>
Be very verbose and explain everything in detail.
</response_style>

# Output Style: Concise
Keep responses short.

# Language
Always respond in Simplified Chinese (简体中文).

<safety_guardrails>
Consider the reversibility and potential impact of your actions.
Scale your caution to the potential impact:
- Low-risk: proceed without hesitation
- High-risk: explain and wait for confirmation
</safety_guardrails>

IMPORTANT: Assist with authorized security testing, defensive security, CTF challenges.
"#,
            "messages": [{"role": "user", "content": "test"}]
        });
        convert_to_openai_responses(&mut body, "gpt-5.4").unwrap();

        let dev_text = body["input"][0]["content"][0]["text"].as_str().unwrap();

        // adapter 前缀存在
        assert!(dev_text.starts_with("You are Codex, based on GPT-5."));

        // 保留的块
        assert!(dev_text.contains("# Memory"));
        assert!(dev_text.contains("Memory path: /home/user/.claude/memory"));
        assert!(dev_text.contains("# Environment"));
        assert!(dev_text.contains("Working directory: /project"));
        assert!(dev_text.contains("# Language"));
        assert!(dev_text.contains("Simplified Chinese"));

        // 剥离的块
        assert!(!dev_text.contains("Available agent types"));
        assert!(!dev_text.contains("- claude: Catch-all"));
        assert!(!dev_text.contains("<identity>"));
        assert!(!dev_text.contains("Claude, Anthropic's AI assistant"));
        assert!(!dev_text.contains("<response_style>"));
        assert!(!dev_text.contains("verbose and explain everything"));
        assert!(!dev_text.contains("# Output Style: Concise"));
        assert!(!dev_text.contains("Keep responses short"));
        assert!(!dev_text.contains("<safety_guardrails>"));
        assert!(!dev_text.contains("Consider the reversibility and potential impact"));
        assert!(!dev_text.contains("IMPORTANT: Assist with authorized security testing"));
        assert!(!dev_text.contains("Claude Code is available as a CLI"));
        assert!(!dev_text.contains("Fast mode for Claude Code"));
    }

    #[test]
    fn test_gpt_strip_preserves_claudemd_and_memory() {
        // 确保 CLAUDE.md 与 memory 内容不被剥离
        let mut body = json!({
            "model": "test",
            "system": r#"
# claudeMd
Contents of /Users/user/.claude/CLAUDE.md:
Project-specific instructions here.

Contents of /Users/user/.claude/memory/MEMORY.md:
- [fact1](fact1.md) — description

<identity>Claude branding</identity>
"#,
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_responses(&mut body, "o1-preview").unwrap();

        let dev_text = body["input"][0]["content"][0]["text"].as_str().unwrap();
        assert!(dev_text.contains("# claudeMd"));
        assert!(dev_text.contains("Contents of /Users/user/.claude/CLAUDE.md"));
        assert!(dev_text.contains("Project-specific instructions"));
        assert!(dev_text.contains("Contents of /Users/user/.claude/memory"));
        assert!(!dev_text.contains("<identity>"));
    }

    #[test]
    fn test_grok_upstream_also_strips_triggers() {
        // Grok 上游同样清洗 system,剥离触发块
        let mut body = json!({
            "model": "test",
            "system": r#"<identity>Claude</identity>
# Memory
Path: /memory

<response_style>
Be verbose.
</response_style>"#,
            "messages": [{"role": "user", "content": "test"}]
        });
        convert_to_openai_responses(&mut body, "grok-3.5").unwrap();

        let dev_text = body["input"][0]["content"][0]["text"].as_str().unwrap();
        assert!(dev_text.starts_with("You are operating inside Claude Code"));
        assert!(dev_text.contains("# Memory"));
        assert!(dev_text.contains("Path: /memory"));
        // 触发块被剥离
        assert!(!dev_text.contains("<identity>Claude</identity>"));
        assert!(!dev_text.contains("<response_style>"));
    }

    #[test]
    fn test_gpt_upstream_injects_adapter_block() {
        // GPT 上游注入 GPT_CODEX_ADAPTER_BLOCK 作为 developer message 前缀
        let mut body = json!({
            "model": "test",
            "system": "Custom system instructions",
            "messages": [{"role": "user", "content": "hello"}]
        });
        convert_to_openai_responses(&mut body, "gpt-5.4").unwrap();

        assert_eq!(body["instructions"], "");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "developer");
        let dev_text = input[0]["content"][0]["text"].as_str().unwrap();
        assert!(dev_text.starts_with("You are Codex, based on GPT-5."));
        assert!(dev_text.contains("Custom system instructions"));
    }

    #[test]
    fn test_gpt6_astra_injects_adapter_block() {
        let mut body = json!({
            "model": "test",
            "system": "Custom system instructions",
            "messages": [{"role": "user", "content": "hello"}]
        });
        convert_to_openai_responses(&mut body, "gpt-6-astra").unwrap();

        assert_eq!(body["instructions"], "");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "developer");
        let dev_text = input[0]["content"][0]["text"].as_str().unwrap();
        assert!(dev_text.starts_with("You are Codex, an agent based on GPT-6."));
        assert!(dev_text.contains("Claude Code CLI"));
        assert!(dev_text.contains("persist across turns"));
        assert!(dev_text.contains("Do not stop at acknowledging capability"));
        assert!(dev_text.contains("Compaction does not end the task"));
        assert!(dev_text.contains("X, not Y"));
        assert!(dev_text.contains("Read/Edit/Write"));
        assert!(dev_text.contains("Custom system instructions"));
        assert!(!dev_text.contains("based on GPT-5."));
        assert!(!dev_text.contains("functions.exec"));
        assert!(!dev_text.contains("commentary"));
    }

    #[test]
    fn test_gpt6_alias_uses_astra_adapter() {
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_responses(&mut body, "gpt-6").unwrap();
        let dev_text = body["input"][0]["content"][0]["text"].as_str().unwrap();
        assert!(dev_text.starts_with("You are Codex, an agent based on GPT-6."));
    }

    #[test]
    fn test_gpt_upstream_adapter_with_empty_system() {
        // GPT 上游空 system 时只发 adapter block（无多余换行）
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": "test"}]
        });
        convert_to_openai_responses(&mut body, "o1-preview").unwrap();

        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "developer");
        let dev_text = input[0]["content"][0]["text"].as_str().unwrap();
        assert!(dev_text.starts_with("You are Codex"));
        // adapter block 本身包含多个 \n\n 分隔的段落，这是正常的
        // 只要不是在末尾追加了额外的 "\n\n" + 空 system 即可
        assert!(!dev_text.ends_with("\n\n"));
    }

    #[test]
    fn test_grok_upstream_still_injects_grok_adapter() {
        // Grok 上游仍然注入 GROK_ADAPTER_BLOCK(无回归)
        let mut body = json!({
            "model": "test",
            "system": "Test system",
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_responses(&mut body, "grok-3.5").unwrap();

        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "developer");
        let dev_text = input[0]["content"][0]["text"].as_str().unwrap();
        assert!(dev_text.starts_with("You are operating inside Claude Code's agent loop"));
        assert!(dev_text.contains("Test system"));
    }

    #[test]
    fn test_non_adapter_upstream_uses_instructions() {
        // 非 adapter 上游(glm/deepseek)保持 system → instructions
        let mut body = json!({
            "model": "test",
            "system": "System prompt",
            "messages": [{"role": "user", "content": "test"}]
        });
        convert_to_openai_responses(&mut body, "glm-5.1").unwrap();

        assert_eq!(body["instructions"], "System prompt");
        let input = body["input"].as_array().unwrap();
        // 首条消息应该是 user,不是 developer
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn test_gpt_upstream_injects_text_verbosity_low() {
        // GPT 目标且无 format 时自动注入 text.verbosity = "low"
        let mut body = json!({
            "model": "test",
            "messages": []
        });
        convert_to_openai_responses(&mut body, "gpt-5.4").unwrap();
        assert_eq!(body["text"]["verbosity"], "low");
        assert!(body["text"].get("format").is_none());

        // GPT 目标带有 format 时合入 verbosity
        let mut body = json!({
            "model": "test",
            "output_config": {"format": {
                "type": "json_schema",
                "schema": {"type": "object"}
            }},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "gpt-5.4").unwrap();
        assert_eq!(body["text"]["verbosity"], "low");
        assert_eq!(
            body["text"]["format"]["name"],
            "cli_proxy_structured_output"
        );
    }

    #[test]
    fn test_max_effort_downgraded_to_xhigh() {
        // glm-5.1 注册表支持到 xhigh,max 自动降级
        let mut body = json!({
            "model": "test",
            "output_config": {"effort": "max"},
            "thinking": {"type": "adaptive"},
            "messages": []
        });
        convert_to_openai_responses_with(&mut body, "glm-5.1", &glm51_registry()).unwrap();
        assert_eq!(body["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn test_service_tier_from_speed() {
        let mut body = json!({"model": "test", "messages": [], "speed": "fast"});
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["service_tier"], "priority");
    }

    #[test]
    fn test_service_tier_fast_is_priority() {
        let mut body = json!({"model": "test", "messages": [], "service_tier": "fast"});
        convert_to_openai_responses(&mut body, "gpt-5.6-terra").unwrap();
        assert_eq!(body["service_tier"], "priority");
    }

    #[test]
    fn test_effort_preserved_from_body() {
        // 保留入站 effort(output_config.effort 显式 high),对齐 codex compact.rs:704
        // 保留 turn_context.reasoning_effort,不因 compact 或其他路径强制覆盖
        let mut body = json!({
            "model": "test",
            "messages": [],
            "output_config": {"effort": "high"}
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-terra").unwrap();
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn test_effort_clamped_to_model() {
        // 超出模型支持上限时钳制到最近级别(glm-5.1 最高 xhigh,max 降为 xhigh)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "output_config": {"effort": "max"}
        });
        convert_to_openai_responses_with(&mut body, "glm-5.1", &glm51_registry()).unwrap();
        assert_eq!(body["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn test_codex_fixed_fields() {
        let mut body = json!({"model": "test", "messages": []});
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        // 无 stream 字段默认非流(对齐 Anthropic API 语义)
        assert_eq!(body["stream"], false);
        assert_eq!(body["store"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        // 无工具时三删,parallel_tool_calls 不发(对齐 CPA)
        assert!(body.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn test_parallel_tool_calls_disabled() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
            "tools": [{"name": "f", "input_schema": {"type": "object"}}]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["parallel_tool_calls"], false);
    }

    #[test]
    fn test_no_tools_drops_tool_fields() {
        // 无工具时三删(对齐 CPA normalizeXAIToolChoiceForTools)
        let mut body = json!({"model": "test", "messages": []});
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("parallel_tool_calls").is_none());

        // 空 tools 数组同样三删
        let mut body2 = json!({"model": "test", "messages": [], "tools": []});
        convert_to_openai_responses(&mut body2, "test-model").unwrap();
        assert!(body2.get("tools").is_none());
        assert!(body2.get("tool_choice").is_none());
        assert!(body2.get("parallel_tool_calls").is_none());

        // 有工具时保留三者
        let mut body3 = json!({
            "model": "test",
            "messages": [],
            "tools": [{"name": "search", "input_schema": {"type": "object"}}]
        });
        convert_to_openai_responses(&mut body3, "test-model").unwrap();
        assert!(body3.get("tools").is_some());
        assert_eq!(body3["tool_choice"], "auto");
        assert_eq!(body3["parallel_tool_calls"], true);
    }

    #[test]
    fn test_sampling_params_not_passed_through() {
        // 采样参数不透传(对齐 CPA:claude 入站不 preserve;
        // CC max_tokens=64000 透传超 grok 上限会 400 死循环)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "max_tokens": 64000,
            "temperature": 0.7,
            "top_p": 0.9,
            "top_k": 40
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("top_k").is_none());
    }

    #[test]
    fn test_pure_const_union_to_enum() {
        // ≥8 纯 const 分支(允许 description/title)→ 替换为 enum
        let branches: Vec<Value> = (0..9)
            .map(|i| json!({"const": i, "title": format!("t{i}")}))
            .collect();
        let mut params = json!({"properties": {"kind": {"oneOf": branches}}});
        simplify_pure_const_unions(&mut params);
        let prop = &params["properties"]["kind"];
        assert!(prop.get("oneOf").is_none());
        assert_eq!(prop["enum"], serde_json::json!([0, 1, 2, 3, 4, 5, 6, 7, 8]));
    }

    #[test]
    fn test_pure_const_union_guards() {
        let branches: Vec<Value> = (0..9)
            .map(|i| json!({"const": i, "title": format!("t{i}")}))
            .collect();
        // 分支数不足阈值:不动
        let mut params = json!({"properties": {"kind": {"oneOf": branches[..7].to_vec()}}});
        simplify_pure_const_unions(&mut params);
        assert!(params["properties"]["kind"].get("oneOf").is_some());

        // 分支含其他约束键(type):不动
        let mut guarded: Vec<Value> = (0..8)
            .map(|i| json!({"const": i, "type": "string"}))
            .collect();
        guarded[0] = json!({"const": 0, "type": "string"});
        let mut params = json!({"properties": {"kind": {"anyOf": guarded}}});
        simplify_pure_const_unions(&mut params);
        assert!(params["properties"]["kind"].get("anyOf").is_some());

        // 语义值重复(1.5 与 1.50 同值):不动
        let dup: Vec<Value> = (0..7)
            .map(|i| json!({"const": i}))
            .chain([json!({"const": "x"})])
            .chain([json!({"const": "x"})])
            .collect();
        let mut params = json!({"properties": {"kind": {"oneOf": dup}}});
        simplify_pure_const_unions(&mut params);
        assert!(params["properties"]["kind"].get("oneOf").is_some());

        // 同时带 oneOf 与 anyOf:不动
        let both = json!({"properties": {"kind": {
            "oneOf": branches.clone(),
            "anyOf": branches
        }}});
        let mut params = both;
        simplify_pure_const_unions(&mut params);
        assert!(params["properties"]["kind"].get("oneOf").is_some());
        assert!(params["properties"]["kind"].get("anyOf").is_some());
    }

    #[test]
    fn test_pure_const_union_existing_enum() {
        // 已有 enum 且与 union 语义一致:仅删 union
        let branches: Vec<Value> = (0..9).map(|i| json!({"const": i})).collect();
        let mut params = json!({"properties": {"kind": {
            "oneOf": branches,
            "enum": [8, 7, 6, 5, 4, 3, 2, 1, 0]
        }}});
        simplify_pure_const_unions(&mut params);
        let prop = &params["properties"]["kind"];
        assert!(prop.get("oneOf").is_none());
        assert_eq!(prop["enum"], serde_json::json!([8, 7, 6, 5, 4, 3, 2, 1, 0]));

        // enum 不一致:整组不动
        let branches: Vec<Value> = (0..9).map(|i| json!({"const": i})).collect();
        let mut params = json!({"properties": {"kind": {
            "oneOf": branches,
            "enum": [0, 1, 2, 3, 4, 5, 6, 7, 99]
        }}});
        simplify_pure_const_unions(&mut params);
        assert!(params["properties"]["kind"].get("oneOf").is_some());
    }

    #[test]
    fn test_pure_const_union_numeric_precision() {
        // 大整数分支逐字保留(arbitrary_precision,无精度损失)
        let raw = "9007199254740993";
        let branches: Vec<Value> = (0..8)
            .map(|i| {
                let token = if i == 7 {
                    raw.to_string()
                } else {
                    i.to_string()
                };
                serde_json::from_str::<Value>(&format!("{{\"const\": {token}}}")).unwrap()
            })
            .collect();
        let mut params = json!({"properties": {"id": {"oneOf": branches}}});
        simplify_pure_const_unions(&mut params);
        assert_eq!(
            params["properties"]["id"]["enum"][7].to_string(),
            raw.to_string()
        );
    }

    #[test]
    fn test_strict_only_when_simplified() {
        // strict 恒设 false(对齐 CPA ConvertClaudeRequestToCodex:
        // strict != false 即强制 false,claude 入站路径无 xAI 后处理层)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [{
                "name": "search",
                "input_schema": {
                    "type": "object",
                    "properties": {"q": {"type": "string"}}
                }
            }]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["tools"][0]["strict"], false);

        // root union 非 object-only 被简化:同样 strict=false
        let mut body2 = json!({
            "model": "test",
            "messages": [],
            "tools": [{
                "name": "search",
                "input_schema": {
                    "type": "object",
                    "anyOf": [{"type": "string"}, {"type": "object"}]
                }
            }]
        });
        convert_to_openai_responses(&mut body2, "test-model").unwrap();
        assert_eq!(body2["tools"][0]["strict"], false);
    }

    #[test]
    fn test_stop_removed() {
        // responses 不支持 stop,转换后删除(对齐 CPA sanitizeXAIResponsesBody)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "stop_sequences": ["\n\n"]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert!(body.get("stop").is_none());
    }

    #[test]
    fn test_tool_choice_mappings() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "any"},
            "tools": [{"name": "f", "input_schema": {"type": "object"}}]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["tool_choice"], "required");

        let mut body2 = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "tool", "name": "search"},
            "tools": [{"name": "search", "input_schema": {"type": "object"}}]
        });
        convert_to_openai_responses(&mut body2, "test-model").unwrap();
        assert_eq!(body2["tool_choice"]["type"], "function");
        assert_eq!(body2["tool_choice"]["name"], "search");

        // 命名 choice 指向未声明工具 → 降级 auto
        let mut body3 = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "tool", "name": "ghost"},
            "tools": [{"name": "search", "input_schema": {"type": "object"}}]
        });
        convert_to_openai_responses(&mut body3, "test-model").unwrap();
        assert_eq!(body3["tool_choice"], "auto");
    }

    #[test]
    fn test_web_search_tool_mapping() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [
                {"type": "web_search_20250305", "name": "web"},
                {"type": "function", "name": "f", "input_schema": {"type": "object"}}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["tools"][0]["type"], "web_search");
        assert_eq!(body["tools"][1]["type"], "function");
    }

    #[test]
    fn test_web_search_tool_choice() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "tool", "name": "web"},
            "tools": [{"type": "web_search_20250305", "name": "web"}]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["tool_choice"]["type"], "web_search");
    }

    /// 对齐 grok-build tool_overrides:xAI filters 用 excluded_domains,
    /// OpenAI 用 blocked_domains;allowed 与 blocked 并存按 allowed 优先
    #[test]
    fn test_web_search_blocked_domains_mapping() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [{"type": "web_search_20250305", "name": "web",
                        "blocked_domains": ["a.com", "b.com"]}]
        });
        convert_to_openai_responses(&mut body, "grok-4.6").unwrap();
        assert_eq!(body["tools"][0]["filters"]["excluded_domains"][0], "a.com");

        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [{"type": "web_search_20250305", "name": "web",
                        "blocked_domains": ["a.com"]}]
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-terra").unwrap();
        assert_eq!(body["tools"][0]["filters"]["blocked_domains"][0], "a.com");

        // 并存:allowed 优先,不产出 excluded/blocked
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [{"type": "web_search_20250305", "name": "web",
                        "allowed_domains": ["x.com"], "blocked_domains": ["a.com"]}]
        });
        convert_to_openai_responses(&mut body, "grok-4.6").unwrap();
        assert_eq!(body["tools"][0]["filters"]["allowed_domains"][0], "x.com");
        assert!(body["tools"][0]["filters"]
            .get("excluded_domains")
            .is_none());
    }

    #[test]
    fn test_system_role_message_reminder() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "system", "content": "procedural note"},
                {"role": "user", "content": "hi"}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "user");
        assert!(body["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("<system-reminder>"));
    }

    #[test]
    fn test_image_block_to_data_url() {
        let mut body = json!({
            "model": "test",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image",
                    "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}
                }]
            }]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(
            body["input"][0]["content"][0]["image_url"],
            "data:image/png;base64,aGVsbG8="
        );
    }

    #[test]
    fn test_document_block_to_input_file() {
        // 对齐 CPA TestConvertClaudeRequestToCodex_PreservesBase64PDFDocumentContent
        let mut body = json!({
            "model": "test",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "before"},
                    {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0xLjQK"}},
                    {"type": "text", "text": "after"}
                ]
            }]
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-sol").unwrap();
        // input[0] = developer (adapter), input[1] = user message
        let content = body["input"][1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[0]["text"], "before");
        assert_eq!(content[1]["type"], "input_file");
        assert_eq!(
            content[1]["file_data"],
            "data:application/pdf;base64,JVBERi0xLjQK"
        );
        assert_eq!(content[1]["filename"], "document.pdf");
        assert_eq!(content[2]["type"], "input_text");
        assert_eq!(content[2]["text"], "after");
    }

    #[test]
    fn test_document_non_pdf_or_non_base64_ignored() {
        let mut body = json!({
            "model": "test",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "document", "source": {"type": "url", "url": "https://example.com/doc.pdf"}},
                    {"type": "document", "source": {"type": "base64", "media_type": "text/plain", "data": "aGVsbG8="}},
                    {"type": "text", "text": "only text"}
                ]
            }]
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-sol").unwrap();
        // input[0] = developer, input[1] = user message
        let content = body["input"][1]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[0]["text"], "only text");
    }

    #[test]
    fn test_tool_result_array_output() {
        let mut body = json!({
            "model": "test",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": [
                        {"type": "text", "text": "result"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}}
                    ]
                }]
            }]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        // 孤儿(无 tool_use):文本转 user text,图片进随后的 user message
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][0]["text"], "result");
        assert_eq!(body["input"][1]["type"], "message");
        assert_eq!(body["input"][1]["role"], "user");
        let parts = body["input"][1]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "input_image");
    }

    #[test]
    fn test_tool_result_multi_text_array_joined() {
        let mut body = json!({
            "model": "test",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": [
                        {"type": "text", "text": "line1"},
                        {"type": "text", "text": "line2"}
                    ]
                }]
            }]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["input"][0]["type"], "message");
        let parts = body["input"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "line1");
        assert_eq!(parts[1]["text"], "line2");
    }

    #[test]
    fn test_tool_result_image_only_orphan_dropped() {
        // 只有图片的孤儿 tool_result:无文本 part 不发独立项,
        // 图片仍进随后的 user message
        let mut body = json!({
            "model": "test",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": [
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}}
                    ]
                }]
            }]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_image");
    }

    #[test]
    fn test_long_call_id_shortened() {
        let long_id = "toolu_".to_string() + &"x".repeat(80);
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": long_id.clone(), "name": "f", "input": {}}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert!(body["input"][0]["call_id"].as_str().unwrap().len() <= 64);
    }

    #[test]
    fn test_empty_and_null_messages_dropped() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": null},
                {"role": "user"},
                {"role": "user", "content": "keep"}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"][0]["text"], "keep");
    }

    #[test]
    fn test_empty_string_content_dropped_in_responses() {
        // 对齐 extractStandardInputTextContent:空串不产出 message item
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": ""},
                {"role": "assistant", "content": ""},
                {"role": "user", "content": "real"}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1, "空串 message 应被跳过");
        assert_eq!(input[0]["content"][0]["text"], "real");
    }

    #[test]
    fn test_empty_array_content_no_output() {
        // 空 content 数组 → 经 flush_message 检查后无 item 产出
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": []},
                {"role": "user", "content": "keep"}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"][0]["text"], "keep");
    }

    #[test]
    fn test_message_flushed_before_function_call() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": "let me check"},
                    {"type": "tool_use", "id": "t1", "name": "search", "input": {}}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"][0]["text"], "let me check");
        assert_eq!(body["input"][1]["type"], "function_call");
    }

    #[test]
    fn test_redacted_thinking_non_grok_skipped() {
        // 非 grok 同样不回放 redacted_thinking(对齐 grok-build parse-only 丢弃)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "redacted_thinking", "data": "opaque_payload_xyz"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let input = body["input"].as_array().unwrap();
        // 只有 developer message (adapter)
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "developer");
    }

    #[test]
    fn test_redacted_thinking_empty_data_skipped() {
        // redacted_thinking 块跳过后,后续 text 块仍正常处理为 message
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "redacted_thinking", "data": ""},
                    {"type": "text", "text": "result"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        // input[0] = developer, input[1] = message
        assert_eq!(body["input"].as_array().unwrap().len(), 2);
        assert_eq!(body["input"][1]["type"], "message");
        assert_eq!(body["input"][1]["content"][0]["text"], "result");
    }

    #[test]
    fn test_redacted_thinking_user_role_ignored() {
        // user 角色的 redacted_thinking 块应被忽略（只处理 assistant）
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": [
                    {"type": "redacted_thinking", "data": "should_ignore"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let input = body["input"].as_array().unwrap();
        // 只有 developer message
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "developer");
    }

    #[test]
    fn test_grok_rejects_foreign_signatures() {
        // Grok 目标应拒绝 GPT/Claude/Gemini 签名(防止上游 400)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t1", "signature": "gAAAA-gpt-fernet"},
                    {"type": "thinking", "thinking": "t2", "signature": "Cais-claude"},
                    {"type": "thinking", "thinking": "t3", "signature": "Egemini"},
                    {"type": "text", "text": "answer"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "grok-4.6").unwrap();
        let input = body["input"].as_array().unwrap();
        // grok 注入 developer,message 包含 text 内容,所有外来签名被过滤
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["text"], "answer");
        // 无 reasoning 项(签名全部过滤)
        assert!(!input.iter().any(|i| i["type"] == "reasoning"));
    }

    #[test]
    fn test_gpt_compatible_signature() {
        // 对齐 CPA codex_claude_request.appendReasoningContent
        const VALID: &str = "gAAAAAAAAAAAAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs8PT4_QA";
        // GPT 目标:Fernet 形状通过,provider 前缀被剥掉
        assert_eq!(
            gpt_compatible_signature(Some(VALID), "gpt-5"),
            Some(VALID.to_string())
        );
        let prefixed = format!("gpt#{VALID}");
        assert_eq!(
            gpt_compatible_signature(Some(&prefixed), "gpt-5"),
            Some(VALID.to_string())
        );
        // 非 GPT 信封、空签名、缺字段一律丢弃
        assert_eq!(gpt_compatible_signature(Some("Cais-claude"), "gpt-5"), None);
        assert_eq!(gpt_compatible_signature(Some("gAAAA-short"), "gpt-5"), None);
        assert_eq!(gpt_compatible_signature(Some(""), "gpt-5"), None);
        assert_eq!(gpt_compatible_signature(None, "gpt-5"), None);
        // grok 目标:无信封高熵 blob 通过
        let mut opaque = vec![0u8; 64];
        for (i, byte) in opaque.iter_mut().enumerate() {
            *byte = (i * 7 % 256) as u8;
        }
        let grok = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&opaque);
        assert_eq!(
            gpt_compatible_signature(Some(&grok), "grok-4.6"),
            Some(grok.clone())
        );
        // CPA 先无条件试 GPT 兼容,再看 grok:合法 Fernet 对 grok 目标同样保留
        assert_eq!(
            gpt_compatible_signature(Some(VALID), "grok-beta"),
            Some(VALID.to_string())
        );
        // 非 grok 目标不接受无信封 blob
        assert_eq!(gpt_compatible_signature(Some(&grok), "gpt-5"), None);
    }

    #[test]
    fn test_responses_tool_pairing_across_system_message() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "f1", "input": {}},
                    {"type": "tool_use", "id": "t2", "name": "f2", "input": {}}
                ]},
                {"role": "system", "content": "Usage note: 50% remaining"},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t2", "content": "res2"},
                    {"type": "tool_result", "tool_use_id": "t1", "content": "res1"},
                    {"type": "text", "text": "next instruction"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        let input = body["input"].as_array().unwrap();
        // 验证时序:
        // 0: function_call(t1)
        // 1: function_call(t2)
        // 2: function_call_output(t1) - 对齐重排到前
        // 3: function_call_output(t2)
        // 4: message(user, system-reminder) - 延后发射
        // 5: message(user, next instruction) - 正文
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "t1");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "t2");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "t1");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "t2");
        assert_eq!(input[4]["type"], "message");
        assert_eq!(input[4]["role"], "user");
        assert!(input[4]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("<system-reminder>"));
        assert_eq!(input[5]["type"], "message");
        assert_eq!(input[5]["role"], "user");
        assert_eq!(input[5]["content"][0]["text"], "next instruction");
    }

    #[test]
    fn test_strip_dialect_keywords_from_tool_schema() {
        // 对齐 CPA 7fac6b15:递归删除 $schema/$id
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "name": "example",
                "input_schema": {
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "$id": "https://example.com/schema",
                    "type": "object",
                    "properties": {
                        "nested": {
                            "$schema": "http://json-schema.org/draft-07/schema#",
                            "type": "string"
                        }
                    },
                    "anyOf": [{
                        "$id": "branch1",
                        "type": "object"
                    }]
                }
            }]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        let params = &body["tools"][0]["parameters"];
        assert!(params.get("$schema").is_none());
        assert!(params.get("$id").is_none());
        assert!(params["properties"]["nested"].get("$schema").is_none());
        assert!(params["anyOf"][0].get("$id").is_none());
    }

    #[test]
    fn test_tool_schema_union_type_array_keeps_properties() {
        // 对齐 CPA 7fac6b15:type 数组 ["object","null"] 保留,仅补 properties
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"name": "u", "input_schema": {"type": ["object", "null"]}},
                {"name": "s", "input_schema": {"type": "string"}}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        let union = &body["tools"][0]["parameters"];
        assert_eq!(union["type"], json!(["object", "null"]));
        assert_eq!(union["properties"], json!({}));
        // 字符串 type 非 object:type 原样,不补 properties
        let scalar = &body["tools"][1]["parameters"];
        assert_eq!(scalar["type"], "string");
        assert!(scalar.get("properties").is_none());
    }
