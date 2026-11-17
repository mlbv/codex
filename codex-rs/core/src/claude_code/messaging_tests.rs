use super::*;

#[test]
fn delegation_routing_preserves_peer_message_arguments() {
    let send = |enabled| {
        specs(enabled)
            .into_iter()
            .find(|(_, tool)| tool.name == "send_message")
            .unwrap()
            .1
    };
    let peer = send(false);
    let combined = send(true);
    assert_eq!(combined.parameters.required, peer.parameters.required);
    let peer_properties = peer.parameters.properties.as_ref().unwrap();
    let combined_properties = combined.parameters.properties.as_ref().unwrap();
    assert_eq!(combined_properties.len(), peer_properties.len());
    assert_eq!(combined_properties["message"], peer_properties["message"]);
    assert_eq!(
        combined_properties["notify_when_idle"],
        peer_properties["notify_when_idle"]
    );
    assert_eq!(combined_properties["message"].encrypted, None);
    assert!(combined.description.contains("Native teammate names"));
    assert!(!peer.description.contains("Native teammate names"));
}

#[tokio::test]
async fn inbox_accepts_authenticated_fragmented_json_frames() {
    let (mut client, server) = tokio::io::duplex(256);
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let cancellation = CancellationToken::new();
    let reader =
        tokio::spawn(async move { receive(server, "peer-secret", &sender, &cancellation).await });
    client
        .write_all(b"{\"type\":\"auth\",\"token\":\"peer-")
        .await
        .unwrap();
    client.write_all(b"secret\"}\n").await.unwrap();
    let message = json!({
        "msgV": 1,
        "msg_id": "message-1",
        "type": "user",
        "from": "uds:peer-address",
        "message": {"role": "user", "content": "First line\nSecond line"}
    });
    write_line(&mut client, &message).await.unwrap();
    client.shutdown().await.unwrap();
    assert_eq!(receiver.recv().await.unwrap(), message);
    reader.await.unwrap().unwrap();
}

#[tokio::test]
async fn inbox_rejects_wrong_token_before_delivering_a_message() {
    let (mut client, server) = tokio::io::duplex(256);
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let cancellation = CancellationToken::new();
    let reader =
        tokio::spawn(async move { receive(server, "peer-secret", &sender, &cancellation).await });
    write_line(
        &mut client,
        &json!({"type":"auth","token":"different-secret"}),
    )
    .await
    .unwrap();
    assert!(reader.await.unwrap().is_err());
    assert!(receiver.recv().await.is_none());
}

fn bridge_for_tests(directory: &Path) -> Bridge {
    let socket = if cfg!(windows) {
        r"\\.\pipe\LOCAL\cc-msg-0123456789abcdef0123456789abcdef".to_owned()
    } else {
        directory.join("owner.sock").to_string_lossy().into_owned()
    };
    Bridge {
        registry: directory.to_path_buf(),
        record_path: directory.join("session.json"),
        key_path: directory.join("session.key"),
        record: json!({"status":"busy","messagingSocketPath":socket}),
        address: format!("uds:{socket}"),
        token: "test-token".to_string(),
        cancellation: CancellationToken::new(),
        subscribers: Vec::new(),
        outstanding: Vec::new(),
        idle_deadline: None,
        finished_at: None,
        permission_class: Some("prompting".to_string()),
        settings_paths: Vec::new(),
        settings_subscriber: None,
        settings_registration: None,
        explicit_policy: None,
        held: Vec::new(),
        pending_events: Vec::new(),
    }
}

fn unregistered_target(directory: &Path) -> LocalTarget {
    LocalTarget {
        socket: if cfg!(windows) {
            r"\\.\pipe\LOCAL\cc-msg-fedcba9876543210fedcba9876543210".to_owned()
        } else {
            directory.join("peer.sock").to_string_lossy().into_owned()
        },
        peer: None,
    }
}

#[tokio::test]
async fn pure_idle_subscription_stays_outside_model_input() {
    let directory = tempfile::tempdir().unwrap();
    let mut bridge = bridge_for_tests(directory.path());
    let target = unregistered_target(directory.path());
    let frame = json!({
        "type":"control","action":"notify_when_idle",
        "from":target.address(),"msg_id":"idle-request-1"
    });
    assert!(bridge.receive(frame).await.unwrap().is_none());
    assert_eq!(bridge.subscribers.len(), 1);
    assert!(bridge.idle_deadline.is_none());
    let before = Instant::now();
    bridge
        .handle(&BridgeRequest {
            id: 1,
            method: "status".to_string(),
            arguments: json!({"status":"idle"}),
        })
        .await
        .unwrap();
    assert!(bridge.idle_deadline.unwrap() >= before + IDLE_DEBOUNCE);
    bridge
        .handle(&BridgeRequest {
            id: 2,
            method: "status".to_string(),
            arguments: json!({"status":"busy"}),
        })
        .await
        .unwrap();
    assert!(bridge.idle_deadline.is_none());
    assert_eq!(bridge.subscribers.len(), 1);
    assert!(bridge.pending_events.is_empty());
}

#[tokio::test]
async fn correlated_idle_notice_is_delivered_once() {
    let directory = tempfile::tempdir().unwrap();
    let mut bridge = bridge_for_tests(directory.path());
    let target = unregistered_target(directory.path());
    bridge.outstanding.push(IdleSubscription {
        target: target.clone(),
        msg_id: "idle-request-1".to_string(),
        requested_at: Instant::now(),
    });
    let frame = json!({
        "type":"control","action":"peer_idle_notice","state":"idle",
        "orig_msg_id":"idle-request-1","from":target.address(),"from_mode":"prompting"
    });
    let event = bridge.receive(frame.clone()).await.unwrap().unwrap();
    assert_eq!(event["type"], "idle_notice");
    assert!(event["text"].as_str().unwrap().contains("is idle now"));
    assert!(bridge.outstanding.is_empty());
    assert!(bridge.receive(frame).await.unwrap().is_none());
}

#[tokio::test]
async fn changing_inbound_policy_releases_held_idle_notice_once() {
    let directory = tempfile::tempdir().unwrap();
    let mut bridge = bridge_for_tests(directory.path());
    let settings = directory.path().join("settings.json");
    bridge.settings_paths = vec![directory.path().join("managed.json"), settings.clone()];
    std::fs::write(&settings, r#"{"crossSessionInbound":"hold"}"#).unwrap();
    bridge.refresh_inbound_policy().await.unwrap();

    let target = unregistered_target(directory.path());
    bridge.outstanding.push(IdleSubscription {
        target: target.clone(),
        msg_id: "held-idle-request".to_string(),
        requested_at: Instant::now(),
    });
    let frame = json!({
        "type":"control","action":"peer_idle_notice","state":"idle",
        "orig_msg_id":"held-idle-request","from":target.address(),"from_mode":"prompting"
    });
    let event = bridge.receive(frame.clone()).await.unwrap().unwrap();
    assert_eq!(event["type"], "hold_notice");
    assert_eq!(bridge.held.len(), 1);
    assert!(bridge.pending_events.is_empty());
    assert!(bridge.outstanding.is_empty());
    assert!(bridge.receive(frame).await.unwrap().is_none());

    std::fs::write(&settings, r#"{"crossSessionInbound":"accept"}"#).unwrap();
    bridge.refresh_inbound_policy().await.unwrap();
    assert!(bridge.held.is_empty());
    assert_eq!(bridge.pending_events.len(), 1);
    assert_eq!(bridge.pending_events[0]["type"], "idle_notice");
    assert_eq!(bridge.pending_events[0]["text"], event["text"]);
    bridge.refresh_inbound_policy().await.unwrap();
    assert_eq!(bridge.pending_events.len(), 1);
}

#[tokio::test]
async fn changing_inbound_policy_releases_held_messages_once() {
    let directory = tempfile::tempdir().unwrap();
    let mut bridge = bridge_for_tests(directory.path());
    let settings = directory.path().join("settings.json");
    bridge.settings_paths = vec![directory.path().join("managed.json"), settings.clone()];
    std::fs::write(&settings, r#"{"crossSessionInbound":"hold"}"#).unwrap();
    let message = json!({
        "type":"user","msg_id":"held-message-1","from_mode":"prompting",
        "message":{"role":"user","content":"Continue the agreed task"}
    });
    let event = bridge.receive(message.clone()).await.unwrap().unwrap();
    assert_eq!(event["type"], "hold_notice");
    assert_eq!(bridge.held.len(), 1);
    assert!(bridge.pending_events.is_empty());
    std::fs::write(&settings, r#"{"crossSessionInbound":"accept"}"#).unwrap();
    bridge.refresh_inbound_policy().await.unwrap();
    assert!(bridge.held.is_empty());
    assert_eq!(bridge.pending_events, vec![message]);
    bridge.refresh_inbound_policy().await.unwrap();
    assert_eq!(bridge.pending_events.len(), 1);
}

#[tokio::test]
async fn trusted_repository_policy_only_tightens_global_policy() {
    let directory = tempfile::tempdir().unwrap();
    let mut bridge = bridge_for_tests(directory.path());
    let user = directory.path().join("user.json");
    let repository = directory.path().join("project.json");
    bridge.settings_paths = vec![
        directory.path().join("managed.json"),
        user.clone(),
        repository.clone(),
    ];
    std::fs::write(&user, r#"{"crossSessionInbound":"hold"}"#).unwrap();
    std::fs::write(&repository, r#"{"crossSessionInbound":"accept"}"#).unwrap();
    bridge.refresh_inbound_policy().await.unwrap();
    assert!(bridge.explicit_policy == Some(InboundPolicy::Hold));
    std::fs::write(&repository, r#"{"crossSessionInbound":"refuse"}"#).unwrap();
    bridge.refresh_inbound_policy().await.unwrap();
    assert!(bridge.explicit_policy == Some(InboundPolicy::Refuse));
    std::fs::write(&repository, r#"{"crossSessionInbound":"invalid"}"#).unwrap();
    bridge.refresh_inbound_policy().await.unwrap();
    assert!(bridge.explicit_policy == Some(InboundPolicy::Hold));
}

#[test]
fn verified_sender_ancestry_does_not_override_explicit_inbound_policy() {
    let directory = tempfile::tempdir().unwrap();
    let mut bridge = bridge_for_tests(directory.path());
    bridge.permission_class = Some("bypass".to_string());
    assert!(bridge.inbound_policy_for(None, false) == InboundPolicy::Hold);
    assert!(bridge.inbound_policy_for(None, true) == InboundPolicy::Accept);
    bridge.explicit_policy = Some(InboundPolicy::Hold);
    assert!(bridge.inbound_policy_for(None, true) == InboundPolicy::Hold);
    bridge.explicit_policy = Some(InboundPolicy::Refuse);
    assert!(bridge.inbound_policy_for(Some("bypass"), true) == InboundPolicy::Refuse);
}
