use super::*;
use crate::agent::api::AgentTarget;
use crate::agent::child_config::build_agent_resume_config;
use crate::agent::control::render_input_preview;
use crate::tools::handlers::multi_agents_spec::create_send_input_tool_v1;
use codex_protocol::protocol::MultiAgentVersion;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;
use std::collections::HashMap;

pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(MULTI_AGENT_V1_NAMESPACE, "send_input")
    }

    fn spec(&self) -> ToolSpec {
        create_send_input_tool_v1()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        multi_agent_tool_search_info(
            "send_input send message existing agent subagent follow up interrupt redirect queue target",
            self.spec(),
        )
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            call_id,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: SendInputArgs = parse_arguments(&arguments)?;
        let broadcast = cfg!(feature = "claude-code")
            && session.enabled(codex_features::Feature::ClaudeDelegation)
            && args.target == "*";
        #[cfg(feature = "claude-code")]
        let receiver_thread_ids = if broadcast {
            crate::claude_code::delegation::native_message_targets(
                &session,
                &turn.session_source,
                &args.target,
            )
            .await
            .unwrap()
        } else {
            vec![resolve_agent_id_target(&session, &turn, &args.target).await?]
        };
        #[cfg(not(feature = "claude-code"))]
        let receiver_thread_ids =
            vec![resolve_agent_id_target(&session, &turn, &args.target).await?];
        let input_items = parse_collab_input(args.message, args.items)?;
        let prompt = render_input_preview(&input_items);
        let local_agent_control = session
            .services
            .local_agent_runtime
            .control(session.session_id());
        let mut receiver_agents = Vec::with_capacity(receiver_thread_ids.len());
        let mut residency_pins = Vec::with_capacity(receiver_thread_ids.len());
        for id in &receiver_thread_ids {
            let receiver_agent = local_agent_control.get_agent_metadata(*id);
            if receiver_agent.is_some() {
                let resume_config = build_agent_resume_config(turn.as_ref())
                    .map_err(FunctionCallError::RespondToModel)?;
                residency_pins.push(
                    local_agent_control
                        .ensure_v2_agent_loaded(resume_config, *id, /*parent*/ None)
                        .await
                        .map_err(|err| collab_agent_error(*id, err))?,
                );
            }
            let receiver_agent = receiver_agent.unwrap_or_default();
            receiver_agents.push(CollabAgentRef {
                thread_id: *id,
                agent_nickname: receiver_agent.agent_nickname,
                agent_role: receiver_agent.agent_role,
            });
            if args.interrupt {
                session
                    .services
                    .agent_control
                    .interrupt(
                        session.thread_id,
                        AgentTarget::Id(*id),
                        MultiAgentVersion::V1,
                    )
                    .await
                    .map_err(|err| collab_agent_error(*id, err))?;
            }
        }
        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::SendInput,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: receiver_thread_ids.clone(),
                    receiver_agents: Vec::new(),
                    prompt: Some(prompt.clone()),
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                }),
            )
            .await;
        let result = async {
            let mut submissions = BTreeMap::new();
            for id in &receiver_thread_ids {
                #[cfg(feature = "claude-code")]
                crate::claude_code::delegation::native_followup(&session, *id).await?;
                let submission_id = local_agent_control
                    .send_input(
                        *id,
                        input_items.clone(),
                        crate::TurnStartOptions {
                            parent_turn_id: Some(turn.sub_id.clone()),
                            root_turn_id: turn.turn_metadata_state.root_turn_id(),
                            turn_trigger: turn.turn_metadata_state.current_turn_trigger(),
                            cyber_access_program: turn.cyber_access_program,
                            ..Default::default()
                        },
                    )
                    .await
                    .map_err(|err| collab_agent_error(*id, err))?;
                submissions.insert(id.to_string(), submission_id);
            }
            Ok::<_, FunctionCallError>(submissions)
        }
        .await;
        drop(residency_pins);
        let mut agents_states = HashMap::new();
        for id in &receiver_thread_ids {
            agents_states.insert(*id, local_agent_control.get_status(*id).await);
        }
        let status = if !broadcast {
            collab_tool_call_status(
                agents_states.values().next().unwrap(),
                receiver_thread_ids.first().copied(),
            )
        } else if result.is_err()
            || agents_states
                .values()
                .any(|status| matches!(status, AgentStatus::Errored(_) | AgentStatus::NotFound))
        {
            CollabAgentToolCallStatus::Failed
        } else {
            CollabAgentToolCallStatus::Completed
        };
        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::SendInput,
                    status,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids,
                    receiver_agents,
                    prompt: Some(prompt),
                    model: None,
                    reasoning_effort: None,
                    agents_states,
                }),
            )
            .await;
        let submissions = result?;
        #[cfg(feature = "claude-code")]
        if broadcast {
            return Ok(boxed_tool_output(SendInputResult::Broadcast {
                submissions,
            }));
        }
        let output = SendInputResult::Single {
            submission_id: submissions.into_values().next().unwrap(),
        };
        Ok(boxed_tool_output(output))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
struct SendInputArgs {
    target: String,
    message: Option<String>,
    items: Option<Vec<UserInput>>,
    #[serde(default)]
    interrupt: bool,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum SendInputResult {
    Single {
        submission_id: String,
    },
    #[cfg(feature = "claude-code")]
    Broadcast {
        submissions: BTreeMap<String, String>,
    },
}

impl ToolOutput for SendInputResult {
    fn log_output(&self) -> String {
        tool_output_json_text(self, "send_input")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "send_input")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "send_input")
    }
}
