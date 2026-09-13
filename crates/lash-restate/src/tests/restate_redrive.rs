use super::*;

#[tokio::test]
async fn fig1128_deadline_wire_typed_refusal_and_no_deadline_shape() {
    let key = restate_await_event_key(
        &durable_turn_scope("fig1128-wire-session", "fig1128-wire-turn"),
        AwaitEventWaitIdentity::tool_completion("fig1128-wire"),
    )
    .expect("derive durable-wait key");
    let no_deadline = crate::durable_wait::restate_durable_wait_request(
        &key,
        None,
        &lash_core::facade_support::SystemClock,
    );
    assert_eq!(
        serde_json::to_value(crate::durable_wait::RestateDurableWaitAwaitInput::from(
            no_deadline,
        ))
        .expect("serialize no-deadline workflow input"),
        serde_json::json!({ "key": key }),
        "the v2 cutover must not move the shipped no-deadline wire shape"
    );

    let predecessor = serde_json::json!({
        "key": key.clone(),
        "timeout_ms": 30_000,
    });
    let endpoint = Endpoint::builder()
        .bind(LashDurableWaitWorkflowImpl.serve())
        .build();
    let refused = invoke_endpoint(
        &endpoint,
        "LashDurableWaitWorkflow",
        "await_resolution",
        &RestateDurableWaitAddress::for_key(&key).workflow_key,
        &predecessor,
    )
    .await
    .expect("invoke predecessor request against the deployed handler");
    let error = restate_output_failure_message(&refused)
        .expect("the predecessor request must return a terminal handler refusal");
    assert!(
        error.contains("predecessor field `timeout_ms` is incompatible with version 2")
            && error.contains("drain deadline-bearing waits before opening this deployment"),
        "the v1 refusal must be typed and name the drain requirement: {error}"
    );
    assert!(
        !error.contains("unknown field") && !error.contains("failed to deserialize"),
        "the v1 refusal must come from the durable-wait compatibility boundary, not generic serde: {error}"
    );

    let incompatible = crate::durable_wait::RestateDurableWaitDeadline {
        version: 1,
        unix_epoch_ms: 1_800_000_030_000,
    };
    let error = incompatible
        .remaining(1_800_000_000_000)
        .expect_err("a stamped predecessor deadline must be refused");
    assert!(
        error.to_string().contains("version 1 is incompatible"),
        "the stamped-version refusal must identify the incompatibility: {error}"
    );
}

#[tokio::test]
pub(super) async fn fig1128_await_event_resolver_journals_deadline_at_production_entry_point() {
    let context = Arc::new(ReplayableRecordingContext::default());
    let key = restate_await_event_key(
        &durable_turn_scope("fig1128-resolver-session", "fig1128-resolver-turn"),
        AwaitEventWaitIdentity::tool_completion("fig1128-resolver"),
    )
    .expect("derive resolver wait key");
    let resolution = Resolution::Ok(serde_json::json!({ "resolver": "stable" }));
    context
        .events
        .resolve_durable_event(RestateDurableWaitResolveRequest {
            key: key.clone(),
            resolution: resolution.clone(),
        });
    let journal_name = format!("lash:durable-wait-deadline:v2:{}", key.key_id);
    let controller = RestateRuntimeEffectController::new(Arc::clone(&context));

    let recorded = controller
        .await_await_event(
            &key,
            tokio_util::sync::CancellationToken::new(),
            Some(std::time::Instant::now() + Duration::from_secs(60)),
        )
        .await
        .expect("record resolver deadline");
    assert_eq!(recorded, resolution);

    context.replaying.store(true, Ordering::SeqCst);
    let replayed = controller
        .await_await_event(
            &key,
            tokio_util::sync::CancellationToken::new(),
            Some(std::time::Instant::now() + Duration::from_secs(120)),
        )
        .await
        .expect("replay resolver deadline from journal");
    assert_eq!(replayed, resolution);
    assert_eq!(
        context.runs.lock_recover().as_slice(),
        [journal_name.as_str(), journal_name.as_str()],
        "both resolver attempts must cross the production deadline journal seam"
    );
    assert_eq!(
        context.records.lock_recover().len(),
        1,
        "redrive must reuse the first absolute deadline instead of recording a second payload"
    );
}

#[tokio::test]
pub(super) async fn fig1128_deadline_wait_redrive_reuses_the_first_payload() {
    const FIRST_WALL_MS: u64 = 1_800_000_000_000;
    let clock = Arc::new(Fig1128DeadlineClock::new(FIRST_WALL_MS, 100));
    let endpoint = Endpoint::builder()
        .bind(
            Fig1128DeadlineRedriveImpl {
                clock: Arc::clone(&clock),
            }
            .serve(),
        )
        .build();
    let workflow_key = "fig1128-deadline-redrive";
    let input = Fig1128DeadlineRedriveInput;

    let first = invoke_endpoint_with_named_call_responses(
        &endpoint,
        "Fig1128DeadlineRedrive",
        "run",
        workflow_key,
        &input,
        Vec::new(),
    )
    .await
    .expect("the first deadline-bearing wait must park");
    let first_calls = restate_call_frames(&first).expect("decode the first wait call");
    let [first_wait] = first_calls.as_slice() else {
        panic!("the first attempt must emit exactly one durable-wait call");
    };
    assert_eq!(first_wait.handler, "await_resolution");

    // A replacement process starts later and spends a different amount of
    // monotonic time building the same logical wait. The captured command is
    // the first process's journal fact; emitting a freshly derived relative
    // timeout against it is the FIG-1128 RT0016 failure.
    clock.begin_attempt(FIRST_WALL_MS + 10_000, 200);
    let terminal = Resolution::Ok(serde_json::json!({ "redrive": "stable" }));
    let terminal_json = serde_json::to_value(&terminal).expect("serialize terminal resolution");
    let replay = if restate_command_frame_types(&first).contains(&RESTATE_RUN_COMMAND_MESSAGE_TYPE)
    {
        encode_captured_run_and_call_replay(
            workflow_key,
            &input,
            &first,
            &[("await_resolution".to_string(), terminal_json)],
        )
        .expect("encode the journaled deadline and wait call")
    } else {
        // Mutation-control compatibility: a call site bypassing the deadline
        // journal has no RunCommand, only its clock-derived call payload.
        // Replaying that exact command fails this test with differing absolute
        // deadline values.
        encode_call_replay(
            workflow_key,
            &input,
            &[(first_wait.clone(), Some(terminal_json))],
            None,
        )
        .expect("encode the pre-fix wait journal")
    };
    let redriven = invoke_endpoint_body(&endpoint, "Fig1128DeadlineRedrive", "run", replay)
        .await
        .expect("redrive the deadline-bearing wait");
    assert!(
        restate_error_message(&redriven).is_none(),
        "deadline redrive must reuse its first payload instead of raising RT0016: {:?}",
        restate_error_message(&redriven)
    );
    assert_eq!(restate_output_json::<Resolution>(&redriven), Some(terminal));
}

#[tokio::test]
pub(super) async fn fig1126_pending_tool_redrives_after_worker_loss_and_resumes_once() {
    let tool_launches = Arc::new(AtomicUsize::new(0));
    let terminal_resumes = Arc::new(AtomicUsize::new(0));
    let endpoint = Endpoint::builder()
        .bind(
            Fig1126PendingToolRedriveImpl {
                tool_launches: Arc::clone(&tool_launches),
                terminal_resumes: Arc::clone(&terminal_resumes),
            }
            .serve(),
        )
        .build();
    let workflow_key = "fig1126-process-loss-redrive";
    let input = Fig1126PendingToolRedriveInput;

    let parked = invoke_endpoint_with_named_call_responses(
        &endpoint,
        "Fig1126PendingToolRedrive",
        "run",
        workflow_key,
        &input,
        vec![("is_revoked".to_string(), serde_json::json!(false))],
    )
    .await
    .expect("the initial worker incarnation must park on the completion key");
    assert_eq!(tool_launches.load(Ordering::SeqCst), 1);
    assert_eq!(terminal_resumes.load(Ordering::SeqCst), 0);
    assert!(
        restate_message_types(&parked)
            .expect("decode parked attempt")
            .contains(&RESTATE_SUSPENSION_MESSAGE_TYPE),
        "the first worker incarnation must suspend: error={:?}",
        restate_error_message(&parked)
    );
    let parked_calls = restate_call_frames(&parked).expect("decode parked call commands");
    assert_eq!(
        parked_calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["is_revoked", "await_resolution", "register_awakeable"],
        "the fixture must park through the production await-event/cancellation controller path"
    );

    let terminal = Resolution::Ok(serde_json::json!({ "answer": "resumed" }));
    let completions = parked_calls
        .iter()
        .map(|call| {
            let completion = match call.handler.as_str() {
                "is_revoked" => serde_json::json!(false),
                "await_resolution" => serde_json::to_value(terminal.clone())
                    .expect("serialize FIG-1126 wait resolution"),
                "register_awakeable" => {
                    serde_json::to_value(RestateDurableWaitRegistration::Registered)
                        .expect("serialize FIG-1126 gate registration")
                }
                other => panic!("unexpected FIG-1126 command `{other}`"),
            };
            (call.handler.clone(), completion)
        })
        .collect::<Vec<_>>();
    let replay = encode_captured_run_and_call_replay(workflow_key, &input, &parked, &completions)
        .expect("splice the exact parked journal and resolved completion key");

    // A second endpoint invocation is a fresh handler incarnation: it has no
    // first worker stack or in-memory future, only the captured journal and the
    // durable completion. This is the in-crate process-loss/redrive seam.
    // The winning event retires its gate entry, so the redrive needs that one
    // further index response before it can produce output.
    let redriven = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig1126PendingToolRedrive",
        "run",
        replay,
        vec![serde_json::Value::Null],
    )
    .await
    .expect("redrive the parked turn after worker loss");
    assert!(
        restate_error_message(&redriven).is_none(),
        "redrive must accept the exact first-incarnation command journal: {:?}",
        restate_error_message(&redriven)
    );
    assert_eq!(restate_output_json::<Resolution>(&redriven), Some(terminal));
    assert_eq!(
        tool_launches.load(Ordering::SeqCst),
        1,
        "journal replay must not launch the pending tool a second time"
    );
    assert_eq!(
        terminal_resumes.load(Ordering::SeqCst),
        1,
        "the resolved completion must execute the terminal continuation exactly once"
    );
}

#[tokio::test]
pub(super) async fn fig1126_revoked_await_refuses_before_command_on_first_execution_and_redrive() {
    let endpoint = Endpoint::builder()
        .bind(Fig1126RevokedAwaitBoundaryImpl.serve())
        .build();
    let workflow_key = "fig1126-revoked-await-boundary"; // gitleaks:allow -- synthetic workflow/turn identity fixture
    let input = Fig1126PendingToolRedriveInput;

    let first = invoke_endpoint_with_named_call_responses(
        &endpoint,
        "Fig1126RevokedAwaitBoundary",
        "run",
        workflow_key,
        &input,
        vec![("is_revoked".to_string(), serde_json::json!(true))],
    )
    .await
    .expect("revoked await must return a typed refusal");
    let calls = restate_call_frames(&first).expect("decode revoked await calls");
    assert_eq!(
        calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["is_revoked"],
        "first execution must refuse before the await-event gate"
    );
    assert!(
        restate_output_failure_message(&first)
            .is_some_and(|failure| failure.contains("await_event_unknown_or_revoked")),
        "first execution must preserve the typed revoked-session refusal"
    );

    let replay = encode_call_replay(
        workflow_key,
        &input,
        &[(calls[0].clone(), Some(serde_json::json!(true)))],
        None,
    )
    .expect("splice the refusing revocation journal");
    let redriven = invoke_endpoint_body(&endpoint, "Fig1126RevokedAwaitBoundary", "run", replay)
        .await
        .expect("redrive the refusing revocation journal");
    assert!(
        restate_call_frames(&redriven)
            .expect("decode revoked await redrive calls")
            .is_empty(),
        "redrive must append no await-event gate command after refusal"
    );
    assert!(
        restate_output_failure_message(&redriven)
            .is_some_and(|failure| failure.contains("await_event_unknown_or_revoked")),
        "redrive must preserve the typed revoked-session refusal"
    );
}

/// Worker-replacement replay proof: splice the captured first-incarnation run
/// into a replacement whose reconstructed envelope has changed, then inspect
/// the error exactly as the Restate host renders it.
#[tokio::test]
pub(super) async fn worker_replacement_mid_turn_surfaces_typed_abort_from_replayed_effect() {
    let model_version = Arc::new(AtomicUsize::new(1));
    let endpoint = Endpoint::builder()
        .bind(
            Fig1142ReplayDivergenceImpl {
                model_version: Arc::clone(&model_version),
            }
            .serve(),
        )
        .build();
    let workflow_key = "fig1142-rendered-divergence";
    let input = Fig1142ReplayDivergenceInput;
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig1142ReplayDivergence",
        "run",
        workflow_key,
        &input,
    )
    .await
    .expect("capture the first-incarnation runtime-effect run");
    assert!(
        restate_message_types(&suspended)
            .expect("decode first-incarnation frames")
            .contains(&RESTATE_SUSPENSION_MESSAGE_TYPE),
        "the fixture must suspend with its runtime-effect run unresolved"
    );

    let recorded = RecordedRuntimeEffect {
        envelope: Arc::new(
            fig1142_llm_envelope(1)
                .canonical_form()
                .expect("canonical first-incarnation envelope"),
        ),
        outcome: Ok(fig793_llm_outcome()),
    };
    let replay = encode_run_replay(
        workflow_key,
        &input,
        &suspended,
        serde_json::to_value(recorded).expect("serialize first-incarnation effect"),
    )
    .expect("splice the first-incarnation runtime-effect run");

    model_version.store(2, Ordering::SeqCst);
    let redriven = invoke_endpoint_body(&endpoint, "Fig1142ReplayDivergence", "run", replay)
        .await
        .expect("the divergent redrive must render a terminal output failure");
    let rendered = restate_output_failure_message(&redriven)
        .expect("the Restate host must render the replay-divergence failure");
    assert!(
        rendered.contains("worker_replacement_abort"),
        "rendered failure omitted the typed replacement-abort code: {rendered}"
    );
    assert!(
        rendered.contains("divergent_paths=[command.request.model]"),
        "rendered failure omitted the per-path divergence summary: {rendered}"
    );
}

/// FIG-779 repro. A not-yet-completed durable timer is an SDK-legitimate
/// synchronous-wake-then-Pending state — it is exactly how the SDK signals a
/// durable suspension. `RestateContextFuture` must fuse that resolved inner
/// future and yield so the SDK's handler-state wrapper writes the suspension.
///
/// This drives the real Restate endpoint, context, VM, and `ctx.sleep()`
/// future. The invocation body is complete (no further frames), so the VM's
/// input is closed; `DoProgress` then hits its suspension condition, the sleep
/// resolves as `Err(Suspended)`, `DurableFutureImpl` records the state, wakes
/// synchronously and returns `Pending`. Restate closes the request stream the
/// same way whenever it parks an invocation on a pending timer.
#[tokio::test]
pub(super) async fn fig779_pending_durable_timer_suspends_through_guard() {
    let endpoint = Endpoint::builder()
        .bind(Fig779TimerGuardReproImpl.serve())
        .build();

    let output = invoke_endpoint(
        &endpoint,
        "Fig779TimerGuardRepro",
        "run",
        "fig779-timer",
        &Fig779TimerGuardReproInput { duration_ms: 2_000 },
    )
    .await
    .expect("pending durable timer invocation should suspend without panicking");
    let message_types = restate_message_types(&output).expect("decode Restate response frames");
    assert_eq!(
        message_types,
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ],
        "the attempt must end as a suspension, not a failed-attempt conversion"
    );
}

/// Once the SDK records suspension, its one-shot handler state is terminal for
/// the attempt. A cancellation made ready by that same synchronous wake must
/// not let the sibling race return `Cancelled` before the SDK consumes the
/// suspension. Conversely, cancellation observed while the timer is genuinely
/// pending and unfused wins after the timer command has been journaled.
#[tokio::test]
pub(super) async fn fig779_sleep_suspension_and_cancellation_preserve_recorded_precedence() {
    let endpoint = Endpoint::builder()
        .bind(Fig779TimerGuardReproImpl.serve())
        .build();
    let input = Fig779TimerGuardReproInput { duration_ms: 2_000 };

    let suspended = invoke_endpoint(
        &endpoint,
        "Fig779TimerGuardRepro",
        "cancel_on_suspend_wake",
        "fig779-cancel-on-suspend",
        &input,
    )
    .await
    .expect("same-poll cancellation must preserve the recorded suspension");
    assert_eq!(
        restate_message_types(&suspended).expect("decode suspended race frames"),
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );

    let cancelled = invoke_endpoint_open(
        &endpoint,
        "Fig779TimerGuardRepro",
        "cancel_before_sleep",
        "fig779-cancel-before-sleep",
        &input,
    )
    .await
    .expect("pre-existing cancellation should complete after journaling the timer");
    assert_eq!(
        restate_message_types(&cancelled).expect("decode cancelled race frames"),
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE,
            RESTATE_END_MESSAGE_TYPE
        ]
    );
}

/// FIG-2499: a process handler records its process-scope effect in the
/// scope's index before journaling it, so the deployed first attempt parks
/// on the `begin_effect` call; once the index admits the effect the attempt
/// journals its timer and parks on that. Returns both legs' output, whose
/// commands are the deployed journal: the call, then the timer.
pub(super) async fn park_process_on_its_timer(
    endpoint: &Endpoint,
    process_id: &ProcessId,
    input: &RestateProcessWorkflowInput,
) -> Vec<u8> {
    let recording = invoke_endpoint(endpoint, "LashProcessWorkflow", "run", process_id, input)
        .await
        .expect("first process attempt should park on recording its effect");
    let calls = restate_call_frames(&recording).expect("decode effect-recording calls");
    assert_eq!(
        calls
            .iter()
            .map(|call| (call.service.as_str(), call.handler.as_str()))
            .collect::<Vec<_>>(),
        vec![("LashDurableWaitIndex", "begin_effect")],
        "the effect is recorded in the scope's index before its timer is journaled"
    );
    assert_eq!(
        restate_message_types(&recording).expect("decode recording frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
    let admitted = encode_call_replay(
        process_id,
        input,
        &[(calls[0].clone(), Some(serde_json::json!(true)))],
        None,
    )
    .expect("splice the admitted effect recording");
    let parked = invoke_endpoint_body(endpoint, "LashProcessWorkflow", "run", admitted)
        .await
        .expect("admitted process attempt should park on its timer");
    assert_eq!(
        restate_message_types(&parked).expect("decode parked process frames"),
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
    let mut journal = recording.to_vec();
    journal.extend_from_slice(&parked);
    journal
}

/// Completions for a replayed process journal: the scope index answers its
/// effect-recording calls, every other call answers `null`, and the timer is
/// fired or left pending.
pub(super) fn process_journal_completion(
    fire_timer: bool,
) -> impl Fn(&endpoint_protocol::RecordedCommand) -> Option<serde_json::Value> {
    move |command| match command.message_type {
        RESTATE_SLEEP_COMMAND_MESSAGE_TYPE => fire_timer.then_some(serde_json::Value::Null),
        RESTATE_CALL_COMMAND_MESSAGE_TYPE => command.call.as_ref().map(|(service, handler)| {
            durable_wait_index_call_response(service, handler).unwrap_or(serde_json::Value::Null)
        }),
        _ => None,
    }
}

/// Complete the effect-recording calls around each replayed trigger delivery
/// and its terminal delivery-sink call.
fn trigger_journal_completion(
    command: &endpoint_protocol::RecordedCommand,
) -> Option<serde_json::Value> {
    let (service, handler) = command.call.as_ref()?;
    durable_wait_index_call_response(service, handler).or_else(|| {
        (handler == "complete" && service == "Fig806TriggerSink").then_some(serde_json::Value::Null)
    })
}

#[tokio::test]
pub(super) async fn fig779_suspended_process_redrive_observes_durable_cancellation() {
    let process_id = "fig779-durable-cancel-redrive";
    let registry = process_registry();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register redrive process");
    let cancel_ingress = RestateIngressClient::new(RestateConnection::with_transport(
        "https://restate.invalid",
        Arc::new(Fig779DurableCancelTransport {
            registry: Arc::clone(&registry),
            process_id: ProcessId::from(process_id.to_string()),
        }),
    ));
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new(
                Arc::new(Fig779SuspendingProcessRunner),
                Arc::clone(&registry),
                continuation_store(),
                cancel_ingress,
            )
            .serve(),
        )
        .build();
    let input = RestateProcessWorkflowInput {
        registration,
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 0,
        execution_id: None,
    };

    let parked = park_process_on_its_timer(&endpoint, &ProcessId::from(process_id), &input).await;

    registry
        .append_event(
            &ProcessId::from(process_id),
            lash_core::ProcessEventAppendRequest::cancel_requested(
                &registry
                    .resolve_process_ref(&ProcessId::from(process_id))
                    .await
                    .expect("retained cancellation target"),
                &lash_core::CancelRequest::new(
                    lash_core::CancelOrigin::OperatorRequested,
                    "actor:fixture:fig779_suspended_process_redrive_observes_durable_cancellation",
                    11,
                ),
            ),
        )
        .await
        .expect("record durable process cancellation");
    let replay = encode_recorded_commands_replay(
        process_id,
        &input,
        &[&parked],
        process_journal_completion(false),
    )
    .expect("encode suspended process redrive");
    let cancelled = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "LashProcessWorkflow",
        "run",
        replay,
        vec![serde_json::Value::Null],
    )
    .await
    .expect("redrive should replay the timer command before observing cancellation");
    assert_eq!(
        restate_message_types(&cancelled).expect("decode cancelled redrive frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_COMPLETE_PROMISE_COMMAND_MESSAGE_TYPE,
            RESTATE_COMPLETE_PROMISE_COMMAND_MESSAGE_TYPE,
            RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE,
            RESTATE_END_MESSAGE_TYPE
        ]
    );
    assert!(matches!(
        registry
            .get_process(&ProcessId::from(process_id))
            .await
            .expect("read process")
            .expect("read redriven process")
            .outcome,
        Some(ref output) if is_process_cancellation(output)
    ));
}

#[tokio::test]
pub(super) async fn fig788_terminal_outcome_landing_preserves_the_suspended_command_prefix() {
    let process_id = "fig788-terminal-outcome-redrive";
    let registry = process_registry();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register FIG-788 process");
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(Fig788TerminalRedriveRunner),
                Arc::clone(&registry),
                continuation_store(),
            )
            .serve(),
        )
        .build();
    let input = RestateProcessWorkflowInput {
        registration,
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 0,
        execution_id: None,
    };

    let parked = park_process_on_its_timer(&endpoint, &ProcessId::from(process_id), &input).await;

    let stored = process_cancellation("terminal outcome landed between attempts", None);
    registry
        .complete_process(
            &ProcessId::from(process_id),
            stored.clone(),
            workflow_key_authority(&ProcessId::from(process_id)),
        )
        .await
        .expect("store terminal outcome between attempts");
    let replay = encode_recorded_commands_replay(
        process_id,
        &input,
        &[&parked],
        process_journal_completion(true),
    )
    .expect("splice the deployed suspended journal");
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "LashProcessWorkflow",
        "run",
        replay,
        vec![serde_json::Value::Null],
    )
    .await
    .expect("terminal redrive must preserve the deployed command prefix");

    assert_eq!(
        restate_output_json::<RestateProcessWorkflowOutput>(&output),
        Some(RestateProcessWorkflowOutput::Terminal {
            output: Box::new(stored),
        })
    );
}

#[tokio::test]
pub(super) async fn fig788_ordinal_one_terminal_delivery_redrive_retains_its_handover() {
    let process_id = "fig788-ordinal-one-terminal-redrive";
    let (registry, continuations) = process_stores();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register ordinal-one process");
    let (execution_authority, started) = invocation_started(
        &ProcessId::from(process_id),
        "fig788-ordinal-one-execution",
        1,
    );
    registry
        .record_first_started_with_authority(
            &ProcessId::from(process_id),
            started,
            &execution_authority,
        )
        .await
        .expect("record retained Restate execution start");
    let persisted = lash_core::PersistedSegmentHandover {
        segment_ordinal: 1,
        handover: lash_core::SegmentHandover {
            reason: lash_core::BoundaryReason::JournalBudget,
            program_hash: "fig788-terminal-program".to_string(),
            engine_state: vec![1],
        },
    };
    continuations
        .put_segment_handover(&ProcessId::from(process_id), persisted.clone())
        .await
        .expect("persist ordinal-one handover");
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(Fig788OrdinalOneTerminalRunner),
                Arc::clone(&registry),
                Arc::clone(&continuations),
            )
            .serve(),
        )
        .build();
    let input = RestateProcessWorkflowInput {
        registration,
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 1,
        execution_id: Some("fig788-ordinal-one-execution".to_string()),
    };

    let terminal_delivery_suspension =
        invoke_endpoint(&endpoint, "LashProcessWorkflow", "run", process_id, &input)
            .await
            .expect("ordinal-one terminal delivery should suspend on its call");
    assert_eq!(
        restate_message_types(&terminal_delivery_suspension)
            .expect("decode ordinal-one terminal suspension"),
        vec![
            RESTATE_COMPLETE_PROMISE_COMMAND_MESSAGE_TYPE,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ],
        "endpoint error: {:?}",
        restate_error_message(&terminal_delivery_suspension)
    );
    assert_eq!(
        continuations
            .get_segment_handover(&ProcessId::from(process_id), 1)
            .await
            .expect("read handover during terminal delivery"),
        Some(persisted.clone()),
        "redrive input must survive until the journaled terminal delivery resolves"
    );

    let replay =
        encode_process_terminal_delivery_replay(process_id, &input, &terminal_delivery_suspension)
            .expect("splice deployed ordinal-one terminal journal");
    let output = invoke_endpoint_body_open(&endpoint, "LashProcessWorkflow", "run", replay)
        .await
        .expect("ordinal-one redrive must reconstruct and resolve the terminal prefix");
    let stored = registry
        .get_process(&ProcessId::from(process_id))
        .await
        .expect("read terminal process")
        .expect("terminal process record")
        .outcome
        .expect("stored terminal outcome");
    assert_eq!(
        restate_output_json::<RestateProcessWorkflowOutput>(&output),
        Some(RestateProcessWorkflowOutput::Terminal {
            output: Box::new(stored),
        })
    );
    assert_eq!(
        continuations
            .get_segment_handover(&ProcessId::from(process_id), 1)
            .await
            .expect("read handover after terminal delivery"),
        Some(persisted),
        "handover must remain replay authority until terminal retention pruning"
    );
}

/// FIG-2083: a terminal segment whose durable handover is gone must fail hard.
/// The removed FIG-811 shim replayed `record.outcome` and re-issued
/// `complete_terminal` from a missing durable fact. Current deployments retain
/// handovers until pruning, so this branch only fired on pre-window state or a
/// genuine bug; a missing handover may not fabricate a terminal completion.
#[tokio::test]
pub(super) async fn fig2083_terminal_segment_with_missing_handover_fails_hard() {
    let process_id = "fig2083-missing-terminal-handover";
    let (registry, continuations) = process_stores();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register FIG-2083 segmented process");
    let (execution_authority, started) = invocation_started(
        &ProcessId::from(process_id),
        "fig2083-missing-terminal-execution",
        1,
    );
    registry
        .record_first_started_with_authority(
            &ProcessId::from(process_id),
            started,
            &execution_authority,
        )
        .await
        .expect("record retained Restate execution start");
    continuations
        .put_segment_handover(
            &ProcessId::from(process_id),
            lash_core::PersistedSegmentHandover {
                segment_ordinal: 1,
                handover: lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: "fig788-terminal-program".to_string(),
                    engine_state: vec![1],
                },
            },
        )
        .await
        .expect("persist FIG-2083 ordinal-one handover");
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(Fig788OrdinalOneTerminalRunner),
                Arc::clone(&registry),
                Arc::clone(&continuations),
            )
            .serve(),
        )
        .build();
    let input = RestateProcessWorkflowInput {
        registration,
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 1,
        execution_id: Some("fig2083-missing-terminal-execution".to_string()),
    };

    let suspended = invoke_endpoint(&endpoint, "LashProcessWorkflow", "run", process_id, &input)
        .await
        .expect("terminal attempt should suspend during root delivery");
    assert_eq!(
        restate_message_types(&suspended).expect("decode terminal suspension"),
        vec![
            RESTATE_COMPLETE_PROMISE_COMMAND_MESSAGE_TYPE,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
    assert!(
        registry
            .get_process(&ProcessId::from(process_id))
            .await
            .expect("read terminal process")
            .expect("terminal process record")
            .outcome
            .is_some(),
        "the attempt commits a durable terminal outcome before its root delivery suspends"
    );
    continuations
        .delete_segment_handovers(&ProcessId::from(process_id))
        .await
        .expect("model a terminal segment whose handover is no longer durable");

    // A fresh attempt on the same terminal input must refuse on the absent
    // handover instead of replaying the stored terminal outcome.
    let refused = invoke_endpoint(&endpoint, "LashProcessWorkflow", "run", process_id, &input)
        .await
        .expect("the missing handover must render inside the invocation");
    let rendered = restate_output_failure_message(&refused)
        .expect("a terminal segment without a durable handover must fail hard");
    assert!(
        rendered.contains(&format!(
            "missing persisted handover for process `{process_id}` segment 1"
        )),
        "a missing durable handover must not replay a terminal outcome: {rendered}"
    );

    // Redriving the already-deployed terminal-delivery journal must also refuse
    // hard, never fabricating a terminal completion from the missing handover.
    //
    // Operator note: a pre-lazy-cleanup journal that still carries the delivered
    // commands is permanently stranded here. The handler now refuses at the
    // absent handover before consuming them, so Restate surfaces a terminal
    // JOURNAL_MISMATCH rather than the explicit refusal. That mismatch is
    // intended, not corruption: the journal is no longer replayable under the
    // current handover contract, and the process already holds its durable
    // terminal, so the invocation must not be retried or re-completed.
    let replay = encode_process_terminal_delivery_replay(process_id, &input, &suspended)
        .expect("splice the deployed terminal delivery");
    let redriven = invoke_endpoint_body_open(&endpoint, "LashProcessWorkflow", "run", replay)
        .await
        .expect("the redrive must render its refusal inside the invocation");
    assert!(
        restate_output_json::<RestateProcessWorkflowOutput>(&redriven).is_none(),
        "redriving a terminal segment without its handover must not fabricate a terminal outcome"
    );
    let redriven_error = restate_error_message(&redriven)
        .expect("redriving a terminal segment without its handover must fail hard");
    assert!(
        redriven_error.contains(&format!(
            "missing persisted handover for process `{process_id}` segment 1"
        )) || restate_error_code(&redriven) == Some(570),
        "the redrive must refuse on the missing handover or a JOURNAL_MISMATCH, \
         never a fabricated outcome: {redriven_error}"
    );
    // Error code 570 is verified only against the in-crate Restate endpoint
    // harness; it is inferred, not observed, on a live Restate runtime.
}

#[tokio::test]
pub(super) async fn fig811_effectful_post_terminal_redrive_replays_the_complete_prefix() {
    let process_id = "fig811-effectful-post-terminal-redrive";
    let (registry, continuations) = process_stores();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register effectful FIG-811 process");
    let (execution_authority, started) = invocation_started(
        &ProcessId::from(process_id),
        "fig811-effectful-terminal-execution",
        1,
    );
    registry
        .record_first_started_with_authority(
            &ProcessId::from(process_id),
            started,
            &execution_authority,
        )
        .await
        .expect("record retained effectful Restate execution start");
    continuations
        .put_segment_handover(
            &ProcessId::from(process_id),
            lash_core::PersistedSegmentHandover {
                segment_ordinal: 1,
                handover: lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: "fig811-effectful-terminal-program".to_string(),
                    engine_state: vec![8, 1, 1],
                },
            },
        )
        .await
        .expect("persist effectful ordinal-one handover");
    let trace_sink = Arc::new(RecordingTraceSink::default());
    let trace_sink_dyn: Arc<dyn lash_trace::TraceSink> = trace_sink.clone();
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(Fig811EffectfulOrdinalOneTerminalRunner),
                Arc::clone(&registry),
                Arc::clone(&continuations),
            )
            .with_trace_sink(
                trace_sink_dyn,
                lash_trace::TraceContext {
                    run_id: Some("fig811-workflow-trace".to_string()),
                    ..lash_trace::TraceContext::default()
                },
            )
            .serve(),
        )
        .build();
    let input = RestateProcessWorkflowInput {
        registration,
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 1,
        execution_id: Some("fig811-effectful-terminal-execution".to_string()),
    };

    let effect_suspension =
        park_process_on_its_timer(&endpoint, &ProcessId::from(process_id), &input).await;
    assert!(trace_sink.records.lock_recover().iter().any(|record| {
        record.event.kind() == "durable_timer_started"
            && record.context.run_id.as_deref() == Some("fig811-workflow-trace")
            && record.context.session_id.as_deref() == Some("session")
    }));

    let completed_effect = encode_recorded_commands_replay(
        process_id,
        &input,
        &[&effect_suspension],
        process_journal_completion(true),
    )
    .expect("splice completed effect prefix");
    let effect_cleared =
        invoke_endpoint_body(&endpoint, "LashProcessWorkflow", "run", completed_effect)
            .await
            .expect("effect completion should clear the effect from the scope's index");
    assert_eq!(
        restate_call_frames(&effect_cleared)
            .expect("decode effect-clearing calls")
            .iter()
            .map(|call| (call.service.as_str(), call.handler.as_str()))
            .collect::<Vec<_>>(),
        vec![("LashDurableWaitIndex", "end_effect")],
        "the completed effect is cleared from the scope's index before terminal delivery"
    );
    assert_eq!(
        restate_message_types(&effect_cleared).expect("decode effect-clearing frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
    let cleared_replay = encode_recorded_commands_replay(
        process_id,
        &input,
        &[&effect_suspension, &effect_cleared],
        process_journal_completion(true),
    )
    .expect("splice the cleared effect prefix");
    let terminal_delivery_suspension =
        invoke_endpoint_body(&endpoint, "LashProcessWorkflow", "run", cleared_replay)
            .await
            .expect("effect completion should reach terminal delivery");
    assert_eq!(
        restate_message_types(&terminal_delivery_suspension)
            .expect("decode effectful terminal suspension"),
        vec![
            RESTATE_COMPLETE_PROMISE_COMMAND_MESSAGE_TYPE,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );

    let complete_replay = encode_recorded_commands_replay(
        process_id,
        &input,
        &[
            &effect_suspension,
            &effect_cleared,
            &terminal_delivery_suspension,
        ],
        process_journal_completion(true),
    )
    .expect("splice the complete effectful terminal prefix");
    let completed = invoke_endpoint_body_open(
        &endpoint,
        "LashProcessWorkflow",
        "run",
        complete_replay.clone(),
    )
    .await
    .expect("terminal delivery should complete before the modeled crash");
    let stored = registry
        .get_process(&ProcessId::from(process_id))
        .await
        .expect("read effectful terminal process")
        .expect("effectful terminal process record")
        .outcome
        .expect("stored effectful terminal outcome");
    assert_eq!(
        restate_output_json::<RestateProcessWorkflowOutput>(&completed),
        Some(RestateProcessWorkflowOutput::Terminal {
            output: Box::new(stored.clone()),
        })
    );

    let redriven =
        invoke_endpoint_body_open(&endpoint, "LashProcessWorkflow", "run", complete_replay)
            .await
            .expect("post-delivery redrive should preserve the complete deployed prefix");
    assert_eq!(
        restate_output_json::<RestateProcessWorkflowOutput>(&redriven),
        Some(RestateProcessWorkflowOutput::Terminal {
            output: Box::new(stored),
        }),
        "endpoint error: {:?}",
        restate_error_message(&redriven)
    );
}

#[tokio::test]
pub(super) async fn fig788_cancel_landing_after_segment_send_preserves_the_deployed_prefix() {
    let process_id = "fig788-segment-cancel-redrive";
    let (registry, continuations) = process_stores();
    let registration = rerunnable_registration(process_id);
    registry
        .register_process(registration.clone())
        .await
        .expect("register FIG-788 segmented process");
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(Fig788SegmentBoundaryRunner),
                Arc::clone(&registry),
                continuations,
            )
            .serve(),
        )
        .build();
    let input = RestateProcessWorkflowInput {
        registration,
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 0,
        execution_id: None,
    };

    let segment_finish_suspension =
        invoke_endpoint(&endpoint, "LashProcessWorkflow", "run", process_id, &input)
            .await
            .expect("first segment attempt should suspend after scheduling its successor");
    assert_eq!(
        restate_message_types(&segment_finish_suspension)
            .expect("decode segment-finish suspension frames"),
        vec![
            RESTATE_COMPLETE_PROMISE_COMMAND_MESSAGE_TYPE,
            0x040E,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ],
        "endpoint error: {:?}",
        restate_error_message(&segment_finish_suspension)
    );

    registry
        .append_event(
            &ProcessId::from(process_id),
            lash_core::ProcessEventAppendRequest::cancel_requested(&registry.resolve_process_ref(&ProcessId::from(process_id)).await.expect("retained cancellation target"),
&lash_core::CancelRequest::new(lash_core::CancelOrigin::OperatorRequested, "actor:fixture:fig788_cancel_landing_after_segment_send_preserves_the_deployed_prefix", 11)),
        )
        .await
        .expect("record between-attempt cancellation");
    let replay = encode_process_segment_send_replay(process_id, &input, &segment_finish_suspension)
        .expect("splice deployed segment-send journal");
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "LashProcessWorkflow",
        "run",
        replay,
        vec![serde_json::Value::Null],
    )
    .await
    .expect("cancelled segment redrive must preserve the deployed send prefix");

    assert_eq!(
        restate_call_frames(&output)
            .expect("decode appended cancellation forwarding")
            .iter()
            .map(|call| (call.key.as_str(), call.handler.as_str()))
            .collect::<Vec<_>>(),
        vec![("fig788-segment-cancel-redrive#1", "deliver_cancel")]
    );
    assert_eq!(
        restate_output_json::<RestateProcessWorkflowOutput>(&output),
        Some(RestateProcessWorkflowOutput::SegmentChained {
            next_segment_ordinal: 1,
        })
    );
}

#[tokio::test]
pub(super) async fn fig806_reserved_trigger_redrive_replays_the_process_start_prefix() {
    let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let source_key = lash_core::facade_support::empty_trigger_source_key("ui.button.pressed")
        .expect("source key");
    let (process_env_store, process_env_ref) = lash_core::testing::process_execution_env_fixture();
    let registration = store
        .execute_command(
            "fig806-register",
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::host("fig806").expect("owner scope"),
                actor: lash_core::ProcessOriginator::host_scoped("fig806"),
                draft: lash_core::TriggerSubscriptionDraft::for_process(
                    "fig806/subscription",
                    process_env_ref,
                    "ui.button.pressed",
                    source_key.clone(),
                    ProcessInput::Engine {
                        kind: "testing-fixture".to_string(),
                        payload: serde_json::json!({}),
                    },
                    lash_core::ProcessIdentity::new("testing-fixture"),
                )
                .with_payload_schema(lash_core::LashSchema::any()),
            },
        )
        .await
        .expect("register trigger subscription")
        .expect("trigger registration outcome");
    assert!(matches!(
        registration,
        lash_core::TriggerCommandOutcome::Mutation { .. }
    ));
    let registry = process_registry();
    let router = lash_core::facade_support::TriggerRouter::new(
        Arc::clone(&store) as Arc<dyn lash_core::TriggerStore>,
        registry_process_wiring(Arc::clone(&registry)),
    )
    .with_process_artifacts(
        process_env_store,
        lash_core::testing::process_engine_fixture(),
    );
    let endpoint = Endpoint::builder()
        .bind(Fig806TriggerRedriveImpl { router }.serve())
        .build();
    let input = Fig806TriggerRedriveInput {
        occurrence: lash_core::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            source_key,
            serde_json::json!({"button": "Blue"}),
            "fig806-occurrence",
        ),
    };
    let workflow_key = "fig806-trigger-redrive";

    let invocation_id = "inv_fig806_trigger_process";
    let suspended = invoke_endpoint_with_scripted_responses(
        &endpoint,
        "Fig806TriggerRedrive",
        "run",
        workflow_key,
        &input,
        vec![invocation_id.to_string()],
        vec![serde_json::Value::Bool(true), serde_json::Value::Null],
    )
    .await
    .expect("trigger start should suspend on its terminal delivery call");
    assert_eq!(
        restate_message_types(&suspended).expect("decode trigger suspension"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            0x040E,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ],
        "endpoint error: {:?}",
        restate_error_message(&suspended)
    );
    assert_eq!(
        restate_call_frames(&suspended)
            .expect("decode trigger calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["begin_effect", "end_effect", "complete"]
    );
    let replay = encode_recorded_commands_with_invocations_replay(
        workflow_key,
        &input,
        &[&suspended],
        &[invocation_id],
        trigger_journal_completion,
    )
    .expect("splice the complete deployed trigger delivery journal");
    let output = invoke_endpoint_body(&endpoint, "Fig806TriggerRedrive", "run", replay)
        .await
        .expect("reserved trigger redrive must preserve the process-start prefix");

    let report = restate_output_json::<lash_core::facade_support::TriggerEmitReport>(&output)
        .expect("decode trigger emit report");
    assert!(matches!(
        report.deliveries.as_slice(),
        [lash_core::facade_support::TriggerDeliveryEmitReceipt {
            outcome: lash_core::facade_support::TriggerDeliveryEmitOutcome::AlreadyReserved,
            ..
        }]
    ));
    assert!(
        restate_call_frames(&output)
            .expect("decode post-journal output")
            .is_empty(),
        "the replayed terminal call must not be emitted a second time"
    );
    assert_eq!(
        registry
            .list_processes(&lash_core::ProcessListFilter::default())
            .await
            .expect("list trigger processes")
            .len(),
        1,
        "one occurrence must still create exactly one process"
    );
}

pub(super) async fn register_fig811_subscription(
    store: &dyn lash_core::TriggerStore,
    operation_id: &str,
    subscription_key: &str,
    source_key: &str,
) -> String {
    let (_, process_env_ref) = lash_core::testing::process_execution_env_fixture();
    let outcome = store
        .execute_command(
            operation_id,
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::host("fig811")
                    .expect("FIG-811 owner scope"),
                actor: lash_core::ProcessOriginator::host_scoped("fig811"),
                draft: lash_core::TriggerSubscriptionDraft::for_process(
                    subscription_key,
                    process_env_ref,
                    "ui.button.pressed",
                    source_key,
                    ProcessInput::Engine {
                        kind: "testing-fixture".to_string(),
                        payload: serde_json::json!({}),
                    },
                    lash_core::ProcessIdentity::new("testing-fixture"),
                )
                .with_payload_schema(lash_core::LashSchema::any()),
            },
        )
        .await
        .expect("register FIG-811 trigger subscription")
        .expect("FIG-811 trigger registration outcome");
    let lash_core::TriggerCommandOutcome::Mutation { receipt } = outcome else {
        panic!("register must return a mutation receipt");
    };
    receipt.subscription_id
}

#[tokio::test]
pub(super) async fn fig811_two_subscription_sqlite_redrive_preserves_canonical_start_order() {
    let store = Arc::new(
        lash_sqlite_store::SqliteTriggerStore::memory()
            .await
            .expect("open SQLite trigger store"),
    );
    let source_key = lash_core::facade_support::empty_trigger_source_key("ui.button.pressed")
        .expect("source key");
    let _alpha_id = register_fig811_subscription(
        store.as_ref(),
        "fig811-register-alpha",
        "alpha",
        &source_key,
    )
    .await;
    let _beta_id =
        register_fig811_subscription(store.as_ref(), "fig811-register-beta", "beta", &source_key)
            .await;
    let mut expected_subscriptions = store
        .list_subscriptions(lash_core::TriggerSubscriptionFilter::default())
        .await
        .expect("list FIG-811 subscriptions for canonical order");
    expected_subscriptions.sort_by(|left, right| {
        left.owner_scope
            .namespace()
            .cmp(&right.owner_scope.namespace())
            .then_with(|| left.subscription_key.cmp(&right.subscription_key))
            .then_with(|| left.subscription_id.cmp(&right.subscription_id))
    });
    let expected_subscription_ids = expected_subscriptions
        .iter()
        .map(|subscription| subscription.subscription_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        expected_subscription_ids.len(),
        2,
        "the FIG-811 fixture must contain exactly its alpha and beta subscriptions"
    );

    let registry = process_registry();
    let (process_env_store, _) = lash_core::testing::process_execution_env_fixture();
    let router = lash_core::facade_support::TriggerRouter::new(
        Arc::clone(&store) as Arc<dyn lash_core::TriggerStore>,
        registry_process_wiring(Arc::clone(&registry)),
    )
    .with_process_artifacts(
        process_env_store,
        lash_core::testing::process_engine_fixture(),
    );
    let endpoint = Endpoint::builder()
        .bind(Fig806TriggerRedriveImpl { router }.serve())
        .build();
    let input = Fig806TriggerRedriveInput {
        occurrence: lash_core::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            source_key,
            serde_json::json!({"button": "Blue"}),
            "fig811-two-subscription-occurrence",
        ),
    };
    let workflow_key = "fig811-two-subscription-redrive";
    let invocation_ids = ["inv_fig811_alpha_process", "inv_fig811_beta_process"];

    let suspended = invoke_endpoint_with_scripted_responses(
        &endpoint,
        "Fig806TriggerRedrive",
        "run",
        workflow_key,
        &input,
        invocation_ids.iter().map(ToString::to_string).collect(),
        vec![
            serde_json::Value::Bool(true),
            serde_json::Value::Null,
            serde_json::Value::Bool(true),
            serde_json::Value::Null,
        ],
    )
    .await
    .expect("initial multi-subscription attempt should suspend after both starts");
    assert_eq!(
        restate_message_types(&suspended).expect("decode multi-subscription suspension"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            0x040E,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            0x040E,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ],
        "endpoint error: {:?}",
        restate_error_message(&suspended)
    );

    assert_eq!(
        restate_call_frames(&suspended)
            .expect("decode multi-subscription calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec![
            "begin_effect",
            "end_effect",
            "begin_effect",
            "end_effect",
            "complete"
        ]
    );

    let replay = encode_recorded_commands_with_invocations_replay(
        workflow_key,
        &input,
        &[&suspended],
        &invocation_ids,
        trigger_journal_completion,
    )
    .expect("splice both deployed process starts");
    let output = invoke_endpoint_body(&endpoint, "Fig806TriggerRedrive", "run", replay)
        .await
        .expect("multi-subscription redrive must preserve process-start ordering");
    let report = restate_output_json::<lash_core::facade_support::TriggerEmitReport>(&output)
        .unwrap_or_else(|| {
            panic!(
                "decode multi-subscription replay report; endpoint error={:?}, output failure={:?}",
                restate_error_message(&output),
                (
                    restate_output_failure_message(&output),
                    restate_message_types(&output)
                )
            )
        });
    assert_eq!(
        report
            .deliveries
            .iter()
            .map(|delivery| (delivery.subscription_id.as_str(), &delivery.outcome,))
            .collect::<Vec<_>>(),
        expected_subscription_ids
            .into_iter()
            .map(|subscription_id| {
                (
                    subscription_id,
                    &lash_core::facade_support::TriggerDeliveryEmitOutcome::AlreadyReserved,
                )
            })
            .collect::<Vec<_>>()
    );
    assert_eq!(
        registry
            .list_processes(&lash_core::ProcessListFilter::default())
            .await
            .expect("list trigger processes")
            .len(),
        2,
        "two subscriptions create exactly two deterministic processes"
    );
}

#[tokio::test]
pub(super) async fn fig811_independent_client_retry_reports_duplicate_without_a_second_process() {
    let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let source_key = lash_core::facade_support::empty_trigger_source_key("ui.button.pressed")
        .expect("source key");
    register_fig811_subscription(
        store.as_ref(),
        "fig811-register-client-retry",
        "client-retry",
        &source_key,
    )
    .await;
    let registry = process_registry();
    let (process_env_store, _) = lash_core::testing::process_execution_env_fixture();
    let router = lash_core::facade_support::TriggerRouter::new(
        Arc::clone(&store) as Arc<dyn lash_core::TriggerStore>,
        registry_process_wiring(Arc::clone(&registry)),
    )
    .with_process_artifacts(
        process_env_store,
        lash_core::testing::process_engine_fixture(),
    );
    let endpoint = Endpoint::builder()
        .bind(Fig806TriggerRedriveImpl { router }.serve())
        .build();
    let input = Fig806TriggerRedriveInput {
        occurrence: lash_core::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            source_key,
            serde_json::json!({"button": "Blue"}),
            "fig811-client-retry-occurrence",
        ),
    };
    let workflow_invocation_id = "inv_restate_workflow_LashProcessWorkflow_fig811_client_retry";

    let first = invoke_endpoint_with_scripted_responses(
        &endpoint,
        "Fig806TriggerRedrive",
        "run",
        "fig811-client-attempt-one",
        &input,
        vec![workflow_invocation_id.to_string()],
        vec![
            serde_json::Value::Bool(true),
            serde_json::Value::Null,
            serde_json::Value::Null,
        ],
    )
    .await
    .expect("first independent client invocation");
    let first = restate_output_json::<lash_core::facade_support::TriggerEmitReport>(&first)
        .expect("decode first client report");
    assert!(matches!(
        first.deliveries.as_slice(),
        [lash_core::facade_support::TriggerDeliveryEmitReceipt {
            outcome: lash_core::facade_support::TriggerDeliveryEmitOutcome::Started,
            ..
        }]
    ));

    let second = invoke_endpoint_with_scripted_responses(
        &endpoint,
        "Fig806TriggerRedrive",
        "run",
        "fig811-client-attempt-two",
        &input,
        vec![workflow_invocation_id.to_string()],
        vec![
            serde_json::Value::Bool(true),
            serde_json::Value::Null,
            serde_json::Value::Null,
        ],
    )
    .await
    .expect("second independent client invocation");
    let second = restate_output_json::<lash_core::facade_support::TriggerEmitReport>(&second)
        .expect("decode second client report");
    assert!(matches!(
        second.deliveries.as_slice(),
        [lash_core::facade_support::TriggerDeliveryEmitReceipt {
            outcome: lash_core::facade_support::TriggerDeliveryEmitOutcome::AlreadyReserved,
            ..
        }]
    ));
    assert_eq!(
        registry
            .list_processes(&lash_core::ProcessListFilter::default())
            .await
            .expect("list trigger processes")
            .len(),
        1,
        "independent retry must retain exactly one process"
    );
}

pub(super) async fn fig793_pre_fix_suspended_llm_run(
    invocation_id: &str,
) -> (Endpoint, Bytes, serde_json::Value) {
    let endpoint = Endpoint::builder()
        .bind(Fig793LlmGateRedriveImpl.serve())
        .build();
    let input = Fig793LlmGateRedriveInput;
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig793LlmGateRedrive",
        "run",
        invocation_id,
        &input,
    )
    .await
    .expect("capture deployed pre-FIG-793 LLM journal");
    assert_eq!(
        restate_message_types(&suspended).expect("decode suspended LLM run frames"),
        vec![
            RESTATE_RUN_COMMAND_MESSAGE_TYPE,
            0x0005,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
    let recorded = RecordedRuntimeEffect {
        envelope: Arc::new(
            fig793_llm_envelope()
                .canonical_form()
                .expect("canonical FIG-793 LLM envelope"),
        ),
        outcome: Ok(fig793_llm_outcome()),
    };
    (
        endpoint,
        suspended,
        serde_json::to_value(recorded).expect("serialize recorded LLM outcome"),
    )
}

#[tokio::test]
pub(super) async fn fig793_pre_fix_suspended_llm_run_redrives_without_cancellation() {
    let invocation_id = "fig793-pre-fix-no-cancel";
    let (endpoint, suspended, completion) = fig793_pre_fix_suspended_llm_run(invocation_id).await;
    let replay = encode_run_replay(
        invocation_id,
        &Fig793LlmGateRedriveInput,
        &suspended,
        completion,
    )
    .expect("splice pre-FIG-793 LLM journal");
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig793LlmGateRedrive",
        "run",
        replay,
        vec![serde_json::json!(false), serde_json::Value::Null],
    )
    .await
    .expect("new cancellation observation must extend the deployed LLM prefix");

    assert_eq!(
        restate_call_frames(&output)
            .expect("decode post-LLM observation calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["is_revoked", "peek"]
    );
    assert_eq!(restate_output_json::<bool>(&output), Some(false));
}

#[tokio::test]
pub(super) async fn fig793_pre_fix_suspended_llm_run_redrives_to_cancelled_boundary() {
    let invocation_id = "fig793-pre-fix-cancelled";
    let (endpoint, suspended, completion) = fig793_pre_fix_suspended_llm_run(invocation_id).await;
    let replay = encode_run_replay(
        invocation_id,
        &Fig793LlmGateRedriveInput,
        &suspended,
        completion,
    )
    .expect("splice pre-FIG-793 cancelled LLM journal");
    let cancellation = Resolution::Ok(serde_json::json!({
        "state": "cancel_requested",
        "cancellation": {
            "request_id": "fig793-cancel",
            "reason": "cancel landed while the LLM run was suspended"
        }
    }));
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig793LlmGateRedrive",
        "run",
        replay,
        vec![
            serde_json::json!(false),
            serde_json::to_value(Some(cancellation)).expect("serialize durable cancellation"),
        ],
    )
    .await
    .expect("cancelled redrive must extend the deployed LLM prefix");

    assert_eq!(
        restate_call_frames(&output)
            .expect("decode cancelled post-LLM observation calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["is_revoked", "peek"]
    );
    assert_eq!(restate_output_json::<bool>(&output), Some(true));
}

/// PR #78's synthetic re-poll defense is re-scoped to the fused-state boundary.
/// A manual second poll stays pending without reaching the fused SDK future or
/// introducing a panic-capable branch in the production handler boundary.
#[tokio::test]
pub(super) async fn fig779_real_restate_timer_repoll_stays_pending_without_panic() {
    let endpoint = Endpoint::builder()
        .bind(Fig779TimerGuardReproImpl.serve())
        .build();
    let output = invoke_endpoint(
        &endpoint,
        "Fig779TimerGuardRepro",
        "repoll_fused_timer",
        "fig779-repoll-fused-timer",
        &Fig779TimerGuardReproInput { duration_ms: 2_000 },
    )
    .await
    .expect("re-polling the fused wrapper must not panic");

    assert_eq!(
        restate_message_types(&output).expect("decode re-poll response frames"),
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ],
        "the fused timer must preserve the SDK suspension"
    );
}

/// FIG-1464: the workbench replay-panic loop. A journaled effect whose
/// `ctx.run` fails at the SDK level leaves an already-`Ready` run future behind
/// `InterceptErrorFuture`'s recorded-failure park. Re-entering it aborts the
/// handler task, so the attempt ends with no output and no `End`, Restate
/// redrives it, and the deterministic replay panics again - the turn can never
/// terminate. The seam must fuse the run future instead.
#[tokio::test]
pub(super) async fn fig1464_failed_journaled_run_repoll_stays_pending_without_panic() {
    let endpoint = Endpoint::builder()
        .bind(Fig1464RunGuardReproImpl.serve())
        .build();
    let output = invoke_endpoint(
        &endpoint,
        "Fig1464RunGuardRepro",
        "repoll_failed_run",
        "fig1464-repoll-failed-run",
        &Fig1464RunGuardReproInput {
            effect_name: "lash:fig1464-unjournalable-effect".to_string(),
        },
    )
    .await
    .expect("re-polling a failed journaled run must not panic");

    let message_types = restate_message_types(&output).expect("decode failed-run response frames");
    assert!(
        !message_types.contains(&RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE)
            && !message_types.contains(&RESTATE_END_MESSAGE_TYPE),
        "a failed journaled run must not land a fabricated output: {message_types:?}"
    );
    assert!(
        restate_error_message(&output)
            .is_some_and(|message| message.contains("cannot be journaled")),
        "the attempt must end on the recorded run failure, not a panic"
    );
}

/// FIG-1464: the same panic loop on the replay path, which is the interleaving
/// the ticket actually reports. The run entry is already journaled, so the SDK
/// skips the closure entirely; reading the recorded value back fails, and
/// `InterceptErrorFuture` parks on the recorded failure after waking
/// synchronously. A fuse that waited for the closure to complete would stay
/// inert for this whole attempt, so the second poll would re-enter an
/// already-resolved SDK future and abort the handler.
#[tokio::test]
pub(super) async fn fig1464_replayed_unreadable_run_repoll_stays_pending_without_panic() {
    let endpoint = Endpoint::builder()
        .bind(Fig1464RunGuardReproImpl.serve())
        .build();
    let key = "fig1464-repoll-replayed-run";
    let input = Fig1464RunGuardReproInput {
        effect_name: "lash:fig1464-unreadable-journaled-effect".to_string(),
    };
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig1464RunGuardRepro",
        "repoll_replayed_run",
        key,
        &input,
    )
    .await
    .expect("the first attempt must journal the run and park");
    assert!(
        restate_message_types(&suspended)
            .expect("decode first-attempt frames")
            .contains(&RESTATE_RUN_COMMAND_MESSAGE_TYPE),
        "the effect must be journaled as a RunCommand before the replay leg"
    );

    let body = encode_run_replay(key, &input, &suspended, serde_json::json!(41))
        .expect("encode completed journaled run replay");
    let output = invoke_endpoint_body(
        &endpoint,
        "Fig1464RunGuardRepro",
        "repoll_replayed_run",
        body,
    )
    .await
    .expect("re-polling a replayed run failure must not panic");

    let message_types = restate_message_types(&output).expect("decode replayed-run frames");
    assert!(
        !message_types.contains(&RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE)
            && !message_types.contains(&RESTATE_END_MESSAGE_TYPE),
        "a replayed run failure must not land a fabricated output: {message_types:?}"
    );
    assert!(
        restate_error_message(&output)
            .is_some_and(|message| message.contains("cannot be read back")),
        "the attempt must end on the recorded replay failure, not a panic: {:?}",
        restate_error_message(&output)
    );
}

/// FIG-1464 contrast: fusing the terminal attempt state must not swallow a
/// journaled run that really did produce a result.
#[tokio::test]
pub(super) async fn fig1464_journaled_run_still_returns_its_recorded_result() {
    let endpoint = Endpoint::builder()
        .bind(Fig1464RunGuardReproImpl.serve())
        .build();
    let key = "fig1464-journaled-run";
    let input = Fig1464RunGuardReproInput {
        effect_name: "lash:fig1464-journaled-effect".to_string(),
    };
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig1464RunGuardRepro",
        "journaled_run",
        key,
        &input,
    )
    .await
    .expect("the journaled run must park on its proposed completion");
    assert!(
        restate_message_types(&suspended)
            .expect("decode journaled run frames")
            .contains(&RESTATE_RUN_COMMAND_MESSAGE_TYPE),
        "the effect must be journaled as a RunCommand"
    );

    let body = encode_run_replay(key, &input, &suspended, serde_json::json!(41))
        .expect("encode completed journaled run replay");
    let output = invoke_endpoint_body(&endpoint, "Fig1464RunGuardRepro", "journaled_run", body)
        .await
        .expect("the completed journaled run must return its recorded result");

    assert_eq!(restate_output_json::<u32>(&output), Some(42));
}

/// FIG-1464: a wake the run closure's own future issued must not fuse the run.
/// `LlmCall` routes to a journaled run with no task boundary between the
/// streaming code and this seam, so a same-task self-wake from that code reaches
/// the guard. Treating it as the SDK's terminal park would fuse a healthy run:
/// the effect would never even be proposed as a `RunCommand`, and the turn would
/// hang holding a paid completion.
#[tokio::test]
pub(super) async fn fig1464_self_waking_run_closure_does_not_fuse_the_run() {
    let endpoint = Endpoint::builder()
        .bind(Fig1464RunGuardReproImpl.serve())
        .build();
    let key = "fig1464-self-waking-run"; // gitleaks:allow -- synthetic workflow/turn identity fixture
    let input = Fig1464RunGuardReproInput {
        effect_name: "lash:fig1464-self-waking-effect".to_string(),
    };
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig1464RunGuardRepro",
        "self_waking_run",
        key,
        &input,
    )
    .await
    .expect("a self-waking run closure must still reach its proposed completion");
    assert!(
        restate_message_types(&suspended)
            .expect("decode self-waking run frames")
            .contains(&RESTATE_RUN_COMMAND_MESSAGE_TYPE),
        "a self-waking run closure must still journal its effect"
    );

    let body = encode_run_replay(key, &input, &suspended, serde_json::json!(41))
        .expect("encode completed self-waking run replay");
    let output = invoke_endpoint_body(&endpoint, "Fig1464RunGuardRepro", "self_waking_run", body)
        .await
        .expect("the completed self-waking run must return its recorded result");

    assert_eq!(restate_output_json::<u32>(&output), Some(42));
}

/// FIG-779 contrast: an already-completed timer replays straight to `Ready`, so
/// the guard never sees a synchronous wake. This is why the panic is only
/// reachable on the attempt that first parks on the timer, not on the resume.
#[tokio::test]
pub(super) async fn fig779_completed_durable_timer_replay_does_not_enter_guard_panic() {
    let endpoint = Endpoint::builder()
        .bind(Fig779TimerGuardReproImpl.serve())
        .build();
    let input = Fig779TimerGuardReproInput { duration_ms: 2_000 };
    let body = encode_completed_sleep_replay("fig779-timer", &input)
        .expect("encode completed durable timer replay");

    invoke_endpoint_body(&endpoint, "Fig779TimerGuardRepro", "run", body)
        .await
        .expect("completed durable timer replay should finish without panicking");
}
