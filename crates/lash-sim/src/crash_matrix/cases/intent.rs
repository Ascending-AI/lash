//! Control intents (S-6, S-7, S-9) and session deletion (S-21).
//!
//! A session runs one root that is held inside its model call, with a child
//! registered to live `Until` the root. The host deletes the session from
//! inside a handler of its own, as a deployment's delete endpoint does:
//! `LashCore::delete_session` writes the `CloseSession` intent (ending the
//! root), releases the root's engine execution, closes the root's and the
//! session's scopes and acknowledges the intent, which arms the session's
//! `SessionDelete` obligation; its delivery deletes the storage once the
//! close's cleanup settled (ADR 0109 §4).
//!
//! The control-intent cells kill the host inside the close's engine half and
//! expect the intent settled and everything it owed done, the deletion
//! included. The deletion cells kill it before the physical delete and expect
//! the obligation to finish the deletion without a caller retry.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_core::runtime::drive::relay::{RelayVerdict, deliver_now};
use lash_core::runtime::session_delete::SessionDeleteRelay;
use lash_core::{ScopeId, SessionId, TurnId};

use super::scope::register_until_child;
use super::{Staged, TRIP_WAIT, crash_and_restart, held_core, send, session_name};

/// The deliverable attempts a deletion's crash point may take to fire.
const ATTEMPTS_TO_TRIP: usize = 6;

/// The recovery ticks the close's cleanup may take before the delete is
/// declared stuck.
const TICKS_TO_DELIVERABLE: usize = 36;
use crate::crash_matrix::deployment::{ArmEffect, HostSite};
use crate::crash_matrix::invariants::{ChildOf, Expected};
use crate::crash_matrix::world::CrashWorld;
use crate::crash_matrix::{CrashPoint, Seam};

/// A deletion's handler execution: the deployment's administration over the
/// handler's own controller.
struct HandlerExecution<'a> {
    admin: lash_core::SessionAdministration,
    scoped: lash_core::ScopedEffectController<'a>,
}

impl lash_core::SessionDeleteExecution for HandlerExecution<'_> {
    fn administration(&self) -> &lash_core::SessionAdministration {
        &self.admin
    }

    fn scoped<'run>(
        &'run self,
        _: lash_core::AdmittedScope,
    ) -> Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        Ok(self.scoped.clone())
    }
}

/// Delete `session` through the live deployment, inside a handler of the
/// host's own, as the host's work: it dies with the deployment.
async fn delete_session(world: &CrashWorld, session: &SessionId) -> Result<(), String> {
    let core = world.core()?;
    let admin = core.session_administration().await;
    let restate = world.engine().clone();
    let session = session.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<(), String>>();
    let deleted = world.host_op(async move {
        let id = session.clone();
        restate
            .run_in_handler(
                lash_core::AdmittedScope::session_delete(&session),
                Arc::new(move |scoped| {
                    let execution = HandlerExecution {
                        admin: admin.clone(),
                        scoped,
                    };
                    let id = id.clone();
                    let tx = tx.clone();
                    Box::pin(async move {
                        let outcome = match lash_core::SessionDeleteContext::from_execution(
                            &execution,
                            id.as_str(),
                        ) {
                            Ok(context) => lash::LashCore::delete_session(context)
                                .await
                                .map(|_| ())
                                .map_err(|error| error.to_string()),
                            Err(error) => Err(error.to_string()),
                        };
                        let _ = tx.send(outcome);
                    })
                }),
            )
            .await
    });
    let ran = deleted.await;
    match (rx.try_recv(), ran) {
        (Ok(outcome), _) => outcome,
        // The host died inside the deletion: nothing answered.
        (Err(_), None) => Ok(()),
        (Err(_), Some(Ok(()))) => Err("the deletion's handler ran without answering".to_owned()),
        (Err(_), Some(Err(error))) => {
            if world.trip().tripped().is_some() {
                Ok(())
            } else {
                Err(format!("the deletion's handler failed: {error}"))
            }
        }
    }
}

/// Wait until `held` counts a model call held open.
async fn await_held(held: &AtomicUsize) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while held.load(Ordering::SeqCst) == 0 {
        if tokio::time::Instant::now() > deadline {
            return Err("the held root never reached its model call".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Ok(())
}

/// Whether `session`'s `SessionDelete` obligation could deliver now, read
/// the way its relay reads it: the obligation armed and the close's cleanup
/// settled.
async fn delete_deliverable(world: &CrashWorld, session: &SessionId) -> Result<bool, String> {
    let admin = world.core()?.session_administration().await;
    let close = admin.session_close();
    let ledger = close.deletes.ledger();
    let Some(_) = ledger
        .delete_obligation(session)
        .await
        .map_err(|error| format!("`{session}`'s delete obligation: {error}"))?
    else {
        // Not armed yet: the close's acknowledgement is still owed.
        return Ok(false);
    };
    let cleanup = ledger
        .undelivered_cleanup(session)
        .await
        .map_err(|error| format!("`{session}`'s cleanup: {error}"))?;
    Ok(cleanup.is_settled())
}

/// Make `session`'s delete's next attempt on the host, as the verb's own
/// immediate attempt does: `deliver_now` claims the due row whatever backoff
/// its deferrals left, so the armed site is reached inside a delivery that
/// dies holding its claim. `None` when the host died inside it — the crash
/// point firing.
async fn deliver_delete_now(
    world: &CrashWorld,
    session: &SessionId,
) -> Result<Option<Result<RelayVerdict, String>>, String> {
    let core = world.core()?;
    let session = session.clone();
    Ok(world
        .host_op(async move {
            let administration = core.session_administration().await;
            let deletes = &administration.session_close().deletes;
            let Some(obligation) = deletes
                .ledger()
                .delete_obligation(&session)
                .await
                .map_err(|error| error.to_string())?
            else {
                return Ok(RelayVerdict::NotDue);
            };
            let clock = administration.session_close().clock.clone();
            let relay = SessionDeleteRelay::new(administration);
            deliver_now(&relay, &obligation.id, clock.as_ref())
                .await
                .map_err(|error| error.to_string())
        })
        .await)
}

/// Tick the recovery interval until the armed crash point fires.
///
/// The delete's delivery may run only once the close's cleanup settled; a
/// slow cleanup can outlast every backoff the due pass would wait out, so no
/// fixed tick count bounds it. The ticks meanwhile run the passes that
/// deliver the cleanup, and once the delivery's own gate reads clear the
/// attempt is made on the host, which dies inside it holding the claim.
async fn tick_until_tripped(world: &CrashWorld, session: &SessionId) -> Result<(), String> {
    let mut ticks = 0_usize;
    let mut attempts = 0_usize;
    while world.trip().tripped().is_none() {
        if !delete_deliverable(world, session).await? {
            ticks += 1;
            if ticks > TICKS_TO_DELIVERABLE {
                return Err(format!(
                    "session `{session}`'s delete never became deliverable in {TICKS_TO_DELIVERABLE} ticks"
                ));
            }
            tokio::select! {
                ticked = world.tick() => {
                    ticked?;
                }
                _ = world.trip().wait(TRIP_WAIT) => {}
            }
            continue;
        }
        attempts += 1;
        if attempts > ATTEMPTS_TO_TRIP {
            return Err(format!(
                "session `{session}`'s delete was deliverable but never fired in {ATTEMPTS_TO_TRIP} attempts"
            ));
        }
        match deliver_delete_now(world, session).await? {
            // The host died inside the delivery: the crash point fired.
            None => return Ok(()),
            Some(Err(error)) => return Err(error),
            Some(Ok(RelayVerdict::Delivered)) => {
                return Err(format!(
                    "session `{session}`'s delete delivered without firing the armed site"
                ));
            }
            Some(Ok(RelayVerdict::Stalled(reason))) => {
                return Err(format!(
                    "session `{session}`'s delete stalled {reason:?} under the armed site"
                ));
            }
            // A deferral or a claim lost to another relay: re-read the
            // delivery's gates and go again.
            Some(Ok(_)) => {}
        }
    }
    Ok(())
}

pub(super) async fn stage_close(point: CrashPoint, seed: u64) -> Result<Staged, String> {
    stage_session_end(Seam::ControlIntent, point, seed).await
}

pub(super) async fn stage_delete(point: CrashPoint, seed: u64) -> Result<Staged, String> {
    stage_session_end(Seam::SessionDelete, point, seed).await
}

async fn stage_session_end(seam: Seam, point: CrashPoint, seed: u64) -> Result<Staged, String> {
    let held = Arc::new(AtomicUsize::new(0));
    let world = CrashWorld::new(seed, held_core(Arc::clone(&held)), false).await?;
    world.restart().await?;
    let session = session_name(seam, seed);
    crate::open_created_session(&world.core()?, session.clone())
        .await
        .map_err(|error| format!("open `{session}`: {error}"))?;
    let root = "held-0";
    let scope = ScopeId::turn(session.clone(), TurnId::from(root));
    let child = register_until_child(&world, &session, &scope).await?;
    let mut expected = Expected {
        children: vec![ChildOf {
            child,
            parent: scope.clone(),
        }],
        closed_scopes: vec![scope, ScopeId::session(session.clone())],
        closed_sessions: vec![session.clone()],
        ..Expected::default()
    };
    if seam == Seam::SessionDelete {
        expected.deleted_sessions.push(session.clone());
    }
    send(&world, &session, root).await?;
    await_held(&held).await?;
    let origin_ms = match (seam, point) {
        (_, CrashPoint::AfterStateCommit) => {
            world.faults().crash_once(HostSite::ReleaseRootBefore);
            delete_session(&world, &session).await?;
            crash_and_restart(&world).await?
        }
        (Seam::ControlIntent, CrashPoint::DuringEngineDelivery) => {
            world.faults().crash_once(HostSite::ReleaseRootAfter);
            delete_session(&world, &session).await?;
            crash_and_restart(&world).await?
        }
        (Seam::ControlIntent, CrashPoint::AfterDeliveryBeforeSettle) => {
            world.faults().crash_once(HostSite::AcknowledgeIntentBefore);
            delete_session(&world, &session).await?;
            crash_and_restart(&world).await?
        }
        (Seam::SessionDelete, CrashPoint::AfterDeliveryBeforeSettle) => {
            world.faults().crash_once(HostSite::DeleteStorageBefore);
            delete_session(&world, &session).await?;
            // The verb's own attempt defers while the engine still runs the
            // released root's work; the stage waits out that deferral and
            // makes the next attempt the host dies inside.
            tick_until_tripped(&world, &session).await?;
            crash_and_restart(&world).await?
        }
        (Seam::ControlIntent, CrashPoint::DeliveryRetryableForever) => {
            // The engine half fails retryably on every attempt; the host
            // lives, and the deletion it asked for answers with the close
            // retained for reconciliation.
            world
                .faults()
                .always(HostSite::ReleaseRootBefore, ArmEffect::FailRetryable);
            let at_ms = world.now_ms();
            let _ = delete_session(&world, &session).await;
            world.trip().fire("fault:release-root-retryable-forever");
            // A release that fails on every attempt never ends the held
            // root's execution, and the session's scope close follows the
            // release inside the engine half: what the ceiling owes is the
            // typed stall, surfaced for an operator, never another attempt.
            // A stalled close arms no physical delete (ADR 0109 §4), so the
            // input its root admitted stays admitted: the store types that
            // turn as held by the stalled close, and the ingress probe reads
            // it as a typed stall, not a lost turn.
            expected.closed_scopes.clear();
            expected.closed_sessions.clear();
            expected.custom.push((
                "control_intent_ceiling",
                stalled_at_ceiling(session.clone()),
            ));
            Some(at_ms)
        }
        (seam, other) => return Err(format!("{seam:?} has no {other:?} cell")),
    };
    Ok(Staged {
        world,
        notes: Vec::new(),
        expected,
        origin_ms,
    })
}

/// Every control intent of `session` failed for good and its obligation
/// stalled typed at exactly the attempt ceiling: the ceiling ended the
/// retries, never an earlier refusal.
fn stalled_at_ceiling(session: SessionId) -> crate::crash_matrix::invariants::CustomCheck {
    Arc::new(move |world: &CrashWorld| {
        let session = session.clone();
        Box::pin(async move {
            let intents = match world
                .backend()
                .session_store_factory()
                .list_control_intents(None, std::num::NonZeroUsize::MIN.saturating_add(63))
                .await
            {
                Ok(intents) => intents,
                Err(error) => return vec![format!("list control intents: {error}")],
            };
            let ledger = world
                .backend()
                .obligation_ledger(lash_core::store::ObligationKind::ControlIntent);
            let page = std::num::NonZeroUsize::MIN.saturating_add(63);
            let stalled = match ledger.list_stalled(None, page).await {
                Ok(stalled) => stalled,
                Err(error) => return vec![format!("list stalled control intents: {error}")],
            };
            let ceiling = lash_core::runtime::drive::relay::RelayPolicy::default()
                .attempt_ceiling
                .get();
            let mut violations = Vec::new();
            let mut seen = 0_usize;
            for intent in intents
                .into_iter()
                .filter(|intent| intent.session_id == session)
            {
                seen += 1;
                if !matches!(
                    intent.state,
                    lash_core::store::ControlIntentState::Failed {
                        retryable: false,
                        ..
                    }
                ) {
                    violations.push(format!(
                        "intent {} is {:?}, not failed for good",
                        intent.id, intent.state
                    ));
                }
                let Some(obligation) = intent.obligation.as_ref() else {
                    violations.push(format!("intent {} has no obligation", intent.id));
                    continue;
                };
                match stalled.iter().find(|entry| &entry.id == obligation) {
                    Some(entry)
                        if entry.reason == lash_core::store::StallReason::AttemptsExhausted
                            && entry.attempts == ceiling => {}
                    Some(entry) => violations.push(format!(
                        "intent {} stalled {:?} after {} attempt(s), not at the ceiling of {ceiling}",
                        intent.id, entry.reason, entry.attempts
                    )),
                    None => violations.push(format!(
                        "intent {}'s obligation is not stalled: {:?}",
                        intent.id,
                        ledger.state(obligation).await
                    )),
                }
            }
            if seen == 0 {
                violations.push(format!("`{session}` recorded no control intent"));
            }
            violations
        })
    })
}
