//! Calls this server runs for the client, as Codex runs them: `web.run`, `tool_search` and MCP
//! connector tools. Each answers with the client's result block and Codex's history item.

use std::fmt;

use codex_core::external_harness::ExternalToolSearch;
use codex_core::external_harness::loadable_names;
use codex_protocol::DEFAULT_FUNCTION_NAMESPACE;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::models::ResponseItem;
use codex_tools::LoadableToolSpec;
use serde_json::Value;
use serde_json::json;

use crate::anthropic::ToolResultBlock;
use crate::anthropic::ToolResultContent;
use crate::anthropic::ToolSearchResultContent;
use crate::tools::ToolSearchTool;
use crate::tools::ToolSet;
use crate::web_run::WebRunCall;

/// A call's answer: the client's result block and the output Codex history records.
#[derive(Debug)]
pub(crate) struct ServerResult {
    pub(crate) block: Value,
    pub(crate) output: ResponseItem,
    /// Result URLs later fetches may open.
    pub(crate) urls: Vec<String>,
    /// `web_search_result_location` citations of search results, by URL.
    pub(crate) citations: Vec<(String, Value)>,
    pub(crate) failed: bool,
}

impl ServerResult {
    pub(crate) fn new(block: Value, output: ResponseItem) -> Self {
        Self {
            block,
            output,
            urls: Vec::new(),
            citations: Vec::new(),
            failed: false,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ServerCall {
    WebRun(Box<WebRunCall>),
    ToolSearch {
        call_id: String,
        arguments: Value,
    },
    Advisor {
        call_id: String,
    },
    Mcp {
        call_id: String,
        server: String,
        tool: String,
        arguments: String,
    },
}

/// A request's deferred tools, searchable by Codex's `tool_search`.
pub(crate) struct ToolSearchIndex(pub(crate) ExternalToolSearch);

impl fmt::Debug for ToolSearchIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ToolSearchIndex")
    }
}

/// `server_tool_use.input` of a search: the variant's field, then `limit`.
pub(crate) fn tool_search_input(tool: &ToolSearchTool, arguments: &Value) -> Value {
    let field = if tool.regex { "pattern" } else { "query" };
    let mut input = json!({ field: arguments.get("query").cloned().unwrap_or_default() });
    if let Some(limit) = arguments.get("limit") {
        input["limit"] = limit.clone();
    }
    input
}

pub(crate) fn tool_search_result(
    index: &ToolSearchIndex,
    tools: &ToolSet,
    call_id: &str,
    arguments: Value,
) -> ServerResult {
    match index.0.search(arguments) {
        Ok(specs) => {
            let references = specs
                .iter()
                .flat_map(loadable_names)
                .map(|(namespace, name)| {
                    let namespace = (namespace != DEFAULT_FUNCTION_NAMESPACE).then_some(namespace);
                    json!({ "type": "tool_reference", "tool_name": tools.anthropic_name(namespace.as_deref(), &name) })
                })
                .collect::<Vec<_>>();
            ServerResult::new(
                json!({
                    "type": "tool_search_tool_result",
                    "tool_use_id": call_id,
                    "content": { "type": "tool_search_tool_search_result", "tool_references": references },
                }),
                tool_search_output(call_id, &specs),
            )
        }
        // Codex answers a failed search with no tools.
        Err(message) => ServerResult::new(
            json!({
                "type": "tool_search_tool_result",
                "tool_use_id": call_id,
                "content": {
                    "type": "tool_search_tool_result_error",
                    "error_code": "invalid_tool_input",
                    "error_message": message,
                },
            }),
            tool_search_output(call_id, &[]),
        ),
    }
}

fn tool_search_output(call_id: &str, specs: &[LoadableToolSpec]) -> ResponseItem {
    ResponseItem::ToolSearchOutput {
        id: None,
        call_id: Some(call_id.to_string()),
        status: "completed".to_string(),
        execution: "client".to_string(),
        tools: specs
            .iter()
            .map(|spec| serde_json::to_value(spec).unwrap_or_default())
            .collect(),
        internal_chat_message_metadata_passthrough: None,
    }
}

/// The `tool_search_call` behind an echoed search `server_tool_use`.
pub(crate) fn replay_tool_search_call(id: String, input: &Value) -> ResponseItem {
    let query = input
        .get("query")
        .or_else(|| input.get("pattern"))
        .cloned()
        .unwrap_or_default();
    let mut arguments = json!({ "query": query });
    if let Some(limit) = input.get("limit") {
        arguments["limit"] = limit.clone();
    }
    ResponseItem::ToolSearchCall {
        encrypted_content: None,
        id: None,
        call_id: Some(id),
        status: Some("completed".to_string()),
        execution: "client".to_string(),
        arguments,
        internal_chat_message_metadata_passthrough: None,
    }
}

/// The output behind an echoed `tool_search_tool_result`: the referenced tools, loaded again.
pub(crate) fn replay_tool_search_output(
    tool_use_id: &str,
    content: ToolSearchResultContent,
    tools: &ToolSet,
) -> ResponseItem {
    let specs = match (content, &tools.tool_search_index) {
        (ToolSearchResultContent::ToolSearchToolSearchResult { tool_references }, Some(index)) => {
            let names = tool_references
                .iter()
                .map(|reference| {
                    let (namespace, name) = tools.callable(&reference.tool_name);
                    (
                        namespace.unwrap_or_else(|| DEFAULT_FUNCTION_NAMESPACE.to_string()),
                        name,
                    )
                })
                .collect::<Vec<_>>();
            index.0.load(&names)
        }
        _ => Vec::new(),
    };
    tool_search_output(tool_use_id, &specs)
}

/// An MCP connector call's result block and Codex output.
pub(crate) fn mcp_result(call_id: &str, result: &CallToolResult) -> ServerResult {
    ServerResult::new(
        json!({
            "type": "mcp_tool_result",
            "tool_use_id": call_id,
            "is_error": !result.success(),
            "content": result.content.iter().map(anthropic_content).collect::<Vec<_>>(),
        }),
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some(call_id.to_string()),
            name: None,
            namespace: None,
            output: result.as_function_call_output_payload(),
            internal_chat_message_metadata_passthrough: None,
        },
    )
}

/// MCP content as an Anthropic block: text and images as such, anything else as its JSON.
fn anthropic_content(content: &Value) -> Value {
    match content.get("type").and_then(Value::as_str) {
        Some("text") => {
            json!({ "type": "text", "text": content.get("text").cloned().unwrap_or_default() })
        }
        Some("image") => json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": content.get("mimeType").cloned().unwrap_or_default(),
                "data": content.get("data").cloned().unwrap_or_default(),
            },
        }),
        _ => json!({ "type": "text", "text": content.to_string() }),
    }
}

/// The output behind an echoed `mcp_tool_result`, rebuilt as the MCP result it carried.
pub(crate) fn replay_mcp_output(
    tool_use_id: String,
    is_error: bool,
    content: Option<ToolResultContent>,
) -> ResponseItem {
    let content = match content {
        None => Vec::new(),
        Some(ToolResultContent::Text(text)) => vec![json!({ "type": "text", "text": text })],
        Some(ToolResultContent::Blocks(blocks)) => blocks.into_iter().map(mcp_content).collect(),
    };
    let result = CallToolResult {
        content,
        structured_content: None,
        is_error: is_error.then_some(true),
        meta: None,
    };
    mcp_result(&tool_use_id, &result).output
}

fn mcp_content(block: ToolResultBlock) -> Value {
    match block {
        ToolResultBlock::Text { text } => json!({ "type": "text", "text": text }),
        ToolResultBlock::Image {
            source: crate::anthropic::MediaSource::Base64 { media_type, data },
        } => json!({ "type": "image", "mimeType": media_type, "data": data }),
        other => json!({ "type": "text", "text": format!("{other:?}") }),
    }
}

#[cfg(test)]
#[path = "server_tools_tests.rs"]
mod tests;
