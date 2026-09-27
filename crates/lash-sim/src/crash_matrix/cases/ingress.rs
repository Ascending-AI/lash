//! Ingress (S-1–S-4): an accepted input and the drive that consumes it.
//!
//! The workload accepts one or two inputs on one session, each its own root;
//! the seed picks which one the crash cuts. Inputs before the target settle
//! first, inputs after it are accepted by the restarted deployment.

use lash_core::TurnId;
use lash_restate_test::{
    CrashPoint as EngineCut, CrashRule, SESSION_DRIVER_SERVICE, TURN_DRIVER_SERVICE,
};

use super::{Staged, crash_and_restart, send, session_name, standard_core};
use crate::crash_matrix::deployment::HostSite;
use crate::crash_matrix::invariants::{AcceptedInput, Expected};
use crate::crash_matrix::world::CrashWorld;
use crate::crash_matrix::{CrashPoint, Seam};

/// The journal commands of a text-only root's `LashTurn` invocation the
/// mid-journal cut draws from: after the input command (0) come the build
/// generation, the root start, the seal, the claim, the turn config, the
/// model call and its checkpoint with their calls, the scope close, the
/// state write and the output — 19 more.
const ROOT_JOURNAL_CUTS: u64 = 19;

pub(super) async fn stage(point: CrashPoint, seed: u64) -> Result<Staged, String> {
    let world = CrashWorld::new(seed, standard_core(), false).await?;
    world.restart().await?;
    let session = session_name(Seam::Ingress, seed);
    let inputs = 1 + world.draw(0..2) as usize;
    let target = world.draw(0..inputs as u64) as usize;
    let mut expected = Expected {
        live_sessions: vec![session.clone()],
        ..Expected::default()
    };
    let mut origin_ms = None;
    let notes = vec![format!("inputs={inputs} target=in-{target}")];
    for index in 0..inputs {
        let root = format!("in-{index}");
        expected.inputs.push(AcceptedInput {
            session: session.clone(),
            root: TurnId::from(root.as_str()),
        });
        if index != target {
            send(&world, &session, &root).await?;
            world.quiesce().await;
            continue;
        }
        origin_ms = match point {
            CrashPoint::AfterStateCommit => {
                world.faults().crash_once(HostSite::DriveAsk);
                send(&world, &session, &root).await?;
                crash_and_restart(&world).await?
            }
            CrashPoint::DuringEngineDelivery => {
                world.crash_on(
                    CrashRule::new(EngineCut::BeforeCommand { index: 1 })
                        .service(SESSION_DRIVER_SERVICE)
                        .key(session.as_str())
                        .within_attempts(1),
                );
                send(&world, &session, &root).await?;
                crash_and_restart(&world).await?
            }
            CrashPoint::AfterDeliveryBeforeSettle => {
                world.crash_on(
                    CrashRule::new(EngineCut::BeforeRunResult { name: None })
                        .service(SESSION_DRIVER_SERVICE)
                        .key(session.as_str())
                        .within_attempts(1),
                );
                send(&world, &session, &root).await?;
                crash_and_restart(&world).await?
            }
            CrashPoint::MidJournalStep => {
                let index = 1 + world.draw(0..ROOT_JOURNAL_CUTS) as usize;
                world.crash_on(
                    CrashRule::new(EngineCut::BeforeCommand { index })
                        .service(TURN_DRIVER_SERVICE)
                        .within_attempts(1),
                );
                send(&world, &session, &root).await?;
                crash_and_restart(&world).await?
            }
            CrashPoint::InvocationLost => {
                // The drive the acceptance asked for is lost by the engine
                // before it admits anything; the host lives on.
                let hold = world.hold_session_drive(&session).await;
                send(&world, &session, &root).await?;
                let prefix = format!("{SESSION_DRIVER_SERVICE}/{session}/");
                // The acceptance's ask is fire-and-forget: wait until it lands.
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                let lost = loop {
                    let lost: Vec<String> = world
                        .invocations()
                        .await
                        .into_iter()
                        .filter(|view| {
                            view.target.starts_with(&prefix) && view.status != "completed"
                        })
                        .map(|view| view.id)
                        .collect();
                    if !lost.is_empty() {
                        break lost;
                    }
                    if tokio::time::Instant::now() > deadline {
                        return Err("the acceptance's drive never reached the engine".to_owned());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                };
                for id in &lost {
                    world.kill_invocation(id).await?;
                }
                world.trip().fire(format!("engine-lost:{prefix}"));
                hold.release();
                world.trip().tripped().map(|tripped| tripped.at_ms)
            }
            other => return Err(format!("ingress has no {other:?} cell")),
        };
    }
    Ok(Staged {
        world,
        notes,
        expected,
        origin_ms,
    })
}
