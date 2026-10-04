//! FIG-4200: only the current owner ends a superseded run.
//!
//! - A refused execution ends its run only while it still owns it. A run whose
//!   commit met a head another writer moved, and whose fence a later
//!   admission superseded before it wrote the end, leaves the run to that
//!   admission's execution: the store checks the fence in the ending
//!   transaction, so an obsolete executor never ends its successor's run.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lash_core::engine::{RunOutcome, ShiftAbort, ShiftOutcome};
use lash_core::store::RunTerminalCause;
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
