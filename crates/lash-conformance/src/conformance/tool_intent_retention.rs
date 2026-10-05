//! FIG-1509 / ADR 0067 / ADR 0023: the host tool-intent submission ledger is
//! reclaimed by the retained-evidence lever only after its owner session is
//! durably deleted, and a reclaimed owner cannot submit again.

use super::session_store_factory::session_store_request;
use super::*;
use std::future::Future;
use std::pin::Pin;

/// One open of the stores a tool-intent retention law runs over.
pub struct ToolIntentRetentionHandles {
    /// The process registry that holds the submission ledger.
    pub registry: Arc<dyn ProcessRegistry>,
    /// The catalog whose deleted-session frontier proves owner death and
    /// whose retained-evidence lever reclaims the ledger.
    pub sessions: Arc<dyn crate::DeploymentStore>,
}

/// Fresh handles on the databases an earlier open wrote: a close and
/// reopen on a file substrate, new handles on the same memory or server
/// database otherwise.
pub type ToolIntentRetentionReopen =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ToolIntentRetentionHandles> + Send>> + Send + Sync>;

/// A fresh backend for one tool-intent retention law.
pub struct ToolIntentRetentionFixture {
    pub open: ToolIntentRetentionHandles,
    pub reopen: ToolIntentRetentionReopen,
}

const LIVE_SESSION: &str = "tool-intent-retention-live";
const DEAD_SESSION: &str = "tool-intent-retention-dead";
const SCOPE: &str = "tool-intent-retention-scope";

fn identity(session: &'static str, call: &'static str) -> crate::ToolIntentIdentity {
    crate::derive_tool_intent_identity(
        &crate::RuntimeOwner::Session(SessionId::from(session)),
        SCOPE,
        &lash_core::ToolCallId::fixture(call),
        0,
    )
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn submission(identity: &crate::ToolIntentIdentity) -> crate::ToolIntentSubmissionRecord {
    let intent = crate::ToolIntent::EmitTrigger(lash_core::EmitTriggerIntent {
        owner: identity.owner.clone(),
        request: crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            "tool-intent-retention-source",
            serde_json::json!({ "call": identity.tool_call_id.as_str() }),
            "tool-intent-retention-occurrence",
        ),
    });
    crate::ToolIntentSubmissionRecord::new(identity.clone(), intent)
        .expect("the submission has a payload hash")
}

fn refused(identity: &crate::ToolIntentIdentity) -> crate::ToolIntentExecutionOutcome {
    crate::ToolIntentExecutionOutcome::Refused {
        identity: Some(identity.clone()),
        intent_index: identity.intent_index,
        kind: crate::ToolIntentKind::EmitTrigger,
        refusal: crate::ToolIntentRefusalReason::ExecutionEnvMissing,
    }
}

fn bound(committed_before_epoch_ms: u64) -> crate::RetentionBound {
    crate::RetentionBound {
        committed_before_epoch_ms,
        turn_watermark: crate::TurnProjectionWatermark::NoProjector,
    }
}

/// How a resubmission of one identity is answered, without the record.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    Admitted,
    Existing { outcome_recorded: bool },
    Reclaimed,
}

/// The ledger's answer to a resubmission of `identity`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn readmit(
    handles: &ToolIntentRetentionHandles,
    identity: &crate::ToolIntentIdentity,
) -> Answer {
    match handles
        .registry
        .admit_tool_intent_submission(submission(identity))
        .await
        .expect("a resubmission reaches the ledger")
    {
        crate::ToolIntentSubmissionAdmission::Admitted => Answer::Admitted,
        crate::ToolIntentSubmissionAdmission::Existing(existing) => Answer::Existing {
            outcome_recorded: existing.execution_outcome().is_some(),
        },
        crate::ToolIntentSubmissionAdmission::Reclaimed => Answer::Reclaimed,
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn reclaim(handles: &ToolIntentRetentionHandles, committed_before_epoch_ms: u64) -> usize {
    handles
        .sessions
        .reclaim_retained_evidence(bound(committed_before_epoch_ms))
        .await
        .expect("the retained-evidence sweep completes")
        .removed_tool_intent_submission_count
}

/// L13 for the host submission ledger (FIG-1509): reclaim is refused while
/// the owner session lives or the row is inside the host's bound, takes a
/// durably deleted owner's rows past the bound, and leaves a fence that no
/// crash or reopen on either side of the reclaim, and no late completion,
/// can resurrect.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn tool_intent_submissions_reclaim_only_after_owner_death<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ToolIntentRetentionFixture>,
{
    let ToolIntentRetentionFixture { open, reopen } = make().await;
    for session in [LIVE_SESSION, DEAD_SESSION] {
        let request = session_store_request(
            &SessionId::from(session),
            "tool-intent-retention-model",
            crate::SessionRelation::Root,
        );
        open.sessions
            .admit_view(&request)
            .await
            .expect("materialize the ledger's owner session");
    }
    let live = identity(LIVE_SESSION, "live-call");
    let dead_completed = identity(DEAD_SESSION, "dead-completed-call");
    let dead_pending = identity(DEAD_SESSION, "dead-pending-call");
    for identity in [&live, &dead_completed, &dead_pending] {
        assert_eq!(
            readmit(&open, identity).await,
            Answer::Admitted,
            "the first submission claims its identity"
        );
    }
    for identity in [&live, &dead_completed] {
        open.registry
            .complete_tool_intent_submission(&identity.replay_key, refused(identity))
            .await
            .expect("retain the first outcome");
    }

    // A live owner's rows are its replay fence, whatever their age.
    assert_eq!(
        reclaim(&open, u64::MAX).await,
        0,
        "no row is reclaimed while its owner session lives"
    );
    open.sessions
        .delete_session(&SessionId::from(DEAD_SESSION))
        .await
        .expect("delete the ledger's owner session");
    // Owner death alone is not enough: the host's bound still holds the rows.
    assert_eq!(
        reclaim(&open, 0).await,
        0,
        "a row admitted at or after the bound survives its owner"
    );
    assert_eq!(
        readmit(&open, &dead_completed).await,
        Answer::Existing {
            outcome_recorded: true
        },
        "a row inside the bound still answers its first outcome"
    );

    // Crash before the reclaim: the reopened ledger still holds every row.
    let before = reopen().await;
    assert_eq!(
        readmit(&before, &dead_pending).await,
        Answer::Existing {
            outcome_recorded: false
        },
        "an unreclaimed pending row survives a reopen"
    );
    assert_eq!(
        reclaim(&before, u64::MAX).await,
        2,
        "past the bound the deleted owner's completed and pending rows go"
    );
    assert_eq!(
        readmit(&before, &live).await,
        Answer::Existing {
            outcome_recorded: true
        },
        "the live owner's fence survives the sweep that reclaimed its neighbour"
    );
    for (identity, what) in [
        (&dead_completed, "a completed identity"),
        (&dead_pending, "a pending identity"),
        (
            &identity(DEAD_SESSION, "dead-fresh-call"),
            "a never-seen identity",
        ),
    ] {
        assert_eq!(
            readmit(&before, identity).await,
            Answer::Reclaimed,
            "{what} of a reclaimed owner is refused, never admitted again"
        );
    }
    // A realization that was in flight across the reclaim cannot put its row
    // back by completing it.
    assert!(
        before
            .registry
            .complete_tool_intent_submission(&dead_pending.replay_key, refused(&dead_pending))
            .await
            .is_err(),
        "completing a reclaimed row fails"
    );
    assert_eq!(
        readmit(&before, &dead_pending).await,
        Answer::Reclaimed,
        "a late completion does not resurrect a reclaimed row"
    );

    // Crash after the reclaim: the fence is durable and the sweep is spent.
    let after = reopen().await;
    assert_eq!(
        readmit(&after, &dead_completed).await,
        Answer::Reclaimed,
        "the owner's fence survives a reopen"
    );
    assert_eq!(
        readmit(&after, &live).await,
        Answer::Existing {
            outcome_recorded: true
        },
        "the live owner's row survives a reopen"
    );
    assert_eq!(
        reclaim(&after, u64::MAX).await,
        0,
        "a repeated sweep finds nothing left to reclaim"
    );
}
