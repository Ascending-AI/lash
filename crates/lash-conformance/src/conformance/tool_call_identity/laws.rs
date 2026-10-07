//! The tool-call identity laws. See the module documentation of
//! [`super`] for the world they run in.

use crate::SessionId;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::{
    AttemptIdentity, DEFERRED, Execution, PROBE, ProbeArgs, ToolCallIdentityTier, World,
    assert_finished, calls, outputs, text,
};

/// The one execution of `label`'s body.
pub(super) fn only(world: &World, label: &str) -> Execution {
    let executions = world.witness.of(label);
    assert_eq!(
        executions.len(),
        1,
        "`{label}` ran exactly once: {executions:?}"
    );
    executions
        .into_iter()
        .next()
        .unwrap_or_else(|| unreachable!())
}

/// Every execution of one logical call saw one call id.
pub(super) fn assert_one_identity(label: &str, executions: &[Execution]) -> lash_core::ToolCallId {
    assert!(!executions.is_empty(), "`{label}` ran");
    let first = executions[0].identity.call_id.clone();
    for execution in executions {
        assert_eq!(
            execution.identity.call_id, first,
            "every re-run of `{label}` sees the call id its first run saw: {executions:?}"
        );
    }
    first
}

/// The label a settled probe call answered with.
fn answered_label(output: &serde_json::Value) -> Option<&str> {
    output.get("label").and_then(serde_json::Value::as_str)
}

/// The idempotency key two logical calls saw must differ.
pub(super) fn assert_distinct_keys(what: &str, one: &AttemptIdentity, other: &AttemptIdentity) {
    assert_ne!(
        one.call_id, other.call_id,
        "{what}: two logical calls share their idempotency key"
    );
}

/// FIG-4073's first law. Two turns of one session whose model provider emits
/// the same call id, `call_0`, are two logical calls: every key the tool can
/// key idempotency on differs between them, or an idempotent tool answers the
/// second call with the first call's result.
pub async fn repeated_provider_id_across_turns_is_distinct(tier: ToolCallIdentityTier) {
    let world = World::new(&tier, "repeated-provider-id");
    for name in ["first", "second"] {
        let turn = world.turn(
            name,
            vec![
                calls(&[("call_0", PROBE, ProbeArgs::label(name))]),
                text(&format!("{name} turn done")),
            ],
        );
        let assembled = world.run(&turn).await;
        assert_finished(name, &assembled);
        assert_eq!(
            outputs(&assembled)
                .iter()
                .map(|(_, _, output)| answered_label(output).map(str::to_owned))
                .collect::<Vec<_>>(),
            vec![Some(name.to_string())],
            "the {name} turn's call answered for itself"
        );
    }
    let first = only(&world, "first");
    let second = only(&world, "second");
    assert_distinct_keys(
        "two turns whose provider emitted `call_0` each",
        &first.identity,
        &second.identity,
    );
}

/// Two deferred calls in one execution scope — one turn, two model steps —
/// whose provider gave both the id `call_0` never consume each other's
/// completion. Each call parks on its own completion key and resolves it
/// with its own label; the second call's recorded outcome must be its own
/// label, and its key must not be the first call's.
pub async fn same_scope_completion_collision(tier: ToolCallIdentityTier) {
    let world = World::new(&tier, "same-scope-completion");
    let turn = world.turn(
        "turn",
        vec![
            calls(&[("call_0", DEFERRED, ProbeArgs::label("first"))]),
            calls(&[("call_0", DEFERRED, ProbeArgs::label("second"))]),
            text("both deferred calls settled"),
        ],
    );
    let assembled = world.run(&turn).await;
    assert_finished("the two-step deferred turn", &assembled);
    let answered = outputs(&assembled)
        .iter()
        .map(|(_, _, output)| answered_label(output).map(str::to_owned))
        .collect::<Vec<_>>();
    let started_second = world.witness.started("second");
    let first = world.witness.of("first");
    let second = world.witness.of("second");
    let keys = |executions: &[Execution]| {
        executions
            .iter()
            .map(|execution| execution.completion_key.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        answered,
        vec![Some("first".to_string()), Some("second".to_string())],
        "each deferred call settles with its own resolution (the second call's body ran {} \
         times; completion keys: first {:?}, second {:?})",
        started_second,
        keys(&first),
        keys(&second),
    );
    let first = only(&world, "first");
    let second = only(&world, "second");
    assert_ne!(
        first.completion_key, second.completion_key,
        "two calls in one scope park on distinct completion keys"
    );
    assert_distinct_keys(
        "two calls in one turn whose provider emitted `call_0` each",
        &first.identity,
        &second.identity,
    );
}

/// The last execution's report: a replaying tier reports once per execution
/// that reaches the end.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the tier's runner ran the attempt it was handed"
)]
async fn last_result(
    mut reported: tokio::sync::mpsc::UnboundedReceiver<
        Result<crate::AssembledTurn, crate::RuntimeError>,
    >,
) -> Result<crate::AssembledTurn, crate::RuntimeError> {
    let mut last = reported.recv().await.expect("the recovered turn reports");
    while let Ok(next) = reported.try_recv() {
        last = next;
    }
    last
}

/// Runs `turn`, whose probe call `held` holds on the law's gate, kills the
/// turn's execution once that call started, and recovers it.
pub(super) async fn crash_while_held(
    world: &World,
    turn: &super::ScriptedTurn,
    held: &'static str,
) -> crate::AssembledTurn {
    crash_while_held_result(world, turn, held)
        .await
        .unwrap_or_else(|error| panic!("the recovered turn runs: {error}"))
}

/// [`crash_while_held`], answering how the recovered turn ended.
pub(super) async fn crash_while_held_result(
    world: &World,
    turn: &super::ScriptedTurn,
    held: &'static str,
) -> Result<crate::AssembledTurn, crate::RuntimeError> {
    crash_when(world, turn, "the held probe starts", move |witness| {
        witness.started(held) >= 1
    })
    .await
}

/// Runs `turn`, kills its execution once `ready` holds, and recovers it.
///
/// The gate a held probe waits on opens as the crash fires. A tier that
/// keeps the crashing execution running dies at its next poll, and the held
/// call then finishes in whichever execution outlives the crash — or runs
/// again. A tier that suspends a turn at every await it cannot answer from
/// its journal has no execution left to kill: the held call finishes, the
/// turn resumes, and the resumed execution finds the crash fired and dies
/// there, so the recovery still follows a crash rather than waiting on a
/// gate that only the recovery would open.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn crash_when(
    world: &World,
    turn: &super::ScriptedTurn,
    what: &'static str,
    ready: impl Fn(&super::Witness) -> bool + Send + Sync + 'static,
) -> Result<crate::AssembledTurn, crate::RuntimeError> {
    let turn = turn.clone();
    let crash = crate::ConformanceCrash::new();
    let (report, reported) = tokio::sync::mpsc::unbounded_channel();
    let crashing: crate::ConformanceTurnAttempt = {
        let world = world.clone();
        let turn = turn.clone();
        let crash = crash.clone();
        Arc::new(move |scope| {
            let world = world.clone();
            let turn = turn.clone();
            let crash = crash.clone();
            Box::pin(async move {
                tokio::select! {
                    biased;
                    () = crash.fired() => panic!("the law kills the turn's execution here"),
                    ended = world.shift(&turn, scope, None) => panic!(
                        "the crashing turn ended ({ended:?}) before its crash fired"
                    ),
                }
            })
        })
    };
    let fire = {
        let world = world.clone();
        let crash = crash.clone();
        crate::task::spawn(async move {
            world.witness.until(what, ready).await;
            crash.fire();
            world.witness.open_gate();
        })
    };
    let run = world.runner().run_crashed_then_redriven_turn(
        world.admitted(&turn),
        crashing,
        world.attempt(&turn, report),
    );
    tokio::pin!(run);
    let mut fire = fire;
    // A trigger that never fires fails the law with its own message at
    // once, rather than leaving the crashing turn to run out the law's bound.
    tokio::select! {
        () = &mut run => fire.await.expect("the crash trigger's task"),
        fired = &mut fire => {
            if let Err(failed) = fired {
                std::panic::resume_unwind(failed.into_panic());
            }
            run.await;
        }
    }
    last_result(reported).await
}

/// A reported failure after the effect — a timeout, say — is retried under
/// the same call id: the first attempt's effect may have happened, so a key
/// that changed would defeat the tool's deduplication. The attempt number
/// advances.
pub async fn reported_failure_retry_preserves_call_id(tier: ToolCallIdentityTier) {
    let world = World::new(&tier, "reported-failure-retry");
    let turn = world.turn(
        "turn",
        vec![
            calls(&[("call_retry", PROBE, ProbeArgs::failing_first("retried"))]),
            text("the retried call settled"),
        ],
    );
    let assembled = world.run(&turn).await;
    assert_finished("the retried call's turn", &assembled);
    let executions = world.witness.of("retried");
    assert_eq!(
        executions
            .iter()
            .map(|execution| execution.identity.attempt)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "the reported failure is retried once, and the attempt number advances"
    );
    assert_one_identity("retried", &executions);
    assert_eq!(
        outputs(&assembled)
            .iter()
            .map(|(_, _, output)| answered_label(output).map(str::to_owned))
            .collect::<Vec<_>>(),
        vec![Some("retried".to_string())],
        "the retry's success is the call's outcome"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixtures establish each result"
)]
pub async fn one_turn_commits_history_once_and_never_replaces_existing_nodes(
    tier: ToolCallIdentityTier,
) {
    let mut world = World::new(&tier, "single-history-commit");
    let receipts = Arc::new(super::super::commit_receipts::CommitReceipts::new(
        world.store().await,
    ));
    world.observed_store = Some(receipts.clone());
    let seed = world.turn("seed", vec![text("immutable prefix")]);
    assert_finished("seed", &world.run(&seed).await);
    let store = world.store().await;
    let before = store
        .load_session_window(&world.session_id, crate::store::WindowSelector::Current)
        .await
        .expect("read seed")
        .expect("seed window");
    let prefix = serde_json::to_value(&before.window.nodes).expect("serialize immutable prefix");
    let turn = world.turn(
        "progress",
        vec![
            calls(&[("call_0", PROBE, ProbeArgs::label("first-progress"))]),
            calls(&[("call_1", PROBE, ProbeArgs::label("second-progress"))]),
            text("all progress completes in one turn"),
        ],
    );
    assert_finished("progress", &world.run(&turn).await);
    only(&world, "first-progress");
    only(&world, "second-progress");
    let after = store
        .load_session_window(&world.session_id, crate::store::WindowSelector::Current)
        .await
        .expect("read completed turn")
        .expect("completed window");
    receipts.assert_since(before.head_revision, after.head_revision, 0, 1, 0, 1);
    let old_ids = before
        .window
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect::<std::collections::HashSet<_>>();
    let retained = after
        .window
        .nodes
        .iter()
        .filter(|node| old_ids.contains(&node.node_id))
        .collect::<Vec<_>>();
    assert_eq!(
        serde_json::to_value(retained).expect("serialize retained prefix"),
        prefix
    );
    assert!(after.window.nodes.len() > before.window.nodes.len());
}

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn suspended_tool_keeps_turn_and_history_head_until_resolution(
    tier: ToolCallIdentityTier,
) {
    let mut world = World::new(&tier, "suspended-history");
    let receipts = Arc::new(super::super::commit_receipts::CommitReceipts::new(
        world.store().await,
    ));
    world.observed_store = Some(receipts.clone());
    assert_finished(
        "seed",
        &world
            .run(&world.turn("seed", vec![text("before suspension")]))
            .await,
    );
    let store = world.store().await;
    let before = store
        .load_session_head_meta(&world.session_id)
        .await
        .expect("head before suspended tool")
        .expect("committed head");
    let turn = world.turn(
        "held-turn",
        vec![
            calls(&[("held", DEFERRED, ProbeArgs::held("suspended"))]),
            text("after resolution"),
        ],
    );
    let (assembled, ()) = tokio::join!(world.run(&turn), async {
        while world.witness.of("suspended").is_empty() {
            tokio::task::yield_now().await;
        }
        let execution = only(&world, "suspended");
        let key = execution.completion_key.expect("pending completion key");
        let crate::waits::HostKeyCheck::Verified(row) =
            crate::waits::check_host_key(tier.effect_host.backend(), key.as_str())
                .await
                .expect("the wait reads")
        else {
            panic!("the completion key names its wait");
        };
        assert!(
            crate::waits::settled(&row)
                .expect("unsettled wait")
                .is_none()
        );
        let waiting = store
            .load_session_head_meta(&world.session_id)
            .await
            .expect("head while waiting")
            .expect("retained head");
        receipts.assert_since(before.head_revision, waiting.head_revision, 0, 0, 0, 1);
        assert_eq!(
            waiting.leaf_node_id, before.leaf_node_id,
            "the plugin transition cannot append a history node while the tool waits"
        );
        assert_eq!(waiting.current_frame_node_id, before.current_frame_node_id);
        assert_eq!(waiting.config, before.config);
        assert_eq!(waiting.pending_follow_on, before.pending_follow_on);
        world.witness.gate.open();
    });
    assert_finished("resolved suspended turn", &assembled);
    let after = store
        .load_session_head_meta(&world.session_id)
        .await
        .expect("head after resolution")
        .expect("head");
    receipts.assert_since(before.head_revision, after.head_revision, 0, 1, 0, 1);
    assert_eq!(world.witness.of("suspended").len(), 1);
    assert_eq!(outputs(&assembled).len(), 1);
}

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn live_and_durable_queue_paths_share_results_and_capability_refusals(
    tier: ToolCallIdentityTier,
) {
    let world = World::new(&tier, "queue-rails");
    let runtime = crate::RuntimeHandle::new(world.runtime(None).await);
    let store = crate::store::SessionStore::new(world.store().await, world.session_id.clone())
        .expect("binding view");
    let ops = crate::facade_support::DurableSessionOps::new(
        world.session_id.clone(),
        Arc::new(crate::facade_support::InMemoryLiveReplayStore::default()),
    );
    let input = crate::TurnInput::text("same request on both rails");
    let first = runtime
        .enqueue_turn_input(
            input.clone(),
            crate::TurnInputIngress::NextTurn,
            Some("same-source".into()),
        )
        .await
        .expect("live enqueue");
    let replay = ops
        .enqueue_turn_input(
            &store,
            input,
            crate::TurnInputIngress::NextTurn,
            Some("same-source".into()),
            crate::RunSpec::default(),
        )
        .await
        .expect("durable replay");
    assert_eq!(
        serde_json::to_value(&first).expect("live result"),
        serde_json::to_value(&replay).expect("durable result")
    );
    assert_eq!(
        ops.pending_turn_inputs(&store)
            .await
            .expect("pending rail")
            .len(),
        1
    );
    assert!(
        runtime
            .cancel_queued_work_batch("unknown-batch")
            .await
            .expect("live absent batch")
            .is_none()
    );
    assert!(
        ops.cancel_queued_work_batch(&store, "unknown-batch")
            .await
            .expect("durable absent batch")
            .is_none()
    );
    assert!(
        ops.cancel_pending_turn_input(&store, first.input_id.as_str())
            .await
            .expect("durable cancellation")
            .is_cancelled()
    );
    assert!(
        ops.pending_turn_inputs(&store)
            .await
            .expect("queue empty")
            .is_empty()
    );
    let missing = crate::PendingTurnInputCancelTarget::input_id("unknown-input");
    let targets = [
        missing.clone(),
        crate::PendingTurnInputCancelTarget::source_key("unknown-source"),
    ];
    assert_eq!(
        serde_json::to_value(
            runtime
                .cancel_pending_turn_input("unknown-input")
                .await
                .expect("live unknown input")
        )
        .expect("encode live cancel"),
        serde_json::to_value(
            ops.cancel_pending_turn_input(&store, "unknown-input")
                .await
                .expect("durable unknown input")
        )
        .expect("encode durable cancel")
    );
    assert_eq!(
        serde_json::to_value(
            runtime
                .cancel_pending_turn_inputs(&targets)
                .await
                .expect("live selected targets")
        )
        .expect("encode live selected"),
        serde_json::to_value(
            ops.cancel_pending_turn_inputs(&store, &targets)
                .await
                .expect("durable selected targets")
        )
        .expect("encode durable selected")
    );
    assert_eq!(
        serde_json::to_value(
            runtime
                .cancel_pending_turn_input_suffix(&missing)
                .await
                .expect("live missing suffix")
        )
        .expect("encode live suffix"),
        serde_json::to_value(
            ops.cancel_pending_turn_input_suffix(&store, &missing)
                .await
                .expect("durable missing suffix")
        )
        .expect("encode durable suffix")
    );
    assert!(
        ops.turn_input_applications(&store)
            .await
            .expect("settled input read")
            .is_empty()
    );
    assert!(
        ops.queued_work(&store)
            .await
            .expect("queued work read")
            .is_empty()
    );
    let batch = store
        .enqueue_queued_work(crate::runtime::QueuedWorkBatchDraft::new(
            world.session_id.clone(),
            crate::DeliveryPolicy::AfterCurrentTurnCommit,
            crate::facade_support::SessionCommand::RefreshToolCatalog {
                reason: "rail parity".into(),
            },
        ))
        .await
        .expect("populate command queue");
    let cancelled = runtime
        .cancel_queued_work_batch(batch.batch_id.as_str())
        .await
        .expect("live populated command cancel")
        .expect("same batch");
    assert_eq!(cancelled.batch_id, batch.batch_id);
    assert!(
        ops.cancel_queued_work_batch(&store, batch.batch_id.as_str())
            .await
            .expect("durable observes command cancellation")
            .is_none()
    );
    let input = runtime
        .enqueue_turn_input(
            crate::TurnInput::text("selected cancellation"),
            crate::TurnInputIngress::NextTurn,
            Some("selected-source".into()),
        )
        .await
        .expect("live selected input");
    let selected = ops
        .cancel_pending_turn_inputs(
            &store,
            &[crate::PendingTurnInputCancelTarget::source_key(
                "selected-source",
            )],
        )
        .await
        .expect("durable source-key cancellation");
    assert!(
        matches!(&selected[0].outcome, crate::PendingTurnInputCancelOutcome::Cancelled(row) if row.input_id == input.input_id)
    );
    assert!(
        ops.pending_turn_inputs(&store)
            .await
            .expect("both mutations drained")
            .is_empty()
    );

    let absent =
        crate::store::SessionStore::new(world.store().await, SessionId::from("absent-rail"))
            .expect("noncreating view");
    assert!(
        ops.enqueue_turn_input(
            &absent,
            crate::TurnInput::text("foreign request"),
            crate::TurnInputIngress::NextTurn,
            None,
            crate::RunSpec::default()
        )
        .await
        .is_err()
    );
    let blocked: Arc<dyn crate::RuntimeStore> =
        Arc::new(QueueCapabilityRefusal(world.store().await));
    let blocked_runtime =
        crate::RuntimeHandle::new(world.runtime_on_store(None, blocked.clone()).await);
    let blocked_view = crate::store::SessionStore::new(blocked, world.session_id.clone())
        .expect("capability-refusing view");
    let live = blocked_runtime
        .enqueue_turn_input(
            crate::TurnInput::text("unsupported"),
            crate::TurnInputIngress::NextTurn,
            None,
        )
        .await
        .expect_err("live capability refusal");
    let durable = ops
        .enqueue_turn_input(
            &blocked_view,
            crate::TurnInput::text("unsupported"),
            crate::TurnInputIngress::NextTurn,
            None,
            crate::RunSpec::default(),
        )
        .await
        .expect_err("durable capability refusal");
    assert_eq!(live.code, durable.code);
    assert_eq!(live.message, durable.message);
    assert!(live.message.contains("admit_pending_turn_inputs"));
    macro_rules! refused {
        ($live:expr, $durable:expr, $operation:literal) => {{
            let live = $live.await.expect_err("live capability refusal");
            let durable = $durable.await.expect_err("durable capability refusal");
            assert_eq!(live.code, durable.code);
            assert_eq!(live.message, durable.message);
            assert!(live.message.contains($operation), "{}", live.message);
        }};
    }
    refused!(
        blocked_runtime.cancel_pending_turn_input("unknown"),
        ops.cancel_pending_turn_input(&blocked_view, "unknown"),
        "cancel_pending_turn_inputs"
    );
    refused!(
        blocked_runtime.cancel_pending_turn_inputs(&targets),
        ops.cancel_pending_turn_inputs(&blocked_view, &targets),
        "cancel_pending_turn_inputs"
    );
    refused!(
        blocked_runtime.cancel_pending_turn_input_suffix(&missing),
        ops.cancel_pending_turn_input_suffix(&blocked_view, &missing),
        "cancel_pending_turn_input_suffix"
    );
    refused!(
        blocked_runtime.cancel_queued_work_batch("unknown"),
        ops.cancel_queued_work_batch(&blocked_view, "unknown"),
        "cancel_queued_work_batch"
    );
    for (operation, error) in [
        (
            "list_pending_turn_inputs",
            ops.pending_turn_inputs(&blocked_view)
                .await
                .expect_err("pending read capability"),
        ),
        (
            "list_turn_input_applications",
            ops.turn_input_applications(&blocked_view)
                .await
                .expect_err("settled read capability"),
        ),
        (
            "list_open_queued_work",
            ops.queued_work(&blocked_view)
                .await
                .expect_err("command read capability"),
        ),
    ] {
        assert!(error.message.contains(operation), "{}", error.message);
    }
}

struct QueueCapabilityRefusal(Arc<dyn crate::RuntimeStore>);
#[async_trait::async_trait]
impl crate::store::RuntimeStoreDecorator for QueueCapabilityRefusal {
    type Inner = dyn crate::RuntimeStore;
    fn inner(&self) -> &Self::Inner {
        self.0.as_ref()
    }
    async fn admit_pending_turn_inputs(
        &self,
        _batch: crate::PendingTurnInputBatch,
    ) -> Result<crate::TurnInputAdmission, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "admit_pending_turn_inputs",
        })
    }
    async fn cancel_pending_turn_inputs(
        &self,
        _session: &SessionId,
        _targets: &[crate::PendingTurnInputCancelTarget],
    ) -> Result<Vec<crate::PendingTurnInputCancelReceipt>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "cancel_pending_turn_inputs",
        })
    }
    async fn cancel_pending_turn_input_suffix(
        &self,
        _session: &SessionId,
        _anchor: &crate::PendingTurnInputCancelTarget,
    ) -> Result<crate::PendingTurnInputSuffixCancelOutcome, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "cancel_pending_turn_input_suffix",
        })
    }
    async fn cancel_queued_work_batch(
        &self,
        _session: &SessionId,
        _batch: &str,
    ) -> Result<Option<crate::QueuedWorkBatch>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "cancel_queued_work_batch",
        })
    }
    async fn list_pending_turn_inputs(
        &self,
        _session: &SessionId,
    ) -> Result<Vec<crate::PendingTurnInputRead>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "list_pending_turn_inputs",
        })
    }
    async fn list_turn_input_applications(
        &self,
        _session: &SessionId,
    ) -> Result<Vec<crate::TurnInputApplication>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "list_turn_input_applications",
        })
    }
    async fn list_open_queued_work(
        &self,
        _session: &SessionId,
    ) -> Result<Vec<crate::QueuedWorkBatch>, crate::StoreError> {
        Err(crate::StoreError::UnsupportedStoreOperation {
            operation: "list_open_queued_work",
        })
    }
}

#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn fork_inherits_history_without_execution_queues_waits_or_journals_on_engine(
    tier: ToolCallIdentityTier,
) {
    let factory = tier.stores.session_store_factory();
    let world = World::new(&tier, "fork-journal-isolation");
    assert_finished(
        "seed",
        &world
            .run(&world.turn("seed", vec![text("fork journal prefix")]))
            .await,
    );
    let store = world.store().await;
    let before = store
        .load_session_window(&world.session_id, crate::store::WindowSelector::Current)
        .await
        .expect("source window")
        .expect("source");
    let leaf = before.window.leaf_node_id.clone().expect("source leaf");
    let branch = SessionId::fixture(format!("{}-branch", world.session_id));
    let executed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for session in [&world.session_id, &branch] {
        if session == branch {
            factory
                .fork_session(&crate::ForkSessionRequest {
                    pending_observer_intents: Vec::new(),
                    session_id: branch.clone(),
                    source_session_id: world.session_id.clone(),
                    head_revision: before.head_revision,
                    relation: crate::SessionRelation::Fork {
                        source_session_id: world.session_id.clone(),
                        source_node_id: Some(leaf.clone()),
                    },
                    config: crate::testing::mock_session_policy().into(),
                })
                .await
                .expect("fork after source journal settled");
        }
        let scope = crate::ExecutionScope::turn(session.clone(), "same-turn");
        assert_eq!(
            scope.session_id(),
            Some(session),
            "journal admission retains the branch session identity"
        );
        assert_ne!(
            crate::ExecutionScope::turn(world.session_id.clone(), "same-turn")
                .journal_identity()
                .expect("source journal"),
            crate::ExecutionScope::turn(branch.clone(), "same-turn")
                .journal_identity()
                .expect("branch journal"),
            "shared history never shares a journal address"
        );
        let envelope = journaled_conformance_envelope(&scope, "same-effect", "same-replay-key");
        let counter = executed.clone();
        let expected = serde_json::json!(session);
        tier.runner.run_turn(crate::admit(scope), Arc::new(move |controller| {
            let (envelope, counter, expected) = (envelope.clone(), counter.clone(), expected.clone());
            Box::pin(async move {
                let output = expected.clone();
                let result = controller.vm_effect(envelope, crate::RuntimeEffectLocalExecutor::testing(move |_| async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(crate::RuntimeEffectOutcome::LanguageRuntimeValue { value: output })
                })).await.expect("journaled effect in its own handler");
                assert!(matches!(result, crate::RuntimeEffectOutcome::LanguageRuntimeValue { value } if value == expected), "fork cannot read its source outcome");
                crate::ConformanceTurnEnd::Settled
            })
        })).await;
    }
    assert_eq!(
        executed.load(Ordering::SeqCst),
        2,
        "both journals execute their first admission"
    );
    tier.runner.scenario_finished().await;
}

/// A journaled language-value envelope under `execution_scope`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a constant replay key is a valid address"
)]
pub(crate) fn journaled_conformance_envelope(
    execution_scope: &crate::ExecutionScope,
    effect_id: &str,
    operation: &str,
) -> crate::RuntimeEffectEnvelope {
    let replay_key = format!("journaled-replay:{effect_id}");
    crate::RuntimeEffectEnvelope::new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(execution_scope.clone(), replay_key)
                .expect("valid journaled conformance address"),
            crate::RuntimeAttribution::for_turn("journaled-session", "journaled-turn", 7, 0),
            format!("journaled:{effect_id}"),
        ),
        crate::RuntimeEffectCommand::LanguageRuntimeValue {
            operation: operation.to_string(),
        },
    )
}
