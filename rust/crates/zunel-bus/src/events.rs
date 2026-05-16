use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageKind {
    User,
    Stream,
    Final,
    Approval,
    ApprovalResponse,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub channel: String,
    pub chat_id: String,
    pub user_id: Option<String>,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<String>,
    pub kind: MessageKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub channel: String,
    pub chat_id: String,
    pub message_id: Option<String>,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<String>,
    pub kind: MessageKind,
}
