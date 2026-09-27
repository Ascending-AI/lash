//! Control intents (S-6, S-7, S-9) and session deletion (S-21).
//!
//! A session runs one root that is held inside its model call, with a child
//! registered to live `Until` the root. The host deletes the session from
//! inside a handler of its own, as a deployment's delete endpoint does:
//! `LashCore::delete_session` writes the `CloseSession` intent (ending the
//! root), releases the root's engine execution, closes the root's and the
//! session's scopes, acknowledges the intent, and then deletes the storage.
//!
//! The control-intent cells kill the host inside the close's engine half and
//! expect the intent settled and everything it owed done. The deletion cells
//! kill it before the physical delete and expect the session deleted anyway,
//! which only ADR 0109's two-phase delete (S8-D) owes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_core::{ScopeId, SessionId, TurnId};

use super::scope::register_until_child;
use super::{Staged, crash_and_restart, held_core, send, session_name};
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
    let restate = world.restate().clone();
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
    world
        .core()?
        .session(session.clone())
        .open()
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
