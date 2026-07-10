//! Client pairing sessions — emulates iLink QR login for Hub-connected backends.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

const PAIRING_TTL: Duration = Duration::from_secs(600);
/// How long a session that has been scanned (phone confirmed QR) but not yet
/// confirmed (operator pressed "allow") remains valid. A short window limits the
/// replay risk: an attacker who captures a scan request has at most 60 seconds to
/// race the legitimate confirm (SEC-002).
const SCANNED_TTL: Duration = Duration::from_secs(60);
/// How long a `Confirmed` pairing session is retained before being purged.
/// The session is no longer needed once confirmed: the vtoken, name, and
/// label are persisted in the registry and store, so the in-memory entry
/// can be safely dropped. F-M1-C: without a TTL here, Confirmed sessions
/// are immortal and `MAX_PAIRING_SESSIONS` is effectively neutered once
/// the live set has cycled through `create` + `confirm`.
const CONFIRMED_TTL: Duration = Duration::from_secs(86_400);
/// How long after confirm a polling client may re-read the same `vtoken`.
///
/// Claim-window (not single-take): the first `get_qrcode_status` response can be
/// lost (network reset, proxy idle timeout, client crash mid-body). Keeping the
/// token reclaimable for this short window lets the legitimate CLI retry without
/// wedging behind an orphan registry row + NameCollision. After the window,
/// the next claim permanently clears the token so a leaked pair code cannot be
/// re-polled for the remaining `CONFIRMED_TTL` (24h) steal hole.
const VTOKEN_CLAIM_WINDOW: Duration = Duration::from_secs(120);
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
    /// When the QR code was first scanned (phone confirmed). Used to enforce the short
    /// `SCANNED_TTL` replay window (SEC-002): confirmation must happen within 60s of scan.
    pub scanned_at: Option<Instant>,
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
        match self.status {
            PairingStatus::Confirmed => false,
            // Once scanned, the confirmation window shrinks to SCANNED_TTL (60s) to reduce
            // the replay attack window (SEC-002). Fall back to PAIRING_TTL if scanned_at is
            // unexpectedly absent.
            PairingStatus::Scanned => self
                .scanned_at
                .map(|t| t.elapsed() > SCANNED_TTL)
                .unwrap_or_else(|| self.created_at.elapsed() > PAIRING_TTL),
            _ => self.created_at.elapsed() > PAIRING_TTL,
        }
    }

    /// F-M1-C: Confirmed sessions are dropped by `purge_expired` after
    /// CONFIRMED_TTL. We keep the public "is this session still meaningful
    /// to a client" semantics in `is_expired` separate from "should this
    /// row be evicted from the registry" so a long-confirmed session
    /// doesn't suddenly show as `expired` to a `get()` caller.
    fn should_evict(&self) -> bool {
        match self.status {
            PairingStatus::Confirmed => self.created_at.elapsed() > CONFIRMED_TTL,
            PairingStatus::Scanned => self
                .scanned_at
                .map(|t| t.elapsed() > SCANNED_TTL)
                .unwrap_or_else(|| self.created_at.elapsed() > PAIRING_TTL),
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
    confirmed_sessions: HashMap<String, (PairingSession, Instant)>,
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
        self.confirmed_sessions
            .retain(|_, (_, confirmed_at)| confirmed_at.elapsed() < CONFIRMED_TTL);
    }

    pub fn create(&mut self) -> Result<String, PairingError> {
        self.purge_expired();
        if self.sessions.len() + self.confirmed_sessions.len() >= MAX_PAIRING_SESSIONS {
            return Err(PairingError::TooManySessions);
        }
        let code = format!("pair_{}", Uuid::new_v4().simple());
        self.sessions.insert(
            code.clone(),
            PairingSession {
                code: code.clone(),
                created_at: Instant::now(),
                scanned_at: None,
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
        if let Some(session) = self.sessions.get(code) {
            return Some(session.clone());
        }
        if let Some((session, _)) = self.confirmed_sessions.get(code) {
            return Some(session.clone());
        }
        None
    }

    /// Claim (or re-claim within `VTOKEN_CLAIM_WINDOW`) a confirmed session's vtoken.
    ///
    /// Semantics (claim-window, not single-take):
    /// - Unknown code → `None`
    /// - Wait/scanned in `sessions` → `(session, None)` unchanged (never invents a token)
    /// - Confirmed, `vtoken: Some`, and `confirmed_at.elapsed() < VTOKEN_CLAIM_WINDOW`
    ///   → `(session, Some(token))` **without clearing** so a lost status response
    ///   can be retried by the legitimate client
    /// - Confirmed and `confirmed_at.elapsed() >= VTOKEN_CLAIM_WINDOW`
    ///   → clear `vtoken` and return `(session, None)` (closes the 24h re-poll steal hole)
    /// - Confirmed with `vtoken` already `None` → `(session, None)`
    ///
    /// Callers hold the pairing write lock around this method; concurrent claims
    /// within the window are serialized and each observes the same token.
    pub fn claim_confirmed_vtoken(
        &mut self,
        code: &str,
    ) -> Option<(PairingSession, Option<String>)> {
        self.purge_expired();
        if let Some((session, confirmed_at)) = self.confirmed_sessions.get_mut(code) {
            if session.vtoken.is_none() {
                return Some((session.clone(), None));
            }
            if confirmed_at.elapsed() < VTOKEN_CLAIM_WINDOW {
                let token = session.vtoken.clone();
                return Some((session.clone(), token));
            }
            // Window elapsed: permanently clear so a leaked pair code cannot
            // re-poll for the bearer token during the remaining CONFIRMED_TTL.
            let _ = session.vtoken.take();
            return Some((session.clone(), None));
        }
        self.sessions
            .get(code)
            .map(|session| (session.clone(), None))
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
                // Record scan time only on the first transition Wait→Scanned.
                // is_expired() uses scanned_at to enforce the SCANNED_TTL (60s) window.
                session.scanned_at = Some(Instant::now());
            }
            // Mint a CSRF token the first time a session is scanned. Subsequent
            // re-scans (page reloads, re-renders) are no-ops on the token so a
            // re-rendered page does not invalidate the legitimate phone's open
            // copy — an attacker who could force a rotation would otherwise be
            // able to wedge the legit user out of their own session. The token
            // is consumed by `confirm` (single-use) and bound to this `code`,
            // so cross-session replay is impossible regardless.
            if session.csrf.is_none() {
                session.csrf = Some(generate_csrf());
            }
            return true;
        }
        false
    }

    pub fn pre_check_confirm(&mut self, code: &str, csrf_header: &str) -> Result<(), PairingError> {
        self.purge_expired();

        if self.confirmed_sessions.contains_key(code) {
            return Err(PairingError::AlreadyConfirmed);
        }

        let session = self.sessions.get(code).ok_or(PairingError::NotFound)?;

        if session.is_expired() {
            return Err(PairingError::Expired);
        }
        if session.status == PairingStatus::Confirmed {
            return Err(PairingError::AlreadyConfirmed);
        }
        match session.csrf.as_deref() {
            Some(token) if constant_time_eq(token.as_bytes(), csrf_header.as_bytes()) => {}
            _ => return Err(PairingError::CsrfMismatch),
        }
        if session.status != PairingStatus::Scanned {
            return Err(PairingError::NotScanned);
        }
        Ok(())
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

        if self.confirmed_sessions.contains_key(code) {
            return Err(PairingError::AlreadyConfirmed);
        }

        let mut session = self.sessions.remove(code).ok_or(PairingError::NotFound)?;

        if session.is_expired() {
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

        self.confirmed_sessions
            .insert(code.to_string(), (session, Instant::now()));
        Ok(())
    }

    pub fn remove_confirmed(&mut self, code: &str) {
        self.confirmed_sessions.remove(code);
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
    NameCollision,
}

/// Generate a 32-character hex CSRF token (128 bits of entropy from OS CSPRNG).
/// Returns a `None` if the OS RNG is unavailable — callers should treat that as
/// a transient error and refuse to mint a session.
fn generate_csrf() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
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

        // Claim-window: within VTOKEN_CLAIM_WINDOW the token remains reclaimable.
        let (_snap, first) = reg.claim_confirmed_vtoken(&code).unwrap();
        assert_eq!(first.as_deref(), Some("vhub_abc"));
        let (_snap, second) = reg.claim_confirmed_vtoken(&code).unwrap();
        assert_eq!(second.as_deref(), Some("vhub_abc"));
        assert_eq!(reg.get(&code).unwrap().status_str(), "confirmed");
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
    fn scanned_session_expires_after_scanned_ttl_not_pairing_ttl() {
        // SEC-002: once scanned, only SCANNED_TTL (60s) remains, not PAIRING_TTL (600s).
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        reg.mark_scanned(&code);

        // Backdate scanned_at past SCANNED_TTL but keep created_at recent.
        let session = reg.sessions.get_mut(&code).unwrap();
        session.scanned_at = Some(Instant::now() - Duration::from_secs(SCANNED_TTL.as_secs() + 5));

        // The session should now appear Expired despite created_at being recent.
        assert_eq!(
            reg.get(&code).unwrap().status_str(),
            "expired",
            "scanned session must expire after SCANNED_TTL, not PAIRING_TTL"
        );
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
        // Session moved to confirmed_sessions on confirm; sessions is now empty.
        assert_eq!(reg.sessions.len(), 0);
        assert_eq!(
            reg.get(&code).unwrap().status_str(),
            "confirmed",
            "freshly confirmed session must be visible via confirmed_sessions"
        );

        // Backdate confirmed_at past CONFIRMED_TTL and force a purge.
        reg.confirmed_sessions.get_mut(&code).unwrap().1 =
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

    /// M2-1: pre_check_confirm with a wrong CSRF token must return CsrfMismatch.
    /// Catches the `constant_time_eq(...) → true` mutant (195:28) which would
    /// bypass the CSRF guard and allow any token to confirm a scanned session.
    #[test]
    fn pre_check_confirm_rejects_wrong_csrf() {
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        reg.mark_scanned(&code);
        // correct csrf is stored in the session; pass a different string.
        let err = reg
            .pre_check_confirm(&code, "definitely-wrong-csrf")
            .unwrap_err();
        assert_eq!(
            err,
            PairingError::CsrfMismatch,
            "wrong CSRF must be rejected with CsrfMismatch"
        );
    }

    /// M2-2: pre_check_confirm with the correct CSRF token must return Ok.
    /// Together with M2-1, this creates a passing/failing pair that catches
    /// both directions of the constant_time_eq mutant.
    #[test]
    fn pre_check_confirm_accepts_correct_csrf() {
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        reg.mark_scanned(&code);
        let csrf = reg.get(&code).unwrap().csrf.clone().unwrap();
        reg.pre_check_confirm(&code, &csrf)
            .expect("correct CSRF must be accepted");
    }

    /// M2-3: A freshly Confirmed session must not be considered expired even
    /// after PAIRING_TTL. Catches the `delete match arm PairingStatus::Confirmed`
    /// mutant (52:13) in `is_expired` which would make Confirmed sessions
    /// expire after PAIRING_TTL using the catch-all branch.
    #[test]
    fn confirmed_session_is_not_expired_after_pairing_ttl() {
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        reg.mark_scanned(&code);
        let csrf = reg.get(&code).unwrap().csrf.clone().unwrap();
        reg.confirm(&code, "client".into(), None, "vhub_ok".into(), &csrf)
            .unwrap();

        // Backdate created_at past PAIRING_TTL to exercise the TTL branch.
        // A Confirmed session should still report "confirmed", not "expired".
        reg.confirmed_sessions.get_mut(&code).unwrap().0.created_at =
            Instant::now() - Duration::from_secs(PAIRING_TTL.as_secs() + 5);

        assert_eq!(
            reg.get(&code).unwrap().status_str(),
            "confirmed",
            "Confirmed session must not appear expired after PAIRING_TTL"
        );
    }

    /// M2-4: A Wait-status session past PAIRING_TTL must be evicted by
    /// purge_expired. Catches the `should_evict → false` mutant (70:9) and the
    /// `purge_expired < → <=` mutant (116:67) — if should_evict always returns
    /// false, expired sessions never disappear and the cap becomes ineffective.
    #[test]
    fn purge_expired_removes_stale_wait_sessions() {
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        // Backdate past PAIRING_TTL so should_evict returns true.
        reg.sessions.get_mut(&code).unwrap().created_at =
            Instant::now() - Duration::from_secs(PAIRING_TTL.as_secs() + 5);

        reg.purge_expired();

        assert!(
            reg.get(&code).is_none(),
            "stale Wait session must be removed by purge_expired"
        );
    }

    /// M2-5: A Scanned-status session past SCANNED_TTL must be evicted by
    /// purge_expired. Catches should_evict comparison mutants for Scanned branch.
    #[test]
    fn purge_expired_removes_stale_scanned_sessions() {
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        reg.mark_scanned(&code);
        // Backdate scanned_at well past SCANNED_TTL.
        let session = reg.sessions.get_mut(&code).unwrap();
        session.scanned_at = Some(Instant::now() - Duration::from_secs(SCANNED_TTL.as_secs() + 5));

        reg.purge_expired();

        assert!(
            reg.get(&code).is_none(),
            "stale Scanned session must be removed by purge_expired"
        );
    }

    /// M2-6: confirmed_sessions count toward the session cap.
    /// Catches the `sessions.len() + confirmed.len()` → `sessions.len() - confirmed.len()`
    /// mutant (121:32): if subtraction is used, having confirmed sessions would
    /// *lower* the effective count, allowing more sessions than the cap allows.
    #[test]
    fn session_cap_includes_confirmed_sessions() {
        let mut reg = PairingRegistry::new();
        // Fill up to (MAX - 1) with normal sessions.
        for _ in 0..(MAX_PAIRING_SESSIONS - 1) {
            reg.create().unwrap();
        }
        // Create and confirm one more session (moves to confirmed_sessions).
        let code = reg.create().unwrap();
        reg.mark_scanned(&code);
        let csrf = reg.get(&code).unwrap().csrf.clone().unwrap();
        reg.confirm(&code, "c".into(), None, "vhub_cap".into(), &csrf)
            .unwrap();
        // sessions has MAX-1 entries, confirmed_sessions has 1 → total == MAX.
        // create() must fail regardless of whether we use + or -.
        let err = reg.create().unwrap_err();
        assert_eq!(
            err,
            PairingError::TooManySessions,
            "cap must account for both pending and confirmed sessions"
        );
    }

    /// Within the claim window, sequential claims all receive the same token
    /// and the stored vtoken is not cleared (lost-response recovery).
    #[test]
    fn claim_confirmed_vtoken_reclaimable_within_window() {
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        reg.mark_scanned(&code);
        let csrf = reg.get(&code).unwrap().csrf.clone().unwrap();
        reg.confirm(&code, "client".into(), None, "vhub_once".into(), &csrf)
            .unwrap();

        let (session1, token1) = reg.claim_confirmed_vtoken(&code).unwrap();
        assert_eq!(session1.status_str(), "confirmed");
        assert_eq!(token1.as_deref(), Some("vhub_once"));

        let (session2, token2) = reg.claim_confirmed_vtoken(&code).unwrap();
        assert_eq!(
            session2.status_str(),
            "confirmed",
            "confirmed stub must remain after claim"
        );
        assert_eq!(
            token2.as_deref(),
            Some("vhub_once"),
            "second claim within window must re-issue the same vtoken"
        );
        assert_eq!(
            reg.get(&code).unwrap().vtoken.as_deref(),
            Some("vhub_once"),
            "stored session must keep vtoken during claim window"
        );
    }

    /// After the claim window, the next claim clears the token permanently.
    #[test]
    fn claim_confirmed_vtoken_clears_after_claim_window() {
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        reg.mark_scanned(&code);
        let csrf = reg.get(&code).unwrap().csrf.clone().unwrap();
        reg.confirm(&code, "client".into(), None, "vhub_window".into(), &csrf)
            .unwrap();

        let (_, token_fresh) = reg.claim_confirmed_vtoken(&code).unwrap();
        assert_eq!(token_fresh.as_deref(), Some("vhub_window"));

        reg.confirmed_sessions.get_mut(&code).unwrap().1 =
            Instant::now() - VTOKEN_CLAIM_WINDOW - Duration::from_secs(1);

        let (session, token) = reg.claim_confirmed_vtoken(&code).unwrap();
        assert_eq!(session.status_str(), "confirmed");
        assert!(
            token.is_none(),
            "claim after window must not return the vtoken"
        );
        assert!(
            reg.get(&code).unwrap().vtoken.is_none(),
            "stored session must keep vtoken cleared after window"
        );

        let (_, token_again) = reg.claim_confirmed_vtoken(&code).unwrap();
        assert!(
            token_again.is_none(),
            "subsequent claims must keep seeing None after clear"
        );
    }

    /// Wait/scanned sessions are readable via claim without inventing a token.
    #[test]
    fn claim_confirmed_vtoken_on_wait_returns_none_token() {
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        let (session, token) = reg.claim_confirmed_vtoken(&code).unwrap();
        assert_eq!(session.status_str(), "wait");
        assert!(token.is_none());

        reg.mark_scanned(&code);
        let (session, token) = reg.claim_confirmed_vtoken(&code).unwrap();
        assert_eq!(session.status_str(), "scaned");
        assert!(token.is_none());
    }

    #[test]
    fn claim_confirmed_vtoken_unknown_code_returns_none() {
        let mut reg = PairingRegistry::new();
        assert!(reg.claim_confirmed_vtoken("pair_missing").is_none());
    }
}
