//! Reassemble streamed tool-call fragments into `ToolCallRequest` values.
//!
//! Providers emit `StreamEvent::ToolCallDelta` chunks keyed by `index`.
//! The first chunk for a given index typically carries `id` and `name`;
//! subsequent chunks stream the `arguments` JSON incrementally. Once the
//! stream completes, `finalize` parses each accumulated argument buffer
//! and produces `ToolCallRequest`s in ascending index order.

use std::collections::BTreeMap;

use crate::{StreamEvent, ToolCallRequest};

#[derive(Debug, Default)]
struct Partial {
    id: Option<String>,
    name: Option<String>,
    args_buf: String,
}

/// Accumulates `StreamEvent::ToolCallDelta` fragments keyed by `index`
/// and produces whole `ToolCallRequest`s on finalize.
///
/// Non-tool events (`ContentDelta`, `Done`) are silently ignored, so a
/// single accumulator can be fed the entire event stream without the
/// caller needing to filter.
#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    partials: BTreeMap<u32, Partial>,
}

impl ToolCallAccumulator {
    /// Feed a single stream event. Non-`ToolCallDelta` events are ignored.
    pub fn push(&mut self, event: StreamEvent) {
        if let StreamEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments_fragment,
        } = event
        {
            let slot = self.partials.entry(index).or_default();
            if let Some(id) = id {
                slot.id = Some(id);
            }
            if let Some(name) = name {
                slot.name = Some(name);
            }
            if let Some(frag) = arguments_fragment {
                slot.args_buf.push_str(&frag);
            }
        }
    }

    /// Consume the accumulator and produce reassembled tool calls in
    /// ascending `index` order. Returns `Err` if any fragment is
    /// missing its id/name or has invalid JSON arguments.
    pub fn finalize(self) -> Result<Vec<ToolCallRequest>, ToolCallAssemblyError> {
        let mut out = Vec::with_capacity(self.partials.len());
        for (index, partial) in self.partials {
            let id = partial
                .id
                .ok_or(ToolCallAssemblyError::MissingId { index })?;
            let name = partial.name.ok_or(ToolCallAssemblyError::MissingName {
                index,
                id: id.clone(),
            })?;
            // Providers may emit an empty-args tool call as a single
            // delta carrying only id+name. Treat an empty buffer as
            // `{}` so no-argument tools work out of the box.
            let raw = if partial.args_buf.is_empty() {
                "{}".to_string()
            } else {
                partial.args_buf
            };
            let arguments: serde_json::Value = serde_json::from_str(&raw).map_err(|source| {
                ToolCallAssemblyError::InvalidJson {
                    index,
                    id: id.clone(),
                    raw: raw.clone(),
                    source,
                }
            })?;
            out.push(ToolCallRequest {
                id,
                name,
                arguments,
                index,
            });
        }
        Ok(out)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolCallAssemblyError {
    #[error("tool call at index {index} is missing an id (provider violated spec)")]
    MissingId { index: u32 },
    #[error("tool call {id} (index {index}) is missing a function name")]
    MissingName { index: u32, id: String },
    #[error("tool call {id} (index {index}) has invalid JSON arguments: {source}. raw = {raw:?}")]
    InvalidJson {
        index: u32,
        id: String,
        raw: String,
        #[source]
        source: serde_json::Error,
    },
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Any valid JSON object, split into two contiguous fragments
        /// at arbitrary byte offsets, must reassemble byte-identical
        /// (modulo JSON value equivalence).
        #[test]
        fn reassembly_round_trips(
            raw_args in r#"\{"[a-z]{1,6}":"[a-z0-9_ ]{0,20}"\}"#,
            split_at in 0usize..50usize,
        ) {
            // Clamp split_at to UTF-8 char boundary inside the string.
            let boundary = {
                let mut b = split_at.min(raw_args.len());
                while !raw_args.is_char_boundary(b) {
                    b -= 1;
                }
                b
            };
            let (head, tail) = raw_args.split_at(boundary);
            let mut acc = ToolCallAccumulator::default();
            acc.push(StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_p".into()),
                name: Some("t".into()),
                arguments_fragment: Some(head.to_string()),
            });
            acc.push(StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_fragment: Some(tail.to_string()),
            });
            let calls = acc.finalize().expect("valid JSON reassembles");
            let expected: serde_json::Value =
                serde_json::from_str(&raw_args).expect("regex produces valid JSON");
            prop_assert_eq!(&calls[0].arguments, &expected);
        }
    }
}
