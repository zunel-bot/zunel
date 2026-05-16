//! Context builder for zunel. Assembles the system prompt and
//! message list that go to the LLM each turn.

mod builder;
mod error;
mod runtime_tag;
mod templates;

pub use builder::ContextBuilder;
pub use error::{Error, Result};
pub use runtime_tag::{strip as strip_runtime_context, CLOSE_TAG, OPEN_TAG};
pub use templates::render_max_iterations_message;
