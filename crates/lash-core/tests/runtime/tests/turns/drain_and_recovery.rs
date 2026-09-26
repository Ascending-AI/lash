use super::*;

#[tokio::test]
pub(super) async fn renewal_failure_mid_turn_does_not_select_a_durable_branch() {
    Box::pin(renewal_failure_mid_turn(false)).await;
}

/// The drive path's half of the rule (FIG-3600, ADR 0105): the drive epoch is
/// the only fence, so an observed lease loss alone refuses nothing. With no
/// successor drive sealed, the root commits and exactly one durable branch
/// exists. The other half, a successor that sealed a newer epoch refusing the
/// stale root's commit, is S7-A's `RuntimeCommit.drive_fence` law.
#[tokio::test]
pub(super) async fn a_drive_root_commits_after_an_observed_lease_loss_when_no_successor_sealed() {
    Box::pin(renewal_failure_mid_turn(true)).await;
}

async fn renewal_failure_mid_turn(through_drive: bool) {
    let backend = memory_backend().await;
    let lease_ttl = std::time::Duration::from_millis(120);
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let store = unbound_recording_store_with_clock(&backend, store_clock).await;
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    let (provider_stalled_tx, provider_stalled_rx) = tokio::sync::oneshot::channel::<()>();
    let provider_stalled_tx = Arc::new(Mutex::new(Some(provider_stalled_tx)));
    let captured_provider_stalled_tx = Arc::clone(&provider_stalled_tx);
    let (provider_continue_tx, provider_continue_rx) = tokio::sync::oneshot::channel::<()>();
    let provider_continue_rx = Arc::new(Mutex::new(Some(provider_continue_rx)));
    let captured_provider_continue_rx = Arc::clone(&provider_continue_rx);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_request| {
            let captured_calls = Arc::clone(&captured_calls);
            let captured_provider_stalled_tx = Arc::clone(&captured_provider_stalled_tx);
            let captured_provider_continue_rx = Arc::clone(&captured_provider_continue_rx);
            async move {
                if captured_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "reach the active-turn checkpoint".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    });
                }
                if let Some(tx) = captured_provider_stalled_tx.lock_recover().take() {
                    let _ = tx.send(());
                }
                let rx = captured_provider_continue_rx
                    .lock_recover()
                    .take()
                    .expect("provider continue receiver available");
                let _ = rx.await;
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "stale claim completion".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let host_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let mut config = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock)
    .with_lease_timings(
        lash_core::facade_support::LeaseTimings::from_ttl(lease_ttl).expect("valid timings"),
    );
    config.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(transport.clone().into_handle()),
    );
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        lash_core::facade_support::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;

    enqueue_idle_turn_input(
        store.as_ref(),
        &SessionId::from("root"),
        "input held when the lease is lost",
    )
    .await;
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = lash_core::SessionScope::new("root");
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                "lease-loss-claimed-wake",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::session(target_scope.clone()),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
        )
        .await
        .expect("register wake process");
    append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        &ProcessId::from("lease-loss-claimed-wake"),
        lash_core::ProcessEventAppendRequest::new(
            "process.wake",
            json!({
                "text": "queued work held when the lease is lost",
                "value": { "status": "lease lost" }
            }),
        ),
    )
    .await;

    let turn = lash_core::task::spawn(async move {
        let options = TurnOptions::new(
            CancellationToken::new(),
            backend_queued_scope(
                &backend,
                &SessionId::from("root"),
                &TurnId::from("renewal-failure-mid-turn"),
            ),
        );
        let drain = if through_drive {
            runtime.drive_next_queued_root(options).await
        } else {
            runtime.stream_next_queued_work(options).await
        };
        drain.map(lash_core::facade_support::QueuedTurnDrain::ran)
    });
    provider_stalled_rx
        .await
        .expect("provider should stall after both ingress claims are held");
    let renewals_before_loss = store.session_execution_lease_renewal_count();

    clock.advance_ms(lease_ttl.as_millis() as u64 + 1);
    let successor = lease_owner("renewal-failure-successor");
    let successor_lease =
        lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
            store.as_ref(),
            &SessionId::from("root"),
            &successor,
            "renewal-failure-mid-turn-does-not-select-a-durable-branch-executor",
            60_000,
        )
        .await
        .expect("claim expired session execution lease")
        .acquired()
        .expect("expired lease should be acquired by successor");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store.session_execution_lease_renewal_count() == renewals_before_loss {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("renewal task must observe the expired predecessor fence");
    provider_continue_tx
        .send(())
        .expect("provider should still be waiting");

    let drained = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("turn should finish")
        .expect("turn task");
    assert!(
        store.session_execution_lease_renewal_count() > renewals_before_loss,
        "the live renewal task must observe the expired predecessor fence"
    );
    if through_drive {
        let turn = drained
            .expect("no successor sealed, so the drive root's commit is admitted")
            .expect("the drive ran the root");
        assert!(matches!(turn.outcome, TurnOutcome::Finished(_)));
        let applications = lash_core::store::TurnInputStore::list_turn_input_applications(
            store.as_ref(),
            &SessionId::from("root"),
        )
        .await
        .expect("read the applied inputs");
        assert_eq!(
            applications.len(),
            1,
            "exactly one durable branch applies the input"
        );
        lash_core::store::SessionExecutionLeaseStore::release_session_execution_lease(
            store.as_ref(),
            &successor_lease.completion(),
        )
        .await
        .expect("release successor lease");
        return;
    }
    let assembled = drained.expect_err("queued progress requires the current lane fence");
    assert_eq!(
        assembled.code,
        lash_core::RuntimeErrorCode::QueuedRunPending
    );
    assert_eq!(
        store.abandoned_claim_counts(),
        (0, 0),
        "lease loss must retain the admission and its assigned claims"
    );

    lash_core::store::SessionExecutionLeaseStore::release_session_execution_lease(
        store.as_ref(),
        &successor_lease.completion(),
    )
    .await
    .expect("release successor lease");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
pub(super) async fn cancellation_sealed_before_renewal_failure_remains_evidence_bearing_cancelled()
{
    let backend = memory_backend().await;
    const SESSION_ID: &str = "cancellation-sealed-renewal";

    let lease_ttl = std::time::Duration::from_millis(120);
    // Renewal scheduling must not consume the cancellation fixture's lease.
    // The phase probe below orders the injected renewal rejection after sealing.
    let clock = Arc::new(ManualClock::new(1_000));
    let store = unbound_recording_store_with_clock(&backend, clock).await;
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let (provider_started_tx, provider_started_rx) = tokio::sync::oneshot::channel::<()>();
    let provider_started_tx = Arc::new(Mutex::new(Some(provider_started_tx)));
    let captured_provider_started_tx = Arc::clone(&provider_started_tx);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_request| {
            let captured_provider_started_tx = Arc::clone(&captured_provider_started_tx);
            async move {
                if let Some(tx) = captured_provider_started_tx.lock_recover().take() {
                    let _ = tx.send(());
                }
                std::future::pending::<Result<LlmResponse, _>>().await
            }
        })
        .build();
    let mut config = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_lease_timings(
        lash_core::facade_support::LeaseTimings::from_ttl(lease_ttl).expect("valid timings"),
    );
    config.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(transport.clone().into_handle()),
    );
    let mut runtime = TestRuntime::new(&backend, transport)
        .tools(Arc::new(EmptyTools))
        .host(lash_core::facade_support::EmbeddedRuntimeHost::new(config))
        .store(runtime_store)
        .with_session_id(SESSION_ID)
        .build()
        .await;
    let turn_driver = lash_core::facade_support::TurnWorkDriver::for_session(
        Arc::clone(&runtime.host.core.control.effect_host),
        SESSION_ID,
        Arc::clone(&store) as Arc<dyn lash_core::RuntimePersistence>,
    );
    let effect_loop_ended = Arc::new(AtomicBool::new(false));
    let release_effect_loop = Arc::new(AtomicBool::new(false));
    runtime.set_turn_phase_probe(Arc::new(PauseAfterEffectLoop {
        entered: Arc::clone(&effect_loop_ended),
        release: Arc::clone(&release_effect_loop),
    }));

    let turn_id = "cancel-before-renewal-failure";
    let persisted_state = runtime.export_persistence_state();
    let turn_scope = backend_admitted_scope(
        &backend,
        lash_core::AdmittedScope::unpinned(persisted_state.turn_scope(turn_id))
            .expect("turn scope"),
    );
    let turn_address =
        lash_core::facade_support::TurnAddress::new(&persisted_state.session_id, turn_id);
    let turn = lash_core::task::spawn(async move {
        runtime
            .run_turn_assembled(
                TurnInput::text("cancel before the lease renewal fails"),
                CancellationToken::new(),
                turn_scope,
            )
            .await
    });
    provider_started_rx
        .await
        .expect("provider should start after lease acquisition");
    let undelivered = lash_core::store::TurnInputStore::enqueue_pending_turn_input(
        store.as_ref(),
        lash_core::PendingTurnInputDraft::new(
            &persisted_state.session_id,
            lash_core::TurnInputIngress::active_turn(
                turn_id,
                lash_core::TurnInputCheckpointBoundary::AfterWork,
            ),
            lash_core::TurnInput::text("unsent steer restored by host"),
        ),
    )
    .await
    .expect("enqueue an undelivered active-turn input");
    let receipt = turn_driver
        .request_cancel(
            lash_core::facade_support::TurnCancelRequest::new(
                turn_address,
                "cancel-before-loss-request",
                Some("test-user".to_string()),
            )
            .with_reason("user stopped the turn")
            .undelivered(lash_core::TurnCancelDisposition::Drop),
        )
        .await
        .expect("seal user cancellation");
    assert!(matches!(
        receipt.outcome,
        lash_core::facade_support::TurnCancelOutcome::Requested(ref evidence)
            if evidence.request_id == "cancel-before-loss-request"
    ));

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !effect_loop_ended.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("turn should observe cancellation before finalization");
    let renewals_before_failure = store.session_execution_lease_renewal_count();
    store.fail_next_session_execution_lease_renewal();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store.session_execution_lease_renewal_count() == renewals_before_failure {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("renewal task should receive the injected store rejection");
    release_effect_loop.store(true, Ordering::SeqCst);

    let assembled = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("cancelled turn should finish")
        .expect("cancelled turn task")
        .expect("sealed cancellation should commit despite later renewal rejection");
    assert_eq!(
        assembled.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled {
            evidence: lash_core::facade_support::TurnCancellationEvidence {
                request_id: "cancel-before-loss-request".to_string(),
                origin: Some("test-user".to_string()),
                reason: Some("user stopped the turn".to_string()),
                undelivered: lash_core::TurnCancelDisposition::Drop,
                mode: lash_core::TurnCancelMode::Immediate,
                honoured_after_step: None,
            }
        })
    );
    assert_eq!(assembled.turn_cancel_input_outcome.affected_inputs.len(), 1);
    assert_eq!(
        assembled.turn_cancel_input_outcome.affected_inputs[0].input_id,
        undelivered.input_id
    );
    assert_eq!(
        assembled.turn_cancel_input_outcome.affected_inputs[0].disposition,
        lash_core::TurnCancelDisposition::Drop
    );
}

#[tokio::test]
pub(super) async fn finish_turn_commit_uses_head_cas_after_advisory_lease_expiry() {
    let backend = memory_backend().await;
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let store = unbound_recording_store_with_clock(&backend, store_clock).await;
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "committed after lease expiry".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let host_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let mut config = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(transport.clone().into_handle()),
    );
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        lash_core::facade_support::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(ExpireLeaseAtPreparedTurn::new(Arc::clone(&clock))));

    let assembled = runtime
        .run_turn_assembled(
            TurnInput::text("lease expires at commit"),
            CancellationToken::new(),
            backend_turn_scope(
                &backend,
                &SessionId::from("root"),
                &TurnId::from("final-commit-lease-expiry-turn"),
            ),
        )
        .await
        .expect("head CAS must authorize final commit after advisory lease expiry");

    assert_eq!(
        assembled.assistant_output.safe_text,
        "committed after lease expiry"
    );
}

#[tokio::test]
pub(super) async fn prepared_checkpoint_continues_after_advisory_lease_expiry() {
    let backend = memory_backend().await;
    let clock = Arc::new(ManualClock::new(1_000));
    let store_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let store = unbound_recording_store_with_clock(&backend, store_clock).await;
    let runtime_store: Arc<dyn lash_core::store::RuntimePersistence> = store.clone();
    let transport = mock_provider(vec![MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "provider reached".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let host_clock: Arc<dyn lash_core::Clock> = clock.clone();
    let mut config = lash_core::facade_support::RuntimeHostConfig::new(
        backend.clone(),
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    )
    .with_clock(host_clock);
    config.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(transport.clone().into_handle()),
    );
    let mut runtime = runtime_with_plugins_and_tools_and_host_and_store(
        Vec::new(),
        Arc::new(EmptyTools),
        transport,
        lash_core::facade_support::EmbeddedRuntimeHost::new(config),
        runtime_store,
    )
    .await;
    runtime.set_turn_phase_probe(Arc::new(ExpireLeaseAfterPromptBuild::new(Arc::clone(
        &clock,
    ))));

    let assembled = runtime
        .run_turn_assembled(
            TurnInput::text("lease expires at prepared checkpoint"),
            CancellationToken::new(),
            backend_turn_scope(
                &backend,
                &SessionId::from("root"),
                &TurnId::from("prepared-checkpoint-lease-expiry-turn"),
            ),
        )
        .await
        .expect("prepared checkpoint must continue after advisory lease expiry");

    assert_eq!(assembled.assistant_output.safe_text, "provider reached");
}

const SEED: u64 = 0x5_f440;

// Boundary: this durable process-wake case stays in `turns.rs` because it
// asserts committed conversation history, streamed turn events, and process
// origin metadata across the full runtime, not only persistence ownership.
#[tokio::test]
pub(super) async fn durable_process_wake_drains_as_committed_event_history_and_acknowledges() {
    let double = kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let transport = mock_provider(vec![
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "first answer".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
        MockCall {
            stream_events: Vec::new(),
            response: Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "acknowledged".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            }),
        },
    ]);
    let (mut runtime, store) =
        standard_runtime_with_transport_and_double_queue_store(&double, transport).await;
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    let target_scope = lash_core::SessionScope::new("root");
    let process_caused_by = lash_core::CausalRef::SessionNode {
        session_id: SessionId::from("root"),
        node_id: "trigger:button".to_string(),
    };
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                "wake-proc",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::session(target_scope.clone())
                    .with_caused_by(Some(process_caused_by.clone())),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(target_scope.session_id.clone())),
        )
        .await
        .expect("register wake process");
    let wake = append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        &ProcessId::from("wake-proc"),
        lash_core::ProcessEventAppendRequest::new(
            "process.wake",
            json!({
                "text": "deploy complete",
                "value": {
                    "status": "deploy complete"
                }
            }),
        ),
    )
    .await;
    let expected_wake_id = wake.wake_id.clone();
    let expected_sequence = wake.sequence;
    let expected_text = format!(
        "Background process wake\nProcess: wake-proc\nEvent: process.wake #{expected_sequence}\nWake input:\ndeploy complete"
    );

    let sink = RecordingSink::default();
    let turn_events = RecordingTurnEvents::default();
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("process-wake-turn"),
        ))
        .await
        .expect("open the turn's handler");
    runtime
        .stream_turn(
            TurnInput::text("hello"),
            TurnOptions::new(CancellationToken::new(), handler.scoped())
                .with_events(&sink)
                .with_turn_events(&turn_events),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    let turn_event_snapshot = turn_events.snapshot();
    let queued_started = turn_event_snapshot
        .iter()
        .find(|activity| {
            matches!(
                &activity.event,
                lash_core::TurnEvent::QueuedWorkStarted { .. }
            )
        })
        .expect("queued work started event");
    let lash_core::TurnEvent::QueuedWorkStarted {
        boundary, causes, ..
    } = &queued_started.event
    else {
        panic!("expected queued work started event");
    };
    assert_eq!(
        *boundary,
        lash_core::testing::runtime_internals::QueuedWorkClaimBoundary::ActiveTurnCheckpoint
    );
    assert!(causes.iter().any(|cause| {
        cause.event_type == "process.wake"
            && cause.id == expected_wake_id
            && cause.text == expected_text
            && matches!(
                &cause.origin,
                lash_core::MessageOrigin::Process {
                    process_id,
                    event_type,
                    sequence,
                    wake_id,
                    caused_by,
                } if process_id == "wake-proc"
                    && event_type == "process.wake"
                    && *sequence == expected_sequence
                    && wake_id.as_deref() == Some(expected_wake_id.as_str())
                    && caused_by.as_ref() == Some(&process_caused_by)
            )
    }));

    assert!(
        sink.snapshot().into_iter().all(|event| {
            !matches!(
                event,
                lash_core::facade_support::SessionStreamEvent::InjectedMessagesCommitted { messages, .. }
                    if messages.iter().any(|message| message.parts.iter().any(|part| part.content() == expected_text))
            )
        }),
        "durable wake events must not be bridged as injected plugin messages"
    );
    assert!(
        lash_core::store::QueuedWorkStore::list_queued_work(
            store.as_ref(),
            &SessionId::from("root")
        )
        .await
        .expect("queued work after commit")
        .is_empty()
    );
    let wake_history = active_conversation_messages(&runtime.state)
        .into_iter()
        .find(|message| {
            message.role == lash_core::MessageRole::Event
                && message
                    .parts
                    .iter()
                    .any(|part| part.content() == expected_text)
        })
        .expect("wake history message");
    assert!(matches!(
        wake_history.origin,
        Some(lash_core::MessageOrigin::Process {
            process_id,
            event_type,
            sequence,
            wake_id,
            caused_by,
        }) if process_id == "wake-proc"
            && event_type == "process.wake"
            && sequence == expected_sequence
            && wake_id.as_deref() == Some(expected_wake_id.as_str())
            && caused_by.as_ref() == Some(&process_caused_by)
    ));
    assert!(
        active_conversation_messages(&runtime.state)
            .iter()
            .all(|message| {
                !((message.role == lash_core::MessageRole::System
                    || message.role == lash_core::MessageRole::User)
                    && message
                        .parts
                        .iter()
                        .any(|part| part.content() == expected_text))
            }),
        "durable wake must not enter history as provider system text"
    );
}

/// FIG-1313 red-side anchor (a): the small-window wedge.
///
/// A 1,000-token model with roughly 900 tokens of retained history used to
/// refuse every selected queued drain outright — the complete projected
/// request (prompt + history + wake + action reserve) could not fit, so the
/// queue could never drain even one short wake, while ordinary turns on the
/// same fixture kept succeeding. Drain size is host policy now, and the shipped
/// one-at-a-time default leaves the provider as the authority on fit.
#[tokio::test]
pub(super) async fn a_selected_queued_wake_drains_under_a_small_window_with_retained_history() {
    let backend = memory_backend().await;
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let captured_provider_calls = Arc::clone(&provider_calls);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| {
            let provider_calls = Arc::clone(&captured_provider_calls);
            async move {
                provider_calls.fetch_add(1, Ordering::SeqCst);
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "retained answer".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(&backend, transport).await;
    runtime.host.core.durability.queued_work_batching =
        lash_core::QueuedWorkBatchingConfig::new(100);
    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            model: Some(
                lash_core::ModelSpec::builder("mock-model")
                    .context_window_tokens(1_000)
                    .build()
                    .expect("valid constrained model"),
            ),
            ..Default::default()
        })
        .await
        .expect("constrain context window");

    runtime
        .run_turn_assembled(
            TurnInput::text("r".repeat(900)),
            CancellationToken::new(),
            backend_turn_scope(
                &backend,
                &SessionId::from("root"),
                &TurnId::from("seed-retained-history"),
            ),
        )
        .await
        .expect("seed retained history without queued work");

    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                "reserve-proc",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::session(lash_core::SessionScope::new("root")),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(SessionId::from("root"))),
        )
        .await
        .expect("register wake process");
    append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        &ProcessId::from("reserve-proc"),
        lash_core::ProcessEventAppendRequest::new(
            "process.wake",
            json!({"text": "short wake", "value": {"status": "done"}}),
        ),
    )
    .await;

    let batch_id = lash_core::store::QueuedWorkStore::list_pending_queued_work(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("list wake for selected drain")
    .into_iter()
    .next()
    .expect("queued wake")
    .batch_id;

    runtime
        .stream_selected_queued_work(
            TurnOptions::new(
                CancellationToken::new(),
                backend_queued_scope(
                    &backend,
                    &SessionId::from("root"),
                    &TurnId::from("small-window-drain"),
                ),
            ),
            &[batch_id],
        )
        .await
        .expect("a short wake drains under a small window");
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    let pending = lash_core::store::QueuedWorkStore::list_pending_queued_work(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("list drained queue");
    assert!(
        pending.is_empty(),
        "the drained wake must not remain pending: {pending:?}"
    );
}

/// FIG-1313: an exact host selection is not resized by the automatic policy.
///
/// The host named this composition, so the drain policy — which answers only
/// "how much of the pending queue should this wake take?" — is not consulted.
/// Under the shipped one-at-a-time default a policy-sized exact claim would
/// take one of the two requested rows, and the caller would abandon the partial
/// claim as unclaimable: `stream_selected_queued_work` on two mergeable wakes
/// could then never succeed, permanently and deterministically.
#[tokio::test]
pub(super) async fn an_exact_two_row_selection_drains_under_the_one_at_a_time_default() {
    let backend = memory_backend().await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured_requests = Arc::clone(&requests);
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |req| {
            let captured_requests = Arc::clone(&captured_requests);
            async move {
                captured_requests.lock_recover().push(req);
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "both wakes answered".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build();
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(&backend, transport).await;
    // The shipped default: no `with_drain_mode`, so `DrainMode::OneAtATime`.
    runtime.host.core.durability.queued_work_batching =
        lash_core::QueuedWorkBatchingConfig::new(100);
    assert_eq!(
        runtime
            .host
            .core
            .durability
            .queued_work_batching
            .drain_policy()
            .name(),
        "one_at_a_time"
    );
    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                "paired-wake-proc",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::session(lash_core::SessionScope::new("root")),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(SessionId::from("root"))),
        )
        .await
        .expect("register wake process");
    for text in ["first paired wake", "second paired wake"] {
        append_process_wake_to_queue(
            registry.as_ref(),
            store.as_ref(),
            &ProcessId::from("paired-wake-proc"),
            lash_core::ProcessEventAppendRequest::new(
                "process.wake",
                json!({"text": text, "value": {"status": "done"}}),
            ),
        )
        .await;
    }

    // Both rows share `PROCESS_WAKE_MERGE_KEY`, so they are mergeable and the
    // automatic policy would have a choice to make here.
    let batch_ids = lash_core::store::QueuedWorkStore::list_pending_queued_work(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("list queued wakes")
    .into_iter()
    .map(|batch| batch.batch_id)
    .collect::<Vec<_>>();
    assert_eq!(batch_ids.len(), 2);

    runtime
        .stream_selected_queued_work(
            TurnOptions::new(
                CancellationToken::new(),
                backend_queued_scope(
                    &backend,
                    &SessionId::from("root"),
                    &TurnId::from("paired-exact-drain"),
                ),
            ),
            &batch_ids,
        )
        .await
        .expect("an exact two-row selection is claimable")
        .expect("the exact selection produces a turn");

    let pending = lash_core::store::QueuedWorkStore::list_pending_queued_work(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("list queue after exact drain");
    assert!(
        pending.is_empty(),
        "both selected rows must drain together: {pending:?}"
    );
    let requests = requests.lock_recover().clone();
    let last = requests.last().expect("a provider call");
    assert!(request_contains_text(last, "first paired wake"));
    assert!(
        request_contains_text(last, "second paired wake"),
        "the exact composition, not a policy-sized prefix, must reach the model"
    );
}

/// FIG-1313 red-side anchor (b): the irreducible residue stays typed.
///
/// A single queued row larger than the whole context window can never be
/// drained by any policy. It must name itself and the window it needs, not
/// wedge the queue silently.
#[tokio::test]
pub(super) async fn an_irreducibly_oversized_queued_row_is_refused_by_name() {
    let backend = memory_backend().await;
    let transport = TestProvider::builder()
        .kind("mock")
        .requires_streaming(true)
        .complete(move |_| async move {
            panic!("an irreducibly oversized row must never reach the provider");
        })
        .build();
    let (mut runtime, store) =
        standard_runtime_with_transport_and_queue_store(&backend, transport).await;
    runtime.host.core.durability.queued_work_batching =
        lash_core::QueuedWorkBatchingConfig::new(100);
    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            model: Some(
                lash_core::ModelSpec::builder("mock-model")
                    .context_window_tokens(1_000)
                    .build()
                    .expect("valid constrained model"),
            ),
            ..Default::default()
        })
        .await
        .expect("constrain context window");

    let registry = runtime
        .host
        .process_registry()
        .cloned()
        .expect("process registry");
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                "oversized-proc",
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::session(lash_core::SessionScope::new("root")),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types([process_wake_event_type()])
            .with_wake_session_id(Some(SessionId::from("root"))),
        )
        .await
        .expect("register wake process");
    let wake = append_process_wake_to_queue(
        registry.as_ref(),
        store.as_ref(),
        &ProcessId::from("oversized-proc"),
        lash_core::ProcessEventAppendRequest::new(
            "process.wake",
            json!({"text": "w".repeat(4_000), "value": {"status": "done"}}),
        ),
    )
    .await;

    let batch_id = lash_core::store::QueuedWorkStore::list_pending_queued_work(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("list oversized wake")
    .into_iter()
    .next()
    .expect("queued wake")
    .batch_id;

    let err = runtime
        .stream_selected_queued_work(
            TurnOptions::new(
                CancellationToken::new(),
                backend_queued_scope(
                    &backend,
                    &SessionId::from("root"),
                    &TurnId::from("oversized-row"),
                ),
            ),
            std::slice::from_ref(&batch_id),
        )
        .await
        .expect_err("a row larger than the window must be refused");
    let super::super::turn_loop::SelectedQueuedWorkDrainError::Refused { cause } = err else {
        panic!("an oversized row must surface as a typed refusal, not a bare runtime error");
    };
    let super::super::turn_loop::SelectedQueuedWorkDrainRefusalCause::
        QueuedItemExceedsContextWindow {
            batch_id: refused_batch_id,
            required_context_tokens,
            max_context_tokens,
            ..
        } = cause
    else {
        panic!("expected the oversized-row refusal, got {cause:?}");
    };
    assert_eq!(refused_batch_id, batch_id);
    assert_eq!(max_context_tokens, 1_000);
    assert!(required_context_tokens > max_context_tokens);
    let pending = lash_core::store::QueuedWorkStore::list_pending_queued_work(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("list refused wake");
    assert_eq!(pending.len(), 1);
    assert!(matches!(
        &pending[0].items[0].payload,
        lash_core::testing::runtime_internals::QueuedWorkPayload::ProcessWake { wake: pending_wake }
            if pending_wake.wake_id == wake.wake_id
    ));
}

#[tokio::test]
pub(super) async fn plugin_command_reuses_caller_scope_on_lost_response_retry() {
    let double = kernel_double(SEED + 1, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let plugin: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(RuntimeTestPluginFactory {
            build: Arc::new(|_| {
                Ok(Arc::new(RuntimeTestPlugin {
                    before_turn: None,
                    checkpoint: None,
                    presentation_steps: vec![],
                    runtime_event: None,
                    external_registrar: Some(Arc::new(|reg| {
                        reg.operations().command(
                            lash_core::plugin::PluginOperationSpec {
                                name: "test.emit".to_string(),
                                description: "emit one durable event".to_string(),
                                session_param: lash_core::facade_support::SessionParam::Optional,
                                input_schema: json!({}),
                                output_schema: json!({}),
                            },
                            Arc::new(|_, _| {
                                Box::pin(async move {
                                    Ok(lash_core::plugin::ErasedPluginOperationOutcome {
                                        output: json!({"ok": true}),
                                        events: vec![lash_core::PluginRuntimeEvent::Custom {
                                            name: "test.event".to_string(),
                                            payload: json!({"value": 1}),
                                        }],
                                        directives: Vec::new(),
                                    })
                                })
                            }),
                        )
                    })),
                }))
            }),
        });
    let store = double_unbound_recording_store(&double).await;
    let store_trait = store.clone() as Arc<dyn lash_core::RuntimePersistence>;
    let mut first = runtime_with_plugins_and_tools_and_host_and_store(
        vec![Arc::clone(&plugin)],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(&backend),
        Arc::clone(&store_trait),
    )
    .await;
    let operation_scope =
        lash_core::ExecutionScope::runtime_operation("root:plugin-command:stable-request");

    first
        .run_plugin_command("test.emit", json!({}), None, operation_scope.clone())
        .await
        .expect("first command attempt");
    let committed_after_first = *store.runtime_commit_count.lock_recover();
    let mut retry = runtime_with_plugins_and_tools_and_host_and_store(
        vec![plugin],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        test_host_config(&backend),
        store_trait,
    )
    .await;

    retry
        .run_plugin_command("test.emit", json!({}), None, operation_scope)
        .await
        .expect("lost-response retry");

    assert_eq!(committed_after_first, 2);
    assert_eq!(
        *store.runtime_commit_count.lock_recover(),
        committed_after_first,
        "retrying one command scope must receipt-hit both durable effects"
    );
}

#[tokio::test]
pub(super) async fn session_manager_can_run_child_session_turn() {
    let double = kernel_double(SEED + 2, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let transport = mock_provider(vec![MockCall {
        stream_events: vec![
            LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "child ".to_string(),
            },
            LlmStreamEvent::Delta {
                block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                text: "session".to_string(),
            },
            LlmStreamEvent::Usage(LlmUsage {
                input_tokens: 7,
                output_tokens: 2,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 1,
            }),
        ],
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "child session".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    }]);
    let runtime = runtime_with_plugins(&backend, Vec::new(), transport).await;
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let plugin_init = runtime
        .session_state_service()
        .expect("session state")
        .session_plugin_init(&SessionId::from(runtime.session_id()))
        .await
        .expect("plugin init");
    let handle = lifecycle
        .create_session(
            lash_core::SessionCreateRequest::root(
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init.clone()),
        )
        .await
        .expect("child session");
    let mut child = reopen_session_runtime(&runtime, &handle.session_id).await;
    let turn_id = "child-lifecycle-turn";
    let handler = double
        .open_handler(AdmittedScope::turn(
            handle.session_id.clone(),
            TurnId::from(turn_id),
        ))
        .await
        .expect("open the child turn's handler");
    let assembled = child
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            handler.scoped(),
        )
        .await
        .expect("child turn");
    handler
        .close()
        .await
        .expect("close the child turn's handler");
    assert_eq!(handle.session_id, "child");
    assert_eq!(handle.policy.model.id, "mock-model");
    assert_eq!(assembled.state.session_id, "child");
}

#[tokio::test]
pub(super) async fn session_manager_preserves_runtime_error_from_child_session_turn() {
    let double = kernel_double(SEED + 3, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let factory = RecordingSessionStoreFactory::over(backend.session_store_factory());
    let backend = LayeredBackend::over(backend)
        .map_session_store_factory(|_| Arc::new(factory.clone()))
        .into_backend();
    let host = test_host_config(&backend);
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        host,
    )
    .await;
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let plugin_init = runtime
        .session_state_service()
        .expect("session state")
        .session_plugin_init(&SessionId::from(runtime.session_id()))
        .await
        .expect("plugin init");
    let handle = lifecycle
        .create_session(
            lash_core::SessionCreateRequest::root(
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("busy-child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init.clone()),
        )
        .await
        .expect("child session");
    let store = factory
        .store_for(&handle.session_id)
        .expect("child session store");
    let held_lease =
        lash_core::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
            store.as_ref(),
            &handle.session_id,
            &lease_owner("other-child-runtime"),
            "session-manager-runtime-error-boundary-executor",
            60_000,
        )
        .await
        .expect("claim child session execution lease")
        .acquired()
        .expect("child session execution lease");
    let turn_id = "busy-child-turn";
    let mut child = reopen_session_runtime(&runtime, &handle.session_id).await;

    let handler = double
        .open_handler(AdmittedScope::turn(
            handle.session_id.clone(),
            TurnId::from(turn_id),
        ))
        .await
        .expect("open the child turn's handler");
    let error = child
        .run_turn_assembled(
            TurnInput::text("preserve the runtime error"),
            CancellationToken::new(),
            handler.scoped(),
        )
        .await
        .expect_err("the held child session lane must refuse the turn");
    handler
        .close()
        .await
        .expect("close the child turn's handler");

    assert!(
        error.code == lash_core::RuntimeErrorCode::SessionExecutionLaneBusy,
        "the ordinary turn boundary must preserve the typed runtime error, got {error:?}"
    );
    lash_core::store::SessionExecutionLeaseStore::release_session_execution_lease(
        store.as_ref(),
        &held_lease.completion(),
    )
    .await
    .expect("release child session execution lease");
}

#[tokio::test]
pub(super) async fn session_manager_persists_child_sessions_in_separate_store() {
    let double = kernel_double(SEED + 4, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let factory = RecordingSessionStoreFactory::over(backend.session_store_factory());
    let backend = LayeredBackend::over(backend)
        .map_session_store_factory(|_| Arc::new(factory.clone()))
        .into_backend();
    let host = test_host_config(&backend);
    let runtime = runtime_with_plugins_and_tools_and_host(
        Vec::new(),
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        host,
    )
    .await;
    let manager = runtime.session_state_service().expect("session state");
    let plugin_init = manager
        .session_plugin_init(&SessionId::from("root"))
        .await
        .expect("plugin init");
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let handle = lifecycle
        .create_session(
            lash_core::SessionCreateRequest::child_session(
                "root",
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("child-store")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init.clone()),
        )
        .await
        .expect("child session");

    assert_eq!(handle.session_id, "child-store");
    let stores = factory.stores();
    assert_eq!(stores.len(), 1);
    let meta = lash_core::store::SessionCommitStore::load_session_meta(stores[0].as_ref())
        .await
        .expect("load session meta")
        .expect("session meta");
    assert_eq!(meta.session_id, "child-store");
    assert_eq!(meta.parent_session_id(), Some("root"));
    let read = lash_core::store::SessionCommitStore::load_session(stores[0].as_ref())
        .await
        .expect("load session")
        .expect("session read");
    let graph = read.graph;
    let child_frame_key = lash_core::FrameKey::from_caller_material("initial-frame")
        .expect("non-empty initial frame material");
    let child_frame_node_id =
        lash_core::session_graph::frame_node_id(&meta.session_id, child_frame_key.as_str());
    assert_eq!(
        graph.nodes.first().map(|node| node.node_id.as_str()),
        Some(child_frame_node_id.as_str())
    );
    assert_eq!(
        graph
            .nodes
            .first()
            .and_then(|node| node.parent_node_id.as_deref()),
        None
    );
    assert_eq!(
        graph
            .nodes
            .iter()
            .filter(|node| matches!(
                node.payload,
                lash_core::SessionNodePayload::FrameOpen { .. }
            ))
            .count(),
        1,
        "child history must not retain the parent frame root"
    );
    let read_model = graph.read_model(None).unwrap();
    assert!(
        read_model.messages.is_empty(),
        "an empty-start child initializes with no inherited messages"
    );
    let checkpoint = read.checkpoint.expect("checkpoint");
    assert_eq!(checkpoint.turn_state.turn_index, 0);
}

#[tokio::test]
pub(super) async fn child_relation_does_not_replace_active_session() {
    let double = kernel_double(SEED + 5, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let mut runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let plugin_init = runtime
        .session_state_service()
        .expect("session state")
        .session_plugin_init(&SessionId::from(runtime.session_id()))
        .await
        .expect("plugin init");
    lifecycle
        .create_session(
            lash_core::SessionCreateRequest::child_session(
                runtime.session_id(),
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("ordinary-child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init.clone()),
        )
        .await
        .expect("child session");

    assert_eq!(runtime.session_id(), "root");
    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("ordinary-child-parent-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let assembled = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "parent turn".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            handler.scoped(),
        )
        .await
        .expect("parent turn");
    handler.close().await.expect("close the turn's handler");

    assert_eq!(assembled.state.session_id, "root");
    assert_eq!(assembled.state.turn_index, 1);
}

#[tokio::test]
pub(super) async fn session_manager_rejects_duplicate_child_session_ids() {
    let double = kernel_double(SEED + 6, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    let lifecycle = runtime
        .session_lifecycle_service()
        .expect("session lifecycle");
    let plugin_init = runtime
        .session_state_service()
        .expect("session state")
        .session_plugin_init(&SessionId::from(runtime.session_id()))
        .await
        .expect("plugin init");
    lifecycle
        .create_session(
            lash_core::SessionCreateRequest::root(
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init.clone()),
        )
        .await
        .expect("first child session");

    let err = lifecycle
        .create_session(
            lash_core::SessionCreateRequest::root(
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("child")
            .with_plugin_source(lash_core::SessionPluginSource::ParentFork)
            .with_plugin_init(plugin_init.clone()),
        )
        .await
        .expect_err("duplicate child session should fail");
    assert!(err.to_string().contains("already exists"));
}

#[test]
pub(super) fn queued_work_payload_cannot_encode_persisted_turn_input() {
    // This exhaustive match is the type-level ingress proof: generic queued
    // work has no model-visible TurnInput representation. Persisted user input
    // therefore has to cross the dedicated PendingTurnInputDraft/
    // TurnInputStore seam used by `LashRuntime::enqueue_turn_input`.
    fn work_class(
        payload: &lash_core::testing::runtime_internals::QueuedWorkPayload,
    ) -> lash_core::store::QueuedWorkClass {
        match payload {
            lash_core::testing::runtime_internals::QueuedWorkPayload::ProcessWake { .. } => {
                lash_core::store::QueuedWorkClass::TurnWork
            }
            lash_core::testing::runtime_internals::QueuedWorkPayload::SessionCommand { .. } => {
                lash_core::store::QueuedWorkClass::SessionCommand
            }
        }
    }

    let payload = lash_core::testing::runtime_internals::QueuedWorkPayload::session_command(
        lash_core::facade_support::SessionCommand::RefreshToolCatalog {
            reason: "type-level ingress proof".to_string(),
        },
    );
    assert_eq!(work_class(&payload), payload.work_class());
}

#[tokio::test]
pub(super) async fn turn_driver_normalizes_alias_effort_into_outgoing_request() {
    let double = kernel_double(SEED + 7, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    use std::sync::{Arc, Mutex};

    let captured: Arc<Mutex<Option<lash_core::ReasoningSelection>>> = Arc::new(Mutex::new(None));
    let captured_for_provider = Arc::clone(&captured);
    let provider = TestProvider::builder()
        .kind("capability-capture")
        .complete(move |req| {
            let captured = Arc::clone(&captured_for_provider);
            async move {
                *captured.lock_recover() = Some(req.model_variant.clone());
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "ok".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle();

    let capability = lash_core::ModelCapability {
        instruction_role: Default::default(),
        native_mid_conversation_system: false,
        attachment_acceptance: Default::default(),
        google_dialect: Default::default(),
        reasoning: Some(lash_core::ReasoningCapability {
            efforts: ["low", "medium", "high", "max"]
                .into_iter()
                .map(String::from)
                .collect(),
            aliases: std::collections::BTreeMap::from([("xhigh".to_string(), "max".to_string())]),
            ..Default::default()
        }),
        cache_control: None,
        stream_termination: None,
        sampling: lash_core::SamplingCapability::Configurable,
        reasoning_retention: Default::default(),
    };
    let model = lash_core::ModelSpec::builder("mock-model")
        .variant(lash_core::ReasoningSelection::Effort("xhigh".to_string()))
        .context_window_tokens(200_000)
        .build()
        .expect("valid model spec")
        .with_capability(capability);

    let mut runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    serve_runtime_providers(&mut runtime, [provider.clone()]);
    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            provider_id: Some(provider.kind().to_string()),
            model: Some(model),
            ..Default::default()
        })
        .await
        .expect("update session config");

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("alias-normalize-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            handler.scoped(),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    assert_eq!(turn.assistant_output.safe_text, "ok");
    let seen = captured
        .lock_recover()
        .clone()
        .expect("provider must be called");
    assert_eq!(
        seen,
        lash_core::ReasoningSelection::Effort("max".to_string()),
        "alias `xhigh` must clamp to canonical `max` before the provider sees the request"
    );
}

#[tokio::test]
pub(super) async fn turn_driver_rejects_unsupported_effort_before_provider_call() {
    let double = kernel_double(SEED + 8, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let called = Arc::new(AtomicBool::new(false));
    let called_for_provider = Arc::clone(&called);
    let provider = TestProvider::builder()
        .kind("capability-reject")
        .complete(move |_req| {
            let called = Arc::clone(&called_for_provider);
            async move {
                called.store(true, Ordering::SeqCst);
                Ok(LlmResponse::default())
            }
        })
        .build()
        .into_handle();

    let capability = lash_core::ModelCapability {
        instruction_role: Default::default(),
        native_mid_conversation_system: false,
        attachment_acceptance: Default::default(),
        google_dialect: Default::default(),
        reasoning: Some(lash_core::ReasoningCapability {
            efforts: ["low", "medium", "high"]
                .into_iter()
                .map(String::from)
                .collect(),
            ..Default::default()
        }),
        cache_control: None,
        stream_termination: None,
        sampling: lash_core::SamplingCapability::Configurable,
        reasoning_retention: Default::default(),
    };
    let model = lash_core::ModelSpec::builder("mock-model")
        .variant(lash_core::ReasoningSelection::Effort("turbo".to_string()))
        .context_window_tokens(200_000)
        .build()
        .expect("valid model spec")
        .with_capability(capability);

    let mut runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    serve_runtime_providers(&mut runtime, [provider.clone()]);
    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            provider_id: Some(provider.kind().to_string()),
            model: Some(model),
            ..Default::default()
        })
        .await
        .expect("update session config");

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("unsupported-effort-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            handler.scoped(),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    assert!(
        !called.load(Ordering::SeqCst),
        "an unsupported effort must be rejected before the provider is called"
    );
    let issue = turn
        .errors
        .iter()
        .find(|issue| issue.kind == lash_core::TurnFailureKind::LlmProvider)
        .expect("llm_provider issue");
    assert_eq!(
        issue.code,
        Some(lash_core::TurnFailureCode::UnsupportedEffort.into())
    );
    assert!(issue.message.contains("Unsupported effort `turbo`"));
}

#[tokio::test]
pub(super) async fn session_generation_options_reach_every_provider_request() {
    let double = kernel_double(SEED + 9, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};

    let captured: Arc<Mutex<Vec<lash_core::GenerationOptions>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_provider = Arc::clone(&captured);
    let provider = TestProvider::builder()
        .kind("generation-capture")
        // A provider-level output cap is provider configuration, not request
        // intent: it must not appear on the request the turn driver builds.
        // The adapter layers it under the request in `resolve_generation_policy`.
        .options(lash_core::facade_support::ProviderOptions {
            max_output_tokens: Some(1_024),
            ..Default::default()
        })
        .complete(move |req| {
            let captured = Arc::clone(&captured_for_provider);
            async move {
                captured.lock_recover().push(req.generation.clone());
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "ok".to_string(),
                        response_meta: None,
                    }],
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle();

    let mut runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    serve_runtime_providers(&mut runtime, [provider.clone()]);
    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            provider_id: Some(provider.kind().to_string()),
            ..Default::default()
        })
        .await
        .expect("update session config");

    let run_turn = async |runtime: &mut LashRuntime, turn_id: &TurnId| {
        let handler = double
            .open_handler(AdmittedScope::turn(
                SessionId::from("root"),
                turn_id.clone(),
            ))
            .await
            .expect("open the turn's handler");
        runtime
            .run_turn_assembled(
                TurnInput {
                    items: vec![InputItem::Text {
                        text: "hello".to_string(),
                    }],
                    protocol_turn_options: None,
                    trace_turn_id: None,
                    protocol_extension: None,
                    turn_context: lash_core::TurnContext::default(),
                },
                CancellationToken::new(),
                handler.scoped(),
            )
            .await
            .expect("turn");
        handler.close().await.expect("close the turn's handler");
    };

    run_turn(&mut runtime, &TurnId::from("generation-default-turn")).await;

    let requested = lash_core::GenerationOptions {
        output_token_cap: NonZeroUsize::new(64),
        temperature: Some(lash_core::NonNegativeFiniteF64::new(0.0).expect("finite temperature")),
        seed: Some(1234),
        stop_sequences: Vec::new(),
        ..Default::default()
    };
    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            generation: Some(lash_core::facade_support::GenerationOverlay::Replace(
                requested.clone(),
            )),
            ..Default::default()
        })
        .await
        .expect("update session config");
    run_turn(&mut runtime, &TurnId::from("generation-requested-turn")).await;

    let seen = captured.lock_recover().clone();
    assert_eq!(seen.len(), 2, "each turn issues one provider call");
    assert_eq!(
        seen[0],
        lash_core::GenerationOptions::default(),
        "a session that requested nothing must not have provider config echoed back as request intent"
    );
    assert_eq!(
        seen[1], requested,
        "the session's generation options must reach the provider request verbatim"
    );
    assert_eq!(
        runtime.session_policy().generation,
        requested,
        "the requested options are durable session policy, not per-turn state"
    );
}

#[tokio::test]
pub(super) async fn omitted_generation_options_are_reported_on_the_turn_llm_call_record() {
    let double = kernel_double(SEED + 10, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    use std::num::NonZeroUsize;

    // The adapter's silent omission (a model that pins sampling, a wire with
    // no seed field) stays silent so one session-wide setting works across
    // mixed models — but the turn record says what actually reached the wire,
    // so a host asserting repeatability learns it was not honored.
    let dropped_sampling = lash_core::GenerationReceipt {
        output_token_cap: lash_core::GenerationOptionOutcome::Applied,
        temperature: lash_core::GenerationOptionOutcome::OmittedSamplingPinned,
        seed: lash_core::GenerationOptionOutcome::OmittedUnsupported,
        stop_sequences: lash_core::GenerationOptionOutcome::NotRequested,
        cache: lash_core::GenerationOptionOutcome::NotRequested,
    };
    let provider = TestProvider::builder()
        .kind("disposition-reporting")
        .complete(move |_req| async move {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "ok".to_string(),
                    response_meta: None,
                }],
                generation_disposition: Some(dropped_sampling),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle();

    let mut runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    serve_runtime_providers(&mut runtime, [provider.clone()]);
    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            provider_id: Some(provider.kind().to_string()),
            generation: Some(lash_core::facade_support::GenerationOverlay::Replace(
                lash_core::GenerationOptions {
                    output_token_cap: NonZeroUsize::new(128),
                    temperature: Some(
                        lash_core::NonNegativeFiniteF64::new(0.2).expect("finite temperature"),
                    ),
                    seed: Some(99),
                    stop_sequences: Vec::new(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        })
        .await
        .expect("update session config");

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("generation-disposition-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            handler.scoped(),
        )
        .await
        .expect("turn");
    handler.close().await.expect("close the turn's handler");

    let attempt = turn
        .llm_calls
        .first()
        .expect("one provider call")
        .attempts
        .first()
        .expect("one attempt");
    let reported = attempt
        .generation_disposition
        .expect("the adapter reported what it sent");
    assert_eq!(reported, dropped_sampling);
    assert!(
        !reported.nothing_omitted(),
        "a host asserting repeatability must be able to see the omission"
    );
}

#[tokio::test]
pub(super) async fn an_output_token_cap_above_the_model_clamps_and_says_so() {
    let double = kernel_double(SEED + 11, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    use std::num::NonZeroUsize;
    use std::sync::{Arc, Mutex};

    // The cap is a bound, not a demand, and it is durable session policy: a
    // `update_session_config` selecting a smaller model must not leave the
    // session failing every remaining turn. It sends what the model can
    // produce, and the disposition says the number was reduced.
    let captured: Arc<Mutex<Vec<lash_core::GenerationOptions>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_for_provider = Arc::clone(&captured);
    let provider = TestProvider::builder()
        .kind("clamping-capture")
        .complete(move |req| {
            let captured = Arc::clone(&captured_for_provider);
            async move {
                captured.lock_recover().push(req.generation.clone());
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "ok".to_string(),
                        response_meta: None,
                    }],
                    // The adapter reports the cap it was handed as applied; it
                    // has no idea a larger one was asked for.
                    generation_disposition: Some(lash_core::GenerationReceipt {
                        output_token_cap: lash_core::GenerationOptionOutcome::Applied,
                        temperature: lash_core::GenerationOptionOutcome::Applied,
                        seed: lash_core::GenerationOptionOutcome::NotRequested,
                        stop_sequences: lash_core::GenerationOptionOutcome::NotRequested,
                        cache: lash_core::GenerationOptionOutcome::NotRequested,
                    }),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle();

    let mut runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    serve_runtime_providers(&mut runtime, [provider.clone()]);
    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            provider_id: Some(provider.kind().to_string()),
            model: Some(
                lash_core::ModelSpec::builder("small-output-model")
                    .context_window_tokens(200_000)
                    .output_token_capacity(2_048)
                    .build()
                    .expect("valid test model"),
            ),
            generation: Some(lash_core::facade_support::GenerationOverlay::Replace(
                lash_core::GenerationOptions {
                    output_token_cap: NonZeroUsize::new(32_000),
                    temperature: Some(
                        lash_core::NonNegativeFiniteF64::new(0.0).expect("finite temperature"),
                    ),
                    seed: None,
                    stop_sequences: Vec::new(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        })
        .await
        .expect("update session config");

    let handler = double
        .open_handler(AdmittedScope::turn(
            SessionId::from("root"),
            TurnId::from("clamped-cap-turn"),
        ))
        .await
        .expect("open the turn's handler");
    let turn = runtime
        .run_turn_assembled(
            TurnInput {
                items: vec![InputItem::Text {
                    text: "hello".to_string(),
                }],
                protocol_turn_options: None,
                trace_turn_id: None,
                protocol_extension: None,
                turn_context: lash_core::TurnContext::default(),
            },
            CancellationToken::new(),
            handler.scoped(),
        )
        .await
        .expect("a cap above the model's capacity must not fail the turn");
    handler.close().await.expect("close the turn's handler");

    let seen = captured.lock_recover().clone();
    assert_eq!(
        seen.first().expect("one provider call").output_token_cap,
        NonZeroUsize::new(2_048),
        "the request carries the model's capacity, not the larger cap asked for"
    );
    assert_eq!(
        runtime.session_policy().generation.output_token_cap,
        NonZeroUsize::new(32_000),
        "clamping is per request against the current model; the session's intent is unchanged"
    );

    let reported = turn
        .llm_calls
        .first()
        .expect("one provider call")
        .attempts
        .first()
        .expect("one attempt")
        .generation_disposition
        .expect("the adapter reported what it sent");
    assert_eq!(
        reported.output_token_cap,
        lash_core::GenerationOptionOutcome::ClampedToCapacity
    );
    assert!(
        reported.nothing_omitted(),
        "a clamped cap reached the wire; it was not dropped"
    );
    assert!(
        !reported.fully_honored(),
        "a host that needs the number it asked for must be able to see the reduction"
    );
}

#[tokio::test]
pub(super) async fn a_mid_run_generation_patch_merges_like_the_spec_overlay_does() {
    let double = kernel_double(SEED + 12, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    use std::num::NonZeroUsize;

    // Both surfaces that set generation options speak one vocabulary. A patch
    // naming only a cap must not drop a temperature and seed the session
    // pinned — the loss `SessionSpec`'s overlay exists to prevent, one API
    // over — and replacing stays available for a host that means it.
    let mut runtime = runtime_with_plugins(&backend, Vec::new(), mock_provider(Vec::new())).await;
    let pinned = lash_core::GenerationOptions {
        output_token_cap: None,
        temperature: Some(lash_core::NonNegativeFiniteF64::new(0.0).expect("finite temperature")),
        seed: Some(42),
        stop_sequences: Vec::new(),
        ..Default::default()
    };
    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            generation: Some(lash_core::facade_support::GenerationOverlay::Replace(
                pinned.clone(),
            )),
            ..Default::default()
        })
        .await
        .expect("update session config");

    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            generation: Some(lash_core::facade_support::GenerationOverlay::Merge(
                lash_core::GenerationOptions {
                    output_token_cap: NonZeroUsize::new(4_096),
                    ..Default::default()
                },
            )),
            ..Default::default()
        })
        .await
        .expect("update session config");
    assert_eq!(
        runtime.session_policy().generation,
        lash_core::GenerationOptions {
            output_token_cap: NonZeroUsize::new(4_096),
            temperature: pinned.temperature.clone(),
            seed: Some(42),
            stop_sequences: Vec::new(),
            ..Default::default()
        },
        "a patch that names only a cap keeps the sampling the session pinned"
    );

    runtime
        .update_session_config(lash_core::facade_support::SessionConfigPatch {
            generation: Some(lash_core::facade_support::GenerationOverlay::Replace(
                lash_core::GenerationOptions::default(),
            )),
            ..Default::default()
        })
        .await
        .expect("update session config");
    assert_eq!(
        runtime.session_policy().generation,
        lash_core::GenerationOptions::default(),
        "an explicit replace still clears every option"
    );
}

/// The storeless half of the empty-drain contract: with no durable store the
/// queue does not exist at all, and the drain must say so by name. Reporting
/// `ExecutionLaneBusy` or `ClaimRefused` here would tell the host to retry or
/// abandon work that was never queued.
#[tokio::test]
pub(super) async fn an_automatic_drain_without_a_durable_queue_says_so() {
    let double = kernel_double(SEED + 13, lash_restate_test::ServerConfig::default()).await;
    let backend = double.lash_backend();
    let mut runtime = standard_runtime_with_transport(&backend, mock_provider(Vec::new())).await;
    let handler = double
        .open_handler(AdmittedScope::queue_drain(
            SessionId::from("root"),
            "storeless-drain",
        ))
        .await
        .expect("open the drain's handler");
    let drain = runtime
        .stream_next_queued_work(TurnOptions::new(CancellationToken::new(), handler.scoped()))
        .await
        .expect("a storeless drain still answers");
    handler.close().await.expect("close the drain's handler");
    assert!(
        matches!(
            drain,
            lash_core::facade_support::QueuedTurnDrain::Empty(
                lash_core::facade_support::EmptyQueuedDrainReason::NoDurableQueue
            )
        ),
        "a session with no durable store must report NoDurableQueue, got {drain:?}"
    );
}

#[tokio::test]
pub(super) async fn no_queued_work_submit_defers_without_refreshing_resident_state() {
    let double = kernel_double(SEED + 14, lash_restate_test::ServerConfig::default()).await;
    let (mut runtime, store) =
        standard_runtime_with_transport_and_double_queue_store(&double, mock_provider(Vec::new()))
            .await;
    let full_loads_before = store.load_session_count();
    let head_reads_before = store.load_session_head_meta_count();

    let receipt = runtime
        .submit_session_command(
            lash_core::facade_support::SessionCommand::RefreshToolCatalog {
                reason: "deferred queued lane".to_string(),
            },
            "deferred-queued-command",
        )
        .await
        .expect("NoSessionWork leaves the durable command pending");

    assert_eq!(store.load_session_count(), full_loads_before);
    assert_eq!(store.load_session_head_meta_count(), head_reads_before);
    let pending = lash_core::store::QueuedWorkStore::list_queued_work(
        store.as_ref(),
        &SessionId::from("root"),
    )
    .await
    .expect("inspect deferred durable command");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].batch_id, receipt.batch_id);
}

/// The drive path's test entry (FIG-3600): an idle session answers an empty
/// claim, a drain runs one root whose claim takes the claimable input prefix,
/// and a drain re-run under the same identity replays its recorded drive
/// instead of admitting anything new.
#[tokio::test]
pub(super) async fn the_drive_entry_runs_one_root_per_drain_and_replays_a_repeated_drain() {
    let backend = memory_backend().await;
    let answer = |text: &str| MockCall {
        stream_events: Vec::new(),
        response: Ok(LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: text.to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        }),
    };
    let (mut runtime, store) = standard_runtime_with_transport_and_queue_store(
        &backend,
        mock_provider(vec![answer("first answer")]),
    )
    .await;
    let session = SessionId::from("root");
    let drain = |id: &str| {
        TurnOptions::new(
            CancellationToken::new(),
            backend_queued_scope(&backend, &session, &TurnId::from(id)),
        )
    };

    let idle = runtime
        .drive_next_queued_root(drain("drive-entry-idle"))
        .await
        .expect("an idle drive answers");
    assert!(matches!(
        idle,
        lash_core::facade_support::QueuedTurnDrain::Empty(
            lash_core::facade_support::EmptyQueuedDrainReason::ClaimRefused(
                lash_core::QueuedWorkClaimRefusal::Empty
            )
        )
    ));

    enqueue_idle_turn_input(store.as_ref(), &session, "first question").await;
    enqueue_idle_turn_input(store.as_ref(), &session, "second question").await;
    let first = runtime
        .drive_next_queued_root(drain("drive-entry-first"))
        .await
        .expect("the first drain runs")
        .expect("the first drain runs a root");
    assert_eq!(first.assistant_output.safe_text, "first answer");

    let repeated = runtime
        .drive_next_queued_root(drain("drive-entry-first"))
        .await
        .expect("the repeated drain replays")
        .expect("the repeated drain answers its recorded root");
    assert_eq!(
        repeated.assistant_output.safe_text, "first answer",
        "a repeated drain replays its recorded root, never the next input"
    );

    let after = runtime
        .drive_next_queued_root(drain("drive-entry-second"))
        .await
        .expect("a later drain answers");
    assert!(
        after.ran().is_none(),
        "the first root's claim took both inputs, so a later drain has nothing to run"
    );
    assert_eq!(
        lash_core::store::TurnInputStore::list_turn_input_applications(store.as_ref(), &session)
            .await
            .expect("read the applied inputs")
            .len(),
        2,
        "each input is applied exactly once"
    );
}
