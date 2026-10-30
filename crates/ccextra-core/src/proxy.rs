//! 代理配置解析(纯逻辑):数据面、Claude 指纹车道与四个 OAuth client 共用一套规则。
//!
//! 规则(对齐 CPA `proxyutil.Parse`,唯一偏离:缺省/空值不继承环境或系统代理,直接报错):
//! - 缺省或空白:配置错误
//! - `direct` / `none`(trim 后大小写不敏感):直连
//! - `http` / `https` / `socks5h` URL:原样使用
//! - `socks5` URL:改写为 `socks5h`(代理端解析 DNS,对齐 CPA/Go x/net socks 的远端解析)
//! - 其他(无 scheme、无 host、未知 scheme、非法 URL):配置错误

/// 解析后的代理设置
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxySetting {
    /// 直连(不读环境变量与系统代理)
    Direct,
    /// 规范化后的代理 URL
    Url(String),
}

impl ProxySetting {
    /// 连接池缓存键:直连为 `"direct"`,否则为规范化 URL
    pub fn key(&self) -> &str {
        match self {
            Self::Direct => "direct",
            Self::Url(url) => url,
        }
    }
}

/// 代理配置错误
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProxyError {
    #[error("未配置代理:请设置 server.proxy_url 或 provider proxy_url(直连写 direct)")]
    Missing,
    #[error("proxy_url 为空(直连写 direct)")]
    Empty,
    #[error("proxy_url {0:?} 无效: {1}")]
    Invalid(String, &'static str),
}

/// 解析单个代理值;`None` 表示未配置
pub fn parse_proxy(raw: Option<&str>) -> Result<ProxySetting, ProxyError> {
    let raw = raw.ok_or(ProxyError::Missing)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ProxyError::Empty);
    }
    if trimmed.eq_ignore_ascii_case("direct") || trimmed.eq_ignore_ascii_case("none") {
        return Ok(ProxySetting::Direct);
    }
    let invalid = |reason| ProxyError::Invalid(raw.to_string(), reason);
    let (scheme, rest) = trimmed
        .split_once("://")
        .ok_or_else(|| invalid("缺少 scheme(http/https/socks5/socks5h)"))?;
    let uri: http::Uri = trimmed.parse().map_err(|_| invalid("URL 非法"))?;
    if uri.host().map_or(true, str::is_empty) {
        return Err(invalid("缺少 host"));
    }
    let scheme = scheme.to_ascii_lowercase();
    match scheme.as_str() {
        "http" | "https" | "socks5h" => Ok(ProxySetting::Url(format!("{scheme}://{rest}"))),
        "socks5" => Ok(ProxySetting::Url(format!("socks5h://{rest}"))),
        _ => Err(invalid("不支持的 scheme(仅 http/https/socks5/socks5h)")),
    }
}

/// 生效代理:provider 覆盖优先,否则用全局;两者都未配置则报错
pub fn resolve_proxy(
    provider_proxy: Option<&str>,
    global_proxy: Option<&str>,
) -> Result<ProxySetting, ProxyError> {
    parse_proxy(provider_proxy.or(global_proxy))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Result<ProxySetting, ProxyError> {
        Ok(ProxySetting::Url(s.to_string()))
    }

    #[test]
    fn parse_proxy_table() {
        let invalid = |s: &str| matches!(parse_proxy(Some(s)), Err(ProxyError::Invalid(..)));
        assert_eq!(parse_proxy(None), Err(ProxyError::Missing));
        assert_eq!(parse_proxy(Some("")), Err(ProxyError::Empty));
        assert_eq!(parse_proxy(Some("   ")), Err(ProxyError::Empty));
        assert_eq!(parse_proxy(Some(" Direct ")), Ok(ProxySetting::Direct));
        assert_eq!(parse_proxy(Some("NONE")), Ok(ProxySetting::Direct));
        assert_eq!(parse_proxy(Some("http://h:1")), url("http://h:1"));
        assert_eq!(parse_proxy(Some(" https://h:1 ")), url("https://h:1"));
        assert_eq!(parse_proxy(Some("HTTP://h:1")), url("http://h:1"));
        assert_eq!(parse_proxy(Some("socks5://h:1")), url("socks5h://h:1"));
        assert_eq!(
            parse_proxy(Some("socks5://u:p@h:1")),
            url("socks5h://u:p@h:1")
        );
        assert_eq!(parse_proxy(Some("socks5h://h:1")), url("socks5h://h:1"));
        assert!(invalid("ftp://h"));
        assert!(invalid("none2"));
        assert!(invalid("127.0.0.1:7897"));
        assert!(invalid("http://"));
        assert!(invalid("http://h:1 x"));
    }

    #[test]
    fn resolve_prefers_provider_and_errors_when_both_missing() {
        assert_eq!(
            resolve_proxy(Some("direct"), Some("http://g:1")),
            Ok(ProxySetting::Direct)
        );
        assert_eq!(resolve_proxy(None, Some("http://g:1")), url("http://g:1"));
        assert_eq!(resolve_proxy(None, None), Err(ProxyError::Missing));
        // provider 写了空串不回落全局:空值本身是配置错误
        assert_eq!(
            resolve_proxy(Some(""), Some("http://g:1")),
            Err(ProxyError::Empty)
        );
        assert_eq!(ProxySetting::Direct.key(), "direct");
    }
}
