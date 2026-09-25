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
/// outcome was recorded, then let the tier recover the same turn (a fresh
/// runtime in process, a redelivered invocation on Restate): exactly one
/// pending row exists across the crash, the redrive returns its id, and the
/// committed turn carries the words once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn direct_turn_acceptance_crash_after_store_commit_admits_one_row<F, S>(
    stores: Arc<dyn crate::StoreSet>,
    make: F,
    host: Arc<dyn crate::EffectHost>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimePersistence + crate::store::StoreTestSupport + 'static,
{
    let scenario = "direct-acceptance-after-store-commit";
    let make = |scenario: &str| make(scenario) as Arc<dyn RuntimePersistence>;
    let identity = ReferenceIdentity::for_scenario(scenario);
    let point = TurnCrashPoint {
        operation: TurnSeamOperation::Effect(EffectOperation::AcceptTurnInput),
        placement: CrashPlacement::AfterExternalEffectBeforeOutcome,
    };
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let acceptance_bodies = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let control = SeamControl::default();
    control.arm(point.clone());
    let crash = crash_at_armed_point(&control);
    let attempt = |seam: SeamLayer,
                   lease_timings: crate::LeaseTimings,
                   turns: Option<
        tokio::sync::mpsc::UnboundedSender<Result<crate::AssembledTurn, crate::RuntimeError>>,
    >|
     -> crate::ConformanceTurnAttempt {
        let stores = Arc::clone(&stores);
        let store = counted(make(scenario), &acceptance_bodies);
        let host = Arc::clone(&host);
        let identity = identity.clone();
        Arc::new(move |scoped| {
            let stores = Arc::clone(&stores);
            let store = SeamStore::wrap(Arc::clone(&store), seam.control.clone());
            let host = Arc::clone(&host);
            let identity = identity.clone();
            let seam = seam.clone();
            let turns = turns.clone();
            Box::pin(async move {
                let mut runtime = Box::pin(try_build_runtime_on_host(
                    stores.as_ref(),
                    store,
                    &seam,
                    host,
                    &identity,
                    TraceTool::default(),
                    lease_timings,
                ))
                .await
                .expect("build the direct-acceptance reference runtime");
                let turn = runtime
                    .stream_turn(
                        direct_input(&identity),
                        crate::TurnOptions::new(
                            tokio_util::sync::CancellationToken::new(),
                            seam.over_scoped(scoped),
                        ),
                    )
                    .await;
                let Some(turns) = turns else {
                    panic!("the armed acceptance crash point was never reached: {turn:?}");
                };
                let end = crate::ConformanceTurnEnd::of(&turn);
                let _ = turns.send(turn);
                end
            })
        })
    };
    runner
        .run_turn_until_crash(
            reference_admitted_scope(&identity),
            attempt(
                SeamLayer {
                    control: control.clone(),
                    executions: Arc::clone(&executions),
                    journal_faults: None,
                },
                crashed_turn_timings(),
                None,
            ),
            crash,
        )
        .await;

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

    // Acceptance commits before the drive takes the session lane (FIG-3600),
    // so the crash leaves no predecessor executor for the recovery to displace.
    wait_for_recovery_lease(&make, scenario, &point, false).await;
    let (turns, mut turned) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(
            reference_admitted_scope(&identity),
            attempt(
                SeamLayer {
                    control: SeamControl::default(),
                    executions: Arc::clone(&executions),
                    journal_faults: None,
                },
                nominal_recovery_timings(),
                Some(turns),
            ),
        )
        .await;
    let turn = turned
        .recv()
        .await
        .expect("the redrive reported its turn")
        .unwrap_or_else(|error| panic!("the redriven acceptance commits the turn: {error}"));

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
