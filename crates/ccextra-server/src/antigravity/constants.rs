// Antigravity OAuth 常量

pub const CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
pub const CLIENT_SECRET: &str = "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";
pub const CALLBACK_PORT: u16 = 51121;

pub const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
pub const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const USERINFO_ENDPOINT: &str = "https://www.googleapis.com/oauth2/v2/userinfo?alt=json";

pub const API_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";
pub const DAILY_API_ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const API_VERSION: &str = "v1internal";

/// 写死 hub 回退版本,不拉 updater manifest
/// Cloud Code 拒绝 <2.9.0 客户端访问新模型,此版本必须 ≥2.9.0
pub const REQUEST_UA: &str = "antigravity/hub/2.9.1 darwin/arm64";
pub const ONBOARD_UA: &str = "antigravity/hub/2.9.1 darwin/arm64 google-api-nodejs-client/10.3.0";
pub const GOOG_API_CLIENT: &str = "gl-node/22.21.1";
pub const TOKEN_REFRESH_UA: &str = "Go-http-client/2.0";

/// 到期前 3000s 视为需刷新
pub const REFRESH_SKEW_SECS: i64 = 3000;

/// 项目缓存下的凭证目录
pub const DEFAULT_AUTH_DIR: &str = ".cache/antigravity";
