//! CLI bridge: connect to iLink Hub as a virtual-token backend and run a local command per text message.
//! Supports **single-profile YAML** (flat `command` / `args`) or **multi-profile YAML**
//! (`profiles` + `routing`: `fixed` or `prefix`).
//!
//! Used by the `ilink-hub-bridge` binary; see `docs/bridge/README.md`.

pub mod builtin;
mod config;
mod connection;
mod dispatcher;
mod executor;
pub mod manager;
mod paths;
mod probe;
pub mod protocol;
pub mod vtoken_env;

pub use config::{BridgeApp, BridgeConfig, BridgeProfile, RoutingStrategy};
pub use connection::{
    default_auto_client_name, default_local_credential_path, hub_response_token_rejected,
    resolve_hub_connection, validate_hub_token,
};
pub use dispatcher::{run_bridge, run_bridge_with_shutdown, BridgeStop};
pub use executor::MAX_CLI_CAPTURE_BYTES;
pub use paths::resolve_bridge_executable;
pub use probe::{
    check_command_exists, dry_run_profile, find_in_path_robust, probe_profile_light, ProbeError,
};
pub use protocol::PROTOCOL_VERSION;

/// Keywords in CLI stderr that indicate an auth/credential problem.
/// When any of these appear in the error output, the bridge treats the failure as fatal.
pub const AUTH_ERROR_KEYWORDS: &[&str] = &[
    "login",
    "logout",
    "auth",
    "credential",
    "sign in",
    "unauthorized",
    "unauthenticated",
    "401",
    "not logged in",
    "keychain",
    "api key",
    "token",
];
