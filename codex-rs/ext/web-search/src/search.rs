//! Standalone search: the `web.run` tool and its search requests, shared by the extension's
//! executor and harnesses that run `web.run` calls themselves.

use std::fmt;
use std::sync::Arc;

use codex_api::ApiError;
use codex_api::SearchClient;
use codex_api::SearchCommands;
use codex_api::SearchRequest;
use codex_api::SearchResponse;
use codex_api::SearchSettings;
use codex_api::map_api_error;
use codex_core::X_CODEX_TURN_METADATA_HEADER;
use codex_core::config::Config;
use codex_extension_api::ResponsesApiTool;
use codex_extension_api::ToolSpec;
use codex_extension_api::parse_tool_input_schema_without_compaction;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::ReqwestTransport;
use codex_login::AuthManager;
use codex_login::default_client::ClientRedirectPolicy;
use codex_login::default_client::add_originator_header;
use codex_login::default_client::create_client_for_route_async;
use codex_login::default_client::create_transport_for_routes_async;
use codex_model_provider::ACCOUNT_ROUTING_HEADER;
use codex_model_provider::SharedModelProvider;
use codex_model_provider::WorkspaceRoutingContext;
use codex_model_provider::create_model_provider;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::ResponseItem;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::default_namespace_description;
use http::HeaderMap;
use http::HeaderValue;

use crate::extension::WebSearchExtensionConfig;
use crate::extension::external_web_access_for_mode;
use crate::history::recent_input;
use crate::schema::commands_schema;

pub const WEB_NAMESPACE: &str = "web";
pub const RUN_TOOL_NAME: &str = "run";
const WEB_RUN_DESCRIPTION: &str = include_str!("../web_run_description.md");

pub fn web_run_spec() -> ToolSpec {
    web_run_spec_with_literature_search(false)
}

pub(crate) fn web_run_spec_with_literature_search(supports_literature_search: bool) -> ToolSpec {
    // parse schema without compaction that removes field metadata/descriptions to match hosted tool definition
    let schema = commands_schema(supports_literature_search);
    let parameters = match parse_tool_input_schema_without_compaction(&schema) {
        Ok(parameters) => parameters,
        Err(err) => panic!("search command schema should parse: {err}"),
    };

    ToolSpec::Namespace(ResponsesApiNamespace {
        name: WEB_NAMESPACE.to_string(),
        description: default_namespace_description(WEB_NAMESPACE),
        tools: vec![ResponsesApiNamespaceTool::Function(ResponsesApiTool {
            name: RUN_TOOL_NAME.to_string(),
            description: WEB_RUN_DESCRIPTION.to_string(),
            strict: false,
            parameters,
            output_schema: None,
            defer_loading: None,
        })],
    })
}

/// A `web.run` call's arguments; empty arguments run no command.
pub fn parse_commands(arguments: &str) -> serde_json::Result<SearchCommands> {
    if arguments.trim().is_empty() {
        return Ok(SearchCommands::default());
    }
    serde_json::from_str(arguments)
}

/// The request a `web.run` call sends, with the conversation tail the search sees.
pub fn search_request(
    session_id: String,
    model: String,
    history: &[ResponseItem],
    commands: SearchCommands,
    settings: SearchSettings,
    max_output_tokens: u64,
) -> SearchRequest {
    SearchRequest {
        id: session_id,
        model,
        reasoning: None,
        input: recent_input(history),
        commands: Some(commands),
        settings: Some(settings),
        max_output_tokens: Some(max_output_tokens),
    }
}

/// Standalone search under one config's provider.
#[derive(Clone)]
pub struct StandaloneSearch {
    http_client_factory: HttpClientFactory,
    provider: SharedModelProvider,
    workspace_routing: Arc<WorkspaceRoutingContext>,
    settings: SearchSettings,
}

impl StandaloneSearch {
    /// `None` when `config` disables search or its provider has no standalone search.
    pub fn from_config(config: &Config, auth_manager: Arc<AuthManager>) -> Option<Self> {
        let search_config = WebSearchExtensionConfig::from(config);
        let mode = config.web_search_mode.value();
        if !search_config.provider_supports_search || mode == WebSearchMode::Disabled {
            return None;
        }
        Some(Self::new(
            &search_config,
            create_model_provider(search_config.provider.clone(), Some(auth_manager)),
            Arc::new(config.workspace_routing_context()),
            mode,
        ))
    }

    pub(crate) fn new(
        config: &WebSearchExtensionConfig,
        provider: SharedModelProvider,
        workspace_routing: Arc<WorkspaceRoutingContext>,
        mode: WebSearchMode,
    ) -> Self {
        Self {
            http_client_factory: config.http_client_factory.clone(),
            provider,
            workspace_routing,
            settings: SearchSettings {
                external_web_access: Some(external_web_access_for_mode(mode)),
                ..config.settings.clone()
            },
        }
    }

    /// Settings from config, before a caller narrows them.
    pub fn settings(&self) -> &SearchSettings {
        &self.settings
    }

    pub async fn connect(&self) -> Result<SearchConnection, SearchError> {
        let setup = codex_model_provider::resolve_request(
            self.provider.as_ref(),
            /*scope*/ None,
            Some(&self.workspace_routing),
            self.http_client_factory.clone(),
        )
        .await
        .map_err(SearchError::Codex)?;
        let transport = if setup.redirect_policy == ClientRedirectPolicy::Reject
            || setup
                .api_provider
                .headers
                .contains_key(ACCOUNT_ROUTING_HEADER)
        {
            create_client_for_route_async(
                setup.http_client_factory,
                setup.api_provider.url_for_path("alpha/search"),
                ClientRouteClass::Api,
                ClientRedirectPolicy::Reject,
            )
            .await
            .map(ReqwestTransport::from_http_client)
        } else {
            create_transport_for_routes_async(
                setup.http_client_factory,
                ClientRouteClass::Api,
                setup.redirect_policy,
            )
            .await
        }
        .map_err(SearchError::Transport)?;
        Ok(SearchConnection {
            client: SearchClient::new(transport, setup.api_provider, setup.api_auth),
        })
    }
}

pub struct SearchConnection {
    client: SearchClient<ReqwestTransport>,
}

impl SearchConnection {
    pub async fn search(
        &self,
        request: &SearchRequest,
        originator: Option<&str>,
        turn_metadata: Option<&str>,
    ) -> Result<SearchResponse, SearchError> {
        self.client
            .search(request, search_request_headers(originator, turn_metadata))
            .await
            .map_err(SearchError::Api)
    }
}

#[derive(Debug)]
pub enum SearchError {
    Codex(CodexErr),
    Transport(std::io::Error),
    Api(ApiError),
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codex(err) => fmt::Display::fmt(err, f),
            Self::Transport(err) => fmt::Display::fmt(err, f),
            Self::Api(err) => fmt::Display::fmt(err, f),
        }
    }
}

impl From<SearchError> for CodexErr {
    fn from(err: SearchError) -> Self {
        match err {
            SearchError::Codex(err) => err,
            SearchError::Transport(err) => CodexErrorDetails::Io(err).into(),
            SearchError::Api(err) => map_api_error(err),
        }
    }
}

pub(crate) fn search_request_headers(
    originator: Option<&str>,
    turn_metadata: Option<&str>,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(turn_metadata) = turn_metadata
        && let Ok(header_value) = HeaderValue::from_str(turn_metadata)
    {
        headers.insert(X_CODEX_TURN_METADATA_HEADER, header_value);
    }

    if let Some(originator) = originator {
        add_originator_header(&mut headers, originator);
    }
    headers
}
