//! An aborted direct turn's input is bound to that turn (FIG-3589, ADR 0069
//! §7).
//!
//! A direct turn that aborts with `Err` keeps its drive claim and hands the
//! host its acceptance receipt. The claim is bound to the turn, so it no longer
//! lapses with its lease generation: no drain and no later turn folds the input
//! into its own message block. Only the aborted turn's redrive, which settles
//! the rows with the claim it drove them under, or a cancel of the input
//! consumes it. A worker that crashes never reaches its abort path, so its
//! claim stays recoverable by the next generation
//! (`orphaned_direct_turn_input_is_drivable_by_another_worker`).

use super::direct_turn_acceptance::{
    Journal, abort_before_commit_plugin, acceptance_runtime, applications, direct_input,
    enqueue_next_turn, pending_input_ids, recording_provider,
};
use crate::admit;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use pretty_assertions::assert_eq;
use std::sync::Arc;

/// The session the direct-turn acceptance harness drives.
const SESSION_ID: &str = "root";

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn statuses(
    store: &Arc<dyn crate::RuntimePersistence>,
) -> Vec<(crate::InputId, crate::PendingTurnInputReadStatus)> {
    store
        .list_pending_turn_inputs(&SessionId::from(SESSION_ID))
        .await
        .expect("read pending inputs")
        .into_iter()
        .map(|read| (read.input.input_id, read.status))
        .collect()
}

fn bound_to(
    turn_id: &TurnId,
    receipt_input_id: &crate::InputId,
) -> crate::PendingTurnInputReadStatus {
    crate::PendingTurnInputReadStatus::TurnBound {
        turn_id: turn_id.clone(),
        receipt_input_id: receipt_input_id.clone(),
    }
}

/// Abort `turn_id` with `Err` after its drive and before its commit, and
/// return the input its acceptance receipt names.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn abort_direct_turn(
    journal: &Journal,
    store: &Arc<dyn crate::RuntimePersistence>,
    provider: crate::ProviderHandle,
    turn_id: &TurnId,
    text: &str,
) -> crate::InputId {
    let aborted = journal
        .run_with_plugins(
            store,
            provider,
            vec![abort_before_commit_plugin()],
            turn_id,
            text,
        )
        .await
        .expect_err("the direct turn aborts before its commit");
    aborted
        .turn_input_acceptance
        .as_deref()
        .expect("an aborted direct turn carries its acceptance receipt")
        .input_id
        .clone()
}

/// Run one queued-work drain as an unrelated worker.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn drain_as_another_worker(
    prefix: &str,
    store: &Arc<dyn crate::RuntimePersistence>,
    journal: &Journal,
    provider: crate::ProviderHandle,
) -> crate::QueuedTurnDrain<crate::AssembledTurn> {
    let mut drainer = acceptance_runtime(
        store,
        &journal.effect_host,
        provider,
        Vec::new(),
        crate::LeaseOwnerIdentity::opaque(
            format!("{prefix}-drain-owner"),
            format!("{prefix}-drain-incarnation"),
        ),
    )
    .await;
    let drain_id = format!("{prefix}-drain");
    let scope = journal
        .effect_host
        .scoped(admit(crate::ExecutionScope::queue_drain(
            SESSION_ID, &drain_id,
        )))
        .expect("scope the drain");
    drainer
        .stream_next_queued_work(crate::TurnOptions::new(
            tokio_util::sync::CancellationToken::new(),
            scope,
        ))
        .await
        .expect("the drain runs")
}

/// A drain never answers an aborted turn's input; the turn's redrive replays
/// its journaled drive and answers it once, under its own turn id.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn aborted_direct_turn_input_is_bound_until_its_redrive(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = TurnId::from(format!("{prefix}-bound-until-redrive"));
    let journal = Journal::new();
    let (provider, requests) = recording_provider("answered by the redrive");
    let accepted =
        abort_direct_turn(&journal, &store, provider.clone(), &turn_id, "bound words").await;
    assert_eq!(
        statuses(&store).await,
        vec![(accepted.clone(), bound_to(&turn_id, &accepted))]
    );

    let drain = drain_as_another_worker(prefix, &store, &journal, provider.clone()).await;
    assert!(
        matches!(drain, crate::QueuedTurnDrain::Empty(_)),
        "a drain under a new lease generation must not answer a bound input"
    );
    assert_eq!(
        statuses(&store).await,
        vec![(accepted.clone(), bound_to(&turn_id, &accepted))]
    );

    let redriven = journal
        .run(&store, provider, &turn_id, "bound words")
        .await
        .expect("the redrive commits the aborted turn");
    assert!(
        matches!(redriven.outcome, crate::TurnOutcome::Finished(_)),
        "{:?}",
        redriven.outcome
    );
    let requests = requests.lock().expect("request lock").clone();
    assert_eq!(requests.len(), 1, "only the redrive reached the provider");
    assert_eq!(
        requests[0].matches("bound words").count(),
        1,
        "{requests:?}"
    );
    let applied = applications(&store).await;
    assert_eq!(applied.len(), 1, "{applied:?}");
    assert_eq!(applied[0].input_id, accepted);
    assert_eq!(applied[0].turn_id, turn_id);
    assert!(pending_input_ids(&store).await.is_empty());
}

/// A later direct turn drives only its own input and leaves the aborted turn's
/// input bound; the host consumes it with a cancel by the receipt.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn later_direct_turn_never_folds_in_a_bound_input(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let aborted_turn = TurnId::from(format!("{prefix}-aborted"));
    let later_turn = TurnId::from(format!("{prefix}-later"));
    let journal = Journal::new();
    let (provider, requests) = recording_provider("answered the later turn");
    let accepted = abort_direct_turn(
        &journal,
        &store,
        provider.clone(),
        &aborted_turn,
        "aborted words",
    )
    .await;

    let later = journal
        .run(&store, provider, &later_turn, "later words")
        .await
        .expect("the later direct turn commits");
    assert!(
        matches!(later.outcome, crate::TurnOutcome::Finished(_)),
        "{:?}",
        later.outcome
    );
    let requests = requests.lock().expect("request lock").clone();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("later words"), "{requests:?}");
    assert!(
        !requests[0].contains("aborted words"),
        "a later turn must not fold in the aborted turn's input: {requests:?}"
    );
    assert!(
        applications(&store)
            .await
            .iter()
            .all(|application| application.input_id != accepted),
        "the later turn settles only its own input"
    );
    assert_eq!(
        statuses(&store).await,
        vec![(accepted.clone(), bound_to(&aborted_turn, &accepted))]
    );

    let cancelled = store
        .cancel_pending_turn_input(&SessionId::from(SESSION_ID), &accepted)
        .await
        .expect("the host cancels the bound input by its receipt");
    assert!(cancelled.is_cancelled(), "{cancelled:?}");
    assert!(pending_input_ids(&store).await.is_empty());
}

/// A cancel of the bound input returns the earlier admissions the aborted
/// turn had absorbed into its drive to the queue, and the next drain answers
/// them without the cancelled input.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn cancelling_a_bound_input_returns_its_drive_to_the_queue(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = TurnId::from(format!("{prefix}-absorbing-aborted"));
    let earlier = enqueue_next_turn(&store, "earlier words").await;
    let journal = Journal::new();
    let (provider, requests) = recording_provider("answered the earlier input");
    let accepted = abort_direct_turn(
        &journal,
        &store,
        provider.clone(),
        &turn_id,
        "aborted words",
    )
    .await;
    assert_eq!(
        statuses(&store).await,
        vec![
            (earlier.input_id.clone(), bound_to(&turn_id, &accepted)),
            (accepted.clone(), bound_to(&turn_id, &accepted)),
        ],
        "the aborted turn's whole drive is bound to it"
    );

    // Cancelling the absorbed row alone would change the drive the aborted
    // turn's journal replays: refused, naming the turn and the receipt.
    let refused = store
        .cancel_pending_turn_input(&SessionId::from(SESSION_ID), &earlier.input_id)
        .await
        .expect("a cancel of a bound row other than the receipt's");
    assert!(
        matches!(
            &refused,
            crate::PendingTurnInputCancelOutcome::TurnBound {
                turn_id: refused_turn,
                receipt_input_id,
                ..
            } if *refused_turn == turn_id && *receipt_input_id == accepted
        ),
        "a cancel of a bound row other than the receipt's is refused: {refused:?}"
    );
    assert_eq!(
        statuses(&store).await,
        vec![
            (earlier.input_id.clone(), bound_to(&turn_id, &accepted)),
            (accepted.clone(), bound_to(&turn_id, &accepted)),
        ],
        "the refused cancel changed nothing"
    );

    let cancelled = store
        .cancel_pending_turn_input(&SessionId::from(SESSION_ID), &accepted)
        .await
        .expect("the host cancels the bound input by its receipt");
    assert!(cancelled.is_cancelled(), "{cancelled:?}");
    assert_eq!(
        statuses(&store).await,
        vec![(
            earlier.input_id.clone(),
            crate::PendingTurnInputReadStatus::Pending
        )],
        "the absorbed admission is back in the queue, unbound"
    );

    let drain = drain_as_another_worker(prefix, &store, &journal, provider).await;
    assert!(
        matches!(drain, crate::QueuedTurnDrain::Ran(_)),
        "the drain answers the released admission"
    );
    let requests = requests.lock().expect("request lock").clone();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("earlier words"), "{requests:?}");
    assert!(!requests[0].contains("aborted words"), "{requests:?}");
    assert!(pending_input_ids(&store).await.is_empty());
}

/// On an effect host that journals nothing, the aborted turn's redrive runs its
/// drive a second time; it re-takes the rows bound to it and commits once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn journal_less_redrive_retakes_its_bound_drive(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = TurnId::from(format!("{prefix}-journal-less-redrive"));
    let effect_host: Arc<dyn crate::EffectHost> = Arc::new(crate::NativeEffectHost::default());
    let (provider, requests) = recording_provider("answered by the redrive");
    let run = |plugins: Vec<Arc<dyn crate::facade_support::PluginFactory>>| {
        let store = Arc::clone(&store);
        let effect_host = Arc::clone(&effect_host);
        let provider = provider.clone();
        let turn_id = turn_id.clone();
        async move {
            let mut runtime = acceptance_runtime(
                &store,
                &effect_host,
                provider,
                plugins,
                crate::testing::runtime_lease_owner(),
            )
            .await;
            let scope = effect_host
                .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, &turn_id)))
                .expect("scope the direct turn");
            runtime
                .stream_turn(
                    direct_input(&turn_id, "unjournaled words"),
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                )
                .await
        }
    };
    run(vec![abort_before_commit_plugin()])
        .await
        .expect_err("the first execution aborts before its commit");
    let accepted = pending_input_ids(&store)
        .await
        .into_iter()
        .next()
        .expect("the aborted turn's input is open");
    assert_eq!(
        statuses(&store).await,
        vec![(accepted.clone(), bound_to(&turn_id, &accepted))]
    );

    let redriven = run(Vec::new())
        .await
        .expect("the redrive re-takes its bound drive and commits");
    assert!(
        matches!(redriven.outcome, crate::TurnOutcome::Finished(_)),
        "{:?}",
        redriven.outcome
    );
    let requests = requests.lock().expect("request lock").clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].matches("unjournaled words").count(),
        1,
        "{requests:?}"
    );
    let applied = applications(&store).await;
    assert_eq!(applied.len(), 1, "{applied:?}");
    assert_eq!(applied[0].input_id, accepted);
    assert_eq!(applied[0].turn_id, turn_id);
    assert!(pending_input_ids(&store).await.is_empty());
}

/// The drive's body claimed the accepted row and the worker then lost the
/// drive's outcome, so the turn aborts without ever holding its drive claim.
/// The abort binds the claim the accepted row carries under the turn's lease
/// generation; no drain answers the input, and the redrive, which has no
/// journaled drive to replay, re-takes the bound rows and commits once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn lost_drive_outcome_still_binds_its_claimed_input(
    prefix: &str,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let turn_id = TurnId::from(format!("{prefix}-lost-drive-outcome"));
    let journal = Journal::new();
    journal
        .controller
        .lose_outcome_at_next(crate::RuntimeEffectKind::ClaimAcceptedTurnInput);
    let (provider, requests) = recording_provider("answered by the redrive");
    let aborted = journal
        .run(&store, provider.clone(), &turn_id, "lost drive words")
        .await
        .expect_err("the lost drive outcome aborts the turn");
    let accepted = aborted
        .turn_input_acceptance
        .as_deref()
        .expect("the aborted turn carries its acceptance receipt")
        .input_id
        .clone();
    assert!(
        journal.controller.journaled_drive().is_none(),
        "the drive's outcome was never journaled"
    );
    assert_eq!(
        statuses(&store).await,
        vec![(accepted.clone(), bound_to(&turn_id, &accepted))],
        "the rows the lost drive claimed are bound to the aborted turn"
    );

    let drain = drain_as_another_worker(prefix, &store, &journal, provider.clone()).await;
    assert!(
        matches!(drain, crate::QueuedTurnDrain::Empty(_)),
        "a drain must not answer the lost drive's input"
    );

    let redriven = journal
        .run(&store, provider, &turn_id, "lost drive words")
        .await
        .expect("the redrive re-takes its bound rows and commits");
    assert!(
        matches!(redriven.outcome, crate::TurnOutcome::Finished(_)),
        "{:?}",
        redriven.outcome
    );
    let requests = requests.lock().expect("request lock").clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].matches("lost drive words").count(),
        1,
        "{requests:?}"
    );
    let applied = applications(&store).await;
    assert_eq!(applied.len(), 1, "{applied:?}");
    assert_eq!(applied[0].input_id, accepted);
    assert_eq!(applied[0].turn_id, turn_id);
    assert!(pending_input_ids(&store).await.is_empty());
}
