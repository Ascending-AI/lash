//! The end-state invariants every crash-matrix case must reach:
//!
//! 1. **Exactly once.** Every accepted input was driven exactly once: its root
//!    has terminal evidence, the committed transcript carries its user
//!    message once and the answer to it once, and no ingress row is left
//!    open. No loss, no double root.
//! 2. **Settled or stalled.** Every obligation is settled, or stalled with a
//!    typed reason: each [`ObligationProbe`] reports nothing unsettled.
//! 3. **No orphan.** No child process outlives its ended parent scope
//!    uncancelled.
//! 4. **Scopes closed.** Every terminal root the case names has its scope
//!    closed: its parent-end plan is recorded and settled, and its
//!    scope-close obligation (ADR 0109 §3) is delivered or stalled typed.
//! 5. **Deleted.** Every session the host asked to delete is deleted.
//! 6. **Not wedged.** No lash drive of a live session is paused or left
//!    running, and a fresh input sent after recovery is driven
//!    ([`probe_live_sessions`]).
//! 7. **Detected in bound.** The first tick at which 1–6 hold comes within
//!    the cell's ADR 0109 §1.8 bound, in sim time (checked by the case
//!    runner).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use lash_core::store::{ObligationKind, ObligationState, scope_close_obligation_id};
use lash_core::{ProcessId, ScopeId, SessionId, TurnId};

use super::world::CrashWorld;

const PAGE: std::num::NonZeroUsize = std::num::NonZeroUsize::MIN.saturating_add(255);

/// The text of the input a case sends as root `root`. The token is
/// terminated, so inputs a claim batches into one user message stay
/// countable one by one.
#[must_use]
pub fn input_text(root: &str) -> String {
    format!("input:{root};")
}

/// The answer the scripted model gives for [`input_text`]`(root)` in the
/// user message it answers.
#[must_use]
pub fn answer_text(root: &str) -> String {
    format!("answer:{root};")
}

/// The roots named by every [`input_text`] token in `text`, in order.
#[must_use]
pub fn input_roots(text: &str) -> Vec<String> {
    text.match_indices("input:")
        .filter_map(|(at, _)| {
            let rest = &text[at + "input:".len()..];
            let end = rest.find(';')?;
            let root = &rest[..end];
            root.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                .then(|| root.to_owned())
        })
        .collect()
}

/// An input the host saw accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedInput {
    pub session: SessionId,
    pub root: TurnId,
}

/// A child process registered to live until `parent` ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildOf {
    pub child: ProcessId,
    pub parent: ScopeId,
}

/// A check a case adds for a seam the common invariants do not read.
pub type CustomCheck = Arc<
    dyn for<'a> Fn(&'a CrashWorld) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>>
        + Send
        + Sync,
>;

/// What a case expects of the end state.
#[derive(Clone, Default)]
pub struct Expected {
    pub inputs: Vec<AcceptedInput>,
    pub children: Vec<ChildOf>,
    pub closed_scopes: Vec<ScopeId>,
    pub deleted_sessions: Vec<SessionId>,
    /// Sessions the host closed: no drive of theirs may stay live.
    pub closed_sessions: Vec<SessionId>,
    pub live_sessions: Vec<SessionId>,
    pub custom: Vec<(&'static str, CustomCheck)>,
}

impl std::fmt::Debug for Expected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Expected")
            .field("inputs", &self.inputs)
            .field("children", &self.children)
            .field("closed_scopes", &self.closed_scopes)
            .field("deleted_sessions", &self.deleted_sessions)
            .field("closed_sessions", &self.closed_sessions)
            .field("live_sessions", &self.live_sessions)
            .finish_non_exhaustive()
    }
}

/// One obligation ledger the settled-or-stalled invariant reads.
///
/// Today's probes read control intents' obligations, in-flight turns and the
/// parent-end plans' `ParentEnd` obligations. An S8 slice that arms an ADR 0109
/// obligation on its ledger adds a probe here that reads its
/// `ObligationLedger`: a `due` or `claimed` row is unsettled, a `delivered`
/// row settled, and a `stalled` row with its `StallReason` is a typed stall
/// (see [`obligation_probes`]).
#[async_trait::async_trait]
pub trait ObligationProbe: Send + Sync {
    /// The ADR 0109 §1.2 kind label this probe reads.
    fn kind(&self) -> &'static str;

    /// Every obligation of this kind that is neither settled nor stalled
    /// with a typed reason, described. `expected` names the rows a ledger
    /// that has no cross-row read keys its obligations by.
    async fn unsettled(
        &self,
        world: &CrashWorld,
        expected: &Expected,
    ) -> Result<Vec<String>, String>;
}

/// Control intents whose ADR 0109 `ControlIntent` obligation is neither
/// delivered nor stalled with a typed reason: `due` or `claimed`, or never
/// armed at all.
struct ControlIntentProbe;

#[async_trait::async_trait]
impl ObligationProbe for ControlIntentProbe {
    fn kind(&self) -> &'static str {
        "control_intent"
    }

    async fn unsettled(
        &self,
        world: &CrashWorld,
        _expected: &Expected,
    ) -> Result<Vec<String>, String> {
        let intents = world
            .backend()
            .session_store_factory()
            .list_control_intents(None, PAGE)
            .await
            .map_err(|error| format!("list control intents: {error}"))?;
        let ledger = world
            .backend()
            .obligation_ledger(lash_core::store::ObligationKind::ControlIntent);
        let mut unsettled = Vec::new();
        for intent in intents {
            let Some(obligation) = intent.obligation.as_ref() else {
                unsettled.push(format!(
                    "control intent {} of `{}` has no obligation: {:?}",
                    intent.id, intent.session_id, intent.state
                ));
                continue;
            };
            let state = ledger
                .state(obligation)
                .await
                .map_err(|error| format!("read obligation {obligation}: {error}"))?;
            if !matches!(
                state,
                Some(
                    lash_core::store::ObligationState::Delivered
                        | lash_core::store::ObligationState::Stalled
                )
            ) {
                unsettled.push(format!(
                    "control intent {} of `{}` is {:?} with its obligation {state:?}",
                    intent.id, intent.session_id, intent.state
                ));
            }
        }
        Ok(unsettled)
    }
}

/// Parent-end plans whose `ParentEnd` obligation (ADR 0109 §3) is still
/// due or claimed. The ledger is read by key, so the probe reads the plan of
/// every scope the case names: the scopes it expects closed and the parents
/// of its children. A `stalled` plan is typed (a refused child stalls its
/// plan, never the rows behind it); a recorded plan that owes no obligation
/// is only settled when it is stamped settled.
struct ParentEndPlanProbe;

#[async_trait::async_trait]
impl ObligationProbe for ParentEndPlanProbe {
    fn kind(&self) -> &'static str {
        "parent_end"
    }

    async fn unsettled(
        &self,
        world: &CrashWorld,
        expected: &Expected,
    ) -> Result<Vec<String>, String> {
        let registry = world.backend().process_registry();
        let mut scopes: Vec<&ScopeId> = expected
            .closed_scopes
            .iter()
            .chain(expected.children.iter().map(|child| &child.parent))
            .collect();
        scopes.sort_by_key(|scope| scope.storage_id());
        scopes.dedup();
        let mut unsettled = Vec::new();
        for scope in scopes {
            let plan = registry
                .get_parent_end_plan(scope)
                .await
                .map_err(|error| format!("read the parent-end plan of `{scope}`: {error}"))?;
            let Some(plan) = plan else { continue };
            match plan.obligation_state {
                Some(ObligationState::Delivered | ObligationState::Stalled) => {}
                None if plan.settled_at_ms.is_some() => {}
                state => unsettled.push(format!(
                    "parent-end plan of `{scope}` (ended at {}) is unsettled: obligation {:?} is {state:?}",
                    plan.ended_at_ms, plan.obligation_id
                )),
            }
        }
        Ok(unsettled)
    }
}

/// Turns in flight that no park holds: claimed inputs or a pending queued
/// run. A parked turn is stalled with its park reason, typed.
struct TurnProbe;

#[async_trait::async_trait]
impl ObligationProbe for TurnProbe {
    fn kind(&self) -> &'static str {
        "ingress"
    }

    async fn unsettled(
        &self,
        world: &CrashWorld,
        _expected: &Expected,
    ) -> Result<Vec<String>, String> {
        let counts = world
            .backend()
            .session_store_factory()
            .count_unsettled_turns()
            .await
            .map_err(|error| format!("count unsettled turns: {error}"))?;
        let unparked = counts.in_flight_turns.saturating_sub(counts.parked_turns);
        Ok(if unparked == 0 {
            Vec::new()
        } else {
            vec![format!(
                "{unparked} session(s) hold a turn in flight that no park accounts for: {counts:?}"
            )]
        })
    }
}

/// `SessionDelete` obligations (S8-D) that are due or claimed. The ledger
/// arms one only when a `CloseSession` intent is acknowledged, so the closed
/// sessions are the ones the intents name; a deleted session's obligation
/// left with its row, and a stalled one carries its reason, typed.
struct SessionDeleteProbe;

#[async_trait::async_trait]
impl ObligationProbe for SessionDeleteProbe {
    fn kind(&self) -> &'static str {
        "session_delete"
    }

    async fn unsettled(
        &self,
        world: &CrashWorld,
        _expected: &Expected,
    ) -> Result<Vec<String>, String> {
        let mut closed: Vec<SessionId> = world
            .backend()
            .session_store_factory()
            .list_control_intents(None, PAGE)
            .await
            .map_err(|error| format!("list control intents: {error}"))?
            .into_iter()
            .filter(|intent| {
                matches!(
                    intent.kind,
                    lash_core::store::ControlIntentKind::CloseSession { .. }
                )
            })
            .map(|intent| intent.session_id)
            .collect();
        closed.sort();
        closed.dedup();
        let ledger = world.backend().session_delete_ledger();
        let mut unsettled = Vec::new();
        for session in closed {
            let obligation = ledger
                .delete_obligation(&session)
                .await
                .map_err(|error| format!("read the delete obligation of `{session}`: {error}"))?;
            if let Some(obligation) = obligation
                && matches!(
                    obligation.state,
                    lash_core::store::ObligationState::Due
                        | lash_core::store::ObligationState::Claimed
                )
            {
                unsettled.push(format!(
                    "the delete obligation {} of `{session}` is {:?}",
                    obligation.id, obligation.state
                ));
            }
        }
        Ok(unsettled)
    }
}

/// Terminal processes whose terminal publication (ADR 0109 §3,
/// `ProcessTerminal`) is still `due` or `claimed`: an engine waiter on their
/// terminal promise may still be stranded. A `stalled` publication is typed.
struct ProcessTerminalProbe;

#[async_trait::async_trait]
impl ObligationProbe for ProcessTerminalProbe {
    fn kind(&self) -> &'static str {
        "process_terminal"
    }

    async fn unsettled(
        &self,
        world: &CrashWorld,
        _expected: &Expected,
    ) -> Result<Vec<String>, String> {
        let registry = world.backend().process_registry();
        let processes = registry
            .list_processes(&lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            })
            .await
            .map_err(|error| format!("list processes: {error}"))?;
        let mut unsettled = Vec::new();
        for record in processes.iter().filter(|record| record.is_terminal()) {
            let publication = registry
                .terminal_publication(&record.id)
                .await
                .map_err(|error| format!("read `{}`'s terminal publication: {error}", record.id))?;
            match publication {
                Some(publication)
                    if matches!(
                        publication.state,
                        lash_core::store::ObligationState::Due
                            | lash_core::store::ObligationState::Claimed
                    ) =>
                {
                    unsettled.push(format!(
                        "process `{}`'s terminal publication {} is {:?}",
                        record.id, publication.id, publication.state
                    ));
                }
                Some(_) => {}
                None => unsettled.push(format!(
                    "terminal process `{}` owes no terminal publication: its terminal transaction armed none",
                    record.id
                )),
            }
        }
        Ok(unsettled)
    }
}

/// The probes the settled-or-stalled invariant reads. An S8 slice lists its
/// ledger's probe here when it lands.
#[must_use]
pub fn obligation_probes() -> Vec<Box<dyn ObligationProbe>> {
    vec![
        Box::new(TurnProbe),
        Box::new(ControlIntentProbe),
        Box::new(ParentEndPlanProbe),
        Box::new(SessionDeleteProbe),
        Box::new(ProcessTerminalProbe),
    ]
}

/// The role and the text of every committed message of `session`.
async fn transcript(
    world: &CrashWorld,
    session: &SessionId,
) -> Result<Vec<(String, String)>, String> {
    // A session that never committed has no transcript yet: nothing of it
    // was driven.
    let Some(view) = world
        .backend()
        .session_store_factory()
        .read_session(session)
        .await
        .map_err(|error| format!("read session `{session}`: {error}"))?
    else {
        return Ok(Vec::new());
    };
    view.messages()
        .iter()
        .map(|message| {
            let value = serde_json::to_value(message)
                .map_err(|error| format!("encode a message of `{session}`: {error}"))?;
            let role = value
                .get("role")
                .map(|role| role.to_string().trim_matches('"').to_ascii_lowercase())
                .unwrap_or_default();
            Ok((role, value.to_string()))
        })
        .collect()
}

/// How often the terminated token `marker` appears in `text`.
fn mentions(text: &str, marker: &str) -> usize {
    text.matches(marker).count()
}

async fn check_inputs(world: &CrashWorld, expected: &Expected, violations: &mut Vec<String>) {
    let factory = world.backend().session_store_factory();
    for input in &expected.inputs {
        if expected.deleted_sessions.contains(&input.session) {
            continue;
        }
        let root = input.root.as_str();
        // The input's own root is evidence for the report, not the
        // invariant: a claim batches inputs accepted before it ran into the
        // head's root, so a batched input never gets a root of its own.
        let terminal = factory
            .root_terminal(&input.session, &input.root)
            .await
            .ok()
            .flatten();
        match transcript(world, &input.session).await {
            Ok(messages) => {
                let asked: usize = messages
                    .iter()
                    .filter(|(role, _)| role == "user")
                    .map(|(_, text)| mentions(text, &input_text(root)))
                    .sum();
                let answered: usize = messages
                    .iter()
                    .filter(|(role, _)| role == "assistant")
                    .map(|(_, text)| mentions(text, &answer_text(root)))
                    .sum();
                if asked != 1 || answered != 1 {
                    violations.push(format!(
                        "input `{root}` of `{}` committed {asked} time(s) and was answered {answered} time(s); exactly once each is required (terminal: {:?}; transcript: {:?})",
                        input.session,
                        terminal.as_ref().map(|terminal| (&terminal.kind, &terminal.cause)),
                        messages
                            .iter()
                            .map(|(role, text)| format!("{role}: {}", text.chars().take(2000).collect::<String>()))
                            .collect::<Vec<_>>()
                    ));
                }
            }
            Err(error) => violations.push(error),
        }
    }
    let mut sessions: Vec<&SessionId> = expected
        .inputs
        .iter()
        .map(|input| &input.session)
        .filter(|session| !expected.deleted_sessions.contains(session))
        .collect();
    sessions.sort();
    sessions.dedup();
    for session in sessions {
        match factory.open_existing_store_by_id(session).await {
            Ok(Some(store)) => match store.list_pending_turn_inputs(session).await {
                Ok(pending) if pending.is_empty() => {}
                Ok(pending) => violations.push(format!(
                    "`{session}` holds {} open ingress row(s) nothing drove: {:?}",
                    pending.len(),
                    pending
                        .iter()
                        .map(|read| read.input.input_id.to_string())
                        .collect::<Vec<_>>()
                )),
                Err(error) => violations.push(format!("list open ingress of `{session}`: {error}")),
            },
            Ok(None) => violations.push(format!("`{session}` has no store")),
            Err(error) => violations.push(format!("open `{session}`: {error}")),
        }
    }
}

async fn check_children(world: &CrashWorld, expected: &Expected, violations: &mut Vec<String>) {
    let registry = world.backend().process_registry();
    for ChildOf { child, parent } in &expected.children {
        let ended = match registry.get_parent_end_plan(parent).await {
            Ok(plan) => plan.is_some(),
            Err(error) => {
                violations.push(format!("read the plan of `{parent}`: {error}"));
                continue;
            }
        };
        if !ended {
            continue;
        }
        match registry.get_process(child).await {
            Ok(Some(record)) => {
                if !record.is_terminal() && record.cancel_request.is_none() {
                    violations.push(format!(
                        "child `{child}` of ended `{parent}` is orphaned: {:?} with no cancel request",
                        record.status
                    ));
                }
            }
            Ok(None) => {}
            Err(error) => violations.push(format!("read child `{child}`: {error}")),
        }
    }
}

async fn check_scopes(world: &CrashWorld, expected: &Expected, violations: &mut Vec<String>) {
    let registry = world.backend().process_registry();
    let scope_closes = world
        .backend()
        .obligation_ledger(ObligationKind::ScopeClose);
    let factory = world.backend().session_store_factory();
    for scope in &expected.closed_scopes {
        // A root with terminal evidence owes its scope close as an ADR 0109
        // obligation on its root row: it must be delivered, or stalled typed.
        if let Some(lash_core::EffectOpener::Turn {
            session_id,
            turn_id,
        }) = scope.opener()
        {
            match factory.root_terminal(session_id, turn_id).await {
                Ok(Some(_)) => {
                    let id = scope_close_obligation_id(session_id, turn_id);
                    match scope_closes.state(&id).await {
                        Ok(Some(ObligationState::Delivered | ObligationState::Stalled)) => {}
                        // A deleted session's rows are gone with their
                        // obligations; its physical delete waited for every
                        // scope close it owed (ADR 0109 §4).
                        Ok(None) if factory.session_was_deleted(session_id).await == Ok(true) => {}
                        Ok(state) => violations.push(format!(
                            "the scope-close obligation `{id}` of terminal root `{scope}` is {state:?}, neither delivered nor stalled"
                        )),
                        Err(error) => {
                            violations.push(format!("read the scope-close obligation `{id}`: {error}"));
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => violations.push(format!("read the terminal of `{scope}`: {error}")),
            }
        }
        match registry.get_parent_end_plan(scope).await {
            Ok(Some(plan)) if plan.settled_at_ms.is_some() => {}
            Ok(Some(plan)) => violations.push(format!(
                "the scope of `{scope}` closed at {} but its plan never settled",
                plan.ended_at_ms
            )),
            Ok(None) => violations.push(format!("the scope of `{scope}` never closed")),
            Err(error) => violations.push(format!("read the plan of `{scope}`: {error}")),
        }
    }
}

async fn check_deletions(world: &CrashWorld, expected: &Expected, violations: &mut Vec<String>) {
    let factory = world.backend().session_store_factory();
    for session in &expected.deleted_sessions {
        match factory.session_was_deleted(session).await {
            Ok(true) => {}
            Ok(false) => violations.push(format!(
                "the host asked to delete `{session}` and it is not deleted"
            )),
            Err(error) => violations.push(format!("read the deletion of `{session}`: {error}")),
        }
    }
}

/// No lash drive of a live session is paused, backing off, or running.
fn check_engine(world: &CrashWorld, expected: &Expected, violations: &mut Vec<String>) {
    for view in world.server().invocations() {
        let lash_drive =
            view.target.starts_with("LashSession/") || view.target.starts_with("LashTurn/");
        if !lash_drive || view.status == "completed" || view.status == "suspended" {
            continue;
        }
        if expected
            .live_sessions
            .iter()
            .chain(expected.deleted_sessions.iter())
            .chain(expected.closed_sessions.iter())
            .any(|session| view.target.contains(session.as_str()))
        {
            violations.push(format!(
                "engine drive {} is {} after {} attempt(s); last failure {:?}",
                view.target, view.status, view.attempts, view.last_failure
            ));
        }
    }
}

/// Every violation of invariants 1–6 in the world's current state. Call it
/// at quiescence.
pub async fn check(world: &CrashWorld, expected: &Expected) -> Vec<String> {
    let mut violations = Vec::new();
    check_inputs(world, expected, &mut violations).await;
    for probe in obligation_probes() {
        match probe.unsettled(world, expected).await {
            Ok(unsettled) => violations.extend(
                unsettled
                    .into_iter()
                    .map(|entry| format!("[{}] {entry}", probe.kind())),
            ),
            Err(error) => violations.push(format!("[{}] {error}", probe.kind())),
        }
    }
    check_children(world, expected, &mut violations).await;
    check_scopes(world, expected, &mut violations).await;
    check_deletions(world, expected, &mut violations).await;
    check_engine(world, expected, &mut violations);
    for (name, custom) in &expected.custom {
        violations.extend(
            custom(world)
                .await
                .into_iter()
                .map(|entry| format!("[{name}] {entry}")),
        );
    }
    violations
}

/// Invariant 6's second half: each live session drives a fresh input sent
/// after recovery, within `ticks` recovery ticks. Returns the violations.
pub async fn probe_live_sessions(
    world: &CrashWorld,
    expected: &Expected,
    ticks: usize,
) -> Vec<String> {
    let mut violations = Vec::new();
    for session in &expected.live_sessions {
        let root = format!("probe-{}", world.seed() % 100_000);
        if let Err(error) = super::cases::send(world, session, &root).await {
            violations.push(format!(
                "`{session}` refused a fresh input after recovery: {error}"
            ));
            continue;
        }
        let probe = Expected {
            inputs: vec![AcceptedInput {
                session: session.clone(),
                root: TurnId::from(root.as_str()),
            }],
            ..Expected::default()
        };
        let mut answered = false;
        for tick in 0..=ticks {
            world.quiesce().await;
            let mut probe_violations = Vec::new();
            check_inputs(world, &probe, &mut probe_violations).await;
            if probe_violations.is_empty() {
                answered = true;
                break;
            }
            if tick < ticks
                && let Err(error) = world.tick().await
            {
                violations.push(error);
                break;
            }
        }
        if !answered {
            let mut last = Vec::new();
            check_inputs(world, &probe, &mut last).await;
            violations.push(format!(
                "`{session}` is wedged: a fresh input sent after recovery was not driven within {ticks} tick(s): {last:?}"
            ));
        }
    }
    violations
}

/// What the engine and the stores hold, for a failed case's report: every
/// lash invocation that has not completed, and every stalled scope close.
pub async fn diagnose(world: &CrashWorld) -> Vec<String> {
    let mut lines: Vec<String> = world
        .server()
        .invocations()
        .into_iter()
        .filter(|view| view.status != "completed")
        .map(|view| {
            format!(
                "engine: {} {} attempts={} last_failure={:?}",
                view.target, view.status, view.attempts, view.last_failure
            )
        })
        .collect();
    match world
        .backend()
        .obligation_ledger(ObligationKind::ScopeClose)
        .list_stalled(None, PAGE)
        .await
    {
        Ok(stalled) => lines.extend(stalled.into_iter().map(|stall| {
            format!(
                "stalled scope close: {} {:?} attempts={} {:?}",
                stall.id, stall.reason, stall.attempts, stall.last_error
            )
        })),
        Err(error) => lines.push(format!("stalled scope closes unreadable: {error}")),
    }
    match world
        .backend()
        .session_store_factory()
        .list_control_intents(None, PAGE)
        .await
    {
        Ok(intents) => lines.extend(intents.into_iter().map(|intent| {
            format!(
                "intent: {} of `{}` {:?} {:?} obligation={:?}",
                intent.id, intent.session_id, intent.kind, intent.state, intent.obligation
            )
        })),
        Err(error) => lines.push(format!("control intents unreadable: {error}")),
    }
    lines
}

/// Every invocation's journal commands, named, for a case whose engine crash
/// point never fired.
pub fn journal_names(world: &CrashWorld) -> Vec<String> {
    let server = world.server();
    server
        .invocations()
        .into_iter()
        .map(|view| {
            let names: Vec<String> = server
                .journal(&view.id)
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| entry.ty.is_command())
                .map(|entry| format!("{:?}:{}", entry.ty, entry.name.unwrap_or_default()))
                .collect();
            format!("journal: {} {names:?}", view.target)
        })
        .collect()
}
