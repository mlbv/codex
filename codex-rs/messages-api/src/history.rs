//! Anthropic messages as the conversation items Codex history holds.

use std::collections::HashMap;

use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::FileReference;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageReference;
use codex_protocol::models::InternalChatMessageMetadataPassthrough;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::WebSearchAction;
use serde_json::Value;
use serde_json::json;

use crate::advisor;
use crate::anthropic::AdvisorResultContent;
use crate::anthropic::ClearAt;
use crate::anthropic::ContentBlock;
use crate::anthropic::DocumentContent;
use crate::anthropic::DocumentSource;
use crate::anthropic::InputMessage;
use crate::anthropic::MediaSource;
use crate::anthropic::MessageContent;
use crate::anthropic::Role;
use crate::anthropic::SystemBlock;
use crate::anthropic::ToolResultBlock;
use crate::anthropic::ToolResultContent;
use crate::codec;
use crate::codec::Marker;
use crate::error::ApiError;
use crate::server_tools;
use crate::tools::ToolSet;
use crate::web_run;

/// Claude Code wraps the context it injects into user messages in this tag.
pub(crate) const CLIENT_CONTEXT_PREFIX: &str = "<system-reminder>";
const COMPACTION_PROMPT_START: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.";
/// Full compaction, and the `up_to` partial form whose summary precedes kept messages.
const NATIVE_COMPACTION_TASKS: [&str; 2] = [
    "Your task is to create a detailed summary of the conversation so far",
    "Your task is to create a detailed summary of this conversation. This summary will be placed at the start",
];

/// A request whose final instruction asks the model to summarize the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CompactionRequest {
    /// Additional tool results arrived with the instruction: compaction lands mid-turn.
    pub(crate) mid_turn: bool,
}

#[derive(Debug)]
pub(crate) struct Conversation {
    pub(crate) items: Vec<ResponseItem>,
    pub(crate) compaction: Option<CompactionRequest>,
}

pub(crate) fn conversation_items(
    messages: Vec<InputMessage>,
    tools: &ToolSet,
    stored: &HashMap<String, Vec<ResponseItem>>,
) -> Result<Conversation, ApiError> {
    let mut items = Vec::new();
    let mut compaction = None;
    // Claude Code may append system messages after the compaction instruction.
    let last_user = messages
        .iter()
        .rposition(|message| message.role == Role::User);
    for (index, message) in messages.into_iter().enumerate() {
        if message.role == Role::System
            && message.clear_at == Some(ClearAt::NextUserMessage)
            && last_user.is_some_and(|last| index < last)
        {
            continue;
        }
        let blocks = match message.content {
            MessageContent::Text(text) => vec![ContentBlock::Text { text }],
            MessageContent::Blocks(blocks) => blocks,
        };
        match message.role {
            Role::User => {
                let blocks = if Some(index) == last_user {
                    let (blocks, request) = strip_compaction_prompt(blocks);
                    compaction = request;
                    blocks
                } else {
                    blocks
                };
                user_items(blocks, stored, &mut items)?;
            }
            Role::Assistant => assistant_items(blocks, tools, &mut items)?,
            Role::System => {
                let mut content = Vec::new();
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            content.push(ContentItem::InputText { text })
                        }
                        ContentBlock::ToolDeclaration { definitions } => {
                            if !content.is_empty() {
                                items.push(message_item(
                                    "developer",
                                    std::mem::take(&mut content),
                                    None,
                                ));
                            }
                            items.push(ResponseItem::AdditionalTools {
                                id: None,
                                role: "developer".to_string(),
                                tools: definitions.into(),
                            });
                        }
                        other => return Err(unexpected_block("system", &other)),
                    }
                }
                if !content.is_empty() {
                    items.push(message_item("developer", content, None));
                }
            }
        }
    }
    Ok(Conversation { items, compaction })
}

fn strip_compaction_prompt(
    mut blocks: Vec<ContentBlock>,
) -> (Vec<ContentBlock>, Option<CompactionRequest>) {
    let is_native = matches!(
        blocks.last(),
        Some(ContentBlock::Text { text })
            if text.starts_with(COMPACTION_PROMPT_START)
                && NATIVE_COMPACTION_TASKS.iter().any(|task| text.contains(task))
    );
    if !is_native {
        return (blocks, None);
    }
    blocks.pop();
    let mid_turn = blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
    (blocks, Some(CompactionRequest { mid_turn }))
}

fn user_items(
    blocks: Vec<ContentBlock>,
    stored: &HashMap<String, Vec<ResponseItem>>,
    items: &mut Vec<ResponseItem>,
) -> Result<(), ApiError> {
    let mut content = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                let replacement = match codec::find_compaction(&text) {
                    Some((_framing, Marker::Inline(encoded), rest)) => {
                        codec::decode_items(encoded).map(|replacement| (replacement, rest))
                    }
                    Some((_framing, Marker::Stored(digest), rest)) => {
                        let replacement = stored.get(digest).cloned().ok_or_else(|| {
                            ApiError::invalid_request(format!(
                                "compaction {digest} is not in the proxy's store"
                            ))
                        })?;
                        Some((replacement, rest))
                    }
                    None => None,
                };
                match replacement {
                    Some((replacement, rest)) => {
                        flush_user(&mut content, items);
                        items.extend(replacement);
                        let rest = rest.trim();
                        if !rest.is_empty() {
                            content.push(ContentItem::InputText {
                                text: rest.to_string(),
                            });
                        }
                    }
                    None => content.push(ContentItem::InputText { text }),
                }
            }
            ContentBlock::Image { source } => content.push(input_image(source)?),
            ContentBlock::Document {
                source,
                title,
                context,
            } => content.extend(document_items(source, title, context)?),
            ContentBlock::SearchResult {
                source,
                title,
                content: result,
            } => content.push(ContentItem::InputText {
                text: search_result_text(&source, title.as_deref(), result),
            }),
            ContentBlock::ToolResult {
                tool_use_id,
                content: result,
                is_error,
            } => {
                flush_user(&mut content, items);
                items.push(ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: Some(tool_use_id),
                    name: None,
                    namespace: None,
                    output: tool_output(result, is_error)?,
                    internal_chat_message_metadata_passthrough: None,
                });
            }
            other => return Err(unexpected_block("user", &other)),
        }
    }
    flush_user(&mut content, items);
    Ok(())
}

fn flush_user(content: &mut Vec<ContentItem>, items: &mut Vec<ResponseItem>) {
    if !content.is_empty() {
        items.push(message_item(
            "user",
            std::mem::take(content),
            /*phase*/ None,
        ));
    }
}

fn assistant_items(
    blocks: Vec<ContentBlock>,
    tools: &ToolSet,
    items: &mut Vec<ResponseItem>,
) -> Result<(), ApiError> {
    // Text ahead of a tool call is commentary; Codex replays the model's own phase.
    let last_tool_call = blocks.iter().rposition(|block| {
        matches!(
            block,
            ContentBlock::ToolUse { .. } | ContentBlock::ServerToolUse { .. }
        )
    });
    for (index, block) in blocks.into_iter().enumerate() {
        match block {
            ContentBlock::Thinking { signature, .. } => {
                // Another backend's reasoning cannot be replayed to this one.
                items.extend(codec::decode_signature(&signature));
            }
            ContentBlock::RedactedThinking {} => {}
            ContentBlock::Text { text } => {
                if text.is_empty() {
                    continue;
                }
                let phase = if last_tool_call.is_some_and(|last| index < last) {
                    MessagePhase::Commentary
                } else {
                    MessagePhase::FinalAnswer
                };
                items.push(message_item(
                    "assistant",
                    vec![ContentItem::OutputText {
                        text,
                        annotations: None,
                        logprobs: None,
                    }],
                    Some(phase),
                ));
            }
            ContentBlock::ToolUse {
                id,
                name,
                input,
                toolset_name,
            } => {
                let (namespace, name) = match toolset_name {
                    Some(toolset) => (
                        Some(tools.toolset_namespace(&toolset).unwrap_or(toolset)),
                        name,
                    ),
                    None => tools.callable(&name),
                };
                items.push(ResponseItem::FunctionCall {
                    encrypted_content: None,
                    status: None,
                    id: None,
                    name,
                    namespace,
                    arguments: serde_json::to_string(&input).map_err(|err| {
                        ApiError::invalid_request(format!("tool_use.input: {err}"))
                    })?,
                    encrypted_function_args: None,
                    call_id: id,
                    internal_chat_message_metadata_passthrough: None,
                });
            }
            ContentBlock::ServerToolUse { id, name, input }
                if tools
                    .tool_search
                    .as_ref()
                    .is_some_and(|tool| tool.name == name) =>
            {
                items.push(server_tools::replay_tool_search_call(id, &input));
            }
            ContentBlock::ToolSearchToolResult {
                tool_use_id,
                content,
            } => items.push(server_tools::replay_tool_search_output(
                &tool_use_id,
                content,
                tools,
            )),
            ContentBlock::McpToolUse {
                id,
                name,
                server_name,
                input,
            } => items.push(ResponseItem::FunctionCall {
                encrypted_content: None,
                status: None,
                id: None,
                name,
                namespace: Some(format!("mcp__{server_name}__")),
                arguments: serde_json::to_string(&input).map_err(|err| {
                    ApiError::invalid_request(format!("mcp_tool_use.input: {err}"))
                })?,
                encrypted_function_args: None,
                call_id: id,
                internal_chat_message_metadata_passthrough: None,
            }),
            ContentBlock::McpToolResult {
                tool_use_id,
                is_error,
                content,
            } => items.push(server_tools::replay_mcp_output(
                tool_use_id,
                is_error,
                content,
            )),
            // Tool lists the server recorded; `prepare` reuses them.
            ContentBlock::McpToolListing { .. } => {}
            ContentBlock::ServerToolUse { id, name, .. } if tools.is_advisor(None, &name) => {
                items.push(ResponseItem::FunctionCall {
                    encrypted_content: None,
                    status: None,
                    id: None,
                    name,
                    namespace: None,
                    arguments: "{}".to_string(),
                    encrypted_function_args: None,
                    call_id: id,
                    internal_chat_message_metadata_passthrough: None,
                });
            }
            ContentBlock::AdvisorToolResult {
                tool_use_id,
                content,
            } => items.push(advisor::advice_output(
                &tool_use_id,
                match content {
                    AdvisorResultContent::Plain { text } => text,
                    AdvisorResultContent::Redacted {} => {
                        "advisor unavailable: encrypted by another backend".to_string()
                    }
                    AdvisorResultContent::Error { error_code } => {
                        format!("advisor unavailable: {error_code}")
                    }
                },
            )),
            ContentBlock::ServerToolUse { id, name, input } if tools.web_run => {
                items.push(web_run::replay_call(id, &name, input)?);
            }
            ContentBlock::WebSearchToolResult {
                tool_use_id,
                content,
            } if tools.web_run => {
                items.push(web_run::replay_search_output(tool_use_id, content));
            }
            ContentBlock::WebFetchToolResult {
                tool_use_id,
                content,
            } if tools.web_run => {
                items.push(web_run::replay_fetch_output(tool_use_id, content)?);
            }
            // The hosted tool's call item stands for its results.
            ContentBlock::WebSearchToolResult { .. } => {}
            ContentBlock::ServerToolUse { id, name, input } if name == web_run::WEB_SEARCH => {
                items.push(ResponseItem::WebSearchCall {
                    encrypted_content: None,
                    id: Some(ResponseItemId::from_server(id)),
                    status: Some("completed".to_string()),
                    action: Some(WebSearchAction::Search {
                        query: input
                            .get("query")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        queries: None,
                    }),
                    internal_chat_message_metadata_passthrough: None,
                });
            }
            other => return Err(unexpected_block("assistant", &other)),
        }
    }
    Ok(())
}

fn tool_output(
    content: Option<ToolResultContent>,
    is_error: bool,
) -> Result<FunctionCallOutputPayload, ApiError> {
    let mut payload = match content {
        None => FunctionCallOutputPayload::from_text(String::new()),
        Some(ToolResultContent::Text(text)) => FunctionCallOutputPayload::from_text(text),
        Some(ToolResultContent::Blocks(blocks)) => {
            let mut items = Vec::new();
            for block in blocks {
                items.extend(tool_result_items(block)?);
            }
            FunctionCallOutputPayload::from_content_items(items)
        }
    };
    payload.success = Some(!is_error);
    Ok(payload)
}

fn tool_result_items(
    block: ToolResultBlock,
) -> Result<Vec<FunctionCallOutputContentItem>, ApiError> {
    Ok(match block {
        ToolResultBlock::Text { text } => vec![FunctionCallOutputContentItem::InputText { text }],
        ToolResultBlock::Image { source } => match input_image(source)? {
            ContentItem::InputImage { image, detail } => {
                vec![FunctionCallOutputContentItem::InputImage { image, detail }]
            }
            _ => Vec::new(),
        },
        ToolResultBlock::Document {
            source,
            title,
            context,
        } => document_items(source, title, context)?
            .into_iter()
            .filter_map(|item| match item {
                ContentItem::InputText { text } => {
                    Some(FunctionCallOutputContentItem::InputText { text })
                }
                ContentItem::InputImage { image, detail } => {
                    Some(FunctionCallOutputContentItem::InputImage { image, detail })
                }
                ContentItem::InputFile { file } => {
                    Some(FunctionCallOutputContentItem::InputFile { file })
                }
                ContentItem::InputAudio { .. } | ContentItem::OutputText { .. } => None,
            })
            .collect(),
        ToolResultBlock::SearchResult {
            source,
            title,
            content,
        } => vec![FunctionCallOutputContentItem::InputText {
            text: search_result_text(&source, title.as_deref(), content),
        }],
        // Without a tool search tool every tool is loaded up front; a reference only names it.
        ToolResultBlock::ToolReference { tool_name } => {
            vec![FunctionCallOutputContentItem::InputText {
                text: format!("Tool loaded: {tool_name}"),
            }]
        }
        ToolResultBlock::BrowserState {
            tabs,
            state_changes,
        } => vec![FunctionCallOutputContentItem::InputText {
            text: json!({ "type": "browser_state", "tabs": tabs, "state_changes": state_changes })
                .to_string(),
        }],
    })
}

fn input_image(source: MediaSource) -> Result<ContentItem, ApiError> {
    let image_url = match source {
        MediaSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
        MediaSource::Url { url } => url,
        MediaSource::File {} => return Err(unsupported("file sources")),
    };
    Ok(ContentItem::InputImage {
        image: ImageReference::Inline { image_url },
        detail: None,
    })
}

fn document_items(
    source: DocumentSource,
    title: Option<String>,
    context: Option<String>,
) -> Result<Vec<ContentItem>, ApiError> {
    let filename = document_filename(title.as_deref(), &source);
    let header = [title, context]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
    let mut items = Vec::new();
    if !header.is_empty() {
        items.push(ContentItem::InputText { text: header });
    }
    match source {
        DocumentSource::Text { data } => items.push(ContentItem::InputText { text: data }),
        DocumentSource::Content {
            content: DocumentContent::Text(text),
        } => items.push(ContentItem::InputText { text }),
        DocumentSource::Content {
            content: DocumentContent::Blocks(blocks),
        } => {
            for block in blocks {
                for item in tool_result_items(block)? {
                    match item {
                        FunctionCallOutputContentItem::InputText { text } => {
                            items.push(ContentItem::InputText { text });
                        }
                        FunctionCallOutputContentItem::InputImage { image, detail } => {
                            items.push(ContentItem::InputImage { image, detail });
                        }
                        FunctionCallOutputContentItem::InputFile { file } => {
                            items.push(ContentItem::InputFile { file });
                        }
                        FunctionCallOutputContentItem::InputAudio { .. }
                        | FunctionCallOutputContentItem::EncryptedContent { .. } => {}
                    }
                }
            }
        }
        DocumentSource::Base64 { media_type, data } => items.push(ContentItem::InputFile {
            file: FileReference::Inline {
                filename,
                file_data: format!("data:{media_type};base64,{data}"),
            },
        }),
        DocumentSource::Url { url } => items.push(ContentItem::InputFile {
            file: FileReference::Url { file_url: url },
        }),
        DocumentSource::File {} => return Err(unsupported("file sources")),
    }
    Ok(items)
}

/// `input_file` data needs a filename; Anthropic documents carry at most a title.
fn document_filename(title: Option<&str>, source: &DocumentSource) -> String {
    let extension = match source {
        DocumentSource::Base64 { media_type, .. } => match media_type.as_str() {
            "application/pdf" => "pdf",
            "text/plain" => "txt",
            other => other.rsplit('/').next().unwrap_or("bin"),
        },
        _ => "txt",
    };
    let stem = title
        .map(|title| {
            title
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        })
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "document".to_string());
    format!("{stem}.{extension}")
}

fn search_result_text(source: &str, title: Option<&str>, content: Vec<SystemBlock>) -> String {
    let body = content
        .into_iter()
        .map(|SystemBlock::Text { text }| text)
        .collect::<Vec<_>>()
        .join("\n");
    match title {
        Some(title) => format!("{title}\n{source}\n\n{body}"),
        None => format!("{source}\n\n{body}"),
    }
}

fn message_item(
    role: &str,
    content: Vec<ContentItem>,
    phase: Option<MessagePhase>,
) -> ResponseItem {
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: role.to_string(),
        internal_chat_message_metadata_passthrough: content_kinds(role, &content).map(|kinds| {
            InternalChatMessageMetadataPassthrough {
                content_item_kinds: Some(kinds),
                ..Default::default()
            }
        }),
        content,
        phase,
    }
}

/// Codex's classification of each content part, as its own history records it.
pub(crate) fn content_kinds(role: &str, content: &[ContentItem]) -> Option<Vec<ContentItemKind>> {
    let kind = |part: &ContentItem| match (role, part) {
        ("developer", _) => "generic.developer_instructions",
        (_, ContentItem::InputText { text })
            if text.trim_start().starts_with(CLIENT_CONTEXT_PREFIX) =>
        {
            "additional_content.system_reminder"
        }
        (_, ContentItem::InputText { .. } | ContentItem::OutputText { .. }) => "user.text",
        (_, ContentItem::InputImage { .. }) => "user.image",
        (_, ContentItem::InputAudio { .. }) => "user.audio",
        (_, ContentItem::InputFile { .. }) => "user.file",
    };
    matches!(role, "user" | "developer").then(|| {
        content
            .iter()
            .map(|part| ContentItemKind(kind(part).to_string()))
            .collect()
    })
}

fn unsupported(what: &str) -> ApiError {
    ApiError::invalid_request(format!("{what} are not supported by the Codex backend"))
}

fn unexpected_block(role: &str, block: &ContentBlock) -> ApiError {
    ApiError::invalid_request(format!(
        "messages: `{}` blocks are not valid in {role} messages",
        block_type(block)
    ))
}

fn block_type(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::ToolDeclaration { .. } => "tool declaration",
        ContentBlock::ToolAddition { .. } => "tool_addition",
        ContentBlock::ToolRemoval { .. } => "tool_removal",
        ContentBlock::Text { .. } => "text",
        ContentBlock::Image { .. } => "image",
        ContentBlock::Document { .. } => "document",
        ContentBlock::SearchResult { .. } => "search_result",
        ContentBlock::Thinking { .. } => "thinking",
        ContentBlock::RedactedThinking {} => "redacted_thinking",
        ContentBlock::ToolUse { .. } => "tool_use",
        ContentBlock::ToolResult { .. } => "tool_result",
        ContentBlock::ServerToolUse { .. } => "server_tool_use",
        ContentBlock::WebSearchToolResult { .. } => "web_search_tool_result",
        ContentBlock::WebFetchToolResult { .. } => "web_fetch_tool_result",
        ContentBlock::ToolSearchToolResult { .. } => "tool_search_tool_result",
        ContentBlock::McpToolUse { .. } => "mcp_tool_use",
        ContentBlock::McpToolResult { .. } => "mcp_tool_result",
        ContentBlock::McpToolListing { .. } => "mcp_tool_listing",
        ContentBlock::AdvisorToolResult { .. } => "advisor_tool_result",
    }
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
