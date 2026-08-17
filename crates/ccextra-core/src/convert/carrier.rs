// Gemini 载荷签名提取与注入

use serde_json::Value;

/// 从 Anthropic system 中提取思考签名
///
/// 查找 cache_control 类型为 "ephemeral" 的文本块,
/// 以 "cpa-gemini-carrier-v1:" 开头的为载荷
pub fn extract_thought_signature(system: &[Value]) -> Option<String> {
    for block in system {
        if let Some("ephemeral") = block
            .get("cache_control")
            .and_then(|cc| cc.get("type"))
            .and_then(|t| t.as_str())
        {
            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                if let Some(payload) = text.strip_prefix("cpa-gemini-carrier-v1:") {
                    return Some(payload.to_string());
                }
            }
        }
    }
    None
}

/// 注入思考签名到 Gemini 请求
///
/// 添加到 systemInstruction 末尾作为 ephemeral 块
pub fn inject_thought_signature(gemini_body: &mut Value, signature: &str) {
    let carrier_text = format!("cpa-gemini-carrier-v1:{}", signature);

    let block = serde_json::json!({
        "text": carrier_text,
        "cache_control": {
            "type": "ephemeral"
        }
    });

    // 确保 systemInstruction 存在
    if !gemini_body.get("systemInstruction").is_some() {
        gemini_body["systemInstruction"] = serde_json::json!([]);
    }

    // 追加到 systemInstruction 数组
    if let Some(system) = gemini_body.get_mut("systemInstruction") {
        if let Some(arr) = system.as_array_mut() {
            arr.push(block);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_thought_signature_found() {
        let system = vec![
            json!({
                "type": "text",
                "text": "You are a helpful assistant"
            }),
            json!({
                "type": "text",
                "text": "cpa-gemini-carrier-v1:eyJzZXNzaW9uIjoiMTIzIn0",
                "cache_control": {
                    "type": "ephemeral"
                }
            }),
        ];

        let signature = extract_thought_signature(&system);
        assert_eq!(signature, Some("eyJzZXNzaW9uIjoiMTIzIn0".to_string()));
    }

    #[test]
    fn test_extract_thought_signature_not_found() {
        let system = vec![json!({
            "type": "text",
            "text": "You are a helpful assistant"
        })];

        let signature = extract_thought_signature(&system);
        assert_eq!(signature, None);
    }

    #[test]
    fn test_extract_thought_signature_wrong_cache_type() {
        let system = vec![json!({
            "type": "text",
            "text": "cpa-gemini-carrier-v1:payload",
            "cache_control": {
                "type": "standard"
            }
        })];

        let signature = extract_thought_signature(&system);
        assert_eq!(signature, None);
    }

    #[test]
    fn test_inject_thought_signature_new_system() {
        let mut gemini = json!({
            "model": "gemini-2.0",
            "contents": []
        });

        inject_thought_signature(&mut gemini, "test-payload");

        assert!(gemini["systemInstruction"].is_array());
        let system = gemini["systemInstruction"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["text"], "cpa-gemini-carrier-v1:test-payload");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn test_inject_thought_signature_existing_system() {
        let mut gemini = json!({
            "model": "gemini-2.0",
            "systemInstruction": [
                {
                    "text": "Existing instruction"
                }
            ],
            "contents": []
        });

        inject_thought_signature(&mut gemini, "test-payload");

        let system = gemini["systemInstruction"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["text"], "Existing instruction");
        assert_eq!(system[1]["text"], "cpa-gemini-carrier-v1:test-payload");
    }
}
