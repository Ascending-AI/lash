//! The direct-turn acceptance placement of the turn crash matrix (FIG-3513).
//!
//! The matrix's reference turn is a queued drain, which never accepts input.
//! A direct turn's first durable write is its journaled acceptance, so the
//! crash that matters there is the one between the acceptance's store commit
//! and the journal recording its outcome: the redrive runs the acceptance body
//! again, and must adopt the row the first run wrote instead of admitting it a
//! second time.

use super::*;
use pretty_assertions::assert_eq;

const DIRECT_INPUT: &str = "direct accepted input";

/// Counts acceptance bodies: the acceptance executor's store write is the one
/// `enqueue_pending_turn_input` a direct turn makes.
struct CountingAcceptanceStore {
    inner: Arc<dyn RuntimePersistence>,
    enqueues: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for CountingAcceptanceStore {
    fn inner(&self) -> &(dyn RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn enqueue_pending_turn_input(
        &self,
        draft: crate::PendingTurnInputDraft,
    ) -> Result<crate::PendingTurnInput, StoreError> {
        self.enqueues
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.enqueue_pending_turn_input(draft).await
    }
}

fn counted(
    inner: Arc<dyn RuntimePersistence>,
    enqueues: &Arc<std::sync::atomic::AtomicUsize>,
) -> Arc<dyn RuntimePersistence> {
    Arc::new(CountingAcceptanceStore {
        inner,
        enqueues: Arc::clone(enqueues),
    })
}

fn direct_input(identity: &ReferenceIdentity) -> crate::TurnInput {
    let mut input = crate::TurnInput::text(DIRECT_INPUT);
    input.trace_turn_id = Some(identity.turn_id.clone());
    input
}

/// Crash a direct turn after its acceptance committed the row and before the
/// outcome was recorded, then redrive the same turn: exactly one pending row
/// exists across the crash, the redrive returns its id, and the committed turn
/// carries the words once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn direct_turn_acceptance_crash_after_store_commit_admits_one_row<F, I>(
    make: F,
    make_invocation: I,
) where
    F: Fn(&str) -> Arc<dyn RuntimePersistence>,
    I: Fn(&str, crate::ExecutionScope) -> crate::ConformanceInvocation,
{
    let scenario = "direct-acceptance-after-store-commit";
    let identity = ReferenceIdentity::for_scenario(scenario);
    let point = TurnCrashPoint {
        operation: TurnSeamOperation::Effect(EffectOperation::AcceptTurnInput),
        placement: CrashPlacement::AfterExternalEffectBeforeOutcome,
    };

    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let acceptance_bodies = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let invocation = make_invocation(scenario, drive_scope(&identity));
    let effect_controller: Arc<dyn RuntimeEffectController> = Arc::new(SeamEffectController {
        inner: invocation.controller_handle(),
        control: control.clone(),
        executions: Arc::clone(&executions),
        journal_faults: invocation.effect_journal_faults(),
    });
    let mut runtime = Box::pin(build_runtime(
        SeamStore::wrap(counted(make(scenario), &acceptance_bodies), control.clone()),
        control.clone(),
        Arc::clone(&effect_controller),
        invocation.process_env_store(),
        &identity,
        TraceTool::default(),
    ))
    .await;
    control.arm(point.clone());
    let task_identity = identity.clone();
    let task = crate::task::spawn(async move {
        runtime
            .stream_turn(
                direct_input(&task_identity),
                crate::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    scoped_controller(effect_controller, &task_identity),
                ),
            )
            .await
    });
    control.wait_for_hit().await;
    control.simulate_process_crash();
    task.abort();
    let _ = task.await;

    let reader = make(scenario);
    super::super::bind_conformance_session(&reader, &identity.session_id).await;
    let admitted = reader
        .list_pending_turn_inputs(&identity.session_id)
        .await
        .expect("list inputs after the crash");
    assert_eq!(
        admitted.len(),
        1,
        "the crashed acceptance committed exactly its own row: {admitted:?}"
    );
    let admitted_id = admitted[0].input.input_id.clone();

    let successor_invocation = invocation.redrive();
    wait_for_recovery_lease(&make, scenario, &point, true).await;
    let successor_control = SeamControl::default();
    let successor_effect_controller: Arc<dyn RuntimeEffectController> =
        Arc::new(SeamEffectController {
            inner: successor_invocation.controller_handle(),
            control: successor_control.clone(),
            executions: Arc::clone(&executions),
            journal_faults: successor_invocation.effect_journal_faults(),
        });
    let mut successor = Box::pin(build_runtime_with_lease_timings(
        SeamStore::wrap(
            counted(make(scenario), &acceptance_bodies),
            successor_control.clone(),
        ),
        successor_control.clone(),
        Arc::clone(&successor_effect_controller),
        successor_invocation.process_env_store(),
        &identity,
        TraceTool::default(),
        nominal_recovery_timings(),
    ))
    .await;
    successor_control.clear();
    let turn = successor
        .stream_turn(
            direct_input(&identity),
            crate::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scoped_controller(successor_effect_controller, &identity),
            ),
        )
        .await
        .unwrap_or_else(|error| panic!("the redriven acceptance commits the turn: {error}"));
    successor_invocation.end();

    assert_eq!(
        acceptance_bodies.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the redrive re-ran the acceptance body: the adoption path was exercised, \
         not a journaled outcome"
    );

    assert_eq!(
        turn.turn_input_acceptance
            .as_ref()
            .map(|acceptance| acceptance.input_id.clone()),
        Some(admitted_id.clone()),
        "the redrive returns the input id the crashed run admitted"
    );
    assert!(
        reader
            .list_pending_turn_inputs(&identity.session_id)
            .await
            .expect("list inputs after the redrive")
            .is_empty(),
        "the redrive admitted no second row"
    );
    let applied = reader
        .list_turn_input_applications(&identity.session_id)
        .await
        .expect("read applications after the redrive");
    assert_eq!(
        applied
            .iter()
            .map(|application| application.input_id.clone())
            .collect::<Vec<_>>(),
        vec![admitted_id],
        "the turn applied the one admission"
    );
    let copies = turn
        .state
        .read_view()
        .expect("the redriven turn's frame scope resolves")
        .messages()
        .iter()
        .filter(|message| {
            matches!(message.role, crate::MessageRole::User)
                && message.parts.iter().any(|part| {
                    matches!(part, crate::Part::Text { content, .. } if content.contains(DIRECT_INPUT))
                })
        })
        .count();
    assert_eq!(copies, 1, "the committed turn carries the words once");
}
