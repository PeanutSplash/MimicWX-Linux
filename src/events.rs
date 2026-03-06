use serde::Serialize;

use crate::db::DbMessage;
use crate::runtime::RuntimeSnapshot;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum WxEvent {
    #[serde(rename = "db_message")]
    Message(DbMessage),
    #[serde(rename = "sent")]
    Sent {
        to: String,
        text: String,
        verified: bool,
    },
    #[serde(rename = "status_change")]
    StatusChange {
        from: RuntimeSnapshot,
        to: RuntimeSnapshot,
    },
    #[serde(rename = "control")]
    Control { cmd: String },
}

impl WxEvent {
    pub fn notification_method(&self) -> &'static str {
        match self {
            Self::Message(_) => "message",
            Self::Sent { .. } => "sent",
            Self::StatusChange { .. } => "status_change",
            Self::Control { .. } => "control",
        }
    }

    pub fn to_jsonrpc_notification(&self) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": self.notification_method(),
            "params": self,
        })
    }
}
