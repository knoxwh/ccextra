use bytes::Bytes;
use ccextra_server::sse::relay_openai_chat_to_anthropic;
use futures::StreamExt;

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
