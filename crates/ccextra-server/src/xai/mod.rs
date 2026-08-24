// xAI OAuth 登录与凭证读写

pub mod constants;
pub mod credential;
pub mod login;
pub mod models;
pub mod oauth;
pub mod provider;
pub mod refresh;
pub mod store;

pub use credential::{credential_file_name, parse_jwt_identity, XAICredential};
pub use login::{run_login, XAILoginOptions};
pub use models::default_grok_models;
pub use provider::load_xai_providers;
pub use refresh::{ensure_credential_fresh, refresh_if_needed};
pub use store::{default_auth_dir, list, load, resolve_auth_dir, save};
