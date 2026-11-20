// 路由决策:model → provider → protocol

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Claude,
    /// 官方 Anthropic 订阅透传(原始字节转发、透传客户端 Authorization,仅 api.anthropic.com)
    AnthropicOfficial,
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    Gemini,
    Antigravity,
}

#[derive(Debug, Clone)]
pub struct RouteDecision {
    /// provider 名(日志 / unfit 登记等);name 允许重复,取配置请用 [`RouteDecision::provider_config`]
    pub provider: String,
    /// 命中 provider 在同一 providers 快照中的下标
    /// (对齐 CPA pickNext 直接返回选中的 *Auth,不按名二次查找)
    pub provider_index: usize,
    pub protocol: Protocol,
    pub upstream_model: String,
}

impl RouteDecision {
    /// 取命中的 provider 配置;`providers` 须为 [`resolve_route`] 使用的同一快照
    pub fn provider_config<'a>(
        &self,
        providers: &'a [ProviderConfig],
    ) -> Option<&'a ProviderConfig> {
        providers.get(self.provider_index)
    }
}

#[derive(Debug, Error)]
pub enum RouteError {
    #[error("模型未找到: {0}")]
    ModelNotFound(String),

    #[error("模型别名冲突: {0} 重复定义")]
    AliasConflict(String),

    #[error("provider {0} 未配置 base_url")]
    EmptyBaseUrl(String),

    #[error("provider {0} 未配置 key")]
    EmptyKey(String),

    #[error("官方 provider {0} 不得配置 key(订阅认证由客户端 Authorization 透传)")]
    OfficialNonEmptyKey(String),

    #[error("官方 provider {0} base_url 无效: {1}")]
    InvalidOfficialBaseUrl(String, String),

    #[error("官方 provider {0} 不得配置 metadata")]
    OfficialMetadataNotAllowed(String),

    #[error("官方 provider {0} 模型 alias({1}) 必须等于 name({2})")]
    OfficialAliasMismatch(String, String, String),

    #[error("provider {0} 通配模型条目无效: {1}")]
    InvalidWildcardModel(String, String),

    #[error("{0} 代理配置错误: {1}")]
    InvalidProxy(String, crate::proxy::ProxyError),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub protocol: Protocol,
    #[serde(
        deserialize_with = "deserialize_base_url",
        serialize_with = "serialize_base_url"
    )]
    base_url: Vec<String>,
    #[serde(default)]
    pub key: String,
    /// 覆盖全局代理;`direct`/`none` = 直连,缺省 = 用全局(全局也缺省为配置错误,见 [`crate::proxy`])
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// 注入派生 prompt_cache_key(provider 级开关,默认 false;
    /// 仅 openai_chat / openai_responses 生效)
    #[serde(default)]
    pub prompt_cache_key: bool,
    pub models: Vec<ModelConfig>,
    /// 额外元数据(用于 Antigravity 的 project_id 等)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<std::collections::HashMap<String, String>>,
}

impl ProviderConfig {
    /// 运行时构造 ProviderConfig（用于动态注入，如 Antigravity）
    pub fn new(
        name: String,
        protocol: Protocol,
        base_url: Vec<String>,
        key: String,
        proxy_url: Option<String>,
        prompt_cache_key: bool,
        models: Vec<ModelConfig>,
    ) -> Self {
        Self {
            name,
            protocol,
            base_url,
            key,
            proxy_url,
            prompt_cache_key,
            models,
            metadata: None,
        }
    }

    /// 设置 metadata (用于 Antigravity provider)
    pub fn with_metadata(mut self, metadata: std::collections::HashMap<String, String>) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// 返回 base_url 列表;gemini 多值回退用
    pub fn base_urls(&self) -> &[String] {
        &self.base_url
    }

    /// 设置单个 base_url (仅测试用，勿在生产代码中使用)
    #[doc(hidden)]
    pub fn set_base_url_for_test(&mut self, url: String) {
        self.base_url = vec![url];
    }
}

/// 自定义反序列化:兼容单字符串或数组
fn deserialize_base_url<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(s) => Ok(vec![s]),
        OneOrMany::Many(v) => Ok(v),
    }
}

/// 序列化 base_url:单元素写字符串,多元素写数组(保持输入风格)
fn serialize_base_url<S>(urls: &[String], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::Serialize;
    if urls.len() == 1 {
        urls[0].serialize(serializer)
    } else {
        urls.serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ModelConfig {
    pub name: String,  // 上游真实名
    pub alias: String, // 入站别名
    /// 模型上下文上限(可选);缺省 max_input_tokens=200000, max_tokens=64000
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
}

fn is_wildcard_model(model: &ModelConfig) -> bool {
    model.alias.ends_with('*') || model.name.ends_with('*')
}

/// 路由决策:从入站 model 解析到 (provider, protocol, upstream_model)
///
/// 启动时由 [`validate_providers`] 保证 alias 全局唯一。
/// provider name 允许重复;alias 可等于其他模型的 name(自身 alias==name 也允许)。
/// 精确阶段跳过通配条目;精确全部未命中时,再对 `AnthropicOfficial` 尝试前缀通配。
pub fn resolve_route(
    inbound_model: &str,
    providers: &[ProviderConfig],
) -> Result<RouteDecision, RouteError> {
    // alias 优先(Claude Code 主对话发别名);name 兜底(部分内部门如
    // count_tokens 发上游裸名);两者均跳过通配条目
    find_route(providers, |m| {
        !is_wildcard_model(m) && m.alias == inbound_model
    })
    .or_else(|| {
        find_route(providers, |m| {
            !is_wildcard_model(m) && m.name == inbound_model
        })
    })
    .or_else(|| find_official_wildcard_route(providers, inbound_model))
    .ok_or_else(|| RouteError::ModelNotFound(inbound_model.to_string()))
}

fn find_route(
    providers: &[ProviderConfig],
    matches: impl Fn(&ModelConfig) -> bool,
) -> Option<RouteDecision> {
    providers
        .iter()
        .enumerate()
        .find_map(|(provider_index, provider)| {
            let model = provider.models.iter().find(|m| matches(m))?;
            Some(RouteDecision {
                provider: provider.name.clone(),
                provider_index,
                protocol: provider.protocol,
                upstream_model: model.name.clone(),
            })
        })
}

fn find_official_wildcard_route(
    providers: &[ProviderConfig],
    inbound_model: &str,
) -> Option<RouteDecision> {
    providers
        .iter()
        .enumerate()
        .filter(|(_, p)| p.protocol == Protocol::AnthropicOfficial)
        .find_map(|(provider_index, provider)| {
            provider.models.iter().find_map(|m| {
                let prefix = m.alias.strip_suffix('*')?;
                if !prefix.starts_with("claude-") || prefix.contains('*') {
                    return None;
                }
                let rest = inbound_model.strip_prefix(prefix)?;
                if rest.is_empty() || rest.contains('*') {
                    return None;
                }
                Some(RouteDecision {
                    provider: provider.name.clone(),
                    provider_index,
                    protocol: provider.protocol,
                    // 原始字节直转:上游模型名即入站原值
                    upstream_model: inbound_model.to_string(),
                })
            })
        })
}

fn validate_official_base_url(provider_name: &str, urls: &[String]) -> Result<(), RouteError> {
    let invalid =
        |reason: &str| RouteError::InvalidOfficialBaseUrl(provider_name.to_string(), reason.into());
    if urls.len() != 1 {
        return Err(invalid("必须且只能配置单个 base_url"));
    }
    let raw = &urls[0];
    if raw.contains('@') || raw.contains('?') || raw.contains('#') {
        return Err(invalid("不得包含 userinfo(@)、query(?) 或 fragment(#)"));
    }
    let uri: http::Uri = raw.parse().map_err(|_| invalid("URL 解析失败"))?;
    if !uri
        .scheme_str()
        .is_some_and(|s| s.eq_ignore_ascii_case("https"))
    {
        return Err(invalid("scheme 必须为 https"));
    }
    let Some(authority) = uri.authority() else {
        return Err(invalid("缺少 host"));
    };
    let auth_str = authority.as_str();
    if !(auth_str.eq_ignore_ascii_case("api.anthropic.com")
        || auth_str.eq_ignore_ascii_case("api.anthropic.com:443"))
    {
        return Err(invalid("仅允许 https://api.anthropic.com(端口为空或 443)"));
    }
    if uri.query().is_some() {
        return Err(invalid("不得包含 query"));
    }
    if !matches!(uri.path(), "" | "/") {
        return Err(invalid("path 必须为空或 /"));
    }
    Ok(())
}

/// 启动与重载配置校验:alias 全局唯一、base_url、key、官方透传红线与通配约束
pub fn validate_providers(providers: &[ProviderConfig]) -> Result<(), RouteError> {
    let mut aliases: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for provider in providers {
        // base_url: [] 会在热路径索引越界/panic;允许 base_url: ""(部分动态 provider 占位)
        if provider.base_urls().is_empty() {
            return Err(RouteError::EmptyBaseUrl(provider.name.clone()));
        }
        if provider.protocol == Protocol::AnthropicOfficial {
            if !provider.key.is_empty() {
                return Err(RouteError::OfficialNonEmptyKey(provider.name.clone()));
            }
            validate_official_base_url(&provider.name, provider.base_urls())?;
            if provider.metadata.is_some() {
                return Err(RouteError::OfficialMetadataNotAllowed(
                    provider.name.clone(),
                ));
            }
            let mut wildcard_count = 0usize;
            for model in &provider.models {
                if !aliases.insert(model.alias.as_str()) {
                    return Err(RouteError::AliasConflict(model.alias.clone()));
                }
                if model.alias != model.name {
                    return Err(RouteError::OfficialAliasMismatch(
                        provider.name.clone(),
                        model.alias.clone(),
                        model.name.clone(),
                    ));
                }
                if model.alias.contains('*') {
                    let Some(prefix) = model.alias.strip_suffix('*') else {
                        return Err(RouteError::InvalidWildcardModel(
                            provider.name.clone(),
                            format!("{} 仅允许末尾单个 *", model.alias),
                        ));
                    };
                    if !prefix.starts_with("claude-") || prefix.contains('*') {
                        return Err(RouteError::InvalidWildcardModel(
                            provider.name.clone(),
                            format!("{} 必须为 claude-* 单星前缀", model.alias),
                        ));
                    }
                    wildcard_count += 1;
                    if wildcard_count > 1 {
                        return Err(RouteError::InvalidWildcardModel(
                            provider.name.clone(),
                            "每个官方 provider 最多允许一条通配条目".into(),
                        ));
                    }
                }
            }
        } else {
            if provider.key.trim().is_empty() {
                return Err(RouteError::EmptyKey(provider.name.clone()));
            }
            for model in &provider.models {
                if !aliases.insert(model.alias.as_str()) {
                    return Err(RouteError::AliasConflict(model.alias.clone()));
                }
                if model.alias.contains('*') || model.name.contains('*') {
                    return Err(RouteError::InvalidWildcardModel(
                        provider.name.clone(),
                        format!(
                            "仅 anthropic_official 允许通配条目(alias={}, name={})",
                            model.alias, model.name
                        ),
                    ));
                }
            }
        }
    }

    Ok(())
}

/// 校验全局代理(若配置)与每个 provider 的生效代理(provider 覆盖,否则全局)。
/// 启动与 `/reload` 共用;缺省或非法均为配置错误(见 [`crate::proxy`])。
pub fn validate_provider_proxies(
    providers: &[ProviderConfig],
    global_proxy: Option<&str>,
) -> Result<(), RouteError> {
    if global_proxy.is_some() {
        crate::proxy::parse_proxy(global_proxy)
            .map_err(|e| RouteError::InvalidProxy("server.proxy_url".into(), e))?;
    }
    for provider in providers {
        crate::proxy::resolve_proxy(provider.proxy_url.as_deref(), global_proxy)
            .map_err(|e| RouteError::InvalidProxy(format!("provider {}", provider.name), e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_route() {
        let yaml = r#"
name: test-claude
protocol: claude
base_url: "https://example.com"
key: sk-test
models:
  - name: claude-opus-5
    alias: test-opus-5
"#;
        let provider: ProviderConfig = yaml_serde::from_str(yaml).unwrap();
        let providers = vec![provider];

        let route = resolve_route("test-opus-5", &providers).unwrap();
        assert_eq!(route.provider, "test-claude");
        assert_eq!(route.protocol, Protocol::Claude);
        assert_eq!(route.upstream_model, "claude-opus-5");
    }

    #[test]
    fn test_model_not_found() {
        let providers = vec![];
        let err = resolve_route("unknown", &providers).unwrap_err();
        assert!(matches!(err, RouteError::ModelNotFound(_)));
    }

    #[test]
    fn test_alias_conflict() {
        let yaml = r#"
- name: provider-a
  protocol: claude
  base_url: "https://a.com"
  key: sk-a
  models:
    - name: model-a
      alias: shared-alias
- name: provider-b
  protocol: openai_chat
  base_url: "https://b.com"
  key: sk-b
  models:
    - name: model-b
      alias: shared-alias
"#;
        let providers: Vec<ProviderConfig> = yaml_serde::from_str(yaml).unwrap();

        let err = validate_providers(&providers).unwrap_err();
        assert!(matches!(err, RouteError::AliasConflict(_)));
    }

    #[test]
    fn test_empty_base_url_list_rejected_at_validate() {
        let yaml = r#"
- name: empty-url
  protocol: claude
  base_url: []
  key: sk-a
  models:
    - name: model-a
      alias: a
"#;
        let providers: Vec<ProviderConfig> = yaml_serde::from_str(yaml).unwrap();
        let err = validate_providers(&providers).unwrap_err();
        assert!(matches!(err, RouteError::EmptyBaseUrl(_)));
    }

    #[test]
    fn test_validate_provider_proxies() {
        let yaml = r#"
- name: inherit
  protocol: claude
  base_url: "https://a.com"
  key: sk-a
  models: []
- name: own
  protocol: claude
  base_url: "https://b.com"
  key: sk-b
  proxy_url: direct
  models: []
"#;
        let providers: Vec<ProviderConfig> = yaml_serde::from_str(yaml).unwrap();
        // 全局有效:inherit 继承全局,own 直连
        validate_provider_proxies(&providers, Some("http://127.0.0.1:7897")).unwrap();
        // 全局缺失:inherit 无可用代理 → 报错并指名 provider
        let err = validate_provider_proxies(&providers, None).unwrap_err();
        assert!(matches!(err, RouteError::InvalidProxy(ref n, _) if n == "provider inherit"));
        // 全局非法:即便所有 provider 自带代理也报错
        let err = validate_provider_proxies(&providers[1..], Some("ftp://x")).unwrap_err();
        assert!(matches!(err, RouteError::InvalidProxy(ref n, _) if n == "server.proxy_url"));
    }

    #[test]
    fn test_resolve_route_by_name_fallback() {
        // 部分内部门(如 count_tokens)发上游裸名,应能兜底路由到该 provider
        let yaml = r#"
name: ckff-codex
protocol: openai_responses
base_url: "https://ckff.dev"
key: sk-test
models:
  - name: gpt-5.6-terra
    alias: ck-gpt-5.6-terra
"#;
        let provider: ProviderConfig = yaml_serde::from_str(yaml).unwrap();
        let providers = vec![provider];

        // alias 优先
        let by_alias = resolve_route("ck-gpt-5.6-terra", &providers).unwrap();
        assert_eq!(by_alias.upstream_model, "gpt-5.6-terra");

        // name 兜底
        let by_name = resolve_route("gpt-5.6-terra", &providers).unwrap();
        assert_eq!(by_name.provider, "ckff-codex");
        assert_eq!(by_name.upstream_model, "gpt-5.6-terra");
    }

    #[test]
    fn test_gemini_protocol_parse() {
        let yaml = r#"
name: antigravity
protocol: gemini
base_url: ""
key: placeholder
models:
  - name: gemini-2.0-flash-thinking-exp
    alias: ag-gemini-thinking
"#;
        let provider: ProviderConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(provider.protocol, Protocol::Gemini);
        assert_eq!(provider.name, "antigravity");
        assert_eq!(provider.base_urls().len(), 1);
        assert_eq!(provider.base_urls()[0], "");
    }

    #[test]
    fn test_base_url_single_string() {
        let yaml = r#"
name: test
protocol: claude
base_url: "https://single.com"
key: sk-test
models:
  - name: model-1
    alias: m1
"#;
        let provider: ProviderConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(provider.base_urls().len(), 1);
        assert_eq!(provider.base_urls()[0], "https://single.com");
    }

    #[test]
    fn test_base_url_array() {
        let yaml = r#"
name: test
protocol: gemini
base_url:
  - "https://daily.example.com"
  - "https://prod.example.com"
key: sk-test
models:
  - name: model-1
    alias: m1
"#;
        let provider: ProviderConfig = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(provider.base_urls().len(), 2);
        assert_eq!(provider.base_urls()[0], "https://daily.example.com");
        assert_eq!(provider.base_urls()[1], "https://prod.example.com");
    }

    #[test]
    fn test_alias_equals_other_model_name_allowed() {
        // name 可重复用途的延伸:A.alias == B.name 允许;inbound 优先命中 alias
        let yaml = r#"
- name: provider-a
  protocol: claude
  base_url: "https://a.com"
  key: sk-a
  models:
    - name: model-a
      alias: model-b
- name: provider-b
  protocol: openai_chat
  base_url: "https://b.com"
  key: sk-b
  models:
    - name: model-b
      alias: b-alias
"#;
        let providers: Vec<ProviderConfig> = yaml_serde::from_str(yaml).unwrap();
        validate_providers(&providers).unwrap();
        let route = resolve_route("model-b", &providers).unwrap();
        assert_eq!(route.provider, "provider-a");
        assert_eq!(route.upstream_model, "model-a");
    }

    #[test]
    fn test_self_alias_equals_name_allowed() {
        let yaml = r#"
- name: provider-a
  protocol: claude
  base_url: "https://a.com"
  key: sk-a
  models:
    - name: same-id
      alias: same-id
"#;
        let providers: Vec<ProviderConfig> = yaml_serde::from_str(yaml).unwrap();
        validate_providers(&providers).unwrap();
    }

    #[test]
    fn test_duplicate_alias_rejected_within_and_across_same_name_providers() {
        let within = r#"
- name: p
  protocol: claude
  base_url: "https://a.com"
  key: sk-a
  models:
    - name: m1
      alias: dup
    - name: m2
      alias: dup
"#;
        let across = r#"
- name: same
  protocol: claude
  base_url: "https://a.com"
  key: sk-a
  models:
    - name: m1
      alias: dup
- name: same
  protocol: claude
  base_url: "https://b.com"
  key: sk-b
  models:
    - name: m2
      alias: dup
"#;
        for yaml in [within, across] {
            let providers: Vec<ProviderConfig> = yaml_serde::from_str(yaml).unwrap();
            let err = validate_providers(&providers).unwrap_err();
            assert!(matches!(err, RouteError::AliasConflict(_)));
        }
    }

    #[test]
    fn test_duplicate_provider_name_routes_to_matched_provider() {
        let yaml = r#"
- name: dup
  protocol: claude
  base_url: "https://a.com"
  key: sk-a
  models:
    - name: m-a
      alias: a1
- name: dup
  protocol: claude
  base_url: "https://b.com"
  key: sk-b
  models:
    - name: m-b
      alias: b1
"#;
        let providers: Vec<ProviderConfig> = yaml_serde::from_str(yaml).unwrap();
        validate_providers(&providers).unwrap();
        let route = resolve_route("b1", &providers).unwrap();
        assert_eq!(route.provider_index, 1);
        let provider = route.provider_config(&providers).unwrap();
        assert_eq!(provider.base_urls()[0], "https://b.com");
        assert_eq!(provider.key, "sk-b");
        // name 兜底同样带下标
        let by_name = resolve_route("m-b", &providers).unwrap();
        assert_eq!(by_name.provider_index, 1);
    }

    #[test]
    fn test_same_provider_alias_equals_sibling_name_allowed() {
        // 同 provider 内 alias 等于兄弟 name 也允许;仅 alias 彼此冲突才拒绝
        let yaml = r#"
- name: provider-a
  protocol: claude
  base_url: "https://a.com"
  key: sk-a
  models:
    - name: model-a
      alias: model-b
    - name: model-b
      alias: b-alias
"#;
        let providers: Vec<ProviderConfig> = yaml_serde::from_str(yaml).unwrap();
        validate_providers(&providers).unwrap();
    }

    fn official_provider(
        base_urls: Vec<&str>,
        key: &str,
        models: &[(&str, &str)],
    ) -> ProviderConfig {
        ProviderConfig::new(
            "anthropic-official".into(),
            Protocol::AnthropicOfficial,
            base_urls.into_iter().map(String::from).collect(),
            key.into(),
            Some("direct".into()),
            false,
            models
                .iter()
                .map(|(n, a)| ModelConfig {
                    name: (*n).into(),
                    alias: (*a).into(),
                    ..Default::default()
                })
                .collect(),
        )
    }

    #[test]
    fn test_official_protocol_serde_and_key_default() {
        let yaml = r#"
- name: anthropic-official
  protocol: anthropic_official
  base_url: "https://api.anthropic.com"
  models:
    - name: claude-opus-5-5
      alias: claude-opus-5-5
    - name: "claude-*"
      alias: "claude-*"
"#;
        let providers: Vec<ProviderConfig> = yaml_serde::from_str(yaml).unwrap();
        assert_eq!(providers[0].protocol, Protocol::AnthropicOfficial);
        assert!(providers[0].key.is_empty());
        validate_providers(&providers).unwrap();
    }

    #[test]
    fn test_official_validation_rules() {
        // 允许变体:无末尾斜杠、带 /、大小写 host、显式 :443
        for ok_url in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/",
            "https://API.ANTHROPIC.COM",
            "https://api.anthropic.com:443",
            "https://api.anthropic.com:443/",
        ] {
            let p = official_provider(vec![ok_url], "", &[("claude-opus-5-5", "claude-opus-5-5")]);
            validate_providers(&[p]).unwrap_or_else(|e| panic!("应通过 {ok_url}: {e}"));
        }

        // 拒绝各类非法 base_url
        for bad_url in [
            "https://api.anthropic.com@evil.example",
            "https://api.anthropic.com:8443",
            "https://api.anthropic.com/v1",
            "http://api.anthropic.com",
            "https://api.anthropic.com?x=1",
            "https://api.anthropic.com#x",
            "https://api.anthropic.com//",
            "https://evil.anthropic.com",
        ] {
            let p = official_provider(vec![bad_url], "", &[("claude-opus-5-5", "claude-opus-5-5")]);
            assert!(
                matches!(
                    validate_providers(&[p]),
                    Err(RouteError::InvalidOfficialBaseUrl(..))
                ),
                "应拒绝 {bad_url}"
            );
        }

        // 多 URL 拒绝
        let multi = official_provider(
            vec!["https://api.anthropic.com", "https://api.anthropic.com"],
            "",
            &[("claude-opus-5-5", "claude-opus-5-5")],
        );
        assert!(matches!(
            validate_providers(&[multi]),
            Err(RouteError::InvalidOfficialBaseUrl(..))
        ));

        // 官方非空 key(含空白)拒绝
        for bad_key in ["sk-ant-1", " "] {
            let p = official_provider(
                vec!["https://api.anthropic.com"],
                bad_key,
                &[("claude-opus-5-5", "claude-opus-5-5")],
            );
            assert!(matches!(
                validate_providers(&[p]),
                Err(RouteError::OfficialNonEmptyKey(_))
            ));
        }

        // 官方 metadata 拒绝
        let with_meta = official_provider(
            vec!["https://api.anthropic.com"],
            "",
            &[("claude-opus-5-5", "claude-opus-5-5")],
        )
        .with_metadata([("k".into(), "v".into())].into_iter().collect());
        assert!(matches!(
            validate_providers(&[with_meta]),
            Err(RouteError::OfficialMetadataNotAllowed(_))
        ));

        // 官方 alias != name 拒绝
        let mismatch = official_provider(
            vec!["https://api.anthropic.com"],
            "",
            &[("claude-opus-5-5", "my-opus")],
        );
        assert!(matches!(
            validate_providers(&[mismatch]),
            Err(RouteError::OfficialAliasMismatch(..))
        ));
    }

    #[test]
    fn test_non_official_empty_key_and_wildcard_rejected() {
        for bad_key in ["", "   "] {
            let p = ProviderConfig::new(
                "relay".into(),
                Protocol::Claude,
                vec!["https://example.com".into()],
                bad_key.into(),
                Some("direct".into()),
                false,
                vec![ModelConfig {
                    name: "claude-opus-5-5".into(),
                    alias: "claude-opus-5-5".into(),
                    ..Default::default()
                }],
            );
            assert!(matches!(
                validate_providers(&[p]),
                Err(RouteError::EmptyKey(_))
            ));
        }

        let relay_wc = ProviderConfig::new(
            "relay".into(),
            Protocol::Claude,
            vec!["https://example.com".into()],
            "sk-relay".into(),
            Some("direct".into()),
            false,
            vec![ModelConfig {
                name: "claude-*".into(),
                alias: "claude-*".into(),
                ..Default::default()
            }],
        );
        assert!(matches!(
            validate_providers(&[relay_wc]),
            Err(RouteError::InvalidWildcardModel(..))
        ));
    }

    #[test]
    fn test_official_wildcard_validation_and_routing() {
        // 合法通配 + 精确条目
        let ok = official_provider(
            vec!["https://api.anthropic.com"],
            "",
            &[
                ("claude-opus-5-5", "claude-opus-5-5"),
                ("claude-*", "claude-*"),
            ],
        );
        validate_providers(std::slice::from_ref(&ok)).unwrap();

        // 两个通配条目被拒
        let two = official_provider(
            vec!["https://api.anthropic.com"],
            "",
            &[("claude-opus-*", "claude-opus-*"), ("claude-*", "claude-*")],
        );
        assert!(matches!(
            validate_providers(&[two]),
            Err(RouteError::InvalidWildcardModel(..))
        ));

        // 非法通配模式被拒
        for bad in ["claude-*-*", "claude-**", "*", "gpt-*", "claude-*x"] {
            let p = official_provider(vec!["https://api.anthropic.com"], "", &[(bad, bad)]);
            assert!(
                matches!(
                    validate_providers(&[p]),
                    Err(RouteError::InvalidWildcardModel(..))
                ),
                "应拒绝非法通配 {bad}"
            );
        }

        // 路由:精确优先、快照通配、边界拒绝
        let relay = ProviderConfig::new(
            "relay".into(),
            Protocol::Claude,
            vec!["https://relay.example".into()],
            "sk-relay".into(),
            Some("direct".into()),
            false,
            vec![ModelConfig {
                name: "claude-exact-relay".into(),
                alias: "claude-exact-relay".into(),
                ..Default::default()
            }],
        );
        let providers = vec![ok, relay];

        let snap = resolve_route("claude-haiku-4-5-20251001", &providers).unwrap();
        assert_eq!(snap.protocol, Protocol::AnthropicOfficial);
        assert_eq!(snap.upstream_model, "claude-haiku-4-5-20251001");

        let exact_other = resolve_route("claude-exact-relay", &providers).unwrap();
        assert_eq!(exact_other.protocol, Protocol::Claude);
        assert_eq!(exact_other.provider, "relay");

        let future_snap = resolve_route("claude-sonnet-9-9-20300101", &providers).unwrap();
        assert_eq!(future_snap.protocol, Protocol::AnthropicOfficial);
        assert_eq!(future_snap.upstream_model, "claude-sonnet-9-9-20300101");

        for miss in [
            "gpt-x",
            "claude-*",
            "claude-",
            "claude-**",
            "claude-foo*bar",
        ] {
            assert!(
                matches!(
                    resolve_route(miss, &providers),
                    Err(RouteError::ModelNotFound(_))
                ),
                "应未命中 {miss}"
            );
        }
    }
}
