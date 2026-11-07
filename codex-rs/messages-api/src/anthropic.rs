//! Messages API request shapes. Responses are emitted as JSON where they are produced.

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub(crate) struct MessagesRequest {
    pub(crate) model: String,
    pub(crate) messages: Vec<InputMessage>,
    #[serde(default)]
    pub(crate) system: Option<SystemPrompt>,
    #[serde(default)]
    pub(crate) tools: Vec<Value>,
    #[serde(default)]
    pub(crate) tool_choice: Option<ToolChoice>,
    #[serde(default)]
    pub(crate) stream: bool,
    #[serde(default)]
    pub(crate) stop_sequences: Vec<String>,
    #[serde(default)]
    pub(crate) thinking: Option<ThinkingConfig>,
    #[serde(default)]
    pub(crate) output_config: Option<OutputConfig>,
    #[serde(default)]
    pub(crate) context_management: Option<ContextManagement>,
    #[serde(default)]
    pub(crate) metadata: Option<Metadata>,
    #[serde(default)]
    pub(crate) speed: Option<String>,
    #[serde(default)]
    pub(crate) service_tier: Option<String>,
    #[serde(default)]
    pub(crate) mcp_servers: Vec<crate::mcp_connector::McpServer>,
    /// Code execution container; Codex has no hosted container.
    #[serde(default)]
    pub(crate) container: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum SystemPrompt {
    Text(String),
    Blocks(Vec<SystemBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum SystemBlock {
    Text { text: String },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    User,
    Assistant,
    System,
}

#[derive(Debug, Deserialize)]
pub(crate) struct InputMessage {
    pub(crate) role: Role,
    pub(crate) content: MessageContent,
    #[serde(default)]
    pub(crate) output_config: Option<OutputConfig>,
    #[serde(default)]
    pub(crate) clear_at: Option<ClearAt>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClearAt {
    Never,
    NextUserMessage,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ContentBlock {
    #[serde(skip)]
    ToolDeclaration {
        definitions: Vec<Value>,
    },
    ToolAddition {
        tool: Value,
    },
    ToolRemoval {
        tool: Value,
    },
    Text {
        text: String,
    },
    Image {
        source: MediaSource,
    },
    Document {
        source: DocumentSource,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        context: Option<String>,
    },
    SearchResult {
        source: String,
        #[serde(default)]
        title: Option<String>,
        content: Vec<SystemBlock>,
    },
    Thinking {
        signature: String,
    },
    RedactedThinking {},
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(default)]
        toolset_name: Option<String>,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: Option<ToolResultContent>,
        #[serde(default)]
        is_error: bool,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
    },
    WebSearchToolResult {
        tool_use_id: String,
        content: WebSearchResultContent,
    },
    WebFetchToolResult {
        tool_use_id: String,
        content: WebFetchResultContent,
    },
    ToolSearchToolResult {
        tool_use_id: String,
        content: ToolSearchResultContent,
    },
    McpToolUse {
        id: String,
        name: String,
        server_name: String,
        input: Value,
    },
    McpToolResult {
        tool_use_id: String,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        content: Option<ToolResultContent>,
    },
    McpToolListing {
        mcp_server_name: String,
        tools: Vec<crate::tools::PinnedTool>,
    },
    AdvisorToolResult {
        tool_use_id: String,
        content: AdvisorResultContent,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AdvisorResultContent {
    #[serde(rename = "advisor_result")]
    Plain { text: String },
    /// Another backend's encrypted advice.
    #[serde(rename = "advisor_redacted_result")]
    Redacted {},
    #[serde(rename = "advisor_tool_result_error")]
    Error { error_code: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ToolSearchResultContent {
    ToolSearchToolSearchResult { tool_references: Vec<ToolReference> },
    ToolSearchToolResultError {},
}

#[derive(Debug, Deserialize)]
pub(crate) struct ToolReference {
    pub(crate) tool_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum WebSearchResultContent {
    Results(Vec<WebSearchResult>),
    Error(ServerToolError),
}

#[derive(Debug, Deserialize)]
pub(crate) struct WebSearchResult {
    pub(crate) url: String,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) encrypted_content: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ServerToolError {
    pub(crate) error_code: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WebFetchResultContent {
    WebFetchResult {
        url: String,
        content: FetchedDocument,
    },
    WebFetchToolResultError {
        error_code: String,
    },
}

#[derive(Debug, Deserialize)]
pub(crate) struct FetchedDocument {
    pub(crate) source: DocumentSource,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum ToolResultContent {
    Text(String),
    Blocks(Vec<ToolResultBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ToolResultBlock {
    Text {
        text: String,
    },
    Image {
        source: MediaSource,
    },
    Document {
        source: DocumentSource,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        context: Option<String>,
    },
    SearchResult {
        source: String,
        #[serde(default)]
        title: Option<String>,
        content: Vec<SystemBlock>,
    },
    ToolReference {
        tool_name: String,
    },
    /// Browser toolset tab context.
    BrowserState {
        #[serde(default)]
        tabs: Value,
        #[serde(default)]
        state_changes: Value,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum MediaSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
    File {},
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum DocumentSource {
    Base64 { media_type: String, data: String },
    Text { data: String },
    Content { content: DocumentContent },
    Url { url: String },
    File {},
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum DocumentContent {
    Text(String),
    Blocks(Vec<ToolResultBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ToolChoice {
    Auto {
        #[serde(default)]
        disable_parallel_tool_use: bool,
    },
    Any {
        #[serde(default)]
        disable_parallel_tool_use: bool,
    },
    Tool {
        name: String,
        #[serde(default)]
        disable_parallel_tool_use: bool,
    },
    None,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ThinkingConfig {
    Enabled {
        budget_tokens: u64,
        #[serde(default)]
        display: Option<ThinkingDisplay>,
    },
    Adaptive {
        #[serde(default)]
        display: Option<ThinkingDisplay>,
    },
    Disabled,
    BetweenTools,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThinkingDisplay {
    Summarized,
    Omitted,
    Updates,
}

#[derive(Debug, Deserialize)]
pub(crate) struct OutputConfig {
    #[serde(default)]
    pub(crate) effort: Option<String>,
    #[serde(default)]
    pub(crate) format: Option<OutputFormat>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum OutputFormat {
    JsonSchema { schema: Value },
}

#[derive(Debug, Deserialize)]
pub(crate) struct ContextManagement {
    #[serde(default)]
    pub(crate) edits: Vec<ContextEdit>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum ContextEdit {
    #[serde(rename = "clear_thinking_20251015")]
    ClearThinking {
        #[serde(default)]
        keep: Option<KeepThinking>,
    },
    #[serde(rename = "clear_tool_uses_20250919")]
    ClearToolUses {
        #[serde(default)]
        trigger: Option<EditThreshold>,
        #[serde(default)]
        keep: Option<EditThreshold>,
        #[serde(default)]
        clear_at_least: Option<EditThreshold>,
        #[serde(default)]
        exclude_tools: Vec<String>,
        #[serde(default)]
        clear_tool_inputs: bool,
    },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum KeepThinking {
    All(KeepAll),
    Turns(EditThreshold),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KeepAll {
    All,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub(crate) struct EditThreshold {
    pub(crate) value: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Metadata {
    #[serde(default)]
    pub(crate) user_id: Option<String>,
}
