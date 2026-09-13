use super::*;

/// FIG-779 control: the identical input against the bare SDK timer is handled
/// correctly — the endpoint writes a `SleepCommand` followed by a `Suspension`
/// frame. The synchronous-wake-then-Pending shape is therefore the SDK's normal
/// suspension protocol, not a driver-invariant violation.
#[tokio::test]
pub(super) async fn fig779_sdk_pending_durable_timer_suspends_cleanly_without_guard() {
    let endpoint = Endpoint::builder()
        .bind(Fig779TimerGuardReproImpl.serve())
        .build();

    let output = invoke_endpoint(
        &endpoint,
        "Fig779TimerGuardRepro",
        "raw_sleep",
        "fig779-raw-timer",
        &Fig779TimerGuardReproInput { duration_ms: 2_000 },
    )
    .await
    .expect("the SDK must encode a pending timer suspension without panicking");
    let message_types = restate_message_types(&output).expect("decode Restate response frames");
    assert_eq!(
        message_types,
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
}

#[derive(Debug, Serialize, serde::Deserialize)]
pub(super) struct Fig790TurnEventPumpInput {
    process_id: ProcessId,
    prequeue_event: bool,
}

#[restate_sdk::workflow]
trait Fig790TurnEventPump {
    async fn run(input: Json<Fig790TurnEventPumpInput>) -> HandlerResult<Json<()>>;
}

pub(super) struct Fig790TurnEventPumpImpl;

impl Fig790TurnEventPump for Fig790TurnEventPumpImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig790TurnEventPumpInput>,
    ) -> HandlerResult<Json<()>> {
        let request: restate_sdk::context::Request<
            '_,
            Json<RestateProcessAwaitRequest>,
            Json<ProcessAwaitOutput>,
        > = ContextClient::request(
            &ctx,
            RequestTarget::workflow(
                "LashProcessWorkflow",
                input.process_id.clone(),
                "await_terminal",
            ),
            Json(RestateProcessAwaitRequest {
                process_id: input.process_id,
            }),
        );
        let mut run_future = Box::pin(async move {
            let Json(_output) = request.call().await?;
            Ok::<(), TerminalError>(())
        });
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(1);
        if input.prequeue_event {
            event_tx
                .send(())
                .await
                .expect("queue the event that makes the pump branch ready");
        }
        drop(event_tx);

        let mut handler_state = ();
        lash_core::drive_with_event_pump(
            run_future.as_mut(),
            &mut event_rx,
            &mut handler_state,
            |(), _| Box::pin(async {}),
        )
        .await?;
        Ok(Json(()))
    }
}

pub(super) async fn assert_fig790_turn_event_pump_suspends_cleanly(
    invocation_id: &str,
    prequeue_event: bool,
) {
    let endpoint = Endpoint::builder()
        .bind(Fig790TurnEventPumpImpl.serve())
        .build();
    let output = invoke_endpoint(
        &endpoint,
        "Fig790TurnEventPump",
        "run",
        invocation_id,
        &Fig790TurnEventPumpInput {
            process_id: ProcessId::from(format!("{invocation_id}-process")),
            prequeue_event,
        },
    )
    .await
    .expect("the event pump must let the substrate consume its suspension");
    assert_eq!(
        restate_message_types(&output).expect("decode process-await suspension frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
}

#[tokio::test]
pub(super) async fn fig790_turn_event_pump_does_not_repoll_a_suspending_durable_future() {
    assert_fig790_turn_event_pump_suspends_cleanly("fig790-turn-event-pump", true).await;
}

#[tokio::test]
pub(super) async fn fig790_turn_event_pump_with_empty_channel_suspends_cleanly() {
    assert_fig790_turn_event_pump_suspends_cleanly("fig790-turn-event-pump-empty", false).await;
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub(super) struct Fig790ProcessAwaitRedriveInput {
    pub(super) process_ref: lash_core::ProcessRef,
    pub(super) cancel_on_suspend_wake: bool,
}

#[restate_sdk::workflow]
trait Fig790ProcessAwaitRedrive {
    async fn run(
        input: Json<Fig790ProcessAwaitRedriveInput>,
    ) -> HandlerResult<Json<ProcessAwaitOutput>>;
}

pub(super) struct Fig790ProcessAwaitRedriveImpl {
    registry: Arc<dyn ProcessRegistry>,
}

impl Fig790ProcessAwaitRedrive for Fig790ProcessAwaitRedriveImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig790ProcessAwaitRedriveInput>,
    ) -> HandlerResult<Json<ProcessAwaitOutput>> {
        let controller = RestateRuntimeEffectController::new(ctx);
        let cancellation = tokio_util::sync::CancellationToken::new();
        let effect = controller.execute_effect(
            RuntimeEffectEnvelope::new(
                runtime_invocation(RuntimeEffectKind::Process, "fig790-process-await"),
                RuntimeEffectCommand::process(ProcessCommand::Await {
                    process_ref: input.process_ref,
                }),
            ),
            registry_local_executor(Arc::clone(&self.registry)).with_process_turn_cancellation(
                lash_core::facade_support::ProcessTurnCancellation::new(
                    cancellation.clone(),
                    durable_turn_scope("session", "turn"),
                ),
            ),
        );
        let outcome = if input.cancel_on_suspend_wake {
            CancelOnWakeFuture {
                future: Box::pin(effect),
                cancellation,
            }
            .await
        } else {
            effect.await
        }
        .map_err(TerminalError::from_error)?;
        let ProcessEffectOutcome::Await { output } =
            outcome.into_process().map_err(TerminalError::from_error)?
        else {
            return Err(TerminalError::new(
                "process-await fixture returned the wrong process outcome",
            )
            .into());
        };
        Ok(Json(*output))
    }
}

pub(super) fn fig790_cancelled_process_output(process_id: &ProcessId) -> ProcessAwaitOutput {
    process_cancellation(
        format!("process `{process_id}` observed durable turn cancellation"),
        None,
    )
}

pub(super) async fn fig790_process_await_endpoint(
    process_id: &ProcessId,
) -> (Endpoint, Arc<dyn ProcessRegistry>) {
    let registry = process_registry();
    registry
        .register_process(rerunnable_registration(process_id))
        .await
        .expect("register FIG-790 process");
    let endpoint = Endpoint::builder()
        .bind(
            Fig790ProcessAwaitRedriveImpl {
                registry: Arc::clone(&registry),
            }
            .serve(),
        )
        .build();
    (endpoint, registry)
}

pub(super) async fn fig790_pre_pr_suspended_process_call(
    process_id: &ProcessId,
) -> endpoint_protocol::RestateCallFrame {
    let endpoint = Endpoint::builder()
        .bind(Fig790TurnEventPumpImpl.serve())
        .build();
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig790TurnEventPump",
        "run",
        &format!("{process_id}-pre-pr-fixture"),
        &Fig790TurnEventPumpInput {
            process_id: ProcessId::from(process_id.to_string()),
            prequeue_event: false,
        },
    )
    .await
    .expect("capture the deployed pre-PR process-await journal shape");
    let calls = restate_call_frames(&suspended).expect("decode pre-PR process-await call");
    let [call] = calls.as_slice() else {
        panic!("pre-PR process-await fixture must contain exactly one call");
    };
    assert_eq!(call.handler, "await_terminal");
    call.clone()
}

// Deployment compatibility gate: a process-await invocation suspended before
// FIG-790 has only `await_terminal` in its journal. Its terminal redrive must
// accept that command as an exact prefix and append cancellation observation.
#[tokio::test]
pub(super) async fn fig790_pre_pr_suspended_process_await_redrives_to_terminal() {
    let process_id = "fig790-pre-pr-terminal";
    let pre_pr_call = fig790_pre_pr_suspended_process_call(&ProcessId::from(process_id)).await;
    let (endpoint, _registry) = fig790_process_await_endpoint(&ProcessId::from(process_id)).await;
    let input = Fig790ProcessAwaitRedriveInput {
        process_ref: lash_core::ProcessRef::new(
            process_id,
            lash_core::ProcessIncarnation::from_registration_sequence(1),
        ),
        cancel_on_suspend_wake: false,
    };
    let terminal = process_success(serde_json::json!({ "deployment_compat": "terminal" }));
    let replay = encode_call_replay(
        "fig790-pre-pr-terminal",
        &input,
        &[(
            pre_pr_call,
            Some(serde_json::to_value(&terminal).expect("serialize process terminal")),
        )],
        None,
    )
    .expect("splice pre-PR terminal journal");
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        replay,
        vec![
            serde_json::to_value(RestateDurableWaitRegistration::Registered)
                .expect("serialize registered observation"),
            serde_json::Value::Null,
        ],
    )
    .await
    .expect("new code must redrive the deployed pre-PR journal prefix");

    assert_eq!(
        restate_call_frames(&output)
            .expect("decode appended terminal-redrive calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["register_awakeable", "unregister_awakeable"]
    );
    assert_eq!(
        restate_output_json::<ProcessAwaitOutput>(&output),
        Some(terminal)
    );
}

// Deployment compatibility gate: the same pre-PR suspended prefix must also
// redrive when turn cancellation was already resolved before registration.
// This is deliberately distinct from a revoked session.
#[tokio::test]
pub(super) async fn fig790_pre_pr_suspended_process_await_redrives_to_cancelled() {
    let process_id = "fig790-pre-pr-cancelled";
    let pre_pr_call = fig790_pre_pr_suspended_process_call(&ProcessId::from(process_id)).await;
    let (endpoint, _registry) = fig790_process_await_endpoint(&ProcessId::from(process_id)).await;
    let input = Fig790ProcessAwaitRedriveInput {
        process_ref: lash_core::ProcessRef::new(
            process_id,
            lash_core::ProcessIncarnation::from_registration_sequence(1),
        ),
        cancel_on_suspend_wake: false,
    };
    let cancelled = fig790_cancelled_process_output(&ProcessId::from(process_id));
    let replay = encode_call_replay(
        "fig790-pre-pr-cancelled",
        &input,
        &[(pre_pr_call, None)],
        Some((
            17,
            serde_json::to_value(RestateTurnCancelWake::TurnCancelled)
                .expect("serialize already-resolved turn cancellation"),
        )),
    )
    .expect("splice pre-PR cancelled journal");
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        replay,
        vec![
            serde_json::to_value(RestateDurableWaitRegistration::Registered)
                .expect("serialize registered observation"),
            serde_json::Value::Null,
            serde_json::to_value(&cancelled).expect("serialize cancelled process terminal"),
        ],
    )
    .await
    .expect("already-resolved cancellation must redrive the pre-PR journal");

    assert_eq!(
        restate_call_frames(&output)
            .expect("decode appended cancellation-redrive calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["register_awakeable", "cancel", "await_terminal"]
    );
    assert_eq!(
        restate_output_json::<ProcessAwaitOutput>(&output),
        Some(cancelled)
    );
}

#[tokio::test]
pub(super) async fn fig790_revoked_session_unwinds_turn_without_cancelling_process() {
    let process_id = "fig790-revoked-session";
    let (endpoint, registry) = fig790_process_await_endpoint(&ProcessId::from(process_id)).await;
    let input = Fig790ProcessAwaitRedriveInput {
        process_ref: lash_core::ProcessRef::new(
            process_id,
            lash_core::ProcessIncarnation::from_registration_sequence(1),
        ),
        cancel_on_suspend_wake: false,
    };
    let registration = serde_json::to_value(RestateDurableWaitRegistration::Revoked)
        .expect("serialize revoked registration");

    let suspended = invoke_endpoint(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        "fig790-revoked-session",
        &input,
    )
    .await
    .expect("capture the process-await command prefix");
    let calls = restate_call_frames(&suspended).expect("decode process-await call frames");
    assert_eq!(
        calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["await_terminal", "register_awakeable"],
        "session revocation must preserve the old journal prefix and emit no process cancel"
    );

    let replay = encode_call_replay(
        "fig790-revoked-session",
        &input,
        &[
            (calls[0].clone(), None),
            (calls[1].clone(), Some(registration)),
        ],
        None,
    )
    .expect("splice revoked-session call journal");
    let first = invoke_endpoint_body(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        replay.clone(),
    )
    .await
    .expect("revoked session should terminalize the dead turn");
    assert_eq!(
        restate_call_frames(&first)
            .expect("decode revoked-session calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        Vec::<&str>::new(),
        "session revocation must emit no process cancel"
    );
    assert!(
        restate_output_failure_message(&first)
            .is_some_and(|failure| failure.contains("used and deleted")),
        "revoked process await must terminalize with the typed deleted-session refusal"
    );
    assert!(
        !registry
            .get_process(&ProcessId::from(process_id))
            .await
            .expect("revoked await keeps its process record")
            .expect("revoked await keeps the process present")
            .is_terminal(),
        "revoking session observation edges must not terminalize the process"
    );

    let redriven = invoke_endpoint_body(&endpoint, "Fig790ProcessAwaitRedrive", "run", replay)
        .await
        .expect("revoked-session redrive must accept the identical command sequence");
    assert_eq!(
        restate_call_frames(&redriven)
            .expect("decode revoked-session redrive calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        Vec::<&str>::new(),
        "revoked-session redrive must not append a process cancel"
    );
    assert!(
        !registry
            .get_process(&ProcessId::from(process_id))
            .await
            .expect("redriven revoked await keeps its process record")
            .expect("redriven revoked await keeps the process present")
            .is_terminal()
    );
}

#[tokio::test]
pub(super) async fn fig790_registered_session_revocation_unwinds_without_cancelling_process() {
    let process_id = "fig790-registered-then-revoked";
    let (endpoint, registry) = fig790_process_await_endpoint(&ProcessId::from(process_id)).await;
    let input = Fig790ProcessAwaitRedriveInput {
        process_ref: lash_core::ProcessRef::new(
            process_id,
            lash_core::ProcessIncarnation::from_registration_sequence(1),
        ),
        cancel_on_suspend_wake: false,
    };
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        "fig790-registered-then-revoked",
        &input,
    )
    .await
    .expect("capture registered process-await commands");
    let calls = restate_call_frames(&suspended).expect("decode registered process-await calls");
    assert_eq!(
        calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["await_terminal", "register_awakeable"]
    );
    let replay = encode_call_replay(
        "fig790-registered-then-revoked",
        &input,
        &[
            (calls[0].clone(), None),
            (
                calls[1].clone(),
                Some(
                    serde_json::to_value(RestateDurableWaitRegistration::Registered)
                        .expect("serialize registered observation"),
                ),
            ),
        ],
        Some((
            17,
            serde_json::to_value(RestateTurnCancelWake::SessionRevoked)
                .expect("serialize registered-session revocation"),
        )),
    )
    .expect("splice registered-then-revoked process await");
    let output = invoke_endpoint_body(&endpoint, "Fig790ProcessAwaitRedrive", "run", replay)
        .await
        .expect("registered revocation must terminalize the dead turn");

    assert!(
        restate_call_frames(&output)
            .expect("decode registered-revocation calls")
            .is_empty(),
        "registered session revocation must emit no process cancel"
    );
    assert!(
        restate_output_failure_message(&output)
            .is_some_and(|failure| failure.contains("used and deleted")),
        "registered revocation must preserve typed SessionDeleted settlement"
    );
    assert!(
        !registry
            .get_process(&ProcessId::from(process_id))
            .await
            .expect("registered revocation keeps its process record")
            .expect("registered revocation keeps the process present")
            .is_terminal(),
        "registered session revocation must not terminalize the process"
    );
}

#[tokio::test]
pub(super) async fn fig790_process_terminal_wins_when_terminal_and_cancellation_are_both_ready() {
    let process_id = "fig790-terminal-and-cancel-ready";
    let (endpoint, _registry) = fig790_process_await_endpoint(&ProcessId::from(process_id)).await;
    let input = Fig790ProcessAwaitRedriveInput {
        process_ref: lash_core::ProcessRef::new(
            process_id,
            lash_core::ProcessIncarnation::from_registration_sequence(1),
        ),
        cancel_on_suspend_wake: false,
    };
    let suspended = invoke_endpoint(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        "fig790-terminal-and-cancel-ready",
        &input,
    )
    .await
    .expect("capture process-await race commands");
    let calls = restate_call_frames(&suspended).expect("decode process-await race calls");
    assert_eq!(
        calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["await_terminal", "register_awakeable"]
    );
    let terminal = process_success(serde_json::json!({ "winner": "process_terminal" }));
    let replay = encode_call_replay(
        "fig790-terminal-and-cancel-ready",
        &input,
        &[
            (
                calls[0].clone(),
                Some(serde_json::to_value(&terminal).expect("serialize process terminal")),
            ),
            (
                calls[1].clone(),
                Some(
                    serde_json::to_value(RestateDurableWaitRegistration::Registered)
                        .expect("serialize registered observation"),
                ),
            ),
        ],
        Some((
            17,
            serde_json::to_value(RestateTurnCancelWake::TurnCancelled)
                .expect("serialize simultaneously-ready cancellation"),
        )),
    )
    .expect("splice both-ready process-await race");
    let output = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        replay,
        vec![serde_json::Value::Null],
    )
    .await
    .expect("process terminal must win the biased Restate handle order");

    assert_eq!(
        restate_call_frames(&output)
            .expect("decode both-ready appended calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["unregister_awakeable"],
        "the both-ready race must not append process cancellation"
    );
    assert_eq!(
        restate_output_json::<ProcessAwaitOutput>(&output),
        Some(terminal)
    );
}

#[tokio::test]
pub(super) async fn fig790_cancel_during_suspension_of_a_process_turn_composes_with_fig779() {
    let process_id = "fig790-cancel-during-suspension";
    let (endpoint, _registry) = fig790_process_await_endpoint(&ProcessId::from(process_id)).await;
    let input = Fig790ProcessAwaitRedriveInput {
        process_ref: lash_core::ProcessRef::new(
            process_id,
            lash_core::ProcessIncarnation::from_registration_sequence(1),
        ),
        cancel_on_suspend_wake: false,
    };
    let registered = serde_json::to_value(RestateDurableWaitRegistration::Registered)
        .expect("serialize registered turn-cancel wait");

    let registering = invoke_endpoint(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        "fig790-cancel-during-suspension",
        &input,
    )
    .await
    .expect("turn-cancel registration must suspend cleanly");
    assert_eq!(
        restate_message_types(&registering).expect("decode turn-cancel registration frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]
    );
    let registration_calls =
        restate_call_frames(&registering).expect("decode turn-cancel registration call");
    assert_eq!(
        registration_calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["await_terminal", "register_awakeable"]
    );

    let registered_replay = encode_call_replay(
        "fig790-cancel-during-suspension",
        &input,
        &[
            (registration_calls[0].clone(), None),
            (registration_calls[1].clone(), Some(registered.clone())),
        ],
        None,
    )
    .expect("splice registered turn-cancel observation");
    let process_suspended = invoke_endpoint_body(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        registered_replay,
    )
    .await
    .expect("process await must preserve suspension precedence");
    assert_eq!(
        restate_message_types(&process_suspended).expect("decode suspended process-await frames"),
        vec![RESTATE_SUSPENSION_MESSAGE_TYPE],
        "the durable process await must suspend before later cancellation is observed"
    );
    assert!(
        restate_call_frames(&process_suspended)
            .expect("decode suspended process-await calls")
            .is_empty()
    );

    let cancelled = fig790_cancelled_process_output(&ProcessId::from(process_id));
    let replay = encode_call_replay(
        "fig790-cancel-during-suspension",
        &input,
        &[
            (registration_calls[0].clone(), None),
            (registration_calls[1].clone(), Some(registered)),
        ],
        Some((
            17,
            serde_json::to_value(RestateTurnCancelWake::TurnCancelled)
                .expect("serialize durable turn cancellation"),
        )),
    )
    .expect("splice suspended process-await journal and cancellation signal");
    let redriven = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        replay,
        vec![
            serde_json::Value::Null,
            serde_json::to_value(&cancelled).expect("serialize cancelled process terminal"),
        ],
    )
    .await
    .expect("cancel-during-suspension redrive should finish deterministically");
    assert_eq!(
        restate_call_frames(&redriven)
            .expect("decode post-cancellation calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["cancel", "await_terminal"]
    );
    assert_eq!(
        restate_message_types(&redriven).expect("decode cancellation redrive frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE,
            RESTATE_END_MESSAGE_TYPE
        ]
    );
    assert_eq!(
        restate_output_json::<ProcessAwaitOutput>(&redriven),
        Some(cancelled)
    );
}

#[tokio::test]
pub(super) async fn fig790_second_await_terminal_suspension_redrives_after_journaled_cancel() {
    let process_id = "fig790-second-await-redrive";
    let (endpoint, _registry) = fig790_process_await_endpoint(&ProcessId::from(process_id)).await;
    let input = Fig790ProcessAwaitRedriveInput {
        process_ref: lash_core::ProcessRef::new(
            process_id,
            lash_core::ProcessIncarnation::from_registration_sequence(1),
        ),
        cancel_on_suspend_wake: false,
    };
    let initial = invoke_endpoint(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        "fig790-second-await-redrive",
        &input,
    )
    .await
    .expect("capture initial process-await calls");
    let initial_calls = restate_call_frames(&initial).expect("decode initial process-await calls");
    let registered = serde_json::to_value(RestateDurableWaitRegistration::Registered)
        .expect("serialize registered turn cancellation");
    let cancellation_signal = Some((
        17,
        serde_json::to_value(RestateTurnCancelWake::TurnCancelled)
            .expect("serialize turn cancellation"),
    ));
    let cancellation_replay = encode_call_replay(
        "fig790-second-await-redrive",
        &input,
        &[
            (initial_calls[0].clone(), None),
            (initial_calls[1].clone(), Some(registered.clone())),
        ],
        cancellation_signal.clone(),
    )
    .expect("splice cancellation-winning process-await journal");
    let cancel_suspended = invoke_endpoint_body(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        cancellation_replay,
    )
    .await
    .expect("suspend on the process-cancel command");
    assert_eq!(
        restate_message_types(&cancel_suspended).expect("decode process-cancel suspension frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE,
        ]
    );
    let cancel_calls = restate_call_frames(&cancel_suspended).expect("decode journaled cancel");
    let [cancel_call] = cancel_calls.as_slice() else {
        panic!("cancellation winner must append exactly one process cancel");
    };
    assert_eq!(cancel_call.handler, "cancel");

    let post_cancel_replay = encode_call_replay(
        "fig790-second-await-redrive",
        &input,
        &[
            (initial_calls[0].clone(), None),
            (initial_calls[1].clone(), Some(registered.clone())),
            (cancel_call.clone(), Some(serde_json::Value::Null)),
        ],
        cancellation_signal.clone(),
    )
    .expect("splice completed process cancel");
    let second_await_suspended = invoke_endpoint_body(
        &endpoint,
        "Fig790ProcessAwaitRedrive",
        "run",
        post_cancel_replay,
    )
    .await
    .expect("suspend on the second process terminal await");
    assert_eq!(
        restate_message_types(&second_await_suspended)
            .expect("decode second-await suspension frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE,
        ]
    );
    let second_await_calls =
        restate_call_frames(&second_await_suspended).expect("decode second terminal await");
    let [second_await_call] = second_await_calls.as_slice() else {
        panic!("post-cancel suspension must append exactly one terminal await");
    };
    assert_eq!(second_await_call.handler, "await_terminal");

    let cancelled = fig790_cancelled_process_output(&ProcessId::from(process_id));
    let redrive = encode_call_replay(
        "fig790-second-await-redrive",
        &input,
        &[
            (initial_calls[0].clone(), None),
            (initial_calls[1].clone(), Some(registered)),
            (cancel_call.clone(), Some(serde_json::Value::Null)),
            (
                second_await_call.clone(),
                Some(
                    serde_json::to_value(&cancelled)
                        .expect("serialize second-await process terminal"),
                ),
            ),
        ],
        cancellation_signal,
    )
    .expect("splice suspended second-await journal");
    let output = invoke_endpoint_body(&endpoint, "Fig790ProcessAwaitRedrive", "run", redrive)
        .await
        .expect("redrive must resume the journaled post-cancel terminal await");
    assert!(
        restate_call_frames(&output)
            .expect("decode second-await redrive calls")
            .is_empty(),
        "redrive must consume the exact journal without appending another cancel"
    );
    assert_eq!(
        restate_output_json::<ProcessAwaitOutput>(&output),
        Some(cancelled)
    );
}

#[test]
pub(super) fn restate_session_cancel_sweep_excludes_turn_control_addresses() {
    let scope = durable_turn_scope("session", "turn");
    let durable_wait =
        restate_await_event_key(&scope, AwaitEventWaitIdentity::tool_completion("tool-wait"))
            .expect("durable wait key");
    let cancel_gate = restate_await_event_key(&scope, AwaitEventWaitIdentity::TurnCancelGate)
        .expect("turn-cancel key");
    let terminal = restate_await_event_key(&scope, AwaitEventWaitIdentity::TurnTerminal)
        .expect("turn-terminal key");

    let (cancelled, retained) = split_cancellable_waits(vec![
        durable_wait.clone(),
        cancel_gate.clone(),
        terminal.clone(),
    ]);
    assert_eq!(cancelled, vec![durable_wait]);
    assert_eq!(retained, vec![cancel_gate, terminal]);
}

// FIG-1631 fixtures: a turn-scoped sleep, which is the second caller of the
// shared turn-cancel gate. These pin the gate's journal geometry and its
// outcomes from the sleep side, so a change that only happened to keep the
// process-await tests green still has to answer for the sleep call site.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub(super) struct Fig1631SleepGateInput {
    duration_ms: u64,
}

#[restate_sdk::workflow]
trait Fig1631SleepGate {
    async fn run(input: Json<Fig1631SleepGateInput>) -> HandlerResult<Json<String>>;
}

pub(super) struct Fig1631SleepGateImpl;

impl Fig1631SleepGate for Fig1631SleepGateImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Fig1631SleepGateInput>,
    ) -> HandlerResult<Json<String>> {
        let controller = RestateRuntimeEffectController::new(ctx);
        let outcome = controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::Sleep, "fig1631-sleep-gate"),
                    RuntimeEffectCommand::Sleep {
                        spec: lash_core::SleepSpec::For {
                            duration_ms: input.duration_ms,
                        },
                    },
                ),
                RuntimeEffectLocalExecutor::sleep(tokio_util::sync::CancellationToken::new())
                    .with_turn_cancel_scope(durable_turn_scope("session", "turn")),
            )
            .await;
        Ok(Json(match outcome {
            Ok(RuntimeEffectOutcome::Sleep) => "slept".to_string(),
            Ok(other) => format!("unexpected outcome: {other:?}"),
            Err(error) => match &error.cause {
                Some(lash_core::RuntimeErrorCause::SessionDeleted { session_id }) => {
                    format!("session_deleted:{session_id}")
                }
                _ => error.code.to_string(),
            },
        }))
    }
}

pub(super) fn fig1631_sleep_gate_endpoint() -> Endpoint {
    Endpoint::builder()
        .bind(Fig1631SleepGateImpl.serve())
        .build()
}

pub(super) fn fig1631_sleep_gate_input() -> Fig1631SleepGateInput {
    Fig1631SleepGateInput {
        duration_ms: 60_000,
    }
}

// FIG-1631 fixtures: a turn-scoped await-event, the third caller of the shared
// gate. Before this change it raced through a nested workflow handler instead,
// so these pin the migrated journal geometry and both retirement paths.
// The scope must match `runtime_invocation`, or the effect is refused for a
// turn-cancel scope mismatch before it journals anything.
pub(super) const FIG1631_AWAIT_SESSION: &str = "session";

#[restate_sdk::workflow]
trait Fig1631AwaitEventGate {
    async fn run(input: Json<Fig1126PendingToolRedriveInput>) -> HandlerResult<Json<String>>;
}

pub(super) struct Fig1631AwaitEventGateImpl;

impl Fig1631AwaitEventGate for Fig1631AwaitEventGateImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(_input): Json<Fig1126PendingToolRedriveInput>,
    ) -> HandlerResult<Json<String>> {
        let scope = durable_turn_scope(FIG1631_AWAIT_SESSION, "turn");
        let key = restate_await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion("fig1631-await-call"),
        )
        .map_err(TerminalError::from_error)?;
        let outcome = RestateRuntimeEffectController::new(ctx)
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    runtime_invocation(RuntimeEffectKind::AwaitEvent, "fig1631-await-gate"),
                    RuntimeEffectCommand::AwaitEvent { key },
                ),
                RuntimeEffectLocalExecutor::await_event(
                    tokio_util::sync::CancellationToken::new(),
                    None,
                )
                .with_turn_cancel_scope(scope),
            )
            .await;
        Ok(Json(match outcome {
            Ok(RuntimeEffectOutcome::AwaitEvent { resolution }) => {
                serde_json::to_string(&resolution).map_err(TerminalError::from_error)?
            }
            Ok(other) => format!("unexpected outcome: {other:?}"),
            Err(error) => match &error.cause {
                Some(lash_core::RuntimeErrorCause::SessionDeleted { session_id }) => {
                    format!("session_deleted:{session_id}")
                }
                _ => error.code.to_string(),
            },
        }))
    }
}

pub(super) fn fig1631_await_event_endpoint() -> Endpoint {
    Endpoint::builder()
        .bind(Fig1631AwaitEventGateImpl.serve())
        .build()
}

pub(super) fn fig1631_resolution_label(resolution: &Resolution) -> String {
    serde_json::to_string(resolution).expect("serialize resolution label")
}

/// Park a turn-scoped await-event on its gate and pin the journal positions.
///
/// The migrated gate journals the event call, then its gate awakeable, then the
/// registration — the same geometry as process await, and the positions every
/// redrive below lands on.
pub(super) async fn fig1631_parked_await_event_gate(
    endpoint: &Endpoint,
    workflow_key: &str,
) -> Vec<endpoint_protocol::RestateCallFrame> {
    let parked = invoke_endpoint_with_named_call_responses(
        endpoint,
        "Fig1631AwaitEventGate",
        "run",
        workflow_key,
        &Fig1126PendingToolRedriveInput,
        vec![("is_revoked".to_string(), serde_json::json!(false))],
    )
    .await
    .expect("park the turn-scoped await-event on its gate");
    let calls = restate_call_frames(&parked).expect("decode await-event gate calls");
    assert_eq!(
        calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["is_revoked", "await_resolution", "register_awakeable"],
        "the await-event gate replaces the nested workflow hop with one gate registration"
    );
    assert!(
        restate_message_types(&parked)
            .expect("decode parked await-event frames")
            .contains(&RESTATE_SUSPENSION_MESSAGE_TYPE),
        "the gate must park once both the event and its gate are journaled"
    );
    calls
}

#[tokio::test]
pub(super) async fn fig1631_await_event_gate_journals_the_event_before_its_gate() {
    let endpoint = fig1631_await_event_endpoint();
    fig1631_parked_await_event_gate(&endpoint, "fig1631-await-gate-positions").await;
}

/// Completion path: the winning event retires the gate entry it registered.
#[tokio::test]
pub(super) async fn fig1631_await_event_completion_retires_its_gate_entry() {
    let endpoint = fig1631_await_event_endpoint();
    let terminal = Resolution::Ok(serde_json::json!({ "answer": "gated" }));
    let completed = invoke_endpoint_with_named_call_responses(
        &endpoint,
        "Fig1631AwaitEventGate",
        "run",
        "fig1631-await-gate-completion",
        &Fig1126PendingToolRedriveInput,
        vec![
            ("is_revoked".to_string(), serde_json::json!(false)),
            ("register_awakeable".to_string(), fig1631_registered_gate()),
            (
                "await_resolution".to_string(),
                serde_json::to_value(&terminal).expect("serialize awaited resolution"),
            ),
            ("unregister_awakeable".to_string(), serde_json::Value::Null),
        ],
    )
    .await
    .expect("the event must win and retire its gate");
    assert_eq!(
        restate_call_frames(&completed)
            .expect("decode completion-path calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec![
            "is_revoked",
            "await_resolution",
            "register_awakeable",
            "unregister_awakeable"
        ],
        "a completed await-event must retire exactly the gate entry it registered"
    );
    assert_eq!(
        restate_output_json::<String>(&completed).as_deref(),
        Some(fig1631_resolution_label(&terminal).as_str())
    );
}

/// Cancel path: the index already dropped the entry, so the waiter must not
/// unregister it again — but it must release the losing event wait, which the
/// retired nested workflow used to do from its own journal.
#[tokio::test]
pub(super) async fn fig1631_turn_cancelled_await_event_releases_the_losing_event_wait() {
    let endpoint = fig1631_await_event_endpoint();
    let workflow_key = "fig1631-await-gate-cancel"; // gitleaks:allow -- synthetic workflow/turn identity fixture
    let calls = fig1631_parked_await_event_gate(&endpoint, workflow_key).await;

    let replay = encode_call_replay(
        workflow_key,
        &Fig1126PendingToolRedriveInput,
        &[
            (calls[0].clone(), Some(serde_json::json!(false))),
            (calls[1].clone(), None),
            (calls[2].clone(), Some(fig1631_registered_gate())),
        ],
        Some((
            17,
            serde_json::to_value(RestateTurnCancelWake::TurnCancelled)
                .expect("serialize turn cancellation"),
        )),
    )
    .expect("splice a turn cancellation over the parked await-event gate");
    let cancelled = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig1631AwaitEventGate",
        "run",
        replay,
        vec![serde_json::to_value(ResolveOutcome::Accepted).expect("serialize resolve outcome")],
    )
    .await
    .expect("turn cancellation must resolve the parked await-event");
    assert_eq!(
        restate_call_frames(&cancelled)
            .expect("decode cancel-path calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["resolve"],
        "the cancel path releases the losing event wait and leaves gate retirement to the index"
    );
    assert_eq!(
        restate_output_json::<String>(&cancelled).as_deref(),
        Some(fig1631_resolution_label(&Resolution::Cancelled).as_str())
    );
}

/// A session revoked out from under a parked await-event unwinds the turn as a
/// deleted session, the same new outcome the sleep gate now reports.
#[tokio::test]
pub(super) async fn fig1631_session_revoked_await_event_unwinds_as_a_deleted_session() {
    let endpoint = fig1631_await_event_endpoint();
    let workflow_key = "fig1631-await-gate-revoked"; // gitleaks:allow -- synthetic workflow/turn identity fixture
    let calls = fig1631_parked_await_event_gate(&endpoint, workflow_key).await;

    let replay = encode_call_replay(
        workflow_key,
        &Fig1126PendingToolRedriveInput,
        &[
            (calls[0].clone(), Some(serde_json::json!(false))),
            (calls[1].clone(), None),
            (
                calls[2].clone(),
                Some(
                    serde_json::to_value(RestateDurableWaitRegistration::Revoked)
                        .expect("serialize revoked registration"),
                ),
            ),
        ],
        None,
    )
    .expect("splice a revoked gate registration");
    let revoked = invoke_endpoint_body(&endpoint, "Fig1631AwaitEventGate", "run", replay)
        .await
        .expect("a revoked registration must unwind the await-event");
    assert_eq!(
        restate_output_json::<String>(&revoked).as_deref(),
        Some(format!("session_deleted:{FIG1631_AWAIT_SESSION}").as_str())
    );
}

pub(super) fn fig1631_registered_gate() -> serde_json::Value {
    serde_json::to_value(RestateDurableWaitRegistration::Registered)
        .expect("serialize registered gate")
}

/// Walk a turn-scoped sleep to the point where its gate is live and its timer
/// is journaled.
///
/// The two stages are the deployed journal shape and the reason the gate is
/// ordered the way it is: the first attempt parks on `register_awakeable`, so
/// no timer is ever journaled until the session is known to be live. Only once
/// that registration completes does the timer become a command.
pub(super) async fn fig1631_parked_sleep_gate(
    endpoint: &Endpoint,
    workflow_key: &str,
) -> (Vec<u8>, Vec<endpoint_protocol::RestateCallFrame>) {
    let registering = invoke_endpoint(
        endpoint,
        "Fig1631SleepGate",
        "run",
        workflow_key,
        &fig1631_sleep_gate_input(),
    )
    .await
    .expect("capture the gate registration");
    let calls = restate_call_frames(&registering).expect("decode sleep-gate calls");
    assert_eq!(
        calls
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["register_awakeable"],
        "the sleep gate registers before it can park on anything"
    );
    assert_eq!(
        restate_message_types(&registering).expect("decode registration frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE,
        ],
        "no timer may be journaled before the gate knows the session is live"
    );

    let replay = encode_call_replay(
        workflow_key,
        &fig1631_sleep_gate_input(),
        &[(calls[0].clone(), Some(fig1631_registered_gate()))],
        None,
    )
    .expect("splice the completed gate registration");
    let parked = invoke_endpoint_body(endpoint, "Fig1631SleepGate", "run", replay)
        .await
        .expect("park on the gate's timer");
    assert_eq!(
        restate_message_types(&parked).expect("decode parked timer frames"),
        vec![
            RESTATE_SLEEP_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE,
        ],
        "a registered gate journals its timer next and parks on it"
    );
    (parked.to_vec(), calls)
}

/// The gate's journal positions are the contract: a fresh handler incarnation
/// must accept the recorded registration and timer and carry the sleep to its
/// ordinary completion.
#[tokio::test]
pub(super) async fn fig1631_sleep_gate_redrives_from_its_journal_positions() {
    let endpoint = fig1631_sleep_gate_endpoint();
    let workflow_key = "fig1631-sleep-gate-redrive";
    let (parked, calls) = fig1631_parked_sleep_gate(&endpoint, workflow_key).await;

    let replay = encode_completed_gate_sleep_replay(
        workflow_key,
        &fig1631_sleep_gate_input(),
        &parked,
        &[(calls[0].clone(), Some(fig1631_registered_gate()))],
    )
    .expect("splice the parked gate journal with a fired timer");
    let redriven = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig1631SleepGate",
        "run",
        replay,
        vec![serde_json::Value::Null],
    )
    .await
    .expect("redrive the parked sleep gate");
    assert!(
        restate_error_message(&redriven).is_none(),
        "redrive must accept the recorded gate prefix: {:?}",
        restate_error_message(&redriven)
    );
    assert_eq!(
        restate_output_json::<String>(&redriven).as_deref(),
        Some("slept")
    );
}

/// The new observable outcome: a session revoked while the sleep is parked
/// unwinds the turn as a deleted session instead of reporting a plain
/// cancellation.
#[tokio::test]
pub(super) async fn fig1631_session_revoked_mid_sleep_unwinds_as_a_deleted_session() {
    let endpoint = fig1631_sleep_gate_endpoint();
    let workflow_key = "fig1631-sleep-gate-revoked-mid-sleep"; // gitleaks:allow -- synthetic workflow/turn identity fixture
    let (_parked, calls) = fig1631_parked_sleep_gate(&endpoint, workflow_key).await;

    let replay = encode_call_replay(
        workflow_key,
        &fig1631_sleep_gate_input(),
        &[(calls[0].clone(), Some(fig1631_registered_gate()))],
        Some((
            17,
            serde_json::to_value(RestateTurnCancelWake::SessionRevoked)
                .expect("serialize mid-sleep revocation"),
        )),
    )
    .expect("splice a revocation that fires after the gate registered");
    let revoked = invoke_endpoint_body(&endpoint, "Fig1631SleepGate", "run", replay)
        .await
        .expect("revocation must resolve the parked sleep");
    assert_eq!(
        restate_output_json::<String>(&revoked).as_deref(),
        Some("session_deleted:session"),
        "a revoked session must not be reported as an ordinary sleep cancellation"
    );
}

/// A session already revoked when the gate registers takes the same exit
/// without ever journaling a timer.
#[tokio::test]
pub(super) async fn fig1631_session_revoked_before_sleep_registers_never_journals_a_timer() {
    let endpoint = fig1631_sleep_gate_endpoint();
    let revoked = invoke_endpoint_with_named_call_responses(
        &endpoint,
        "Fig1631SleepGate",
        "run",
        "fig1631-sleep-gate-revoked-at-registration",
        &fig1631_sleep_gate_input(),
        vec![(
            "register_awakeable".to_string(),
            serde_json::to_value(RestateDurableWaitRegistration::Revoked)
                .expect("serialize revoked registration"),
        )],
    )
    .await
    .expect("a revoked registration must unwind without parking");
    assert_eq!(
        restate_message_types(&revoked).expect("decode revoked-registration frames"),
        vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE,
            RESTATE_END_MESSAGE_TYPE,
        ],
        "a revoked session must not journal a timer it will never wait on"
    );
    assert_eq!(
        restate_output_json::<String>(&revoked).as_deref(),
        Some("session_deleted:session")
    );
}

/// Completion path: the winning timer must hand the gate entry back, or the
/// index keeps owing a wake to an awakeable nobody is holding.
#[tokio::test]
pub(super) async fn fig1631_sleep_completion_retires_its_gate_entry() {
    let endpoint = fig1631_sleep_gate_endpoint();
    let workflow_key = "fig1631-sleep-gate-completion-retires"; // gitleaks:allow -- synthetic workflow/turn identity fixture
    let (parked, calls) = fig1631_parked_sleep_gate(&endpoint, workflow_key).await;

    let replay = encode_completed_gate_sleep_replay(
        workflow_key,
        &fig1631_sleep_gate_input(),
        &parked,
        &[(calls[0].clone(), Some(fig1631_registered_gate()))],
    )
    .expect("splice a fired timer over the parked gate");
    let completed = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "Fig1631SleepGate",
        "run",
        replay,
        vec![serde_json::Value::Null],
    )
    .await
    .expect("the timer must win and retire the gate");
    assert_eq!(
        restate_call_frames(&completed)
            .expect("decode completion-path calls")
            .iter()
            .map(|call| call.handler.as_str())
            .collect::<Vec<_>>(),
        vec!["unregister_awakeable"],
        "a completed sleep must retire exactly the gate entry it registered"
    );
}

/// Cancel path: the index resolved the awakeable, so it has already dropped the
/// entry. A second `unregister_awakeable` here would be the waiter clearing
/// state it no longer owns.
#[tokio::test]
pub(super) async fn fig1631_turn_cancelled_sleep_leaves_gate_retirement_to_the_index() {
    let endpoint = fig1631_sleep_gate_endpoint();
    let workflow_key = "fig1631-sleep-gate-cancel-retires"; // gitleaks:allow -- synthetic workflow/turn identity fixture
    let (_parked, calls) = fig1631_parked_sleep_gate(&endpoint, workflow_key).await;

    let replay = encode_call_replay(
        workflow_key,
        &fig1631_sleep_gate_input(),
        &[(calls[0].clone(), Some(fig1631_registered_gate()))],
        Some((
            17,
            serde_json::to_value(RestateTurnCancelWake::TurnCancelled)
                .expect("serialize turn cancellation"),
        )),
    )
    .expect("splice a turn cancellation over the parked gate");
    let cancelled = invoke_endpoint_body(&endpoint, "Fig1631SleepGate", "run", replay)
        .await
        .expect("turn cancellation must resolve the parked sleep");
    assert!(
        restate_call_frames(&cancelled)
            .expect("decode cancel-path calls")
            .is_empty(),
        "the index owns the entry it just resolved; the waiter must not unregister it again"
    );
    assert_eq!(
        restate_output_json::<String>(&cancelled).as_deref(),
        Some("runtime_effect_sleep_cancelled")
    );
}

pub(super) fn fig1943_put_varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        buf.push(((value as u8) & 0x7f) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

pub(super) fn fig1943_put_len_field(buf: &mut Vec<u8>, field: u64, value: &[u8]) {
    fig1943_put_varint(buf, (field << 3) | 2);
    fig1943_put_varint(buf, value.len() as u64);
    buf.extend_from_slice(value);
}

pub(super) fn fig1943_encode_message(message_type: u16, payload: &[u8]) -> Bytes {
    let header = ((message_type as u64) << 48) | payload.len() as u64;
    let mut encoded = Vec::with_capacity(8 + payload.len());
    encoded.extend_from_slice(&header.to_be_bytes());
    encoded.extend_from_slice(payload);
    Bytes::from(encoded)
}

pub(super) fn fig1943_invocation_with_state<T: Serialize>(
    object_key: &str,
    input: &T,
    state: &BTreeMap<String, Vec<u8>>,
) -> Bytes {
    let mut start = Vec::new();
    fig1943_put_len_field(&mut start, 1, object_key.as_bytes());
    fig1943_put_len_field(&mut start, 2, object_key.as_bytes());
    fig1943_put_varint(&mut start, 3 << 3);
    fig1943_put_varint(&mut start, 1);
    for (key, value) in state {
        let mut entry = Vec::new();
        fig1943_put_len_field(&mut entry, 1, key.as_bytes());
        fig1943_put_len_field(&mut entry, 2, value);
        fig1943_put_len_field(&mut start, 4, &entry);
    }
    fig1943_put_len_field(&mut start, 6, object_key.as_bytes());

    let input = serde_json::to_vec(input).expect("serialize FIG-1943 handler input");
    let mut input_value = Vec::new();
    fig1943_put_len_field(&mut input_value, 1, &input);
    let mut input_command = Vec::new();
    fig1943_put_len_field(&mut input_command, 14, &input_value);

    let start = fig1943_encode_message(0x0000, &start);
    let input = fig1943_encode_message(0x0400, &input_command);
    let mut body = Vec::with_capacity(start.len() + input.len());
    body.extend_from_slice(&start);
    body.extend_from_slice(&input);
    Bytes::from(body)
}

pub(super) fn fig1943_decode_varint(input: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut value = 0_u64;
    let mut shift = 0;
    loop {
        let byte = *input.get(*cursor)?;
        *cursor += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

pub(super) fn fig1943_len_field(input: &[u8], target: u64) -> Option<&[u8]> {
    let mut cursor = 0;
    while cursor < input.len() {
        let key = fig1943_decode_varint(input, &mut cursor)?;
        match key & 7 {
            0 => {
                let _ = fig1943_decode_varint(input, &mut cursor)?;
            }
            2 => {
                let len = usize::try_from(fig1943_decode_varint(input, &mut cursor)?).ok()?;
                let end = cursor.checked_add(len)?;
                let value = input.get(cursor..end)?;
                if key >> 3 == target {
                    return Some(value);
                }
                cursor = end;
            }
            _ => return None,
        }
    }
    None
}

pub(super) fn fig1943_apply_state_commands(state: &mut BTreeMap<String, Vec<u8>>, output: &[u8]) {
    let mut cursor = 0;
    while cursor < output.len() {
        let header = u64::from_be_bytes(
            output[cursor..cursor + 8]
                .try_into()
                .expect("FIG-1943 Restate frame header"),
        );
        let message_type = (header >> 48) as u16;
        let payload_len = usize::try_from(header & 0x0000_FFFF_FFFF_FFFF)
            .expect("FIG-1943 Restate frame payload length");
        let frame_end = cursor + 8 + payload_len;
        let payload = &output[cursor + 8..frame_end];
        match message_type {
            0x0403 => {
                let key = String::from_utf8(
                    fig1943_len_field(payload, 1)
                        .expect("FIG-1943 set-state key")
                        .to_vec(),
                )
                .expect("FIG-1943 UTF-8 state key");
                let value = fig1943_len_field(
                    fig1943_len_field(payload, 3).expect("FIG-1943 set-state value wrapper"),
                    1,
                )
                .expect("FIG-1943 set-state value")
                .to_vec();
                state.insert(key, value);
            }
            0x0404 => {
                let key = String::from_utf8(
                    fig1943_len_field(payload, 1)
                        .expect("FIG-1943 clear-state key")
                        .to_vec(),
                )
                .expect("FIG-1943 UTF-8 state key");
                state.remove(&key);
            }
            0x0405 => state.clear(),
            _ => {}
        }
        cursor = frame_end;
    }
}

#[tokio::test]
pub(super) async fn durable_wait_workflow_rejects_a_key_for_a_different_workflow_address() {
    let endpoint = Endpoint::builder()
        .bind(LashDurableWaitWorkflowImpl.serve())
        .build();
    let key = restate_await_event_key(
        &ExecutionScope::runtime_operation("fig2005-forged-address"),
        AwaitEventWaitIdentity::Custom {
            key: "fig2005-forged-address".to_string(),
        },
    )
    .expect("derive FIG-2005 wait key");
    let expected_workflow_key = RestateDurableWaitAddress::for_key(&key).workflow_key;

    let output = invoke_endpoint(
        &endpoint,
        "LashDurableWaitWorkflow",
        "resolve",
        "forged-workflow-key",
        &RestateDurableWaitResolveRequest {
            key: key.clone(),
            resolution: Resolution::Cancelled,
        },
    )
    .await
    .expect("invoke forged FIG-2005 workflow request");
    let error = restate_output_failure_message(&output)
        .or_else(|| restate_error_message(&output))
        .expect("a mismatched wait-key preimage must fail the workflow invocation");
    assert!(error.contains("durable-wait workflow key mismatch"));
    assert!(error.contains(&expected_workflow_key));
    assert!(error.contains("forged-workflow-key"));
}

#[tokio::test]
pub(super) async fn durable_wait_index_rejects_an_inconsistent_key_preimage_before_state_write() {
    let endpoint = Endpoint::builder()
        .bind(LashDurableWaitIndexImpl.serve())
        .build();
    let scope = durable_turn_scope("fig2005-forged-session", "fig2005-forged-turn");
    let mut key = restate_await_event_key(&scope, AwaitEventWaitIdentity::TurnCancelGate)
        .expect("derive FIG-2005 turn-control key");
    key.wait = AwaitEventWaitIdentity::tool_completion("fig2005-forged-tool-completion");
    let address = RestateDurableWaitAddress::for_key(&key);
    assert_eq!(
        address.classification,
        RestateDurableWaitClassification::DurableWait,
        "the forged wait must target the sweepable partition"
    );
    let object_key = address.index_key();
    let mut state = BTreeMap::new();

    let output = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "register",
        fig1943_invocation_with_state(
            &object_key,
            &RestateDurableWaitIndexRequest { key: key.clone() },
            &state,
        ),
    )
    .await
    .expect("invoke FIG-2005 forged-key registration");
    let registration = restate_output_json::<RestateDurableWaitRegistration>(&output);
    fig1943_apply_state_commands(&mut state, &output);
    let state_key = durable_wait_index_state_key(&address);
    let error = restate_output_failure_message(&output).or_else(|| restate_error_message(&output));
    assert!(
        error.is_some(),
        "inconsistent key was accepted as {registration:?} and stored in the sweepable partition: {}",
        state.contains_key(&state_key)
    );
    assert!(
        error
            .expect("inconsistent key preimage must fail")
            .contains("inconsistent durable-wait key preimage"),
        "terminal error must name the key-preimage inconsistency"
    );
    assert!(state.is_empty(), "terminal rejection must not write state");
}

#[test]
pub(super) fn durable_wait_register_and_sweep_derive_the_same_address_for_every_scope() {
    let scopes = [
        durable_turn_scope("fig2005-session", "fig2005-turn"),
        ExecutionScope::process("fig2005-process"),
        ExecutionScope::queue_drain("fig2005-session", "fig2005-drain"),
        ExecutionScope::session_delete("fig2005-session"),
        ExecutionScope::runtime_operation("fig2005-operation"),
    ];

    for (ordinal, scope) in scopes.into_iter().enumerate() {
        let key = restate_await_event_key(
            &scope,
            AwaitEventWaitIdentity::Custom {
                key: format!("fig2005-round-trip-{ordinal}"),
            },
        )
        .expect("derive FIG-2005 round-trip key");
        let registered = RestateDurableWaitAddress::for_key(&key);
        let swept =
            durable_wait_address_from_state_key(&key, &durable_wait_index_state_key(&registered))
                .expect("reconstruct FIG-2005 wait address from index state");
        assert_eq!(swept, registered, "scope {scope:?} changed during sweep");
    }
}

#[tokio::test]
pub(super) async fn fig1943_cancel_all_mirrors_the_workflow_terminal_verdict() {
    let endpoint = Endpoint::builder()
        .bind(LashDurableWaitIndexImpl.serve())
        .build();
    let object_key = "fig1943-session";
    let key = restate_await_event_key(
        &durable_turn_scope(object_key, "fig1943-turn"), // gitleaks:allow -- synthetic workflow/turn identity fixture
        AwaitEventWaitIdentity::tool_completion("fig1943-tool-wait"),
    )
    .expect("derive FIG-1943 tool-wait key");
    let mut state = BTreeMap::new();

    let registered = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "register",
        fig1943_invocation_with_state(
            object_key,
            &RestateDurableWaitIndexRequest { key: key.clone() },
            &state,
        ),
    )
    .await
    .expect("register the FIG-1943 wait");
    assert_eq!(
        restate_output_json::<RestateDurableWaitRegistration>(&registered),
        Some(RestateDurableWaitRegistration::Registered)
    );
    fig1943_apply_state_commands(&mut state, &registered);
    let state_key = durable_wait_index_state_key(&RestateDurableWaitAddress::for_key(&key));
    let indexed_key: AwaitEventKey = serde_json::from_slice(
        state
            .get(&state_key)
            .expect("FIG-2005 index state stores the key preimage"),
    )
    .expect("decode FIG-2005 indexed key preimage");
    assert_eq!(indexed_key, key);

    let terminal = Resolution::Ok(serde_json::json!({ "tool_result": "complete" }));
    let resolved = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "LashDurableWaitIndex",
        "resolve",
        fig1943_invocation_with_state(
            object_key,
            &RestateDurableWaitResolveRequest {
                key: key.clone(),
                resolution: terminal.clone(),
            },
            &state,
        ),
        vec![serde_json::to_value(ResolveOutcome::Accepted).expect("serialize accepted verdict")],
    )
    .await
    .expect("resolve the FIG-1943 wait with a value");
    assert_eq!(
        restate_output_json::<ResolveOutcome>(&resolved),
        Some(ResolveOutcome::Accepted)
    );
    fig1943_apply_state_commands(&mut state, &resolved);

    let cancelled = invoke_endpoint_body_with_json_call_responses(
        &endpoint,
        "LashDurableWaitIndex",
        "cancel_all",
        fig1943_invocation_with_state(object_key, &(), &state),
        vec![
            serde_json::to_value(ResolveOutcome::AlreadyResolved {
                terminal: terminal.clone(),
            })
            .expect("serialize already-resolved verdict"),
        ],
    )
    .await
    .expect("cancel all FIG-1943 session waits");
    fig1943_apply_state_commands(&mut state, &cancelled);

    let reregistered = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "register",
        fig1943_invocation_with_state(object_key, &RestateDurableWaitIndexRequest { key }, &state),
    )
    .await
    .expect("re-register the already-resolved FIG-1943 wait");
    assert_eq!(
        restate_output_json::<RestateDurableWaitRegistration>(&reregistered),
        Some(RestateDurableWaitRegistration::Resolved(terminal))
    );
}

#[tokio::test]
pub(super) async fn outstanding_wait_read_is_pure_and_filters_retained_control_terminals() {
    let endpoint = Endpoint::builder()
        .bind(LashDurableWaitIndexImpl.serve())
        .build();
    let object_key = "fig2946-session";
    let mut state = BTreeMap::new();

    let mint_probe = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "is_revoked",
        fig1943_invocation_with_state(object_key, &(), &state),
    )
    .await
    .expect("probe an unknown FIG-2946 session before minting");
    assert_eq!(restate_output_json::<bool>(&mint_probe), Some(false));
    let before_mint_probe = state.clone();
    fig1943_apply_state_commands(&mut state, &mint_probe);
    assert_eq!(
        state, before_mint_probe,
        "the ingress mint probe writes no state"
    );

    let unknown = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "outstanding",
        fig1943_invocation_with_state(object_key, &(), &state),
    )
    .await
    .expect("list an unknown FIG-2946 session");
    assert_eq!(
        restate_output_json::<Vec<AwaitEventKey>>(&unknown),
        Some(Vec::new())
    );
    let before_read = state.clone();
    fig1943_apply_state_commands(&mut state, &unknown);
    assert_eq!(
        state, before_read,
        "an unknown-session read writes no state"
    );

    let key = restate_await_event_key(
        &durable_turn_scope(object_key, "turn"),
        AwaitEventWaitIdentity::TurnCancelGate,
    )
    .expect("derive FIG-2946 control key");
    let registered = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "register",
        fig1943_invocation_with_state(
            object_key,
            &RestateDurableWaitIndexRequest { key: key.clone() },
            &state,
        ),
    )
    .await
    .expect("register FIG-2946 control wait");
    fig1943_apply_state_commands(&mut state, &registered);
    let state_key = durable_wait_index_state_key(&RestateDurableWaitAddress::for_key(&key));
    assert!(state.contains_key(&state_key));

    let pending = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "outstanding",
        fig1943_invocation_with_state(object_key, &(), &state),
    )
    .await
    .expect("list registered FIG-2946 control wait");
    assert_eq!(
        restate_output_json::<Vec<AwaitEventKey>>(&pending),
        Some(vec![key.clone()])
    );

    let settled = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "settle",
        fig1943_invocation_with_state(
            object_key,
            &RestateDurableWaitSettleRequest {
                key: key.clone(),
                resolution: Resolution::Cancelled,
            },
            &state,
        ),
    )
    .await
    .expect("settle FIG-2946 control wait");
    fig1943_apply_state_commands(&mut state, &settled);
    assert!(
        state.contains_key(&state_key),
        "Restate deliberately retains the settled control preimage"
    );

    let terminal = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "outstanding",
        fig1943_invocation_with_state(object_key, &(), &state),
    )
    .await
    .expect("list after FIG-2946 control settlement");
    assert_eq!(
        restate_output_json::<Vec<AwaitEventKey>>(&terminal),
        Some(Vec::new()),
        "retained control state with a terminal is not outstanding"
    );
}

#[test]
pub(super) fn durable_wait_index_epoch_rejects_legacy_state_and_accepts_fresh_state() {
    let error = validate_durable_wait_index_epoch(None, &["waits".to_string()])
        .expect_err("pre-cutover aggregate state must be rejected");
    assert!(error.contains("drain and recreate"));
    assert!(
        validate_durable_wait_index_epoch(None, &["wait-index/v1/metadata".to_string()])
            .expect_err("v1 wait-index state must be rejected")
            .contains("pre-cutover")
    );
    validate_durable_wait_index_epoch(None, &[]).expect("fresh state opens");
    validate_durable_wait_index_epoch(
        Some(DURABLE_WAIT_INDEX_IDENTITY_EPOCH),
        &[DURABLE_WAIT_INDEX_METADATA_KEY.to_string()],
    )
    .expect("matching epoch reopens current state");
    let wrong_epoch = validate_durable_wait_index_epoch(
        Some(DURABLE_WAIT_INDEX_IDENTITY_EPOCH - 1),
        &[DURABLE_WAIT_INDEX_METADATA_KEY.to_string()],
    )
    .expect_err("wrong identity epoch must be rejected");
    assert!(wrong_epoch.contains("incompatible with epoch 5"));
    assert!(wrong_epoch.contains("drain and recreate"));
    assert!(DURABLE_WAIT_INDEX_METADATA_KEY.starts_with("wait-index/v2/"));
}

#[test]
pub(super) fn durable_wait_identity_epoch_five_rejects_epoch_four_state() {
    let error =
        validate_durable_wait_index_epoch(Some(4), &[DURABLE_WAIT_INDEX_METADATA_KEY.to_string()])
            .expect_err("epoch-4 durable-wait state must not open under epoch 5");
    assert!(error.contains("identity epoch 4 is incompatible with epoch 5"));
    assert!(error.contains("drain and recreate"));
}

pub(super) fn wait_index_measurement_key(ordinal: usize) -> AwaitEventKey {
    restate_await_event_key(
        &durable_turn_scope("restate-postgres-workers-e2e", "measurement"),
        AwaitEventWaitIdentity::Custom {
            key: format!("wait-{ordinal:064x}"),
        },
    )
    .expect("derive wait-index measurement key")
}

pub(super) fn aggregate_wait_index_serialized_bytes(k: usize) -> usize {
    let waits = (0..k).map(wait_index_measurement_key).collect::<Vec<_>>();
    let mut bytes = 0;
    for registered in 1..=k {
        bytes += serde_json::to_vec(&(false, &waits[..registered]))
            .expect("serialize aggregate wait-index register state")
            .len();
    }
    for remaining in (0..k).rev() {
        bytes += serde_json::to_vec(&(false, &waits[..remaining]))
            .expect("serialize aggregate wait-index settle state")
            .len();
    }
    bytes
}

pub(super) fn keyed_wait_index_serialized_bytes(k: usize) -> usize {
    let metadata_bytes = serde_json::to_vec(&RestateDurableWaitIndexMetadata::default())
        .expect("serialize keyed wait-index metadata")
        .len();
    metadata_bytes
        + (0..k)
            .map(wait_index_measurement_key)
            .map(|key| {
                serde_json::to_vec(&key)
                    .expect("serialize keyed wait-index key preimage")
                    .len()
            })
            .sum::<usize>()
}

#[test]
pub(super) fn durable_wait_index_k_effect_measurements_are_linear() {
    for k in [4, 16] {
        // Each concurrent effect produces one register and one settle index
        // invocation in the workers-harness turn shape.
        let index_object_calls = 2 * k;
        let before_bytes = aggregate_wait_index_serialized_bytes(k);
        let after_bytes = keyed_wait_index_serialized_bytes(k);
        println!(
            "wait-index measurement K={k}: index_object_calls={index_object_calls}, before_serialized_state_bytes={before_bytes}, after_serialized_state_bytes={after_bytes}"
        );
        assert_eq!(index_object_calls, 2 * k);
        assert!(after_bytes < before_bytes);
    }
    assert_eq!(
        keyed_wait_index_serialized_bytes(16)
            - serde_json::to_vec(&RestateDurableWaitIndexMetadata::default())
                .expect("serialize metadata")
                .len(),
        4 * (keyed_wait_index_serialized_bytes(4)
            - serde_json::to_vec(&RestateDurableWaitIndexMetadata::default())
                .expect("serialize metadata")
                .len())
    );
}

#[test]
pub(super) fn restate_effect_name_uses_lash_replay_key() {
    let identity = lash_core::derive_tool_intent_identity(
        &lash_sansio::SessionId::from("session"),
        "turn",
        Some("call"),
        0,
    )
    .expect("derive tool-intent identity");
    let invocation = lash_core::RuntimeEffectInvocation::new(
        lash_core::EffectAddress::new(
            durable_turn_scope("session", "turn"),
            identity.replay_key.clone(),
        )
        .expect("valid Restate effect-name address"),
        lash_core::RuntimeAttribution::for_turn("session", "turn", 1, 2),
        "effect",
    )
    .with_replay_attribution(lash_core::RuntimeReplayAttribution::ToolIntent(identity));

    assert_eq!(
        restate_effect_name(&invocation),
        format!("lash:{}", invocation.replay_key())
    );
}

lash_conformance::effect_host_tests!({
    ((), || {
        Arc::new(RestateEffectHost::new("http://127.0.0.1:8080")) as Arc<dyn EffectHost>
    })
});
