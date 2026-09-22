// Codex (OpenAI ChatGPT 订阅) OAuth 与上游常量
// (对齐 CLIProxyAPI internal/auth/codex/openai_auth.go 与 codex_executor_request.go)

pub const AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// 本地回调地址;端口可由 CLI 覆盖,路径固定
pub const CALLBACK_PATH: &str = "/auth/callback";
pub const DEFAULT_CALLBACK_PORT: u16 = 1455;
/// 授权请求 scope (对齐 CPA GenerateAuthURL)
pub const AUTH_SCOPE: &str = "openid email profile offline_access";
/// 刷新请求 scope (对齐 CPA refreshTokensSingleFlight)
pub const REFRESH_SCOPE: &str = "openid profile email";

/// 上游 base URL (对齐 CPA codex_executor: chatgpt.com/backend-api/codex)
pub const DEFAULT_API_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// 提前刷新窗口 (秒): 对齐 CPA CodexAuthenticator.RefreshLead = 24h
pub const REFRESH_SKEW_SECS: i64 = 86400;
/// 刷新重试次数 (对齐 CPA RefreshTokensWithRetry maxRetries = 3)
pub const REFRESH_MAX_RETRIES: usize = 3;
