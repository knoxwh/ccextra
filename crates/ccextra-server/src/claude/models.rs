// Claude 订阅默认模型列表 (对齐 config.example.yaml 常见别名)

use ccextra_core::route::ModelConfig;

/// 返回默认支持的 Claude 订阅模型列表
pub fn default_claude_models() -> Vec<ModelConfig> {
    vec![
        ModelConfig {
            name: "claude-opus-5".to_string(),
            alias: "claude-opus-5".to_string(),
            max_input_tokens: Some(200_000),
            max_tokens: Some(64_000),
        },
        ModelConfig {
            name: "claude-sonnet-5".to_string(),
            alias: "claude-sonnet-5".to_string(),
            max_input_tokens: None,
            max_tokens: None,
        },
        ModelConfig {
            name: "claude-fable-5".to_string(),
            alias: "claude-fable-5".to_string(),
            max_input_tokens: None,
            max_tokens: None,
        },
    ]
}
