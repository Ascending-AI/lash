//! The end state an epoch's ledger owes: the crash matrix's invariants
//! ([`invariants::check`]) over what the host saw admitted, and the soak's
//! own checks for what the matrix does not read — admissions the host never
//! saw answered, held roots, every waiter, and every retired generation.

use std::sync::Arc;

use lash_core::store::{ObligationKind, ObligationState, scope_close_obligation_id};
use lash_core::{ScopeId, SessionId, TurnId};

use super::driver::{Admission, Ledger, Retired};
use crate::crash_matrix::invariants::{
    self, AcceptedInput, CustomCheck, Expected, answer_text, input_text,
};
use crate::crash_matrix::world::CrashWorld;

const PAGE: std::num::NonZeroUsize = std::num::NonZeroUsize::MIN.saturating_add(255);

/// What the end state owes a session the host asked to delete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeleteFate {
    /// The host saw the delete answered, or the close it asked for
    /// committed before the host died: the session must be gone.
    Owed,
    /// The delete never took: the session owes what a live one does.
    Live,
    /// The session's create never committed (its host died inside the
    /// open), so a delete had nothing to delete.
    NeverCreated,
}

async fn delete_fate(
    world: &CrashWorld,
    session: &SessionId,
    admission: Admission,
) -> Result<DeleteFate, String> {
    let factory = world.backend().session_store_factory();
    if factory.session_was_deleted(session).await? {
        return Ok(DeleteFate::Owed);
    }
    if never_created(world, session).await? {
        return Ok(DeleteFate::NeverCreated);
    }
    if admission == Admission::Known {
        return Ok(DeleteFate::Owed);
    }
    let intents = factory
        .list_control_intents(None, PAGE)
        .await
        .map_err(|error| format!("list control intents: {error}"))?;
    let closing = intents.iter().any(|intent| {
        intent.session_id == *session
            && matches!(
                intent.kind,
                lash_core::store::ControlIntentKind::CloseSession { .. }
            )
    });
    Ok(if closing {
        DeleteFate::Owed
    } else {
        DeleteFate::Live
    })
}

/// Whether the catalog never held `session`: a session that is not deleted
/// and that the facade does not know.
async fn never_created(world: &CrashWorld, session: &SessionId) -> Result<bool, String> {
    let core = world.core()?;
    let queued = match core.session(session.clone()).durable().await {
        Ok(durable) => durable.queued_work().await.map(|_| ()),
        Err(error) => Err(error),
    };
    match queued {
        Ok(()) => Ok(false),
        Err(lash::EmbedError::UnknownSession { .. }) => Ok(true),
        Err(error) => Err(format!("read the queued work of `{session}`: {error}")),
    }
}

/// The epoch's end state, resolved against the stores: which sessions owe
/// their deletion, and what the rest owe.
pub(super) async fn expected(world: &CrashWorld, ledger: &Ledger) -> Result<Expected, String> {
    let mut deleted = Vec::new();
    let mut live = Vec::new();
    for slot in &ledger.sessions {
        let fate = match slot.deleted {
            Some(admission) => delete_fate(world, &slot.id, admission).await?,
            None => DeleteFate::Live,
        };
        match fate {
            DeleteFate::Owed => deleted.push(slot.id.clone()),
            DeleteFate::Live => live.push(slot.id.clone()),
            DeleteFate::NeverCreated => {}
        }
    }
    let known: Vec<AcceptedInput> = ledger
        .inputs
        .iter()
        .filter(|input| input.admission == Admission::Known && live.contains(&input.session))
        .map(|input| AcceptedInput {
            session: input.session.clone(),
            root: TurnId::from(input.root.as_str()),
        })
        .collect();
    let mut closed_scopes: Vec<ScopeId> = ledger.child_scopes.clone();
    for session in &deleted {
        closed_scopes.push(ScopeId::session(session.clone()));
    }
    let custom: Vec<(&'static str, CustomCheck)> = vec![
        ("at_most_once", at_most_once(ledger, &live)),
        ("ingress", no_open_ingress(live.clone())),
        ("commands", no_queued_work(live.clone())),
        ("scope_close", roots_closed(ledger, &live)),
        ("waiters", waiters_served(ledger)),
        (
            "generations",
            generations_hold_nothing(ledger.retired.clone()),
        ),
    ];
    Ok(Expected {
        inputs: known,
        children: ledger.children.clone(),
        closed_scopes,
        deleted_sessions: deleted.clone(),
        closed_sessions: deleted,
        live_sessions: live,
        custom,
    })
}

/// How often `marker` appears in the `role` messages of `messages`.
fn count(messages: &[(String, String)], role: &str, marker: &str) -> usize {
    messages
        .iter()
        .filter(|(message_role, _)| message_role == role)
        .map(|(_, text)| text.matches(marker).count())
        .sum()
}

/// Admissions the host never saw answered, and held roots: each committed at
/// most once; an unanswered-admission input committed is answered once, and
/// a held root is never answered.
fn at_most_once(ledger: &Ledger, live: &[SessionId]) -> CustomCheck {
    let maybe: Vec<(SessionId, String, bool)> = ledger
        .inputs
        .iter()
        .filter(|input| input.admission == Admission::Maybe && live.contains(&input.session))
        .map(|input| (input.session.clone(), input.root.clone(), false))
        .chain(
            ledger
                .held
                .iter()
                .filter(|held| live.contains(&held.session))
                .map(|held| (held.session.clone(), held.root.clone(), true)),
        )
        .collect();
    Arc::new(move |world: &CrashWorld| {
        let maybe = maybe.clone();
        Box::pin(async move {
            let mut violations = Vec::new();
            for (session, root, held) in maybe {
                let messages = match invariants::transcript(world, &session).await {
                    Ok(messages) => messages,
                    Err(error) => {
                        violations.push(error);
                        continue;
                    }
                };
                let asked = count(&messages, "user", &input_text(&root));
                let answered = count(&messages, "assistant", &answer_text(&root));
                let owed = if held { 0 } else { asked };
                if asked > 1 || answered != owed {
                    violations.push(format!(
                        "{} `{root}` of `{session}` committed {asked} time(s) and was answered {answered} time(s)",
                        if held { "held root" } else { "input the host never saw accepted" }
                    ));
                }
            }
            violations
        })
    })
}

/// No live session holds an open ingress row nothing drove.
fn no_open_ingress(live: Vec<SessionId>) -> CustomCheck {
    Arc::new(move |world: &CrashWorld| {
        let live = live.clone();
        Box::pin(async move {
            let factory = world.backend().session_store_factory();
            let mut violations = Vec::new();
            for session in live {
                match factory.open_existing_store_by_id(&session).await {
                    Ok(Some(store)) => match store.list_pending_turn_inputs(&session).await {
                        Ok(pending) if pending.is_empty() => {}
                        Ok(pending) => violations.push(format!(
                            "`{session}` holds {} open ingress row(s) nothing drove: {:?}",
                            pending.len(),
                            pending
                                .iter()
                                .map(|read| read.input.input_id.to_string())
                                .collect::<Vec<_>>()
                        )),
                        Err(error) => {
                            violations.push(format!("list open ingress of `{session}`: {error}"));
                        }
                    },
                    // A session whose open never committed has nothing open.
                    Ok(None) => {}
                    Err(error) => violations.push(format!("open `{session}`: {error}")),
                }
            }
            violations
        })
    })
}

/// No live session holds queued work — a session command, a process wake —
/// that nothing settled.
fn no_queued_work(live: Vec<SessionId>) -> CustomCheck {
    Arc::new(move |world: &CrashWorld| {
        let live = live.clone();
        Box::pin(async move {
            let core = match world.core() {
                Ok(core) => core,
                Err(error) => return vec![error],
            };
            let mut violations = Vec::new();
            for session in live {
                let queued = match core.session(session.clone()).durable().await {
                    Ok(durable) => durable.queued_work().await,
                    Err(error) => Err(error),
                };
                match queued {
                    Ok(batches) if batches.is_empty() => {}
                    Ok(batches) => violations.push(format!(
                        "`{session}` holds {} queued-work batch(es) nothing settled: {:?}",
                        batches.len(),
                        batches
                            .iter()
                            .map(|batch| format!("{} {:?}", batch.batch_id, batch.kind))
                            .collect::<Vec<_>>()
                    )),
                    // A session whose create never committed holds nothing.
                    Err(lash::EmbedError::UnknownSession { .. }) => {}
                    Err(error) => {
                        violations.push(format!("read the queued work of `{session}`: {error}"));
                    }
                }
            }
            violations
        })
    })
}

/// Every root with terminal evidence owes its scope close as an ADR 0109
/// obligation on its root row: delivered, or stalled typed.
fn roots_closed(ledger: &Ledger, live: &[SessionId]) -> CustomCheck {
    let roots: Vec<(SessionId, String)> = ledger
        .inputs
        .iter()
        .map(|input| (input.session.clone(), input.root.clone()))
        .chain(
            ledger
                .held
                .iter()
                .map(|held| (held.session.clone(), held.root.clone())),
        )
        .filter(|(session, _)| live.contains(session))
        .collect();
    Arc::new(move |world: &CrashWorld| {
        let roots = roots.clone();
        Box::pin(async move {
            let factory = world.backend().session_store_factory();
            let ledger = world
                .backend()
                .obligation_ledger(ObligationKind::ScopeClose);
            let mut violations = Vec::new();
            for (session, root) in roots {
                let turn = TurnId::from(root.as_str());
                match factory.root_terminal(&session, &turn).await {
                    Ok(Some(_)) => {
                        let id = scope_close_obligation_id(&session, &turn);
                        match ledger.state(&id).await {
                            Ok(Some(ObligationState::Delivered | ObligationState::Stalled)) => {}
                            Ok(state) => violations.push(format!(
                                "the scope-close obligation of terminal root `{root}` of `{session}` is {state:?}"
                            )),
                            Err(error) => violations
                                .push(format!("read the scope close of `{root}`: {error}")),
                        }
                    }
                    // A batched input owns no root.
                    Ok(None) => {}
                    Err(error) => {
                        violations.push(format!("read the terminal of `{root}`: {error}"))
                    }
                }
            }
            violations
        })
    })
}

/// Every process ended and its engine waiter was answered by the terminal;
/// no process the stores hold is left unfinished but the held roots'
/// children, which only their cancel ends (the no-orphan invariant reads
/// those).
fn waiters_served(ledger: &Ledger) -> CustomCheck {
    let processes = ledger.processes.clone();
    let children: Vec<lash_core::ProcessId> = ledger
        .children
        .iter()
        .map(|child| child.child.clone())
        .collect();
    Arc::new(move |world: &CrashWorld| {
        let processes = processes.clone();
        let children = children.clone();
        Box::pin(async move {
            let mut violations = Vec::new();
            for started in processes {
                let check = crate::crash_matrix::cases::process::waiter_completed(
                    started.waiter.clone(),
                    started.process.clone(),
                );
                violations.extend(check(world).await);
            }
            match world
                .backend()
                .process_registry()
                .list_processes(&lash_core::ProcessListFilter {
                    status: lash_core::ProcessStatusFilter::Any,
                    ..lash_core::ProcessListFilter::default()
                })
                .await
            {
                Ok(records) => violations.extend(
                    records
                        .into_iter()
                        .filter(|record| !record.is_terminal() && !children.contains(&record.id))
                        .map(|record| {
                            let Ok(double) = world.double() else {
                                return format!("process `{}` never finished", record.id);
                            };
                            let engine: Vec<String> = double
                                .server()
                                .invocations()
                                .into_iter()
                                .filter(|view| view.target.contains(record.id.as_str()))
                                .map(|view| {
                                    format!(
                                        "{} {} {:?}",
                                        view.target,
                                        view.status,
                                        double.server().outcome(&view.id).map(|outcome| {
                                            outcome.map(|bytes| {
                                                String::from_utf8_lossy(&bytes)
                                                    .chars()
                                                    .take(300)
                                                    .collect::<String>()
                                            })
                                        })
                                    )
                                })
                                .collect();
                            format!(
                                "process `{}` is {:?}, first started {:?}, external ref {:?}: nothing finished it; its engine invocations: {engine:?}",
                                record.id, record.status, record.first_started, record.external_ref
                            )
                        }),
                ),
                Err(error) => violations.push(format!("list processes: {error}")),
            }
            violations
        })
    })
}

/// Every generation a rolling deploy retired holds nothing: no live or
/// parked process, no parked turn, and no open invocation of its build.
fn generations_hold_nothing(retired: Vec<Retired>) -> CustomCheck {
    Arc::new(move |world: &CrashWorld| {
        let retired = retired.clone();
        Box::pin(async move {
            let mut violations = Vec::new();
            let drain = world.backend().generation_drain();
            for Retired {
                generation,
                deployment,
            } in retired
            {
                match drain.generation_work(&generation).await {
                    Ok(work)
                        if work
                            == lash_core::store::generation_drain::GenerationWork::default() => {}
                    Ok(work) => violations.push(format!(
                        "retired generation `{generation}` still holds {work:?}"
                    )),
                    Err(error) => violations.push(format!("read `{generation}`'s work: {error}")),
                }
                let pinned = super::driver::pinned_open(world, &deployment);
                if !pinned.is_empty() {
                    violations.push(format!(
                        "retired generation `{generation}`'s build still pins {pinned:?}"
                    ));
                }
                if world
                    .double()
                    .is_ok_and(|double| double.server().deployments().contains(&deployment))
                {
                    violations.push(format!(
                        "retired generation `{generation}`'s build is still registered"
                    ));
                }
            }
            violations
        })
    })
}

/// Every stalled obligation, described: a stall is typed and passes the
/// settled-or-stalled invariant, but the soak injects no refusal, so the
/// report names each one.
pub(super) async fn stalls(world: &CrashWorld) -> Vec<String> {
    let mut lines = Vec::new();
    for kind in ObligationKind::ALL {
        match world
            .backend()
            .obligation_ledger(kind)
            .list_stalled(None, PAGE)
            .await
        {
            Ok(stalled) => lines.extend(stalled.into_iter().map(|stall| {
                format!(
                    "stalled {}: {} {:?} attempts={} {:?}",
                    kind.label(),
                    stall.id,
                    stall.reason,
                    stall.attempts,
                    stall.last_error
                )
            })),
            Err(error) => lines.push(format!("stalled {} unreadable: {error}", kind.label())),
        }
    }
    lines
}

/// What recovery itself looks like, for a failed epoch: the recovery lease's
/// row (a lowest-rank probe reads it, and gives back a lease nobody held),
/// against the wall time the live deployment came up, and every turn park.
pub(super) async fn diagnose_recovery(world: &CrashWorld, live_since_wall_ms: i64) -> Vec<String> {
    let mut lines = Vec::new();
    let backend = world.backend();
    let store = backend.recovery_leader();
    let name = lash_core::store::LeaseName::new(format!(
        "recovery:{}",
        backend.effect_host().turn_control_binding_id()
    ));
    let probe = lash_core::store::LeaseClaim {
        name: name.clone(),
        holder: lash_core::store::HolderId::new("chaos-soak-probe"),
        generation_rank: i64::MIN,
        ttl_ms: 1_000,
        min_tenure_ms: 0,
    };
    match store.acquire(&probe).await {
        Ok(answer) if answer.leader => {
            lines.push(format!(
                "recovery lease: NO deployment led recovery; the probe took {:?}",
                answer.row
            ));
            if let Some(row) = answer.row {
                let _ = store.resign(&name, &probe.holder, row.term).await;
            }
        }
        Ok(answer) => lines.push(format!(
            "recovery lease: held by {:?} at db time {}; the live deployment came up at {live_since_wall_ms}",
            answer.row, answer.db_now_ms
        )),
        Err(error) => lines.push(format!("recovery lease unreadable: {error}")),
    }
    match backend
        .session_store_factory()
        .list_turn_parks(&lash_core::store::TurnParkQuery {
            reasons: None,
            session: None,
            parked_at_or_before_ms: None,
            after: None,
            limit: PAGE,
        })
        .await
    {
        Ok(parks) => lines.extend(parks.into_iter().map(|park| {
            format!(
                "turn park: `{}` root `{}` {:?} attempts={} engine={:?}",
                park.session_id, park.turn_id, park.reason, park.attempts, park.engine
            )
        })),
        Err(error) => lines.push(format!("turn parks unreadable: {error}")),
    }
    lines
}
