// 手写 SSE 解析器:跨 chunk 累积,按空行切分事件
//
// SSE 格式:
//   event: message_start
//   data: {...}
//
//   (空行结束一个事件)
//
// 一个 chunk 可能包含 0 个、1 个或多个完整事件,也可能切在事件中间。
// 本解析器维护内部 buffer,逐行处理,空行时产出完整事件。

/// 一个解析出的 SSE 事件
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// 事件类型(缺省时按 message 处理)
    pub event: Option<String>,
    /// data 内容(多行 data 以 \n 连接)
    pub data: String,
}

/// 跨 chunk 状态化的 SSE 解析器
///
/// 缓冲用字节(Vec<u8>)而非 String:多字节 UTF-8 字符(中文等)可能被
/// TCP 分片切到两个 chunk,from_utf8_lossy 会把两半都变成 U+FFFD。
/// 按字节找换行,整行齐了才转字符串,规避截断乱码。
#[derive(Debug, Default)]
pub struct SseParser {
    /// 未处理的完整行字节缓冲
    buf: Vec<u8>,
    /// 当前事件类型
    event: Option<String>,
    /// 当前事件的 data 行
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入新字节,返回产生的完整事件
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        self.process_lines()
    }

    /// 流结束,flush 剩余缓冲
    pub fn finish(&mut self) -> Vec<SseEvent> {
        // 补一个换行,让最后一行也能被处理
        if !self.buf.is_empty() {
            self.buf.push(b'\n');
        }
        let mut events = self.process_lines();
        // 末尾可能没有空行,强制 flush 残留事件
        if let Some(ev) = self.take_event() {
            events.push(ev);
        }
        events
    }

    /// 逐行处理缓冲,产出完整事件
    fn process_lines(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        while let Some(newline) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line_bytes = self.buf[..newline].to_vec();
            self.buf.drain(..=newline);

            // 去除行尾 \r
            if line_bytes.last() == Some(&b'\r') {
                line_bytes.pop();
            }

            // 整行字节齐了才转字符串,跨 chunk 的多字节字符不会被切断
            let line = String::from_utf8_lossy(&line_bytes);

            if line.is_empty() {
                // 空行结束当前事件
                if let Some(ev) = self.take_event() {
                    events.push(ev);
                }
            } else if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim_start().to_string();
                self.data.push(data);
            } else if let Some(event) = line.strip_prefix("event:") {
                self.event = Some(event.trim().to_string());
            }
            // 其他行(comment/retry/id)忽略
        }

        events
    }

    /// 取出当前累积的事件并重置
    fn take_event(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() && self.event.is_none() {
            return None;
        }
        let data = self.data.join("\n");
        self.data.clear();
        let event = self.event.take();
        Some(SseEvent { event, data })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_event() {
        let mut p = SseParser::new();
        let events = p.push(b"event: message_start\ndata: {\"id\":\"1\"}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{\"id\":\"1\"}");
    }

    #[test]
    fn test_event_split_across_chunks() {
        let mut p = SseParser::new();
        assert!(p.push(b"event: mess").is_empty());
        let events = p.push(b"age_start\ndata: {\"a\":1}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
    }

    #[test]
    fn test_multiple_events_in_one_chunk() {
        let mut p = SseParser::new();
        let events = p.push(b"data: {}\n\ndata: {}\n\n");
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn test_multi_line_data() {
        let mut p = SseParser::new();
        let events = p.push(b"data: a\ndata: b\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "a\nb");
    }

    #[test]
    fn test_done_marker() {
        let mut p = SseParser::new();
        let events = p.push(b"data: [DONE]\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "[DONE]");
    }

    #[test]
    fn test_finish_flushes_trailing() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: x").is_empty());
        let events = p.finish();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "x");
    }

    #[test]
    fn test_crlf_line_endings() {
        let mut p = SseParser::new();
        let events = p.push(b"event: test\r\ndata: content\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("test"));
        assert_eq!(events[0].data, "content");
    }

    #[test]
    fn test_event_without_data() {
        let mut p = SseParser::new();
        let events = p.push(b"event: ping\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(events[0].data, "");
    }

    #[test]
    fn test_data_only_no_event() {
        let mut p = SseParser::new();
        let events = p.push(b"data: content\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, None);
        assert_eq!(events[0].data, "content");
    }

    #[test]
    fn test_comment_lines_ignored() {
        let mut p = SseParser::new();
        let events = p.push(b": this is a comment\ndata: real\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    #[test]
    fn test_utf8_split_across_chunks() {
        // "中" = E4 B8 AD,被 chunk 边界切成两半,不得产生 U+FFFD 乱码
        let mut p = SseParser::new();
        assert!(p.push(&[b'd', b'a', b't', b'a', b':', b' ', 0xE4, 0xB8]).is_empty());
        let events = p.push(&[0xAD, b'\n', b'\n']);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "中");
    }

    #[test]
    fn test_empty_buffer_after_finish() {
        let mut p = SseParser::new();
        p.push(b"data: x\n\n");
        p.finish();
        // 再次 push 应正常工作
        let events = p.push(b"data: y\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "y");
    }
}