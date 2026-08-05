// 配置文件加载与解析

use anyhow::Result;
use ccextra_core::route::ProviderConfig;
use ccextra_server::http::{LoggingConfig, NormalizeConfig, PayloadRule};
use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub providers: Vec<ProviderConfig>,
    pub payload: Option<Vec<PayloadRule>>,
    pub normalize: NormalizeConfig,
    pub logging: LoggingConfig,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    /// 全局代理兜底(可选);"direct"/"" = 直连
    #[serde(default)]
    pub proxy_url: Option<String>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let config: Config = serde_yaml::from_str(&content)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_load_valid_config() {
        let yaml = r#"
server:
  host: "127.0.0.1"
  port: 8222

providers:
  - name: test-provider
    protocol: claude
    base_url: https://example.com
    key: sk-test
    models:
      - name: claude-opus-5
        alias: test-opus

normalize:
  enabled: true
  drift_detector: false

logging:
  level: info
  request_body: false
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        let config = Config::load(file.path().to_str().unwrap()).unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 8222);
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.providers[0].name, "test-provider");
    }

    #[test]
    fn test_load_with_proxy_url() {
        let yaml = r#"
server:
  host: "0.0.0.0"
  port: 8080
  proxy_url: "http://proxy:7890"

providers:
  - name: test
    protocol: openai_chat
    base_url: https://api.openai.com
    key: sk-xxx
    proxy_url: "direct"
    models:
      - name: gpt-4
        alias: gpt4

normalize:
  enabled: false
  drift_detector: false

logging:
  level: debug
  request_body: true
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        let config = Config::load(file.path().to_str().unwrap()).unwrap();
        assert_eq!(config.server.proxy_url, Some("http://proxy:7890".into()));
        assert_eq!(config.providers[0].proxy_url, Some("direct".into()));
        assert!(config.logging.request_body);
    }

    #[test]
    fn test_load_with_payload_rules() {
        let yaml = r#"
server:
  host: "127.0.0.1"
  port: 8222

providers:
  - name: test
    protocol: claude
    base_url: https://example.com
    key: sk-test
    models:
      - name: model-1
        alias: m1

payload:
  - models:
      - "*glm*"
      - "*kimi*"
    params:
      max_tokens: 32000
      temperature: 0.1

normalize:
  enabled: true
  drift_detector: true

logging:
  level: info
  request_body: false
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        let config = Config::load(file.path().to_str().unwrap()).unwrap();
        assert!(config.payload.is_some());
        let rules = config.payload.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].models.len(), 2);
        assert_eq!(rules[0].params.get("max_tokens").unwrap(), &serde_json::json!(32000));
    }

    #[test]
    fn test_load_missing_file() {
        let result = Config::load("/nonexistent/config.yaml");
        assert!(result.is_err());
    }

    #[test]
    fn test_load_invalid_yaml() {
        let yaml = "this is not valid: yaml: content:";
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        let result = Config::load(file.path().to_str().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn test_load_missing_required_field() {
        let yaml = r#"
server:
  host: "127.0.0.1"
  port: 8222

providers: []

normalize:
  enabled: true
  drift_detector: false
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        // 缺少 logging 字段,应解析失败
        let result = Config::load(file.path().to_str().unwrap());
        assert!(result.is_err());
    }
}
