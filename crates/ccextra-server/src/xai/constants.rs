// xAI Grok OAuth 与 API 常量 (对齐 CLIProxyAPI internal/auth/xai/types.go)

pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
pub const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
pub const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

pub const DEFAULT_API_BASE_URL: &str = "https://api.x.ai/v1";
pub const CLI_CHAT_PROXY_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";

/// 提前刷新窗口 (秒): 对齐 CLIProxyAPI refreshLead = 5 * time.Minute
pub const REFRESH_SKEW_SECS: i64 = 300;

/// 刷新 HTTP 最大尝试次数(仅明确 5xx 可重试,对齐 Claude/Codex)
pub const REFRESH_MAX_RETRIES: usize = 3;
