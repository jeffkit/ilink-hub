//! Client pairing sessions — emulates iLink QR login for Hub-connected backends.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use uuid::Uuid;

const PAIRING_TTL: Duration = Duration::from_secs(600);

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
}

impl PairingSession {
    fn is_expired(&self) -> bool {
        self.created_at.elapsed() > PAIRING_TTL && self.status != PairingStatus::Confirmed
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
        self.sessions
            .retain(|_, s| !s.is_expired() || s.status == PairingStatus::Confirmed);
    }

    pub fn create(&mut self) -> String {
        self.purge_expired();
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
            },
        );
        code
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
    ) -> Result<(), PairingError> {
        self.purge_expired();
        let session = self.sessions.get_mut(code).ok_or(PairingError::NotFound)?;

        if session.is_expired() {
            session.status = PairingStatus::Expired;
            return Err(PairingError::Expired);
        }
        if session.status == PairingStatus::Confirmed {
            return Err(PairingError::AlreadyConfirmed);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_confirm_pairing() {
        let mut reg = PairingRegistry::new();
        let code = reg.create();
        reg.mark_scanned(&code);
        reg.confirm(
            &code,
            "openclaw-test".to_string(),
            Some("Test".to_string()),
            "vhub_abc".to_string(),
        )
        .unwrap();

        let session = reg.get(&code).unwrap();
        assert_eq!(session.status_str(), "confirmed");
        assert_eq!(session.vtoken.as_deref(), Some("vhub_abc"));
    }

    #[test]
    fn expired_pairing_rejected() {
        let mut reg = PairingRegistry::new();
        let code = reg.create();
        let session = reg.sessions.get_mut(&code).unwrap();
        session.created_at = Instant::now() - Duration::from_secs(700);

        assert_eq!(reg.get(&code).unwrap().status_str(), "expired");
        assert!(reg
            .confirm(&code, "x".into(), None, "vhub_x".into())
            .is_err());
    }

    #[test]
    fn double_confirm_returns_already_confirmed() {
        let mut reg = PairingRegistry::new();
        let code = reg.create();
        reg.mark_scanned(&code);

        let first = reg.confirm(
            &code,
            "openclaw-test".to_string(),
            Some("Test".to_string()),
            "vhub_abc".to_string(),
        );
        assert!(first.is_ok(), "first confirm should succeed");
        assert_eq!(reg.get(&code).unwrap().status_str(), "confirmed");

        let second = reg.confirm(
            &code,
            "openclaw-test".to_string(),
            Some("Test".to_string()),
            "vhub_abc".to_string(),
        );
        assert_eq!(second, Err(PairingError::AlreadyConfirmed));

        // Session remains Confirmed and original vtoken is preserved.
        let session = reg.get(&code).unwrap();
        assert_eq!(session.status_str(), "confirmed");
        assert_eq!(session.vtoken.as_deref(), Some("vhub_abc"));
    }
}
