//! Shared foreground-turn control conformance.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::conformance::durable_turn_address;
use crate::{
    AwaitEventWaitIdentity, EffectHost, ExecutionScope, Resolution, TurnAddress, TurnCancelMode,
    TurnCancelOutcome, TurnCancelRequest, TurnCancellationEvidence, TurnFinish, TurnOutcome,
    TurnStop, TurnTerminal, TurnWorkDriver,
};
use lash_core::testing::conformance_support::{ActiveTurnControl, TurnCancelPeekIdentity};
use pretty_assertions::assert_eq;

fn address(label: &str) -> TurnAddress {
    durable_turn_address(
        format!("turn-control-{label}-{}", uuid::Uuid::new_v4()),
        "turn-a",
    )
}

fn request(address: TurnAddress, request_id: &str) -> TurnCancelRequest {
    TurnCancelRequest::new(address, request_id, Some("conformance-user".to_string()))
        .with_reason("stop button")
}

async fn driver_for_session(host: Arc<dyn EffectHost>, address: &TurnAddress) -> TurnWorkDriver {
    let store = Arc::new(crate::InMemorySessionStore::new()) as Arc<dyn crate::RuntimePersistence>;
    super::bind_conformance_session(&store, &address.session_id).await;
    TurnWorkDriver::for_session(host, address.session_id.clone(), store)
}

/// Run the exact-address, replay, terminal, sweep, and revocation contract for
/// a keyed-promise adapter.
pub async fn turn_work_driver(host: Arc<dyn EffectHost>) {
    cancel_before_start_duplicate_replay_and_terminal_attach(Arc::clone(&host)).await;
    completion_seal_vs_cancel_is_first_writer_wins(Arc::clone(&host)).await;
    exact_scope_and_session_sweep_isolation(Arc::clone(&host)).await;
    after_step_request_defers_until_immediate_escalates_it(Arc::clone(&host)).await;
    after_step_request_is_honoured_at_the_step_boundary(Arc::clone(&host)).await;
    session_deletion_revokes_control_promises(host).await;
}

/// An after-step request rides the same gate as an immediate one but is not
/// effective until a step boundary. A stronger request upgrades it through
/// the reserved escalation promise and reports `Escalated`; the owner then
/// observes the abort at its next peek, before any boundary.
async fn after_step_request_defers_until_immediate_escalates_it(host: Arc<dyn EffectHost>) {
    let address = address("escalation");
    let driver = driver_for_session(Arc::clone(&host), &address).await;
    let peek = host
        .scoped(address.execution_scope())
        .expect("scoped peek controller");
    let active = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("active control");
    let stop = driver
        .request_cancel(request(address.clone(), "stop-1").mode(TurnCancelMode::AfterStep))
        .await
        .expect("after-step request");
    let stop_evidence = match stop.outcome {
        TurnCancelOutcome::Requested(evidence) => evidence,
        other => panic!("expected requested, got {other:?}"),
    };
    assert_eq!(stop_evidence.mode, TurnCancelMode::AfterStep);
    assert_eq!(stop_evidence.honoured_after_step, None);

    // Mid-model-call observation: remembered as deferred, never effective.
    let observed = active
        .observe_pending_cancel(
            peek.controller(),
            TurnCancelPeekIdentity::AfterLlm {
                protocol_iteration: 0,
            },
        )
        .await
        .expect("peek after llm");
    assert_eq!(observed, None);
    assert_eq!(active.evidence(), None);
    assert_eq!(active.deferred_evidence(), Some(stop_evidence.clone()));

    let same_strength = driver
        .request_cancel(request(address.clone(), "stop-2").mode(TurnCancelMode::AfterStep))
        .await
        .expect("second after-step request");
    assert!(matches!(
        same_strength.outcome,
        TurnCancelOutcome::AlreadyRequested(ref evidence) if evidence.request_id == "stop-1"
    ));

    let abort = driver
        .request_cancel(request(address.clone(), "abort-1"))
        .await
        .expect("escalate");
    let abort_evidence = match abort.outcome {
        TurnCancelOutcome::Escalated(evidence) => evidence,
        other => panic!("expected escalated, got {other:?}"),
    };
    assert_eq!(abort_evidence.request_id, "abort-1");
    assert_eq!(abort_evidence.mode, TurnCancelMode::Immediate);
    let repeat = driver
        .request_cancel(request(address.clone(), "abort-2"))
        .await
        .expect("repeat abort");
    assert!(matches!(
        repeat.outcome,
        TurnCancelOutcome::AlreadyRequested(ref evidence) if evidence.request_id == "abort-1"
    ));

    let observed = active
        .observe_pending_cancel(
            peek.controller(),
            TurnCancelPeekIdentity::AfterLlm {
                protocol_iteration: 1,
            },
        )
        .await
        .expect("peek after escalation");
    assert_eq!(observed, Some(abort_evidence.clone()));
    let settled = active
        .settle_before_commit(host.as_ref(), true, None)
        .await
        .expect("settle")
        .expect("escalated abort is what commits");
    assert_eq!(settled, abort_evidence);
    let terminal = TurnTerminal::Committed {
        outcome: TurnOutcome::Stopped(TurnStop::Cancelled { evidence: settled }),
        session_revision: Some(9),
    };
    active
        .publish_terminal(host.as_ref(), &terminal)
        .await
        .expect("publish terminal");
    match driver
        .await_terminal(&address)
        .await
        .expect("attach terminal")
    {
        TurnTerminal::Committed {
            outcome: TurnOutcome::Stopped(TurnStop::Cancelled { evidence }),
            session_revision: Some(9),
        } => assert_eq!(evidence, abort_evidence),
        other => panic!("attached terminal does not name the escalated abort: {other:?}"),
    }
    // A recreated owner sees the escalated abort, not the superseded stop.
    let recovered = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("recreate active control");
    let observed = recovered
        .observe_pending_cancel(peek.controller(), TurnCancelPeekIdentity::StartGate)
        .await
        .expect("peek start gate")
        .expect("durable abort survives owner loss");
    assert_eq!(observed, abort_evidence);
}

/// An after-step request lands at the peek that closes an iteration, and
/// only there: the honoured evidence names the iteration; a start gate honours
/// it without one; a replaying owner reaches the same evidence at the same
/// identity.
async fn after_step_request_is_honoured_at_the_step_boundary(host: Arc<dyn EffectHost>) {
    let address = address("boundary");
    let driver = driver_for_session(Arc::clone(&host), &address).await;
    let peek = host
        .scoped(address.execution_scope())
        .expect("scoped peek controller");
    let active = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("active control");
    let requested = match driver
        .request_cancel(request(address.clone(), "stop-1").mode(TurnCancelMode::AfterStep))
        .await
        .expect("after-step request")
        .outcome
    {
        TurnCancelOutcome::Requested(evidence) => evidence,
        other => panic!("expected requested, got {other:?}"),
    };
    assert_eq!(
        active
            .observe_pending_cancel(
                peek.controller(),
                TurnCancelPeekIdentity::AfterLlm {
                    protocol_iteration: 3,
                },
            )
            .await
            .expect("peek after llm"),
        None,
        "the after-LLM gate never honours an after-step request"
    );
    let honoured = active
        .observe_pending_cancel(
            peek.controller(),
            TurnCancelPeekIdentity::AfterStep {
                protocol_iteration: 3,
            },
        )
        .await
        .expect("peek at the boundary")
        .expect("after-step lands at the boundary");
    assert_eq!(
        honoured,
        TurnCancellationEvidence {
            honoured_after_step: Some(3),
            ..requested.clone()
        }
    );
    assert_eq!(active.evidence(), Some(honoured.clone()));
    let settled = active
        .settle_before_commit(host.as_ref(), false, None)
        .await
        .expect("settle")
        .expect("honoured stop commits");
    assert_eq!(settled, honoured);
    let terminal = TurnTerminal::Committed {
        outcome: TurnOutcome::Stopped(TurnStop::Cancelled { evidence: settled }),
        session_revision: Some(10),
    };
    active
        .publish_terminal(host.as_ref(), &terminal)
        .await
        .expect("publish terminal");

    // Crash between request and honour: the next owner honours the same
    // request at the same boundary identity, or at its start gate.
    let recovered = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("recreate active control");
    let again = recovered
        .observe_pending_cancel(
            peek.controller(),
            TurnCancelPeekIdentity::AfterStep {
                protocol_iteration: 3,
            },
        )
        .await
        .expect("peek at the boundary again")
        .expect("durable request survives owner loss");
    assert_eq!(again, honoured);
    let before_start = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("recreate active control");
    let refused = before_start
        .observe_pending_cancel(peek.controller(), TurnCancelPeekIdentity::StartGate)
        .await
        .expect("peek start gate")
        .expect("start gate honours either mode");
    assert_eq!(refused, requested);
    assert_eq!(refused.honoured_after_step, None);
}

async fn cancel_before_start_duplicate_replay_and_terminal_attach(host: Arc<dyn EffectHost>) {
    let address = address("before-start");
    let driver = driver_for_session(Arc::clone(&host), &address).await;
    let first = driver
        .request_cancel(request(address.clone(), "request-1"))
        .await
        .expect("request cancellation");
    let evidence = match first.outcome {
        TurnCancelOutcome::Requested(evidence) => evidence,
        other => panic!("expected requested, got {other:?}"),
    };
    let duplicate = driver
        .request_cancel(request(address.clone(), "request-2"))
        .await
        .expect("duplicate cancellation");
    assert!(matches!(
        duplicate.outcome,
        TurnCancelOutcome::AlreadyRequested(TurnCancellationEvidence {
            ref request_id,
            origin: Some(ref origin),
            ..
        }) if request_id == "request-1" && origin == "conformance-user"
    ));

    // Recreating this bridge models a new owner after lease loss. The durable
    // cancellation remains visible; the session-head CAS and any re-claimed
    // batch ownership decide whether an old owner's final commit can land.
    let recovered = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("recreate active control");
    let observed = recovered
        .settle_before_commit(host.as_ref(), false, None)
        .await
        .expect("settle recovered turn")
        .expect("pending cancellation survives replay");
    assert_eq!(observed, evidence);

    let terminal = TurnTerminal::Committed {
        outcome: crate::TurnOutcome::Stopped(TurnStop::Cancelled { evidence: observed }),
        session_revision: Some(7),
    };
    recovered
        .publish_terminal(host.as_ref(), &terminal)
        .await
        .expect("publish terminal");
    let attached = driver
        .await_terminal(&address)
        .await
        .expect("attach terminal");
    assert!(matches!(
        attached,
        TurnTerminal::Committed {
            outcome: crate::TurnOutcome::Stopped(TurnStop::Cancelled {
                evidence: TurnCancellationEvidence {
                    ref request_id,
                    origin: Some(ref origin),
                    ..
                }
            }),
            session_revision: Some(7),
        } if request_id == "request-1" && origin == "conformance-user"
    ));

    // Terminal publication is idempotent and first-writer-wins too. A stale
    // owner cannot replace the recovered owner's authoritative cancellation.
    recovered
        .publish_terminal(
            host.as_ref(),
            &TurnTerminal::Committed {
                outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
                    text: "stale completion".to_string(),
                }),
                session_revision: Some(6),
            },
        )
        .await
        .expect("duplicate terminal publication is idempotent");
    let attached_again = driver
        .await_terminal(&address)
        .await
        .expect("reattach terminal");
    assert!(matches!(
        attached_again,
        TurnTerminal::Committed {
            outcome: TurnOutcome::Stopped(TurnStop::Cancelled {
                evidence: TurnCancellationEvidence {
                    ref request_id,
                    origin: Some(ref origin),
                    ..
                }
            }),
            session_revision: Some(7),
        } if request_id == "request-1" && origin == "conformance-user"
    ));
}

async fn completion_seal_vs_cancel_is_first_writer_wins(host: Arc<dyn EffectHost>) {
    let address = address("race");
    let driver = driver_for_session(Arc::clone(&host), &address).await;
    let active = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("active control");
    let (seal, cancel) = tokio::join!(
        active.settle_before_commit(host.as_ref(), false, None),
        driver.request_cancel(request(address.clone(), "race-request")),
    );
    let terminal = match (seal.expect("seal"), cancel.expect("cancel").outcome) {
        (None, TurnCancelOutcome::CompletionWonRace) => TurnTerminal::Committed {
            outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
                text: "completion won".to_string(),
            }),
            session_revision: Some(8),
        },
        (Some(evidence), TurnCancelOutcome::Requested(requested)) => {
            assert_eq!(evidence, requested);
            TurnTerminal::Committed {
                outcome: TurnOutcome::Stopped(TurnStop::Cancelled { evidence }),
                session_revision: Some(8),
            }
        }
        other => panic!("inconsistent gate race result: {other:?}"),
    };
    active
        .publish_terminal(host.as_ref(), &terminal)
        .await
        .expect("publish race terminal");
    let attached = driver
        .await_terminal(&address)
        .await
        .expect("attach race terminal");
    // Whichever side won, the attached terminal must be the one that was
    // published: a cancelled terminal names the racing request, a completed
    // one carries the completion text.
    match attached {
        TurnTerminal::Committed {
            outcome: TurnOutcome::Stopped(TurnStop::Cancelled { evidence }),
            ..
        } => assert_eq!(evidence.request_id, "race-request"),
        TurnTerminal::Committed {
            outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage { text }),
            ..
        } => assert_eq!(text, "completion won"),
        other => panic!("attached terminal does not match the settled gate: {other:?}"),
    }
    assert!(matches!(
        driver
            .request_cancel(request(address, "after-terminal"))
            .await
            .expect("late cancellation is a typed no-op")
            .outcome,
        TurnCancelOutcome::CompletionWonRace
    ));
}

async fn exact_scope_and_session_sweep_isolation(host: Arc<dyn EffectHost>) {
    let address_a = address("scope");
    let driver = driver_for_session(Arc::clone(&host), &address_a).await;
    let address_b = TurnAddress::new(&address_a.session_id, "turn-b");
    let address_future = TurnAddress::new(&address_a.session_id, "turn-future");

    let active = Arc::new(
        ActiveTurnControl::new(host.as_ref(), address_a.clone())
            .await
            .expect("active control"),
    );
    let waiter_host = Arc::clone(&host);
    let waiter_active = Arc::clone(&active);
    let cancel_wait = crate::task::spawn(async move {
        waiter_active
            .await_cancel(waiter_host.as_ref(), CancellationToken::new())
            .await
    });
    tokio::task::yield_now().await;

    let tool_key = host
        .await_event_key(
            &ExecutionScope::turn(&address_a.session_id, "tool-turn"),
            AwaitEventWaitIdentity::tool_completion("tool-call"),
        )
        .await
        .expect("tool key");
    let tool_host = Arc::clone(&host);
    let tool_wait = crate::task::spawn(async move {
        tool_host
            .await_await_event(&tool_key, CancellationToken::new(), None)
            .await
    });
    tokio::task::yield_now().await;
    host.cancel_await_events_for_session(&address_a.session_id)
        .await
        .expect("cancel durable waits");
    assert!(matches!(
        tool_wait
            .await
            .expect("tool wait task")
            .expect("tool resolution"),
        Resolution::Cancelled
    ));
    assert!(
        !cancel_wait.is_finished(),
        "wait sweep cancelled the turn gate"
    );

    assert!(matches!(
        driver
            .request_cancel(request(address_a.clone(), "request-a"))
            .await
            .expect("cancel a")
            .outcome,
        TurnCancelOutcome::Requested(_)
    ));
    assert!(
        cancel_wait
            .await
            .expect("turn cancellation waiter")
            .expect("turn cancellation observation")
            .is_some()
    );
    assert!(matches!(
        driver
            .request_cancel(request(address_b, "request-b"))
            .await
            .expect("cancel b")
            .outcome,
        TurnCancelOutcome::Requested(_)
    ));
    assert!(matches!(
        driver
            .request_cancel(request(address_future, "request-future"))
            .await
            .expect("cancel future")
            .outcome,
        TurnCancelOutcome::Requested(_)
    ));
}

async fn session_deletion_revokes_control_promises(host: Arc<dyn EffectHost>) {
    let address = address("revoke");
    let driver = driver_for_session(Arc::clone(&host), &address).await;
    let active = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("create reserved control promises");
    let waiter_driver = driver.clone();
    let waiter_address = address.clone();
    let terminal_wait =
        crate::task::spawn(async move { waiter_driver.await_terminal(&waiter_address).await });
    tokio::task::yield_now().await;
    host.revoke_await_events_for_session(&address.session_id)
        .await
        .expect("revoke session");
    assert!(matches!(
        driver
            .request_cancel(request(address, "request-after-delete"))
            .await
            .expect("revoked outcome")
            .outcome,
        TurnCancelOutcome::UnknownOrRevoked
    ));
    assert!(
        terminal_wait.await.expect("terminal waiter task").is_err(),
        "session deletion did not revoke the reserved terminal promise"
    );
    assert!(
        active
            .settle_before_commit(host.as_ref(), false, None)
            .await
            .is_err(),
        "session deletion left the reserved cancellation gate live"
    );
}
