// 路由决策:model → provider → protocol

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Claude,
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
    pub key: String,
    /// 覆盖全局代理;Some("direct") = 直连(缺省 = 用全局)
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// 注入派生 prompt_cache_key(provider 级开关,默认 false;
    /// 仅 openai_chat / openai_responses 生效)
    #[serde(default)]
    pub prompt_cache_key: bool,
    #[serde(serialize_with = "serialize_nonempty_vec")]
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

/// 空数组规范化:输出 [] 不是 null(对齐 magpie orNone)
fn serialize_nonempty_vec<T, S>(vec: &[T], serializer: S) -> Result<S::Ok, S::Error>
where
    T: Serialize,
    S: serde::Serializer,
{
    use serde::Serialize;
    vec.serialize(serializer)
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

/// 路由决策:从入站 model 解析到 (provider, protocol, upstream_model)
///
/// 启动时由 [`validate_providers`] 保证 alias 全局唯一。
/// provider name 允许重复;alias 可等于其他模型的 name(自身 alias==name 也允许)。
pub fn resolve_route(
    inbound_model: &str,
    providers: &[ProviderConfig],
) -> Result<RouteDecision, RouteError> {
    // alias 优先(Claude Code 主对话发别名);name 兜底(部分内部门如
    // count_tokens 发上游裸名)
    find_route(providers, |m| m.alias == inbound_model)
        .or_else(|| find_route(providers, |m| m.name == inbound_model))
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

/// 启动时验证配置:检查 alias 全局唯一(跨 provider 与同 provider 内)与空 base_url
pub fn validate_providers(providers: &[ProviderConfig]) -> Result<(), RouteError> {
    let mut aliases: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for provider in providers {
        // base_url: [] 会在热路径索引越界/panic;允许 base_url: ""(部分动态 provider 占位)
        if provider.base_urls().is_empty() {
            return Err(RouteError::EmptyBaseUrl(provider.name.clone()));
        }
        for model in &provider.models {
            if !aliases.insert(model.alias.as_str()) {
                return Err(RouteError::AliasConflict(model.alias.clone()));
            }
        }
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
}
