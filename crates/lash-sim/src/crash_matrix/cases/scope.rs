//! Scope close (S-8) and parent-end plans (S-10, S-11).
//!
//! **Scope close.** One input's root runs with one or two children registered
//! to live `Until` its scope. The root's terminal commit is followed by its
//! recorded `CloseRootScope` step, whose close records the root's parent-end
//! plan and delivers each child's cancel through the process-work port. The
//! cells cut that step before it starts, inside its child delivery, after
//! the delivery, before its result is journaled, and by losing the root's
//! invocation outright.
//!
//! **Parent-end plans.** The plan's producer is the registry's scope-close
//! sink closing a scope the host owns (a root that never ran, so no drive
//! closes it): it records the plan and applies it, delivering each `Until`
//! child's cancel. The host dies inside that apply, and the plan's
//! `ParentEnd` obligation (ADR 0109 §3), claimed by the recovery tick's due
//! pass, is its only owner.

use std::sync::Arc;

use lash_core::engine::ScopeCloseSink as _;
use lash_core::store::{ObligationKind, ObligationState, scope_close_obligation_id};
use lash_core::{ProcessId, ScopeId, SessionId, TurnId};
use lash_restate_test::{CrashPoint as EngineCut, CrashRule, TURN_DRIVER_SERVICE};

use super::{Staged, crash_and_restart, send, session_name, standard_core};
use crate::crash_matrix::deployment::{ArmEffect, HostSite};
use crate::crash_matrix::invariants::{AcceptedInput, ChildOf, Expected};
use crate::crash_matrix::world::CrashWorld;
use crate::crash_matrix::{CrashPoint, Seam};

/// Register a child of `session` that lives until `parent` ends.
pub(super) async fn register_until_child(
    world: &CrashWorld,
    session: &SessionId,
    parent: &ScopeId,
) -> Result<ProcessId, String> {
    let mut registration = lash_core::ProcessRegistration::new(
        lash_core::ProcessInput::External {
            metadata: serde_json::json!({ "crash_matrix": "child" }),
        },
        lash_core::ProcessProvenance::session(lash_core::SessionScope::new(session.as_str())),
        lash_core::Lifetime::Detached,
    );
    registration.ancestry = lash_core::Ancestry::from_scopes([parent.clone()]);
    registration.lifetime = lash_core::LifetimeDecision::Until {
        scope: parent.clone(),
        grant: lash_core::ScopeGrant::Ancestor,
    };
    world
        .backend()
        .process_registry()
        .register_process(registration)
        .await
        .map(|registered| registered.id)
        .map_err(|error| format!("register a child of `{parent}`: {error}"))
}

/// Kill every invocation of `root`'s `LashTurn` workflow and wait until the
/// attempts it stopped have ended.
async fn lose_root_invocation(world: &CrashWorld, root: &str) -> Result<(), String> {
    let lost: Vec<String> = world
        .invocations()
        .await
        .into_iter()
        .filter(|view| {
            view.target.starts_with(&format!("{TURN_DRIVER_SERVICE}/"))
                && view.target.contains(root)
                && view.status != "completed"
        })
        .map(|view| view.id)
        .collect();
    if lost.is_empty() {
        return Err(format!(
            "no live `{TURN_DRIVER_SERVICE}` invocation of `{root}`"
        ));
    }
    for id in &lost {
        world.kill_invocation(id).await?;
    }
    Ok(())
}

pub(super) async fn stage_scope_close(point: CrashPoint, seed: u64) -> Result<Staged, String> {
    stage_scope_close_with_claim(point, seed, false).await
}

#[cfg(test)]
pub(super) async fn stage_scope_close_claimed(
    point: CrashPoint,
    seed: u64,
) -> Result<Staged, String> {
    stage_scope_close_with_claim(point, seed, true).await
}

async fn claim_scope_close_before_restart(
    world: &CrashWorld,
    session: &SessionId,
    root: &str,
) -> Result<(), String> {
    world.kill().await;
    let id = scope_close_obligation_id(session, &TurnId::from(root));
    let ledger = world
        .backend()
        .obligation_ledger(ObligationKind::ScopeClose);
    match ledger
        .state(&id)
        .await
        .map_err(|error| format!("read scope-close obligation `{id}`: {error}"))?
    {
        Some(ObligationState::Claimed) => {}
        Some(ObligationState::Due) => {
            if ledger
                .claim(&id, world.now_ms(), 60_000)
                .await
                .map_err(|error| format!("claim scope-close obligation `{id}`: {error}"))?
                .is_none()
            {
                return Err(format!("scope-close obligation `{id}` lost its due claim"));
            }
        }
        other => {
            return Err(format!(
                "scope-close obligation `{id}` was {other:?} after the crash"
            ));
        }
    }
    world.restart().await
}

async fn stage_scope_close_with_claim(
    point: CrashPoint,
    seed: u64,
    claim_before_restart: bool,
) -> Result<Staged, String> {
    let world = CrashWorld::new(seed, standard_core(), false).await?;
    world.restart().await?;
    let session = session_name(Seam::ScopeClose, seed);
    let root = "in-0";
    let scope = ScopeId::turn(session.clone(), TurnId::from(root));
    let child_count = 1 + world.draw(0..2) as usize;
    let mut expected = Expected {
        inputs: vec![AcceptedInput {
            session: session.clone(),
            root: TurnId::from(root),
        }],
        closed_scopes: vec![scope.clone()],
        live_sessions: vec![session.clone()],
        ..Expected::default()
    };
    // The session exists before its root's children are registered.
    let core = world.core()?;
    core.session(session.clone())
        .open()
        .await
        .map_err(|error| format!("open `{session}`: {error}"))?;
    for _ in 0..child_count {
        let child = register_until_child(&world, &session, &scope).await?;
        expected.children.push(ChildOf {
            child,
            parent: scope.clone(),
        });
    }
    let close_step = format!("lash:drive-close:{root}");
    let notes = vec![format!("children={child_count}")];
    let origin_ms = match point {
        CrashPoint::AfterStateCommit => {
            world.crash_on(
                CrashRule::new(EngineCut::BeforeRun { name: close_step })
                    .service(TURN_DRIVER_SERVICE)
                    .within_attempts(1),
            );
            send(&world, &session, root).await?;
            if claim_before_restart {
                let tripped = world.trip().wait(super::TRIP_WAIT).await;
                if let Some(tripped) = tripped {
                    claim_scope_close_before_restart(&world, &session, root).await?;
                    Some(tripped.at_ms)
                } else {
                    None
                }
            } else {
                crash_and_restart(&world).await?
            }
        }
        CrashPoint::DuringEngineDelivery => {
            world.faults().crash_once(HostSite::DeliverCancelBefore);
            send(&world, &session, root).await?;
            crash_and_restart(&world).await?
        }
        CrashPoint::AfterDeliveryBeforeSettle => {
            world.faults().crash_once(HostSite::DeliverCancelAfter);
            send(&world, &session, root).await?;
            crash_and_restart(&world).await?
        }
        CrashPoint::MidJournalStep => {
            world.crash_on(
                CrashRule::new(EngineCut::BeforeRunResult {
                    name: Some(close_step),
                })
                .service(TURN_DRIVER_SERVICE)
                .within_attempts(1),
            );
            send(&world, &session, root).await?;
            crash_and_restart(&world).await?
        }
        CrashPoint::InvocationLost => {
            // The root's terminal is committed and its close step is next;
            // the deployment dies there and the engine loses the root's
            // invocation, so no replay closes the scope.
            world.crash_on(
                CrashRule::new(EngineCut::BeforeRun { name: close_step })
                    .service(TURN_DRIVER_SERVICE)
                    .within_attempts(1),
            );
            send(&world, &session, root).await?;
            match world.trip().wait(std::time::Duration::from_secs(20)).await {
                Some(tripped) => {
                    lose_root_invocation(&world, root).await?;
                    if claim_before_restart {
                        claim_scope_close_before_restart(&world, &session, root).await?;
                    } else {
                        world.crash_and_restart().await?;
                    }
                    Some(tripped.at_ms)
                }
                None => None,
            }
        }
        other => return Err(format!("scope close has no {other:?} cell")),
    };
    Ok(Staged {
        world,
        notes,
        expected,
        origin_ms,
    })
}

/// The row a crash before the close step left to recovery. On a live engine the
/// `BeforeRun` frame can race the SDK's local close attempt: it may claim the
/// row before the deployment dies even though the frame never reached the
/// journal. The cell must judge the actual row under the matching §1.8 bound.
pub(super) async fn obligation_at_restart(
    world: &CrashWorld,
    seed: u64,
) -> Result<ObligationState, String> {
    let session = session_name(Seam::ScopeClose, seed);
    let id = scope_close_obligation_id(&session, &TurnId::from("in-0"));
    world
        .backend()
        .obligation_ledger(ObligationKind::ScopeClose)
        .state(&id)
        .await
        .map_err(|error| format!("read scope-close obligation `{id}` after the crash: {error}"))?
        .ok_or_else(|| format!("the terminal did not arm scope-close obligation `{id}`"))
}

/// The scope-close sink a producer closes a host-owned scope through: the
/// registry's, delivering over the deployment's process-work port.
fn producer_sink(world: &CrashWorld) -> Result<lash_core::RegistryScopeClose, String> {
    let port = world.backend().process_work();
    Ok(lash_core::RegistryScopeClose::with_delivery(
        world.backend().process_registry(),
        Arc::clone(port.port()),
        world.backend().clock(),
    ))
}

/// The terminal a producer closes the host-owned root `root` with.
fn host_root_terminal(
    world: &CrashWorld,
    session: &SessionId,
    root: &str,
) -> lash_core::store::RootTerminal {
    lash_core::store::RootTerminal {
        session_id: session.clone(),
        root: TurnId::from(root),
        kind: lash_core::store::RootTerminalKind::Answered,
        cause: lash_core::store::RootTerminalCause::Committed {
            commit: lash_core::store::TurnCommitId::new(TurnId::from(root), 0),
            turn: TurnId::from(root),
            stop: None,
        },
        head_revision: None,
        at_ms: world.now_ms(),
    }
}

/// Close the host-owned scope of `root` as the producer does — record the
/// plan, apply it — as the deployment's own host work. A refused delivery
/// fails the producer's apply and leaves the plan to the recovery pass; a
/// host that died inside the close never answers.
async fn produce_parent_end(
    world: &CrashWorld,
    session: &SessionId,
    root: &str,
) -> Result<(), String> {
    let sink = producer_sink(world)?;
    let terminal = host_root_terminal(world, session, root);
    let _ = world
        .host_op(async move { sink.close_root_scope(&terminal).await })
        .await;
    Ok(())
}

/// The number of poisoned plans ahead of the victim: one page of the
/// recovery tick's due pass (64) and one more.
const POISONED_PLANS: usize = 65;

pub(super) async fn stage_parent_end(point: CrashPoint, seed: u64) -> Result<Staged, String> {
    let world = CrashWorld::new(seed, standard_core(), false).await?;
    world.restart().await?;
    let session = session_name(Seam::ParentEnd, seed);
    world
        .core()?
        .session(session.clone())
        .open()
        .await
        .map_err(|error| format!("open `{session}`: {error}"))?;
    let mut expected = Expected {
        live_sessions: vec![session.clone()],
        ..Expected::default()
    };
    let mut notes = Vec::new();
    let origin_ms = match point {
        CrashPoint::AfterStateCommit
        | CrashPoint::DuringEngineDelivery
        | CrashPoint::AfterDeliveryBeforeSettle => {
            let root = "host-scope";
            let parent = ScopeId::turn(session.clone(), TurnId::from(root));
            // The mid-delivery cut needs a child delivered before the one the
            // host dies on.
            let child_count = if point == CrashPoint::DuringEngineDelivery {
                2
            } else {
                1 + world.draw(0..2) as usize
            };
            let mut children = Vec::new();
            for _ in 0..child_count {
                let child = register_until_child(&world, &session, &parent).await?;
                expected.children.push(ChildOf {
                    child: child.clone(),
                    parent: parent.clone(),
                });
                children.push(child);
            }
            expected.closed_scopes.push(parent);
            notes.push(format!("children={child_count}"));
            match point {
                CrashPoint::AfterStateCommit => {
                    world.faults().crash_once(HostSite::DeliverCancelBefore);
                }
                CrashPoint::DuringEngineDelivery => {
                    let last = children.last().map(ToString::to_string).unwrap_or_default();
                    world
                        .faults()
                        .crash_once_matching(HostSite::DeliverCancelBefore, last);
                }
                _ => {
                    let last = children.last().map(ToString::to_string).unwrap_or_default();
                    world
                        .faults()
                        .crash_once_matching(HostSite::DeliverCancelAfter, last);
                }
            }
            produce_parent_end(&world, &session, root).await?;
            crash_and_restart(&world).await?
        }
        CrashPoint::DeliveryRefused => {
            // A page of plans whose only child refuses every cancel, then a
            // victim plan behind them whose child accepts it.
            for index in 0..POISONED_PLANS {
                let root = format!("poisoned-{index}");
                let parent = ScopeId::turn(session.clone(), TurnId::from(root.as_str()));
                let child = register_until_child(&world, &session, &parent).await?;
                world.faults().always_matching(
                    HostSite::DeliverCancelBefore,
                    ArmEffect::Refuse,
                    format!("{child}/"),
                );
                expected.children.push(ChildOf {
                    child,
                    parent: parent.clone(),
                });
                produce_parent_end(&world, &session, &root).await?;
            }
            let victim = ScopeId::turn(session.clone(), TurnId::from("victim"));
            let child = register_until_child(&world, &session, &victim).await?;
            expected.children.push(ChildOf {
                child: child.clone(),
                parent: victim.clone(),
            });
            expected.closed_scopes.push(victim);
            // The victim's own close dies before it delivers, so only a
            // recovery pass can reach it.
            world
                .faults()
                .crash_once_matching(HostSite::DeliverCancelBefore, format!("{child}/"));
            produce_parent_end(&world, &session, "victim").await?;
            notes.push(format!("poisoned_plans={POISONED_PLANS}"));
            // §1.8's bound is the pass that claims the rows, so it counts
            // from the deployment that can run one: the seeded outage between
            // the crash and the restart runs no pass.
            crash_and_restart(&world).await?.map(|_| world.now_ms())
        }
        other => return Err(format!("parent end has no {other:?} cell")),
    };
    Ok(Staged {
        world,
        notes,
        expected,
        origin_ms,
    })
}
