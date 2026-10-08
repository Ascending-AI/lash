//! Real provider turns deliver their model-call ledgers, attempt evidence
//! included, on the record surfaces consumers read: the local observation
//! stream and the product snapshot's `model_call_recorded` events. The page
//! no longer renders these records (FIG-5036); E2E, the load-test
//! measurements and the provider tests read them here.

use super::*;
use lash::LlmCallRecord;
use lash::provider::{AttemptOutcome, ProtocolPosition, ProviderFailureKind};

/// The next model-call record the local observation stream delivers.
async fn next_model_call(
    observations: &mut lash::persistence::LiveReplaySubscription,
) -> LlmCallRecord {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let event = observations
                .next()
                .await
                .expect("the local observation stream stays open")
                .expect("a local observation update");
            if let lash::observe::SessionObservationEventPayload::TurnActivity(activity) =
                &event.payload
                && let lash::TurnEvent::ModelCallRecorded { record } = &activity.event
            {
                return record.clone();
            }
        }
    })
    .await
    .expect("the model-call observation arrives")
}

/// The terminal attempt of `record` carries the provider's own evidence.
fn assert_provider_evidence(
    record: &LlmCallRecord,
    response_id: &str,
    served_model: &str,
    finish: &str,
) {
    let evidence = serde_json::to_value(
        &record
            .attempts
            .last()
            .expect("a provider call has a terminal attempt")
            .evidence,
    )
    .expect("encode the evidence");
    assert_eq!(evidence["provider_response_id"], response_id, "{evidence}");
    assert_eq!(evidence["served_model"], served_model, "{evidence}");
    assert_eq!(evidence["provider_finish_reason"], finish, "{evidence}");
    assert_eq!(evidence["reasoning_output_tokens"], 0, "{evidence}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_execution_evidence_reaches_the_record_surfaces() {
    for (provider_kind, response_id, served_model, finish) in [
        (
            lash_sim::runtime_providers::GOOGLE_OAUTH,
            "google-evidence-1",
            "gemini-3.1-pro-served",
            "STOP",
        ),
        (
            lash_sim::runtime_providers::ANTHROPIC,
            "msg_anthropic_evidence_1",
            "claude-sonnet-4-20250514-served",
            "end_turn",
        ),
    ] {
        let answer = finish_cell(&format!("{provider_kind} execution evidence"));
        let script = if provider_kind == lash_sim::runtime_providers::GOOGLE_OAUTH {
            lash_sim::runtime_providers::google_runtime_script_for_text_with_explicit_zero_reasoning(
                &answer,
            )
            .expect("the Google explicit-zero provider fixture")
        } else {
            lash_sim::runtime_providers::runtime_script_for_text(provider_kind, &answer)
                .expect("the provider evidence fixture")
        };
        // The first call fails before any response and is retried.
        let mut failed_before_response = script.clone();
        failed_before_response.name = format!("{provider_kind}.retryable-before-response");
        *failed_before_response.timeline_mut() =
            vec![lash_sim::ProviderWireEvent::TransportError {
                at: 0,
                message: "connection failed before response".to_string(),
                retryable: Some(true),
            }];
        failed_before_response.expected_provider = Some(json!({
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
        let (mut provider, model, _) =
            lash_sim::runtime_providers::runtime_provider_components(provider_kind, &transport)
                .expect("the provider fixture's components");
        let mut options = provider.options();
        options.reliability = options
            .reliability
            .max_attempts(2)
            .base_delay_ms(0)
            .max_delay_ms(0);
        provider.set_options(options);
        let workbench = Workbench::builder(provider).build().await;
        let state = &workbench.state;
        let session_id = state.current_session_id();
        let session = state
            .create_or_open_session(&session_id, "test")
            .await
            .expect("open the provider evidence session");
        // The workbench catalog mints metadata for any id, so a cap the
        // fixture's model records by default (Messages requires one) is
        // stated as the session's generation; the sends select its model.
        if let Some(cap) = model.limits.output_tokens.default_cap() {
            let config = session.admin().config();
            config
                .apply(
                    lash::config::ConfigWrite::new(
                        format!("{provider_kind}-output-cap"),
                        config.revision().await.expect("read the config revision"),
                    ),
                    lash::config::ConfigTransaction::of(lash::config::SetGeneration {
                        generation: lash::GenerationOverlay::Merge(
                            lash::direct::GenerationOptions {
                                output_token_cap: Some(cap),
                                ..Default::default()
                            },
                        ),
                    }),
                )
                .await
                .expect("state the fixture model's output cap");
        }
        let observable = session.observe();
        let initial = observable.snapshot().await.expect("a durable snapshot");
        // Establish the subscription before the route's model-selection
        // command changes the durable head. A recovery stream subscribes
        // only on its first poll, which could observe the config commit
        // before its publication and correctly report a gap (FIG-5423).
        let subscribed = observable
            .subscribe_from_cursor(&initial.cursor)
            .await
            .expect("subscribe through the local observation facade");
        let lash::observe::SessionObservationSubscription::Subscribed(mut observations) =
            subscribed
        else {
            panic!("a live provider observation must not gap");
        };
        drop(session);

        let mut observed = Vec::new();
        for text in ["first evidence turn", "second evidence turn"] {
            let accepted = send_turn(
                State(state.clone()),
                Query(SessionQuery::default()),
                Json(TurnRequest {
                    model: Some(model.wire_model.clone()),
                    ..turn_request(text)
                }),
            )
            .await
            .expect("the send is admitted")
            .0;
            let turn_id = started_turn_id(&accepted);
            observed.push(next_model_call(&mut observations).await);
            wait_for_turn_released(state, &session_id, &turn_id, Duration::from_secs(30)).await;
        }

        let first = &observed[0];
        let [failed, completed] = first.attempts.as_slice() else {
            panic!("the first call retried once: {first:?}");
        };
        assert_eq!(failed.outcome, AttemptOutcome::Failed);
        assert_eq!(failed.protocol_position, ProtocolPosition::NoResponse);
        assert!(failed.evidence.is_none());
        assert_eq!(
            failed
                .error
                .as_ref()
                .expect("the failed attempt keeps its normalized error")
                .class,
            ProviderFailureKind::Transport
        );
        assert!(
            failed
                .retry_decision
                .as_ref()
                .expect("the failed attempt records its retry decision")
                .is_scheduled()
        );
        assert_eq!(completed.ordinal, 2);
        assert_eq!(completed.outcome, AttemptOutcome::Completed);
        for record in &observed {
            assert_provider_evidence(record, response_id, served_model, finish);
        }

        let published = state
            .event_tx
            .snapshot(&session_id)
            .events
            .into_iter()
            .filter_map(|event| match event.item {
                StreamItem::ModelCallRecorded { record } => Some(record),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            published, observed,
            "the product snapshot carries exactly the observed ledgers"
        );
        drop(observations);
        drop(observable);
        workbench.shutdown().await;
    }
}
