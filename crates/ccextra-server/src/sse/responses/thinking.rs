use bytes::Bytes;
use serde_json::Value;

use crate::sse::emit;

use super::state_machine::ResponsesRelay;
use super::CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX;

impl ResponsesRelay {
    pub(crate) fn start_thinking(&mut self) -> Vec<Bytes> {
        if self.thinking_open {
            return Vec::new();
        }
        self.thinking_index = self.next_block_index;
        self.next_block_index += 1;
        self.thinking_open = true;
        vec![emit::content_block_start_thinking(self.thinking_index)]
    }

    pub(crate) fn thinking_delta(&self, text: &str) -> Vec<Bytes> {
        if text.is_empty() || !self.thinking_open {
            return Vec::new();
        }
        vec![emit::content_block_delta_thinking(
            self.thinking_index,
            text,
        )]
    }

    /// reasoning delta 统一进块:先收 text,再开 thinking 块发内容
    /// (reasoning_summary_text.delta 与明文 reasoning_text.delta 共用)
    pub(crate) fn plaintext_reasoning_delta(&mut self, root: &Value) -> Vec<Bytes> {
        let delta = root.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        let mut out = self.stop_text();
        // 根据 thinking_is_redacted 标志打开对应类型的块
        if self.thinking_is_redacted {
            out.extend(self.start_redacted_thinking());
        } else {
            out.extend(self.start_thinking());
        }
        out.extend(self.thinking_delta(delta));
        self.thinking_summary_seen = true;
        out
    }

    /// 关闭 thinking 块:有 signature 先发 signature_delta(加密内容回放闭环)
    /// 若 signature 为 redacted_thinking 载荷(带前缀),发 redacted_thinking 块而非 thinking
    pub(crate) fn finalize_thinking(&mut self) -> Vec<Bytes> {
        if !self.thinking_open {
            return Vec::new();
        }
        let mut out = Vec::new();
        // 检测 redacted_thinking 载荷(对齐 responsesRedactedThinkingData)
        if let Some(data) = self
            .thinking_signature
            .strip_prefix(CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX)
        {
            if !data.is_empty() {
                // 发送 redacted_thinking 的 data 字段
                out.push(emit::content_block_delta_redacted_thinking_data(
                    self.thinking_index,
                    data,
                ));
            }
        } else if !self.thinking_signature.is_empty() {
            // 普通 thinking 签名
            out.push(emit::content_block_delta_signature(
                self.thinking_index,
                &self.thinking_signature,
            ));
        }
        out.push(emit::content_block_stop(self.thinking_index));
        self.thinking_open = false;
        out
    }

    /// 无 summary 只有 encrypted_content 的 reasoning item:开块即收尾
    /// 检测 redacted_thinking 载荷,开 redacted_thinking 块而非 thinking 块
    pub(crate) fn finalize_signature_only_thinking(&mut self) -> Vec<Bytes> {
        if self.thinking_signature.is_empty() {
            return Vec::new();
        }
        // 检测是否 redacted_thinking
        let is_redacted = self
            .thinking_signature
            .starts_with(CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX);
        let mut out = if is_redacted {
            self.start_redacted_thinking()
        } else {
            self.start_thinking()
        };
        out.extend(self.finalize_thinking());
        out
    }

    /// 开 redacted_thinking 块(对齐 content_block_start redacted_thinking)
    pub(crate) fn start_redacted_thinking(&mut self) -> Vec<Bytes> {
        if self.thinking_open {
            return Vec::new();
        }
        self.thinking_index = self.next_block_index;
        self.next_block_index += 1;
        self.thinking_open = true;
        vec![emit::content_block_start_redacted_thinking(
            self.thinking_index,
        )]
    }
}
