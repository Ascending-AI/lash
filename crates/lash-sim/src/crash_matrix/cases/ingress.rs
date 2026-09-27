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
use crate::crash_matrix::invariants::{self, AcceptedInput, Expected};
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

/// Kill every invocation of `session`'s drive the engine holds once the
/// engine accepted the ask `request`: the engine loses that ask before it
/// admits anything.
async fn lose_session_drive(
    world: &CrashWorld,
    session: &lash_core::SessionId,
    request: &str,
) -> Result<(), String> {
    let prefix = format!("{SESSION_DRIVER_SERVICE}/{session}/");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !world.drives().asked().iter().any(|asked| asked == request) {
        if tokio::time::Instant::now() > deadline {
            return Err(format!("the engine never accepted the ask `{request}`"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let lost = loop {
        let lost: Vec<String> = world
            .invocations()
            .await
            .into_iter()
            .filter(|view| view.target.starts_with(&prefix) && view.status != "completed")
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
    Ok(())
}

/// FIG-3879: a waiter on an accepted input attaches to its first attempt's
/// drive, and when the engine loses that drive before it admits anything (an
/// operator kill) the waiter follows the relay's ask under the next attempt,
/// `ingress:{input}:{attempt}`, to the input's answer. The session's drive is
/// held until the waiter attached to the re-asked one, so the waiter cannot
/// answer from the store before it follows. Answers what the case found
/// wrong: empty when the waiter awaited the re-asked drive and answered.
pub async fn a_waiter_follows_its_input_past_a_lost_ask(seed: u64) -> Result<Vec<String>, String> {
    let world = CrashWorld::new(seed, standard_core(), false).await?;
    world.restart().await?;
    // A held drive never settles: wait on the engine briefly per tick.
    world.set_quiesce_budget(std::time::Duration::from_millis(100));
    let session = session_name(Seam::Ingress, seed);
    let root = "followed";
    let hold = world.hold_session_drive(&session).await;
    let core = world.core()?;
    let accepted = {
        let session = session.clone();
        let text = invariants::input_text(root);
        world
            .host_op(async move {
                let session = core.session(session).open().await?;
                session.send(lash::TurnInput::text(text)).id(root).await
            })
            .await
            .ok_or_else(|| "the host died inside the send".to_owned())?
            .map_err(|error| format!("the send was refused: {error}"))?
    };
    let item = lash_core::PendingTurnInputDraft::keyed_input_id(&session, root);
    let ask = |attempt| lash_core::drive::ingress_drive_request(&item, attempt);
    let first = ask(lash_core::drive::FIRST_INGRESS_ATTEMPT);
    let next = ask(lash_core::drive::FIRST_INGRESS_ATTEMPT + 1);
    let waiter = world.spawn_host(async move { accepted.outcome().await });
    lose_session_drive(&world, &session, first.as_str()).await?;
    let mut violations = Vec::new();
    // The lost ask's claim lapses and the relay asks again, a claim TTL and a
    // tick later; its drive is held.
    for _ in 0..12 {
        if world
            .drives()
            .asked()
            .iter()
            .any(|asked| asked == next.as_str())
        {
            break;
        }
        world.tick().await?;
        world.quiesce().await;
    }
    if !world
        .drives()
        .asked()
        .iter()
        .any(|asked| asked == next.as_str())
    {
        violations.push(format!("the relay never asked `{}`", next.as_str()));
    }
    // The waiter follows the re-asked drive while it cannot yet run.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !world
        .drives()
        .awaited()
        .iter()
        .any(|awaited| awaited == next.as_str())
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let awaited = world.drives().awaited();
    if !awaited.iter().any(|request| request == next.as_str()) {
        violations.push(format!(
            "the waiter never followed the relay's ask `{}` past the lost `{}`: it awaited \
             {awaited:?}; the engine accepted the asks {:?}",
            next.as_str(),
            first.as_str(),
            world.drives().asked()
        ));
    }
    hold.release();
    for _ in 0..6 {
        world.quiesce().await;
        if waiter.is_finished() {
            break;
        }
        world.tick().await?;
    }
    match tokio::time::timeout(std::time::Duration::from_secs(30), waiter).await {
        Ok(Ok(Ok(outcome))) if matches!(outcome.status, lash::TurnStatus::Answered) => {}
        Ok(Ok(Ok(outcome))) => violations.push(format!(
            "the waiter answered {:?}, not Answered",
            outcome.status
        )),
        Ok(Ok(Err(error))) => violations.push(format!("the waiter failed: {error}")),
        Ok(Err(error)) => violations.push(format!("the waiter's task died: {error}")),
        Err(_) => violations.push("the waiter never answered".to_owned()),
    }
    world.finish().await;
    Ok(violations)
}
