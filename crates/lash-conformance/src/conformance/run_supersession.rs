//! FIG-4200: who ends a superseded run, and which moved heads still park.
//!
//! - A refused execution ends its run only while it still owns it. A run whose
//!   commit met a head another writer moved, and whose fence a later
//!   admission superseded before it wrote the end, leaves the run to that
//!   admission's execution: the store checks the fence in the ending
//!   transaction, so an obsolete executor never ends its successor's run.
//! - A run's recorded head inspection decides a moved head by its
//!   components. A higher revision is ordinary overtaking and ends typed; a
//!   head that is inconsistent with the admission's base (a lower revision,
//!   or the same revision with another leaf or checkpoint) still parks for an
//!   operator, with nothing ended.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lash_core::engine::{RunOutcome, ShiftAbort, ShiftOutcome};
use lash_core::store::{RunTerminalCause, SessionHeadRef};
use lash_sansio::TurnId;
use pretty_assertions::assert_eq;

use super::shift_admission::{ShiftParts, admitted, on_tier};

/// A shift of the session through its shift loop, answered as it ended.
async fn shift(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &ShiftParts,
    id: &str,
) -> Result<ShiftOutcome, ShiftAbort> {
    let request = parts.request(id);
    on_tier(runner, parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(
            async move { lash_core::shift::work_session(&mut runtime, &scope, &request).await },
        )
    })
    .await
}

/// The run `input` was admitted to.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn run_of(parts: &ShiftParts, input: &crate::InputId) -> TurnId {
    parts
        .store
        .run_of_input(&parts.session_id, input)
        .await
        .expect("read the input's run")
        .expect("the input was admitted to a run")
}

/// Whether `input` is still open, admitted to `run`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn held_by(parts: &ShiftParts, input: &crate::InputId, run: &TurnId) -> bool {
    parts
        .store
        .list_pending_turn_inputs(&parts.session_id)
        .await
        .expect("read pending input")
        .into_iter()
        .any(|read| {
            read.input.input_id == *input
                && matches!(
                    read.status,
                    crate::PendingTurnInputReadStatus::Admitted { run: ref holder }
                        if holder == run
                )
        })
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn terminal(parts: &ShiftParts, run: &TurnId) -> Option<lash_core::store::RunTerminal> {
    parts
        .store
        .run_terminal(&parts.session_id, run)
        .await
        .expect("read the run's terminal")
}

fn superseded_refusal(terminal: Option<&lash_core::store::RunTerminal>) -> bool {
    terminal.is_some_and(|terminal| {
        matches!(
            &terminal.cause,
            RunTerminalCause::Refused { code, .. }
                if *code == crate::RuntimeErrorCode::StoreCommitSuperseded
        )
    })
}

/// A run whose commit meets a head another writer moved is refused
/// `StoreCommitSuperseded`, but a successor admission seals a newer shift
/// epoch before the run writes the end. The obsolete run writes nothing: the
/// run stays unfinished with its input admitted to it. The successor's
/// shift resumes the run, finds the head overtaken, and ends it typed under
/// its own fence; the next input then admits a new run.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_obsolete_executor_never_ends_its_successors_run(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "obsolete-executor", &effect_host, &stores, 8).await;
    let recording = Arc::new(
        lash_core::testing::runtime_helpers::RecordingStore::over_session(
            Arc::clone(&parts.store),
            parts.session_id.clone(),
        ),
    );
    parts.store = Arc::clone(&recording) as Arc<dyn crate::RuntimeStore>;
    // The first model call moves the head through another writer, which
    // commits the session under the law's own policy; every later call
    // answers.
    let policy = parts.initial_state().policy;
    let calls = Arc::new(AtomicUsize::new(0));
    let overtaken = Arc::new(AtomicBool::new(false));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let recording = Arc::clone(&recording);
            let calls = Arc::clone(&calls);
            let overtaken = Arc::clone(&overtaken);
            move |_request| {
                let recording = Arc::clone(&recording);
                let policy = policy.clone();
                let index = calls.fetch_add(1, Ordering::SeqCst);
                let overtake = !overtaken.swap(true, Ordering::SeqCst);
                async move {
                    if overtake {
                        // Another writer, outside every shift: the lane-less
                        // commit races the bound run's first commit, which
                        // the created head still admits (FIG-4202).
                        lash_core::testing::runtime_helpers::advance_session_head_unfenced(
                            &recording,
                            |state| state.policy = policy,
                        )
                        .await;
                    }
                    Ok(crate::LlmResponse {
                        parts: vec![crate::LlmOutputPart::Text {
                            text: format!("answer {}", index + 1),
                            response_meta: None,
                        }],
                        ..crate::LlmResponse::default()
                    })
                }
            }
        })
        .build();
    parts.host.providers.models = crate::testing::standard_test_llm_profiles(model.into_handle());
    // Between the obsolete run meeting its refusal and writing its end, a
    // successor admission seals a newer shift epoch.
    let successor_sealed = Arc::new(AtomicBool::new(false));
    recording.before_next_end_refused_run(Arc::new({
        let store = Arc::clone(&parts.store);
        let session_id = parts.session_id.clone();
        let successor_sealed = Arc::clone(&successor_sealed);
        move || {
            let store = Arc::clone(&store);
            let session_id = session_id.clone();
            let successor_sealed = Arc::clone(&successor_sealed);
            Box::pin(async move {
                let stored = store
                    .shift_epoch(&session_id)
                    .await
                    .expect("read the shift epoch");
                let sealed = store
                    .seal_shift_epoch(
                        &session_id,
                        &lash_core::store::AdmissionId::new(format!("{session_id}-successor")),
                        stored.epoch,
                        &lash_core::store::RunStartNonce::new(format!(
                            "{session_id}-successor-start"
                        )),
                        None,
                    )
                    .await
                    .expect("the successor seals");
                assert!(
                    matches!(sealed, lash_core::store::ShiftEpochSeal::Sealed(_)),
                    "{sealed:?}"
                );
                successor_sealed.store(true, Ordering::SeqCst);
            })
        }
    }));

    let input = parts.enqueue("ask", None).await;
    let request = parts.request("obsolete-executor-shift");
    let refused = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            let admitted = admitted(
                lash_core::shift::admit_shift(&mut runtime, &scope, &request, 0, None)
                    .await
                    .expect("admit the run"),
            );
            lash_core::shift::execute_admitted_run(&mut runtime, &scope, admitted).await
        })
    })
    .await;
    assert!(
        successor_sealed.load(Ordering::SeqCst),
        "the successor sealed"
    );
    match refused {
        Err(ShiftAbort::Refused(error)) => assert_eq!(
            error.code,
            crate::RuntimeErrorCode::StoreCommitSuperseded,
            "{error:?}"
        ),
        other => panic!("the obsolete run's commit is refused superseded: {other:?}"),
    }
    let run = run_of(&parts, &input).await;
    assert_eq!(
        terminal(&parts, &run).await,
        None,
        "the obsolete execution never ends its successor's run"
    );
    assert_eq!(
        parts
            .store
            .unfinished_run(&parts.session_id)
            .await
            .expect("read the unfinished run")
            .map(|unfinished| unfinished.run),
        Some(run.clone()),
        "the run still holds the session"
    );
    assert!(
        held_by(&parts, &input, &run).await,
        "the input stays admitted to the run"
    );

    // The successor's execution owns the run: it resumes it, finds the head
    // overtaken, and ends it typed under its own fence.
    let successor = shift(&runner, &parts, "obsolete-executor-successor").await;
    match successor {
        Err(ShiftAbort::Refused(error)) => assert_eq!(
            error.code,
            crate::RuntimeErrorCode::StoreCommitSuperseded,
            "{error:?}"
        ),
        other => panic!("the successor ends the overtaken run typed: {other:?}"),
    }
    assert!(
        superseded_refusal(terminal(&parts, &run).await.as_ref()),
        "the successor ends the run: {:?}",
        terminal(&parts, &run).await
    );
    assert_eq!(
        parts
            .store
            .load_turn_park(&parts.session_id)
            .await
            .expect("read the park"),
        None
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the successor made no model call"
    );

    let next = parts.enqueue("ask again", None).await;
    let outcome = shift(&runner, &parts, "obsolete-executor-next")
        .await
        .expect("the next shift runs");
    let next_run = run_of(&parts, &next).await;
    assert_ne!(next_run, run, "the next input heads a new run");
    assert!(
        outcome
            .ran
            .iter()
            .any(|ran| matches!(ran, RunOutcome::Committed { run, .. } if *run == next_run)),
        "{outcome:?}"
    );
}

/// How a law's recorded admission base is inconsistent with the live head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InconsistentHead {
    /// The live head's revision is lower than the base's.
    LowerRevision,
    /// The same revision, with another leaf.
    OtherLeaf,
    /// The same revision, with another checkpoint.
    OtherCheckpoint,
}

/// A run admitted on a base the live head is inconsistent with, `head`,
/// parks when a shift on a fresh journal inspects it: the verdict is
/// `Diverged`, the park is an `EffectReplayDivergence`, nothing ends the
/// run, its input stays admitted to it, and no model call runs.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn inconsistent_divergence_still_parks(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    head: InconsistentHead,
) {
    let parts = ShiftParts::new(
        prefix,
        &format!("inconsistent-{head:?}").to_lowercase(),
        &effect_host,
        &stores,
        8,
    )
    .await;
    let run = TurnId::fixture(format!("inconsistent-{head:?}-run").to_lowercase());
    let input = parts.enqueue("ask", Some(run.as_str())).await;
    // An earlier execution recorded the run's admission on `base`.
    let fence = lash_core::testing::store_fixtures::seal_shift_fence_for_test(
        &parts.store,
        &parts.session_id,
        "inconsistent-first-execution",
    )
    .await;
    // The head a shift reads live: the store's, or the initial state's for a
    // session that committed nothing yet.
    let state = parts.initial_state();
    let live = match parts
        .store
        .load_session_head_meta(&parts.session_id)
        .await
        .expect("read the session head")
    {
        Some(head) => SessionHeadRef {
            generation: 0,
            revision: head.head_revision,
            leaf: head.leaf_node_id,
            checkpoint: head.checkpoint_ref,
        },
        None => SessionHeadRef {
            generation: 0,
            revision: state.head_revision,
            leaf: state.session_graph.leaf_node_id.clone(),
            checkpoint: state.checkpoint_ref.clone(),
        },
    };
    let base = match head {
        InconsistentHead::LowerRevision => SessionHeadRef {
            revision: live.revision + 1,
            ..live.clone()
        },
        InconsistentHead::OtherLeaf => SessionHeadRef {
            leaf: Some(crate::NodeId::from("inconsistent-leaf")),
            ..live.clone()
        },
        InconsistentHead::OtherCheckpoint => SessionHeadRef {
            checkpoint: Some(lash_core::store::BlobRef(
                "inconsistent-checkpoint".to_string(),
            )),
            ..live.clone()
        },
    };
    parts
        .store
        .admit_run(&lash_core::store::AdmitRunRequest {
            fence,
            run: run.clone(),
            head: lash_core::store::AdmittedHead::Input(input.clone()),
            max_inputs: 1,
            policy: lash_core::testing::queued_work_admission_policy(1),
            base,
            turn_index: state.turn_index as u64 + 1,
            generation: None,
            admitted_generation: lash_core::engine::BuildGeneration::for_test("conformance-law"),
            executor: lash_core::store::RunExecutor::Run,
            plugins: Default::default(),
            turn_cancellation: None,
            trace_scopes: std::sync::Arc::new(lash_core::UntracedScopes),
        })
        .await
        .expect("record the run's admission")
        .expect("the run's admission reaches its head");

    let parked = shift(&runner, &parts, "inconsistent-shift").await;
    match parked {
        Err(ShiftAbort::Parked { run: parked, error }) => {
            assert_eq!(parked, run);
            assert_eq!(
                error.code,
                crate::RuntimeErrorCode::EffectReplayDivergence,
                "{error:?}"
            );
        }
        other => panic!("an inconsistent head parks the run: {other:?}"),
    }
    let park = parts
        .store
        .load_turn_park(&parts.session_id)
        .await
        .expect("read the park")
        .expect("the run parked");
    assert_eq!(park.turn_id, run);
    assert!(
        matches!(
            park.reason,
            lash_core::store::ParkReason::EffectReplayDivergence { .. }
        ),
        "{park:?}"
    );
    assert_eq!(terminal(&parts, &run).await, None, "nothing ends the run");
    assert!(
        held_by(&parts, &input, &run).await,
        "the input stays admitted to the parked run"
    );
    assert_eq!(parts.calls(), 0, "the parked run made no model call");
    let blocked = shift(&runner, &parts, "inconsistent-blocked")
        .await
        .expect("the shift stops");
    assert!(
        matches!(blocked.stop, lash_core::engine::ShiftStop::Parked(_)),
        "{blocked:?}"
    );
}

/// [`inconsistent_divergence_still_parks`] on a live head below the base.
pub async fn inconsistent_divergence_still_parks_on_a_lower_revision(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    inconsistent_divergence_still_parks(
        prefix,
        effect_host,
        stores,
        runner,
        InconsistentHead::LowerRevision,
    )
    .await;
}

/// [`inconsistent_divergence_still_parks`] on the base's revision with
/// another leaf.
pub async fn inconsistent_divergence_still_parks_on_another_leaf(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    inconsistent_divergence_still_parks(
        prefix,
        effect_host,
        stores,
        runner,
        InconsistentHead::OtherLeaf,
    )
    .await;
}

/// [`inconsistent_divergence_still_parks`] on the base's revision with
/// another checkpoint.
pub async fn inconsistent_divergence_still_parks_on_another_checkpoint(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    inconsistent_divergence_still_parks(
        prefix,
        effect_host,
        stores,
        runner,
        InconsistentHead::OtherCheckpoint,
    )
    .await;
}
