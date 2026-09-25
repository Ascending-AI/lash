//! The FIG-3571 cutover laws of the turn-crash matrix: a turn left in flight
//! by the pre-cutover build is refused, typed, before any effect, and so is a
//! claim on a session whose generation moved behind a runtime already open on
//! it (FIG-3619).
//!
//! Both laws run their turns on the tier's
//! [`ConformanceTurnRunner`](crate::ConformanceTurnRunner), so one body states
//! the refusal on every engine: in process, and inside a Restate handler.

use super::*;
use pretty_assertions::assert_eq;

/// What a refused run of a law's turn reports: the builder's admission
/// refusal, or the turn's own.
type Refusal = Result<crate::SessionError, crate::RuntimeError>;

/// A turn in flight under the previous session-state generation is refused
/// before any model, tool or provider effect when the next build redrives it.
///
/// The reference turn runs on the tier's runner and crashes after its tool
/// ran and its group settled, before the outcome reached the runtime, so the
/// durable prefix holds a claimed run, a model response and an executed tool.
/// The session's physical generation marker is then stamped to the generation
/// before [`crate::store::CURRENT_SESSION_STATE_VERSION`] — the marker every
/// session the pre-cutover build created carries — with every payload left as
/// the crashed turn wrote it, so without the generation gate the successor
/// would redrive the turn. The tier then recovers the turn its own way (a
/// fresh runtime in process, a redelivered invocation on Restate). Admission
/// must refuse it with the typed `SessionStateVersionUnsupported` store error,
/// and no provider request, tool dispatch, effect execution or durable commit
/// may cross a seam.
///
/// The refused turn was in flight, so the redrive parks it with the typed
/// `SessionStateGenerationRefused` reason (FIG-3735): an earlier execution
/// already journaled its commands, and a durable engine must keep that
/// journal for a build of the turn's own generation rather than end the
/// invocation where the journal holds its next command. The tier's engine
/// rests the parked turn — Restate pauses the invocation after the turn
/// handler's attempt budget — and each refused run re-parks it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn pre_cutover_generation_turn_redrive_is_refused_before_any_effect<F, S>(
    stores: Arc<dyn crate::StoreSet>,
    make: F,
    host: Arc<dyn crate::EffectHost>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimePersistence + crate::store::StoreTestSupport + 'static,
{
    let scenario = "pre-cutover-generation-redrive";
    let identity = ReferenceIdentity::for_scenario(scenario);
    let store = make(scenario) as Arc<dyn RuntimePersistence>;
    seed_reference_ingress(&store, &identity, scenario).await;
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tool = TraceTool::default();
    let control = SeamControl::default();
    let seam = SeamLayer {
        control: control.clone(),
        executions: Arc::clone(&executions),
        journal_faults: None,
    };
    let point = TurnCrashPoint {
        operation: TurnSeamOperation::Effect(EffectOperation::GroupSettle),
        placement: CrashPlacement::InsideCall,
    };
    control.arm(point.clone());
    let crash = crash_at_armed_point(&control);
    let crashing: crate::ConformanceTurnAttempt = {
        let stores = Arc::clone(&stores);
        let store = Arc::clone(&store);
        let host = Arc::clone(&host);
        let identity = identity.clone();
        let tool = tool.clone();
        Arc::new(move |scoped| {
            let stores = Arc::clone(&stores);
            let store = SeamStore::wrap(Arc::clone(&store), seam.control.clone());
            let host = Arc::clone(&host);
            let identity = identity.clone();
            let seam = seam.clone();
            let tool = tool.clone();
            Box::pin(async move {
                let runtime = Box::pin(try_build_runtime_on_host(
                    Arc::clone(&stores),
                    store,
                    &seam,
                    host,
                    &identity,
                    tool,
                    crashed_turn_timings(),
                ))
                .await
                .expect("build the crashing reference runtime");
                let turn = Box::pin(drive_turn_on(runtime, seam.over_scoped(scoped))).await;
                panic!("the armed crash point was never reached: {turn:?}");
            })
        })
    };
    runner
        .run_turn_until_crash(reference_admitted_scope(&identity), crashing, crash)
        .await;
    let crashed = control.trace();
    assert!(
        crashed
            .iter()
            .any(|operation| matches!(operation, TurnSeamOperation::Provider(_))),
        "the crashed turn asked the model before it died: {crashed:?}"
    );
    let executed_before = tool.executed.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        executed_before > 0,
        "the crashed turn executed its tool before it died"
    );
    let dispatched_before = executions.load(std::sync::atomic::Ordering::SeqCst);
    wait_for_recovery_lease(
        &|scenario: &str| make(scenario) as Arc<dyn RuntimePersistence>,
        scenario,
        &point,
        true,
    )
    .await;

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
    let successor_seam = SeamLayer {
        control: successor_control.clone(),
        executions: Arc::clone(&executions),
        journal_faults: None,
    };
    let (refusals, mut refused) = tokio::sync::mpsc::unbounded_channel::<Refusal>();
    let admitted = reference_admitted_scope(&identity);
    let redrive: crate::ConformanceTurnAttempt = {
        let stores = Arc::clone(&stores);
        let store = make(scenario) as Arc<dyn RuntimePersistence>;
        let host = Arc::clone(&host);
        let identity = identity.clone();
        let tool = tool.clone();
        let admitted = admitted.clone();
        let session = Arc::clone(&predecessor) as Arc<dyn RuntimePersistence>;
        Arc::new(move |scoped| {
            let stores = Arc::clone(&stores);
            let store = SeamStore::wrap(Arc::clone(&store), successor_seam.control.clone());
            let host = Arc::clone(&host);
            let identity = identity.clone();
            let seam = successor_seam.clone();
            let tool = tool.clone();
            let admitted = admitted.clone();
            let session = Arc::clone(&session);
            let refusals = refusals.clone();
            Box::pin(async move {
                let refusal = match Box::pin(try_build_runtime_on_host(
                    Arc::clone(&stores),
                    store,
                    &seam,
                    host,
                    &identity,
                    tool,
                    nominal_recovery_timings(),
                ))
                .await
                {
                    Err(refusal) => Ok(refusal),
                    Ok(runtime) => {
                        match Box::pin(drive_turn_on(runtime, seam.over_scoped(scoped))).await {
                            Err(error) => Err(error),
                            Ok(turn) => panic!(
                                "a pre-cutover session must not be admitted by the next build: {turn:?}"
                            ),
                        }
                    }
                };
                let generations = match &refusal {
                    Ok(crate::SessionError::Store { source, .. }) => {
                        crate::SessionStateVersionRefusal::of_store_error(source)
                    }
                    Ok(_) => None,
                    Err(error) => error.session_state_version_refusal(),
                };
                // The redrive met the generation gate with its turn in flight:
                // the turn parks, typed, and its handler ends the attempt as
                // every parked turn does.
                let parked = match generations {
                    Some(generations) => crate::park_turn_refused_by_generation(
                        session.as_ref(),
                        admitted.scope(),
                        generations,
                        stores.clock().timestamp_ms(),
                    )
                    .await
                    .expect("record the refused turn's park"),
                    None => None,
                };
                let _ = refusals.send(refusal);
                if parked.is_some() {
                    crate::ConformanceTurnEnd::Aborted(crate::TurnFailureCause::Parked)
                } else {
                    crate::ConformanceTurnEnd::Settled
                }
            })
        })
    };
    let runs = runner
        .run_parking_turn_until_rested(admitted, redrive)
        .await;
    assert!(runs >= 1, "the redrive ran");
    for run in 0..runs {
        let refused = refused
            .recv()
            .await
            .expect("every run of the redrive reported its refusal");
        match refused {
            Ok(crate::SessionError::Store { source, .. }) => assert!(
                matches!(
                    &source,
                    StoreError::SessionStateVersionUnsupported { found, current }
                        if *found == previous
                            && *current == crate::store::CURRENT_SESSION_STATE_VERSION
                ),
                "run {run}: the pre-cutover generation must be refused as unsupported, got \
                 {source:?}"
            ),
            Ok(other) => {
                panic!("run {run}: the refusal must be the typed store error, got {other:?}")
            }
            Err(error) => assert_eq!(
                error.session_state_version_refusal(),
                Some(crate::SessionStateVersionRefusal {
                    found: previous,
                    current: crate::store::CURRENT_SESSION_STATE_VERSION,
                }),
                "run {run}: the pre-cutover generation must be refused as unsupported, got \
                 {error:?}"
            ),
        }
    }
    let park = predecessor
        .load_turn_park(&identity.session_id)
        .await
        .expect("read the refused turn's park")
        .expect("the refused in-flight turn is parked");
    assert_eq!(
        (&park.turn_id, park.reason.code(), park.attempts as usize),
        (
            &identity.turn_id,
            crate::store::ParkReasonCode::SessionStateGenerationRefused,
            runs
        ),
        "the turn parks once per refused run, with the typed generation refusal: {park:?}"
    );
    assert!(
        matches!(
            park.reason,
            crate::store::ParkReason::SessionStateGenerationRefused { found, current, .. }
                if found == previous && current == crate::store::CURRENT_SESSION_STATE_VERSION
        ),
        "the park names both generations: {park:?}"
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
    assert_eq!(
        tool.executed.load(std::sync::atomic::Ordering::SeqCst),
        executed_before,
        "the refused redrive ran no tool"
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
pub async fn pre_cutover_generation_turn_claim_is_refused_typed<F, S>(
    stores: Arc<dyn crate::StoreSet>,
    make: F,
    host: Arc<dyn crate::EffectHost>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimePersistence + crate::store::StoreTestSupport + 'static,
{
    for (scenario, path) in [
        ("pre-cutover-generation-claim-direct", ClaimPath::Direct),
        ("pre-cutover-generation-claim-queued", ClaimPath::Queued),
    ] {
        Box::pin(refuse_claim(&stores, &make, &host, &runner, scenario, path)).await;
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn refuse_claim<F, S>(
    stores: &Arc<dyn crate::StoreSet>,
    make: &F,
    host: &Arc<dyn crate::EffectHost>,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    scenario: &str,
    path: ClaimPath,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimePersistence + crate::store::StoreTestSupport + 'static,
{
    let identity = ReferenceIdentity::for_scenario(scenario);
    let store = make(scenario) as Arc<dyn RuntimePersistence>;
    seed_reference_ingress(&store, &identity, scenario).await;
    let previous = crate::store::CURRENT_SESSION_STATE_VERSION - 1;
    let predecessor = make(scenario);
    super::super::bind_conformance_session(
        &(Arc::clone(&predecessor) as Arc<dyn RuntimePersistence>),
        &identity.session_id,
    )
    .await;
    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seam = SeamLayer {
        control: control.clone(),
        executions: Arc::clone(&executions),
        journal_faults: None,
    };
    let tool = TraceTool::default();
    let (refusals, mut refused) = tokio::sync::mpsc::unbounded_channel();
    let attempt: crate::ConformanceTurnAttempt = {
        let stores = Arc::clone(stores);
        let host = Arc::clone(host);
        let identity = identity.clone();
        let tool = tool.clone();
        Arc::new(move |scoped| {
            let stores = Arc::clone(&stores);
            let store = SeamStore::wrap(Arc::clone(&store), seam.control.clone());
            let host = Arc::clone(&host);
            let identity = identity.clone();
            let seam = seam.clone();
            let tool = tool.clone();
            let predecessor = Arc::clone(&predecessor);
            let refusals = refusals.clone();
            Box::pin(async move {
                let mut runtime = Box::pin(try_build_runtime_on_host(
                    Arc::clone(&stores),
                    store,
                    &seam,
                    host,
                    &identity,
                    tool,
                    crashed_turn_timings(),
                ))
                .await
                .expect("build the reference runtime while its generation is current");
                // The pre-cutover build's marker lands under the open runtime.
                predecessor
                    .stamp_session_state_version_for_testing(previous)
                    .await
                    .expect("stamp the pre-cutover generation marker");
                seam.control.clear();
                let scoped = seam.over_scoped(scoped);
                let refused = match path {
                    ClaimPath::Direct => {
                        let mut input =
                            crate::TurnInput::text("direct turn on a pre-cutover session");
                        input.trace_turn_id = Some(identity.turn_id.clone());
                        runtime
                            .stream_turn(
                                input,
                                crate::TurnOptions::new(
                                    tokio_util::sync::CancellationToken::new(),
                                    scoped,
                                ),
                            )
                            .await
                            .err()
                    }
                    ClaimPath::Queued => Box::pin(drive_turn_on(runtime, scoped)).await.err(),
                }
                .unwrap_or_else(|| panic!("{path:?}: a pre-cutover session must not run a turn"));
                let _ = refusals.send(refused);
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    runner
        .run_turn(reference_admitted_scope(&identity), attempt)
        .await;
    let refused: crate::RuntimeError = refused.recv().await.expect("the turn reported its refusal");

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
    assert_eq!(
        tool.executed.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "{path:?}: the refused turn ran no tool"
    );
}
