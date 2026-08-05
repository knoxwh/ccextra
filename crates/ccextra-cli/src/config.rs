// 配置文件加载与解析

use anyhow::Result;
use ccextra_core::route::ProviderConfig;
use ccextra_core::secret::looks_like_bcrypt;
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
    /// 入口 secret key(可选);配置后 /v1/models 与 /v1/messages 需 x-api-key 匹配
    #[serde(default)]
    pub secret_key: Option<String>,
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
        let mut config: Config = serde_yaml::from_str(&content)?;

        // 参考 CPA:secret_key 明文自动转 bcrypt,并回写配置文件(下次启动识别哈希不重复转)
        if let Some(secret) = &config.secret_key {
            if !secret.is_empty() && !looks_like_bcrypt(secret) {
                let hashed = bcrypt::hash(secret, bcrypt::DEFAULT_COST)?;
                config.secret_key = Some(hashed.clone());
                persist_secret(path, &content, &hashed)?;
            }
        }

        Ok(config)
    }
}

/// 回写 secret_key 哈希到配置文件(保留其余内容与缩进;保留行尾注释)
fn persist_secret(path: &str, content: &str, hashed: &str) -> Result<()> {
    let mut out = String::with_capacity(content.len() + 16);
    for line in content.lines() {
        // 用字符索引找首个非空白字节偏移,避免非 ASCII 空白时字节切片 panic
        let indent_len = line
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(line.len());
        if line[indent_len..].starts_with("secret_key:") {
            let indent = &line[..indent_len];
            let comment = find_trailing_comment(&line[indent_len..]);
            match comment {
                Some(c) => out.push_str(&format!("{indent}secret_key: \"{hashed}\" {c}\n")),
                None => out.push_str(&format!("{indent}secret_key: \"{hashed}\"\n")),
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    fs::write(path, out)?;
    Ok(())
}

/// 提取行尾注释(引号外的 `#` 之后),保留重写行时不会丢注释
fn find_trailing_comment(line: &str) -> Option<&str> {
    let mut in_quotes = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => {
                let comment = line[i..].trim();
                return if comment.is_empty() { None } else { Some(comment) };
            }
            _ => {}
        }
    }
    None
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
    fn test_load_prompt_cache_key_flag() {
        // provider 级 prompt_cache_key 开关:缺省 false,显式 true 生效
        let yaml = r#"
server:
  host: "127.0.0.1"
  port: 8222

providers:
  - name: with-flag
    protocol: openai_responses
    base_url: https://example.com
    key: sk-test
    prompt_cache_key: true
    models:
      - name: m1
        alias: a1
  - name: without-flag
    protocol: openai_chat
    base_url: https://example.com
    key: sk-test
    models:
      - name: m2
        alias: a2

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
        assert!(config.providers[0].prompt_cache_key);
        assert!(!config.providers[1].prompt_cache_key);
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
    fn test_secret_key_plaintext_hashed_and_persisted() {
        let yaml = r#"
server:
  host: "127.0.0.1"
  port: 8222

secret_key: "sk-plain-123" # 入口 key,重启自动转哈希

providers: []

normalize:
  enabled: true
  drift_detector: false

logging:
  level: info
  request_body: false
"#;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        let path = file.path().to_str().unwrap().to_string();

        let config = Config::load(&path).unwrap();
        let hashed = config.secret_key.unwrap();
        assert!(looks_like_bcrypt(&hashed), "应转为 bcrypt 哈希");
        assert!(bcrypt::verify("sk-plain-123", &hashed).unwrap());

        // 回写后配置文件应为哈希,二次加载不再重复哈希;行尾注释保留
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("$2a$") || content.contains("$2b$") || content.contains("$2y$"));
        assert!(!content.contains("sk-plain-123"));
        assert!(content.contains("# 入口 key,重启自动转哈希"));
    }

    #[test]
    fn test_persist_secret_unicode_whitespace_indent() {
        // 全角空格缩进(非 ASCII 空白):字符索引切片不应 panic,且正确重写
        let yaml = "　secret_key: \"sk-u\"\n";
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        let path = file.path().to_str().unwrap().to_string();
        persist_secret(&path, yaml, "$2b$12$abc").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("secret_key: \"$2b$12$abc\""));
        assert!(!content.contains("sk-u"));
    }

    #[test]
    fn test_secret_key_already_bcrypt_not_rehashed() {
        let hash = bcrypt::hash("sk-already", bcrypt::DEFAULT_COST).unwrap();
        let yaml = format!(
            r#"
server:
  host: "127.0.0.1"
  port: 8222

secret_key: "{hash}"

providers: []

normalize:
  enabled: true
  drift_detector: false

logging:
  level: info
  request_body: false
"#
        );
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();
        let config = Config::load(file.path().to_str().unwrap()).unwrap();
        assert_eq!(config.secret_key.unwrap(), hash);
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
