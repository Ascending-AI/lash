async fn run_provider_evidence_turn(
    state: &AppState,
    session: &lash::LashSession,
    turn_id: &str,
) -> (lash::TurnReport, Arc<Mutex<TurnStreamState>>) {
    state.track_turn(&session.session_id(), turn_id);
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let output = session
        .turn(lash::TurnInput::text("answer directly"))
        .turn_id(turn_id)
        .require_finish()
        .expect("require provider fixture finish")
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&turn_state),
        })
        .await
        .expect("provider fixture completes through a real Lash turn");
    (output, turn_state)
}

async fn next_remote_model_call(
    recovery: &mut lash::observe::RemoteSessionObservationStream,
) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let update = recovery
                .next()
                .await
                .expect("remote observation stream stays open")
                .expect("remote observation update");
            match update {
                lash::observe::RemoteSessionObservationStreamItem::Event(remote)
                    if matches!(
                        &remote.event,
                        lash::remote::observations::RemoteSessionObservationEventPayload::TurnActivity {
                            activity,
                        } if matches!(
                            &activity.event,
                            lash::remote::usage::RemoteTurnEvent::ModelCallRecorded { .. }
                        )
                    ) =>
                {
                    return serde_json::json!({
                        "type": "observation",
                        "event": remote,
                    });
                }
                lash::observe::RemoteSessionObservationStreamItem::Event(_) => {}
                lash::observe::RemoteSessionObservationStreamItem::Gap { .. } => {
                    panic!("live provider observation must not gap")
                }
            }
        }
    })
    .await
    .expect("provider model-call observation timeout")
}

async fn next_terminal_replacement(
    recovery: &mut lash::recoverable_chat::RecoverableChatSubscription,
    sequence: u64,
) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match recovery
                .next()
                .await
                .expect("recoverable chat stream stays open")
                .expect("recoverable chat update")
            {
                lash::recoverable_chat::RecoverableChatUpdate::TerminalReplacement {
                    event,
                    snapshot,
                    ..
                } => {
                    let remote = lash::remote::observations::RemoteSessionObservationEvent::from_core(
                        sequence,
                        Arc::clone(&event),
                    )
                    .expect("remote event");
                    return serde_json::json!({
                        "type": "terminal_replacement",
                        "event": remote,
                        "cursor": snapshot.cursor.to_string(),
                    });
                }
                lash::recoverable_chat::RecoverableChatUpdate::Event { .. } => {}
                lash::recoverable_chat::RecoverableChatUpdate::ResidentReplacement { .. } => {}
                lash::recoverable_chat::RecoverableChatUpdate::ReplayGap { .. } => {
                    panic!("live recoverable chat must not gap")
                }
            }
        }
    })
    .await
    .expect("provider terminal replacement timeout")
}

async fn provider_state_snapshot(state: &AppState, session_id: &str) -> serde_json::Value {
    let Json(snapshot) = app_state(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(session_id.to_string()),
        }),
    )
    .await
    .expect("production state facade projects provider evidence");
    serde_json::to_value(snapshot).expect("serialize production state snapshot")
}

fn assert_delivered_provider_evidence(
    observation_line: &serde_json::Value,
    runtime_record: &lash::LlmCallRecord,
    response_id: &str,
    served_model: &str,
    finish: &str,
    reasoning_tokens: u64,
) {
    let remote_record = observation_line
        .pointer("/event/activity/record")
        .expect("remote observation carries the model-call record");
    assert_eq!(remote_record["call_id"], runtime_record.call_id.0);
    let remote_attempts = remote_record["attempts"]
        .as_array()
        .expect("remote observation carries attempt rows");
    assert_eq!(remote_attempts.len(), runtime_record.attempts.len());
    let evidence = &remote_attempts
        .last()
        .expect("provider call has a terminal attempt")["evidence"];
    assert_eq!(evidence["provider_response_id"], response_id);
    assert_eq!(evidence["served_model"], served_model);
    assert_eq!(evidence["provider_finish_reason"], finish);
    assert_eq!(evidence["reasoning_output_tokens"], reasoning_tokens);
}

async fn provider_execution_evidence_scenarios() -> serde_json::Value {
    let mut scenarios = Vec::new();
    for (provider_kind, response_id, served_model, finish, reasoning_tokens) in [
        (
            lash_sim::runtime_providers::GOOGLE_OAUTH,
            "google-evidence-1",
            "gemini-3.1-pro-served",
            "STOP",
            0,
        ),
        (
            lash_sim::runtime_providers::ANTHROPIC,
            "msg_anthropic_evidence_1",
            "claude-sonnet-4-20250514-served",
            "end_turn",
            0,
        ),
    ] {
        let answer = format!(
            "<lashlang>\nfinish \"{provider_kind} execution evidence\"\n</lashlang>"
        );
        let script = if provider_kind == lash_sim::runtime_providers::GOOGLE_OAUTH {
            lash_sim::runtime_providers::google_runtime_script_for_text_with_explicit_zero_reasoning(
                &answer,
            )
            .expect("Google explicit-zero provider fixture")
        } else {
            lash_sim::runtime_providers::runtime_script_for_text(provider_kind, &answer)
                .expect("provider evidence fixture")
        };
        let mut failed_before_response = script.clone();
        failed_before_response.name = format!("{provider_kind}.retryable-before-response");
        *failed_before_response.timeline_mut() = vec![lash_sim::ProviderWireEvent::TransportError {
            at: 0,
            message: "connection failed before response".to_string(),
            retryable: Some(true),
        }];
        failed_before_response.expected_provider = Some(serde_json::json!({
            "failure": "transport",
            "response_started": false,
            "retryable": true,
        }));
        let transport = Arc::new(
            lash_sim::ScriptedLlmHttpTransport::from_scripts([
                failed_before_response,
                script.clone(),
                script,
            ])
            .expect("valid provider scripts"),
        );
        let (mut provider, model, _) = lash_sim::runtime_providers::runtime_provider_components(
            provider_kind,
            &transport,
        )
        .expect("provider fixture components");
        let mut provider_options = provider.options();
        provider_options.reliability = provider_options
            .reliability
            .max_attempts(2)
            .base_delay_ms(0)
            .max_delay_ms(0);
        provider.set_options(provider_options);
        let data_dir = tempfile::tempdir().expect("provider evidence workbench tempdir");
        let state = recoverable_chat_test_state_with_provider(data_dir.path(), 64, provider).await;
        let session_id = state.current_session_id();
        let session = state
            .core
            .session(session_id.clone())
            .open()
            .await
            .expect("open provider evidence session");
        session
            .configure(lash::SessionConfigPatch {
                model: Some(model),
                ..Default::default()
            })
            .await
            .expect("configure provider-specific model");

        let observable = session.observe();
        let initial = observable.recoverable_chat_snapshot();
        let remote_cursor = lash::remote::observations::RemoteSessionCursor::new(
            initial.cursor.to_string(),
        );
        let mut observation_recovery = observable
            .subscribe_and_recover_remote(remote_cursor)
            .expect("subscribe through the remote observation facade");
        let mut chat_recovery = observable.subscribe_recoverable_chat(initial.cursor);

        let first_turn_id = format!("{provider_kind}-evidence-turn-1");
        let (first_observation_line, first_terminal_replacement_line, first_execution) = tokio::join!(
            next_remote_model_call(&mut observation_recovery),
            next_terminal_replacement(&mut chat_recovery, 0),
            run_provider_evidence_turn(&state, &session, &first_turn_id),
        );
        let (first_output, first_turn_state) = first_execution;
        let first_record = first_output
            .llm_calls
            .first()
            .expect("first runtime turn seals one model-call ledger")
            .clone();
        assert_eq!(first_record.attempts.len(), 2);
        let failed_attempt = &first_record.attempts[0];
        assert_eq!(failed_attempt.outcome, lash::provider::AttemptOutcome::Failed);
        assert_eq!(
            failed_attempt.protocol_position,
            lash::provider::ProtocolPosition::NoResponse
        );
        assert!(failed_attempt.evidence.is_none());
        let failed_error = failed_attempt
            .error
            .as_ref()
            .expect("failed retry attempt keeps its normalized error");
        assert_eq!(failed_error.class, "transport");
        let retry = failed_attempt
            .retry_decision
            .as_ref()
            .expect("failed first attempt records its retry decision");
        assert!(retry.scheduled);
        assert!(
            retry
                .delay
                .is_some_and(|delay| (Duration::ZERO..=Duration::from_millis(500))
                    .contains(&delay)),
            "retry delay must stay within the bounded jitter envelope, got {:?}",
            retry.delay
        );
        assert_eq!(first_record.attempts[1].ordinal, 2);
        assert_eq!(
            first_record.attempts[1].outcome,
            lash::provider::AttemptOutcome::Completed
        );
        assert_delivered_provider_evidence(
            &first_observation_line,
            &first_record,
            response_id,
            served_model,
            finish,
            reasoning_tokens,
        );
        crate::restate::record_turn_output(
            &state,
            &session,
            &first_turn_id,
            first_output,
            first_turn_state,
            "test.provider_execution_evidence.first.completed",
        )
        .await
        .expect("workbench publishes the first runtime turn output");
        state.active_turns.remove(&session_id, &first_turn_id);
        let first_snapshot = provider_state_snapshot(&state, &session_id).await;

        let second_turn_id = format!("{provider_kind}-evidence-turn-2");
        let (second_observation_line, second_terminal_replacement_line, second_execution) = tokio::join!(
            next_remote_model_call(&mut observation_recovery),
            next_terminal_replacement(&mut chat_recovery, 1),
            run_provider_evidence_turn(&state, &session, &second_turn_id),
        );
        let (second_output, second_turn_state) = second_execution;
        let second_record = second_output
            .llm_calls
            .first()
            .expect("second runtime turn seals one model-call ledger")
            .clone();
        assert_delivered_provider_evidence(
            &second_observation_line,
            &second_record,
            response_id,
            served_model,
            finish,
            reasoning_tokens,
        );
        crate::restate::record_turn_output(
            &state,
            &session,
            &second_turn_id,
            second_output,
            second_turn_state,
            "test.provider_execution_evidence.second.completed",
        )
        .await
        .expect("workbench publishes the second runtime turn output");
        state.active_turns.remove(&session_id, &second_turn_id);
        let final_snapshot = provider_state_snapshot(&state, &session_id).await;

        let product_records = final_snapshot
            .pointer("/product_events/events")
            .and_then(serde_json::Value::as_array)
            .expect("product event snapshot")
            .iter()
            .filter(|event| event["type"] == "model_call_recorded")
            .map(|event| event["record"]["call_id"].as_str().expect("call id"))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            product_records,
            BTreeSet::from([first_record.call_id.0.as_str(), second_record.call_id.0.as_str()]),
            "the product snapshot must contain the exact runtime-published ledgers"
        );

        scenarios.push(serde_json::json!({
            "provider_kind": provider_kind,
            "first_observation_line": first_observation_line,
            "first_terminal_replacement_line": first_terminal_replacement_line,
            "first_snapshot": first_snapshot,
            "second_observation_line": second_observation_line,
            "second_terminal_replacement_line": second_terminal_replacement_line,
            "final_snapshot": final_snapshot,
            "expected": {
                "first_call_id": first_record.call_id.0,
                "second_call_id": second_record.call_id.0,
                "response_id": response_id,
                "served_model": served_model,
                "finish": finish,
                "reasoning_tokens": reasoning_tokens,
                "failed_attempt_error_class": failed_error.class,
            },
        }));
        drop(observation_recovery);
        drop(chat_recovery);
        drop(observable);
        session.close().await.expect("close provider evidence session");
    }
    serde_json::json!({ "providers": scenarios })
}
