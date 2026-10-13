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
pub const SANDBOX_DAILY_API_ENDPOINT: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com";
pub const API_VERSION: &str = "v1internal";

/// 写死 hub 回退版本,不拉 updater manifest
/// Cloud Code 拒绝 <2.9.0 客户端访问新模型,此版本必须 ≥2.9.0
/// 聊天 UA（user_agents.antigravity）与 fetch_models / login 缺省同源，避免版本分裂。
macro_rules! antigravity_hub_ua {
    () => {
        "antigravity/hub/2.21.1 darwin/arm64"
    };
}
pub const REQUEST_UA: &str = antigravity_hub_ua!();
/// 由同一 hub UA 字面量派生，禁止再手写一份版本号。
pub const ONBOARD_UA: &str = concat!(antigravity_hub_ua!(), " google-api-nodejs-client/10.3.0");
pub const GOOG_API_CLIENT: &str = "gl-node/22.21.1";
pub const TOKEN_REFRESH_UA: &str = "Go-http-client/2.0";

const NODE_API_CLIENT_UA: &str = "google-api-nodejs-client/10.3.0";
/// Cloud Code 拒绝 <2.9.0；解析失败时回退（对齐 CPA antigravityFallbackVersion）。
pub const IDE_VERSION_FALLBACK: &str = "2.9.1";

fn is_antigravity_family_ua(lower: &str) -> bool {
    lower.starts_with("antigravity/hub/") || lower.starts_with("antigravity/")
}

/// 去掉 onboard 长 UA 尾缀，得到短 hub/legacy UA（对齐 CPA antigravityBaseUserAgent）。
pub fn base_user_agent(user_agent: &str) -> String {
    let user_agent = user_agent.trim();
    if user_agent.is_empty() {
        return REQUEST_UA.to_string();
    }
    let lower = user_agent.to_ascii_lowercase();
    if is_antigravity_family_ua(&lower) {
        if let Some(idx) = lower.find(" google-api-nodejs-client/") {
            let trimmed = user_agent[..idx].trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    user_agent.to_string()
}

/// 配置覆盖 chat/hub UA 时，按同规则派生 onboard UA（对齐 CPA AntigravityOnboardUserUserAgent）。
pub fn onboard_ua(request_ua: &str) -> String {
    let user_agent = request_ua.trim();
    if user_agent.is_empty() {
        return ONBOARD_UA.to_string();
    }
    let lower = user_agent.to_ascii_lowercase();
    if !is_antigravity_family_ua(&lower) {
        return user_agent.to_string();
    }
    if lower.contains("google-api-nodejs-client/") {
        return user_agent.to_string();
    }
    format!("{} {NODE_API_CLIENT_UA}", base_user_agent(user_agent))
}

/// 从短/长 Antigravity UA 解析 ide_version（对齐 CPA AntigravityVersionFromUserAgent）。
pub fn version_from_user_agent(user_agent: &str) -> String {
    let base = base_user_agent(user_agent);
    let lower = base.to_ascii_lowercase();
    let rest = if let Some(rest) = lower.strip_prefix("antigravity/hub/") {
        // 用原串切片保留大小写（版本号通常无大小写差异）
        &base[base.len() - rest.len()..]
    } else if let Some(rest) = lower.strip_prefix("antigravity/") {
        &base[base.len() - rest.len()..]
    } else {
        return IDE_VERSION_FALLBACK.to_string();
    };
    let ver = rest
        .split(|c: char| c.is_whitespace())
        .next()
        .unwrap_or("")
        .trim();
    if ver.is_empty() {
        IDE_VERSION_FALLBACK.to_string()
    } else {
        ver.to_string()
    }
}

/// 到期前 300s 视为需刷新(对齐 CPA antigravityRequestTokenSafetyWindow = 5 * time.Minute)
pub const REFRESH_SKEW_SECS: i64 = 300;

/// 项目缓存下的凭证目录
pub const DEFAULT_AUTH_DIR: &str = ".cache/antigravity";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onboard_ua_appends_node_client_once() {
        assert_eq!(
            onboard_ua(REQUEST_UA),
            format!("{REQUEST_UA} google-api-nodejs-client/10.3.0")
        );
        assert_eq!(onboard_ua(ONBOARD_UA), ONBOARD_UA);
        assert_eq!(onboard_ua("curl/8.0"), "curl/8.0");
    }

    #[test]
    fn version_from_hub_and_legacy_ua() {
        assert_eq!(
            version_from_user_agent("antigravity/hub/2.21.1 darwin/arm64"),
            "2.21.1"
        );
        assert_eq!(
            version_from_user_agent("antigravity/1.23.2 windows/amd64"),
            "1.23.2"
        );
        assert_eq!(
            version_from_user_agent(ONBOARD_UA),
            "2.21.1"
        );
        assert_eq!(version_from_user_agent("curl/8"), IDE_VERSION_FALLBACK);
    }
}
