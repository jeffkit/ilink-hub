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
        if self.status == PairingStatus::Confirmed {
            return false;
        }
        let ttl = match self.status {
            PairingStatus::Scanned => Duration::from_secs(60),
            _ => PAIRING_TTL,
        };
        self.created_at.elapsed() > ttl
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
        self.sessions.retain(|_, s| !s.is_expired());
        let confirmed_ttl = Duration::from_secs(60);
        self.confirmed_sessions
            .retain(|_, (_, confirmed_at)| confirmed_at.elapsed() < confirmed_ttl);
    }

    pub fn create(&mut self) -> Result<String, PairingError> {
        self.purge_expired();
        if self.sessions.len() >= 10 {
            return Err(PairingError::LimitExceeded);
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

    pub fn mark_scanned(&mut self, code: &str) -> bool {
        self.purge_expired();
        if let Some(session) = self.sessions.get_mut(code) {
            if session.is_expired() {
                session.status = PairingStatus::Expired;
                return false;
            }
            if session.status == PairingStatus::Wait {
                session.status = PairingStatus::Scanned;
                session.created_at = Instant::now(); // Reset TTL/created_at to 60s
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

        if self.confirmed_sessions.contains_key(code) {
            return Err(PairingError::AlreadyConfirmed);
        }

        let mut session = self.sessions.remove(code).ok_or(PairingError::NotFound)?;

        if session.is_expired() {
            return Err(PairingError::Expired);
        }

        session.status = PairingStatus::Confirmed;
        session.vtoken = Some(vtoken);
        session.client_name = Some(client_name);
        session.client_label = client_label;

        self.confirmed_sessions
            .insert(code.to_string(), (session, Instant::now()));
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairingError {
    NotFound,
    Expired,
    AlreadyConfirmed,
    LimitExceeded,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_confirm_pairing() {
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
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
        let code = reg.create().unwrap();
        let session = reg.sessions.get_mut(&code).unwrap();
        session.created_at = Instant::now() - Duration::from_secs(700);

        assert_eq!(reg.get(&code).unwrap().status_str(), "expired");
        assert!(reg
            .confirm(&code, "x".into(), None, "vhub_x".into())
            .is_err());
    }

    #[test]
    fn test_scanned_ttl_reset() {
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();

        // Mark scanned, which should transition status to Scanned and reset created_at
        assert!(reg.mark_scanned(&code));
        let session = reg.get(&code).unwrap();
        assert_eq!(session.status_str(), "scaned");

        // Manually push back created_at by 50 seconds -> still valid
        {
            let s = reg.sessions.get_mut(&code).unwrap();
            s.created_at = Instant::now() - Duration::from_secs(50);
        }
        assert_eq!(reg.get(&code).unwrap().status_str(), "scaned");

        // Manually push back created_at by 65 seconds -> should be expired
        {
            let s = reg.sessions.get_mut(&code).unwrap();
            s.created_at = Instant::now() - Duration::from_secs(65);
        }
        assert_eq!(reg.get(&code).unwrap().status_str(), "expired");
    }

    #[test]
    fn test_concurrent_sessions_limit() {
        let mut reg = PairingRegistry::new();
        // Create 10 sessions successfully
        for _ in 0..10 {
            assert!(reg.create().is_ok());
        }
        // 11th session should fail
        assert_eq!(reg.create().unwrap_err(), PairingError::LimitExceeded);

        // Expire one session and verify we can create a new one
        let code = {
            let code = reg.sessions.keys().next().unwrap().clone();
            let s = reg.sessions.get_mut(&code).unwrap();
            s.created_at = Instant::now() - Duration::from_secs(700);
            code
        };

        // This will purge the expired one, bringing count back to 9, allowing 1 new session
        let new_code = reg.create().unwrap();
        assert_ne!(code, new_code);

        // Confirming a session immediately evicts it from sessions map
        reg.mark_scanned(&new_code);
        reg.confirm(
            &new_code,
            "test-client".to_string(),
            None,
            "vtoken_123".to_string(),
        )
        .unwrap();

        // The sessions map size is now 9, so we can create another one
        assert!(reg.create().is_ok());
    }

    #[test]
    fn test_confirmed_eviction_and_temporary_lookup() {
        let mut reg = PairingRegistry::new();
        let code = reg.create().unwrap();
        reg.mark_scanned(&code);
        reg.confirm(
            &code,
            "test-client".to_string(),
            None,
            "vtoken_123".to_string(),
        )
        .unwrap();

        // Confirmed session is no longer in `sessions`
        assert!(!reg.sessions.contains_key(&code));

        // But it is still retrievable via `get`
        let session = reg.get(&code).unwrap();
        assert_eq!(session.status_str(), "confirmed");

        // After 61 seconds, it should be purged
        {
            let entry = reg.confirmed_sessions.get_mut(&code).unwrap();
            entry.1 = Instant::now() - Duration::from_secs(65);
        }
        reg.purge_expired();
        assert!(reg.get(&code).is_none());
    }
}
