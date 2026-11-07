//! Anthropic tool definitions as the tools Codex exposes: functions, MCP namespaces, and the
//! server tools Codex search answers.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;

use codex_protocol::DEFAULT_FUNCTION_NAMESPACE;
use codex_protocol::config_types::WebSearchUserLocationType;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ResponsesApiWebSearchFilters;
use codex_tools::ResponsesApiWebSearchUserLocation;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_tools::default_namespace_description;
use codex_tools::mcp_tool_to_responses_api_tool;
use codex_tools::parse_tool_input_schema;
use serde::Deserialize;
use serde_json::Value;

use crate::advisor::ADVISOR_DESCRIPTION;
use crate::advisor::AdvisorTool;
use crate::client_tools;
use crate::client_toolsets;
use crate::error::ApiError;
use crate::mcp_connector::McpServer;
use crate::server_tools::ToolSearchIndex;

const MCP_PREFIX: &str = "mcp__";
const MCP_DELIMITER: &str = "__";

#[derive(Debug, Default)]
pub(crate) struct ToolSet {
    pub(crate) available: Option<HashSet<String>>,
    pub(crate) specs: Vec<ToolSpec>,
    /// `(namespace, callable name)` -> Anthropic tool name.
    names: HashMap<(Option<String>, String), String>,
    /// Anthropic tool name -> `(namespace, callable name)`.
    callables: HashMap<String, (Option<String>, String)>,
    /// Anthropic names of tools whose input streams as the model writes it.
    eager: HashSet<String>,
    /// Codex namespace -> Anthropic client toolset; calls there carry `toolset_name`.
    toolsets: HashMap<String, String>,
    /// `tool_search_tool_*`, which makes `defer_loading` tools searchable.
    pub(crate) tool_search: Option<ToolSearchTool>,
    /// Deferred tools, loaded by tool search.
    pub(crate) deferred: Vec<ToolSpec>,
    /// MCP connector toolsets; `prepare` connects them and adds their tools.
    pub(crate) mcp_toolsets: Vec<McpToolset>,
    /// Namespace -> MCP connector server; calls there run on this server.
    pub(crate) mcp_namespaces: HashMap<String, String>,
    /// `mcp_servers` by name.
    pub(crate) mcp_servers: HashMap<String, McpServer>,
    /// The deferred tools' search index, once every tool is known.
    pub(crate) tool_search_index: Option<ToolSearchIndex>,
    pub(crate) web_search: Option<ServerTool>,
    pub(crate) web_fetch: Option<ServerTool>,
    pub(crate) advisor: Option<AdvisorTool>,
    /// System prompt additions of Anthropic-defined tools.
    pub(crate) instructions: Vec<&'static str>,
    /// This request's model searches with `web.run`, which this server runs.
    pub(crate) web_run: bool,
}

/// A `web_search_*` or `web_fetch_*` tool definition.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct ServerTool {
    #[serde(default)]
    pub(crate) max_uses: Option<u64>,
    #[serde(default)]
    pub(crate) allowed_domains: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) blocked_domains: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) user_location: Option<UserLocation>,
    #[serde(default)]
    pub(crate) max_content_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct UserLocation {
    #[serde(default)]
    pub(crate) city: Option<String>,
    #[serde(default)]
    pub(crate) region: Option<String>,
    #[serde(default)]
    pub(crate) country: Option<String>,
    #[serde(default)]
    pub(crate) timezone: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ToolSearchTool {
    pub(crate) name: String,
    /// The regex variant's input field is `pattern`; BM25's is `query`.
    pub(crate) regex: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct McpToolset {
    pub(crate) mcp_server_name: String,
    #[serde(default)]
    pub(crate) default_config: ToolConfig,
    #[serde(default)]
    pub(crate) configs: HashMap<String, ToolConfig>,
    /// A pinned tool list: used instead of listing the server's tools.
    #[serde(default)]
    pub(crate) tools: Option<Vec<PinnedTool>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct ToolConfig {
    #[serde(default)]
    pub(crate) enabled: Option<bool>,
    #[serde(default)]
    pub(crate) defer_loading: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
pub(crate) struct PinnedTool {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: String,
    pub(crate) input_schema: Value,
}

#[derive(Deserialize)]
struct CustomTool {
    name: String,
    #[serde(default)]
    description: String,
    input_schema: Value,
    #[serde(default)]
    eager_input_streaming: bool,
    #[serde(default)]
    defer_loading: bool,
}

#[derive(Deserialize)]
struct DefinedTool {
    name: String,
    #[serde(default)]
    eager_input_streaming: bool,
    #[serde(default)]
    defer_loading: bool,
}

#[derive(Deserialize)]
struct ToolsetEntry {
    #[serde(default)]
    configs: HashMap<String, ToolConfig>,
}

impl ToolSet {
    /// Resolve another catalog against MCP lists already fetched for this request.
    pub(crate) fn subset(&self, definitions: &[Value]) -> Result<Self, ApiError> {
        let mut set = Self::from_request(definitions)?;
        let toolsets = std::mem::take(&mut set.mcp_toolsets);
        for toolset in &toolsets {
            let listed = self
                .mcp_toolsets
                .iter()
                .find(|resolved| resolved.mcp_server_name == toolset.mcp_server_name)
                .and_then(|resolved| resolved.tools.as_ref())
                .ok_or_else(|| {
                    ApiError::invalid_request(format!(
                        "tools: unresolved MCP toolset `{}`",
                        toolset.mcp_server_name
                    ))
                })?;
            set.add_mcp_tools(toolset, listed.clone())?;
        }
        set.mcp_toolsets = toolsets;
        Ok(set)
    }

    pub(crate) fn limit_to(&mut self, active: &Self) {
        let mut available = active.callables.keys().cloned().collect::<HashSet<_>>();
        if active.web_search.is_some() || active.web_fetch.is_some() {
            available.insert("web.run".to_string());
        }
        if let Some(search) = &active.tool_search {
            available.insert(search.name.clone());
        }
        self.available = Some(available);
        self.deferred = active.deferred.clone();
        self.web_search = active.web_search.clone();
        self.web_fetch = active.web_fetch.clone();
    }

    pub(crate) fn is_available(&self, namespace: Option<&str>, name: &str) -> bool {
        self.available.as_ref().is_none_or(|available| {
            let name = if namespace == Some("web") && name == "run" {
                "web.run".to_string()
            } else {
                self.anthropic_name(namespace, name)
            };
            available.contains(&name)
        })
    }
    pub(crate) fn from_request(tools: &[Value]) -> Result<Self, ApiError> {
        let mut set = Self::default();
        let mut namespaces: BTreeMap<String, usize> = BTreeMap::new();
        for tool in tools {
            if let Some(kind) = tool.get("type").and_then(Value::as_str)
                && kind.starts_with("tool_search_tool_")
            {
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        kind.trim_end_matches(char::is_numeric)
                            .trim_end_matches('_')
                            .to_string()
                    });
                set.tool_search = Some(ToolSearchTool {
                    name,
                    regex: kind.starts_with("tool_search_tool_regex"),
                });
            }
        }
        for tool in tools {
            match tool.get("type").and_then(Value::as_str) {
                None | Some("custom") => {
                    let tool: CustomTool = parse_definition(tool)?;
                    set.add_function(
                        Function {
                            name: tool.name,
                            namespace: None,
                            description: tool.description,
                            schema: tool.input_schema,
                            eager: tool.eager_input_streaming,
                            deferred: tool.defer_loading,
                        },
                        &mut namespaces,
                    )?;
                }
                Some(kind) if kind.starts_with("tool_search_tool_") => {}
                Some(kind) if kind.starts_with("advisor_") => {
                    let advisor: AdvisorTool = parse_definition(tool)?;
                    set.add_function(
                        Function {
                            name: advisor.name.clone(),
                            namespace: None,
                            description: ADVISOR_DESCRIPTION.to_string(),
                            schema: serde_json::json!({ "type": "object", "properties": {} }),
                            eager: false,
                            deferred: false,
                        },
                        &mut namespaces,
                    )?;
                    set.advisor = Some(advisor);
                }
                Some("mcp_toolset") => set.mcp_toolsets.push(parse_definition(tool)?),
                Some(kind) if client_toolsets::toolset(kind).is_some() => {
                    let Some(toolset) = client_toolsets::toolset(kind) else {
                        continue;
                    };
                    let entry: ToolsetEntry = parse_definition(tool)?;
                    if let Some(unknown) = entry.configs.keys().find(|name| {
                        !toolset
                            .members
                            .iter()
                            .any(|member| member.name == name.as_str())
                    }) {
                        return Err(ApiError::invalid_request(format!(
                            "tools.{kind}.configs: unknown member `{unknown}`"
                        )));
                    }
                    // The backend reserves `computer` as a namespace.
                    let namespace = format!("{}_toolset", toolset.name);
                    set.toolsets
                        .insert(namespace.clone(), toolset.name.to_string());
                    for member in toolset.members {
                        let config = entry.configs.get(member.name).cloned().unwrap_or_default();
                        if !config.enabled.unwrap_or(member.enabled_by_default) {
                            continue;
                        }
                        set.add_function(
                            Function {
                                name: member.name.to_string(),
                                namespace: Some(namespace.clone()),
                                description: member.description.to_string(),
                                schema: member.input_schema,
                                eager: false,
                                deferred: config.defer_loading.unwrap_or_default(),
                            },
                            &mut namespaces,
                        )?;
                    }
                    set.instructions.extend(toolset.instructions);
                }
                Some(kind) if kind.starts_with("web_search_") => {
                    set.web_search = Some(server_tool(tool, "web_search")?);
                }
                Some(kind) if kind.starts_with("web_fetch_") => {
                    set.web_fetch = Some(server_tool(tool, "web_fetch")?);
                }
                Some(kind) => {
                    let Some(definition) = client_tools::definition(kind, tool) else {
                        return Err(ApiError::invalid_request(format!(
                            "tools: tool type `{kind}` is not supported by the Codex backend"
                        )));
                    };
                    let tool: DefinedTool = parse_definition(tool)?;
                    set.add_function(
                        Function {
                            name: tool.name,
                            namespace: None,
                            description: definition.description,
                            schema: definition.input_schema,
                            eager: tool.eager_input_streaming,
                            deferred: tool.defer_loading,
                        },
                        &mut namespaces,
                    )?;
                    set.instructions.extend(definition.instructions);
                }
            }
        }
        set.sort_namespaces();
        Ok(set)
    }

    /// Tools of an MCP connector server, under Codex's `mcp__<server>__` namespace.
    pub(crate) fn add_mcp_tools(
        &mut self,
        toolset: &McpToolset,
        tools: Vec<PinnedTool>,
    ) -> Result<(), ApiError> {
        let namespace = format!("{MCP_PREFIX}{}{MCP_DELIMITER}", toolset.mcp_server_name);
        self.mcp_namespaces
            .insert(namespace.clone(), toolset.mcp_server_name.clone());
        let mut namespaces = self
            .specs
            .iter()
            .enumerate()
            .filter_map(|(index, spec)| match spec {
                ToolSpec::Namespace(spec) => Some((spec.name.clone(), index)),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        for tool in tools {
            let config = toolset.configs.get(&tool.name);
            let setting = |pick: fn(&ToolConfig) -> Option<bool>| {
                config
                    .and_then(pick)
                    .or_else(|| pick(&toolset.default_config))
            };
            if !setting(|config| config.enabled).unwrap_or(true) {
                continue;
            }
            let rmcp_tool: rmcp::model::Tool = serde_json::from_value(serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.input_schema,
            }))
            .map_err(|err| ApiError::invalid_request(format!("mcp tool `{}`: {err}", tool.name)))?;
            let function = mcp_tool_to_responses_api_tool(
                &ToolName::namespaced(namespace.clone(), tool.name.clone()),
                &rmcp_tool,
                /*schema_max_bytes*/ None,
            )
            .map_err(|err| ApiError::invalid_request(format!("mcp tool `{}`: {err}", tool.name)))?;
            self.insert_function(
                format!("{namespace}{}", tool.name),
                Some(namespace.clone()),
                function,
                /*eager*/ false,
                setting(|config| config.defer_loading).unwrap_or_default(),
                &mut namespaces,
            )?;
        }
        self.sort_namespaces();
        Ok(())
    }

    fn sort_namespaces(&mut self) {
        for spec in &mut self.specs {
            if let ToolSpec::Namespace(namespace) = spec {
                namespace.tools.sort_by(|left, right| {
                    namespace_tool_name(left).cmp(namespace_tool_name(right))
                });
            }
        }
    }

    fn add_function(
        &mut self,
        function: Function,
        namespaces: &mut BTreeMap<String, usize>,
    ) -> Result<(), ApiError> {
        let Function {
            name,
            namespace,
            description,
            mut schema,
            eager,
            deferred,
        } = function;
        // OpenAI models require `properties`; Codex inserts it for MCP schemas that omit it.
        if let Value::Object(object) = &mut schema
            && object.get("properties").is_none_or(Value::is_null)
        {
            object.insert("properties".to_string(), Value::Object(Default::default()));
        }
        let parameters = parse_tool_input_schema(&schema).map_err(|err| {
            ApiError::invalid_request(format!("tools.{name}.input_schema: {err}"))
        })?;
        let (namespace, callable) = match namespace {
            Some(namespace) => (Some(namespace), name.clone()),
            None => split_mcp_name(&name),
        };
        let function = ResponsesApiTool {
            name: callable,
            description,
            strict: false,
            defer_loading: None,
            parameters,
            output_schema: None,
        };
        self.insert_function(name, namespace, function, eager, deferred, namespaces)
    }

    fn insert_function(
        &mut self,
        name: String,
        namespace: Option<String>,
        function: ResponsesApiTool,
        eager: bool,
        deferred: bool,
        namespaces: &mut BTreeMap<String, usize>,
    ) -> Result<(), ApiError> {
        let callable = function.name.clone();
        match &namespace {
            _ if deferred && self.tool_search.is_some() => {
                self.deferred.push(match &namespace {
                    Some(namespace) => ToolSpec::Namespace(ResponsesApiNamespace {
                        name: namespace.clone(),
                        description: default_namespace_description(namespace),
                        tools: vec![ResponsesApiNamespaceTool::Function(function)],
                    }),
                    None => ToolSpec::Function(function),
                });
            }
            Some(namespace) => {
                let index = *namespaces.entry(namespace.clone()).or_insert_with(|| {
                    self.specs.push(ToolSpec::Namespace(ResponsesApiNamespace {
                        name: namespace.clone(),
                        description: default_namespace_description(namespace),
                        tools: Vec::new(),
                    }));
                    self.specs.len() - 1
                });
                if let ToolSpec::Namespace(spec) = &mut self.specs[index] {
                    spec.tools
                        .push(ResponsesApiNamespaceTool::Function(function));
                }
            }
            None => self.specs.push(ToolSpec::Function(function)),
        }
        let key = (namespace, callable);
        if self.names.insert(key.clone(), name.clone()).is_some() {
            return Err(ApiError::invalid_request(format!(
                "tools: duplicate tool name `{name}`"
            )));
        }
        if eager {
            self.eager.insert(name.clone());
        }
        self.callables.insert(name, key);
        Ok(())
    }

    pub(crate) fn is_advisor(&self, namespace: Option<&str>, name: &str) -> bool {
        namespace.is_none()
            && self
                .advisor
                .as_ref()
                .is_some_and(|advisor| advisor.name == name)
    }

    /// The `toolset_name` a call in `namespace` carries.
    pub(crate) fn toolset(&self, namespace: Option<&str>) -> Option<&str> {
        self.toolsets.get(namespace?).map(String::as_str)
    }

    /// The Codex namespace of a client toolset.
    pub(crate) fn toolset_namespace(&self, toolset: &str) -> Option<String> {
        self.toolsets
            .iter()
            .find_map(|(namespace, name)| (name == toolset).then(|| namespace.clone()))
    }

    /// The Anthropic name of a Codex function call. Calls of the default `functions`
    /// namespace may come back with or without the namespace.
    pub(crate) fn anthropic_name(&self, namespace: Option<&str>, name: &str) -> String {
        let key = |namespace: Option<&str>| (namespace.map(str::to_string), name.to_string());
        self.names
            .get(&key(namespace))
            .or_else(|| self.names.get(&key(None)))
            .cloned()
            .unwrap_or_else(|| match namespace {
                Some(namespace) if namespace != DEFAULT_FUNCTION_NAMESPACE => {
                    format!("{namespace}{name}")
                }
                _ => name.to_string(),
            })
    }

    /// The Codex `(namespace, name)` of an Anthropic tool name.
    pub(crate) fn callable(&self, anthropic_name: &str) -> (Option<String>, String) {
        self.callables
            .get(anthropic_name)
            .cloned()
            .unwrap_or_else(|| split_mcp_name(anthropic_name))
    }

    pub(crate) fn is_eager(&self, anthropic_name: &str) -> bool {
        self.eager.contains(anthropic_name)
    }
}

struct Function {
    name: String,
    namespace: Option<String>,
    description: String,
    schema: Value,
    eager: bool,
    deferred: bool,
}

fn parse_definition<T: for<'de> Deserialize<'de>>(tool: &Value) -> Result<T, ApiError> {
    serde_json::from_value(tool.clone())
        .map_err(|err| ApiError::invalid_request(format!("tools: {err}")))
}

fn server_tool(tool: &Value, name: &str) -> Result<ServerTool, ApiError> {
    let tool: ServerTool = parse_definition(tool)?;
    if tool.allowed_domains.is_some() && tool.blocked_domains.is_some() {
        return Err(ApiError::invalid_request(format!(
            "tools.{name}: use either allowed_domains or blocked_domains, not both"
        )));
    }
    Ok(tool)
}

/// `mcp__server__tool` -> (`mcp__server__`, `tool`), the layout Codex gives MCP tools.
fn split_mcp_name(name: &str) -> (Option<String>, String) {
    if let Some(rest) = name.strip_prefix(MCP_PREFIX)
        && let Some(split) = rest.find(MCP_DELIMITER)
        && split > 0
        && split + MCP_DELIMITER.len() < rest.len()
    {
        let namespace = format!("{MCP_PREFIX}{}{MCP_DELIMITER}", &rest[..split]);
        return (
            Some(namespace),
            rest[split + MCP_DELIMITER.len()..].to_string(),
        );
    }
    (None, name.to_string())
}

fn namespace_tool_name(tool: &ResponsesApiNamespaceTool) -> &str {
    match tool {
        ResponsesApiNamespaceTool::Function(tool) => &tool.name,
        ResponsesApiNamespaceTool::Custom(tool) => &tool.name,
    }
}

impl ServerTool {
    /// Narrows the hosted tool Codex config produced; the hosted tool has no blocklist.
    pub(crate) fn apply_hosted(&self, mut spec: ToolSpec) -> ToolSpec {
        if let ToolSpec::WebSearch {
            filters,
            user_location,
            ..
        } = &mut spec
        {
            if let Some(allowed_domains) = &self.allowed_domains {
                *filters = Some(ResponsesApiWebSearchFilters {
                    allowed_domains: Some(allowed_domains.clone()),
                });
            }
            if let Some(location) = &self.user_location {
                *user_location = Some(ResponsesApiWebSearchUserLocation {
                    r#type: WebSearchUserLocationType::Approximate,
                    country: location.country.clone(),
                    region: location.region.clone(),
                    city: location.city.clone(),
                    timezone: location.timezone.clone(),
                });
            }
        }
        spec
    }
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
