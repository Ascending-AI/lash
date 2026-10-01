//! The committed-final recovery laws (ADR 0099 §5, §8, W7; FIG-4454): a
//! child whose final committed, and whose invocation then died inside its
//! drain and outlived its retention, is recovered by the engine itself and
//! drained on its opener's lane, across a deployment change.
//!
//! Nothing here dispatches the successor by hand. The tier's operator only
//! kills the committing invocation and expires its retention; the opener's
//! redrive reopens the group, and the reopen re-sends every committed child
//! whose seat is still owed. The re-send carries the child's retained
//! identity and replay key, so the successor it mints — whose journal starts
//! empty, whatever invocation id the key mints — drains the final the point
//! retained, through the child's own driver, the one drain authority, and
//! seats it at the rank its commit reserved. Its typed attach-expired
//! refusal never replaces the committed final, and the tool's body never
//! runs again (W15).
//!
//! Between the loss and the reopen, a newer build of another generation
//! becomes the newest deployment, and its session admission refuses the
//! session's marker. The successor still runs on the lane the opener's build
//! serves, so it drains; it never lands on the newer build, where it would be
//! refused and report the final lost.

use pretty_assertions::assert_eq;

use super::commit_boundary::{assert_nothing_landed, commit_group};
use super::*;

/// Where the opener stands when the committing invocation dies.
#[derive(Clone, Copy, Debug)]
enum OpenerAtLoss {
    /// The group is still open; the loss lands after the child's first
    /// declared intent and before its second.
    Open,
    /// The opener closed the group under `Cancel` after the child committed;
    /// the loss lands before any declared intent.
    CancelClosed,
    /// Every declared intent landed, but the invocation crashes before its
    /// settlement is recorded. The tier holds that boundary until expiry.
    BeforeSeat,
}

/// A committed child whose invocation is killed and expired across a
/// deployment change is re-sent by the opener's reopen and drained on its
/// compatible lane: every declared intent lands exactly once, and the rank
/// settles with the committed final.
pub async fn a_committed_final_is_recovered_on_its_lane_across_a_deployment_change(
    fixture: &ToolChildLawFixture,
    prefix: &str,
    expire: &ChildInvocationExpiry,
    change: &DeploymentChange,
) {
    committed_final_recovers(fixture, prefix, expire, change, OpenerAtLoss::Open).await;
}

/// The same recovery after the opener closed the group under `Cancel`: the
/// close protects a committed child (ADR 0099 §4), so the successor its
/// reopen mints is admitted to drain the committed final, not refused.
pub async fn a_cancel_closed_groups_committed_final_is_recovered_on_its_lane(
    fixture: &ToolChildLawFixture,
    prefix: &str,
    expire: &ChildInvocationExpiry,
    change: &DeploymentChange,
) {
    committed_final_recovers(fixture, prefix, expire, change, OpenerAtLoss::CancelClosed).await;
}

/// A crash after the last intent but before seating preserves the committed
/// final and its reserved rank. The tier stops the child at that boundary;
/// expiry and reopening recover it without repeating its body or intents.
pub async fn a_child_crashed_after_its_last_intent_recovers_its_reserved_seat(
    fixture: &ToolChildLawFixture,
    prefix: &str,
    expire: &ChildInvocationExpiry,
    change: &DeploymentChange,
) {
    committed_final_recovers(fixture, prefix, expire, change, OpenerAtLoss::BeforeSeat).await;
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn committed_final_recovers(
    fixture: &ToolChildLawFixture,
    prefix: &str,
    expire: &ChildInvocationExpiry,
    change: &DeploymentChange,
    at_loss: OpenerAtLoss,
) {
    let label = match at_loss {
        OpenerAtLoss::Open => "recovery",
        OpenerAtLoss::CancelClosed => "cancel-recovery",
        OpenerAtLoss::BeforeSeat => "seat-recovery",
    };
    let session_id = crate::SessionId::from(format!("{prefix}-{label}"));
    let turn_id = crate::TurnId::from(format!("{prefix}-{label}-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-{label}-group");
    let call_0 = format!("{group_key}-call-0");

    let host = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await
    .host;
    let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
    let sink = Arc::new(IntentSink::default());
    match at_loss {
        OpenerAtLoss::Open => sink.hold_kind(&call_0, "event"),
        OpenerAtLoss::CancelClosed => sink.hold_all(),
        OpenerAtLoss::BeforeSeat => {}
    }
    let processes: Arc<dyn crate::ProcessService> = Arc::new(GatedProcessService {
        inner: crate::testing::effect_backed_process_service(
            Arc::clone(&scenario.registry),
            Arc::clone(&scenario.process_env_store),
        ),
        sink: Arc::clone(&sink),
    });
    let _guard = register_opener_with_processes(
        &host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        processes,
        Arc::clone(&scenario.process_env_store),
        opener,
        tokio_util::sync::CancellationToken::new(),
    );
    let scoped = host
        .scoped(crate::admit(scope.clone()))
        .expect("the group scope binds");
    let cancellation = recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await;
    let group = || {
        commit_group(
            &scope,
            &session_id,
            &group_key,
            &scenario.env_ref,
            1,
            crate::LoserPolicy::RunToCompletion,
            ToolChildCompletionRouting::Durable,
            cancellation.clone(),
        )
    };
    let handle = scoped
        .controller()
        .open_effect_group(group())
        .await
        .expect("the group opens under the live opener");
    // The child committed its final and is parked inside its drain.
    if !matches!(at_loss, OpenerAtLoss::BeforeSeat) {
        sink.await_blocked(&call_0).await;
    }
    match at_loss {
        OpenerAtLoss::Open => {
            sink.await_landed_len(1).await;
            assert_eq!(
                sink.landed(),
                vec![(call_0.clone(), "start")],
                "the drain landed its first intent and parked at its second"
            );
        }
        OpenerAtLoss::CancelClosed => {
            assert!(
                sink.landed().is_empty(),
                "nothing of the committed final's drain landed before the close"
            );
            scoped
                .controller()
                .close_effect_group(handle, crate::LoserPolicy::Cancel)
                .await
                .expect("the caller closes under Cancel after the child committed");
        }
        OpenerAtLoss::BeforeSeat => {
            sink.await_landed_len(2).await;
            assert_eq!(
                sink.landed(),
                vec![(call_0.clone(), "start"), (call_0.clone(), "event")],
                "every intent landed before the crash at the seat boundary"
            );
        }
    }
    // The committing invocation dies before its seat and outlives its
    // retention; a newer build that refuses the session becomes the newest
    // deployment. Nothing is dispatched for the child.
    expire(group_key.clone(), 0).await;
    change(session_id.clone()).await;
    assert_nothing_landed_beyond(&sink, &call_0, at_loss).await;
    // The opener's redrive reopens the group, and the reopen re-sends the
    // committed child whose seat is owed.
    let mut reopened = scoped
        .controller()
        .open_effect_group(group())
        .await
        .expect("the group reopens");
    sink.release_all();
    sink.await_landed_len(2).await;
    tokio::time::sleep(ABSENCE_BUDGET).await;
    assert_eq!(
        sink.landed(),
        vec![(call_0.clone(), "start"), (call_0.clone(), "event")],
        "the recovered drain realized each declared intent exactly once, in order"
    );
    let settled = next_settlement(&scoped, &mut reopened, 0).await;
    assert_eq!(settled.position, 0, "the child seats at its reserved rank");
    assert!(
        settled.outcome.is_ok(),
        "the committed final seats, not a refusal or a lost report: {:?}",
        settled.outcome.as_ref().err()
    );
    assert_eq!(
        scenario.observation.executions_of("law_commit").len(),
        1,
        "the tool's body ran once: recovery drained the committed final and never re-ran it"
    );
    let disposition = match at_loss {
        OpenerAtLoss::Open | OpenerAtLoss::BeforeSeat => crate::LoserPolicy::RunToCompletion,
        OpenerAtLoss::CancelClosed => crate::LoserPolicy::Cancel,
    };
    scoped
        .controller()
        .close_effect_group(reopened, disposition)
        .await
        .expect("the group closes");
}

/// Nothing lands while the committing invocation is gone and the opener has
/// not reopened: no one but the recovery the reopen starts drains the final.
async fn assert_nothing_landed_beyond(sink: &IntentSink, call_0: &str, at_loss: OpenerAtLoss) {
    match at_loss {
        OpenerAtLoss::Open => {
            tokio::time::sleep(ABSENCE_BUDGET).await;
            assert_eq!(
                sink.landed(),
                vec![(call_0.to_string(), "start")],
                "no intent landed between the loss and the reopen"
            );
        }
        OpenerAtLoss::CancelClosed => {
            assert_nothing_landed(sink, "the loss before the reopen").await;
        }
        OpenerAtLoss::BeforeSeat => {
            tokio::time::sleep(ABSENCE_BUDGET).await;
            assert_eq!(
                sink.landed(),
                vec![(call_0.to_string(), "start"), (call_0.to_string(), "event")],
                "no intent repeated between the loss and the reopen"
            );
        }
    }
}
