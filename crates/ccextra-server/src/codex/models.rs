// Codex 静态模型列表 (对齐项目 models.json 已注册的 GPT 模型)

use ccextra_core::route::ModelConfig;

/// 返回默认支持的 Codex 订阅模型列表
pub fn default_codex_models() -> Vec<ModelConfig> {
    vec![
        ModelConfig {
            name: "gpt-5.6-terra".to_string(),
            alias: "gpt-5.6-terra".to_string(),
            max_input_tokens: None,
            max_tokens: None,
        },
        ModelConfig {
            name: "gpt-5.6-sol".to_string(),
            alias: "gpt-5.6-sol".to_string(),
            max_input_tokens: None,
            max_tokens: None,
        },
        ModelConfig {
            name: "gpt-6-astra".to_string(),
            alias: "gpt-6-astra".to_string(),
            max_input_tokens: None,
            max_tokens: None,
        },
    ]
}
