use super::*;
use lash_core::testing::TestTurnExecution as _;

#[tokio::test]
pub(super) async fn restate_handler_replay_retries_final_lash_commit_idempotently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "restate-final-commit-replay";
    let turn_id = "restate-turn-1";
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            move |_request| {
                let provider_calls = Arc::clone(&provider_calls);
                async move {
                    let call_index = provider_calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(
                        call_index, 0,
                        "Restate replay should return the recorded LLM effect"
                    );
                    Ok(lash_core::LlmResponse {
                        parts: vec![lash_core::LlmOutputPart::Text {
                            text: "committed once".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core::LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let mut host = memory_host_config().await;
    host.providers.models = lash_core::testing::standard_test_llm_profiles(provider);
    host.durability.attachment_store = Arc::new(
        lash_core::facade_support::RuntimeAttachmentStore::ephemeral(Arc::new(
            lash_core::facade_support::FileAttachmentStore::new(dir.path().join("attachments")),
        )),
    );
    let store = Arc::new(
        lash_sqlite_store::SqliteStore::open(&dir.path().join("store.db"))
            .await
            .expect("open session store"),
    );
    let runtime_store = session_view(store.clone(), session_id);
    let policy = lash_core::testing::mock_session_policy();
    let initial_state = replay_test_state(&SessionId::from(session_id), &policy);
    let context = Arc::new(ReplayableRecordingContext::default());
    bind_restate_test_effect_host(&mut host, &context);

    let mut first = replay_test_runtime(
        &SessionId::from(session_id),
        policy.clone(),
        initial_state.clone(),
        host.clone(),
        runtime_store.clone(),
    )
    .await;
    let first_turn = run_restate_replay_turn(
        &mut first,
        Arc::clone(&context),
        &SessionId::from(session_id),
        &TurnId::from(turn_id),
    )
    .await;
    assert!(matches!(
        first_turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    let first_runs = context.runs();
    assert!(!first_runs.is_empty());

    context.start_replay();
    let retry_store = decorated_view(&runtime_store, CommitRetryStore::new);
    let mut replay = replay_test_runtime(
        &SessionId::from(session_id),
        policy,
        initial_state,
        host,
        retry_store,
    )
    .await;
    let replay_turn = run_restate_replay_turn(
        &mut replay,
        Arc::clone(&context),
        &SessionId::from(session_id),
        &TurnId::from(turn_id),
    )
    .await;
    assert!(matches!(
        replay_turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(first_turn.llm_calls.len(), 1);
    assert_eq!(replay_turn.llm_calls, first_turn.llm_calls);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    let conn = rusqlite::Connection::open(dir.path().join("store.db"))
        .expect("open raw session sqlite store");
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runtime_turn_commits WHERE session_id = ?1 AND turn_id = ?2",
            rusqlite::params![
                session_id,
                lash_core::store::OperationId::turn(session_id, turn_id, "final")
                    .storage_key()
                    .expect("the final operation key")
            ],
            |row| row.get(0),
        )
        .expect("count turn commit stamps");
    assert_eq!(rows, 1);
}

/// A dropped suspended handler is redriven under a newer shift admission and
/// publishes one final commit even though its first provider attempt was lost.
#[tokio::test]
pub(super) async fn restate_replay_shift_seal_takes_recorded_branch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = "restate-replay-lease-branch";
    let turn_id = "restate-replay-lease-turn-1";
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let first_provider_started = Arc::new(tokio::sync::Notify::new());
    let provider = lash_core::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            let first_provider_started = Arc::clone(&first_provider_started);
            move |_| {
                let provider_calls = Arc::clone(&provider_calls);
                let first_provider_started = Arc::clone(&first_provider_started);
                async move {
                    let call_index = provider_calls.fetch_add(1, Ordering::SeqCst);
                    if call_index == 0 {
                        first_provider_started.notify_one();
                        std::future::pending::<()>().await;
                    }
                    Ok(lash_core::LlmResponse {
                        parts: vec![lash_core::LlmOutputPart::Text {
                            text: "fresh worker progressed".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..lash_core::LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let mut host = memory_host_config().await;
    host.providers.models = lash_core::testing::standard_test_llm_profiles(provider);
    host.durability.attachment_store = Arc::new(
        lash_core::facade_support::RuntimeAttachmentStore::ephemeral(Arc::new(
            lash_core::facade_support::FileAttachmentStore::new(dir.path().join("attachments")),
        )),
    );

    let store = Arc::new(
        lash_sqlite_store::SqliteStore::open(&dir.path().join("store.db"))
            .await
            .expect("open session store"),
    );
    let underlying_store = session_view(store.clone(), session_id);
    let admission_count = Arc::new(AtomicUsize::new(0));
    let probed_store = decorated_view(&underlying_store, |inner| CommitRetryStore {
        inner,
        admission_count: Arc::clone(&admission_count),
    });
    let runtime_store: lash_core::store::SessionStore = probed_store;
    let policy = lash_core::testing::mock_session_policy();
    let initial_state = replay_test_state(&SessionId::from(session_id), &policy);
    let context = Arc::new(ReplayableRecordingContext::default());
    bind_restate_test_effect_host(&mut host, &context);

    let mut suspended = replay_test_runtime(
        &SessionId::from(session_id),
        policy.clone(),
        initial_state.clone(),
        host.clone(),
        runtime_store.clone(),
    )
    .await;
    let suspended_context = Arc::clone(&context);
    let suspended_turn = tokio::spawn(async move {
        run_restate_replay_turn(
            &mut suspended,
            suspended_context,
            &SessionId::from(session_id),
            &TurnId::from(turn_id),
        )
        .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        first_provider_started.notified(),
    )
    .await
    .expect("first durable worker reaches provider suspension");
    assert_eq!(admission_count.load(Ordering::SeqCst), 1);
    assert!(
        !context.runs().is_empty(),
        "the suspended handler reached the real Restate run boundary"
    );
    suspended_turn.abort();
    assert!(
        suspended_turn
            .await
            .expect_err("dropped suspended handler future")
            .is_cancelled()
    );

    // The fresh worker is the dropped handler's retry: Restate replays the
    // journal the first attempt recorded, and the provider call it never
    // recorded runs live. A worker that cannot read that journal is a fresh
    // execution of a started run, which is SubstrateLost (ADR 0105 L-S8).
    context.start_replay_allowing_journal_extension();
    let mut fresh_worker = replay_test_runtime(
        &SessionId::from(session_id),
        policy,
        initial_state,
        host,
        runtime_store,
    )
    .await;
    let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let scoped_effect_controller = controller
        .scoped_effect_controller(durable_admission(&durable_turn_scope(session_id, turn_id)))
        .expect("scoped replay controller");
    let replay_turn = fresh_worker
        .execute_turn(
            replay_test_input(&TurnId::from(turn_id)),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scoped_effect_controller,
            ),
        )
        .await;

    let replay_turn = replay_turn.unwrap_or_else(|error| {
        panic!(
            "fresh durable worker must redrive under a new admission: \
             {error:?}; admissions={}",
            admission_count.load(Ordering::SeqCst)
        )
    });
    assert!(matches!(
        replay_turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(
        replay_turn.assistant_output.safe_text,
        "fresh worker progressed"
    );
    assert_eq!(
        admission_count.load(Ordering::SeqCst),
        1,
        "the fresh worker replays the recorded shift seal"
    );
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        2,
        "the dropped provider attempt is retried by the fresh worker"
    );

    let conn = rusqlite::Connection::open(dir.path().join("store.db"))
        .expect("open raw session sqlite store");
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runtime_turn_commits WHERE session_id = ?1 AND turn_id = ?2",
            rusqlite::params![
                session_id,
                lash_core::store::OperationId::turn(session_id, turn_id, "final")
                    .storage_key()
                    .expect("the final operation key")
            ],
            |row| row.get(0),
        )
        .expect("count liveness turn commit stamps");
    assert_eq!(rows, 1, "the fresh worker commits exactly once");
}

#[tokio::test]
pub(super) async fn restate_controller_schedules_process_workflow_without_running_executor() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let registry = process_registry();
    let registration =
        held_registration().with_start_key(Some(lash_core::StartKey::for_host("background-start")));
    let outcome = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "background-start"),
                RuntimeEffectCommand::process(ProcessCommand::Start {
                    registration: registration.into(),
                    observers: vec![SessionId::from("session")],
                    execution_context: Box::new(ProcessExecutionContext::default()),
                }),
            ),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("start");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record, .. },
    } = outcome
    else {
        panic!("wrong outcome");
    };
    let process_id = record.id.clone();

    assert_eq!(
        record
            .external_ref
            .as_ref()
            .map(|external| external.id.as_str()),
        Some(format!("LashProcessWorkflow/{process_id}").as_str())
    );
    assert_eq!(
        registry
            .get_process(&process_id)
            .await
            .expect("read process")
            .expect("get")
            .external_ref
            .as_ref()
            .map(|external| external.id.as_str()),
        Some(format!("LashProcessWorkflow/{process_id}").as_str())
    );
    assert_eq!(
        registry
            .list_observed_by(
                &SessionId::from("session"),
                &lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..Default::default()
                }
            )
            .await
            .expect("observed")
            .into_iter()
            .next()
            .and_then(|record| record.external_ref)
            .map(|external| (
                external.backend,
                external
                    .metadata
                    .and_then(|metadata| metadata.get("invocation_id").cloned())
            )),
        Some((
            "restate".to_string(),
            Some(serde_json::json!(format!("invocation-{process_id}")))
        ))
    );
    assert_eq!(
        context
            .started
            .lock_recover()
            .iter()
            .map(|registration| registration.start_key.clone())
            .collect::<Vec<_>>(),
        vec![Some(lash_core::StartKey::for_host("background-start"))]
    );
    // The start journals its frontier marker (FIG-3779), its registration
    // (ADR 0107) and, after the send, its external reference: each a run of
    // its own, and the workflow send a journaled command between them, never
    // a Restate call from inside a run.
    let runs = context.runs.lock_recover().clone();
    assert_eq!(runs.len(), 3, "the start's runs: {runs:?}");
    assert!(runs[0].ends_with(":frontier"), "{runs:?}");
    assert!(runs[1].contains("process-start-register"), "{runs:?}");
    assert!(runs[2].contains("process-start-external-ref"), "{runs:?}");
}

/// FIG-2964: a workflow-submission failure after registration cancels the row
/// it created, inside the scheduling boundary, before the error reaches the
/// caller.
///
/// Registration has already committed when the submit fails, so returning the
/// error bare would leave a Running row that Restate never received and that
/// nothing in the caller's cancel path can reach. A `StartFailed` request
/// against a row with no execution and no external reference is terminal on the
/// spot, so the child is cancelled and never runs.
#[tokio::test]
pub(super) async fn restate_workflow_submission_failure_cancels_the_row_it_registered() {
    let context = Arc::new(RecordingContext::default());
    context.fail_next_process_workflow_start();
    let host = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let stores = memory_process_stores().await;
    let registry = Arc::clone(&stores.registry);
    let env_store = Arc::clone(&stores.env_store);
    let start_key = "restate-start-failed-cancels";
    let spec = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        recovery_session_policy(),
    );
    let expected_ref = spec.stable_ref().expect("stable environment ref");

    let injected_error = host
        .execute_effect(
            start_recovery_effect(env_store.as_ref(), start_key, &spec).await,
            registry_local_executor(registry.clone()).with_process_env_store(env_store.clone()),
        )
        .await
        .expect_err("the submission failure must reach the caller as an error");

    let record = the_only_process(registry.as_ref()).await;
    assert!(
        record.is_terminal(),
        "the compensated row must be terminal, got {:?}; error: {injected_error}",
        record.status()
    );
    assert_eq!(
        record.status(),
        lash_core::ProcessStatus::Cancelled,
        "a StartFailed request against a never-started row is Cancelled"
    );
    assert_eq!(
        record
            .cancel_request
            .as_deref()
            .map(|request| request.origin),
        Some(lash_core::CancelOrigin::StartFailed),
        "the cancellation must name its origin so it is distinguishable from an operator cancel"
    );
    assert!(
        record.external_ref.is_none(),
        "a row Restate never accepted must carry no backend owner"
    );
    assert!(
        record.first_started.is_none(),
        "the compensated child must never have run"
    );
    // The inputs stay readable: the cancellation is a terminal fact about this
    // start, not a reclamation of the caller's committed input.
    assert!(
        env_store
            .get_process_execution_env(&expected_ref)
            .await
            .expect("read preserved start input")
            .is_some(),
    );
}

/// FIG-2964 acceptance: if the StartFailed compensation write itself fails, the
/// start returns the record rather than the error.
///
/// The row is then exactly the shape the `ProcessStart` obligation's relay
/// retries — nonterminal, no external reference, no cancel request — so the
/// obligation owns the start and the honest answer to the caller is the
/// record it registered.
#[tokio::test]
pub(super) async fn restate_failed_start_compensation_returns_the_registered_record() {
    let context = Arc::new(RecordingContext::default());
    context.fail_next_process_workflow_start();
    let host = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let stores = memory_process_stores().await;
    let registry = Arc::clone(&stores.registry);
    registry.fail_next_cancel_request(PluginError::Session(
        "injected cancel-request write failure".to_string(),
    ));
    let env_store = Arc::clone(&stores.env_store);
    let start_key = "restate-start-failed-compensation-fails";
    let spec = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        recovery_session_policy(),
    );

    let outcome = host
        .execute_effect(
            start_recovery_effect(env_store.as_ref(), start_key, &spec).await,
            registry_local_executor(registry.clone()).with_process_env_store(env_store.clone()),
        )
        .await
        .expect("a failed compensation write must return the record, not the error");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record, .. },
    } = outcome
    else {
        panic!("wrong start outcome")
    };
    assert_eq!(record.id, the_only_process(registry.as_ref()).await.id);

    let stored = the_only_process(registry.as_ref()).await;
    assert!(
        !stored.is_terminal(),
        "the uncompensated row must stay nonterminal so the start obligation can be redelivered"
    );
    assert!(
        stored.external_ref.is_none() && stored.cancel_request.is_none(),
        "the row must match the shape the start obligation's relay retries, got {stored:?}"
    );
}

/// A failure *after* the workflow was accepted is a different case: Restate
/// already owns the run, so the row is left for exact recovery rather than
/// cancelled. Only the external-reference write is missing, and repeating the
/// start completes the ownership transfer.
#[tokio::test]
pub(super) async fn restate_external_ref_write_failure_preserves_inputs_for_exact_recovery() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let stores = memory_process_stores().await;
    let registry = Arc::clone(&stores.registry);
    registry.fail_next_external_ref_write(PluginError::attempt_fault(
        "injected external-ref write failure".to_string(),
    ));
    let env_store = Arc::clone(&stores.env_store);
    let start_key = "restate-start-recovery-external-ref";
    let spec = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        recovery_session_policy(),
    );
    let expected_ref = spec.stable_ref().expect("stable environment ref");
    let executor =
        || registry_local_executor(registry.clone()).with_process_env_store(env_store.clone());

    // The reference write's step meets the fault, and the attempt ends there.
    let injected_error = context
        .attempt
        .run(host.execute_effect(
            start_recovery_effect(env_store.as_ref(), start_key, &spec).await,
            executor(),
        ))
        .await
        .expect_err("injected post-registration start failure")
        .failure;
    assert!(
        injected_error.contains("injected external-ref write failure"),
        "only the injected fault ends the attempt: {injected_error}"
    );
    let record = the_only_process(registry.as_ref()).await;
    assert!(
        !record.is_terminal(),
        "a row Restate already accepted must not be cancelled; error: {injected_error}"
    );
    assert!(
        env_store
            .get_process_execution_env(&expected_ref)
            .await
            .expect("read preserved start input")
            .is_some(),
        "a retriable external-ref failure must not reclaim the committed process input"
    );

    let outcome = host
        .execute_effect(
            start_recovery_effect(env_store.as_ref(), start_key, &spec).await,
            executor(),
        )
        .await
        .expect("exact start retry completes ownership transfer");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record, .. },
    } = outcome
    else {
        panic!("wrong recovery outcome")
    };
    assert_eq!(record.env_ref.as_ref(), Some(&expected_ref));
    assert!(record.external_ref.is_some());
}

/// FIG-2964: an ambiguous submission failure does not compensate.
///
/// A failure carrying no proof of non-acceptance — a dropped connection, a
/// reply that never arrived — may have left an invocation running. Writing the
/// StartFailed terminal there would terminalise a row whose workflow is doing
/// the child's work, and the workflow's own terminal write would then fail
/// against the row it was supposed to settle. The row is left alive and
/// obligation-owned, and the start returns the record.
#[tokio::test]
pub(super) async fn restate_ambiguous_submission_failure_leaves_the_row_for_recovery() {
    let context = Arc::new(RecordingContext::default());
    context.fail_next_process_workflow_start_ambiguously();
    let host = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let stores = memory_process_stores().await;
    let registry = Arc::clone(&stores.registry);
    let env_store = Arc::clone(&stores.env_store);
    let start_key = "restate-start-ambiguous";
    let spec = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        recovery_session_policy(),
    );

    let outcome = host
        .execute_effect(
            start_recovery_effect(env_store.as_ref(), start_key, &spec).await,
            registry_local_executor(registry.clone()).with_process_env_store(env_store.clone()),
        )
        .await
        .expect("an ambiguous failure must return the record, not the error");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record, .. },
    } = outcome
    else {
        panic!("wrong start outcome")
    };
    assert_eq!(record.id, the_only_process(registry.as_ref()).await.id);

    let stored = the_only_process(registry.as_ref()).await;
    assert!(
        !stored.is_terminal(),
        "a submission that may be running must not be terminalised, got {:?}",
        stored.status()
    );
    assert!(
        stored.cancel_request.is_none(),
        "no cancellation may be recorded for a submission whose fate is unknown"
    );
    assert!(stored.external_ref.is_none());
}

/// FIG-2964: an exact retry whose submission fails must not cancel the row the
/// first attempt registered.
///
/// Registration is idempotent by start key on every backend, so the second
/// call's registration succeeds by returning the first call's row. Treating
/// that as "I created this" would let a retry write a terminal onto a row whose
/// first attempt may already be running.
#[tokio::test]
pub(super) async fn restate_exact_retry_start_failure_does_not_cancel_the_first_attempts_row() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
    let stores = memory_process_stores().await;
    let registry = Arc::clone(&stores.registry);
    let env_store = Arc::clone(&stores.env_store);
    let start_key = "restate-exact-retry-start-failure";
    let spec = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        recovery_session_policy(),
    );
    let executor =
        || registry_local_executor(registry.clone()).with_process_env_store(env_store.clone());

    // The first attempt reaches Restate and registers the row, but its
    // external-reference write fails, so the row it leaves is nonterminal with
    // no reference — exactly the shape the second attempt's compensation would
    // find terminalisable.
    registry.fail_next_external_ref_write(PluginError::attempt_fault(
        "injected external-ref write failure".to_string(),
    ));
    let ended = context
        .attempt
        .run(host.execute_effect(
            start_recovery_effect(env_store.as_ref(), start_key, &spec).await,
            executor(),
        ))
        .await
        .expect_err("the first attempt's reference write fails");
    assert!(
        ended
            .failure
            .contains("injected external-ref write failure"),
        "only the injected fault ends the attempt: {ended:?}"
    );
    let first = the_only_process(registry.as_ref()).await;
    assert!(first.external_ref.is_none() && !first.is_terminal());

    // The second attempt is an exact repeat: registration returns the existing
    // row, and this submission is definitively refused.
    context.fail_next_process_workflow_start();
    let outcome = host
        .execute_effect(
            start_recovery_effect(env_store.as_ref(), start_key, &spec).await,
            executor(),
        )
        .await
        .expect("a retry that did not create the row returns it rather than cancelling it");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record, .. },
    } = outcome
    else {
        panic!("wrong start outcome")
    };
    assert_eq!(record.id, the_only_process(registry.as_ref()).await.id);

    let stored = the_only_process(registry.as_ref()).await;
    assert!(
        !stored.is_terminal(),
        "a retry must never terminalise the row an earlier attempt created, got {:?}",
        stored.status()
    );
    assert!(
        stored.cancel_request.is_none(),
        "a retry must record no cancellation against the first attempt's row"
    );
    assert_eq!(stored.id, first.id);
}

/// The effect name [`start_recovery_effect`] journals its steps under.
pub(super) fn start_recovery_effect_name(start_key: &str) -> String {
    restate_effect_name(&runtime_invocation(RuntimeEffectKind::Process, start_key))
}

pub(super) async fn start_recovery_effect(
    env_store: &dyn lash_core::ProcessExecutionEnvStore,
    start_key: &str,
    spec: &lash_core::ProcessExecutionEnvSpec,
) -> RuntimeEffectEnvelope {
    let key = lash_core::StartKey::for_host(start_key);
    let claim = lash_core::ReferrerClaim::guarded(lash_core::ReferrerGuard::Journal(
        runtime_invocation(RuntimeEffectKind::Process, start_key)
            .execution_scope()
            .journal_identity()
            .expect("starter journal"),
    ));
    let env_ref = lash_core::publish_process_execution_env(env_store, &claim, spec)
        .await
        .expect("publish start environment");
    let registration = ProcessRegistration::new(
        ProcessInput::Engine {
            kind: "testing-fixture".to_string(),
            payload: serde_json::Value::Null,
        },
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_start_key(Some(key))
    .with_execution_env_ref(Some(env_ref));
    RuntimeEffectEnvelope::new(
        runtime_invocation(RuntimeEffectKind::Process, start_key),
        RuntimeEffectCommand::process(ProcessCommand::Start {
            registration: registration.into(),
            observers: vec![SessionId::from("session")],
            execution_context: Box::new(ProcessExecutionContext::default()),
        }),
    )
}

/// The one row a start-failure law registered: its id was minted inside the
/// failed start, so the law reads it back from the registry.
pub(super) async fn the_only_process(registry: &dyn ProcessRegistry) -> lash_core::ProcessRecord {
    let mut records = registry
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..Default::default()
        })
        .await
        .expect("list registered processes");
    assert_eq!(records.len(), 1, "the law registers exactly one process");
    records.remove(0)
}

#[tokio::test]
pub(super) async fn restate_controller_replays_process_start_await_command_sequence() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let registry = process_registry();

    let start = || {
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "process-start-replay"),
            RuntimeEffectCommand::process(ProcessCommand::Start {
                registration: held_registration()
                    .with_start_key(Some(lash_core::StartKey::for_host("process-start-replay")))
                    .into(),
                observers: Vec::new(),
                execution_context: Box::new(ProcessExecutionContext::default()),
            }),
        )
    };
    let terminal = process_success(serde_json::json!({ "done": true }));

    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record, .. },
    } = host
        .execute_effect(start(), registry_local_executor(registry.clone()))
        .await
        .expect("first start")
    else {
        panic!("the start must report the started process");
    };
    let process_id = record.id;
    let await_terminal = || {
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "process-await-replay"),
            RuntimeEffectCommand::process(ProcessCommand::Await {
                process_id: process_id.clone(),
            }),
        )
    };
    registry
        .complete_process(
            &process_id,
            terminal.clone(),
            lash_core::ProcessCompletionAuthority::workflow_key(&process_id),
        )
        .await
        .expect("complete child process");
    context.resolve_process_terminal(&process_id, &terminal);
    host.execute_effect(await_terminal(), registry_local_executor(registry.clone()))
        .await
        .expect("first await");

    // Simulates Restate replay of the same parent handler after a later
    // suspension resumes. The already persisted registry record has an
    // external_ref at this point. The start still sends to its recorded id;
    // the terminal await returns its recorded observation without an attach.
    host.execute_effect(start(), registry_local_executor(registry.clone()))
        .await
        .expect("replay start");
    host.execute_effect(await_terminal(), registry_local_executor(registry.clone()))
        .await
        .expect("replay await");

    assert_eq!(
        context.process_command_log.lock_recover().as_slice(),
        &[format!("send:{process_id}"), format!("send:{process_id}"),],
        "the start replays its send and terminal awaits issue no attach"
    );
}

fn attach_key(key_id: &str) -> lash_core::AwaitEventKey {
    lash_core::AwaitEventKey {
        scope: lash_core::ExecutionScope::turn("session", "turn"),
        wait: lash_core::AwaitEventWaitIdentity::ToolCompletion {
            tool_call_id: lash_core::ToolCallId::fixture(&format!("{key_id}-call")),
        },
        key_id: key_id.to_string(),
        signature: format!("{key_id}-signature"),
    }
}

/// An attach for a process the registry never registered must refuse: arming a
/// waiter on an unknown process would park the caller on a wait nothing can
/// ever resolve.
#[tokio::test]
pub(super) async fn restate_controller_refuses_attach_for_an_unregistered_process() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let registry = process_registry();
    let process_id = ProcessId::fixture("task-attach-unknown");

    let error = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "process-attach-unknown"),
                RuntimeEffectCommand::process(ProcessCommand::AttachTerminal {
                    process_id,
                    key: attach_key("attach-unknown"),
                }),
            ),
            registry_local_executor(registry),
        )
        .await
        .expect_err("an attach on an unregistered process must refuse");
    assert!(
        context.process_attachments.lock_recover().is_empty(),
        "a refused attach must not have sent an arming: {error}"
    );
}

/// FIG-3611 L5 on Restate: a start under a key whose process was pruned starts
/// a new process, and its workflow is keyed by the new minted id, so Restate
/// never coalesces it onto the pruned run's workflow (ADR 0107).
///
/// Before the minted id the workflow key was the host-chosen process name, so
/// the restart's send addressed the pruned run's workflow and Restate
/// coalesced it onto the finished invocation.
#[tokio::test]
pub(super) async fn restate_controller_start_after_prune_sends_a_new_workflow() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let registry = process_registry();
    let start = |effect_id: &'static str| {
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, effect_id),
            RuntimeEffectCommand::process(ProcessCommand::Start {
                registration: held_registration()
                    .with_start_key(Some(lash_core::StartKey::for_host("restart-after-prune")))
                    .into(),
                observers: Vec::new(),
                execution_context: Box::new(ProcessExecutionContext::default()),
            }),
        )
    };
    let started = |outcome: RuntimeEffectOutcome| {
        let RuntimeEffectOutcome::Process {
            result: ProcessEffectOutcome::Start { record, .. },
        } = outcome
        else {
            panic!("a start reports its process");
        };
        record.id
    };

    let first = started(
        host.execute_effect(
            start("first-start"),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("first start"),
    );
    registry
        .complete_process(
            &first,
            process_success(serde_json::json!({ "run": "first" })),
            lash_core::ProcessCompletionAuthority::workflow_key(&first),
        )
        .await
        .expect("complete the first run");
    registry
        .prune_terminal_processes(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect("prune the first run");

    let restarted = started(
        host.execute_effect(start("restart"), registry_local_executor(registry.clone()))
            .await
            .expect("restart under the same key"),
    );
    assert_ne!(restarted, first, "the restart is minted a new id");
    assert_eq!(
        context.process_command_log.lock_recover().as_slice(),
        &[format!("send:{first}"), format!("send:{restarted}")],
        "the restart sends its own workflow, never the pruned run's"
    );
}

#[tokio::test]
pub(super) async fn restate_controller_start_emits_send_when_external_ref_already_exists() {
    let context = Arc::new(RecordingContext::default());
    let host = RestateRuntimeEffectController::new_for_test(context.clone());
    let registry = process_registry();
    let registration = held_registration().with_start_key(Some(lash_core::StartKey::for_host(
        "process-start-existing-ref",
    )));
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register process")
        .id;
    registry
        .set_external_ref(
            &process_id,
            ProcessExternalRef {
                backend: "restate".to_string(),
                id: format!("LashProcessWorkflow/{process_id}"),
                metadata: Some(serde_json::json!({
                    "invocation_id": format!("invocation-{process_id}")
                })),
                segment_ordinal: None,
            },
        )
        .await
        .expect("pre-set external ref");

    host.execute_effect(
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "process-start-existing-ref"),
            RuntimeEffectCommand::process(ProcessCommand::Start {
                registration: registration.into(),
                observers: Vec::new(),
                execution_context: Box::new(ProcessExecutionContext::default()),
            }),
        ),
        registry_local_executor(registry),
    )
    .await
    .expect("start with existing external ref");

    assert_eq!(
        context.process_command_log.lock_recover().as_slice(),
        &[format!("send:{process_id}")],
        "pre-existing external_ref must not suppress the journaled Restate send"
    );
}

pub(super) async fn run_parent_shaped_start_await_suspend_flow(
    host: &RestateRuntimeEffectController<'_, Arc<RecordingContext>>,
    registry: Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
    suspend_key: AwaitEventKey,
) {
    let started = host
        .execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "parent-flow-start-child"),
                RuntimeEffectCommand::process(ProcessCommand::Start {
                    registration: held_registration()
                        .with_start_key(Some(lash_core::StartKey::for_host("parent-flow-child")))
                        .into(),
                    observers: Vec::new(),
                    execution_context: Box::new(ProcessExecutionContext::default()),
                }),
            ),
            registry_local_executor(registry.clone()),
        )
        .await
        .expect("parent flow start child");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record, .. },
    } = started
    else {
        panic!("parent flow start must report the started child");
    };
    assert_eq!(
        &record.id, process_id,
        "every replay of the keyed start must name the one minted child"
    );

    host.execute_effect(
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::Process, "parent-flow-await-child"),
            RuntimeEffectCommand::process(ProcessCommand::Await {
                process_id: process_id.clone(),
            }),
        ),
        registry_local_executor(registry),
    )
    .await
    .expect("parent flow await child");

    host.execute_effect(
        RuntimeEffectEnvelope::new(
            runtime_invocation(RuntimeEffectKind::AwaitEvent, "parent-flow-suspend"),
            RuntimeEffectCommand::AwaitEvent { key: suspend_key },
        ),
        RuntimeEffectLocalExecutor::await_event(tokio_util::sync::CancellationToken::new())
            .with_turn_cancel_scope(durable_turn_scope("session", "turn")),
    )
    .await
    .expect("parent flow await resume event");
}
