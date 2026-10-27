//! The MCP connector: remote MCP servers a request names, reached through Codex's MCP client.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_core::config::Config;
use codex_exec_server::HttpClient;
use codex_exec_server::RouteAwareHttpClient;
use codex_mcp::call_tool_result_from_rmcp;
use codex_protocol::mcp::CallToolResult;
use codex_rmcp_client::ElicitationAction;
use codex_rmcp_client::ElicitationResponse;
use codex_rmcp_client::RmcpClient;
use futures::FutureExt;
use rmcp::model::ClientCapabilities;
use rmcp::model::Implementation;
use rmcp::model::InitializeRequestParams;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ProtocolVersion;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Mutex;

use crate::error::ApiError;
use crate::tools::PinnedTool;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// An `mcp_servers` entry.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct McpServer {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) url: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) authorization_token: Option<String>,
    /// `mcp-client-2025-04-04`: tool settings on the server instead of an `mcp_toolset`.
    #[serde(default)]
    pub(crate) tool_configuration: Option<LegacyToolConfiguration>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct LegacyToolConfiguration {
    #[serde(default)]
    pub(crate) enabled: Option<bool>,
    #[serde(default)]
    pub(crate) allowed_tools: Option<Vec<String>>,
}

/// A server URL and its authorization token.
type ClientKey = (String, Option<String>);

/// Initialized clients, one per server URL and token, shared across requests.
pub(crate) struct McpConnector {
    http_client: Arc<dyn HttpClient>,
    clients: Mutex<HashMap<ClientKey, Arc<RmcpClient>>>,
}

impl McpConnector {
    pub(crate) fn new(config: &Config) -> Self {
        Self {
            http_client: Arc::new(
                RouteAwareHttpClient::new(config.http_client_factory()).with_tls_backend_fallback(),
            ),
            clients: Mutex::new(HashMap::new()),
        }
    }

    async fn client(&self, server: &McpServer) -> Result<Arc<RmcpClient>, ApiError> {
        if server.kind != "url" {
            return Err(ApiError::invalid_request(format!(
                "mcp_servers.{}: only `url` servers are supported",
                server.name
            )));
        }
        let key = (server.url.clone(), server.authorization_token.clone());
        let cached = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned();
        if let Some(client) = cached {
            return Ok(client);
        }
        let client = RmcpClient::new_streamable_http_client(
            &server.name,
            &server.url,
            server.authorization_token.clone(),
            /*http_headers*/ None,
            /*env_http_headers*/ None,
            OAuthCredentialsStoreMode::default(),
            AuthKeyringBackendKind::default(),
            Arc::clone(&self.http_client),
            /*auth_provider*/ None,
        )
        .await
        .map_err(|err| connection_error(server, &err))?;
        client
            .initialize(
                initialize_params(),
                Some(STARTUP_TIMEOUT),
                // The connector has no user to ask; elicitations are declined.
                Box::new(|_, _| {
                    async {
                        Ok(ElicitationResponse {
                            action: ElicitationAction::Decline,
                            content: None,
                            meta: None,
                        })
                    }
                    .boxed()
                }),
            )
            .await
            .map_err(|err| connection_error(server, &err))?;
        // A concurrent connect may have won; its client is kept.
        Ok(Arc::clone(
            self.clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(key)
                .or_insert_with(|| Arc::new(client)),
        ))
    }

    /// Every tool the server lists, across pages.
    pub(crate) async fn list_tools(&self, server: &McpServer) -> Result<Vec<PinnedTool>, ApiError> {
        let client = self.client(server).await?;
        let mut tools = Vec::new();
        let mut cursor = None;
        loop {
            let page = client
                .list_tools(
                    Some(PaginatedRequestParams::default().with_cursor(cursor)),
                    Some(STARTUP_TIMEOUT),
                )
                .await
                .map_err(|err| connection_error(server, &err))?;
            for tool in page.tools {
                tools.push(PinnedTool {
                    name: tool.name.to_string(),
                    description: tool
                        .description
                        .map(|description| description.to_string())
                        .unwrap_or_default(),
                    input_schema: Value::Object(tool.input_schema.as_ref().clone()),
                });
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(tools);
            }
        }
    }

    /// A tool call's result; failures become error results, as Codex reports them.
    pub(crate) async fn call(
        &self,
        server: &McpServer,
        tool: &str,
        arguments: &str,
    ) -> CallToolResult {
        let arguments = match serde_json::from_str::<Value>(arguments) {
            Ok(arguments) => arguments,
            Err(err) => return CallToolResult::from_error_text(format!("err: {err}")),
        };
        let result = match self.client(server).await {
            Ok(client) => client
                .call_tool(
                    tool.to_string(),
                    Some(arguments),
                    /*meta*/ None,
                    /*timeout*/ None,
                )
                .await
                .map(call_tool_result_from_rmcp)
                .map_err(|err| format!("tool call failed for `{}/{tool}`: {err:#}", server.name)),
            Err(err) => Err(err.message().to_string()),
        };
        CallToolResult::from_result(result)
    }
}

fn initialize_params() -> InitializeRequestParams {
    InitializeRequestParams::new(
        ClientCapabilities::default(),
        Implementation::new("codex-mcp-client", env!("CARGO_PKG_VERSION")).with_title("Codex"),
    )
    .with_protocol_version(ProtocolVersion::V_2025_06_18)
}

fn connection_error(server: &McpServer, err: &anyhow::Error) -> ApiError {
    ApiError::invalid_request(format!(
        "mcp_servers.{}: cannot reach `{}`: {err:#}",
        server.name, server.url
    ))
}
