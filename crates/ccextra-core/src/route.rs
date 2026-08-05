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
    pub models: Vec<ModelConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    pub name: String,      // 上游真实名
    pub alias: String,     // 入站别名
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
        let providers = vec![
            ProviderConfig {
                name: "evol-claude".into(),
                protocol: Protocol::Claude,
                base_url: "https://example.com".into(),
                key: "sk-test".into(),
                proxy_url: None,
                models: vec![ModelConfig {
                    name: "claude-opus-5".into(),
                    alias: "evol-opus-5".into(),
                }],
            },
        ];

        let route = resolve_route("evol-opus-5", &providers).unwrap();
        assert_eq!(route.provider, "evol-claude");
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
                models: vec![ModelConfig {
                    name: "model-a".into(),
                    alias: "shared-alias".into(),
                }],
            },
            ProviderConfig {
                name: "provider-b".into(),
                protocol: Protocol::OpenAiChat,
                base_url: "https://b.com".into(),
                key: "sk-b".into(),
                proxy_url: None,
                models: vec![ModelConfig {
                    name: "model-b".into(),
                    alias: "shared-alias".into(),
                }],
            },
        ];

        let err = validate_providers(&providers).unwrap_err();
        assert!(matches!(err, RouteError::AliasConflict(_)));
    }
}
