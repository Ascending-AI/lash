//! The FIG-3571 cutover law of the turn-crash matrix: a turn left in flight
//! by the pre-cutover build is refused, typed, before any effect.
//!
//! Split out of `turn_crash_matrix.rs` to keep that file under the line
//! budget; the law keeps its path through the parent's re-export.

use super::*;
use pretty_assertions::assert_eq;

/// A turn in flight under the previous session-state generation is refused
/// before any model, tool or provider effect when the next build redrives it.
///
/// The reference turn runs and crashes after its tool attempt executed and
/// before the outcome reached the runtime, so the durable prefix holds a
/// claimed run, a journaled model response and a dispatched tool. The
/// session's physical generation marker is then stamped to the generation
/// before [`crate::store::CURRENT_SESSION_STATE_VERSION`] — the marker every
/// session the pre-cutover build created carries — with every payload left as
/// the crashed turn wrote it, so without the generation gate the successor
/// would redrive the turn. Recovery builds a successor runtime over the same store the way a restarted
/// host does. Admission must refuse it with the typed
/// `SessionStateVersionUnsupported` store error, and no provider request, tool
/// dispatch, effect execution or durable commit may cross a seam.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pre_cutover_generation_turn_redrive_is_refused_before_any_effect<F, S, I>(
    make: F,
    make_invocation: I,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimePersistence + crate::store::StoreTestSupport + 'static,
    I: Fn(&str) -> crate::ConformanceInvocation,
{
    let scenario = "pre-cutover-generation-redrive";
    let make_runtime = |scenario: &str| make(scenario) as Arc<dyn RuntimePersistence>;
    let identity = ReferenceIdentity::for_scenario(scenario);
    let raw = make_runtime(scenario);
    seed_reference_ingress(&raw, &identity, scenario).await;
    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let decorated = SeamStore::wrap(raw, control.clone());
    let invocation = make_invocation(scenario);
    let effect_controller: Arc<dyn RuntimeEffectController> = Arc::new(SeamEffectController {
        inner: invocation.controller_handle(),
        control: control.clone(),
        executions: Arc::clone(&executions),
        journal_faults: None,
    });
    let runtime = Box::pin(build_runtime(
        decorated,
        control.clone(),
        Arc::clone(&effect_controller),
        &identity,
        TraceTool::default(),
    ))
    .await;
    let point = TurnCrashPoint {
        operation: TurnSeamOperation::Effect(EffectOperation::ToolAttempt {
            name: "trace_effect".to_string(),
        }),
        placement: CrashPlacement::AfterExternalEffectBeforeOutcome,
    };
    control.arm(point.clone());
    let task_identity = identity.clone();
    let task = crate::task::spawn(async move {
        Box::pin(drive_turn(runtime, effect_controller, &task_identity)).await
    });
    control.wait_for_hit().await;
    control.simulate_process_crash();
    task.abort();
    let _ = task.await;
    let crashed = control.trace();
    assert!(
        crashed
            .iter()
            .any(|operation| matches!(operation, TurnSeamOperation::Provider(_))),
        "the crashed turn asked the model before it died: {crashed:?}"
    );
    let dispatched_before = executions.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        dispatched_before > 0,
        "the crashed turn dispatched its tool before it died"
    );
    wait_for_recovery_lease(&make_runtime, scenario, &point, true).await;

    // What the pre-cutover build left behind: a session on the previous
    // generation.
    let previous = crate::store::CURRENT_SESSION_STATE_VERSION - 1;
    let predecessor = make(scenario);
    super::super::bind_conformance_session(
        &(Arc::clone(&predecessor) as Arc<dyn RuntimePersistence>),
        &identity.session_id,
    )
    .await;
    predecessor
        .stamp_session_state_version_for_testing(previous)
        .await
        .expect("stamp the pre-cutover generation marker");

    let successor_control = SeamControl::default();
    let successor_store = SeamStore::wrap(make_runtime(scenario), successor_control.clone());
    let successor_invocation = invocation.redrive();
    let successor_effect_controller: Arc<dyn RuntimeEffectController> =
        Arc::new(SeamEffectController {
            inner: successor_invocation.controller_handle(),
            control: successor_control.clone(),
            executions: Arc::clone(&executions),
            journal_faults: None,
        });
    successor_control.clear();
    let refused = Box::pin(try_build_runtime_with_lease_timings(
        successor_store,
        successor_control.clone(),
        successor_effect_controller,
        &identity,
        TraceTool::default(),
        nominal_recovery_timings(),
    ))
    .await
    .err()
    .expect("a pre-cutover session must not be admitted by the next build");
    successor_invocation.end();

    let crate::SessionError::Store { source, .. } = &refused else {
        panic!("the refusal must be the typed store error, got {refused:?}");
    };
    assert!(
        matches!(
            source,
            StoreError::SessionStateVersionUnsupported { found, current }
                if *found == previous && *current == crate::store::CURRENT_SESSION_STATE_VERSION
        ),
        "the pre-cutover generation must be refused as unsupported, got {source:?}"
    );
    let redriven = successor_control.trace();
    assert_eq!(
        redriven
            .iter()
            .filter(|operation| matches!(
                operation,
                TurnSeamOperation::Provider(_)
                    | TurnSeamOperation::Effect(_)
                    | TurnSeamOperation::TurnControl(_)
                    | TurnSeamOperation::Store(StoreOperation::CommitFinalHead { .. })
            ))
            .collect::<Vec<_>>(),
        Vec::<&TurnSeamOperation>::new(),
        "no provider request, effect or commit may follow the refusal"
    );
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        dispatched_before,
        "the refused redrive dispatched no effect"
    );
}

/// Which turn-lane claim a pre-cutover session is refused on.
#[derive(Clone, Copy, Debug)]
enum ClaimPath {
    /// A direct turn claims the lane before its acceptance effect.
    Direct,
    /// A queued drain claims the lane through the queued-lane probe.
    Queued,
}

/// A runtime already open on a session whose generation marker then moves
/// behind this build is refused, typed, at the turn-lane claim (FIG-3619).
///
/// The builder admission is not what refuses here: the runtime is built while
/// the marker is current, and the pre-cutover generation lands afterwards, the
/// way a durable engine redrives a turn on a runtime it already holds. The
/// claim's admission must then surface `SessionStateVersionUnsupported` with
/// the found and current generations as a typed, terminal
/// [`RuntimeError`](crate::RuntimeError) — never a `store_commit_failed`
/// string a durable engine would retry — on both the direct and the queued
/// claim. Exactly one claim is attempted, and no provider request, effect or
/// commit follows it.
pub async fn pre_cutover_generation_turn_claim_is_refused_typed<F, S, I>(
    make: F,
    make_invocation: I,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimePersistence + crate::store::StoreTestSupport + 'static,
    I: Fn(&str) -> crate::ConformanceInvocation,
{
    for (scenario, path) in [
        ("pre-cutover-generation-claim-direct", ClaimPath::Direct),
        ("pre-cutover-generation-claim-queued", ClaimPath::Queued),
    ] {
        Box::pin(refuse_claim(&make, &make_invocation, scenario, path)).await;
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn refuse_claim<F, S, I>(make: &F, make_invocation: &I, scenario: &str, path: ClaimPath)
where
    F: Fn(&str) -> Arc<S>,
    S: RuntimePersistence + crate::store::StoreTestSupport + 'static,
    I: Fn(&str) -> crate::ConformanceInvocation,
{
    let identity = ReferenceIdentity::for_scenario(scenario);
    let raw = make(scenario) as Arc<dyn RuntimePersistence>;
    seed_reference_ingress(&raw, &identity, scenario).await;
    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let invocation = make_invocation(scenario);
    let effect_controller: Arc<dyn RuntimeEffectController> = Arc::new(SeamEffectController {
        inner: invocation.controller_handle(),
        control: control.clone(),
        executions: Arc::clone(&executions),
        journal_faults: None,
    });
    let mut runtime = Box::pin(build_runtime(
        SeamStore::wrap(raw, control.clone()),
        control.clone(),
        Arc::clone(&effect_controller),
        &identity,
        TraceTool::default(),
    ))
    .await;

    // The pre-cutover build's marker lands under the open runtime.
    let previous = crate::store::CURRENT_SESSION_STATE_VERSION - 1;
    let predecessor = make(scenario);
    super::super::bind_conformance_session(
        &(Arc::clone(&predecessor) as Arc<dyn RuntimePersistence>),
        &identity.session_id,
    )
    .await;
    predecessor
        .stamp_session_state_version_for_testing(previous)
        .await
        .expect("stamp the pre-cutover generation marker");

    control.clear();
    let refused = match path {
        ClaimPath::Direct => {
            let mut input = crate::TurnInput::text("direct turn on a pre-cutover session");
            input.trace_turn_id = Some(identity.turn_id.clone());
            runtime
                .stream_turn(
                    input,
                    crate::TurnOptions::new(
                        tokio_util::sync::CancellationToken::new(),
                        scoped_controller(effect_controller, &identity),
                    ),
                )
                .await
                .err()
        }
        ClaimPath::Queued => Box::pin(drive_turn(runtime, effect_controller, &identity))
            .await
            .err(),
    }
    .unwrap_or_else(|| panic!("{path:?}: a pre-cutover session must not run a turn"));
    invocation.end();

    assert_eq!(
        refused.code,
        crate::RuntimeErrorCode::SessionStateVersionUnsupported,
        "{path:?}: the claim refusal keeps its type, not a store-commit string: {refused:?}"
    );
    assert_eq!(
        refused.session_state_version_refusal(),
        Some(crate::SessionStateVersionRefusal {
            found: previous,
            current: crate::store::CURRENT_SESSION_STATE_VERSION,
        }),
        "{path:?}: the refused call returns the found generation typed"
    );
    assert!(
        refused
            .message
            .contains(&format!("session state version {previous}")),
        "{path:?}: a stored copy of the error still names the found generation: {}",
        refused.message
    );
    assert!(
        refused.is_terminal() && !refused.is_retryable(),
        "{path:?}: a durable engine must end the invocation, not retry it: {refused:?}"
    );
    let trace = control.trace();
    assert_eq!(
        trace
            .iter()
            .filter(|operation| matches!(
                operation,
                TurnSeamOperation::Store(StoreOperation::ClaimSessionExecutionLease)
            ))
            .count(),
        1,
        "{path:?}: the refusal is not retried: {trace:?}"
    );
    assert_eq!(
        trace
            .iter()
            .filter(|operation| matches!(
                operation,
                TurnSeamOperation::Provider(_)
                    | TurnSeamOperation::Effect(_)
                    | TurnSeamOperation::TurnControl(_)
                    | TurnSeamOperation::Store(StoreOperation::CommitFinalHead { .. })
            ))
            .collect::<Vec<_>>(),
        Vec::<&TurnSeamOperation>::new(),
        "{path:?}: no provider request, effect or commit may follow the refusal"
    );
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "{path:?}: the refused turn dispatched no effect"
    );
}
