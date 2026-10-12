// Claude 订阅 (Anthropic OAuth) 登录与凭证读写

pub mod constants;
pub mod credential;
pub mod login;
pub mod models;
pub mod oauth;
pub mod provider;
pub mod refresh;
pub mod store;

pub use credential::{credential_file_name, is_claude_oauth_token, ClaudeCredential};
pub use login::{run_login, ClaudeLoginOptions};
pub use models::default_claude_models;
pub use provider::load_claude_providers;
pub use refresh::{ensure_credential_fresh, refresh_if_needed};
pub use store::{default_auth_dir, list, load, resolve_auth_dir, save};
