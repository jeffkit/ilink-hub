//! JSON message protocol between Hub and the public pairing relay.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayMessage {
    Register {
        device_id: String,
        public_key: String,
        timestamp: i64,
        signature: String,
    },
    Registered {
        ok: bool,
        error: Option<String>,
    },
    Request {
        id: String,
        method: String,
        path: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
    },
    Response {
        id: String,
        status: u16,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
    },
    Ping,
    Pong,
}

impl RelayMessage {
    pub fn to_json(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    pub fn from_json(s: &str) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(s)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_register() {
        let msg = RelayMessage::Register {
            device_id: "abc-123".into(),
            public_key: "cGk=".into(),
            timestamp: 1,
            signature: "c2c=".into(),
        };
        let json = msg.to_json().unwrap();
        let back = RelayMessage::from_json(&json).unwrap();
        assert!(matches!(back, RelayMessage::Register { .. }));
    }

    #[test]
    fn roundtrip_registered_ok() {
        let msg = RelayMessage::Registered {
            ok: true,
            error: None,
        };
        let back = RelayMessage::from_json(&msg.to_json().unwrap()).unwrap();
        assert!(matches!(back, RelayMessage::Registered { ok: true, .. }));
    }

    #[test]
    fn roundtrip_registered_error() {
        let msg = RelayMessage::Registered {
            ok: false,
            error: Some("invalid signature".into()),
        };
        let json = msg.to_json().unwrap();
        let back = RelayMessage::from_json(&json).unwrap();
        match back {
            RelayMessage::Registered { ok, error } => {
                assert!(!ok);
                assert_eq!(error.as_deref(), Some("invalid signature"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_request_with_body_and_headers() {
        let mut headers = HashMap::new();
        headers.insert("x-custom".to_string(), "value".to_string());
        let msg = RelayMessage::Request {
            id: "req-1".into(),
            method: "POST".into(),
            path: "/hub/pair".into(),
            headers,
            body: Some("payload".into()),
        };
        let back = RelayMessage::from_json(&msg.to_json().unwrap()).unwrap();
        match back {
            RelayMessage::Request {
                id, method, body, ..
            } => {
                assert_eq!(id, "req-1");
                assert_eq!(method, "POST");
                assert_eq!(body.as_deref(), Some("payload"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_response_with_status_and_body() {
        let msg = RelayMessage::Response {
            id: "req-1".into(),
            status: 200,
            headers: HashMap::new(),
            body: Some("ok".into()),
        };
        let back = RelayMessage::from_json(&msg.to_json().unwrap()).unwrap();
        match back {
            RelayMessage::Response {
                id, status, body, ..
            } => {
                assert_eq!(id, "req-1");
                assert_eq!(status, 200);
                assert_eq!(body.as_deref(), Some("ok"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_ping_and_pong() {
        let ping_back = RelayMessage::from_json(&RelayMessage::Ping.to_json().unwrap()).unwrap();
        assert!(matches!(ping_back, RelayMessage::Ping));
        let pong_back = RelayMessage::from_json(&RelayMessage::Pong.to_json().unwrap()).unwrap();
        assert!(matches!(pong_back, RelayMessage::Pong));
    }

    #[test]
    fn from_json_invalid_input_returns_error() {
        let result = RelayMessage::from_json("{not valid json}");
        assert!(result.is_err(), "invalid JSON must return Err");
    }
}
