// JWT Payload 解析工具（对齐 magpie googleClientOf）

use base64::Engine;
use serde_json::Value;

/// 提取 JWT ID Token 的 client_id（优先 azp，回退 aud）
///
/// 对齐 magpie `googleClientOf`:
/// - Google/xAI OAuth 返回的 id_token 带 `azp`(authorized party) 或 `aud`(audience)
/// - `azp` 存在时优先（多 client 场景），否则用 `aud`
pub fn extract_client_id(id_token: &str) -> Option<String> {
    let claims = parse_jwt_payload(id_token)?;
    claims["azp"]
        .as_str()
        .or_else(|| claims["aud"].as_str())
        .map(String::from)
}

/// 提取 JWT 的 email 和 sub
pub fn extract_identity(id_token: &str) -> (Option<String>, Option<String>) {
    let Some(claims) = parse_jwt_payload(id_token) else {
        return (None, None);
    };
    let email = claims["email"].as_str().map(String::from);
    let sub = claims["sub"].as_str().map(String::from);
    (email, sub)
}

/// 解析 JWT payload（不验证签名，仅用于读取 user-owned token 的声明）
fn parse_jwt_payload(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }

    // JWT payload 是 base64url，对齐 magpie：先剥离现有 padding 再解码
    let payload_b64 = parts[1].trim_end_matches('=');
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;

    // 尝试直接解码
    let payload_bytes = engine
        .decode(payload_b64.as_bytes())
        .or_else(|_| {
            // padding 补齐后重试
            let padded = match payload_b64.len() % 4 {
                2 => format!("{payload_b64}=="),
                3 => format!("{payload_b64}="),
                _ => payload_b64.to_string(),
            };
            engine.decode(padded.as_bytes())
        })
        .ok()?;

    serde_json::from_slice(&payload_bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_token(payload: &str) -> String {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let encoded = engine.encode(payload.as_bytes());
        format!("header.{encoded}.sig")
    }

    #[test]
    fn extract_client_id_prefers_azp() {
        let token = make_test_token(r#"{"azp":"client-azp","aud":"client-aud"}"#);
        assert_eq!(extract_client_id(&token), Some("client-azp".into()));
    }

    #[test]
    fn extract_client_id_fallback_aud() {
        let token = make_test_token(r#"{"aud":"client-aud"}"#);
        assert_eq!(extract_client_id(&token), Some("client-aud".into()));
    }

    #[test]
    fn extract_client_id_missing_both() {
        let token = make_test_token(r#"{"sub":"user-123"}"#);
        assert_eq!(extract_client_id(&token), None);
    }

    #[test]
    fn extract_identity_both_present() {
        let token = make_test_token(r#"{"email":"a@b.com","sub":"user-123"}"#);
        let (email, sub) = extract_identity(&token);
        assert_eq!(email, Some("a@b.com".into()));
        assert_eq!(sub, Some("user-123".into()));
    }

    #[test]
    fn extract_identity_partial() {
        let token = make_test_token(r#"{"sub":"user-456"}"#);
        let (email, sub) = extract_identity(&token);
        assert_eq!(email, None);
        assert_eq!(sub, Some("user-456".into()));
    }

    #[test]
    fn parse_invalid_token() {
        assert_eq!(extract_client_id("not.a.valid.jwt.extra"), None);
        assert_eq!(extract_client_id("only-one-part"), None);
    }

    #[test]
    fn parse_token_with_existing_padding() {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let payload = r#"{"azp":"test-client"}"#;
        let encoded = engine.encode(payload.as_bytes());
        // 手动添加 padding（模拟非标准 token）
        let with_padding = format!("header.{}==.sig", encoded);
        assert_eq!(extract_client_id(&with_padding), Some("test-client".into()));
    }
}
