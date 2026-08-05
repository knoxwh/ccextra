// 路由决策:model → provider → protocol

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Claude,
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
}

#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub provider: String,
    pub protocol: Protocol,
    pub upstream_model: String,
}

#[derive(Debug, Error)]
pub enum RouteError {
    #[error("模型未找到: {0}")]
    ModelNotFound(String),

    #[error("模型别名冲突: {0} 在多个 provider 中定义")]
    AliasConflict(String),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub key: String,
    /// 覆盖全局代理;Some("direct") = 直连(缺省 = 用全局)
    #[serde(default)]
    pub proxy_url: Option<String>,
    /// 注入派生 prompt_cache_key(对齐 CPA support-prompt-cache-key,默认 false;
    /// 仅 openai_chat / openai_responses 生效)
    #[serde(default)]
    pub prompt_cache_key: bool,
    pub models: Vec<ModelConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
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
/// 启动时验证:
/// - 所有 alias 唯一(跨 provider 不能重复)
/// - 同一 provider 内 alias 与 name 不冲突
pub fn resolve_route(
    inbound_model: &str,
    providers: &[ProviderConfig],
) -> Result<RouteDecision, RouteError> {
    // alias 优先(Claude Code 主对话发别名);name 兜底(部分内部门如
    // count_tokens 发上游裸名,见 CPA countTokens 场景)
    for provider in providers {
        for model in &provider.models {
            if model.alias == inbound_model {
                return Ok(RouteDecision {
                    provider: provider.name.clone(),
                    protocol: provider.protocol,
                    upstream_model: model.name.clone(),
                });
            }
        }
    }
    for provider in providers {
        for model in &provider.models {
            if model.name == inbound_model {
                return Ok(RouteDecision {
                    provider: provider.name.clone(),
                    protocol: provider.protocol,
                    upstream_model: model.name.clone(),
                });
            }
        }
    }

    Err(RouteError::ModelNotFound(inbound_model.to_string()))
}

/// 启动时验证配置:检查 alias 冲突
pub fn validate_providers(providers: &[ProviderConfig]) -> Result<(), RouteError> {
    use std::collections::HashMap;

    let mut alias_map: HashMap<&str, &str> = HashMap::new();

    for provider in providers {
        for model in &provider.models {
            if let Some(existing_provider) = alias_map.get(model.alias.as_str()) {
                if existing_provider != &provider.name.as_str() {
                    return Err(RouteError::AliasConflict(model.alias.clone()));
                }
            }
            alias_map.insert(&model.alias, &provider.name);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_route() {
        let providers = vec![ProviderConfig {
            name: "test-claude".into(),
            protocol: Protocol::Claude,
            base_url: "https://example.com".into(),
            key: "sk-test".into(),
            proxy_url: None,
            prompt_cache_key: false,
            models: vec![ModelConfig {
                name: "claude-opus-5".into(),
                alias: "test-opus-5".into(),
                ..Default::default()
            }],
        }];

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
        let providers = vec![
            ProviderConfig {
                name: "provider-a".into(),
                protocol: Protocol::Claude,
                base_url: "https://a.com".into(),
                key: "sk-a".into(),
                proxy_url: None,
                prompt_cache_key: false,
                models: vec![ModelConfig {
                    name: "model-a".into(),
                    alias: "shared-alias".into(),
                    ..Default::default()
                }],
            },
            ProviderConfig {
                name: "provider-b".into(),
                protocol: Protocol::OpenAiChat,
                base_url: "https://b.com".into(),
                key: "sk-b".into(),
                proxy_url: None,
                prompt_cache_key: false,
                models: vec![ModelConfig {
                    name: "model-b".into(),
                    alias: "shared-alias".into(),
                    ..Default::default()
                }],
            },
        ];

        let err = validate_providers(&providers).unwrap_err();
        assert!(matches!(err, RouteError::AliasConflict(_)));
    }

    #[test]
    fn test_resolve_route_by_name_fallback() {
        // 部分内部门(如 count_tokens)发上游裸名,应能兜底路由到该 provider
        let providers = vec![ProviderConfig {
            name: "ckff-codex".into(),
            protocol: Protocol::OpenAiResponses,
            base_url: "https://ckff.dev".into(),
            key: "sk-test".into(),
            proxy_url: None,
            prompt_cache_key: false,
            models: vec![ModelConfig {
                name: "gpt-5.6-terra".into(),
                alias: "ck-gpt-5.6-terra".into(),
                ..Default::default()
            }],
        }];

        // alias 优先
        let by_alias = resolve_route("ck-gpt-5.6-terra", &providers).unwrap();
        assert_eq!(by_alias.upstream_model, "gpt-5.6-terra");

        // name 兜底
        let by_name = resolve_route("gpt-5.6-terra", &providers).unwrap();
        assert_eq!(by_name.provider, "ckff-codex");
        assert_eq!(by_name.upstream_model, "gpt-5.6-terra");
    }
}
