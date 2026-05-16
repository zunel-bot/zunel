use std::collections::VecDeque;

use tokio::sync::{mpsc, Mutex};

use crate::{InboundMessage, OutboundMessage};

#[derive(Debug, thiserror::Error)]
pub enum BusError {
    #[error("message bus receiver is closed")]
    Closed,
}

#[derive(Clone)]
pub struct InboundPublisher {
    tx: mpsc::Sender<InboundMessage>,
}

impl InboundPublisher {
    pub async fn send(&self, message: InboundMessage) -> Result<(), BusError> {
        self.tx.send(message).await.map_err(|_| BusError::Closed)
    }
}

#[derive(Clone)]
pub struct OutboundPublisher {
    tx: mpsc::Sender<OutboundMessage>,
}

impl OutboundPublisher {
    pub async fn send(&self, message: OutboundMessage) -> Result<(), BusError> {
        self.tx.send(message).await.map_err(|_| BusError::Closed)
    }
}

pub struct MessageBus {
    inbound_tx: mpsc::Sender<InboundMessage>,
    inbound_rx: Mutex<mpsc::Receiver<InboundMessage>>,
    inbound_pending: Mutex<VecDeque<InboundMessage>>,
    outbound_tx: mpsc::Sender<OutboundMessage>,
    outbound_rx: Mutex<mpsc::Receiver<OutboundMessage>>,
}

impl MessageBus {
    pub fn new(buffer: usize) -> Self {
        let (inbound_tx, inbound_rx) = mpsc::channel(buffer);
        let (outbound_tx, outbound_rx) = mpsc::channel(buffer);
        Self {
            inbound_tx,
            inbound_rx: Mutex::new(inbound_rx),
            inbound_pending: Mutex::new(VecDeque::new()),
            outbound_tx,
            outbound_rx: Mutex::new(outbound_rx),
        }
    }

    pub fn inbound_publisher(&self) -> InboundPublisher {
        InboundPublisher {
            tx: self.inbound_tx.clone(),
        }
    }

    pub fn outbound_publisher(&self) -> OutboundPublisher {
        OutboundPublisher {
            tx: self.outbound_tx.clone(),
        }
    }

    pub async fn publish_inbound(&self, message: InboundMessage) -> Result<(), BusError> {
        self.inbound_publisher().send(message).await
    }

    pub async fn publish_outbound(&self, message: OutboundMessage) -> Result<(), BusError> {
        self.outbound_publisher().send(message).await
    }

    pub async fn next_inbound(&self) -> Option<InboundMessage> {
        if let Some(message) = self.inbound_pending.lock().await.pop_front() {
            return Some(message);
        }
        self.inbound_rx.lock().await.recv().await
    }

    pub async fn next_inbound_matching(
        &self,
        matches: impl Fn(&InboundMessage) -> bool,
    ) -> Option<InboundMessage> {
        {
            let mut pending = self.inbound_pending.lock().await;
            if let Some(idx) = pending.iter().position(&matches) {
                return pending.remove(idx);
            }
        }
        loop {
            let message = self.inbound_rx.lock().await.recv().await?;
            if matches(&message) {
                return Some(message);
            }
            self.inbound_pending.lock().await.push_back(message);
        }
    }

    pub async fn next_outbound(&self) -> Option<OutboundMessage> {
        self.outbound_rx.lock().await.recv().await
    }
}
