use zunel_bus::{InboundMessage, MessageBus, MessageKind, OutboundMessage};

#[tokio::test]
async fn message_bus_routes_inbound_and_outbound_messages_in_order() {
    let bus = MessageBus::new(8);

    bus.publish_inbound(InboundMessage {
        channel: "slack".into(),
        chat_id: "C123:T456".into(),
        user_id: Some("U123".into()),
        content: "hello".into(),
        media: Vec::new(),
        kind: MessageKind::User,
    })
    .await
    .unwrap();

    bus.publish_outbound(OutboundMessage {
        channel: "slack".into(),
        chat_id: "C123:T456".into(),
        message_id: None,
        content: "hi there".into(),
        media: Vec::new(),
        kind: MessageKind::Final,
    })
    .await
    .unwrap();

    let inbound = bus.next_inbound().await.unwrap();
    assert_eq!(inbound.channel, "slack");
    assert_eq!(inbound.chat_id, "C123:T456");
    assert_eq!(inbound.content, "hello");

    let outbound = bus.next_outbound().await.unwrap();
    assert_eq!(outbound.channel, "slack");
    assert_eq!(outbound.chat_id, "C123:T456");
    assert_eq!(outbound.content, "hi there");
}

#[tokio::test]
async fn message_bus_exposes_cloneable_handles_for_services() {
    let bus = MessageBus::new(8);
    let inbound = bus.inbound_publisher();
    let outbound = bus.outbound_publisher();

    inbound
        .send(InboundMessage {
            channel: "cron".into(),
            chat_id: "job-1".into(),
            user_id: None,
            content: "run".into(),
            media: Vec::new(),
            kind: MessageKind::System,
        })
        .await
        .unwrap();

    outbound
        .send(OutboundMessage {
            channel: "cron".into(),
            chat_id: "job-1".into(),
            message_id: Some("msg-1".into()),
            content: "done".into(),
            media: Vec::new(),
            kind: MessageKind::Final,
        })
        .await
        .unwrap();

    assert_eq!(bus.next_inbound().await.unwrap().channel, "cron");
    assert_eq!(
        bus.next_outbound().await.unwrap().message_id.as_deref(),
        Some("msg-1")
    );
}
