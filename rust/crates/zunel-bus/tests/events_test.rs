use zunel_bus::{InboundMessage, MessageKind, OutboundMessage};

#[test]
fn round_trips_inbound_through_json() {
    let msg = InboundMessage {
        channel: "cli".into(),
        chat_id: "direct".into(),
        user_id: Some("me".into()),
        content: "hi".into(),
        media: vec!["file.txt".into()],
        kind: MessageKind::User,
    };
    let raw = serde_json::to_string(&msg).unwrap();
    let back: InboundMessage = serde_json::from_str(&raw).unwrap();
    assert_eq!(back.content, "hi");
    assert_eq!(back.media, vec!["file.txt"]);
    assert!(matches!(back.kind, MessageKind::User));
}

#[test]
fn outbound_stream_kind_serializes() {
    let msg = OutboundMessage {
        channel: "slack".into(),
        chat_id: "C123".into(),
        message_id: Some("ts-1".into()),
        content: "hello".into(),
        media: vec!["artifact.png".into()],
        kind: MessageKind::Stream,
    };
    let raw = serde_json::to_string(&msg).unwrap();
    assert!(raw.contains("\"kind\":\"stream\""), "got {raw}");
    assert!(raw.contains("artifact.png"), "got {raw}");
}
