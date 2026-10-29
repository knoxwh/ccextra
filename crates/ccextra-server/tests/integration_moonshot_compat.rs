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

    let stream = futures::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
    let mut relay_stream = relay_openai_chat_to_anthropic(stream, Some(100));

    let mut frames = Vec::new();
    while let Some(Ok(frame)) = relay_stream.next().await {
        frames.push(frame);
    }

    // 按 SSE 帧解析 message_delta 的 data JSON 后断言 usage
    let delta: serde_json::Value = frames
        .iter()
        .flat_map(|f| {
            let text = String::from_utf8_lossy(f).into_owned();
            text.split("\n\n")
                .filter_map(|event| {
                    let mut lines = event.lines();
                    (lines.next()? == "event: message_delta")
                        .then(|| lines.find_map(|l| l.strip_prefix("data: ")))
                        .flatten()
                        .map(str::to_owned)
                })
                .collect::<Vec<_>>()
        })
        .map(|data| serde_json::from_str(&data).expect("message_delta data is JSON"))
        .next()
        .expect("should have message_delta");

    assert_eq!(delta["type"], "message_delta");
    assert_eq!(delta["usage"]["input_tokens"], 120);
    assert_eq!(delta["usage"]["output_tokens"], 10);
}
