use super::*;

struct SettlementExecutor {
    calls: std::sync::atomic::AtomicUsize,
    first_started: AtomicBool,
    started: tokio::sync::Notify,
    unsettled: AtomicBool,
    dispositions: Mutex<Vec<crate::plugin::CodeExecutionDisposition>>,
    nested_error: bool,
}

impl SettlementExecutor {
    fn new(nested_error: bool) -> Self {
        Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
            first_started: AtomicBool::new(false),
            started: tokio::sync::Notify::new(),
            unsettled: AtomicBool::new(false),
            dispositions: Mutex::new(Vec::new()),
            nested_error,
        }
    }

    async fn wait_for_first_execution(&self) {
        loop {
            let notified = self.started.notified();
            if self.first_started.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    fn dispositions(&self) -> Vec<crate::plugin::CodeExecutionDisposition> {
        self.dispositions.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl crate::plugin::CodeExecutorPlugin for SettlementExecutor {
    async fn execute_code(
        &self,
        ctx: crate::RuntimeExecutionContext<'_>,
        _request: crate::ExecRequest,
    ) -> Result<crate::ExecResponse, crate::SessionError> {
        if self.unsettled.swap(true, Ordering::SeqCst) {
            return Err(crate::SessionError::Protocol(
                "the previous code execution response has not been settled".to_string(),
            ));
        }
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first_started.store(true, Ordering::SeqCst);
            self.started.notify_waiters();
            while !ctx.is_cancelled() {
                tokio::task::yield_now().await;
            }
            if self.nested_error {
                ctx.record_nested_effect_error(crate::RuntimeEffectControllerError::foreign(
                    "injected_exec_handoff_failure",
                    "injected code-effect response handoff failure",
                ));
                return Err(crate::SessionError::Protocol(
                    "code execution stopped at the response handoff".to_string(),
                ));
            }
            return Ok(crate::ExecResponse {
                observations: Vec::new(),
                calls: Vec::new(),
                printed_images: Vec::new(),
                error: Some("code execution stopped".to_string()),
                duration_ms: 1,
                degraded_bindings: Vec::new(),
                terminal_finish: None,
            });
        }
        Ok(crate::ExecResponse {
            observations: vec![crate::Observation {
                text: "next cell executed".to_string(),
                projection: Default::default(),
            }],
            calls: Vec::new(),
            printed_images: Vec::new(),
            error: None,
            duration_ms: 1,
            degraded_bindings: Vec::new(),
            terminal_finish: None,
        })
    }

    async fn settle_code_execution(
        &self,
        disposition: crate::plugin::CodeExecutionDisposition,
    ) -> Result<(), crate::SessionError> {
        self.dispositions.lock_recover().push(disposition);
        self.unsettled.store(false, Ordering::SeqCst);
        Ok(())
    }
}

fn protocol_factory(executor: Arc<SettlementExecutor>) -> Arc<dyn crate::PluginFactory> {
    Arc::new(EffectControllerTestProtocolFactory {
        code_executor: Some(executor),
    })
}

fn turn_scope<'a>(
    controller: &'a dyn crate::RuntimeEffectController,
    session_id: &str,
    turn_id: &str,
) -> crate::ScopedEffectController<'a> {
    crate::ScopedEffectController::borrowed(
        controller,
        crate::ExecutionScope::turn(session_id, turn_id),
    )
    .expect("turn scope")
}

#[derive(Debug)]
struct ManualClock {
    epoch_ms: std::sync::atomic::AtomicU64,
}

impl ManualClock {
    fn new(epoch_ms: u64) -> Self {
        Self {
            epoch_ms: std::sync::atomic::AtomicU64::new(epoch_ms),
        }
    }

    fn advance_ms(&self, delta_ms: u64) {
        self.epoch_ms.fetch_add(delta_ms, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl crate::Clock for ManualClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_ms(&self) -> u64 {
        self.epoch_ms.load(Ordering::SeqCst)
    }

    fn timestamp_rfc3339(&self) -> String {
        self.timestamp_datetime().to_rfc3339()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::<chrono::Utc>::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(self.timestamp_ms()),
        )
    }

    async fn sleep(&self, duration: std::time::Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

#[tokio::test]
async fn bare_cancelled_token_after_mid_cell_lease_loss_is_not_a_cancelled_terminal() {
    let lease_ttl = std::time::Duration::from_millis(120);
    let clock = Arc::new(ManualClock::new(1_000));
    let store = Arc::new(RecordingStore::with_clock(clock.clone()));
    let executor = Arc::new(SettlementExecutor::new(false));
    let controller = RecordingEffectController::default().with_local_code_execution();
    let config = runtime_host_config_with_native_controller(Arc::new(controller.clone()))
        .with_clock(clock.clone())
        .with_lease_timings(crate::LeaseTimings::from_ttl(lease_ttl).expect("valid lease timings"));
    let mut runtime = TestRuntime::new(mock_provider(Vec::new()))
        .plugins(vec![protocol_factory(Arc::clone(&executor))])
        .host(EmbeddedRuntimeHost::new(config))
        .store(store.clone())
        .without_process_registry()
        .build()
        .await;
    let cancel = CancellationToken::new();
    let cancel_for_turn = cancel.clone();
    let hint = crate::TurnCancelOriginHint::default();
    hint.configure_local_token(None);
    let mut input = TurnInput::text("run until the session lease is lost");
    input.turn_context.set_local_cancel_origin_hint(hint);
    let controller_for_turn = controller.clone();
    let turn = crate::task::spawn(async move {
        runtime
            .run_turn_assembled(
                input,
                cancel_for_turn,
                turn_scope(&controller_for_turn, "root", "lease-loss-mid-cell"),
            )
            .await
    });
    executor.wait_for_first_execution().await;
    let renewals_before_loss = store.session_execution_lease_renewal_count();

    clock.advance_ms(lease_ttl.as_millis() as u64 + 1);
    crate::store::SessionExecutionLeaseStore::try_claim_session_execution_lease(
        store.as_ref(),
        "root",
        &crate::LeaseOwnerIdentity::opaque("lease-loss-successor", "incarnation"),
        "lease-loss-successor-executor",
        60_000,
    )
    .await
    .expect("claim expired session execution lease")
    .acquired()
    .expect("successor takes over the expired lease");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store.session_execution_lease_renewal_count() == renewals_before_loss {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("renewal observes the lost predecessor fence");
    cancel.cancel();

    let error = turn
        .await
        .expect("turn task")
        .expect_err("lease loss must not commit a fabricated Cancelled terminal");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::SessionExecutionLeaseLost
    );
    assert_eq!(
        executor.dispositions(),
        vec![crate::plugin::CodeExecutionDisposition::Accepted]
    );
}

#[tokio::test]
async fn user_stop_mid_cell_settles_cancelled_with_recorded_evidence() {
    let executor = Arc::new(SettlementExecutor::new(false));
    let controller = RecordingEffectController::default().with_local_code_execution();
    let host = host_with_effect_recorder(controller.clone());
    let turn_driver = crate::TurnWorkDriver::new(Arc::clone(&host.core.control.effect_host));
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![protocol_factory(Arc::clone(&executor))],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        host,
    )
    .await;
    let turn_id = "user-stop-mid-cell";
    let turn = crate::task::spawn(async move {
        runtime
            .run_turn_assembled(
                TurnInput::text("run the first cell"),
                CancellationToken::new(),
                turn_scope(&controller, "root", turn_id),
            )
            .await
    });
    executor.wait_for_first_execution().await;

    let receipt = turn_driver
        .request_cancel(crate::TurnCancelRequest::new(
            crate::TurnAddress::new("root", turn_id),
            "user-stop-request",
            Some("test-user".to_string()),
        ))
        .await
        .expect("record user cancellation");
    assert!(matches!(
        receipt.outcome,
        crate::TurnCancelOutcome::Requested(_)
    ));

    let assembled = turn.await.expect("turn task").expect("cancelled turn");
    assert!(matches!(
        assembled.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { ref evidence })
            if evidence.request_id == "user-stop-request"
                && evidence.origin.as_deref() == Some("test-user")
    ));
    assert_eq!(
        executor.dispositions(),
        vec![crate::plugin::CodeExecutionDisposition::Cancelled]
    );
}

#[tokio::test]
async fn response_handoff_abort_settles_before_the_next_cell() {
    let executor = Arc::new(SettlementExecutor::new(false));
    let controller = RecordingEffectController::default()
        .with_local_code_execution()
        .with_failing_exec_handoff_once();
    let host = host_with_effect_recorder(controller.clone());
    let turn_driver = crate::TurnWorkDriver::new(Arc::clone(&host.core.control.effect_host));
    let mut runtime = runtime_with_plugins_and_tools_and_host(
        vec![protocol_factory(Arc::clone(&executor))],
        Arc::new(EmptyTools),
        mock_provider(Vec::new()),
        host,
    )
    .await;
    let turn_id = "response-handoff-abort";
    let controller_for_first = controller.clone();
    let first = crate::task::spawn(async move {
        let result = runtime
            .run_turn_assembled(
                TurnInput::text("abort the first cell handoff"),
                CancellationToken::new(),
                turn_scope(&controller_for_first, "root", turn_id),
            )
            .await;
        (runtime, result)
    });
    executor.wait_for_first_execution().await;
    controller.fail_failure_disposition();
    turn_driver
        .request_cancel(crate::TurnCancelRequest::new(
            crate::TurnAddress::new("root", turn_id),
            "handoff-abort-stop",
            Some("test-user".to_string()),
        ))
        .await
        .expect("record cancellation before the failed disposition lookup");
    let (mut runtime, first_result) = first.await.expect("first turn task");
    let first = first_result.expect("the cancellation path assembles its terminal");
    assert!(matches!(
        first.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { ref evidence })
            if evidence.request_id == "handoff-abort-stop"
    ));

    let second = runtime
        .run_turn_assembled(
            TurnInput::text("run the next cell"),
            CancellationToken::new(),
            turn_scope(&controller, "root", "response-handoff-next-cell"),
        )
        .await
        .expect("the next cell executes after settlement");
    assert!(matches!(second.outcome, TurnOutcome::Finished(_)));
    assert_eq!(second.assistant_output.safe_text, "next cell executed");
    assert_eq!(
        executor.dispositions(),
        vec![
            crate::plugin::CodeExecutionDisposition::Cancelled,
            crate::plugin::CodeExecutionDisposition::Accepted,
        ]
    );
}
