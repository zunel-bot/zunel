use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use zunel_bus::{MessageBus, MessageKind, OutboundMessage};
use zunel_channels::{Channel, ChannelManager, ChannelStatus};

#[derive(Default)]
struct RecordingChannel {
    started: Mutex<bool>,
    stopped: Mutex<bool>,
    delivered: Mutex<Vec<OutboundMessage>>,
}

#[async_trait]
impl Channel for RecordingChannel {
    fn name(&self) -> &'static str {
        "recording"
    }

    async fn start(&self, _bus: Arc<MessageBus>) -> zunel_channels::Result<()> {
        *self.started.lock().await = true;
        Ok(())
    }

    async fn stop(&self) -> zunel_channels::Result<()> {
        *self.stopped.lock().await = true;
        Ok(())
    }

    async fn send(&self, message: OutboundMessage) -> zunel_channels::Result<()> {
        self.delivered.lock().await.push(message);
        Ok(())
    }

    async fn status(&self) -> ChannelStatus {
        ChannelStatus {
            name: "recording".into(),
            enabled: true,
            connected: *self.started.lock().await && !*self.stopped.lock().await,
            detail: Some("test channel".into()),
        }
    }
}

#[tokio::test]
async fn manager_starts_stops_and_reports_channel_status() {
    let bus = Arc::new(MessageBus::new(8));
    let channel = Arc::new(RecordingChannel::default());
    let manager = ChannelManager::new(bus);
    manager.register(channel.clone());

    manager.start_all().await.unwrap();
    let statuses = manager.statuses().await;
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].name, "recording");
    assert!(statuses[0].connected);

    manager.stop_all().await.unwrap();
    assert!(*channel.stopped.lock().await);
}

#[tokio::test]
async fn manager_dispatches_outbound_messages_to_matching_channel() {
    let bus = Arc::new(MessageBus::new(8));
    let channel = Arc::new(RecordingChannel::default());
    let manager = ChannelManager::new(bus);
    manager.register(channel.clone());

    manager
        .dispatch(OutboundMessage {
            channel: "recording".into(),
            chat_id: "chat-1".into(),
            message_id: None,
            content: "hello from agent".into(),
            media: Vec::new(),
            kind: MessageKind::Final,
        })
        .await
        .unwrap();

    let delivered = channel.delivered.lock().await;
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].content, "hello from agent");
}

#[tokio::test]
async fn manager_dispatches_next_outbound_message_from_bus() {
    let bus = Arc::new(MessageBus::new(8));
    let channel = Arc::new(RecordingChannel::default());
    let manager = ChannelManager::new(bus.clone());
    manager.register(channel.clone());

    bus.publish_outbound(OutboundMessage {
        channel: "recording".into(),
        chat_id: "chat-1".into(),
        message_id: None,
        content: "from bus".into(),
        media: Vec::new(),
        kind: MessageKind::Final,
    })
    .await
    .unwrap();

    manager.dispatch_next_outbound().await.unwrap();

    let delivered = channel.delivered.lock().await;
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].content, "from bus");
}
