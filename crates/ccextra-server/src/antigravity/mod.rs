// Antigravity OAuth 登录与凭证读写

pub mod constants;
pub mod credential;
pub mod login;
pub mod models;
pub mod oauth;
pub mod project;
pub mod provider;
pub mod refresh;
pub mod store;

pub use credential::{credential_file_name, AntigravityCredential};
pub use login::{ensure_fresh, run_login, LoginOptions};
pub use provider::load_antigravity_providers;
pub use store::{default_auth_dir, list, load, resolve_auth_dir, save};
