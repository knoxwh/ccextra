// Cursor 订阅凭证:PKCE 登录、token 刷新与本地存储。
// 代理路径已移除,仅保留登录与凭证保鲜能力。
pub mod constants;
pub mod credential;
pub mod login;
pub mod oauth;
pub mod refresh;
pub mod store;

pub use credential::CursorCredential;
pub use login::{run_login, CursorLoginOptions};
pub use refresh::ensure_credential_fresh;
pub use store::{load, resolve_auth_dir, save};
