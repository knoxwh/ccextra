use bytes::Bytes;
use ccextra_server::sse::relay_openai_chat_to_anthropic;
use futures::StreamExt;
use serde_json::json;

#[tokio::test]
async fn test_moonshot_streaming_usage_extraction() {
    // 模拟 Moonshot 流式响应:usage 在 choices[0]
    let chunks = vec![
        Bytes::from(concat!(
            "data: {\"id\":\"chat-1\",\"model\":\"moonshot-v1-8k\",",
            "\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"index\":0}]}\n\n"
        )),
        Bytes::from(concat!(
            "data: {\"id\":\"chat-1\",\"model\":\"moonshot-v1-8k\",",
            "\"choices\":[{\"delta\":{\"content\":\"你好\"},\"index\":0,",
            "\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":10}}]}\n\n"
        )),
        Bytes::from("data: [DONE]\n\n"),
    ];

    let stream = futures::stream::iter(chunks.into_iter().map(Ok::<_, reqwest::Error>));
    let mut relay_stream = relay_openai_chat_to_anthropic(stream, Some(100));

    let mut frames = Vec::new();
    while let Some(Ok(frame)) = relay_stream.next().await {
        frames.push(frame);
    }

    // 验证 message_delta 包含正确的 usage
    let delta_frame = frames
        .iter()
        .find(|f| String::from_utf8_lossy(f).contains("message_delta"))
        .expect("should have message_delta");

    let s = String::from_utf8_lossy(delta_frame);
    assert!(s.contains("\"input_tokens\":120"));
    assert!(s.contains("\"output_tokens\":10"));
}

#[tokio::test]
async fn test_assistant_tool_calls_no_content_field() {
    // 验证 tool_calls + 空文本的请求体不含 content 键
    use ccextra_core::convert::to_openai_chat::convert_to_openai_chat;

    let mut body = json!({
        "model": "moonshot-v1-8k",
        "messages": [{
            "role": "user",
            "content": "天气如何"
        }, {
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": "t1", "name": "$web_search", "input": {"query": "北京天气"}}
            ]
        }]
    });

    convert_to_openai_chat(&mut body, "moonshot-v1-8k").unwrap();

    let assistant_msg = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("should have assistant message");

    // 关键验证:content 键不存在
    assert!(assistant_msg.get("content").is_none());
    assert!(assistant_msg.get("tool_calls").is_some());
}
