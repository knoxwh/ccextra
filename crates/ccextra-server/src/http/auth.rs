use crate::http::error::AppError;
use axum::http::HeaderMap;
use ccextra_core::secret::looks_like_bcrypt;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Mutex as StdMutex, OnceLock};

/// bcrypt 验证结果缓存:(expected hash, key) → 已验证,避免每请求一次 ~100ms 的 bcrypt verify。
/// 键绑定 expected hash:reload 换 secret 后,旧 secret 的 true/false 结果不可命中
/// (旧 true 不得让新 secret 错误接受 key,旧 false 不得错误拒绝合法 key)。
/// 上限 1024 条,满时淘汰最久未访问条目;secret 可热重载,/reload 一律清空缓存。
static AUTH_CACHE: OnceLock<StdMutex<LruCache<String, bool>>> = OnceLock::new();

const AUTH_CACHE_CAPACITY: usize = 1024;

pub fn auth_cache() -> &'static StdMutex<LruCache<String, bool>> {
    AUTH_CACHE.get_or_init(|| {
        StdMutex::new(LruCache::new(
            NonZeroUsize::new(AUTH_CACHE_CAPACITY).expect("缓存容量非零"),
        ))
    })
}

/// 缓存键:expected hash 与 key 以 NUL 拼接(两者均不可能含 NUL:bcrypt hash
/// 为受限字符集,header 提取的 key 不含控制字符),保证不同 secret 互不串扰
fn cache_key(expected: &str, key: &str) -> String {
    format!("{expected}\u{0}{key}")
}

/// bcrypt 校验(带缓存;锁毒化时降级直验)
pub fn verify_cached(key: &str, expected: &str) -> bool {
    match auth_cache().lock() {
        Ok(mut cache) => verify_with_cache(&mut cache, key, expected),
        Err(_) => bcrypt::verify(key, expected).unwrap_or(false),
    }
}

/// 对给定缓存执行「查缓存 → bcrypt 验证 → 回填」。
/// 独立于全局缓存,便于测试用私有缓存实例复现 reload 竞争窗口。
fn verify_with_cache(cache: &mut LruCache<String, bool>, key: &str, expected: &str) -> bool {
    let ck = cache_key(expected, key);
    if let Some(&ok) = cache.get(&ck) {
        return ok;
    }
    let ok = bcrypt::verify(key, expected).unwrap_or(false);
    cache.put(ck, ok);
    ok
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

    // ── B3:认证缓存按 expected hash 隔离 ──────────────────────────────
    // 竞争窗口一:新 secret 已发布、缓存未清,新请求命中旧结果
    // 竞争窗口二:旧请求复制旧 secret 后暂停,reload 清缓存后恢复并回填旧结果
    // 两个窗口都要求:旧 hash 下的 true/false 不得影响新 hash 的判定

    fn fresh_cache() -> LruCache<String, bool> {
        LruCache::new(NonZeroUsize::new(AUTH_CACHE_CAPACITY).expect("容量非零"))
    }

    #[test]
    fn cache_isolates_both_results_across_reload_windows() {
        let old_hash = bcrypt::hash("sk-old", 4).unwrap();
        let new_hash = bcrypt::hash("sk-new", 4).unwrap();
        for backfill in [false, true] {
            for (key, old_result) in [("sk-old", true), ("sk-new", false)] {
                let mut cache = fresh_cache();
                assert_eq!(verify_with_cache(&mut cache, key, &old_hash), old_result);
                if backfill {
                    cache.clear();
                    assert_eq!(verify_with_cache(&mut cache, key, &old_hash), old_result);
                }
                assert_eq!(verify_with_cache(&mut cache, key, &new_hash), !old_result);
            }
        }
    }

    #[test]
    fn cache_evicts_lru_and_refreshes_hits() {
        for hit_oldest in [false, true] {
            let mut cache = fresh_cache();
            for i in 0..AUTH_CACHE_CAPACITY {
                cache.put(cache_key("h", &format!("k{i}")), true);
            }
            if hit_oldest {
                assert!(verify_with_cache(&mut cache, "k0", "h"));
            }
            // 经生产回填路径插入,只淘汰一条;无效 hash 的 false 也缓存。
            assert!(!verify_with_cache(&mut cache, "k-new", "h"));
            assert_eq!(cache.len(), AUTH_CACHE_CAPACITY);
            assert_eq!(cache.contains(&cache_key("h", "k0")), hit_oldest);
            assert_eq!(cache.contains(&cache_key("h", "k1")), !hit_oldest);
            assert_eq!(cache.peek(&cache_key("h", "k-new")), Some(&false));
        }
    }
}
