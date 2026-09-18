use super::*;
use super::compensations::*;
use super::state_machine::*;
use bytes::Bytes;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use crate::sse::parser::SseEvent;

fn s(bufs: &[Bytes]) -> String {
    String::from_utf8_lossy(&bufs.concat()).into_owned()
}
fn ev(data: &str) -> SseEvent {
    SseEvent {
        event: None,
        data: data.into(),
    }
}

fn created() -> SseEvent {
    ev(r#"{"type":"response.created","response":{"id":"r1","model":"gpt-5"}}"#)
}

/// 从 SSE 帧提取 data JSON
fn frame_data(frame: &Bytes) -> Value {
    let s = String::from_utf8_lossy(frame);
    let (_, rest) = s.split_once('\n').unwrap();
    serde_json::from_str(rest.strip_prefix("data: ").unwrap().trim()).unwrap()
}

#[test]
fn test_message_start_uses_estimated_when_upstream_silent() {
    // 上游未回真实 usage 时,message_start 用入站估算占位(context 不跳 1)
    let mut r = ResponsesRelay::new(Some(1234));
    let out = r.process(&created());
    let start = out
        .iter()
        .find(|b| b.starts_with(b"event: message_start"))
        .expect("message_start 应存在");
    let v = frame_data(start);
    assert_eq!(v["message"]["usage"]["input_tokens"], 1234);
    assert_eq!(v["message"]["usage"]["cache_read_input_tokens"], 0);
}

#[test]
fn test_responses_text_stream() {
    let mut r = ResponsesRelay::new(None);
    let out1 = r.process(&created());
    assert!(out1.iter().any(|b| b.starts_with(b"event: message_start")));

    let out2 = r.process(&ev(
        r#"{"type":"response.output_text.delta","delta":"hello"}"#,
    ));
    assert!(out2
        .iter()
        .any(|b| b.starts_with(b"event: content_block_start")));
    assert!(out2
        .iter()
        .any(|b| b.starts_with(b"event: content_block_delta")));
}

#[test]
fn test_responses_completed_with_tool_call() {
    let mut r = ResponsesRelay::new(None);
    r.process(&created());

    let out = r.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r1","output":[
            {"type":"function_call","id":"fc_1","call_id":"call_9","name":"get_weather","arguments":"{\"city\":\"beijing\"}","status":"completed"}
        ],"usage":{"input_tokens":10,"output_tokens":5}}}"#,
    ));
    assert!(out.iter().any(|b| b.starts_with(b"event: message_delta")));
    assert!(out.iter().any(|b| b.starts_with(b"event: message_stop")));
    // call_id 优先于 id(对齐 codexFunctionCallID)
    let start = out
        .iter()
        .find(|b| b.starts_with(b"event: content_block_start"))
        .unwrap();
    let s = String::from_utf8_lossy(start);
    assert!(s.contains("get_weather"));
    assert!(s.contains("call_9"));
    assert!(!s.contains("fc_1"));
    // 有工具调用 → stop_reason tool_use
    let delta = out
        .iter()
        .find(|b| b.starts_with(b"event: message_delta"))
        .unwrap();
    assert!(String::from_utf8_lossy(delta).contains("tool_use"));
}

#[test]
fn test_reasoning_replay_streaming_with_signature() {
    // 完整闭环:summary 流式可见 + encrypted_content 走 signature_delta
    let mut r = ResponsesRelay::new(None);
    r.process(&created());

    let out1 = r.process(&ev(
        r#"{"type":"response.reasoning_summary_text.delta","delta":"让我想想"}"#,
    ));
    let bufs1 = out1.concat();
    let s1 = String::from_utf8_lossy(&bufs1);
    assert!(s1.contains("content_block_start"));
    assert!(s1.contains("thinking_delta"));
    assert!(s1.contains("让我想想"));

    let out2 = r.process(&ev(
        r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"gAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}}"#,
    ));
    let bufs2 = out2.concat();
    let s2 = String::from_utf8_lossy(&bufs2);
    assert!(s2.contains("signature_delta"));
    assert!(s2.contains("gAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
    assert!(s2.contains("content_block_stop"));
}

#[test]
fn test_reasoning_signature_only() {
    // 无 summary 的 reasoning item:开块即收尾,仍带 signature
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    r.process(&ev(
        r#"{"type":"response.output_item.added","item":{"type":"reasoning"}}"#,
    ));
    let out = r.process(&ev(
        r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"gAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}}"#,
    ));
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    assert!(s.contains("\"type\":\"thinking\""));
    assert!(s.contains("signature_delta"));
    assert!(s.contains("gAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
}

#[test]
fn test_reasoning_grok_encrypted_content_signature() {
    // Grok 无信封密文: 应合法保留并发送 signature_delta
    const GROK_CIPHER: &str =
        "5+C3No7B0G0P2dR5lSbMvwrctRb+B3vLAJ5/VYemHZNuaDbKv8IfNb+4Gyd125rfZtRtG4+iqYjT5uWOZbE44A";
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    r.process(&ev(
        r#"{"type":"response.output_item.added","item":{"type":"reasoning"}}"#,
    ));
    let out = r.process(&ev(&format!(
        r#"{{"type":"response.output_item.done","item":{{"type":"reasoning","encrypted_content":"{GROK_CIPHER}"}}}}"#
    )));
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    assert!(s.contains("\"type\":\"thinking\""));
    assert!(s.contains("signature_delta"));
    assert!(s.contains(GROK_CIPHER));
}

#[test]
fn test_reasoning_plaintext_delta() {
    // 非订阅网关发明文 reasoning_text.delta(无 encrypted_content):内容进 thinking 块
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"response.reasoning_text.delta","delta":"明文推理"}"#,
    ));
    let so = s(&out);
    assert!(so.contains("content_block_start"));
    assert!(so.contains("thinking_delta"));
    assert!(so.contains("明文推理"));
    // done 发分隔符(对齐 CPA "\n\n");text 到来时正常收尾
    let done = r.process(&ev(
        r#"{"type":"response.reasoning_text.done","item":{"type":"reasoning"}}"#,
    ));
    let sd = s(&done);
    assert!(sd.contains("thinking_delta"), "done 应发分隔 delta");
    let text = r.process(&ev(
        r#"{"type":"response.output_text.delta","delta":"答案"}"#,
    ));
    let st = s(&text);
    assert!(st.contains("content_block_stop"));
    assert!(st.contains("text_delta"));
}

#[test]
fn test_content_part_added_reasoning_plaintext() {
    // content_part.added(part.type=reasoning):保持块打开,分隔符续接,不误关块
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    r.process(&ev(
        r#"{"type":"response.output_item.added","item":{"type":"reasoning"}}"#,
    ));
    let out = r.process(&ev(
        r#"{"type":"response.content_part.added","part":{"type":"reasoning"}}"#,
    ));
    let s1 = s(&out);
    assert!(
        s1.contains("content_block_start"),
        "明文 reasoning part 应开块,实际: {s1}"
    );
    let out1b = r.process(&ev(
        r#"{"type":"response.reasoning_text.delta","delta":"推理中"}"#,
    ));
    let s1b = s(&out1b);
    assert!(
        s1b.contains("thinking_delta"),
        "明文 delta 应进块,实际: {s1b}"
    );
    assert!(
        !s1b.contains("content_block_start"),
        "块应保持打开,实际: {s1b}"
    );
    let out2 = r.process(&ev(
        r#"{"type":"response.content_part.added","part":{"type":"output_text"}}"#,
    ));
    let s2 = s(&out2);
    assert!(
        s2.contains("content_block_stop"),
        "output_text part 到来应收尾块,实际: {s2}"
    );
    assert!(s2.contains("content_block_start"));
    assert!(s2.contains("text"));
}

#[test]
fn test_reasoning_summary_parts_separated() {
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    r.process(&ev(r#"{"type":"response.reasoning_summary_part.added"}"#));
    r.process(&ev(
        r#"{"type":"response.reasoning_summary_text.delta","delta":"A"}"#,
    ));
    let out = r.process(&ev(r#"{"type":"response.reasoning_summary_part.added"}"#));
    // 第二个 part:块保持打开,发空行分隔而非新块
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    assert!(s.contains("thinking_delta"));
    assert!(!s.contains("content_block_start"));
}

#[test]
fn test_usage_subtracts_cached_tokens() {
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r1","output":[],
            "usage":{"input_tokens":100,"output_tokens":5,
                     "input_tokens_details":{"cached_tokens":80}}}}"#,
    ));
    let delta = out
        .iter()
        .find(|b| b.starts_with(b"event: message_delta"))
        .unwrap();
    let v = frame_data(delta);
    assert_eq!(v["usage"]["input_tokens"], 20);
    assert_eq!(v["usage"]["cache_read_input_tokens"], 80);
}

#[test]
fn test_usage_maps_cache_write_tokens() {
    // 对齐 CPA 893abbab:cache_write_tokens → cache_creation_input_tokens,
    // 0 时不下发该键;Anthropic 互斥语义:input 扣除 cached 与 cache_write
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r1","output":[],
            "usage":{"input_tokens":100,"output_tokens":5,
                     "input_tokens_details":{"cached_tokens":80,"cache_write_tokens":15}}}}"#,
    ));
    let delta = out
        .iter()
        .find(|b| b.starts_with(b"event: message_delta"))
        .unwrap();
    let v = frame_data(delta);
    assert_eq!(v["usage"]["input_tokens"], 5); // 100 - 80 - 15
    assert_eq!(v["usage"]["cache_read_input_tokens"], 80);
    assert_eq!(v["usage"]["cache_creation_input_tokens"], 15);

    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r1","output":[],
            "usage":{"input_tokens":100,"output_tokens":5,
                     "input_tokens_details":{"cached_tokens":80,"cache_write_tokens":0}}}}"#,
    ));
    let delta = out
        .iter()
        .find(|b| b.starts_with(b"event: message_delta"))
        .unwrap();
    let v = frame_data(delta);
    assert_eq!(v["usage"]["input_tokens"], 20); // 100 - 80 - 0
    assert!(v["usage"].get("cache_creation_input_tokens").is_none());
}

#[test]
fn test_empty_output_synthesizes_text_block() {
    // 空轮次合成空 text 块(对齐 synthesizeCodexEmptyTextBlock)
    let mut r = ResponsesRelay::new(None);
    let out = r.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#,
    ));
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    assert!(s.contains("message_stop"));
    assert!(s.contains("\"type\":\"text\""));
}

#[test]
fn test_terminal_text_recovered_from_completed_output() {
    // 正文只出现在 response.completed 的 output[].message(无任何 delta):
    // 补发文本,不再合成空块(对齐 sub2api resToAnthRecoverTerminalText)
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r1","output":[
            {"type":"message","role":"assistant","content":[
                {"type":"output_text","text":"最终答案"}
            ]}],"usage":{"input_tokens":10,"output_tokens":5}}}"#,
    ));
    let s = s(&out);
    assert!(s.contains("text_delta"), "应补发正文: {s}");
    assert!(s.contains("最终答案"));
    // 仅一个 text 块,不与合成空块叠加
    assert_eq!(s.matches("event: content_block_start").count(), 1);

    // 已流式发出过文本则不重复补发
    let mut r2 = ResponsesRelay::new(None);
    r2.process(&created());
    r2.process(&ev(
        r#"{"type":"response.output_text.delta","delta":"流式"}"#,
    ));
    let out2 = r2.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r2","output":[
            {"type":"message","role":"assistant","content":[
                {"type":"output_text","text":"流式+终态"}
            ]}]}}"#,
    ));
    let buf2 = out2.concat();
    let s2 = String::from_utf8_lossy(&buf2);
    assert!(!s2.contains("终态"), "已发 delta 不得重复: {s2}");
}

#[test]
fn test_thinking_only_turn_synthesizes_text_block() {
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    r.process(&ev(
        r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"E"}}"#,
    ));
    let out = r.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#,
    ));
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    assert!(s.contains("\"type\":\"text\""));
}

#[test]
fn test_stream_error_event() {
    let mut r = ResponsesRelay::new(None);
    let out = r.process(&ev(
        r#"{"type":"error","error":{"type":"invalid_request","code":"cyber_policy","message":"blocked"}}"#,
    ));
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    assert!(s.starts_with("event: error"));
    assert!(s.contains("invalid_request_error"));
    assert!(s.contains("blocked"));
}

#[test]
fn test_response_failed_event() {
    let mut r = ResponsesRelay::new(None);
    let out = r.process(&ev(
        r#"{"type":"response.failed","sequence_number":0,"response":{"status":"failed","error":{"type":"server_error","code":"internal_server_error","message":"boom"}}}"#,
    ));
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    assert!(s.starts_with("event: error"));
    assert!(s.contains("server_error"));
    assert!(s.contains("boom"));
}

#[test]
fn test_response_failed_during_function_call_is_immediate() {
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    r.process(&ev(
        r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"get_weather","output_index":0}}"#,
    ));
    let out = r.process(&ev(
        r#"{"type":"response.failed","response":{"status":"failed","error":{"type":"server_error","message":"boom"}}}"#,
    ));
    let s = s(&out);
    assert!(s.starts_with("event: error"), "失败事件不得延迟,实际: {s}");
    assert!(s.contains("server_error"), "应保留上游错误类型,实际: {s}");
    assert!(s.contains("boom"), "应保留上游错误消息,实际: {s}");
    assert!(r.finish().is_empty(), "失败后 finish 应为空");
}

#[test]
fn test_error_event_stops_followup_events() {
    // 上游 error 后仍发正常事件(部分网关 overloaded 后混续):只保留 error,
    // 不得再产出 text/delta/stop 混合流,否则 SDK 判 "malformed"。
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"error","error":{"type":"overloaded_error","message":"servers overloaded"}}"#,
    ));
    assert!(out.iter().any(|b| b.starts_with(b"event: error")));
    // 后续 completed 等事件一律忽略
    let after = r.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#,
    ));
    assert!(after.is_empty(), "error 后不得继续处理,实际: {after:?}");
    // finish 兜底也不得再产 message_delta/stop
    let fin = r.finish();
    assert!(fin.is_empty(), "error 后 finish 应为空,实际: {fin:?}");
}

#[test]
fn test_doom_loop_confident_signal_aborts_stream() {
    // 置信信号(thinking channel tail_repetition ≤ 64):吞掉事件 + 发 error 中断
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:32@thinking"]}}"#,
    ));
    let s = s(&out);
    assert!(
        s.starts_with("event: error"),
        "置信信号应发 error 中断,实际: {s}"
    );
    assert!(
        s.contains("doom loop detected"),
        "error 应含 doom loop 信息,实际: {s}"
    );
    // 中断后后续事件一律忽略
    let after = r.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#,
    ));
    assert!(after.is_empty(), "中断后不得继续处理,实际: {after:?}");
    assert!(
        r.finish().is_empty(),
        "中断后 finish 应为空,实际: {:?}",
        r.finish()
    );
}

#[test]
fn test_doom_loop_non_confident_signal_passes_through() {
    // 非置信信号(response channel):吞掉事件,流照常
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:32@response"]}}"#,
    ));
    assert!(out.is_empty(), "非置信信号不应产出任何帧,实际: {out:?}");
    // 流继续正常处理
    let text = r.process(&ev(r#"{"type":"response.output_text.delta","delta":"ok"}"#));
    assert!(s(&text).contains("text_delta"));
}

#[test]
fn test_doom_loop_terminal_field_confident() {
    // 终态响应对象上的 doom_loop_check 字段:置信时中断而非正常收尾
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r1","output":[],"doom_loop_check":{"triggers":["tail_repetition:16@thinking"]}}}"#,
    ));
    let s = s(&out);
    assert!(
        s.starts_with("event: error"),
        "终态置信信号应发 error,实际: {s}"
    );
    assert!(!s.contains("message_stop"), "不得正常收尾,实际: {s}");
}

#[test]
fn test_doom_loop_dedup_repeated_triggers() {
    // 服务端重发累计集:同 label 去重,不重复判定
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    // 第一次:非置信
    r.process(&ev(
        r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:32@response"]}}"#,
    ));
    // 第二次重发同 label:仍非置信,流照常
    let out = r.process(&ev(
        r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:32@response"]}}"#,
    ));
    assert!(out.is_empty(), "重复 label 应去重,实际: {out:?}");
}

#[test]
fn test_doom_loop_malformed_payload_never_fails() {
    // malformed payload:best-effort,不弄挂流
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":[123,null]}}"#,
    ));
    assert!(
        out.is_empty(),
        "malformed 不应产出帧也不应报错,实际: {out:?}"
    );
    // 流继续正常
    let text = r.process(&ev(r#"{"type":"response.output_text.delta","delta":"ok"}"#));
    assert!(s(&text).contains("text_delta"));
}

#[test]
fn test_stop_reason_mappings() {
    assert_eq!(map_stop_reason("content_filter", false), "refusal");
    assert_eq!(map_stop_reason("max_output_tokens", false), "max_tokens");
    assert_eq!(map_stop_reason("max_prompt_tokens", false), "max_tokens");
    assert_eq!(map_stop_reason("max_time_limit", false), "max_tokens");
    assert_eq!(map_stop_reason("", false), "end_turn");
    assert_eq!(map_stop_reason("stop", true), "tool_use");
    assert_eq!(map_stop_reason("pause_turn", false), "pause_turn");
    let r = json!({"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}});
    assert_eq!(codex_stop_reason(&r), "max_output_tokens");
    let r = json!({"status":"incomplete","incomplete_details":{"reason":"max_prompt_tokens"}});
    assert_eq!(codex_stop_reason(&r), "max_prompt_tokens");
    let r = json!({"status":"incomplete","incomplete_details":{"reason":"max_time_limit"}});
    assert_eq!(codex_stop_reason(&r), "max_time_limit");
    let r = json!({"stop_reason":"stop","stop_sequence":"END"});
    assert_eq!(codex_stop_reason(&r), "stop_sequence");
}

#[test]
fn test_stop_sequence_in_message_delta() {
    let mut r = ResponsesRelay::new(None);
    let out = r.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r1","output":[],
            "stop_reason":"stop","stop_sequence":"END"}}"#,
    ));
    let delta = out
        .iter()
        .find(|b| b.starts_with(b"event: message_delta"))
        .unwrap();
    let s = String::from_utf8_lossy(delta);
    assert!(s.contains("stop_sequence"));
    assert!(s.contains("END"));
}

#[test]
fn test_model_fallback() {
    let mut r = ResponsesRelay::new(None);
    let out = r.process(&ev(r#"{"type":"response.created","response":{"id":"r1"}}"#));
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    assert!(s.contains(FALLBACK_MODEL));
}

#[test]
fn test_message_item_text_fallback() {
    // 无 delta 流时从 output_item.done(message) 的 content 补发
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"response.output_item.done","item":{"type":"message",
            "content":[{"type":"output_text","text":"补发文本"}]}}"#,
    ));
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    assert!(s.contains("补发文本"));
    assert!(s.contains("content_block_stop"));
}

#[test]
fn test_finish_eof_after_completed_keeps_normal() {
    // 正常 completed 后 finish 幂等,不再产出(已完成流)
    let mut r = ResponsesRelay::new(None);
    r.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#,
    ));
    assert!(r.finish().is_empty());
}

#[test]
fn test_finish_eof_without_completed_errors() {
    // 残缺流:已发 content delta 但未 completed 就 EOF,显式报 error,不包装成正常收尾
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    r.process(&ev(
        r#"{"type":"response.output_text.delta","delta":"partial"}"#,
    ));
    let out = r.finish();
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    assert!(s.contains("event: error"), "残缺流应发 error 帧,实际: {s}");
    assert!(
        !s.contains("message_delta"),
        "残缺流不得发正常 message_delta,实际: {s}"
    );
    assert!(
        !s.contains("message_stop"),
        "残缺流不得发正常 message_stop,实际: {s}"
    );
    // 二次 finish 幂等
    assert!(r.finish().is_empty());
}

#[test]
fn test_finish_eof_empty_stream_errors() {
    // 空流:一个事件都没有,EOF 直接报 error
    let mut r = ResponsesRelay::new(None);
    let out = r.finish();
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    assert!(s.contains("event: error"), "空流应发 error 帧,实际: {s}");
    assert!(
        !s.contains("message_start"),
        "空流不得伪造 message_start,实际: {s}"
    );
    assert!(!s.contains("message_stop"));
}

#[test]
fn test_sanitize_tool_id() {
    assert_eq!(sanitize_tool_id("call_abc-123"), "call_abc-123");
    assert_eq!(sanitize_tool_id("a.b/c"), "a_b_c");
    let long = "x".repeat(100);
    assert_eq!(sanitize_tool_id(&long).len(), 64);
}

#[test]
fn test_function_call_streaming_full_lifecycle() {
    // 完整生命周期:added → args delta → args done → item done → start/stop
    let mut r = ResponsesRelay::new(None);
    r.process(&created());

    let out1 = r.process(&ev(
        r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"get_weather","output_index":0}}"#,
    ));
    let s1 = s(&out1);
    assert!(s1.contains("content_block_start"));
    assert!(s1.contains("\"name\":\"get_weather\""));
    assert!(s1.contains("\"id\":\"call_1\""));

    let out2 = r.process(&ev(
        r#"{"type":"response.function_call_arguments.delta","delta":"{\"cit"}"#,
    ));
    let s2 = s(&out2);
    assert!(!s2.contains("content_block_start"), "参数增量不应重复开块");
    assert!(s2.contains("input_json_delta"));
    assert!(s2.contains("{\\\"cit"));

    let out3 = r.process(&ev(
        r#"{"type":"response.function_call_arguments.done","call_id":"call_1","arguments":"{\"city\":\"beijing\"}"}"#,
    ));
    let s3 = s(&out3);
    assert!(s3.contains("input_json_delta"));
    assert!(s3.contains("beijing"));

    let out4 = r.process(&ev(
        r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"beijing\"}"}}"#,
    ));
    let s4 = s(&out4);
    assert!(s4.contains("content_block_stop"));
}

#[test]
fn test_function_call_tool_name_restored() {
    // 请求侧缩短的名,响应侧还原(对齐 buildReverseMap)
    let mut names = HashMap::new();
    names.insert(
        "mcp__short".to_string(),
        "mcp__very_long_original_name".to_string(),
    );
    let mut r = ResponsesRelay::new(None);
    r.tool_names = Some(Arc::new(names));
    r.process(&created());

    let out = r.process(&ev(
        r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_9","name":"mcp__short"}}"#,
    ));
    let s = s(&out);
    assert!(s.contains("mcp__very_long_original_name"));
    assert!(!s.contains("\"name\":\"mcp__short\""));
}

#[test]
fn test_function_call_multiple_queued_serially() {
    // 并发两个调用:串行输出,每个独立 start/delta/stop
    let mut r = ResponsesRelay::new(None);
    r.process(&created());

    r.process(&ev(
        r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"a","name":"tool_a","output_index":0}}"#,
    ));
    r.process(&ev(
        r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"b","name":"tool_b","output_index":1}}"#,
    ));
    let out = r.process(&ev(
        r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"a","name":"tool_a","arguments":"{}"}}"#,
    ));
    let s = s(&out);
    assert!(s.contains("tool_b"), "a 完成后应接着输出 b");
}

#[test]
fn test_response_completed_flushes_terminal_calls_with_deferred() {
    // 无流式事件只有 completed:从 response.output 补发工具调用
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r1","output":[
            {"type":"function_call","call_id":"call_9","name":"get_weather","arguments":"{\"city\":\"beijing\"}"}
        ],"usage":{"input_tokens":10,"output_tokens":5}}}"#,
    ));
    let s = s(&out);
    assert!(s.contains("content_block_start"));
    assert!(s.contains("get_weather"));
    assert!(s.contains("call_9"));
    assert!(s.contains("message_delta"));
    assert!(s.contains("tool_use"));
}

#[test]
fn test_custom_tool_call_streaming_full_lifecycle() {
    // custom 工具:added → input delta → input done → item done,
    // input 是字符串,包成 {"input": str} 发 tool_use(对齐响应转换 custom 分支)
    let mut r = ResponsesRelay::new(None);
    r.process(&created());

    let out1 = r.process(&ev(
        r#"{"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"call_c","name":"apply_patch","input":"","output_index":0}}"#,
    ));
    let s1 = s(&out1);
    assert!(s1.contains("content_block_start"));
    assert!(s1.contains("\"name\":\"apply_patch\""));
    assert!(s1.contains("\"id\":\"call_c\""));

    let out2 = r.process(&ev(
        r#"{"type":"response.custom_tool_call_input.delta","call_id":"call_c","delta":"*** Begin Patch\n"}"#,
    ));
    // delta 阶段不立即发流(一次性在 done 发)
    assert!(!s(&out2).contains("input_json_delta"));

    let out3 = r.process(&ev(
        r#"{"type":"response.custom_tool_call_input.done","call_id":"call_c","input":"*** Begin Patch\n+hello"}"#,
    ));
    assert!(!s(&out3).contains("input_json_delta"));

    let out4 = r.process(&ev(
        r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_c","name":"apply_patch","input":"*** Begin Patch\n+hello"}}"#,
    ));
    let s4 = s(&out4);
    assert!(s4.contains("input_json_delta"));
    assert!(s4.contains("{\\\"input\\\":\\\"*** Begin Patch"));
    assert!(s4.contains("content_block_stop"));
}

#[test]
fn test_custom_tool_call_from_terminal_output() {
    // 无流式事件只有 completed:从 response.output 补发 custom 工具调用
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r1","output":[
            {"type":"custom_tool_call","call_id":"call_c","name":"apply_patch","input":"patch-content"}
        ],"usage":{"input_tokens":10,"output_tokens":5}}}"#,
    ));
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    assert!(s.contains("content_block_start"));
    assert!(s.contains("apply_patch"));
    assert!(s.contains("call_c"));
    assert!(
        s.contains("{\"input\":\"patch-content\"}")
            || s.contains("{\\\"input\\\":\\\"patch-content\\\"}")
    );
    assert!(s.contains("message_delta"));
    assert!(s.contains("tool_use"));
}

#[test]
fn test_custom_tool_call_done_with_full_input_direct() {
    // output_item.added 直接带完整 input(无 delta 流),done 时一次性发
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    r.process(&ev(
        r#"{"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"call_x","name":"freeform","input":"pwd","output_index":0}}"#,
    ));
    let out = r.process(&ev(
        r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_x","name":"freeform","input":"pwd"}}"#,
    ));
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    // 字符串 input 包成 {"input": "pwd"} 对象
    assert!(s.contains("{\\\"input\\\":\\\"pwd\\\"}") || s.contains("{\"input\":\"pwd\"}"));
    assert!(s.contains("content_block_stop"));
}

#[test]
fn test_web_search_call_streaming() {
    // web_search_call → server_tool_use + web_search_tool_result
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"response.output_item.done","item":{"type":"web_search_call","id":"ws_1","action":{"query":"rust async"},"results":[{"url":"https://example.com","title":"Example"}]}}"#,
    ));
    let s = s(&out);
    assert!(s.contains("server_tool_use"));
    assert!(s.contains("web_search"));
    assert!(s.contains("web_search_tool_result"));
    assert!(s.contains("rust async"));
    assert!(s.contains("https://example.com"));
}

#[test]
fn test_redacted_thinking_restores_from_encrypted_content() {
    // encrypted_content 带前缀 "claude-redacted-thinking:" 应还原为 redacted_thinking 块
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    r.process(&ev(
        r#"{"type":"response.output_item.added","item":{"type":"reasoning"}}"#,
    ));
    let out = r.process(&ev(
        r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"claude-redacted-thinking:opaque_data_xyz"}}"#,
    ));
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    // 应发 redacted_thinking 块，不是 thinking 块
    assert!(
        s.contains("\"type\":\"redacted_thinking\""),
        "应发 redacted_thinking 块,实际: {s}"
    );
    assert!(
        s.contains("redacted_thinking_data"),
        "应发 redacted_thinking_data delta,实际: {s}"
    );
    assert!(s.contains("opaque_data_xyz"), "应包含 data 载荷,实际: {s}");
    assert!(
        !s.contains("signature_delta"),
        "redacted_thinking 不应发 signature_delta,实际: {s}"
    );
    assert!(s.contains("content_block_stop"));
}

#[test]
fn test_redacted_thinking_with_summary_seen() {
    // 带 summary 的 redacted_thinking: summary 可见 + 最后发 data
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    r.process(&ev(
        r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking process"}"#,
    ));
    let out = r.process(&ev(
        r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"claude-redacted-thinking:data123"}}"#,
    ));
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    assert!(
        s.contains("\"type\":\"redacted_thinking\""),
        "应为 redacted_thinking 块"
    );
    assert!(s.contains("redacted_thinking_data"));
    assert!(s.contains("data123"));
}

#[test]
fn test_normal_thinking_signature_not_affected() {
    // 无前缀的普通 signature 应继续正常处理（不误判为 redacted_thinking）
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out1 = r.process(&ev(
        r#"{"type":"response.reasoning_summary_text.delta","delta":"思考中"}"#,
    ));
    let buf1 = out1.concat();
    let s1 = String::from_utf8_lossy(&buf1);

    let out2 = r.process(&ev(
        r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"gAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}}"#,
    ));
    let buf2 = out2.concat();
    let s2 = String::from_utf8_lossy(&buf2);

    let combined = format!("{}{}", s1, s2);
    assert!(
        combined.contains("\"type\":\"thinking\""),
        "普通签名应发 thinking 块,实际: {}",
        combined
    );
    assert!(
        combined.contains("signature_delta"),
        "应发 signature_delta,实际: {}",
        combined
    );
    assert!(combined.contains("gAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
    assert!(
        !combined.contains("redacted_thinking"),
        "不应误判为 redacted_thinking"
    );
}

#[test]
fn test_redacted_thinking_signature_only() {
    // 无 summary 只有 encrypted_content 的 redacted_thinking
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    r.process(&ev(
        r#"{"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"claude-redacted-thinking:sig_only"}}"#,
    ));
    let out = r.process(&ev(
        r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"claude-redacted-thinking:sig_only"}}"#,
    ));
    let bufs = out.concat();
    let s = String::from_utf8_lossy(&bufs);
    assert!(s.contains("\"type\":\"redacted_thinking\""));
    assert!(s.contains("redacted_thinking_data"));
    assert!(s.contains("sig_only"));
    assert!(s.contains("content_block_stop"));
}

#[test]
fn test_empty_incomplete_becomes_error() {
    // 0 token 空 incomplete:报错而非伪成功(对齐 CPA 502 语义)
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"response.incomplete","response":{"id":"r1","output":[],"usage":{"output_tokens":0}}}"#,
    ));
    assert_eq!(out.len(), 1);
    let v = frame_data(&out[0]);
    assert_eq!(v["type"], "error");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("incomplete empty response"));
}

#[test]
fn test_empty_incomplete_with_output_not_error() {
    // 已有输出 delta:空 incomplete 判定不成立,正常收尾
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    r.process(&ev(r#"{"type":"response.output_text.delta","delta":"hi"}"#));
    let out = r.process(&ev(
        r#"{"type":"response.incomplete","response":{"id":"r1","output":[],"usage":{"output_tokens":0}}}"#,
    ));
    assert!(out.iter().any(|b| b.starts_with(b"event: message_delta")));
}

#[test]
fn test_empty_incomplete_guards() {
    // output_tokens 非显式整数 0(缺失/浮点/非零):不算静默中止
    for usage in [
        r#""usage":{}"#,
        r#""usage":{"output_tokens":0.5}"#,
        r#""usage":{"output_tokens":3}"#,
    ] {
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        let out = r.process(&ev(&format!(
            r#"{{"type":"response.incomplete","response":{{"id":"r1","output":[],{usage}}}}}"#
        )));
        assert!(
            out.iter().any(|b| b.starts_with(b"event: message_delta")),
            "usage={usage} 应正常收尾"
        );
    }
    // 有已完成 item(response.output_item.done 计数):不算
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    r.process(&ev(
        r#"{"type":"response.output_item.done","item":{"type":"reasoning"}}"#,
    ));
    let out = r.process(&ev(
        r#"{"type":"response.incomplete","response":{"id":"r1","output":[],"usage":{"output_tokens":0}}}"#,
    ));
    assert!(out.iter().any(|b| b.starts_with(b"event: message_delta")));
    // response.completed 不触发该判定
    let mut r = ResponsesRelay::new(None);
    r.process(&created());
    let out = r.process(&ev(
        r#"{"type":"response.completed","response":{"id":"r1","output":[],"usage":{"output_tokens":0}}}"#,
    ));
    assert!(out.iter().any(|b| b.starts_with(b"event: message_delta")));
}


#[test]
fn test_golden_sse_typical_flows_byte_invariance() {
    // 1. 文本增量 (Text Delta Stream)
    {
        let mut r = ResponsesRelay::new(Some(100));
        let mut out = Vec::new();
        out.extend(r.process(&ev(r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#)));
        out.extend(r.process(&ev(r#"{"type":"response.output_text.delta","delta":"hello "}"#)));
        out.extend(r.process(&ev(r#"{"type":"response.output_text.delta","delta":"world"}"#)));
        out.extend(r.process(&ev(r#"{"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":100,"output_tokens":2}}}"#)));
        let text_stream = s(&out);
        assert_eq!(
            text_stream,
            concat!(
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"resp_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"gpt-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":100,\"output_tokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello \"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"world\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":100,\"output_tokens\":2}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
            )
        );
    }

    // 2. Thinking 思考过程与签名收尾 (Thinking + Signature Stream)
    {
        let mut r = ResponsesRelay::new(None);
        let mut out = Vec::new();
        let sig = "gAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        out.extend(r.process(&ev(r#"{"type":"response.created","response":{"id":"resp_2","model":"gpt-5"}}"#)));
        out.extend(r.process(&ev(r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking step 1"}"#)));
        out.extend(r.process(&ev(&format!(r#"{{"type":"response.output_item.done","item":{{"type":"reasoning","encrypted_content":"{sig}"}}}}"#))));
        out.extend(r.process(&ev(r#"{"type":"response.output_text.delta","delta":"answer"}"#)));
        out.extend(r.process(&ev(r#"{"type":"response.completed","response":{"id":"resp_2","usage":{"input_tokens":50,"output_tokens":10}}}"#)));
        let thinking_stream = s(&out);
        assert_eq!(
            thinking_stream,
            format!(
                concat!(
                    "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"resp_2\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"gpt-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{{\"input_tokens\":1,\"output_tokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}}}}\n\n",
                    "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"thinking\",\"thinking\":\"\"}}}}\n\n",
                    "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"thinking_delta\",\"thinking\":\"thinking step 1\"}}}}\n\n",
                    "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"signature_delta\",\"signature\":\"{sig}\"}}}}\n\n",
                    "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                    "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":1,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n",
                    "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"text_delta\",\"text\":\"answer\"}}}}\n\n",
                    "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":1}}\n\n",
                    "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\",\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":50,\"output_tokens\":10}}}}\n\n",
                    "event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
                ),
                sig = sig
            )
        );
    }

    // 3. Function Call 串行队列 (Function Call Queue Stream)
    {
        let mut r = ResponsesRelay::new(None);
        let mut out = Vec::new();
        out.extend(r.process(&ev(r#"{"type":"response.created","response":{"id":"resp_3","model":"gpt-5"}}"#)));
        out.extend(r.process(&ev(r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"c1","name":"tool_a"}}"#)));
        out.extend(r.process(&ev(r#"{"type":"response.function_call_arguments.delta","call_id":"c1","delta":"{\"x\":1}"}"#)));
        out.extend(r.process(&ev(r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"c1","name":"tool_a","arguments":"{\"x\":1}"}}"#)));
        out.extend(r.process(&ev(r#"{"type":"response.completed","response":{"id":"resp_3","usage":{"input_tokens":20,"output_tokens":5}}}"#)));
        let fc_stream = s(&out);
        assert_eq!(
            fc_stream,
concat!(
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"resp_3\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"gpt-5\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n\n",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"c1\",\"name\":\"tool_a\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"x\\\":1}\"}}\n\n",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":20,\"output_tokens\":5}}\n\n",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
            )
        );
    }

    // 4. 0-token incomplete 终态检测与报错 (0-token incomplete terminal)
    {
        let mut r = ResponsesRelay::new(None);
        let mut out = Vec::new();
        out.extend(r.process(&ev(r#"{"type":"response.created","response":{"id":"resp_4","model":"gpt-5"}}"#)));
        let inc = ev(r#"{"type":"response.incomplete","response":{"id":"resp_4","output":[],"usage":{"input_tokens":10,"output_tokens":0}}}"#);
        out.extend(r.process(&inc));
        let inc_stream = s(&out);
        assert!(inc_stream.contains("event: error"));
        assert!(inc_stream.contains("incomplete empty response"));
    }
}
