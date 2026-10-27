//! Resolve inline tool changes in order, retaining definitions needed to replay old calls.

use serde_json::Map;
use serde_json::Value;

use crate::anthropic::{ContentBlock, MessageContent, MessagesRequest, Role};
use crate::error::ApiError;
use crate::tools::ToolSet;

pub(crate) struct ToolPlan {
    pub(crate) initial: Vec<Value>,
    pub(crate) active: Vec<Value>,
}

pub(crate) fn apply(request: &mut MessagesRequest, inline: bool) -> Result<ToolPlan, ApiError> {
    let mut known = Map::new();
    for definition in &request.tools {
        let key = key(definition)?;
        if known.insert(key.clone(), definition.clone()).is_some() {
            return Err(ApiError::invalid_request(format!(
                "tools: duplicate tool `{key}`"
            )));
        }
    }
    let searchable = request.tools.iter().any(|tool| {
        tool["type"]
            .as_str()
            .is_some_and(|kind| kind.starts_with("tool_search_tool_"))
    });
    let mut active = known
        .iter()
        .filter(|(_, definition)| !inline || searchable || definition["defer_loading"] != true)
        .map(|(key, definition)| (key.clone(), definition.clone()))
        .collect::<Map<_, _>>();
    let initial = active.values().cloned().collect();
    for message in &mut request.messages {
        let MessageContent::Blocks(blocks) = &mut message.content else {
            continue;
        };
        for block in blocks {
            let (tool, addition) = match block {
                ContentBlock::ToolAddition { tool } => (tool, true),
                ContentBlock::ToolRemoval { tool } => (tool, false),
                _ => continue,
            };
            if message.role != Role::System {
                return Err(ApiError::invalid_request(
                    "tool changes belong in system messages",
                ));
            }
            let (tool_key, member, mut definition) = if tool["type"] == "tool_definition" {
                if !addition {
                    return Err(ApiError::invalid_request(
                        "tool_removal requires a reference",
                    ));
                }
                let definition = tool["definition"].clone();
                let tool_key = key(&definition)?;
                if let Some(previous) = known.get(&tool_key) {
                    if family(previous) != family(&definition) {
                        return Err(ApiError::invalid_request(format!(
                            "tools: conflicting tool type for `{tool_key}`"
                        )));
                    }
                }
                known.insert(tool_key.clone(), definition.clone());
                (tool_key, None, definition)
            } else {
                let (tool_key, member) = reference(tool)?;
                let mut definition = known.get(&tool_key).cloned().ok_or_else(|| {
                    ApiError::invalid_request(format!(
                        "tools: unresolved tool reference `{tool_key}`"
                    ))
                })?;
                definition["defer_loading"] = Value::Bool(false);
                (tool_key, member, definition)
            };
            let notice = if let Some(member) = member {
                definition = match active.get(&tool_key) {
                    Some(active) => active.clone(),
                    None => {
                        definition["default_config"]["enabled"] = Value::Bool(false);
                        definition["configs"] = Value::Object(Map::new());
                        definition
                    }
                };
                definition["configs"][&member]["enabled"] = Value::Bool(addition);
                definition["configs"][&member]["defer_loading"] = Value::Bool(false);
                active.insert(tool_key.clone(), definition.clone());
                format!("{tool_key}.{member}")
            } else {
                if addition {
                    active.insert(tool_key.clone(), definition.clone());
                } else {
                    active.remove(&tool_key);
                }
                tool_key
            };
            *block = if addition {
                ContentBlock::ToolAddition { tool: definition }
            } else {
                ContentBlock::Text {
                    text: format!(
                        "The following tool is unavailable from this point onward: {notice}. Do not call it unless a later tool addition makes it available again."
                    ),
                }
            };
        }
    }
    request.tools = known.into_values().collect();
    Ok(ToolPlan {
        initial,
        active: active.into_values().collect(),
    })
}

/// Lite accepts appended declarations; ordinary Responses requests use the current catalog.
pub(crate) fn render(
    messages: &mut [crate::anthropic::InputMessage],
    tools: &ToolSet,
    lite: bool,
) -> Result<(), ApiError> {
    for message in messages {
        let MessageContent::Blocks(blocks) = &mut message.content else {
            continue;
        };
        let mut rendered = Vec::new();
        for block in std::mem::take(blocks) {
            let ContentBlock::ToolAddition { tool } = block else {
                rendered.push(block);
                continue;
            };
            let set = tools.subset(std::slice::from_ref(&tool))?;
            for instruction in &set.instructions {
                rendered.push(ContentBlock::Text {
                    text: instruction.to_string(),
                });
            }
            if lite {
                let mut specs = set.specs;
                if set.web_search.is_some() || set.web_fetch.is_some() {
                    specs.push(codex_web_search_extension::web_run_spec());
                }
                if set.tool_search.is_some() {
                    specs.push(
                        codex_core::external_harness::ExternalToolSearch::new(
                            tools.deferred.clone(),
                        )
                        .spec(),
                    );
                }
                let definitions = codex_tools::create_tools_json_for_responses_lite(&specs)
                    .map_err(|error| ApiError::invalid_request(format!("inline tool: {error}")))?;
                rendered.push(ContentBlock::ToolDeclaration { definitions });
            } else {
                rendered.push(ContentBlock::Text {
                    text: format!(
                        "The following tool is available from this point onward: {}.",
                        key(&tool)?
                    ),
                });
            }
        }
        *blocks = rendered;
    }
    Ok(())
}

fn family(definition: &Value) -> &str {
    definition["type"]
        .as_str()
        .unwrap_or("custom")
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_end_matches('_')
}

fn key(definition: &Value) -> Result<String, ApiError> {
    if definition["type"] == "mcp_toolset" {
        return name(definition, "mcp_server_name").map(|name| format!("mcp:{name}"));
    }
    if let Some(name) = definition["name"].as_str() {
        return Ok(name.to_string());
    }
    match definition["type"].as_str() {
        Some(kind) if kind.contains("_toolset_") => {
            Ok(kind.split("_toolset_").next().unwrap_or(kind).to_string())
        }
        _ => Err(ApiError::invalid_request(
            "tools: a tool definition needs a name",
        )),
    }
}

fn reference(tool: &Value) -> Result<(String, Option<String>), ApiError> {
    match tool["type"].as_str() {
        Some("tool_reference") => Ok((name(tool, "name")?.to_string(), None)),
        Some("mcp_toolset_reference") => Ok((format!("mcp:{}", name(tool, "server_name")?), None)),
        Some("mcp_tool_reference") => Ok((
            format!("mcp:{}", name(tool, "server_name")?),
            Some(name(tool, "name")?.to_string()),
        )),
        _ => Err(ApiError::invalid_request(
            "tools: expected a tool reference or inline definition",
        )),
    }
}

fn name<'a>(value: &'a Value, field: &str) -> Result<&'a str, ApiError> {
    value[field]
        .as_str()
        .ok_or_else(|| ApiError::invalid_request(format!("tools: missing {field}")))
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
