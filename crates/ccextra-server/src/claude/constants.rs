// Claude 订阅 (Anthropic OAuth) 常量
// (对齐 CLIProxyAPI internal/auth/claude/anthropic_auth.go)

/// 授权页
pub const AUTH_URL: &str = "https://claude.ai/oauth/authorize";
/// 换码 / 刷新端点 (Claude Code 2.1.220 起走 platform.claude.com)
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const REFRESH_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// 登录后可选身份查询
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
/// Claude Code 桌面 client id
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// 固定回调(须与 Anthropic 注册一致;端口不可随意改)
pub const REDIRECT_URI: &str = "http://localhost:54545/callback";
pub const CALLBACK_PATH: &str = "/callback";
pub const DEFAULT_CALLBACK_PORT: u16 = 54545;
/// 授权 / 刷新 scope
pub const AUTH_SCOPE: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// Messages 上游
pub const DEFAULT_API_BASE_URL: &str = "https://api.anthropic.com";
/// OAuth 凭证须声明的 beta
pub const OAUTH_BETA: &str = "oauth-2025-04-20";

/// 提前刷新窗口 (秒): 对齐 CPA ClaudeAuthenticator.RefreshLead = 4h
pub const REFRESH_SKEW_SECS: i64 = 4 * 3600;
/// 仅对明确可重试的 HTTP 5xx 重试;传输/解码错误不盲重放 (refresh 疑似单次使用)
pub const REFRESH_MAX_RETRIES: usize = 3;
