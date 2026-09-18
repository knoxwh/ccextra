use serde_json::{json, Value};

/// tool_result content → responses output 与图片 parts 元组(对齐 convertToolResultOutput)。
/// function_call_output.output 只接受文本,内嵌 image 抽出为元组第二项,
/// 由调用方追加成随后的 user message(input_image parts)。
/// 孤儿 tool_result 的 content → 独立 user text 输入项
/// (对齐 CPA 8c984672 buildOpenAIResponsesStandaloneToolOutputTextParts:
/// 每个非空文本一个 input_text part;空白/无文本丢弃)
pub(crate) fn orphan_tool_output_as_user(content: Option<&Value>) -> Option<Value> {
    let texts: Vec<String> = match content {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
            .map(String::from)
            .collect(),
        Some(other) => vec![other.to_string()],
        None => Vec::new(),
    };
    let parts: Vec<Value> = texts
        .into_iter()
        .filter(|t| !t.trim().is_empty())
        .map(|t| json!({"type": "input_text", "text": t}))
        .collect();
    if parts.is_empty() {
        return None;
    }
    Some(json!({
        "type": "message",
        "role": "user",
        "content": parts
    }))
}

pub(crate) fn tool_result_output(content: &Value) -> (Value, Vec<Value>) {
    match content {
        Value::Array(items) => {
            let mut texts: Vec<String> = Vec::new();
            let mut images: Vec<Value> = Vec::new();
            for item in items {
                match item.get("type").and_then(|v| v.as_str()) {
                    Some("image") => {
                        let data = item
                            .pointer("/source/data")
                            .and_then(|v| v.as_str())
                            .or_else(|| item.pointer("/source/base64").and_then(|v| v.as_str()))
                            .unwrap_or("");
                        if !data.is_empty() {
                            let media = item
                                .pointer("/source/media_type")
                                .and_then(|v| v.as_str())
                                .or_else(|| {
                                    item.pointer("/source/mime_type").and_then(|v| v.as_str())
                                })
                                .unwrap_or("application/octet-stream");
                            images.push(json!({
                                "type": "input_image",
                                "image_url": format!("data:{media};base64,{data}")
                            }));
                        }
                    }
                    Some("text") => {
                        if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                            texts.push(t.to_string());
                        }
                    }
                    _ => {}
                }
            }
            if texts.is_empty() {
                // 无文本可留:抽取过图片用占位(对齐 sub2api "(empty)"),
                // 否则维持原样序列化,未识别结构不丢信息
                if images.is_empty() {
                    (Value::String(content.to_string()), images)
                } else {
                    (Value::String("(no output)".to_string()), images)
                }
            } else {
                (Value::String(texts.join("\n")), images)
            }
        }
        Value::String(_) => (content.clone(), Vec::new()),
        other => (Value::String(other.to_string()), Vec::new()),
    }
}

/// image block → data URL(对齐 image 分支)
pub(crate) fn image_to_data_url(part: &Value) -> Option<String> {
    let source = part.get("source")?;
    let data = source
        .get("data")
        .or_else(|| source.get("base64"))
        .and_then(|v| v.as_str())?;
    if data.is_empty() {
        return None;
    }
    let media_type = source
        .get("media_type")
        .and_then(|v| v.as_str())
        .or_else(|| source.get("mime_type").and_then(|v| v.as_str()))
        .unwrap_or("application/octet-stream");
    Some(format!("data:{media_type};base64,{data}"))
}

/// document block → input_file(对齐 CPA appendDocumentContent:
/// 仅支持 base64 + application/pdf,输出 type=input_file,filename=document.pdf)
pub(crate) fn document_to_input_file(part: &Value) -> Option<Value> {
    let source = part.get("source")?;
    if source.get("type").and_then(|v| v.as_str()) != Some("base64") {
        return None;
    }
    let media_type = source
        .get("media_type")
        .or_else(|| source.get("mime_type"))
        .and_then(|v| v.as_str())
        .map(str::trim)?;
    if !media_type.eq_ignore_ascii_case("application/pdf") {
        return None;
    }
    let data = source
        .get("data")
        .or_else(|| source.get("base64"))
        .and_then(|v| v.as_str())?;
    if data.is_empty() {
        return None;
    }
    Some(json!({
        "type": "input_file",
        "file_data": format!("data:{media_type};base64,{data}"),
        "filename": "document.pdf"
    }))
}

/// Claude custom 工具 tool_use.input → 字符串(对齐 unwrapCustomToolInput)
///
/// 响应侧把 custom 工具字符串 input 包成 {"input": str} 对象发回;
/// 请求侧此处解包还原。格式不符时回退到原始文本。
pub(crate) fn unwrap_custom_tool_input(input: Option<&Value>) -> String {
    match input {
        Some(Value::Object(map)) => {
            if let Some(inner) = map.get("input") {
                match inner {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                }
            } else {
                input.map(|v| v.to_string()).unwrap_or_default()
            }
        }
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}


/// call_id 超 64 字符确定性截短(对齐 shortenCodexCallIDIfNeeded)
pub(crate) fn shorten_call_id(id: &str) -> String {
    const LIMIT: usize = 64;
    if id.len() <= LIMIT {
        return id.to_string();
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(id.as_bytes());
    let suffix = format!("_{}", hex::encode(&digest[..8]));
    let mut prefix_len = LIMIT.saturating_sub(suffix.len());
    while prefix_len > 0 && !id.is_char_boundary(prefix_len) {
        prefix_len -= 1;
    }
    format!("{}{}", &id[..prefix_len], suffix)
}

