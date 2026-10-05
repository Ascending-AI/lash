use super::*;
use lash::rlm::RlmSendBuilderExt;

#[test]
fn committed_transcript_and_provider_history_survive_web_process_reconstruction() {
    run_async_test_on_stack_budget("workbench-session-resume-test", || {
        committed_transcript_and_provider_history_survive_web_process_reconstruction_inner()
    });
}

async fn committed_transcript_and_provider_history_survive_web_process_reconstruction_inner() {
    let data_dir = std::env::temp_dir().join(format!(
        "agent-workbench-session-resume-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&data_dir).expect("create session resume data dir");
    let session_id_path = data_dir.join("session-id");
    let first_session_ids = WorkbenchSessions::persistent(session_id_path.clone())
        .expect("create persistent session id");
    let session_id = first_session_ids.current();
    // One double outlives both web processes: its stores are the durable
    // state the second process reconstructs from.
    let double = crate::tests::test_double_backend(0).await;
    let first_registry = double.engine_stores().process_registry();
    let first_response = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let first_response_for_provider = Arc::clone(&first_response);
    let first_provider = lash::testing::TestProvider::builder()
        .kind("workbench-session-resume-first")
        .complete(move |_| {
            let first_response = Arc::clone(&first_response_for_provider);
            async move {
                let index = first_response.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(match index {
                    0 => {
                        text_response("<typescript>\nfinish(\"resume answer one\");\n</typescript>")
                    }
                    1 => {
                        text_response("<typescript>\nfinish(\"resume answer two\");\n</typescript>")
                    }
                    other => panic!("unexpected first-process provider call {other}"),
                })
            }
        })
        .build()
        .into_handle();
    let model = lash::LlmProfileMetadata::builder("test-model")
        .context_window_tokens(4096)
        .build()
        .expect("model spec");
    let first_core = explicit_durable_test_facets_on(double.lash_backend())
        .serve_workbench_llm_profile(first_provider, model.clone())
        .build(crate::test_core_owner())
        .expect("build first workbench core");
    let first_session = crate::created_session(&first_core, session_id.clone())
        .await
        .open()
        .await
        .expect("open first-process session");
    for (turn_id, text) in [
        ("resume-turn-one", "resume question one"),
        ("resume-turn-two", "resume question two"),
    ] {
        first_session
            .send(lash::TurnInput::text(text))
            .id(lash::TurnId::parse(turn_id).expect("nonblank host identity"))
            .require_finish()
            .expect("require finish")
            .output()
            .await
            .expect("commit pre-restart turn");
    }
    let committed = first_session.read_view();
    let committed_sequence: lash::messages::MessageSequence = committed.messages().to_vec().into();
    let committed_sequence: lash::messages::MessageSequence = serde_json::from_value(
        serde_json::to_value(&committed_sequence).expect("serialize committed message sequence"),
    )
    .expect("deserialize committed message sequence");
    assert_eq!(
        committed_sequence.len(),
        4,
        "each turn commits its one reply and nothing else assistant-side"
    );
    // The runtime commits each value-finished turn's reply itself, marked
    // with its turn (FIG-1493 §5.5); no host writer is involved.
    assert_eq!(
        committed_sequence
            .iter()
            .filter_map(|message| message.reply_marker.as_ref())
            .map(|reply| reply.turn_id().to_string())
            .collect::<Vec<_>>(),
        vec!["resume-turn-one", "resume-turn-two"]
    );
    assert_eq!(
        committed_sequence
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>(),
        vec![
            lash::messages::MessageRole::User,
            lash::messages::MessageRole::Assistant,
            lash::messages::MessageRole::User,
            lash::messages::MessageRole::Assistant,
        ]
    );
    let projection: lash::persistence::ChronologicalProjection =
        committed.chronological_projection();
    let entries: &[lash::persistence::ChronologicalEntry] = projection.entries();
    assert_eq!(
        entries.iter().map(|entry| entry.index).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4, 5, 6, 7]
    );
    let projected_kinds = entries
        .iter()
        .map(|entry| match &entry.payload {
            lash::persistence::ChronologicalPayload::Message(message) => match message.role {
                lash::messages::MessageRole::User => "user",
                lash::messages::MessageRole::Assistant => "assistant",
                lash::messages::MessageRole::System => "system",
                lash::messages::MessageRole::Event => "event",
            },
            lash::persistence::ChronologicalPayload::ProtocolEvent(event) => {
                assert_eq!(event.plugin_id, "rlm_protocol");
                let versioned = &event.payload["event"];
                if versioned.get("RlmDiagnostic").is_some() {
                    "rlm_diagnostic"
                } else if versioned.get("RlmTrajectoryEntry").is_some() {
                    "rlm_trajectory"
                } else {
                    panic!("unexpected RLM protocol payload: {:?}", event.payload);
                }
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        projected_kinds,
        vec![
            "user",
            "rlm_diagnostic",
            "rlm_trajectory",
            "assistant",
            "user",
            "rlm_diagnostic",
            "rlm_trajectory",
            "assistant",
        ]
    );
    let projected_messages = entries
        .iter()
        .filter_map(|entry| match &entry.payload {
            lash::persistence::ChronologicalPayload::Message(message) => {
                Some((message.role, lash::message_text(message)))
            }
            lash::persistence::ChronologicalPayload::ProtocolEvent(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        projected_messages,
        vec![
            (
                lash::messages::MessageRole::User,
                "resume question one".to_string()
            ),
            (
                lash::messages::MessageRole::Assistant,
                "resume answer one".to_string()
            ),
            (
                lash::messages::MessageRole::User,
                "resume question two".to_string()
            ),
            (
                lash::messages::MessageRole::Assistant,
                "resume answer two".to_string()
            ),
        ]
    );
    assert_eq!(committed.turn_index(), 2);
    first_session.close().await.expect("close first session");
    // The first process's last shift outlives its answer while it closes the
    // run's scope (FIG-3979), and would admit an input sent meanwhile on
    // that process's driver.
    double.settle_session_shift(&session_id).await;
    drop(first_core);
    drop(first_registry);
    drop(first_session_ids);

    let resumed_requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let resumed_requests_for_provider = Arc::clone(&resumed_requests);
    let resumed_provider = lash::testing::TestProvider::builder()
        .kind("workbench-session-resume-first")
        .complete(move |request| {
            let resumed_requests = Arc::clone(&resumed_requests_for_provider);
            async move {
                resumed_requests
                    .lock_recover()
                    .push(serde_json::to_string(&request).expect("serialize resumed request"));
                Ok(text_response(
                    "<typescript>\nfinish(\"resume answer three\");\n</typescript>",
                ))
            }
        })
        .build()
        .into_handle();
    let resumed_store_factory: Arc<dyn lash::persistence::DeploymentStore> =
        double.stores().session_store_factory();
    let resumed_core = explicit_durable_test_facets_on(double.lash_backend())
        .serve_workbench_llm_profile(resumed_provider, model)
        .build(crate::test_core_owner())
        .expect("build reconstructed workbench core");
    let resumed_session_ids =
        WorkbenchSessions::persistent(session_id_path).expect("reopen persistent session id");
    assert_eq!(resumed_session_ids.current(), session_id);
    let process_observer = resumed_core
        .processes()
        .observer()
        .expect("process observer configured");
    let state = AppState {
        session_defaults: crate::tests::test_session_defaults(),
        core: resumed_core,
        attachment_store: test_attachment_store(),
        session_store_factory: Arc::clone(&resumed_store_factory),
        trigger_store: detached_trigger_store(),
        process_observer,
        // Process work is resolved through the core.
        sessions: resumed_session_ids,
        messages: Arc::new(Mutex::new(Vec::new())),
        selected_llm_profile: Arc::new(Mutex::new(LlmProfileSelection {
            model: "test-model".to_string(),
            model_variant: Default::default(),
        })),
        trace_sink: None,
        lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
        event_tx: SessionEventRegistry::new(16),
        restate_ingress_url: "http://127.0.0.1:8080".to_string(),
        restate_admin_url: "http://127.0.0.1:9070".to_string(),
        restate_http: reqwest::Client::new(),
        restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
        mail_world: mail::MailWorld::new(),
        active_turns: ActiveTurns::default(),
        unknown_turn_terminals: UnknownTurnTerminals::default(),
        authorization: WorkbenchAuthorization::allow_all(),
        approvals: approvals::WorkbenchApprovals::in_memory().unwrap(),
    };

    assert!(
        state.messages_snapshot().is_empty(),
        "the reconstructed web process must begin with no local transcript cache"
    );
    let Json(before) = Box::pin(app_state(
        State(state.clone()),
        Query(SessionQuery::default()),
    ))
    .await
    .expect("project committed transcript after restart");
    let before_messages = before
        .transcript
        .iter()
        .filter_map(transcript_message)
        .collect::<Vec<_>>();
    let before_rows = before_messages
        .iter()
        .map(|message| (message.role.as_str(), message.text.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        before_rows,
        vec![
            ("user", "resume question one"),
            ("assistant", "resume answer one"),
            ("user", "resume question two"),
            ("assistant", "resume answer two"),
        ]
    );

    let resumed_session = crate::created_session(&state.core, session_id.clone())
        .await
        .open()
        .await
        .expect("open resumed session");
    resumed_session
        .send(lash::TurnInput::text("resume question three"))
        .id(lash::TurnId::parse("resume-turn-three").expect("nonblank host identity"))
        .require_finish()
        .expect("require resumed finish")
        .output()
        .await
        .expect("commit resumed turn");
    resumed_session
        .close()
        .await
        .expect("close resumed session");

    {
        let requests = resumed_requests.lock_recover();
        assert_eq!(requests.len(), 1);
        for marker in [
            "resume question one",
            "resume answer one",
            "resume question two",
            "resume answer two",
            "resume question three",
        ] {
            assert!(
                requests[0].contains(marker),
                "resumed provider request omitted committed history marker {marker:?}: {}",
                requests[0]
            );
        }
    }

    let Json(after) = Box::pin(app_state(
        State(state.clone()),
        Query(SessionQuery::default()),
    ))
    .await
    .expect("project transcript after resumed turn");
    let after_messages = after
        .transcript
        .iter()
        .filter_map(transcript_message)
        .collect::<Vec<_>>();
    assert_eq!(after_messages.len(), 6);
    assert_eq!(after_messages[4].text, "resume question three");
    assert_eq!(after_messages[5].text, "resume answer three");
    let _ = std::fs::remove_dir_all(data_dir);
}
