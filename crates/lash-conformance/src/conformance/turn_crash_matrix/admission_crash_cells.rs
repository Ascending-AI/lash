//! FIG-3927 N9's crash cells on the reference drain: a worker dies at a
//! durable admission or settlement write whose outcome never reached the
//! journal, and the tier redrives the root.
//!
//! - (b) The root's final commit landed and its reply was lost. The redrive
//!   replays the committed receipt by the commit's identity: it asks the
//!   model nothing, runs no tool, and settles nothing a second time.
//! - (c) The `AfterWork` checkpoint's admission bound the active-turn input
//!   and the ready wake to the root, and the step's outcome was never
//!   journaled. An active-turn input addressed to the same turn arrives
//!   before the redrive. The step runs again and must read back the rows it
//!   bound under its own step key, not compose again over what is open now:
//!   the model call after the checkpoint renders the input and the wake it
//!   bound and never the newcomer, which only a later boundary may take.
//!
//! Cell (a), the root admission crashed before its record, is the drive law
//! `a_root_admission_survives_a_worker_crash_without_widening`.
//!
//! Each cell crashes and redrives through
//! [`ConformanceTurnRunner::run_crashed_then_redriven_turn`](crate::ConformanceTurnRunner::run_crashed_then_redriven_turn):
//! the redrive is the tier's redelivery of the crashed execution.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use pretty_assertions::assert_eq;

/// The newcomer of cell (c): an active-turn input addressed to the reference
/// turn that arrives after the checkpoint bound its rows.
const NEWCOMER: &str = "newcomer checkpoint input";

/// Crash the reference drain of `scenario` at `point` and redrive it; before
/// the redrive's first run, `before_redrive` writes to the scenario's store.
/// Returns how the redrive's drain ended, the redrive's seam trace and how
/// many times the tool effect executed in all.
async fn crash_then_redrive(
    law: &MatrixLaw<'_>,
    identity: &ReferenceIdentity,
    scenario: &str,
    point: TurnCrashPoint,
    before_redrive: Option<PendingTurnInputDraft>,
) -> (reference_turn::DrainReport, Vec<TurnSeamOperation>, usize) {
    let executions = Arc::new(AtomicUsize::new(0));
    let control = SeamControl::default();
    let crash = crash_at_armed_point(&control);
    let crashing = ReferenceTurn::new(
        law.stores,
        (law.make)(scenario),
        law.host,
        identity,
        control,
        &executions,
        crashed_turn_timings(),
    )
    .before_drive(move |control| control.arm(point.clone()))
    .attempt();
    let crashing: crate::ConformanceTurnAttempt = Arc::new(move |scoped| {
        let crashing = Arc::clone(&crashing);
        let crash = crash.clone();
        Box::pin(async move {
            tokio::select! {
                biased;
                () = crash.fired() => panic!("the conformance crash killed the attempt"),
                end = crashing(scoped) => {
                    panic!("the crashing attempt ended ({end:?}) before its crash fired")
                }
            }
        })
    });

    let successor_control = SeamControl::default();
    let (successor, redriven) = ReferenceTurn::new(
        law.stores,
        (law.make)(scenario),
        law.host,
        identity,
        successor_control.clone(),
        &executions,
        nominal_recovery_timings(),
    )
    .before_drive(SeamControl::clear)
    .reporting();
    let writer = (law.make)(scenario);
    let pending_write = Arc::new(std::sync::Mutex::new(before_redrive));
    let written = Arc::new(AtomicBool::new(false));
    let redrive: crate::ConformanceTurnAttempt = Arc::new(move |scoped| {
        let successor = Arc::clone(&successor);
        let writer = Arc::clone(&writer);
        let pending_write = Arc::clone(&pending_write);
        let written = Arc::clone(&written);
        Box::pin(async move {
            let draft = if written.swap(true, Ordering::SeqCst) {
                None
            } else {
                pending_write.lock_recover().take()
            };
            if let Some(draft) = draft {
                #[expect(clippy::expect_used, reason = "conformance-law fixture write")]
                writer
                    .enqueue_pending_turn_input(draft)
                    .await
                    .expect("write before the redrive");
            }
            successor(scoped).await
        })
    });
    law.runner
        .run_crashed_then_redriven_turn(reference_admitted_scope(identity), crashing, redrive)
        .await;
    let report = reference_turn::reported(redriven).await;
    (
        report,
        successor_control.trace(),
        executions.load(Ordering::SeqCst),
    )
}

/// The law's matrix view of the tier's fixture.
fn matrix_law<'law>(
    stores: &'law Arc<dyn crate::StoreSet>,
    make: &'law dyn Fn(&str) -> Arc<dyn RuntimePersistence>,
    host: &'law LawSeamHost,
    runner: &'law Arc<dyn crate::ConformanceTurnRunner>,
) -> MatrixLaw<'law> {
    MatrixLaw {
        stores,
        make,
        host,
        runner,
    }
}

/// N9 (b): the root's final commit landed and its reply was lost. The
/// redrive replays the committed receipt: it asks the model nothing, runs no
/// tool, commits the root's one physical turn once, applies each input once
/// and leaves no row bound or queued.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_final_commit_whose_reply_was_lost_replays_its_receipt_and_settles_nothing_twice<
    F,
    S,
>(
    stores: Arc<dyn crate::StoreSet>,
    make: F,
    host: Arc<dyn crate::EffectHost>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimePersistence + crate::store::StoreTestSupport + 'static,
{
    let make = |scenario: &str| make(scenario) as Arc<dyn RuntimePersistence>;
    let host = LawSeamHost::over(host);
    let law = matrix_law(&stores, &make, &host, &runner);
    let scenario = "final-commit-reply-lost";
    let identity = ReferenceIdentity::for_scenario(scenario);
    let reader = make(scenario);
    seed_reference_ingress_for_drive(&reader, &identity, scenario).await;
    let seeded = reader
        .list_pending_turn_inputs(&identity.session_id)
        .await
        .expect("list the seeded inputs")
        .into_iter()
        .map(|read| read.input.input_id)
        .collect::<Vec<_>>();
    let before = reader
        .load_session_head_meta()
        .await
        .expect("read the head before the root")
        .map_or(0, |head| head.head_revision);

    let (report, redriven, executions) = Box::pin(crash_then_redrive(
        &law,
        &identity,
        scenario,
        TurnCrashPoint {
            operation: TurnSeamOperation::Store(StoreOperation::ApplyTurnCancelEffectsAndConsume),
            placement: CrashPlacement::InsideCall,
        },
        None,
    ))
    .await;
    let turn = report
        .unwrap_or_else(|error| panic!("the redrive replays the committed root: {error}"))
        .ran()
        .expect("the redrive runs its root");
    assert_eq!(
        turn.assistant_output.safe_text, "trace turn complete",
        "the redrive reads the committed root's answer back"
    );
    assert_eq!(executions, 1, "the tool ran once, before the crash");
    assert!(
        redriven.iter().all(|operation| !matches!(
            operation,
            TurnSeamOperation::Provider(_)
                | TurnSeamOperation::Effect(EffectOperation::ToolAttempt { .. })
        )),
        "the redrive asks the model nothing and runs no tool: {redriven:?}"
    );
    let head = reader
        .load_session_head_meta()
        .await
        .expect("read the head after the redrive")
        .expect("the root committed");
    assert_eq!(
        head.head_revision,
        before + 1,
        "the root's one physical turn committed once"
    );
    let applications = reader
        .list_turn_input_applications(&identity.session_id)
        .await
        .expect("read the applications");
    for input in &seeded {
        assert_eq!(
            applications
                .iter()
                .filter(|application| application.input_id == *input)
                .count(),
            1,
            "{input} is applied once: {applications:?}"
        );
    }
    assert!(
        reader
            .list_pending_turn_inputs(&identity.session_id)
            .await
            .expect("list the inputs after the redrive")
            .is_empty(),
        "no input stays pending"
    );
    assert!(
        reader
            .list_queued_work(&identity.session_id)
            .await
            .expect("list queued work after the redrive")
            .is_empty(),
        "the wake is settled once and nothing stays queued"
    );
    assert!(
        reader
            .unfinished_root(&identity.session_id)
            .await
            .expect("read the unfinished root")
            .is_none(),
        "the root ended"
    );
}

/// N9 (c): the checkpoint admission's worker died after the store bound its
/// rows and before the journal recorded the step. The redriven step reads
/// its own rows back: the model call after the checkpoint renders the
/// active-turn input and the wake it bound, and never the newcomer that
/// arrived in between.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_checkpoint_admission_crashed_before_its_record_redelivers_its_rows<F, S>(
    stores: Arc<dyn crate::StoreSet>,
    make: F,
    host: Arc<dyn crate::EffectHost>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimePersistence + crate::store::StoreTestSupport + 'static,
{
    let make = |scenario: &str| make(scenario) as Arc<dyn RuntimePersistence>;
    let host = LawSeamHost::over(host);
    let law = matrix_law(&stores, &make, &host, &runner);
    let scenario = "checkpoint-admission-crash";
    let identity = ReferenceIdentity::for_scenario(scenario);
    let reader = make(scenario);
    seed_reference_ingress_for_drive(&reader, &identity, scenario).await;
    let newcomer = PendingTurnInputDraft::new(
        &identity.session_id,
        crate::TurnInputIngress::active_turn(
            &identity.turn_id,
            crate::TurnInputCheckpointBoundary::AfterWork,
        ),
        crate::TurnInput::text(NEWCOMER),
    );

    let (report, _, _) = Box::pin(crash_then_redrive(
        &law,
        &identity,
        scenario,
        TurnCrashPoint {
            operation: TurnSeamOperation::Store(StoreOperation::AdmitAtCheckpoint {
                checkpoint: "afterwork".to_string(),
            }),
            placement: CrashPlacement::InsideCall,
        },
        Some(newcomer),
    ))
    .await;
    report
        .unwrap_or_else(|error| panic!("the redriven root failed: {error}"))
        .ran()
        .expect("the redrive runs its root");

    let state = crate::load_persisted_session_state(reader.as_ref())
        .await
        .expect("read the redriven root's state")
        .expect("the redriven root committed");
    let read_model = state
        .session_graph
        .read_model(None)
        .expect("the committed frame resolves");
    let parts = read_model
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .map(|part| part.content().to_string())
        .collect::<Vec<_>>();
    let first_answer = parts
        .iter()
        .position(|part| part == "trace turn complete")
        .unwrap_or_else(|| panic!("the redriven root answered: {parts:?}"));
    let position = |matches: &dyn Fn(&str) -> bool| parts.iter().position(|part| matches(part));
    for (row, found) in [
        (
            "the active-turn input",
            position(&|part| part.contains("active checkpoint input")),
        ),
        (
            "the ready wake",
            position(&|part| part.ends_with("Wake input:\ntrace-source")),
        ),
    ] {
        assert!(
            found.is_some_and(|index| index < first_answer),
            "the re-executed checkpoint delivered {row} it had bound: {parts:?}"
        );
    }
    assert!(
        position(&|part| part.contains(NEWCOMER)).is_none_or(|index| index > first_answer),
        "the re-executed checkpoint never takes the newcomer: {parts:?}"
    );
}
