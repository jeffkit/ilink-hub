//! iLink-compatible HTTP routes exposed to backend clients.
//! Clients configure `base_url = https://your-hub.example.com` and
//! use their virtual token — they see the same API as ilinkai.weixin.qq.com.
//!
//! Module layout (split from a former monolithic `routes.rs`):
//!
//! * [`auth`] — vtoken / admin auth helpers
//! * [`bot`] — register + iLink bot API
//! * [`admin`] — Hub admin / iLink status / UI
//! * [`metrics`] — Prometheus text endpoint
//! * [`wait`] — long-poll shutdown-aware wait helpers

mod admin;
mod auth;
mod bot;
mod metrics;
mod wait;

#[cfg(test)]
mod admin_auth_tests;
#[cfg(test)]
mod shutdown_poll_tests;

pub use admin::*;
pub use auth::{extract_vtoken_pub, AdminGuard, UNKNOWN_VTOKEN_MSG};
pub use bot::*;
pub use metrics::metrics;
