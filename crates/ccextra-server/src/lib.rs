// ccextra-server: HTTP 入口 + 上游客户端 + SSE 转换

pub mod antigravity;
pub mod http;
pub mod oauth;
pub mod sse;
pub mod upstream;
pub mod xai;

pub use http::serve;
