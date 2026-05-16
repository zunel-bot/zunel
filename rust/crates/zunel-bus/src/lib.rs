//! In-process message bus types.

mod events;
mod runtime;

pub use events::{InboundMessage, MessageKind, OutboundMessage};
pub use runtime::{BusError, InboundPublisher, MessageBus, OutboundPublisher};
