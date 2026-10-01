//! FIG-4492: a refused host append over a head its creator has not yet
//! committed leaves nothing of it in the head.
//!
//! A session admitted `Config` has only its created head — the creator's
//! config at revision 0, no committed graph — until its first commit
//! (FIG-4553): whether the commit budget, the protocol or the store's
//! ancestor check refuses the append, the command settles with its refusal
//! over that head, and none of the append's nodes reach it.

use std::sync::Arc;

use lash_core::engine::DriveStop;
use pretty_assertions::assert_eq;

use super::command_budget::{engine_drive, refused_codes, refused_commit_bytes, under_budget};
use crate::conformance::drive_admission::DriveParts;

/// The text every refused append carries.
const REFUSED_NOTE: &str = "a note the refused append carried";

/// A byte budget below any head's bare commit: the measuring drive's
/// settlement is refused too, and its refusal names the bare settlement's
/// bytes.
const MEASURING_BYTES: usize = 64;

/// Queue a host append of `text` on the session's command lane, requiring
/// `ancestor` on the active path when given.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the session store queues the command"
)]
async fn queue_append(parts: &DriveParts, text: &str, ancestor: Option<&str>) -> crate::BatchId {
    parts
        .store
        .enqueue_queued_work(
            crate::QueuedWorkBatchDraft::new(
                parts.session_id.clone(),
                crate::DeliveryPolicy::AfterCurrentTurnCommit,
                crate::SessionCommand::AppendSessionNodes {
                    request: Box::new(crate::AppendSessionNodesRequest {
                        operation_id: "host-append:refused".to_string(),
                        nodes: vec![crate::SessionAppendNode::message(
                            crate::PluginMessage::text(crate::MessageRole::User, text),
                        )],
                        requires_ancestor_node_id: ancestor.map(|id| id.to_string().into()),
                    }),
                },
            )
            .with_source_key("refused-append".to_string()),
        )
        .await
        .expect("queue the host append")
        .batch_id
}

/// The law's session's only durable head is the created head its admission
/// wrote: its creator has not committed a head.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store reads its own head"
)]
async fn assert_uncommitted(parts: &DriveParts) {
    let head = crate::conformance::helpers::load_window_state(&parts.store, &parts.session_id)
        .await
        .expect("read the session's head")
        .expect("the admission wrote the session's created head");
    assert_eq!(
        head.head_revision, 0,
        "the creator has not committed the session's head"
    );
    // The resident graph's synthesized initial frame is not durable content:
    // the created head owns no committed node or checkpoint.
    assert!(
        head.persisted_node_ids.is_empty() && head.checkpoint_ref.is_none(),
        "the created head carries no committed graph"
    );
}

/// Drive the session, and check that the refused append settled and the
/// lane drained, with none of the append's nodes in the head its settlement
/// committed. Answers the command's outcome.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn settle_without_the_note(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &DriveParts,
    command: &crate::BatchId,
    drive: &str,
) -> crate::SessionCommandOutcome {
    let (refusals, stop) = engine_drive(runner, parts, drive)
        .await
        .unwrap_or_else(|looped| panic!("the drive stops: {looped}"));
    assert!(
        refusals.is_empty(),
        "a refused append ends no root refused: {:?}",
        refused_codes(&refusals)
    );
    assert_eq!(stop, DriveStop::Idle, "the lane drains");
    assert!(
        parts
            .store
            .list_open_queued_work(&parts.session_id)
            .await
            .expect("read the session's open work")
            .is_empty(),
        "the refused append settled"
    );
    let completion = parts
        .store
        .queued_work_batch_completion(&parts.session_id, command.as_str())
        .await
        .expect("read the command's settlement")
        .expect("the refused append wrote its settlement");
    let head = crate::conformance::helpers::load_window_state(&parts.store, &parts.session_id)
        .await
        .expect("load the settled head")
        .expect("the settlement committed the creator's head");
    let stranded = head
        .session_graph
        .nodes
        .iter()
        .filter(|node| format!("{:?}", node.payload).contains(REFUSED_NOTE))
        .map(|node| node.node_id.to_string())
        .collect::<Vec<_>>();
    assert!(
        stranded.is_empty(),
        "nothing of the refused append reached the head: {stranded:?}"
    );
    completion
        .command_outcomes
        .get(command)
        .cloned()
        .expect("the settlement carries the command's outcome")
}

/// An append over the commit budget, on a head its creator has not
/// committed, settles failed with the budget refusal, and its nodes stay out
/// of the head (FIG-4492). A drive under a budget below any head measures
/// the bare settlement of a fixed initial-head fixture; both drives clone
/// that fixture so the initial frame's timestamp has the same encoded size
/// (FIG-4621). Under a budget of exactly that size (its refusal
/// receipt is not charged, FIG-4471), the append is refused and its failed
/// settlement commits the creator's head alone.
pub async fn an_over_budget_append_on_an_uncommitted_head_leaves_nothing_of_it(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts =
        DriveParts::new(prefix, "uncommitted-over-budget", &effect_host, &stores, 1).await;
    let mut initial_head = parts.initial_state();
    initial_head.ensure_agent_frame_initialized_with_clock(&crate::testing::TestClock::new(0));
    parts.initial_head = Some(initial_head);
    assert_uncommitted(&parts).await;
    let note = REFUSED_NOTE.repeat(256);
    let command = queue_append(&parts, &note, None).await;

    let measuring = crate::CommitBudget::new(
        crate::CommitBudgetLimit::bounded(MEASURING_BYTES),
        crate::CommitBudgetLimit::Unbounded,
    );
    let (refusals, _) = engine_drive(
        &runner,
        &under_budget(&parts, measuring),
        "uncommitted-over-budget-measure",
    )
    .await
    .unwrap_or_else(|looped| panic!("the measuring drive stops: {looped}"));
    let [(_, bare)] = refusals.as_slice() else {
        panic!(
            "the measuring budget refuses the bare settlement once: {:?}",
            refused_codes(&refusals)
        );
    };
    assert_eq!(
        bare.code,
        crate::RuntimeErrorCode::StoreCommitByteBudgetExceeded
    );
    assert_uncommitted(&parts).await;
    let fitting_bytes = refused_commit_bytes(&bare.message);

    let fitting = crate::CommitBudget::new(
        crate::CommitBudgetLimit::bounded(fitting_bytes),
        crate::CommitBudgetLimit::Unbounded,
    );
    let outcome = settle_without_the_note(
        &runner,
        &under_budget(&parts, fitting),
        &command,
        "uncommitted-over-budget-settle",
    )
    .await;
    let crate::SessionCommandOutcome::Failed { code, message } = outcome else {
        panic!("the over-budget append settles failed: {outcome:?}");
    };
    assert_eq!(code, crate::RuntimeErrorCode::StoreCommitByteBudgetExceeded);
    assert!(
        refused_commit_bytes(&message) > note.len(),
        "the refusal names the append's commit: {message}"
    );
}

/// A protocol session that refuses every appended node.
struct RefusingAppendProtocol;

#[async_trait::async_trait]
impl lash_core::plugin::ProtocolSessionPlugin for RefusingAppendProtocol {
    async fn append_session_nodes(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        _nodes: &[crate::SessionAppendNode],
    ) -> Result<(), crate::SessionError> {
        Err(crate::SessionError::Protocol(
            "the law's protocol refuses every appended node".to_string(),
        ))
    }
}

/// An append the protocol refuses, on a head its creator has not
/// committed, settles failed with the protocol's refusal, and its nodes stay
/// out of the head (FIG-4492).
pub async fn a_protocol_refused_append_on_an_uncommitted_head_leaves_nothing_of_it(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = DriveParts::new(prefix, "uncommitted-protocol", &effect_host, &stores, 1).await;
    parts.protocol = Some(Arc::new(RefusingAppendProtocol));
    assert_uncommitted(&parts).await;
    let command = queue_append(&parts, REFUSED_NOTE, None).await;

    let outcome =
        settle_without_the_note(&runner, &parts, &command, "uncommitted-protocol-settle").await;
    let crate::SessionCommandOutcome::Failed { code, message } = outcome else {
        panic!("the protocol-refused append settles failed: {outcome:?}");
    };
    assert_eq!(code, crate::RuntimeErrorCode::SessionCommandRun);
    assert!(
        message.contains("the law's protocol refuses every appended node"),
        "{message}"
    );
}

/// An append whose required ancestor is not on the active path, on a head
/// its creator has not committed, settles `StaleBranch`, and its nodes stay
/// out of the head (FIG-4492).
pub async fn an_ancestor_refused_append_on_an_uncommitted_head_leaves_nothing_of_it(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    const ABSENT_ANCESTOR: &str = "an-ancestor-the-head-never-had";
    let parts = DriveParts::new(prefix, "uncommitted-ancestor", &effect_host, &stores, 1).await;
    assert_uncommitted(&parts).await;
    let command = queue_append(&parts, REFUSED_NOTE, Some(ABSENT_ANCESTOR)).await;

    let outcome =
        settle_without_the_note(&runner, &parts, &command, "uncommitted-ancestor-settle").await;
    let crate::SessionCommandOutcome::AppendSessionNodes {
        outcome: crate::AppendSessionNodesOutcome::StaleBranch { required_node_id },
    } = outcome
    else {
        panic!("the ancestor-refused append settles stale: {outcome:?}");
    };
    assert_eq!(required_node_id.to_string(), ABSENT_ANCESTOR);
}
