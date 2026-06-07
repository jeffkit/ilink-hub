//! Helpers for AI backends connecting to a local iLink Hub instance.

pub mod pairing;

pub use pairing::{HubPairingCredentials, HubPairingOptions, HubPairingClient};
