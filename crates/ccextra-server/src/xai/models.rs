// xAI Grok 标准模型列表定义 (对齐 CLIProxyAPI registry/model_definitions.go)

use ccextra_core::route::ModelConfig;

/// 返回默认支持的 Grok 模型列表
pub fn default_grok_models() -> Vec<ModelConfig> {
    vec![
        ModelConfig {
            name: "grok-4.6".to_string(),
            alias: "grok-4.6".to_string(),
            max_input_tokens: Some(256000),
            max_tokens: Some(131072),
        },
        ModelConfig {
            name: "grok-4.5".to_string(),
            alias: "grok-4.5".to_string(),
            max_input_tokens: Some(256000),
            max_tokens: Some(131072),
        },
        ModelConfig {
            name: "grok-4.3".to_string(),
            alias: "grok-4.3".to_string(),
            max_input_tokens: Some(256000),
            max_tokens: Some(131072),
        },
        ModelConfig {
            name: "grok-4.6".to_string(),
            alias: "grok-latest".to_string(),
            max_input_tokens: Some(256000),
            max_tokens: Some(131072),
        },
        ModelConfig {
            name: "grok-3-mini".to_string(),
            alias: "grok-3-mini".to_string(),
            max_input_tokens: Some(131072),
            max_tokens: Some(65536),
        },
        ModelConfig {
            name: "grok-3-mini-fast".to_string(),
            alias: "grok-3-mini-fast".to_string(),
            max_input_tokens: Some(131072),
            max_tokens: Some(65536),
        },
    ]
}
