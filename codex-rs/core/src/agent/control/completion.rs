//! Delivers terminal child results and completion activity to the agent tree.
//!
//! Sessions capture terminal state; the controller owns routing and queue-only delivery.
//! Delivery remains best effort; metrics distinguish queue acceptance from delivery failure.

use super::LocalAgentControl;
use crate::TurnStartOptions;
use crate::agent::api::AgentTurnOutcome;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::session_prefix::format_guardian_interruption_message;
use crate::session_prefix::format_inter_agent_completion_message;
use codex_features::Feature;
use codex_protocol::AgentPath;
use codex_protocol::error::CodexErr;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_protocol::protocol::SubAgentSource;
use codex_rollout_trace::AgentResultTracePayload;
use codex_rollout_trace::ThreadTraceContext;
use std::sync::Arc;
use tracing::debug;

impl LocalAgentControl {
    /// Routes a captured terminal outcome without retaining the child's live turn context.
    pub(crate) async fn notify_parent_of_terminal_turn(
        &self,
        outcome: AgentTurnOutcome,
        trace: &ThreadTraceContext,
    ) {
        if matches!(&outcome.status, AgentStatus::Completed(_))
            && let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id, ..
            }) = &outcome.source
            && let Ok(manager) = self.runtime.upgrade()
            && let Ok(parent) = manager.get_thread(*parent_thread_id).await
            && parent.session.enabled(Feature::ClaudeDelegation)
            && let Some(parent_runtime) = crate::claude_code::session_runtime(&parent.session)
            && let Ok(child) = manager.get_thread(outcome.thread_id).await
            && let Some(child_runtime) = crate::claude_code::session_runtime(&child.session)
        {
            let owner = *parent_thread_id;
            let residency = Arc::clone(&child.residency_gate).read_owned().await;
            let control = self.clone();
            let trace = trace.clone();
            let waiting = child_runtime.clone();
            let watcher = tokio::spawn(async move {
                let _residency = residency;
                if !matches!(
                    crate::claude_code::events::wait_until_idle(
                        &waiting,
                        &parent_runtime.cancellation,
                    )
                    .await,
                    Ok(AgentStatus::Completed(_))
                ) {
                    return;
                }
                // A completion from a yielded turn is superseded by its resumed turn.
                if !child
                    .session
                    .is_latest_started_turn(&outcome.turn_id)
                    .await
                {
                    return;
                }
                control.notify_parent_of_settled_turn(outcome, &trace).await;
            });
            child_runtime.delegation.watch(owner, watcher).await;
            return;
        }
        self.notify_parent_of_settled_turn(outcome, trace).await;
    }

    async fn notify_parent_of_settled_turn(
        &self,
        outcome: AgentTurnOutcome,
        trace: &ThreadTraceContext,
    ) {
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            agent_path: Some(child_agent_path),
            ..
        }) = &outcome.source
        else {
            return;
        };
        let parent_thread_id = *parent_thread_id;
        let status = outcome.status;
        let parent_runtime = if let Ok(manager) = self.runtime.upgrade()
            && let Ok(parent) = manager.get_thread(parent_thread_id).await
            && parent.session.enabled(Feature::ClaudeDelegation)
        {
            crate::claude_code::session_runtime(&parent.session)
        } else {
            None
        };
        let Some(parent_agent_path) = child_agent_path
            .as_str()
            .rsplit_once('/')
            .and_then(|(parent, _)| AgentPath::try_from(parent).ok())
        else {
            return;
        };

        if matches!(status, AgentStatus::Completed(_))
            && let Some(parent_turn_id) = outcome.parent_turn_id
        {
            let initiating_thread_id = match outcome.initiating_agent_path.as_ref() {
                Some(initiating_agent_path) if initiating_agent_path != &parent_agent_path => {
                    self.runtime.resolve_agent_reference(
                        outcome.thread_id,
                        &outcome.source,
                        initiating_agent_path.as_str(),
                    )
                    .await
                    .inspect_err(|err| {
                        debug!(
                            "failed to resolve completed activity initiator {initiating_agent_path}: {err}"
                        );
                    })
                    .ok()
                }
                _ => Some(parent_thread_id),
            };
            if let Some(initiating_thread_id) = initiating_thread_id
                && let Err(err) = self
                    .emit_sub_agent_activity(
                        initiating_thread_id,
                        parent_turn_id,
                        SubAgentActivityItem {
                            model: None,
                            reasoning_effort: None,
                            id: format!("subagent-completed-{}", outcome.turn_id),
                            kind: SubAgentActivityKind::Completed,
                            agent_thread_id: outcome.thread_id,
                            agent_path: child_agent_path.clone(),
                        },
                    )
                    .await
            {
                debug!(
                    "failed to emit completed activity to initiating thread {initiating_thread_id}: {err}"
                );
            }
        }

        if let Some(runtime) = &parent_runtime
            && !runtime.delegation.reports_result(outcome.thread_id).await
        {
            return;
        }

        let message = match (&status, outcome.error_info) {
            (AgentStatus::Errored(error), Some(CodexErrorInfo::TooManyDenials)) => {
                Some(format_guardian_interruption_message(
                    parent_agent_path.clone(),
                    child_agent_path.clone(),
                    error,
                ))
            }
            _ => format_inter_agent_completion_message(
                parent_agent_path.clone(),
                child_agent_path.clone(),
                &status,
            ),
        };
        let Some(message) = message else {
            return;
        };
        // `communication` owns the message. Keep a second copy only when the
        // recorder will actually need it after parent delivery succeeds.
        let trace_message = trace.is_enabled().then(|| message.clone());
        let communication = InterAgentCommunication::new(
            child_agent_path.clone(),
            parent_agent_path,
            Vec::new(),
            message,
            /*trigger_turn*/ false,
        );
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Result, outcome.thread_id);
        let delivery = if let Some(runtime) = parent_runtime {
            runtime
                .events
                .emit("agent_result", communication.content.clone())
                .await
                .map(|()| outcome.turn_id.clone())
                .map_err(|error| CodexErr::InvalidRequest(error.to_string()))
        } else {
            self.send_inter_agent_communication(
                parent_thread_id,
                communication,
                context,
                TurnStartOptions::default(),
            )
            .await
        };
        if let Some(metrics) = codex_otel::global() {
            let _ = metrics.counter(
                "codex.multi_agent.result_delivery",
                /*inc*/ 1,
                &[(
                    "outcome",
                    if delivery.is_ok() { "queued" } else { "failed" },
                )],
            );
        }
        if let Err(err) = delivery {
            debug!("failed to notify parent thread {parent_thread_id}: {err}");
            return;
        }
        if let Some(message) = trace_message {
            trace.record_agent_result_interaction(
                outcome.turn_id.as_str(),
                parent_thread_id,
                &AgentResultTracePayload {
                    child_agent_path: child_agent_path.as_str(),
                    message: &message,
                    status: &status,
                },
            );
        }
    }
}
