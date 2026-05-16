//! `ConverseStream` → `StreamEvent` translation for [`super::provider`].
//!
//! Bedrock streams typed events (not SSE) over `EventReceiver`. This
//! module owns the per-content-block bookkeeping required to map them
//! into zunel's `StreamEvent` shape:
//!
//! * `MessageStart`: ignored (no analog in zunel's stream model).
//! * `ContentBlockStart { ToolUse { tool_use_id, name } }`: emit a
//!   `ToolCallDelta` carrying id+name with empty arguments_fragment so
//!   the accumulator records both fields up-front.
//! * `ContentBlockDelta { Text(s) }`: emit `ContentDelta(s)`.
//! * `ContentBlockDelta { ToolUse { input: partial JSON } }`: emit a
//!   `ToolCallDelta` carrying just `arguments_fragment`. Bedrock's
//!   per-event `content_block_index` is what disambiguates parallel
//!   tool calls — it lines up directly with zunel's `index` field.
//! * `MessageStop { stop_reason }`: latch finish_reason for the final
//!   `Done`.
//! * `Metadata { usage }`: latch usage for the final `Done`.
//!
//! When the stream ends (or the SDK hands us `None`), we finalize the
//! accumulator and emit a single `Done(LLMResponse)`.

use std::sync::Arc;

use aws_sdk_bedrockruntime::types::{
    ContentBlockDelta, ContentBlockStart, ConverseStreamOutput as StreamEventEnum,
};
use aws_sdk_bedrockruntime::Client;
use futures::stream::BoxStream;

use super::provider::BedrockProvider;
use super::wire::{
    convert_messages, convert_tools, reasoning_to_additional_fields, stop_reason_to_finish_reason,
    token_usage_to_usage,
};
use crate::base::{ChatMessage, GenerationSettings, LLMResponse, StreamEvent, ToolSchema, Usage};
use crate::error::{Error, Result};
use crate::tool_call_accumulator::ToolCallAccumulator;

impl BedrockProvider {
    pub(super) fn stream_impl<'a>(
        &'a self,
        model: &'a str,
        messages: &'a [ChatMessage],
        tools: &'a [ToolSchema],
        settings: &'a GenerationSettings,
    ) -> BoxStream<'a, Result<StreamEvent>> {
        let client: Arc<Client> = self.client();
        let model = model.to_string();
        Box::pin(async_stream::try_stream! {
            let converted = convert_messages(messages)?;
            let tool_cfg = convert_tools(tools)?;
            let additional = reasoning_to_additional_fields(settings.reasoning_effort.as_deref());

            let mut request = client.converse_stream().model_id(&model);
            for sys in converted.system {
                request = request.system(sys);
            }
            for msg in converted.messages {
                request = request.messages(msg);
            }
            if let Some(cfg) = tool_cfg {
                request = request.tool_config(cfg);
            }
            if let Some(infer) = build_inference_config(settings) {
                request = request.inference_config(infer);
            }
            if let Some(extra) = additional {
                request = request.additional_model_request_fields(extra);
            }

            let mut output = request
                .send()
                .await
                .map_err(|e| map_sdk_error("ConverseStream", e))?;

            let mut accumulated = String::new();
            let mut acc = ToolCallAccumulator::default();
            let mut finish_reason: Option<String> = None;
            let mut usage = Usage::default();

            loop {
                let next = output
                    .stream
                    .recv()
                    .await
                    .map_err(|e| map_sdk_error("ConverseStream chunk", e))?;
                let Some(event) = next else { break; };
                match event {
                    StreamEventEnum::MessageStart(_) => {}
                    StreamEventEnum::ContentBlockStart(start) => {
                        if let (Some(ContentBlockStart::ToolUse(tu)), idx) = (start.start, start.content_block_index) {
                            let idx = idx.max(0) as u32;
                            let delta = StreamEvent::ToolCallDelta {
                                index: idx,
                                id: Some(tu.tool_use_id.clone()),
                                name: Some(tu.name.clone()),
                                arguments_fragment: None,
                            };
                            acc.push(StreamEvent::ToolCallDelta {
                                index: idx,
                                id: Some(tu.tool_use_id),
                                name: Some(tu.name),
                                arguments_fragment: None,
                            });
                            yield delta;
                        }
                    }
                    StreamEventEnum::ContentBlockDelta(delta_event) => {
                        let idx = delta_event.content_block_index.max(0) as u32;
                        if let Some(delta) = delta_event.delta {
                            match delta {
                                ContentBlockDelta::Text(text) if !text.is_empty() => {
                                    accumulated.push_str(&text);
                                    yield StreamEvent::ContentDelta(text);
                                }
                                ContentBlockDelta::ToolUse(tu_delta) => {
                                    let frag = tu_delta.input;
                                    if !frag.is_empty() {
                                        acc.push(StreamEvent::ToolCallDelta {
                                            index: idx,
                                            id: None,
                                            name: None,
                                            arguments_fragment: Some(frag.clone()),
                                        });
                                        yield StreamEvent::ToolCallDelta {
                                            index: idx,
                                            id: None,
                                            name: None,
                                            arguments_fragment: Some(frag),
                                        };
                                    }
                                }
                                _ => {
                                    // Reasoning deltas, citations, etc. — accepted
                                    // by the SDK but not surfaced to the agent loop.
                                }
                            }
                        }
                    }
                    StreamEventEnum::ContentBlockStop(_) => {}
                    StreamEventEnum::MessageStop(stop) => {
                        finish_reason = Some(stop_reason_to_finish_reason(&stop.stop_reason));
                    }
                    StreamEventEnum::Metadata(meta) => {
                        if let Some(token_usage) = meta.usage {
                            usage = token_usage_to_usage(&token_usage);
                        }
                    }
                    other => {
                        tracing::debug!(?other, "bedrock: unhandled ConverseStream event");
                    }
                }
            }

            let tool_calls = acc
                .finalize()
                .map_err(|e| Error::Parse(format!("bedrock tool_call reassembly: {e}")))?;
            let response = LLMResponse {
                content: if accumulated.is_empty() {
                    None
                } else {
                    Some(accumulated)
                },
                tool_calls,
                usage,
                finish_reason,
            };
            yield StreamEvent::Done(response);
        })
    }
}

pub(super) fn build_inference_config(
    settings: &GenerationSettings,
) -> Option<aws_sdk_bedrockruntime::types::InferenceConfiguration> {
    if settings.temperature.is_none() && settings.max_tokens.is_none() {
        return None;
    }
    let mut builder = aws_sdk_bedrockruntime::types::InferenceConfiguration::builder();
    if let Some(temp) = settings.temperature {
        builder = builder.temperature(temp);
    }
    if let Some(max) = settings.max_tokens {
        builder = builder.max_tokens(max as i32);
    }
    Some(builder.build())
}

/// Surface SDK errors as `Error::ProviderReturned` so the agent runner's
/// existing classification works without bedrock-specific branches.
pub(super) fn map_sdk_error<E, R>(
    op: &'static str,
    err: aws_sdk_bedrockruntime::error::SdkError<E, R>,
) -> Error
where
    E: std::fmt::Display + std::fmt::Debug,
    R: std::fmt::Debug,
{
    use aws_sdk_bedrockruntime::error::SdkError;
    match err {
        SdkError::ServiceError(svc) => Error::ProviderReturned {
            status: 500,
            body: format!("bedrock {op}: {}", svc.into_err()),
        },
        SdkError::TimeoutError(_) => Error::ProviderReturned {
            status: 408,
            body: format!("bedrock {op}: request timed out"),
        },
        SdkError::DispatchFailure(d) => Error::ProviderReturned {
            status: 502,
            body: format!("bedrock {op}: dispatch failure: {d:?}"),
        },
        SdkError::ResponseError(r) => Error::ProviderReturned {
            status: 502,
            body: format!("bedrock {op}: bad response: {r:?}"),
        },
        SdkError::ConstructionFailure(c) => Error::ProviderReturned {
            status: 500,
            body: format!("bedrock {op}: request construction failed: {c:?}"),
        },
        other => Error::ProviderReturned {
            status: 500,
            body: format!("bedrock {op}: {other:?}"),
        },
    }
}
