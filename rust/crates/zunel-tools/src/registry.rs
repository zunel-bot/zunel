use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::tool::{DynTool, ToolContext, ToolResult};

/// Suffix appended to error strings so the LLM can self-correct.
const HINT_SUFFIX: &str = "\n\n[Analyze the error above and try a different approach.]";

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, DynTool>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: DynTool) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Drop a single tool by name. Returns the previously-registered
    /// `DynTool` so the caller can confirm something was actually
    /// removed (or `None` if no matching name was registered). Used
    /// by the SDK facade and by the MCP reload path's targeted
    /// "drop just this server's tools" step.
    pub fn unregister(&mut self, name: &str) -> Option<DynTool> {
        self.tools.remove(name)
    }

    pub fn get(&self, name: &str) -> Option<&DynTool> {
        self.tools.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    /// Drop every tool whose name does not satisfy `keep`. Used by the
    /// MCP reload path to evict tools belonging to the server(s) being
    /// reconnected before the freshly listed tools are merged in.
    pub fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&str) -> bool,
    {
        self.tools.retain(|k, _| keep(k));
    }

    /// Move every tool from `other` into `self`, overwriting on name
    /// collision. Used by the MCP reload path to splice a freshly
    /// connected server's tools into the live registry under a single
    /// brief write lock.
    pub fn extend(&mut self, other: ToolRegistry) {
        self.tools.extend(other.tools);
    }

    /// Tool definitions in OpenAI function-call format, with `mcp_*`
    /// tools sorted to the end so they don't crowd the native tool list.
    pub fn get_definitions(&self) -> Vec<Value> {
        let (mcp, native): (Vec<&DynTool>, Vec<&DynTool>) = self
            .tools
            .values()
            .partition(|t| t.name().starts_with("mcp_"));

        let mut out: Vec<Value> = Vec::with_capacity(native.len() + mcp.len());
        let mut push = |t: &DynTool| {
            out.push(json!({
                "type": "function",
                "function": {
                    "name": t.name(),
                    "description": t.description(),
                    "parameters": t.parameters(),
                }
            }));
        };
        for t in &native {
            push(t);
        }
        for t in &mcp {
            push(t);
        }
        out
    }

    /// Dispatch a tool call. Always returns `Ok(ToolResult)` — schema
    /// or unknown-tool failures become `ToolResult::err` with the
    /// standard `HINT_SUFFIX` appended.
    pub async fn execute(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolContext,
    ) -> std::result::Result<ToolResult, std::convert::Infallible> {
        let Some(tool) = self.tools.get(name) else {
            return Ok(ToolResult::err(format!(
                "unknown tool: {name}{HINT_SUFFIX}"
            )));
        };
        if let Err(err) = validate_args(name, &tool.parameters(), &args) {
            return Ok(ToolResult::err(format!("{err}{HINT_SUFFIX}")));
        }
        Ok(tool.execute(args, ctx).await)
    }
}

/// Minimal JSON-schema validation: checks `required` keys exist and
/// values match the declared primitive type. Covers the subset of
/// JSON Schema actually used by zunel's tool definitions.
fn validate_args(tool: &str, schema: &Value, args: &Value) -> Result<()> {
    let invalid = |message: String| Error::InvalidArgs {
        tool: tool.to_string(),
        message,
    };
    let Some(obj) = schema.as_object() else {
        return Ok(());
    };
    if let Some(req) = obj.get("required").and_then(Value::as_array) {
        for key in req {
            let Some(k) = key.as_str() else { continue };
            if args.get(k).is_none() {
                return Err(invalid(format!("missing required field: {k}")));
            }
        }
    }
    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for (k, prop) in props {
            let Some(v) = args.get(k) else { continue };
            let ty = prop.get("type").and_then(Value::as_str).unwrap_or("any");
            let ok = match ty {
                "string" => v.is_string(),
                "integer" => v.is_i64() || v.is_u64(),
                "number" => v.is_number(),
                "boolean" => v.is_boolean(),
                "array" => v.is_array(),
                "object" => v.is_object(),
                _ => true,
            };
            if !ok {
                return Err(invalid(format!("field {k}: expected {ty}, got {v}")));
            }
        }
    }
    Ok(())
}
