use super::*;
use lash::SessionId;
use lash::TurnId;

/// Queued turn work for the workbench tests: one durable process wake from a
/// process named `source_key`, so every call site's row has its own source.
pub(crate) fn queued_work_test_draft(
    session_id: &SessionId,
    source_key: &str,
) -> lash::persistence::QueuedWorkBatchDraft {
    let process_id = || ProcessId::fixture(source_key);
    workbench_process_wake_draft(lash::process::ProcessWakeDelivery {
        version: lash::formats::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: format!("{source_key}-wake-1"),
        target_session_id: session_id.clone(),
        process_id: process_id(),
        sequence: 1,
        event_type: "process.wake".to_string(),
        event_invocation: lash::runtime::RuntimeInvocation {
            attribution: lash::runtime::RuntimeAttribution::for_session(session_id.clone()),
            subject: lash::durability::RuntimeSubject::ProcessEvent {
                process_id: process_id(),
                sequence: 1,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: lash::persistence::QueuedWorkAuthority::default(),
        input: source_key.to_string(),
        created_at_ms: 1,
    })
}

fn workbench_process_wake_draft(
    wake: lash::process::ProcessWakeDelivery,
) -> lash::persistence::QueuedWorkBatchDraft {
    let source_key = lash::process::process_wake_source_key(&wake.process_id, wake.sequence);
    let process_id = wake.process_id.clone();
    let sequence = wake.sequence;
    lash::persistence::QueuedWorkBatchDraft::new(
        wake.target_session_id.clone(),
        lash::persistence::DeliveryPolicy::EarliestSafeBoundary,
        lash::persistence::TurnWorkPayload::process_wake(wake),
    )
    .with_merge_key(lash::persistence::PROCESS_WAKE_MERGE_KEY)
    .with_source_key(source_key)
    .with_process_wake_source(process_id, sequence)
}

#[test]
fn workbench_lists_and_controls_individual_queued_batches() {
    run_async_test_on_stack_budget("workbench-queued-work-controls", || async {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-queued-controls-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create queued-work controls dir");
        let double = crate::tests::test_double_backend(0).await;
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
            double.stores().session_store_factory();
        let state = recoverable_chat_test_state_with_dependencies(
            &double,
            16,
            lash::testing::TestProvider::builder()
                .kind("workbench-queued-controls-test")
                .complete_error("queued-work controls should not call the provider")
                .build()
                .into_handle(),
            detached_trigger_store(),
            Arc::clone(&store_factory),
        )
        .await;
        let session_id = state.current_session_id();
        let session = state
            .core
            .session(session_id.clone())
            .open()
            .await
            .expect("open queued-work controls session");
        // The engine admits none of the batches while the test lists and
        // controls them.
        let _hold = double.hold_session_drive(&session_id).await;
        let cursor = session.observe().current_observation().cursor;
        let store = store_factory
            .create_store(&lash::persistence::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: lash::persistence::SessionRelation::Root,
                policy: session.policy_snapshot(),
            })
            .await
            .expect("open queued-work controls store");
        let first = store
            .enqueue_queued_work(queued_work_test_draft(
                &session_id,
                "workbench-control:first",
            ))
            .await
            .expect("enqueue first controlled batch");
        let second = store
            .enqueue_queued_work(queued_work_test_draft(
                &session_id,
                "workbench-control:second",
            ))
            .await
            .expect("enqueue second controlled batch");

        let Json(listed) = list_queued_work(State(state.clone()), Query(SessionQuery::default()))
            .await
            .expect("list workbench queued work");
        assert_eq!(
            listed
                .iter()
                .map(|batch| batch.batch_id.as_str())
                .collect::<Vec<_>>(),
            vec![first.batch_id.as_str(), second.batch_id.as_str()]
        );

        let Json(cancelled) = cancel_queued_work_batch(
            AxumPath(first.batch_id.to_string()),
            State(state.clone()),
            Query(SessionQuery::default()),
        )
        .await
        .expect("cancel first queued batch");
        assert!(cancelled.accepted);
        assert_eq!(cancelled.batch_id, first.batch_id);
        let remaining = session
            .durable()
            .queued_work()
            .await
            .expect("list after cancel");
        assert_eq!(
            remaining
                .iter()
                .map(|batch| batch.batch_id.as_str())
                .collect::<Vec<_>>(),
            vec![second.batch_id.as_str()]
        );
        let lash::observe::SessionResume::Replayed { events } = session
            .observe()
            .resume_from_cursor(&cursor)
            .expect("resume queue events")
        else {
            panic!("recent workbench cursor must replay queue events");
        };
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            lash::observe::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
                if *kind == lash::observe::SessionQueueEventKind::Cancelled
                    && batch_ids.as_slice() == std::slice::from_ref(&first.batch_id)
        )));

        assert!(ui::INDEX_HTML.contains("id=\"queuedWorkList\""));
        // The engine drives every pending batch; the page only cancels one.
        assert!(!ui::INDEX_HTML.contains("Run only this queued-work batch now"));
        assert!(ui::INDEX_HTML.contains("Cancel this pending queued-work batch"));
        let _ = std::fs::remove_dir_all(data_dir);
    });
}

#[test]
fn workbench_wake_redelivery_absorbs_into_the_live_receiver_row() {
    run_async_test_on_stack_budget("workbench-targeted-wake-drain", || async {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-targeted-wake-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create targeted wake dir");
        let double = crate::tests::test_double_backend(0).await;
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
            double.stores().session_store_factory();
        let state = recoverable_chat_test_state_with_dependencies_and_context(
            &double,
            16,
            lash::testing::TestProvider::builder()
                .kind("workbench-targeted-wake-test")
                .complete(|_| async {
                    Ok(text_response(
                        "<typescript>\nfinish(\"processed wake\");\n</typescript>",
                    ))
                })
                .build()
                .into_handle(),
            detached_trigger_store(),
            Arc::clone(&store_factory),
            // FIG-1313 regression witness: a small model window that the old
            // hardwired projected-request guard wedged. It must still run one
            // row per wake.
            4_096,
        )
        .await;
        let session_id = state.current_session_id();
        let session = state
            .core
            .session(session_id.clone())
            .open()
            .await
            .expect("open targeted wake session");
        let target = store_factory
            .create_store(&lash::persistence::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: lash::persistence::SessionRelation::Root,
                policy: session.policy_snapshot(),
            })
            .await
            .expect("open targeted wake receiver");

        let clock = Arc::new(lash::testing::TestClock::new(1_800_000_000_000));
        let wake_delivery_config = lash::process::WakeDeliveryConfig::new(10_000)
            .expect("valid wake expiry")
            .with_enqueuing_stale_after_ms(25)
            .expect("valid stale claim age");
        assert_eq!(wake_delivery_config.delivery_expiry_ms, 10_000);
        assert_eq!(wake_delivery_config.enqueuing_stale_after_ms, 25);
        let registry = crate::tests::standalone_process_registry(
            &data_dir,
            Arc::clone(&clock) as Arc<dyn lash::runtime::Clock>,
            Some(wake_delivery_config),
        )
        .await;
        let process_id = registry
            .register_process(
                lash::process::ProcessRegistration::new(
                    lash::process::ProcessInput::External {
                        metadata: Value::Null,
                    },
                    lash::process::RecoveryContract::ExternallyOwned,
                    lash::process::ProcessProvenance::host(),
                    lash::process::Lifetime::Detached,
                )
                .with_extra_event_types([lash::process::ProcessEventType {
                    name: "producer.wake".to_string(),
                    payload_schema: lash::triggers::LashSchema::any(),
                    semantics: lash::process::ProcessEventSemanticsSpec {
                        wake: Some(lash::process::ProcessWakeSpec {
                            when: Some(lash::process::ProcessValueSelector::Present(
                                "/wake_input".to_string(),
                            )),
                            input: lash::process::ProcessValueSelector::Pointer(
                                "/wake_input".to_string(),
                            ),
                        }),
                        ..lash::process::ProcessEventSemanticsSpec::default()
                    },
                }])
                .with_wake_session_id(Some(session_id.clone())),
            )
            .await
            .expect("register targeted wake producer")
            .id;
        let earlier_wake = registry
            .append_event(
                &process_id,
                lash::process::ProcessEventAppendRequest::new(
                    "producer.wake",
                    json!({"wake_input": "earlier"}),
                ),
            )
            .await
            .expect("append earlier wake")
            .wake_delivery
            .expect("earlier wake delivery");
        let later_wake = registry
            .append_event(
                &process_id,
                lash::process::ProcessEventAppendRequest::new(
                    "producer.wake",
                    json!({"wake_input": "later"}),
                ),
            )
            .await
            .expect("append later wake")
            .wake_delivery
            .expect("later wake delivery");
        assert!(earlier_wake.sequence < later_wake.sequence);
        let stale_earlier_claim = registry
            .claim_pending_wake_deliveries(1)
            .await
            .expect("claim earlier sender wake")
            .into_iter()
            .next()
            .expect("earlier sender wake is claimable");
        assert_eq!(stale_earlier_claim.wake.sequence, earlier_wake.sequence);
        let earlier = target
            .enqueue_queued_work(workbench_process_wake_draft(earlier_wake.clone()))
            .await
            .expect("enqueue earlier receiver wake");
        let later = target
            .enqueue_queued_work(workbench_process_wake_draft(later_wake))
            .await
            .expect("enqueue later receiver wake");

        clock.advance(24);
        let before_stale_boundary = lash::process::WakeDeliveryDriver::drive_pending_once(
            Arc::clone(&registry),
            Arc::clone(&store_factory),
            Arc::new(lash::runtime::NoSessionWork::new()),
            Arc::clone(&clock) as Arc<dyn lash::runtime::Clock>,
            32,
        )
        .await
        .expect("claimed wake stays unavailable before configured stale age");
        assert_eq!(before_stale_boundary.inspected, 0);

        clock.advance(1);
        let redelivery: lash::process::WakeDeliveryDriveReport =
            lash::process::WakeDeliveryDriver::drive_pending_once(
                Arc::clone(&registry),
                Arc::clone(&store_factory),
                Arc::new(lash::runtime::NoSessionWork::new()),
                Arc::clone(&clock) as Arc<dyn lash::runtime::Clock>,
                32,
            )
            .await
            .expect("redeliver earlier wake through host driver");
        assert_eq!(redelivery.inspected, 1);
        assert_eq!(redelivery.enqueued, 1);
        assert_eq!(redelivery.discarded_expired, 0);
        assert_eq!(redelivery.discarded_target_gone, 0);
        assert_eq!(redelivery.floor_absorbed, 1);
        assert_eq!(redelivery.discarded_sequence_rewound, 0);
        assert_eq!(redelivery.retryable_failures, 0);
        assert_eq!(
            target
                .list_queued_work(&session_id)
                .await
                .expect("list after earlier live-row absorption")
                .iter()
                .map(|batch| batch.batch_id.as_str())
                .collect::<Vec<_>>(),
            vec![earlier.batch_id.as_str(), later.batch_id.as_str()],
            "live-row absorption must keep the earlier wake's receiver row"
        );

        let expiry_clock = Arc::new(lash::testing::TestClock::new(1_900_000_000_000));
        let expiry_config = lash::process::WakeDeliveryConfig::new(50)
            .expect("valid boundary wake expiry")
            .with_enqueuing_stale_after_ms(25)
            .expect("valid boundary stale age");
        let expiry_registry = crate::tests::standalone_process_registry(
            &data_dir,
            Arc::clone(&expiry_clock) as Arc<dyn lash::runtime::Clock>,
            Some(expiry_config),
        )
        .await;
        let workbench_expiring_wake_process_id = expiry_registry
            .register_process(
                lash::process::ProcessRegistration::new(
                    lash::process::ProcessInput::External {
                        metadata: Value::Null,
                    },
                    lash::process::RecoveryContract::ExternallyOwned,
                    lash::process::ProcessProvenance::host(),
                    lash::process::Lifetime::Detached,
                )
                .with_extra_event_types([lash::process::ProcessEventType {
                    name: "producer.wake".to_string(),
                    payload_schema: lash::triggers::LashSchema::any(),
                    semantics: lash::process::ProcessEventSemanticsSpec {
                        wake: Some(lash::process::ProcessWakeSpec {
                            when: Some(lash::process::ProcessValueSelector::Present(
                                "/wake_input".to_string(),
                            )),
                            input: lash::process::ProcessValueSelector::Pointer(
                                "/wake_input".to_string(),
                            ),
                        }),
                        ..lash::process::ProcessEventSemanticsSpec::default()
                    },
                }])
                .with_wake_session_id(Some(SessionId::from("workbench-never-created-target"))),
            )
            .await
            .expect("register expiring wake producer")
            .id;
        expiry_registry
            .append_event(
                &workbench_expiring_wake_process_id,
                lash::process::ProcessEventAppendRequest::new(
                    "producer.wake",
                    json!({"wake_input": "expires"}),
                ),
            )
            .await
            .expect("append expiring wake");
        let first_expiry_attempt = lash::process::WakeDeliveryDriver::drive_pending_once(
            Arc::clone(&expiry_registry),
            Arc::clone(&store_factory),
            Arc::new(lash::runtime::NoSessionWork::new()),
            Arc::clone(&expiry_clock) as Arc<dyn lash::runtime::Clock>,
            32,
        )
        .await
        .expect("defer wake for a target that has never existed");
        assert_eq!(first_expiry_attempt.inspected, 1);
        assert_eq!(first_expiry_attempt.retryable_failures, 1);
        assert_eq!(first_expiry_attempt.discarded_target_gone, 0);
        expiry_clock.advance(49);
        let before_expiry_boundary = lash::process::WakeDeliveryDriver::drive_pending_once(
            Arc::clone(&expiry_registry),
            Arc::clone(&store_factory),
            Arc::new(lash::runtime::NoSessionWork::new()),
            Arc::clone(&expiry_clock) as Arc<dyn lash::runtime::Clock>,
            32,
        )
        .await
        .expect("wake remains deferred before configured expiry");
        assert_eq!(before_expiry_boundary.inspected, 0);
        expiry_clock.advance(1);
        let at_expiry_boundary = lash::process::WakeDeliveryDriver::drive_pending_once(
            expiry_registry,
            Arc::clone(&store_factory),
            Arc::new(lash::runtime::NoSessionWork::new()),
            expiry_clock as Arc<dyn lash::runtime::Clock>,
            32,
        )
        .await
        .expect("discard wake at configured expiry");
        assert_eq!(at_expiry_boundary.inspected, 1);
        assert_eq!(at_expiry_boundary.discarded_expired, 1);
        assert_eq!(at_expiry_boundary.discarded_target_gone, 0);

        let deleted_target_id = "workbench-deleted-wake-target";
        store_factory
            .create_store(&lash::persistence::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: SessionId::from(deleted_target_id.to_string()),
                relation: lash::persistence::SessionRelation::Root,
                policy: session.policy_snapshot(),
            })
            .await
            .expect("create wake target before deletion");
        store_factory
            .delete_session(&SessionId::from(deleted_target_id))
            .await
            .expect("delete wake target before delivery");
        let target_gone_registry = crate::tests::standalone_process_registry(
            &data_dir,
            Arc::clone(&clock) as Arc<dyn lash::runtime::Clock>,
            Some(wake_delivery_config),
        )
        .await;
        let workbench_target_gone_wake_process_id = target_gone_registry
            .register_process(
                lash::process::ProcessRegistration::new(
                    lash::process::ProcessInput::External {
                        metadata: Value::Null,
                    },
                    lash::process::RecoveryContract::ExternallyOwned,
                    lash::process::ProcessProvenance::host(),
                    lash::process::Lifetime::Detached,
                )
                .with_extra_event_types([lash::process::ProcessEventType {
                    name: "producer.wake".to_string(),
                    payload_schema: lash::triggers::LashSchema::any(),
                    semantics: lash::process::ProcessEventSemanticsSpec {
                        wake: Some(lash::process::ProcessWakeSpec {
                            when: Some(lash::process::ProcessValueSelector::Present(
                                "/wake_input".to_string(),
                            )),
                            input: lash::process::ProcessValueSelector::Pointer(
                                "/wake_input".to_string(),
                            ),
                        }),
                        ..lash::process::ProcessEventSemanticsSpec::default()
                    },
                }])
                .with_wake_session_id(Some(SessionId::from(deleted_target_id.to_string()))),
            )
            .await
            .expect("register target-gone wake producer")
            .id;
        target_gone_registry
            .append_event(
                &workbench_target_gone_wake_process_id,
                lash::process::ProcessEventAppendRequest::new(
                    "producer.wake",
                    json!({"wake_input": "target gone"}),
                ),
            )
            .await
            .expect("append target-gone wake");
        let target_gone = lash::process::WakeDeliveryDriver::drive_pending_once(
            target_gone_registry,
            Arc::clone(&store_factory),
            Arc::new(lash::runtime::NoSessionWork::new()),
            Arc::clone(&clock) as Arc<dyn lash::runtime::Clock>,
            32,
        )
        .await
        .expect("discard wake for deleted target");
        assert_eq!(target_gone.inspected, 1);
        assert_eq!(target_gone.discarded_expired, 0);
        assert_eq!(target_gone.discarded_target_gone, 1);

        let _ = std::fs::remove_dir_all(data_dir);
    });
}

/// One background wake turn must leave exactly one agent reply behind — once in
/// the durable transcript and once on screen.
///
/// A wake turn runs without `require_finish`, so a prose-only model reply
/// terminates naturally and the turn finishes with
/// `TurnFinish::AssistantMessage`. That is precisely the outcome the runtime
/// materializes its own terminal assistant message for
/// (`materialize_terminal_output`), so a host that also commits its own copy of
/// the same reply writes the answer into the transcript twice and renders it
/// twice. A foreground send never showed it because `require_finish` forces the
/// reply through `finish`, which the runtime does not materialize (FIG-984).
#[test]
fn wake_turn_leaves_exactly_one_agent_reply_committed_and_rendered() {
    run_async_test_on_stack_budget("workbench-wake-single-agent-reply", || async {
        const WAKE_REPLY: &str = "You pressed the Red button!";
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-wake-single-reply-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create wake single-reply dir");
        let double = crate::tests::test_double_backend(0).await;
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
            double.stores().session_store_factory();
        let state = recoverable_chat_test_state_with_dependencies_and_context(
            &double,
            16,
            lash::testing::TestProvider::builder()
                .kind("workbench-wake-single-reply-test")
                .complete(|_| async { Ok(text_response(WAKE_REPLY)) })
                .build()
                .into_handle(),
            detached_trigger_store(),
            Arc::clone(&store_factory),
            // FIG-1313 regression witness: a small model window that the old
            // hardwired projected-request guard wedged. It must still run one
            // row per wake.
            4_096,
        )
        .await;
        let session_id = state.current_session_id();
        // The workbench follows every root the engine starts on this session,
        // as its boot does: the watch is in place before the wake exists, so
        // a drive the engine starts on its own is followed too.
        crate::restate::watch_session_roots(&state, &session_id).await;
        let registry = state.core.process_registry();
        let process_id = registry
            .register_process(
                lash::process::ProcessRegistration::new(
                    lash::process::ProcessInput::External {
                        metadata: Value::Null,
                    },
                    lash::process::RecoveryContract::ExternallyOwned,
                    lash::process::ProcessProvenance::host(),
                    lash::process::Lifetime::Detached,
                )
                .with_extra_event_types([lash::process::ProcessEventType {
                    name: "producer.wake".to_string(),
                    payload_schema: lash::triggers::LashSchema::any(),
                    semantics: lash::process::ProcessEventSemanticsSpec {
                        wake: Some(lash::process::ProcessWakeSpec {
                            when: Some(lash::process::ProcessValueSelector::Present(
                                "/wake_input".to_string(),
                            )),
                            input: lash::process::ProcessValueSelector::Pointer(
                                "/wake_input".to_string(),
                            ),
                        }),
                        ..lash::process::ProcessEventSemanticsSpec::default()
                    },
                }])
                .with_wake_session_id(Some(session_id.clone())),
            )
            .await
            .expect("register wake single-reply producer")
            .id;
        registry
            .append_event(
                &process_id,
                lash::process::ProcessEventAppendRequest::new(
                    "producer.wake",
                    json!({"wake_input": "the user pressed the Red button"}),
                ),
            )
            .await
            .expect("append wake single-reply event")
            .wake_delivery
            .expect("wake single-reply delivery");

        // Delivering the wake asks the engine to drive it; the engine's own
        // sweep may already have.
        state
            .core
            .processes()
            .drive_wake_deliveries()
            .await
            .expect("deliver the wake to its session");
        await_rendered_assistant_text(&state, WAKE_REPLY).await;

        // A lease-free read: the engine's drive may still hold the session.
        let committed = state
            .core
            .session(session_id.clone())
            .durable()
            .await
            .expect("bind the durable session")
            .read()
            .await
            .expect("read the committed session")
            .expect("the session has committed state");
        let committed_agent_replies = committed
            .messages()
            .iter()
            .filter(|message| {
                lash::message_role(message) == "assistant"
                    && lash::message_text(message).contains(WAKE_REPLY)
            })
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            committed_agent_replies.len(),
            1,
            "a completed wake turn must commit the agent reply exactly once, \
            got {committed_agent_replies:?}"
        );

        let Json(snapshot) = app_state(State(state.clone()), Query(SessionQuery::default()))
            .await
            .expect("read wake single-reply snapshot");
        let rendered_agent_rows = snapshot
            .transcript
            .iter()
            .filter_map(|row| match row {
                TranscriptRow::Message { message }
                    if message.role == "assistant" && message.text.contains(WAKE_REPLY) =>
                {
                    Some(message.id.clone())
                }
                TranscriptRow::Message { .. }
                | TranscriptRow::Reasoning { .. }
                | TranscriptRow::CodeBlock { .. }
                | TranscriptRow::Note { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rendered_agent_rows.len(),
            1,
            "the settled snapshot must render the agent reply exactly once, \
             got {rendered_agent_rows:?}"
        );
        assert_eq!(
            rendered_agent_rows, committed_agent_replies,
            "the rendered agent row must be the committed transcript copy"
        );
        let _ = std::fs::remove_dir_all(data_dir);
    });
}

/// A wake turn must not retract the answer the turn before it left on screen.
///
/// A cause-only turn commits no turn input: its cause lands as an `Event`
/// message and the reply that follows belongs to the wake, not to the send
/// before it. A projection that settles a turn's protocol-authored reply only
/// at the next *turn input* folds the two turns together and drops the first
/// answer the moment the wake commits its own — an answer disappearing after
/// the user read it (FIG-1406).
#[test]
fn a_wake_turn_leaves_the_previous_reasoned_reply_rendered() {
    run_async_test_on_stack_budget("workbench-wake-keeps-previous-reply", || async {
        const REASONED_REPLY: &str = "FIG-1406 reasoned send answer";
        const WAKE_REPLY: &str = "FIG-1406 wake answer";
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-wake-keeps-previous-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create wake keeps-previous dir");
        let double = crate::tests::test_double_backend(0).await;
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
            double.stores().session_store_factory();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state = recoverable_chat_test_state_with_dependencies_and_context(
            &double,
            16,
            lash::testing::TestProvider::builder()
                .kind("workbench-wake-keeps-previous-test")
                .complete(move |_| {
                    let calls = Arc::clone(&calls);
                    async move {
                        let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if call == 0 {
                            // The send answers with reasoning attached, so the
                            // RLM protocol owns the committed copy.
                            let mut response = text_response(REASONED_REPLY);
                            response.parts.insert(
                                0,
                                lash::direct::LlmOutputPart::Reasoning {
                                    text: "FIG-1406 send reasoning".to_string(),
                                    replay: None,
                                },
                            );
                            Ok(response)
                        } else {
                            Ok(text_response(WAKE_REPLY))
                        }
                    }
                })
                .build()
                .into_handle(),
            detached_trigger_store(),
            Arc::clone(&store_factory),
            // FIG-1313 regression witness: a small model window that the old
            // hardwired projected-request guard wedged. It must still run one
            // row per wake.
            4_096,
        )
        .await;
        let session_id = state.current_session_id();
        let session = state
            .core
            .session(session_id.clone())
            .open()
            .await
            .expect("open wake keeps-previous session");

        let send_turn_id = "workbench-turn-reasoned-send";
        state.track_turn(&session_id, &TurnId::from(send_turn_id));
        let send_turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
        let send_output = session
            .send(lash::TurnInput::text("answer with reasoning"))
            .id(send_turn_id)
            .output_into(&ChannelTurnEvents {
                turn_state: Arc::clone(&send_turn_state),
            })
            .await
            .expect("run reasoned send turn");
        crate::restate::record_turn_output(
            &state,
            &session,
            &TurnId::from(send_turn_id),
            send_output,
            send_turn_state,
            "test.wake_keeps_previous.send",
        )
        .await
        .expect("record reasoned send output");
        let Json(live_send_snapshot) =
            app_state(State(state.clone()), Query(SessionQuery::default()))
                .await
                .expect("read live reasoned send snapshot");
        let live_send_agent_rows = live_send_snapshot
            .state
            .messages
            .iter()
            .filter(|message| message.role == "assistant" && message.text == REASONED_REPLY)
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            live_send_agent_rows.len(),
            1,
            "the live snapshot must keep the workbench-owned answer visible while the \
             RLM-owned durable reply is withheld, got {live_send_agent_rows:?}"
        );
        crate::restate::settle_workbench_turn(&state, &session_id, &TurnId::from(send_turn_id))
            .await
            .expect("settle reasoned send turn");

        let registry = state.core.process_registry();
        let process_id = registry
            .register_process(
                lash::process::ProcessRegistration::new(
                    lash::process::ProcessInput::External {
                        metadata: Value::Null,
                    },
                    lash::process::RecoveryContract::ExternallyOwned,
                    lash::process::ProcessProvenance::host(),
                    lash::process::Lifetime::Detached,
                )
                .with_extra_event_types([lash::process::ProcessEventType {
                    name: "producer.wake".to_string(),
                    payload_schema: lash::triggers::LashSchema::any(),
                    semantics: lash::process::ProcessEventSemanticsSpec {
                        wake: Some(lash::process::ProcessWakeSpec {
                            when: Some(lash::process::ProcessValueSelector::Present(
                                "/wake_input".to_string(),
                            )),
                            input: lash::process::ProcessValueSelector::Pointer(
                                "/wake_input".to_string(),
                            ),
                        }),
                        ..lash::process::ProcessEventSemanticsSpec::default()
                    },
                }])
                .with_wake_session_id(Some(session_id.clone())),
            )
            .await
            .expect("register wake keeps-previous producer")
            .id;
        registry
            .append_event(
                &process_id,
                lash::process::ProcessEventAppendRequest::new(
                    "producer.wake",
                    json!({"wake_input": "the producer woke this session"}),
                ),
            )
            .await
            .expect("append wake keeps-previous event")
            .wake_delivery
            .expect("wake keeps-previous delivery");

        crate::restate::watch_session_roots(&state, &session_id).await;
        state
            .core
            .processes()
            .drive_wake_deliveries()
            .await
            .expect("deliver the wake to its session");
        await_rendered_assistant_text(&state, WAKE_REPLY).await;
        // A lease-free read: the engine's drive may still hold the session.
        let committed = state
            .core
            .session(session_id.clone())
            .durable()
            .await
            .expect("bind the durable session")
            .read()
            .await
            .expect("read the committed session")
            .expect("the session has committed state");
        assert!(
            committed
                .messages()
                .iter()
                .any(|message| lash::message_role(message) == "event"),
            "the wake turn must commit its cause as an event message, which is \
             the only boundary this projection can read"
        );
        session
            .close()
            .await
            .expect("close wake keeps-previous session");

        let Json(snapshot) = app_state(State(state.clone()), Query(SessionQuery::default()))
            .await
            .expect("read wake keeps-previous snapshot");
        let agent_rows = snapshot
            .state
            .messages
            .iter()
            .filter(|message| message.role == "assistant")
            .map(|message| message.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            agent_rows,
            vec![REASONED_REPLY.to_string(), WAKE_REPLY.to_string()],
            "the send's reasoned answer must survive the wake that followed it"
        );
        let _ = std::fs::remove_dir_all(data_dir);
    });
}

/// Wait until the workbench's follower has rendered an assistant row with
/// `text`: the engine ran the root, and the follower recorded and settled it.
async fn await_rendered_assistant_text(state: &AppState, text: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let Json(snapshot) = app_state(State(state.clone()), Query(SessionQuery::default()))
                .await
                .expect("read the workbench snapshot");
            if snapshot
                .state
                .messages
                .iter()
                .any(|message| message.role == "assistant" && message.text.contains(text))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the followed root's reply is rendered");
}
