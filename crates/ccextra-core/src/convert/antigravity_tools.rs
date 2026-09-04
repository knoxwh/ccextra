// Antigravity agents (Interactions API) ship intrinsic sandbox tools such as
// read_file / write_file / execute_code. When a client-defined function re-declares
// one of those names, Google returns 500 "Unknown Error". The proxy therefore renames
// colliding client tools with the ExternalToolPrefix on the way upstream and
// strips the prefix again on every response path, so the client keeps seeing
// its original names.
//
// 对齐 CPA internal/translator/common/antigravity_tools.go (6a26e92a)

const EXTERNAL_TOOL_PREFIX: &str = "external_";

/// 列出与 Antigravity 内置沙箱工具冲突的客户端工具名
fn is_colliding_tool(name: &str) -> bool {
    matches!(name, "read_file" | "write_file" | "execute_code")
}

/// 将客户端工具名映射为发往 Antigravity Interactions API 的名称。
/// 冲突名加 `external_` 前缀,非冲突名原样返回。
pub fn antigravity_tool_name_to_upstream(name: &str) -> String {
    if is_colliding_tool(name) {
        format!("{}{}", EXTERNAL_TOOL_PREFIX, name)
    } else {
        name.to_string()
    }
}

/// 从上游工具名还原客户端名称。仅当前缀为 `external_` 且基础名为冲突工具时才剥离前缀,
/// 其余名称原样返回。
pub fn antigravity_upstream_tool_name_to_client(name: &str) -> String {
    if let Some(base) = name.strip_prefix(EXTERNAL_TOOL_PREFIX) {
        if is_colliding_tool(base) {
            return base.to_string();
        }
    }
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_antigravity_tool_name_to_upstream() {
        assert_eq!(
            antigravity_tool_name_to_upstream("read_file"),
            "external_read_file"
        );
        assert_eq!(
            antigravity_tool_name_to_upstream("write_file"),
            "external_write_file"
        );
        assert_eq!(
            antigravity_tool_name_to_upstream("execute_code"),
            "external_execute_code"
        );
        assert_eq!(
            antigravity_tool_name_to_upstream("web_search"),
            "web_search"
        );
        assert_eq!(
            antigravity_tool_name_to_upstream("get_weather"),
            "get_weather"
        );
    }

    #[test]
    fn test_antigravity_upstream_tool_name_to_client() {
        assert_eq!(
            antigravity_upstream_tool_name_to_client("external_read_file"),
            "read_file"
        );
        assert_eq!(
            antigravity_upstream_tool_name_to_client("external_write_file"),
            "write_file"
        );
        assert_eq!(
            antigravity_upstream_tool_name_to_client("external_execute_code"),
            "execute_code"
        );
        // 非冲突名即使有前缀也不剥离
        assert_eq!(
            antigravity_upstream_tool_name_to_client("external_weather"),
            "external_weather"
        );
        assert_eq!(
            antigravity_upstream_tool_name_to_client("web_search"),
            "web_search"
        );
    }
}
