//! Long-running Hub process helpers (shared by CLI and future desktop shell).
//!
//! Callers must install a [`tracing`] subscriber before invoking [`serve::run_serve`];
//! this crate does not initialize logging.

pub mod serve;
