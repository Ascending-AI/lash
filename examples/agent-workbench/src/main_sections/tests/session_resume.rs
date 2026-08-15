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
        let first_session_ids = WorkbenchSessionIds::persistent(session_id_path.clone())
            .expect("create persistent session id");
        let session_id = first_session_ids.current();
        let first_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(&data_dir.join("processes.db"), data_dir.join("lash-sessions"))
                .await
                .expect("open first process registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let first_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        );
        let first_response = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_response_for_provider = Arc::clone(&first_response);
        let first_provider = lash::testing::TestProvider::builder()
            .kind("workbench-session-resume-first")
            .complete(move |_| {
                let first_response = Arc::clone(&first_response_for_provider);
                async move {
                    let index = first_response.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(match index {
                        0 => text_response(
                            "<lashlang>\nfinish \"resume answer one\"\n</lashlang>",
                        ),
                        1 => text_response(
                            "<lashlang>\nfinish \"resume answer two\"\n</lashlang>",
                        ),
                        other => panic!("unexpected first-process provider call {other}"),
                    })
                }
            })
            .build()
            .into_handle();
        let model = lash::ModelSpec::builder("test-model")
            .context_window_tokens(4096)
            .build()
        .expect("model spec");
        let first_core = explicit_durable_test_facets(&data_dir)
            .provider(first_provider)
            .model(model.clone())
            .store_factory(Arc::clone(&first_store_factory))
            .process_registry(Arc::clone(&first_registry))
            .disable_queued_work_driver()
            .build(crate::test_core_owner())
            .expect("build first workbench core");
        let first_session = first_core
            .session(session_id.clone())
            .open()
            .await
            .expect("open first-process session");
        for (turn_id, text) in [
            ("resume-turn-one", "resume question one"),
            ("resume-turn-two", "resume question two"),
        ] {
            let output = first_session
                .turn(lash::TurnInput::text(text))
                .turn_id(turn_id)
                .require_finish()
                .expect("require finish")
                .run()
                .await
                .expect("commit pre-restart turn");
            crate::commit_assistant_transcript(
                &first_session,
                turn_id,
                output
                    .final_value()
                    .and_then(serde_json::Value::as_str)
                    .expect("string terminal value")
                    .to_string(),
                None,
            )
            .await
            .expect("commit assistant transcript");
        }
        crate::commit_assistant_transcript(
            &first_session,
            "resume-turn-one",
            "resume answer one".to_string(),
            None,
        )
        .await
        .expect("replay first assistant transcript after a later turn");
        let committed = first_session.read_view();
        let committed_sequence: lash::messages::MessageSequence =
            committed.messages().to_vec().into();
        let committed_sequence: lash::messages::MessageSequence = serde_json::from_value(
            serde_json::to_value(&committed_sequence).expect("serialize committed message sequence"),
        )
        .expect("deserialize committed message sequence");
        assert_eq!(
            committed_sequence.len(),
            4,
            "turn replay must not append a duplicate assistant message"
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
                    if event.payload.get("RlmDiagnostic").is_some() {
                        "rlm_diagnostic"
                    } else if event.payload.get("RlmTrajectoryEntry").is_some() {
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
                (lash::messages::MessageRole::User, "resume question one".to_string()),
                (lash::messages::MessageRole::Assistant, "resume answer one".to_string()),
                (lash::messages::MessageRole::User, "resume question two".to_string()),
                (lash::messages::MessageRole::Assistant, "resume answer two".to_string()),
            ]
        );
        assert_eq!(committed.turn_index(), 2);
        first_session.close().await.expect("close first session");
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
                        "<lashlang>\nfinish \"resume answer three\"\n</lashlang>",
                    ))
                }
            })
            .build()
            .into_handle();
        let resumed_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(&data_dir.join("processes.db"), data_dir.join("lash-sessions"))
                .await
                .expect("reopen process registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let resumed_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        );
        let resumed_core = explicit_durable_test_facets(&data_dir)
            .provider(resumed_provider)
            .model(model)
            .store_factory(Arc::clone(&resumed_store_factory))
            .process_registry(Arc::clone(&resumed_registry))
            .disable_queued_work_driver()
            .build(crate::test_core_owner())
            .expect("build reconstructed workbench core");
        let resumed_session_ids = WorkbenchSessionIds::persistent(session_id_path)
            .expect("reopen persistent session id");
        assert_eq!(resumed_session_ids.current(), session_id);
        let process_observer = resumed_core
            .processes()
            .observer()
            .expect("process observer configured");
        let state = AppState {
            core: resumed_core,
            rlm_dialect: lash::rlm::RlmDialect::Lashlang,
            attachment_store: test_attachment_store(),
            trigger_store: in_memory_trigger_store(),
            process_observer,
            process_work_driver: inert_process_work_driver(Arc::clone(&resumed_registry)),
            session_ids: resumed_session_ids,
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "test-model".to_string(),
                model_variant: Default::default(),
            })),
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx: SessionEventRegistry::new(16),
            queued_work_driver: inert_queued_work_driver(),
            restate_ingress_url: "http://127.0.0.1:8080".to_string(),
            restate_admin_url: "http://127.0.0.1:9070".to_string(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::default(),
            authorization: WorkbenchAuthorization::allow_all(),
        };

        assert!(
            state.messages_snapshot().is_empty(),
            "the reconstructed web process must begin with no local transcript cache"
        );
        let Json(before) =
            Box::pin(app_state(State(state.clone()), Query(SessionQuery::default())))
                .await
                .expect("project committed transcript after restart");
        let before_rows = before
            .messages
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

        let resumed_session = state
            .core
            .session(session_id.clone())
            .open()
            .await
            .expect("open resumed session");
        let resumed_output = resumed_session
            .turn(lash::TurnInput::text("resume question three"))
            .turn_id("resume-turn-three")
            .require_finish()
            .expect("require resumed finish")
            .run()
            .await
            .expect("commit resumed turn");
        crate::commit_assistant_transcript(
            &resumed_session,
            "resume-turn-three",
            resumed_output
                .final_value()
                .and_then(serde_json::Value::as_str)
                .expect("string resumed terminal value")
                .to_string(),
            None,
        )
        .await
        .expect("commit resumed assistant transcript");
        resumed_session.close().await.expect("close resumed session");

        {
            let requests = resumed_requests
                .lock_recover();
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

        let Json(after) =
            Box::pin(app_state(State(state.clone()), Query(SessionQuery::default())))
                .await
                .expect("project transcript after resumed turn");
        assert_eq!(after.messages.len(), 6);
        assert_eq!(after.messages[4].text, "resume question three");
        assert_eq!(after.messages[5].text, "resume answer three");
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn committed_chat_projection_keeps_provider_reasoning_hidden() {
        let message = lash::messages::Message {
            id: "assistant-with-replay".to_string(),
            role: lash::messages::MessageRole::Assistant,
            parts: Arc::new(vec![
                lash_core::Part::text(
                    "assistant-with-replay.p0".to_string(),
                    "visible answer".to_string(),
                    None,
                ),
                lash_core::Part::reasoning(
                    "assistant-with-replay.p1".to_string(),
                    "hidden portable reasoning".to_string(),
                    Some(lash_core::llm::types::ProviderReasoningReplay {
                        signature: Some("opaque".to_string()),
                        ..Default::default()
                    }),
                ),
            ]),
            origin: None,
        };

        assert_eq!(committed_chat_text(&message), "visible answer");
        assert_eq!(
            chat_message_from_committed(&message).text,
            "visible answer"
        );
    }
