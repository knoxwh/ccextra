// Claude 直通:只改 model 字段,其余字节原样保留
//
// 注意:这里仍要 parse 顶层(修改 model),但尽量保持其他字段不动。
// serde_json 的 preserve_order feature 保证 Map 顺序,最小化字节变化。

use serde_json::Value;
use super::Result;

/// Claude 直通转换:只改 model 字段
///
/// 输入:归一化后的 anthropic body
/// 输出:改完 model 的 body,其余原样
pub fn convert_passthrough(body: &mut Value, upstream_model: &str) -> Result<()> {
    body["model"] = Value::String(upstream_model.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_passthrough_only_changes_model() {
        let mut body = json!({
            "model": "evol-opus-5",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 1024,
            "thinking": {"type": "enabled"}
        });

        convert_passthrough(&mut body, "claude-opus-5").unwrap();

        assert_eq!(body["model"], "claude-opus-5");
        assert_eq!(body["messages"][0]["content"], "test");
        assert_eq!(body["max_tokens"], 1024);
        assert!(body["thinking"].is_object());
    }
}
