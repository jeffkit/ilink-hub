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
    Registered { ok: bool, error: Option<String> },
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
}
