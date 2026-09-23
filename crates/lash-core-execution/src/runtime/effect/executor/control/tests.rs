use super::*;

struct CompletionKeyProbe {
    issue_calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl AwaitEventResolver for CompletionKeyProbe {
    async fn await_event_key(
        &self,
        _scope: &ExecutionScope,
        _wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.issue_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(RuntimeError::new(
            RuntimeErrorCode::ToolCompletionKeyProcessLifetime,
            "probe must not issue a key",
        ))
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for CompletionKeyProbe {
    async fn execute_effect(
        &self,
        _envelope: RuntimeEffectEnvelope,
        _local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        unreachable!("completion-key preparation test does not execute effects")
    }

    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("CompletionKeyProbe"))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: crate::CancellationToken,
    ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("CompletionKeyProbe"))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("CompletionKeyProbe"))
    }
}

#[tokio::test]
async fn completion_key_preparation_issues_nothing_when_deferral_is_impossible() {
    let probe = CompletionKeyProbe {
        issue_calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let preparation = probe
        .prepare_completion_key(
            &ExecutionScope::turn("completion-key-session", "completion-key-turn"),
            AwaitEventWaitIdentity::tool_completion("completion-key-call"),
            false,
        )
        .await
        .expect("completion-key preparation");

    assert!(
        matches!(preparation, CompletionKeyPreparation::NotNeeded),
        "non-deferring work must select NotNeeded"
    );
    assert_eq!(
        probe.issue_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "NotNeeded must not issue an await-event key"
    );
}

struct TestResolver;

impl AwaitEventResolver for TestResolver {}

#[async_trait::async_trait]
impl RuntimeEffectController for TestResolver {
    async fn execute_effect(
        &self,
        _envelope: RuntimeEffectEnvelope,
        _local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        unreachable!("queued-lane controller tests do not execute effects")
    }

    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("TestResolver"))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: crate::CancellationToken,
    ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("TestResolver"))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("TestResolver"))
    }
}

#[derive(Default)]
struct EffectAdmissionProbe {
    controller_calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl AwaitEventResolver for EffectAdmissionProbe {}

#[async_trait::async_trait]
impl RuntimeEffectController for EffectAdmissionProbe {
    async fn execute_effect(
        &self,
        _envelope: RuntimeEffectEnvelope,
        _local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.controller_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(RuntimeEffectOutcome::Sleep)
    }

    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("EffectAdmissionProbe"))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: crate::CancellationToken,
    ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("EffectAdmissionProbe"))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("EffectAdmissionProbe"))
    }
}

fn sleep_envelope(scope: ExecutionScope, replay_key: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope, replay_key).expect("effect address"),
            crate::RuntimeAttribution::none(),
            "scope-admission-sleep",
        ),
        crate::RuntimeEffectCommand::Sleep {
            spec: crate::SleepSpec::For { duration_ms: 1 },
        },
    )
}

#[tokio::test]
async fn scoped_controller_refuses_wrong_scope_before_controller_or_local_execution() {
    let probe = EffectAdmissionProbe::default();
    let local_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let local_calls_for_executor = Arc::clone(&local_calls);
    let scoped = ScopedEffectController::borrowed(
        &probe,
        AdmittedScope::runtime_operation("admitted-scope"),
    )
    .expect("scoped admission probe");

    let error = scoped
        .execute_effect(
            sleep_envelope(
                ExecutionScope::runtime_operation("wrong-scope"),
                "shared-replay-key",
            ),
            RuntimeEffectLocalExecutor::testing(move |_envelope| async move {
                local_calls_for_executor.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(RuntimeEffectOutcome::Sleep)
            }),
        )
        .await
        .expect_err("wrong scope must be refused");

    assert_eq!(error.code, RuntimeErrorCode::RuntimeEffectScopeMismatch);
    assert_eq!(
        probe
            .controller_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(local_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn task_proxy_refuses_wrong_scope_before_handoff() {
    let probe = EffectAdmissionProbe::default();
    let (scoped, mut requests) = EffectTaskController::scoped(
        &probe,
        AdmittedScope::runtime_operation("admitted-proxy-scope"),
    )
    .expect("scoped task proxy");

    let error = scoped
        .controller()
        .execute_effect(
            sleep_envelope(
                ExecutionScope::runtime_operation("wrong-proxy-scope"),
                "shared-replay-key",
            ),
            RuntimeEffectLocalExecutor::testing(|_envelope| async {
                panic!("wrong-scope proxy must not run the local executor")
            }),
        )
        .await
        .expect_err("wrong proxy scope must be refused");

    assert_eq!(error.code, RuntimeErrorCode::RuntimeEffectScopeMismatch);
    assert_eq!(
        probe
            .controller_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(matches!(
        requests.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

fn test_group(scope: ExecutionScope, key: &str) -> RuntimeEffectGroup {
    RuntimeEffectGroup::try_new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope.clone(), format!("{key}:group"))
                .expect("valid group address"),
            crate::RuntimeAttribution::none(),
            "group",
        ),
        key,
        vec![sleep_envelope(scope, &format!("{key}:child:0"))],
        crate::GroupWakePolicy::All,
        crate::LoserPolicy::RunToCompletion,
    )
    .expect("a one-child group assembles")
}

#[tokio::test]
async fn task_proxy_group_open_refuses_wrong_scope_before_handoff() {
    let probe = EffectAdmissionProbe::default();
    let (scoped, mut requests) = EffectTaskController::scoped(
        &probe,
        AdmittedScope::runtime_operation("admitted-group-scope"),
    )
    .expect("scoped task proxy");

    let error = scoped
        .controller()
        .open_effect_group(test_group(
            ExecutionScope::runtime_operation("foreign-group-scope"),
            "group-foreign",
        ))
        .await
        .expect_err("a group under a foreign scope must be refused");

    assert_eq!(error.code, RuntimeErrorCode::RuntimeEffectScopeMismatch);
    assert!(matches!(
        requests.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn task_proxy_group_await_writes_back_the_advanced_cursor() {
    let probe = EffectAdmissionProbe::default();
    let (scoped, mut requests) = EffectTaskController::scoped(
        &probe,
        AdmittedScope::runtime_operation("admitted-group-scope"),
    )
    .expect("scoped task proxy");
    let mut handle =
        EffectGroupHandle::restored("group-cursor:0", 2, 0).expect("a valid restored cursor");

    let await_call = scoped
        .controller()
        .await_next_settlement(&mut handle, CancellationToken::new());
    let service = async {
        let EffectControllerTaskRequest::AwaitNextSettlement {
            mut handle,
            response,
            ..
        } = requests.recv().await.expect("a settlement request")
        else {
            panic!("expected a group settlement request");
        };
        // The task-side cursor is a copy of the caller's; the controller
        // advances it on the settlement it returns, and the caller's handle —
        // still the sole cursor of record — is written back to that.
        assert_eq!(handle.group_key(), "group-cursor:0");
        assert_eq!(handle.consumed(), 0);
        handle
            .advance()
            .expect("the delivered settlement advances the cursor");
        let _ = response.send((
            handle,
            Ok(GroupSettlement {
                position: 0,
                sequence: 1,
                outcome: Ok(RuntimeEffectOutcome::Sleep),
            }),
        ));
    };
    let (settlement, ()) = tokio::join!(await_call, service);

    let settlement = settlement.expect("the settlement lands through the proxy");
    assert_eq!((settlement.position, settlement.sequence), (0, 1));
    assert_eq!(
        handle.consumed(),
        1,
        "the caller's handle becomes the cursor the task side returned"
    );
}

#[tokio::test]
async fn task_proxy_group_await_carries_a_live_cancellation() {
    let probe = EffectAdmissionProbe::default();
    let (scoped, mut requests) = EffectTaskController::scoped(
        &probe,
        AdmittedScope::runtime_operation("admitted-group-scope"),
    )
    .expect("scoped task proxy");
    let mut handle =
        EffectGroupHandle::restored("group-cancel:0", 2, 0).expect("a valid restored cursor");
    let cancel = CancellationToken::new();

    let await_call = scoped
        .controller()
        .await_next_settlement(&mut handle, cancel.clone());
    let service = async {
        let EffectControllerTaskRequest::AwaitNextSettlement {
            handle,
            cancel,
            response,
        } = requests.recv().await.expect("a settlement request")
        else {
            panic!("expected a group settlement request");
        };
        // The token in the request is the caller's own: cancelling the await
        // is what wakes the task side, so it stays live for the whole await.
        cancel.cancelled().await;
        let _ = response.send((
            handle,
            Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
                "the await was cancelled",
            )),
        ));
    };
    let cancel_after_arrival = async {
        tokio::task::yield_now().await;
        cancel.cancel();
    };
    let (result, (), ()) = tokio::join!(await_call, service, cancel_after_arrival);

    let error = result.expect_err("a cancelled await returns its typed error");
    assert_eq!(
        error.code,
        RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled
    );
    assert_eq!(
        handle.consumed(),
        0,
        "a cancelled await leaves the caller's cursor untouched"
    );
}

#[tokio::test]
async fn task_proxy_group_calls_fail_closed_when_the_task_is_gone() {
    let probe = EffectAdmissionProbe::default();
    let (scoped, requests) = EffectTaskController::scoped(
        &probe,
        AdmittedScope::runtime_operation("admitted-group-scope"),
    )
    .expect("scoped task proxy");
    drop(requests);

    let scope = ExecutionScope::runtime_operation("admitted-group-scope");
    let open_error = scoped
        .controller()
        .open_effect_group(test_group(scope, "group-closed-task"))
        .await
        .expect_err("a group open on a closed task must return a typed error");
    assert_eq!(
        open_error.code,
        RuntimeErrorCode::RuntimeEffectControllerTaskClosed
    );

    let mut handle =
        EffectGroupHandle::restored("group-closed-task:0", 1, 0).expect("a valid restored cursor");
    let await_error = scoped
        .controller()
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect_err("a settlement await on a closed task must return a typed error");
    assert_eq!(
        await_error.code,
        RuntimeErrorCode::RuntimeEffectControllerTaskClosed
    );
    assert_eq!(
        handle.consumed(),
        0,
        "a send failure leaves the caller's cursor untouched"
    );

    let close_error = scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect_err("a group close on a closed task must return a typed error");
    assert_eq!(
        close_error.code,
        RuntimeErrorCode::RuntimeEffectControllerTaskClosed
    );
}

#[tokio::test]
async fn task_proxy_group_open_reports_a_dropped_response() {
    let probe = EffectAdmissionProbe::default();
    let (scoped, mut requests) = EffectTaskController::scoped(
        &probe,
        AdmittedScope::runtime_operation("admitted-group-scope"),
    )
    .expect("scoped task proxy");

    let open_call = scoped.controller().open_effect_group(test_group(
        ExecutionScope::runtime_operation("admitted-group-scope"),
        "group-dropped",
    ));
    let accept_then_drop = async {
        match requests.recv().await.expect("a group-open request") {
            EffectControllerTaskRequest::OpenEffectGroup { response, .. } => drop(response),
            _ => panic!("expected a group-open request"),
        }
    };
    let (result, ()) = tokio::join!(open_call, accept_then_drop);

    let error = result.expect_err("a dropped response must return a typed error");
    assert_eq!(
        error.code,
        RuntimeErrorCode::RuntimeEffectControllerTaskClosed
    );
}

struct FakeQueuedLaneProbe {
    attempts: std::sync::Mutex<std::collections::VecDeque<QueuedLaneAttempt>>,
    try_calls: std::sync::atomic::AtomicUsize,
    pause_calls: std::sync::atomic::AtomicUsize,
}

impl FakeQueuedLaneProbe {
    fn new(attempts: impl IntoIterator<Item = QueuedLaneAttempt>) -> Self {
        Self {
            attempts: std::sync::Mutex::new(attempts.into_iter().collect()),
            try_calls: std::sync::atomic::AtomicUsize::new(0),
            pause_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn try_calls(&self) -> usize {
        self.try_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn pause_calls(&self) -> usize {
        self.pause_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl QueuedLaneProbe for FakeQueuedLaneProbe {
    async fn try_acquire(&self) -> Result<QueuedLaneAttempt, RuntimeError> {
        self.try_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self
            .attempts
            .lock()
            .expect("fake queued-lane attempts")
            .pop_front()
            .expect("fake queued-lane attempt available"))
    }

    async fn pause(&self, _slice: std::time::Duration) {
        self.pause_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn queued_lane_holder(expires_at_epoch_ms: u64) -> QueuedLaneHolder {
    QueuedLaneHolder::new(crate::store::SessionExecutionLease {
        session_id: SessionId::from("queued-lane-test"),
        owner: crate::LeaseOwnerIdentity::opaque("holder", "holder:incarnation"),
        executor_id: "holder-executor".to_string(),
        lease_token: "holder-token".to_string(),
        fencing_token: 7,
        claimed_at_epoch_ms: 1_000,
        lease_term_ms: 6_400,
        expires_at_epoch_ms,
    })
}

#[tokio::test]
async fn default_queued_lane_acquisition_stops_after_one_busy_attempt() {
    let probe = Arc::new(FakeQueuedLaneProbe::new([QueuedLaneAttempt::Busy(
        queued_lane_holder(7_400),
    )]));

    let result = TestResolver
        .acquire_queued_lane(
            Arc::clone(&probe) as Arc<dyn QueuedLaneProbe>,
            CancellationToken::new(),
        )
        .await
        .expect("default queued-lane acquisition");

    assert!(matches!(result, QueuedLaneAcquisition::NotAcquired));
    assert_eq!(probe.try_calls(), 1);
    assert_eq!(probe.pause_calls(), 0);
}

#[tokio::test]
async fn provided_wait_reports_a_renewing_holder_as_typed_retryable_busy() {
    #[cfg(feature = "otel-trace")]
    let metrics = crate::operational_metrics::TestMetrics::install();
    let probe = Arc::new(FakeQueuedLaneProbe::new([
        QueuedLaneAttempt::Busy(queued_lane_holder(7_400)),
        QueuedLaneAttempt::Busy(queued_lane_holder(7_401)),
    ]));

    let result = TestResolver
        .wait_out_crashed_lane_holder(
            Arc::clone(&probe) as Arc<dyn QueuedLaneProbe>,
            CancellationToken::new(),
        )
        .await;

    let Err(error) = result else {
        panic!("a renewing holder must hand pacing back to the engine")
    };
    assert_eq!(error.code, RuntimeErrorCode::SessionExecutionLaneBusy);
    assert!(error.is_retryable());
    assert_eq!(probe.try_calls(), 2);
    assert_eq!(probe.pause_calls(), 1);
    #[cfg(feature = "otel-trace")]
    {
        assert_eq!(
            metrics.histogram_count("lash.session_execution_lane.contention_wait.duration"),
            1
        );
        assert_eq!(
            metrics.counter_value("lash.session_execution_lane.give_ups"),
            1
        );
    }
}

#[tokio::test]
async fn closed_task_controller_returns_a_typed_queued_lane_error() {
    let controller = TestResolver;
    let (scoped, requests) = EffectTaskController::scoped(
        &controller,
        AdmittedScope::queue_drain("queued-lane-test", "closed"),
    )
    .expect("queued-lane task proxy");
    drop(requests);

    let result = scoped
        .controller()
        .acquire_queued_lane(
            Arc::new(FakeQueuedLaneProbe::new([QueuedLaneAttempt::Busy(
                queued_lane_holder(7_400),
            )])),
            CancellationToken::new(),
        )
        .await;

    let Err(error) = result else {
        panic!("a closed queued-lane task must return a typed error")
    };
    assert_eq!(
        error.code,
        RuntimeErrorCode::RuntimeEffectControllerTaskClosed
    );
}

#[tokio::test]
async fn dropped_queued_lane_response_returns_a_typed_error() {
    let controller = TestResolver;
    let (scoped, mut requests) = EffectTaskController::scoped(
        &controller,
        AdmittedScope::queue_drain("queued-lane-test", "dropped-response"),
    )
    .expect("queued-lane task proxy");

    let acquire = scoped.controller().acquire_queued_lane(
        Arc::new(FakeQueuedLaneProbe::new([QueuedLaneAttempt::Busy(
            queued_lane_holder(7_400),
        )])),
        CancellationToken::new(),
    );
    let accept_then_drop = async {
        match requests.recv().await.expect("queued-lane task request") {
            EffectControllerTaskRequest::AcquireQueuedLane { response, .. } => drop(response),
            _ => panic!("expected a queued-lane request"),
        }
    };
    let (result, ()) = tokio::join!(acquire, accept_then_drop);

    let Err(error) = result else {
        panic!("a dropped queued-lane response must return a typed error")
    };
    assert_eq!(
        error.code,
        RuntimeErrorCode::RuntimeEffectControllerTaskClosed
    );
}

#[test]
fn queued_lane_holder_debug_redacts_the_lease_token() {
    let holder = queued_lane_holder(9_000);
    let rendered = format!("{holder:?}");
    assert!(
        !rendered.contains("holder-token"),
        "Debug output must not leak the lease token: {rendered}"
    );
    assert!(rendered.contains("holder-executor"));
}

#[test]
fn journal_identity_is_typed_and_session_qualified() {
    let scopes = [
        ExecutionScope::turn("session", "shared"),
        ExecutionScope::queue_drain("session", "shared"),
        ExecutionScope::session_delete("session"),
        ExecutionScope::process("shared"),
        ExecutionScope::runtime_operation("shared"),
    ];
    let identities = scopes
        .iter()
        .map(|scope| scope.journal_identity().expect("durable identity"))
        .collect::<Vec<_>>();
    let keys = identities
        .iter()
        .map(EffectJournalIdentity::key)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(keys.len(), scopes.len());
    for identity in &identities[..3] {
        assert_eq!(identity.session_id(), Some(&SessionId::from("session")));
    }
    for identity in &identities[3..] {
        assert_eq!(identity.session_id(), None);
    }
}

/// Every variant survives the round trip, not just the one the drain
/// happens to exercise.
///
/// `from_journal_key` is the drain's only way back from a journal row to a
/// scope, and it re-derives each variant from a `kind` string by hand. A
/// variant whose forward and backward spellings drift apart makes every
/// row written under it undrainable — refused as a scope no version of this
/// runtime writes, which is exactly the wrong answer for a scope this
/// version writes constantly. Only the whole set proves the mapping; one
/// variant proves the plumbing.
#[test]
fn every_scope_variant_round_trips_through_its_journal_key() {
    for scope in [
        ExecutionScope::turn("session", "shared"),
        ExecutionScope::queue_drain("session", "shared"),
        ExecutionScope::session_delete("session"),
        ExecutionScope::process("shared"),
        ExecutionScope::runtime_operation("shared"),
    ] {
        let key = scope
            .journal_identity()
            .expect("durable identity")
            .key()
            .to_string();
        assert_eq!(
            ExecutionScope::from_journal_key(&key),
            Some(scope.clone()),
            "scope {scope:?} did not come back from its own journal key `{key}`"
        );
    }
}

/// A key this build cannot read is refused rather than guessed at, which is
/// what lets the drain treat `None` as corruption instead of as a default.
#[test]
fn a_journal_key_this_build_cannot_read_is_refused() {
    for key in [
        "",
        "not json",
        r#"{"version":1,"kind":"turn","session_id":"s","execution_id":"t"}"#,
        r#"{"version":2,"kind":"nonsense","execution_id":"t"}"#,
        // Right kind, missing the field that kind requires.
        r#"{"version":2,"kind":"turn","session_id":"s"}"#,
        // Decodes, but to a scope the forward direction would refuse.
        r#"{"version":2,"kind":"process","execution_id":""}"#,
    ] {
        assert_eq!(
            ExecutionScope::from_journal_key(key),
            None,
            "`{key}` is not a scope this build wrote"
        );
    }
}
