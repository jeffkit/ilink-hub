//! MCP (Model Context Protocol) server — Streamable HTTP transport.
//!
//! Exposes two tools to Agent backends:
//!   - `list_agents`  — list all registered Agent backends (name + online status)
//!   - `call_agent`   — forward a message to another Agent and wait for its reply
//!
//! Authentication reuses the existing vtoken Bearer scheme.  The calling Agent
//! supplies its own vtoken so the Hub can identify who is calling and which
//! WeChat conversation context (vctx) to attach the exchange to.

pub mod protocol;
pub mod router;
pub mod tools;
pub mod waiter;

pub use router::mcp_router;
pub use waiter::A2aWaiter;
