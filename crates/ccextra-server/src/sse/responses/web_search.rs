use serde_json::{json, Value};

/// web_search_call 的 query(对齐 codexWebSearchQuery:item/root 多路径)
pub(crate) fn web_search_query(root: &Value, item: &Value) -> String {
    for path in ["/action/query", "/query", "/input/query"] {
        if let Some(v) = item.pointer(path).and_then(|v| v.as_str()) {
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
        if let Some(v) = root.pointer(path).and_then(|v| v.as_str()) {
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

/// web_search_call 的 results → web_search_result 块数组(对齐 codexWebSearchResultContent)
pub fn web_search_result_content(root: &Value, item: &Value) -> Vec<Value> {
    let results = item
        .get("results")
        .and_then(|v| v.as_array())
        .or_else(|| root.get("results").and_then(|v| v.as_array()));
    let Some(results) = results else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for result in results {
        let url = result
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if url.is_empty() {
            continue;
        }
        let mut title = result
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if title.is_empty() {
            title = url.clone();
        }
        out.push(json!({
            "type": "web_search_result",
            "title": title,
            "url": url,
            "page_age": null
        }));
    }
    out
}
