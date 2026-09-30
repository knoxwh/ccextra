// Cursor 订阅凭证:PKCE 登录、token 刷新与本地存储。
// 代理路径已移除,仅保留登录与凭证保鲜能力。
pub mod catalog;
pub mod client;
pub mod constants;
pub mod credential;
pub mod login;
pub mod oauth;
pub mod refresh;
pub mod relay;
pub mod sidecar;
pub mod store;

pub use catalog::{
    build_cursor_models, cursor_enabled, load_cursor_provider, synthesize_cursor_provider,
    CursorConfig, CursorRuntime,
};
pub use client::{CursorSidecarError, SidecarClient};
pub use credential::CursorCredential;
pub use login::{run_login, CursorLoginOptions};
pub use refresh::ensure_credential_fresh;
pub use relay::{
    collect_cursor_sdk_response, relay_cursor_sdk_to_anthropic, CursorRelayMeta, CursorSdkStream,
};
pub use sidecar::{resolve_sidecar_dir, CursorSidecar, CursorSidecarConfig};
pub use store::{load, resolve_auth_dir, save};
