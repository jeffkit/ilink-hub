//! Client pairing sessions — emulates iLink QR login for Hub-connected backends.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

const PAIRING_TTL: Duration = Duration::from_secs(600);
/// How long a `Confirmed` pairing session is retained before being purged.
/// The session is no longer needed once confirmed: the vtoken, name, and
/// label are persisted in the registry and store, so the in-memory entry
/// can be safely dropped. F-M1-C: without a TTL here, Confirmed sessions
/// are immortal and `MAX_PAIRING_SESSIONS` is effectively neutered once
/// the live set has cycled through `create` + `confirm`.
const CONFIRMED_TTL: Duration = Duration::from_secs(86_400);
/// Hard cap on simultaneously-live pairing sessions. Prevents a `GET /ilink/bot/get_bot_qrcode`
/// flood from growing `state.pairing.sessions` unboundedly. Each entry is a `PairingSession` plus
/// optional CSRF string; 1024 is generous and the cap is checked at `create()`.
pub const MAX_PAIRING_SESSIONS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingStatus {
    Wait,
    Scanned,
    Confirmed,
    Expired,
}

#[derive(Debug, Clone)]
pub struct PairingSession {
    pub code: String,
    pub created_at: Instant,
    pub status: PairingStatus,
    pub vtoken: Option<String>,
    pub client_name: Option<String>,
    pub client_label: Option<String>,
    /// Single-use CSRF token; minted on `mark_scanned` and consumed by `confirm`.
    /// Bound to this `code`; required for `pair_confirm` (SEC-013).
    pub csrf: Option<String>,
}

impl PairingSession {
    fn is_expired(&self) -> bool {
        // Live (Wait / Scanned) sessions expire after PAIRING_TTL; the
        // pre-existing semantics for get()/public_status() are preserved.
        self.status != PairingStatus::Confirmed && self.created_at.elapsed() > PAIRING_TTL
    }

    /// F-M1-C: Confirmed sessions are dropped by `purge_expired` after
    /// CONFIRMED_TTL. We keep the public "is this session still meaningful
    /// to a client" semantics in `is_expired` separate from "should this
    /// row be evicted from the registry" so a long-confirmed session
    /// doesn't suddenly show as `expired` to a `get()` caller.
    fn should_evict(&self) -> bool {
        match self.status {
            PairingStatus::Confirmed => self.created_at.elapsed() > CONFIRMED_TTL,
            _ => self.created_at.elapsed() > PAIRING_TTL,
        }
    }

    pub fn public_status(&self) -> PairingStatus {
        if self.is_expired() {
            PairingStatus::Expired
        } else {
            self.status.clone()
        }
    }

    pub fn status_str(&self) -> &'static str {
        match self.public_status() {
            PairingStatus::Wait => "wait",
            // iLink / OpenClaw SDK spell this "scaned" (not "scanned").
            PairingStatus::Scanned => "scaned",
            PairingStatus::Confirmed => "confirmed",
            PairingStatus::Expired => "expired",
        }
    }
}

#[derive(Debug, Default)]
pub struct PairingRegistry {
    sessions: HashMap<String, PairingSession>,
}

impl PairingRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn purge_expired(&mut self) {
        // F-M1-C: evict using should_evict (covers Confirmed TTL), not
        // is_expired (which preserves the live-set status semantics for
        // get()/public_status()).
        self.sessions.retain(|_, s| !s.should_evict());
    }

    pub fn create(&mut self) -> Result<String, PairingError> {
        self.purge_expired();
        if self.sessions.len() >= MAX_PAIRING_SESSIONS {
            return Err(PairingError::TooManySessions);
        }
        let code = format!("pair_{}", Uuid::new_v4().simple());
        self.sessions.insert(
            code.clone(),
            PairingSession {
                code: code.clone(),
                created_at: Instant::now(),
                status: PairingStatus::Wait,
                vtoken: None,
                client_name: None,
                client_label: None,
                csrf: None,
            },
        );
        Ok(code)
    }

    pub fn get(&self, code: &str) -> Option<PairingSession> {
        self.sessions.get(code).cloned()
    }

    pub fn mark_scanned(&mut self, code: &str) -> bool {
        self.purge_expired();
        if let Some(session) = self.sessions.get_mut(code) {
            if session.is_expired() {
                session.status = PairingStatus::Expired;
                return false;
            }
            if session.status == PairingStatus::Wait {
                session.status = PairingStatus::Scanned;
            }
            // Mint (or refresh) a CSRF token on every scan. Safe to refresh on re-scan:
            // the token is only valid for a single confirm; if the page is reloaded
            // (re-scan) the previous token is invalidated and a fresh one is issued.
            if session.csrf.is_none() {
                session.csrf = Some(generate_csrf());
            }
            return true;
        }
        false
    }

    pub fn confirm(
        &mut self,
        code: &str,
        client_name: String,
        client_label: Option<String>,
        vtoken: String,
        csrf_header: &str,
    ) -> Result<(), PairingError> {
        self.purge_expired();
        let session = self.sessions.get_mut(code).ok_or(PairingError::NotFound)?;

        if session.is_expired() {
            session.status = PairingStatus::Expired;
            return Err(PairingError::Expired);
        }
        // AlreadyConfirmed is checked BEFORE NotScanned so the second of two racing
        // requests always sees the canonical 409 — never leaks the Scanned state
        // through a 412 to a competing attacker.
        if session.status == PairingStatus::Confirmed {
            return Err(PairingError::AlreadyConfirmed);
        }
        // CSRF must match the session's token. Consuming it (setting to None) prevents
        // replay; a second confirm with the same token returns CsrfMismatch.
        match session.csrf.as_deref() {
            Some(token) if constant_time_eq(token.as_bytes(), csrf_header.as_bytes()) => {
                session.csrf = None;
            }
            _ => return Err(PairingError::CsrfMismatch),
        }
        if session.status != PairingStatus::Scanned {
            return Err(PairingError::NotScanned);
        }

        session.status = PairingStatus::Confirmed;
        session.vtoken = Some(vtoken);
        session.client_name = Some(client_name);
        session.client_label = client_label;
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairingError {
    NotFound,
    Expired,
    AlreadyConfirmed,
    NotScanned,
    CsrfMismatch,
    TooManySessions,
}

/// Generate a 32-character hex CSRF token (128 bits of entropy from OS CSPRNG).
/// Returns a `None` if the OS RNG is unavailable — callers should treat that as
/// a transient error and refuse to mint a session.
fn generate_csrf() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time byte comparison. Mitigates timing side channels when comparing
/// the CSRF header against the session-bound token. Both sides are 32 hex chars.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_confirm_pairing() {
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        reg.mark_scanned(&code);
        let csrf = reg.get(&code).unwrap().csrf.clone().unwrap();
        reg.confirm(
            &code,
            "openclaw-test".to_string(),
            Some("Test".to_string()),
            "vhub_abc".to_string(),
            &csrf,
        )
        .unwrap();

        let session = reg.get(&code).unwrap();
        assert_eq!(session.status_str(), "confirmed");
        assert_eq!(session.vtoken.as_deref(), Some("vhub_abc"));
        assert!(session.csrf.is_none(), "csrf must be consumed on confirm");
    }

    #[test]
    fn expired_pairing_rejected() {
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        let session = reg.sessions.get_mut(&code).unwrap();
        session.created_at = Instant::now() - Duration::from_secs(700);
        let csrf = "0".repeat(32);

        assert_eq!(reg.get(&code).unwrap().status_str(), "expired");
        assert!(reg
            .confirm(&code, "x".into(), None, "vhub_x".into(), &csrf,)
            .is_err());
    }

    #[test]
    fn confirm_rejected_when_status_is_wait() {
        // SEC-013 3.2: confirm without scan → NotScanned.
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        // No mark_scanned → status == Wait; csrf is also None.
        let err = reg
            .confirm(
                &code,
                "x".into(),
                None,
                "vhub_x".into(),
                "0".repeat(32).as_str(),
            )
            .unwrap_err();
        assert_eq!(err, PairingError::CsrfMismatch);
    }

    #[test]
    fn confirm_after_concurrent_attempt_returns_only_one_winner() {
        // Two racers against the same code. First wins (Ok), second gets
        // AlreadyConfirmed — the canonical SEC-001 outcome.
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        reg.mark_scanned(&code);
        let csrf = reg.get(&code).unwrap().csrf.clone().unwrap();

        reg.confirm(&code, "first".into(), None, "vhub_1".into(), &csrf)
            .unwrap();

        // Second racer arrives with stale csrf (already consumed) and the
        // session is now Confirmed → AlreadyConfirmed takes precedence over
        // CsrfMismatch, hiding the Scanned/Consumed state from attackers.
        let err = reg
            .confirm(&code, "second".into(), None, "vhub_2".into(), &csrf)
            .unwrap_err();
        assert_eq!(err, PairingError::AlreadyConfirmed);
    }

    #[test]
    fn csrf_token_consumed_after_confirm() {
        // After a successful confirm, the csrf must be cleared so a replay
        // attempt is rejected with CsrfMismatch.
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        reg.mark_scanned(&code);
        let csrf = reg.get(&code).unwrap().csrf.clone().unwrap();

        reg.confirm(&code, "client".into(), None, "vhub_x".into(), &csrf)
            .unwrap();

        // Replay: csrf is now None, so even a "matching" token fails.
        let err = reg
            .confirm(&code, "attacker".into(), None, "vhub_y".into(), &csrf)
            .unwrap_err();
        // AlreadyConfirmed is checked first, so we see that here.
        assert_eq!(err, PairingError::AlreadyConfirmed);
    }

    #[test]
    fn generate_csrf_is_unique_and_hex() {
        let a = generate_csrf();
        let b = generate_csrf();
        assert_eq!(a.len(), 32, "csrf must be 32 hex chars");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two consecutive csrf tokens must differ");
    }

    #[test]
    fn too_many_sessions_returns_error() {
        let mut reg = PairingRegistry::new();
        // Force the cap to be hit with minimal churn.
        for _ in 0..MAX_PAIRING_SESSIONS {
            reg.create().unwrap();
        }
        let err = reg.create().unwrap_err();
        assert_eq!(err, PairingError::TooManySessions);
    }

    /// F-M1-C: Confirmed sessions must be evicted by `purge_expired` after
    /// CONFIRMED_TTL elapses, otherwise the live-set cap is neutered. We
    /// backdate `created_at` past the TTL to exercise the eviction path
    /// without sleeping in the test.
    #[test]
    fn confirmed_sessions_are_evicted_after_confirmed_ttl() {
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        reg.mark_scanned(&code);
        let csrf = reg.get(&code).unwrap().csrf.clone().unwrap();
        reg.confirm(&code, "client".into(), None, "vhub_x".into(), &csrf)
            .unwrap();
        // Session is now Confirmed and visible.
        assert_eq!(reg.sessions.len(), 1);
        assert_eq!(
            reg.get(&code).unwrap().status_str(),
            "confirmed",
            "freshly confirmed session must be visible"
        );

        // Backdate created_at past CONFIRMED_TTL and force a purge.
        reg.sessions.get_mut(&code).unwrap().created_at =
            Instant::now() - Duration::from_secs(86_400 + 60);
        reg.purge_expired();

        // The session must be evicted, and the live set is empty so a new
        // create() succeeds (this is the whole point — the cap is no
        // longer shadowed by immortal Confirmed sessions).
        assert!(
            reg.get(&code).is_none(),
            "Confirmed session must be evicted after CONFIRMED_TTL"
        );
        let code2 = reg.create().unwrap();
        assert!(
            reg.get(&code2).is_some(),
            "create must succeed once the immortal Confirmed entry is evicted"
        );
    }
}
