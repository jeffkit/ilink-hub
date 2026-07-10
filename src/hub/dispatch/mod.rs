//! Inbound message dispatching: the broadcast→backend pipeline, quote-reply
//! resolution, `@mention` routing, and the per-conversation `HubExt` helpers.
//!
//! Module layout (split from a former monolithic `dispatch.rs`):
//!
//! * [`pipeline`] — spawn + per-message routing
//! * [`quote`] — quote-reply resolution fallbacks
//! * [`mention`] — `@backend` temporary routing
//! * [`queue`] — push helpers
//! * [`hub_ext`] — virtual context / HubExt builders

mod hub_ext;
mod mention;
mod pipeline;
mod queue;
mod quote;

#[cfg(test)]
mod tests;

pub use hub_ext::{build_hub_ext_for_vctx, resolve_vctx_for_message};
pub use pipeline::spawn_dispatcher;
pub use queue::push_to_queue_pub;
