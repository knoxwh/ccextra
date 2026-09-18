use crate::http::error::AppError;
use axum::http::HeaderMap;
use ccextra_core::secret::looks_like_bcrypt;
use std::collections::HashMap;
use std::sync::{Mutex as StdMutex, OnceLock};

/// bcrypt 验证结果缓存:key→已验证,避免每请求一次 ~100ms 的 bcrypt verify
/// 上限 1024 条,超限清空(防内存无限增长);secret 可热重载,/reload 一律清空缓存
static AUTH_CACHE: OnceLock<StdMutex<HashMap<String, bool>>> = OnceLock::new();

pub fn auth_cache() -> &'static StdMutex<HashMap<String, bool>> {
    AUTH_CACHE.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// bcrypt 校验(带缓存;锁毒化时降级直验)
pub fn verify_cached(key: &str, expected: &str) -> bool {
    match auth_cache().lock() {
        Ok(mut cache) => {
            if let Some(&ok) = cache.get(key) {
                return ok;
            }
            let ok = bcrypt::verify(key, expected).unwrap_or(false);
            if cache.len() >= 1024 {
                cache.clear();
            }
            cache.insert(key.to_string(), ok);
            ok
        }
        Err(_) => bcrypt::verify(key, expected).unwrap_or(false),
    }
}

/// 校验入口 key:secret 未配置时放行;配置时需匹配,否则 401
/// 支持 x-api-key 与 Authorization: Bearer 两种头(兼容 cc-switch 等工具)
/// secret 为 bcrypt 哈希时用 verify,否则明文比对(便于测试/旧配置)
pub fn check_secret(headers: &HeaderMap, secret: &Option<String>) -> Result<(), AppError> {
    let Some(expected) = secret else {
        return Ok(());
    };
    let got = extract_key(headers);
    let ok = if looks_like_bcrypt(expected) {
        verify_cached(got, expected)
    } else {
        got == expected
    };
    if ok {
        Ok(())
    } else {
        Err(AppError::unauthorized(
            "x-api-key 或 Authorization 缺失/不匹配",
        ))
    }
}

/// 从请求头提取 key:x-api-key 优先,其次 Authorization: Bearer
/// scheme 大小写不敏感(RFC 6750),容忍任意空白
pub fn extract_key(headers: &HeaderMap) -> &str {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        if !v.is_empty() {
            return v;
        }
    }
    if let Some(v) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some((scheme, token)) = v.split_once(char::is_whitespace) {
            if scheme.eq_ignore_ascii_case("bearer") {
                let token = token.trim();
                if !token.is_empty() {
                    return token;
                }
            }
        }
    }
    ""
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn test_extract_key_prefers_x_api_key() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("key-1"));
        headers.insert("authorization", HeaderValue::from_static("Bearer key-2"));
        assert_eq!(extract_key(&headers), "key-1");
    }

    #[test]
    fn test_extract_key_fallback_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer secret-token"),
        );
        assert_eq!(extract_key(&headers), "secret-token");

        headers.insert(
            "authorization",
            HeaderValue::from_static("bearer   token2  "),
        );
        assert_eq!(extract_key(&headers), "token2");
    }

    #[test]
    fn test_extract_key_missing_or_empty() {
        let headers = HeaderMap::new();
        assert_eq!(extract_key(&headers), "");
    }
}
