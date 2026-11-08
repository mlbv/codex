//! Applies captured Multi-Agent V2 catalog overrides and namespaces to tool specifications.
//! Parameter schemas retain harness-owned encryption annotations and enabled delegation inputs.

use crate::session::session::Session;
use crate::tools::context::ToolInvocation;
use crate::tools::registry::CoreToolRuntime;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolExecutor;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use std::sync::Arc;

pub(crate) const MULTI_AGENT_V2_NAMESPACE_DESCRIPTION: &str =
    "Tools for spawning and managing sub-agents.";

pub(super) fn multi_agent_v2_handler(
    handler: impl CoreToolRuntime + 'static,
    namespace: Option<&str>,
    description_override: Option<&str>,
    parameters_override: Option<&str>,
) -> Arc<dyn CoreToolRuntime> {
    let parameters_override = parameters_override.map(|parameters| -> Result<JsonSchema, &str> {
        let mut parameters = codex_tools::parse_catalog_tool_parameters(parameters)?;
        if let ToolSpec::Function(tool) = handler.spec()
            && let Some(properties) = tool.parameters.properties
        {
            // Argument transport requires these markers even without server encryption config.
            for (name, schema) in *properties {
                #[cfg(feature = "claude-code")]
                if tool.name == "spawn_agent"
                    && matches!(name.as_str(), "name" | "run_in_background")
                {
                    parameters
                        .properties
                        .get_or_insert_default()
                        .insert(name.clone(), schema.clone());
                }
                if schema.encrypted == Some(true) {
                    let property = parameters
                        .properties
                        .as_mut()
                        .and_then(|properties| properties.get_mut(&name))
                        .ok_or("schema omits an encrypted parameter")?;
                    property.encrypted = Some(true);
                }
            }
        }
        Ok(parameters)
    });
    if let Some(Err(reason)) = &parameters_override {
        tracing::warn!(tool = %handler.tool_name(), reason, "Invalid catalog tool parameters; using bundled parameters");
    }
    let parameters_override = parameters_override.and_then(Result::ok);
    if namespace.is_none() && description_override.is_none() && parameters_override.is_none() {
        return Arc::new(handler);
    }
    Arc::new(MultiAgentV2ToolOverrides {
        handler: Arc::new(handler),
        namespace: namespace.map(str::to_owned),
        description_override: description_override.map(str::to_owned),
        parameters_override,
        #[cfg(feature = "claude-code")]
        claude_delegation: false,
    })
}

#[cfg(feature = "claude-code")]
pub(super) fn claude_delegation_handler(
    handler: Arc<dyn CoreToolRuntime>,
    enabled: bool,
) -> Arc<dyn CoreToolRuntime> {
    if !enabled {
        return handler;
    }
    Arc::new(MultiAgentV2ToolOverrides {
        handler,
        namespace: None,
        description_override: None,
        parameters_override: None,
        claude_delegation: true,
    })
}

#[cfg(all(test, feature = "claude-code"))]
mod tests {
    use super::*;
    use crate::tools::handlers::multi_agents::SendInputHandler;
    use crate::tools::handlers::multi_agents_spec::SpawnAgentToolOptions;
    use crate::tools::handlers::multi_agents_v2::SendMessageHandler;
    use crate::tools::handlers::multi_agents_v2::SpawnAgentHandler;

    #[test]
    fn claude_delegation_disabled_preserves_native_message_spec() {
        let expected = SendInputHandler.spec();
        let handler = claude_delegation_handler(Arc::new(SendInputHandler), false);
        assert_eq!(handler.spec(), expected);
    }

    #[test]
    fn claude_delegation_extends_deferred_v1_broadcast_output() {
        let handler = claude_delegation_handler(Arc::new(SendInputHandler), true);
        let ToolSpec::Namespace(namespace) = handler.spec() else {
            unreachable!()
        };
        let ResponsesApiNamespaceTool::Function(tool) = &namespace.tools[0] else {
            unreachable!()
        };
        assert!(tool.description.contains("broadcast"));
        let output = tool.output_schema.as_ref().unwrap().to_value();
        assert_eq!(
            output["anyOf"][0]["required"],
            serde_json::json!(["submission_id"])
        );
        assert_eq!(
            output["anyOf"][1]["required"],
            serde_json::json!(["submissions"])
        );
        let search_spec: ToolSpec = handler
            .search_info()
            .unwrap()
            .entry
            .to_loadable_spec()
            .into();
        let ToolSpec::Namespace(search_namespace) = search_spec else {
            unreachable!()
        };
        let ResponsesApiNamespaceTool::Function(search_tool) = &search_namespace.tools[0] else {
            unreachable!()
        };
        assert_eq!(search_namespace.name, namespace.name);
        assert_eq!(search_tool.description, tool.description);
        assert_eq!(search_tool.parameters, tool.parameters);
        assert_eq!(search_tool.defer_loading, Some(true));
        assert!(search_tool.output_schema.is_none());
    }

    #[test]
    fn claude_delegation_guidance_survives_namespaced_catalog_overrides() {
        let handler = claude_delegation_handler(
            multi_agent_v2_handler(
                SendMessageHandler,
                Some("agents"),
                Some("Configured message guidance. Does not trigger a new turn."),
                Some(
                    r#"{"type":"object","properties":{"target":{"type":"string"},"message":{"type":"string"}},"required":["target","message"],"additionalProperties":false}"#,
                ),
            ),
            true,
        );
        let ToolSpec::Namespace(namespace) = handler.spec() else {
            unreachable!()
        };
        assert_eq!(namespace.name, "agents");
        let ResponsesApiNamespaceTool::Function(tool) = &namespace.tools[0] else {
            unreachable!()
        };
        assert!(tool.description.contains("Configured message guidance."));
        assert!(
            tool.description
                .contains("main wakes the main conversation")
        );
        assert!(tool.description.contains("broadcast"));
        assert!(!tool.description.contains("Does not trigger a new turn."));
        assert_eq!(
            tool.parameters.properties.as_ref().unwrap()["message"].encrypted,
            Some(true)
        );
    }

    #[test]
    fn claude_code_spawn_inputs_survive_catalog_parameters() {
        let catalog = r#"{"type":"object","properties":{"task_name":{"type":"string"},"message":{"type":"string"}},"required":["task_name","message"],"additionalProperties":false}"#;
        for enabled in [false, true] {
            let handler = multi_agent_v2_handler(
                SpawnAgentHandler::new(
                    SpawnAgentToolOptions {
                        claude_delegation: enabled,
                        ..Default::default()
                    },
                    None,
                ),
                None,
                None,
                Some(catalog),
            );
            let ToolSpec::Function(spec) = handler.spec() else {
                unreachable!()
            };
            let properties = spec.parameters.properties.unwrap();
            assert_eq!(properties.contains_key("name"), enabled);
            assert_eq!(properties.contains_key("run_in_background"), enabled);
            assert_eq!(properties["message"].encrypted, Some(true));
        }
    }
}

struct MultiAgentV2ToolOverrides {
    handler: Arc<dyn CoreToolRuntime>,
    namespace: Option<String>,
    description_override: Option<String>,
    parameters_override: Option<JsonSchema>,
    #[cfg(feature = "claude-code")]
    claude_delegation: bool,
}

impl ToolExecutor<ToolInvocation> for MultiAgentV2ToolOverrides {
    fn tool_name(&self) -> ToolName {
        let tool_name = self.handler.tool_name();
        match &self.namespace {
            Some(namespace) => ToolName::namespaced(namespace.clone(), tool_name.name),
            None => tool_name,
        }
    }

    fn spec(&self) -> ToolSpec {
        let mut spec = self.handler.spec();
        if let ToolSpec::Function(tool) = &mut spec {
            if let Some(description) = &self.description_override {
                tool.description.clone_from(description);
            }
            if let Some(parameters) = &self.parameters_override {
                tool.parameters.clone_from(parameters);
            }
        }
        #[cfg(feature = "claude-code")]
        if self.claude_delegation {
            match &mut spec {
                ToolSpec::Function(tool) => {
                    crate::tools::handlers::multi_agents_spec::apply_claude_delegation_extensions(
                        tool,
                    );
                }
                ToolSpec::Namespace(namespace) => {
                    for tool in &mut namespace.tools {
                        if let ResponsesApiNamespaceTool::Function(tool) = tool {
                            crate::tools::handlers::multi_agents_spec::apply_claude_delegation_extensions(tool);
                        }
                    }
                }
                _ => {}
            }
        }
        match (&self.namespace, spec) {
            (Some(namespace), ToolSpec::Function(tool)) => {
                ToolSpec::Namespace(ResponsesApiNamespace {
                    name: namespace.clone(),
                    description: MULTI_AGENT_V2_NAMESPACE_DESCRIPTION.to_string(),
                    tools: vec![ResponsesApiNamespaceTool::Function(tool)],
                })
            }
            (_, spec) => spec,
        }
    }

    fn exposure(&self) -> ToolExposure {
        self.handler.exposure()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        self.handler.supports_parallel_tool_calls()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        let info = self.handler.search_info()?;
        #[cfg(feature = "claude-code")]
        if self.claude_delegation {
            return ToolSearchInfo::from_spec(
                info.entry.search_text,
                self.spec(),
                info.source_info,
            );
        }
        Some(info)
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        self.handler.handle(invocation)
    }
}

impl CoreToolRuntime for MultiAgentV2ToolOverrides {
    fn wait_until_ready<'a>(&'a self, session: &'a Arc<Session>) -> Option<BoxFuture<'a, ()>> {
        self.handler.wait_until_ready(session)
    }

    fn matches_kind(&self, payload: &crate::tools::context::ToolPayload) -> bool {
        self.handler.matches_kind(payload)
    }

    fn create_diff_consumer(
        &self,
    ) -> Option<Box<dyn crate::tools::registry::ToolArgumentDiffConsumer>> {
        self.handler.create_diff_consumer()
    }
}
