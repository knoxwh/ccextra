// ccextra-core: 纯逻辑,无 IO
//
// 职责:
// - normalize/: 九个 tklite 归一化模块
// - convert/: 三条 body-to-body 转换
// - route/: model → provider 路由决策
// - session/: 会话身份派生

pub mod cache_stabilization;
pub mod convert;
pub mod normalize;
pub mod prompt_cache;
pub mod route;
pub mod secret;
pub mod session;
pub mod thinking;

pub use route::{Protocol, RouteDecision};
