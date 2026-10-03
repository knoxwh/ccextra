// Cursor 原生 connect-rpc 代理路径:PKCE 登录、token 刷新、模型目录与
// 双向流会话(h2 直连 api2.cursor.sh,不经通用 upstream)。
pub mod catalog;
pub mod constants;
pub mod credential;
pub mod drive;
pub mod error;
pub mod handler;
mod journal;
pub mod login;
pub mod models;
pub mod oauth;
pub mod provider;
pub mod refresh;
mod response;
pub mod session;
pub mod store;
pub mod stream;

pub use catalog::{
    build_cursor_models, cursor_enabled, load_cursor_provider, new_cursor_runtime, CursorConfig,
    CursorRuntime,
};
pub use credential::CursorCredential;
pub use login::{run_login, CursorLoginOptions};
pub use provider::credential_fingerprint;
pub use refresh::ensure_credential_fresh;
pub use store::{load, resolve_auth_dir, save};

#[cfg(test)]
mod handler_tests;
