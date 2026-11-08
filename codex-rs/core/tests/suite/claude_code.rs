//! Background observations must wake idle conversations and paused children.

use anyhow::Result;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::Feature;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use core_test_support::ThreadIdle;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::sse;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use test_case::test_case;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

fn answer(id: &str, text: &str) -> String {
    sse(vec![ev_assistant_message(id, text), ev_completed(id)])
}

fn call(id: &str, name: &str, arguments: Value) -> String {
    sse(vec![
        ev_function_call(id, name, &arguments.to_string()),
        ev_completed(id),
    ])
}

#[test_case(false, false, true; "command_wakes_idle_main")]
#[test_case(false, true, true; "command_wakes_paused_foreground_subagent")]
#[test_case(false, false, false; "yielded_command_wakes_idle_main")]
#[test_case(false, true, false; "yielded_command_wakes_paused_foreground_subagent")]
#[test_case(true, false, true; "monitor_wakes_idle_main")]
#[test_case(true, true, true; "monitor_wakes_paused_foreground_subagent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn background_events_resume_the_owning_conversation(
    monitor: bool,
    child: bool,
    explicit_background: bool,
) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let script = format!(
        "import socket; s=socket.create_connection((\"127.0.0.1\",{port})); s.sendall(b\"r\"); s.recv(1); print(\"observed-background-event\",flush=True){}",
        if monitor { "; s.recv(1)" } else { "" },
    );
    let python = if cfg!(windows) { "python" } else { "python3" };
    let command = format!("{python} -c '{script}'");
    let mut arguments = if monitor {
        json!({"command":command,"description":"gated test source"})
    } else {
        json!({"cmd":command,"description":"gated test source","yield_time_ms":1})
    };
    if cfg!(windows) {
        arguments["shell"] = json!("powershell.exe");
    }
    if !monitor {
        arguments["run_in_background"] = json!(explicit_background);
    }
    let mut responses = Vec::new();
    if child {
        responses.push(sse(vec![
            ev_function_call_with_namespace(
                "delegate",
                "collaboration",
                "spawn_agent",
                &json!({
                    "task_name":"gated_worker",
                    "message":"Run the assigned background source and wait for its observations.",
                    "fork_turns":"none",
                    "run_in_background":false
                })
                .to_string(),
            ),
            ev_completed("delegate"),
        ]));
    }
    responses.push(call(
        "start-source",
        if monitor { "monitor" } else { "exec_command" },
        arguments,
    ));
    responses.push(answer("paused", "Waiting for the source."));
    responses.push(answer("observed", "Received the background observation."));
    if monitor {
        responses.push(answer("source-finished", "The watch finished."));
    }
    if child {
        responses.push(answer("parent-finished", "The child finished."));
    }
    let expected_requests = responses.len();
    let streams = responses
        .into_iter()
        .map(|body| vec![StreamingSseChunk { gate: None, body }])
        .collect();
    let (server, _) = start_streaming_sse_server(streams).await;
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_model("gpt-5.2")
        .with_model_info_override("gpt-5.2", |model| {
            model.use_responses_lite = false;
        })
        .with_config(move |config| {
            config
                .features
                .enable(Feature::ClaudeBackgroundTasks)
                .unwrap();
            config.features.disable(Feature::CodeMode).unwrap();
            if child {
                config.features.enable(Feature::ClaudeDelegation).unwrap();
            }
        })
        .build_with_streaming_server(&server)
        .await?;
    let submit = test.submit_turn("Run the source and react when it produces an event.");
    tokio::pin!(submit);
    let mut submit_finished = false;
    let (mut source, _) = tokio::select! {
        connection = listener.accept() => connection?,
        result = &mut submit => { result?; submit_finished = true; listener.accept().await? },
    };
    let mut ready = [0];
    source.read_exact(&mut ready).await?;
    let owner = if child {
        let child_id = test
            .thread_manager
            .list_thread_ids()
            .await
            .into_iter()
            .find(|id| *id != test.session_configured.thread_id)
            .unwrap();
        let owner = test.thread_manager.get_thread(child_id).await?;
        wait_for_event(&owner, |event| matches!(event, EventMsg::TurnComplete(_))).await;
        owner
    } else {
        if !submit_finished {
            (&mut submit).await?;
        }
        Arc::clone(&test.codex)
    };
    ThreadIdle::wait(&owner).await;
    if child {
        assert!(matches!(
            test.codex.agent_status().await,
            AgentStatus::Running
        ));
    }
    source.write_all(b"1").await?;
    if monitor {
        wait_for_event(&owner, |event| matches!(event, EventMsg::TurnComplete(_))).await;
        ThreadIdle::wait(&owner).await;
        if child {
            assert!(matches!(
                test.codex.agent_status().await,
                AgentStatus::Running
            ));
        }
        source.write_all(b"2").await?;
    }
    if child {
        assert!(
            !submit_finished,
            "a paused child must not finish its foreground delegation"
        );
        (&mut submit).await?;
    } else {
        wait_for_event(&owner, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    }
    ThreadIdle::wait(&test.codex).await;
    let requests = server
        .requests()
        .await
        .into_iter()
        .map(|body| serde_json::from_slice::<Value>(&body))
        .collect::<serde_json::Result<Vec<_>>>()?;
    assert_eq!(requests.len(), expected_requests);
    let first = &requests[0];
    assert!(first["tools"].as_array().unwrap().iter().all(|tool| {
        !matches!(tool["name"].as_str(), Some("bash" | "agent"))
            && !tool["name"]
                .as_str()
                .unwrap_or_default()
                .starts_with("claude")
    }));
    assert!(first["tools"].as_array().unwrap().iter().any(|tool| {
        if child {
            tool["name"] == "collaboration"
                && tool["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|nested| nested["name"] == "spawn_agent")
        } else {
            tool["name"] == if monitor { "monitor" } else { "exec_command" }
        }
    }));
    assert!(
        requests
            .iter()
            .any(|request| request["input"].to_string().contains("<background_event>"))
    );
    if monitor {
        assert!(requests.iter().any(|request| {
            request["input"]
                .to_string()
                .contains("observed-background-event")
        }));
    }
    test.thread_manager
        .shutdown_all_threads_bounded(std::time::Duration::from_secs(10))
        .await;
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreground_command_retains_output_after_process_exit() -> Result<()> {
    let python = if cfg!(windows) { "python" } else { "python3" };
    let script = "import sys; sys.stdout.write(\"x\"*2097152); sys.stdout.flush(); sys.stderr.write(\"stderr-tail\"); sys.stderr.flush()";
    let mut arguments = json!({
        "cmd":format!("{python} -c '{script}'"),
        "description":"large trailing output",
        "run_in_background":false,
        "yield_time_ms":30000
    });
    if cfg!(windows) {
        arguments["shell"] = json!("powershell.exe");
    }
    let streams = vec![
        call("large-output", "exec_command", arguments),
        answer("done", "The command completed."),
    ]
    .into_iter()
    .map(|body| vec![StreamingSseChunk { gate: None, body }])
    .collect();
    let (server, _) = start_streaming_sse_server(streams).await;
    let test = test_codex()
        .with_model("gpt-5.2")
        .with_model_info_override("gpt-5.2", |model| {
            model.use_responses_lite = false;
        })
        .with_config(|config| {
            config
                .features
                .enable(Feature::ClaudeBackgroundTasks)
                .unwrap();
            config.features.disable(Feature::CodeMode).unwrap();
        })
        .build_with_streaming_server(&server)
        .await?;
    test.submit_turn("Run the command and collect its output.")
        .await?;
    let requests = server.requests().await;
    let completion: Value = serde_json::from_slice(&requests[1])?;
    let output = completion["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == "large-output")
        .unwrap();
    let result: Value = serde_json::from_str(output["output"].as_str().unwrap())?;
    assert_eq!(result["status"], "completed");
    let retained = tokio::fs::read(result["output_file"].as_str().unwrap()).await?;
    assert_eq!(
        retained.iter().filter(|byte| **byte == b'x').count(),
        2_097_152
    );
    assert!(String::from_utf8_lossy(&retained).contains("stderr-tail"));
    test.thread_manager
        .shutdown_all_threads_bounded(std::time::Duration::from_secs(10))
        .await;
    server.shutdown().await;
    Ok(())
}
