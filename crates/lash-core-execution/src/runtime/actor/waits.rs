//! Waits, completion keys, `await_external` and `await_process`
//! (ADR 0132 §6, §11; S5 of I0, FIG-5194). Owned by L5 (FIG-5173).
//!
//! # Contracts
//!
//! - A wait's deadline is written once, at minting, and never refreshed.
//! - The first resolution wins; a repeat with the same digest answers
//!   `AlreadyResolved`, with another `Conflict`.
//! - A host resolve refuses every kind but `tool_completion` and `custom`
//!   with `ReservedKind` and writes nothing.
//! - A resolution's lock order is the wait row, then the actor row. An owner
//!   commit fences its own actor row first, so on PostgreSQL a resolve and
//!   an owner's `Due` can deadlock; the database aborts one, and every
//!   writer here retries a contended transaction unchanged.
//! - A host-resolvable wait's completion key is its wait id: 128 random
//!   bits from the operating system's CSPRNG. It is a bearer capability:
//!   lash keeps no completion secret, and the host decides who may resolve
//!   (its API authentication, its webhook signatures) and hands the key
//!   only to callers it has authorized.
//! - A key that names no wait answers `Unknown` and writes nothing.
//! - Every await races the awaiter's own cancel mail; there is no
//!   `HandedOver`: failover keeps the same row, key and deadline.
//! - A due wait settles in an owner transaction (`wait.timeout`) before
//!   anything acts on it, and that transaction re-checks the row under the
//!   same lock, so a resolution committed first wins.

use std::time::Duration;

use lash_durable::domain::{
    CANCEL_MAIL, DomainWrite, MailAnswer, MailDomainWrite, ScopeKey, WaitLifecycle, WaitPurpose,
    WaitResolution, WaitRow, WaitState, WaitWrite,
};
use lash_durable::{
    ActorKind, ActorTx, CommitLabel, DueSource, DurableError, DurableInstant, Fenced, MailTx,
    StoreFailure, StoreFailureKind,
};
use sha2::{Digest as _, Sha256};

use super::ActorContext;
use crate::{Backend, ExecutionScope, ProcessId, ProcessOutcome};

pub use lash_core_effect::Resolution;
pub use lash_durable::domain::{ResolveAnswer, WaitId, WaitKind};

pub use super::wait_effects::{race_timer, sleep_until_timer, timer};

/// How many times a resolve or a due settlement tries a contended
/// transaction.
const RESOLVE_ATTEMPTS: usize = 3;

/// A host-resolvable wait's completion key: its wait id's 32 lowercase hex
/// digits. The id is 128 random bits, so the key is unguessable, and whoever
/// holds it may resolve the wait: it is a bearer capability, and the host
/// hands it only to callers it has authorized. It carries no scope or kind.
#[derive(Clone, PartialEq, Eq)]
pub struct PinnedKey(String);

impl PinnedKey {
    /// A key as a host stored it.
    #[must_use]
    pub fn new(key: String) -> Self {
        Self(key)
    }

    /// The key of wait `id`.
    #[must_use]
    pub fn of(id: &WaitId) -> Self {
        Self(id.to_hex())
    }

    /// The wait this key names; `None` when it is not a key's spelling.
    #[must_use]
    pub fn wait(&self) -> Option<WaitId> {
        WaitId::parse_hex(&self.0)
    }

    /// The key a host is handed.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PinnedKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PinnedKey(..)")
    }
}

/// A wait's deadline: written once, at minting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WaitDeadline(DurableInstant);

/// A refused wait deadline.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WaitDeadlineRefusal {
    /// The request is above the ceiling.
    #[error("a wait of {requested:?} is above the ceiling {ceiling:?}")]
    AboveCeiling {
        /// The requested wait.
        requested: Duration,
        /// The ceiling.
        ceiling: Duration,
    },
    /// The deadline does not fit a durable instant.
    #[error("a wait of {0:?} does not fit a durable instant")]
    Unrepresentable(Duration),
}

impl WaitDeadline {
    /// The deadline a wait of `requested` (or `default`) minted at `now`
    /// gets, refused above `ceiling`. Callers pass `ExecutionBudgets`'
    /// `wait_default` and `wait_ceiling`; a nested wait keeps its own
    /// deadline.
    ///
    /// # Errors
    ///
    /// [`WaitDeadlineRefusal`].
    pub fn resolve(
        requested: Option<Duration>,
        default: Duration,
        ceiling: Duration,
        now: DurableInstant,
    ) -> Result<Self, WaitDeadlineRefusal> {
        let wait = requested.unwrap_or(default);
        if wait > ceiling {
            return Err(WaitDeadlineRefusal::AboveCeiling {
                requested: wait,
                ceiling,
            });
        }
        i64::try_from(wait.as_millis())
            .ok()
            .and_then(|millis| now.0.checked_add(millis))
            .map(|at| Self(DurableInstant(at)))
            .ok_or(WaitDeadlineRefusal::Unrepresentable(wait))
    }

    /// The deadline at the stored instant `at`: a deadline read back from
    /// its row, or an absolute one its caller recorded before.
    #[must_use]
    pub fn at_instant(at: DurableInstant) -> Self {
        Self(at)
    }

    /// The deadline.
    #[must_use]
    pub fn at(self) -> DurableInstant {
        self.0
    }
}

/// What to pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitSpec {
    /// What it waits for.
    pub kind: WaitKind,
    /// The scope that revokes it.
    pub scope: ScopeKey,
    /// For a process-terminal wait, the process; for a child-session wait,
    /// the process whose child turn it waits for.
    pub target_process: Option<ProcessId>,
    /// Its deadline.
    pub deadline: Option<WaitDeadline>,
}

/// A pinned wait, as its owner holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WaitRef {
    id: WaitId,
    kind: WaitKind,
}

impl WaitRef {
    /// The wait `id` of `kind`.
    #[must_use]
    pub fn new(id: WaitId, kind: WaitKind) -> Self {
        Self { id, kind }
    }

    /// Its identity.
    #[must_use]
    pub fn id(&self) -> WaitId {
        self.id
    }

    /// What it waits for.
    #[must_use]
    pub fn kind(&self) -> WaitKind {
        self.kind
    }
}

/// A refused pin; nothing was recorded.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PinRefusal {
    /// A process-terminal or child-session wait names no process, or
    /// another kind names one.
    #[error("a {0:?} wait's target process does not match its kind")]
    TargetMismatch(WaitKind),
    /// A timer has no deadline: a timer is its due time.
    #[error("a timer wait needs a deadline")]
    TimerWithoutDeadline,
}

/// Which of a race's waits won.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RaceWinner {
    /// The wait resolved.
    Resolved {
        /// The wait.
        wait: WaitRef,
        /// Its resolution.
        resolution: Resolution,
    },
    /// The wait's deadline passed first.
    TimedOut(WaitRef),
    /// The awaiter's own cancel mail came first, or the scope that owns the
    /// wait ended and revoked it.
    Cancelled,
}

/// How an [`await_external`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExternalWaitOutcome {
    /// The host resolved the key.
    Resolved(Resolution),
    /// The deadline passed first.
    TimedOut,
    /// The awaiter was cancelled first.
    Cancelled,
}

/// How an [`await_process`] ended.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessWaitOutcome {
    /// The process ended.
    Resolved(ProcessOutcome),
    /// The deadline passed first.
    TimedOut,
    /// The awaiter was cancelled first.
    Cancelled,
}

/// How a settled wait row ended, decoded: what an activation that reads
/// its waits' rows itself hands to its own logic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitSettled {
    /// Resolved: a timer with `Ok(null)`, a process-terminal wait with the
    /// outcome [`process_outcome`] decodes.
    Resolved(Resolution),
    /// Its deadline passed first, in a committed `Due`.
    TimedOut,
    /// Its scope ended first.
    Revoked,
}

/// `row` decoded, or `None` while it is pending.
///
/// # Errors
///
/// A stored resolution that does not decode.
pub fn settled(row: &WaitRow) -> Result<Option<WaitSettled>, DurableError> {
    Ok(match row.lifecycle.state() {
        WaitState::Pending => None,
        WaitState::Resolved => Some(WaitSettled::Resolved(decode_resolution(row)?)),
        WaitState::TimedOut => Some(WaitSettled::TimedOut),
        WaitState::Revoked => Some(WaitSettled::Revoked),
    })
}

/// Mint a wait on `tx`: insert its row pending under a fresh random id, and
/// for a host-resolvable kind hand back its key, the id. The key names a
/// wait only once `tx` commits, before any step that submits it.
///
/// # Errors
///
/// [`PinRefusal`]; nothing is recorded.
pub fn pin(tx: &mut ActorTx, spec: WaitSpec) -> Result<(WaitRef, Option<PinnedKey>), PinRefusal> {
    let targeted = matches!(
        spec.kind,
        WaitKind::ProcessTerminal | WaitKind::ChildSession
    );
    if spec.target_process.is_some() != targeted {
        return Err(PinRefusal::TargetMismatch(spec.kind));
    }
    if spec.kind == WaitKind::Timer && spec.deadline.is_none() {
        return Err(PinRefusal::TimerWithoutDeadline);
    }
    let id = fresh_wait_id();
    let key = spec.kind.host_resolvable().then(|| PinnedKey::of(&id));
    tx.write(DomainWrite::Wait(WaitWrite::Pin {
        id,
        scope: spec.scope,
        purpose: WaitPurpose::decode(
            spec.kind,
            spec.target_process,
            spec.deadline.map(WaitDeadline::at),
        )
        .ok_or(PinRefusal::TargetMismatch(spec.kind))?,
    }));
    Ok((WaitRef::new(id, spec.kind), key))
}

/// A host's resolve of `key`: resolve the wait it names from `pending` and
/// wake the owner, in one mailbox transaction. The store refuses a reserved
/// kind with `ReservedKind`, and the first resolution wins.
///
/// A key that names no wait answers `Unknown` and writes nothing. Lash does
/// not decide who may resolve: whoever holds the key may, and the host
/// authorizes its callers before it calls this.
///
/// # Errors
///
/// A store failure.
pub async fn resolve_host(
    backend: &Backend,
    key: &str,
    resolution: Resolution,
) -> Result<ResolveAnswer, DurableError> {
    let Some(id) = WaitId::parse_hex(key) else {
        return Ok(ResolveAnswer::Unknown);
    };
    let (digest, resolution_ref) = encode_resolution(&resolution)?;
    resolve_row(
        backend,
        WaitResolution {
            id,
            by_host: true,
            digest,
            resolution_ref,
        },
    )
    .await
}

/// Resolve one wait from outside its owner, retrying a contended
/// transaction.
async fn resolve_row(
    backend: &Backend,
    resolution: WaitResolution,
) -> Result<ResolveAnswer, DurableError> {
    let mut attempt = 0;
    loop {
        let mut tx = MailTx::new();
        tx.write(MailDomainWrite::ResolveWait(resolution.clone()));
        match backend
            .durable()
            .commit_mail(tx, CommitLabel::WAIT_RESOLVE)
            .await
        {
            Ok(commit) => {
                return match commit.answers.first() {
                    Some(MailAnswer::ResolveWait(answer)) => Ok(*answer),
                    _ => Err(corrupt("a wait resolution's commit carried no answer")),
                };
            }
            Err(error) if contended(&error) && attempt + 1 < RESOLVE_ATTEMPTS => attempt += 1,
            Err(error) => return Err(error),
        }
    }
}

/// Wait until one of `waits` resolves or times out, raced against the
/// awaiter's own cancel mail (an unacknowledged mail of kind
/// [`CANCEL_MAIL`]). Suspension is a state: the waits are rows, so dropping
/// this future loses nothing, and an activation with nothing runnable
/// commits and releases as `waiting` at [`ActorContext::next_due`], which
/// this race keeps at the earliest pending deadline.
///
/// A due wait settles in its own owner transaction (`wait.timeout`) before
/// the race answers `TimedOut`.
///
/// # Errors
///
/// A store failure; [`DurableError::OwnershipLost`], also when the
/// activation's cancel token stops it.
pub async fn race(cx: &ActorContext, waits: &[WaitRef]) -> Result<RaceWinner, DurableError> {
    let store = std::sync::Arc::clone(cx.backend().durable());
    let poll = cx.backend().config().lease().settings().claim_poll;
    let clock = cx.backend().clock();
    loop {
        let mut rows = Vec::with_capacity(waits.len());
        for wait in waits {
            let row = store
                .wait(&wait.id())
                .await?
                .ok_or_else(|| corrupt(&format!("wait {} is not stored", wait.id())))?;
            rows.push((*wait, row));
        }
        if let Some(winner) = decided(&rows)? {
            refresh_wait_dues(cx).await?;
            return Ok(winner);
        }
        let tx = cx.begin().await?;
        if tx
            .mail()
            .iter()
            .any(|mail| mail.kind.as_str() == CANCEL_MAIL)
        {
            refresh_wait_dues(cx).await?;
            return Ok(RaceWinner::Cancelled);
        }
        let now = store.now().await?;
        let earliest = rows
            .iter()
            .filter_map(|(_, row)| row.purpose.deadline())
            .min();
        if earliest.is_some_and(|due| due <= now) {
            match settle_rows(cx, tx, rows.iter().map(|(_, row)| row), now).await {
                Ok(()) => {}
                // A resolver holding the wait row lost a deadlock to this
                // commit or won one: re-read and settle again.
                Err(error) if contended(&error) => {}
                Err(error) => return Err(error),
            }
            continue;
        }
        for (_, row) in &rows {
            if let Some(due) = row.purpose.deadline() {
                cx.note_due(due_source(row.purpose.kind()), due);
            }
        }
        let until_due = earliest.map_or(poll, |due| {
            Duration::from_millis(u64::try_from(due.0 - now.0).unwrap_or(0)).min(poll)
        });
        tokio::select! {
            () = clock.sleep(until_due) => {}
            () = cx.cancel().cancelled() => return Err(stopped(cx)),
        }
    }
}

/// Settle every pending wait of the context's actor whose deadline passed:
/// a timer resolves, any other wait times out, in one owner transaction
/// (`wait.timeout`). An activation runs this first on a claim, so a due
/// outcome is durable before anything acts on it. Answers the waits it
/// settled.
///
/// # Errors
///
/// A store failure; [`DurableError::OwnershipLost`].
pub async fn settle_due(cx: &ActorContext) -> Result<Vec<WaitRef>, DurableError> {
    let store = cx.backend().durable();
    let pending = store.pending_waits(cx.actor()).await?;
    let now = store.now().await?;
    let due: Vec<WaitRef> = pending
        .iter()
        .filter(|row| {
            row.purpose
                .deadline()
                .is_some_and(|deadline| deadline <= now)
        })
        .map(|row| WaitRef::new(row.id, row.purpose.kind()))
        .collect();
    if !due.is_empty() {
        let mut attempt = 1;
        loop {
            let tx = cx.begin().await?;
            match settle_rows(cx, tx, pending.iter(), now).await {
                Err(error) if contended(&error) && attempt < RESOLVE_ATTEMPTS => attempt += 1,
                settled => break settled?,
            }
        }
    }
    refresh_wait_dues(cx).await?;
    Ok(due)
}

/// Write `Due` for every pending row past its deadline as of `now` and
/// commit. The store re-checks each row under its lock.
async fn settle_rows<'a>(
    cx: &ActorContext,
    mut tx: ActorTx,
    rows: impl Iterator<Item = &'a WaitRow>,
    now: DurableInstant,
) -> Result<(), DurableError> {
    for row in rows {
        if row.lifecycle.state() == WaitState::Pending
            && row.purpose.deadline().is_some_and(|due| due <= now)
        {
            tx.write(DomainWrite::Wait(WaitWrite::Due { id: row.id }));
        }
    }
    cx.commit(tx, CommitLabel::WAIT_TIMEOUT).await.map(|_| ())
}

/// Re-note the wait and timer due times from the actor's pending rows, so a
/// release as `waiting` records exactly the earliest one still pending.
async fn refresh_wait_dues(cx: &ActorContext) -> Result<(), DurableError> {
    let pending = cx.backend().durable().pending_waits(cx.actor()).await?;
    cx.clear_due(DueSource::WaitDeadline);
    cx.clear_due(DueSource::Timer);
    for row in pending {
        if let Some(due) = row.purpose.deadline() {
            cx.note_due(due_source(row.purpose.kind()), due);
        }
    }
    Ok(())
}

fn due_source(kind: WaitKind) -> DueSource {
    if kind == WaitKind::Timer {
        DueSource::Timer
    } else {
        DueSource::WaitDeadline
    }
}

/// The race's winner among rows that are no longer pending: the earliest
/// settled, in the order given on a tie.
fn decided(rows: &[(WaitRef, WaitRow)]) -> Result<Option<RaceWinner>, DurableError> {
    let first = rows
        .iter()
        .filter(|(_, row)| row.lifecycle.state() != WaitState::Pending)
        .min_by_key(|(_, row)| row.lifecycle.resolved_at());
    let Some((wait, row)) = first else {
        return Ok(None);
    };
    Ok(Some(match row.lifecycle.state() {
        WaitState::Resolved => RaceWinner::Resolved {
            wait: *wait,
            resolution: decode_resolution(row)?,
        },
        WaitState::TimedOut => RaceWinner::TimedOut(*wait),
        WaitState::Revoked | WaitState::Pending => RaceWinner::Cancelled,
    }))
}

/// Wait on a pinned host-resolvable wait, bounded by its deadline and raced
/// against the awaiter's cancel mail.
///
/// # Errors
///
/// A store failure; [`DurableError::OwnershipLost`].
pub async fn await_external(
    cx: &ActorContext,
    wait: &WaitRef,
) -> Result<ExternalWaitOutcome, DurableError> {
    Ok(match race(cx, std::slice::from_ref(wait)).await? {
        RaceWinner::Resolved { resolution, .. } => ExternalWaitOutcome::Resolved(resolution),
        RaceWinner::TimedOut(_) => ExternalWaitOutcome::TimedOut,
        RaceWinner::Cancelled => ExternalWaitOutcome::Cancelled,
    })
}

/// Wait for `process` to end, bounded by `deadline`, raced against the
/// awaiter's cancel mail, so a cycle of waits is cancellable. The wait is
/// [`pin_process_terminal`]'s.
///
/// # Errors
///
/// A store failure; [`DurableError::OwnershipLost`].
pub async fn await_process(
    cx: &ActorContext,
    process: &ProcessId,
    deadline: WaitDeadline,
) -> Result<ProcessWaitOutcome, DurableError> {
    let wait = pin_process_terminal(cx, wait_scope(cx), process, Some(deadline)).await?;
    Ok(match race(cx, &[wait]).await? {
        RaceWinner::Resolved { resolution, .. } => {
            ProcessWaitOutcome::Resolved(process_outcome(resolution)?)
        }
        RaceWinner::TimedOut(_) => ProcessWaitOutcome::TimedOut,
        RaceWinner::Cancelled => ProcessWaitOutcome::Cancelled,
    })
}

/// Pin a `process_terminal` wait of `scope` on `process`, bounded by
/// `deadline`, and commit it (`wait.mint`). A process whose terminal
/// committed before the wait resolved no wait of it, so the registry is
/// read after the commit: a process that already ended resolves the wait
/// from its outcome at once, first winner beside the process's own
/// terminal transaction, and that resolution wakes the wait's owner.
///
/// # Errors
///
/// A store or registry failure; [`DurableError::OwnershipLost`].
pub async fn pin_process_terminal(
    cx: &ActorContext,
    scope: ScopeKey,
    process: &ProcessId,
    deadline: Option<WaitDeadline>,
) -> Result<WaitRef, DurableError> {
    let mut tx = cx.begin().await?;
    let (wait, _) = pin(
        &mut tx,
        WaitSpec {
            kind: WaitKind::ProcessTerminal,
            scope,
            target_process: Some(process.clone()),
            deadline,
        },
    )
    .map_err(|refusal| corrupt(&refusal.to_string()))?;
    cx.commit(tx, CommitLabel::WAIT_MINT).await?;
    resolve_ended_terminal(cx.backend(), &wait).await?;
    Ok(wait)
}

/// Resolve `wait`, a pending process-terminal wait, from its process's
/// recorded outcome when the process had already ended: a process that ended
/// before the wait was pinned resolved no wait in its terminal transaction.
/// First winner beside that transaction, and the resolution wakes the wait's
/// owner; a wait already settled, one on a process still running, and one on
/// a process pruned since it ended (its outcome went with it; the wait's
/// deadline answers) are left as they are.
///
/// # Errors
///
/// A store or registry failure.
pub async fn resolve_ended_terminal(backend: &Backend, wait: &WaitRef) -> Result<(), DurableError> {
    let Some(row) = backend.durable().wait(&wait.id()).await? else {
        return Ok(());
    };
    let Some(process) = row
        .purpose
        .target_process()
        .filter(|_| row.lifecycle.state() == WaitState::Pending)
    else {
        return Ok(());
    };
    let record = match backend.process_registry().get_process(process).await {
        Ok(record) => record,
        Err(crate::PluginError::ProcessNoLongerRetained { .. }) => return Ok(()),
        Err(error) => {
            return Err(DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Unavailable,
                message: error.to_string(),
            }));
        }
    };
    if let Some(outcome) = record.and_then(|record| record.outcome()) {
        let (digest, resolution_ref) = encode_process_outcome(&outcome)?;
        resolve_row(
            backend,
            WaitResolution {
                id: wait.id(),
                by_host: false,
                digest,
                resolution_ref,
            },
        )
        .await?;
    }
    Ok(())
}

/// Resolve every pending process-terminal wait on `process` with `outcome`
/// in its terminal transaction, and wake each owner. Called by L6.
///
/// # Errors
///
/// The outcome does not encode.
pub fn resolve_process_terminal_waits(
    tx: &mut ActorTx,
    process: &ProcessId,
    outcome: &ProcessOutcome,
) -> Result<(), DurableError> {
    let (digest, resolution_ref) = encode_process_outcome(outcome)?;
    tx.write(DomainWrite::Wait(WaitWrite::ResolveProcessTerminal {
        process: process.clone(),
        digest,
        resolution_ref,
    }));
    Ok(())
}

/// Resolve every pending child-session wait on `process` in the
/// transaction that ends its child turn, and wake each owner. The process
/// reads the turn's end from the child session's store; the resolution
/// names only the turn. A run that is no process's child turn has no such
/// wait, and the write changes nothing.
///
/// # Errors
///
/// The resolution does not encode.
pub fn resolve_child_session_waits(
    tx: &mut ActorTx,
    process: &ProcessId,
) -> Result<(), DurableError> {
    let (digest, resolution_ref) = encode_resolution(&Resolution::Ok(
        serde_json::json!({ "run": process.as_str() }),
    ))?;
    tx.write(DomainWrite::Wait(WaitWrite::ResolveChildSession {
        process: process.clone(),
        digest,
        resolution_ref,
    }));
    Ok(())
}

/// Revoke every pending wait of `scope`. Called by L6 and L6b.
pub fn revoke_scope(tx: &mut ActorTx, scope: &ScopeKey) {
    tx.write(DomainWrite::Wait(WaitWrite::RevokeScope(scope.clone())));
}

/// The host key of `wait`, as [`pin`] minted it: what an owner hands a body
/// that runs again under the wait it pinned before. `None` for a kind no
/// host resolves.
#[must_use]
pub fn host_key(wait: &WaitRef) -> Option<PinnedKey> {
    wait.kind()
        .host_resolvable()
        .then(|| PinnedKey::of(&wait.id()))
}

/// The keys of `owner`'s pending host-resolvable waits: what an operator
/// lists.
///
/// # Errors
///
/// A store failure.
pub async fn outstanding_keys(
    backend: &Backend,
    owner: &lash_durable::ActorKey,
) -> Result<Vec<PinnedKey>, DurableError> {
    Ok(backend
        .durable()
        .pending_waits(owner)
        .await?
        .into_iter()
        .filter(|row| row.purpose.kind().host_resolvable())
        .map(|row| PinnedKey::of(&row.id))
        .collect())
}

/// The scope a wait minted by `cx` belongs to: its admitted scope, or its
/// actor's for a scope that names neither a turn, a session nor a process.
pub fn wait_scope(cx: &ActorContext) -> ScopeKey {
    match cx.execution_scope() {
        ExecutionScope::Turn {
            session_id,
            turn_id,
        } => ScopeKey::Turn(session_id.clone(), turn_id.clone()),
        ExecutionScope::Process { process_id } => ScopeKey::Process(process_id.clone()),
        ExecutionScope::SessionOperation { session_id, .. }
        | ExecutionScope::SessionDelete { session_id } => ScopeKey::Session(session_id.clone()),
        ExecutionScope::RuntimeOperation { .. } => actor_scope(cx.actor()),
    }
}

fn actor_scope(actor: &lash_durable::ActorKey) -> ScopeKey {
    // An actor's id is never empty, so it parses as either identity.
    let session = || {
        crate::SessionId::parse(actor.id()).unwrap_or_else(|_| crate::SessionId::from("unscoped"))
    };
    match actor.kind() {
        ActorKind::Session => ScopeKey::Session(session()),
        ActorKind::Process => ProcessId::parse(actor.id())
            .map_or_else(|_| ScopeKey::Session(session()), ScopeKey::Process),
    }
}

/// 128 random bits: two v4 UUIDs' randomness (244 bits from the operating
/// system's CSPRNG), hashed down, so the id is unguessable and its key a
/// capability.
fn fresh_wait_id() -> WaitId {
    let mut hasher = Sha256::new();
    hasher.update(uuid::Uuid::new_v4().as_bytes());
    hasher.update(uuid::Uuid::new_v4().as_bytes());
    let digest = hasher.finalize();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    WaitId(id)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// A resolution's stored digest and reference: its JSON, inline.
fn encode_resolution(resolution: &Resolution) -> Result<(String, String), DurableError> {
    let encoded = serde_json::to_string(resolution)
        .map_err(|error| corrupt(&format!("a resolution does not encode: {error}")))?;
    Ok((
        format!("sha256:{}", hex(&Sha256::digest(&encoded))),
        encoded,
    ))
}

fn decode_resolution(row: &WaitRow) -> Result<Resolution, DurableError> {
    match &row.lifecycle {
        WaitLifecycle::TimerElapsed { .. } => Ok(Resolution::Ok(serde_json::Value::Null)),
        WaitLifecycle::Resolved { resolution_ref, .. } => serde_json::from_str(resolution_ref)
            .map_err(|error| corrupt(&format!("wait {}'s resolution: {error}", row.id))),
        _ => Err(corrupt(&format!("wait {} is not resolved", row.id))),
    }
}

fn encode_process_outcome(outcome: &ProcessOutcome) -> Result<(String, String), DurableError> {
    let value = serde_json::to_value(outcome)
        .map_err(|error| corrupt(&format!("a process outcome does not encode: {error}")))?;
    encode_resolution(&Resolution::Ok(value))
}

/// The process outcome a resolved process-terminal wait carries.
///
/// # Errors
///
/// A resolution that is not a stored process outcome.
pub fn process_outcome(resolution: Resolution) -> Result<ProcessOutcome, DurableError> {
    match resolution {
        Resolution::Ok(value) => serde_json::from_value(value)
            .map_err(|error| corrupt(&format!("a process outcome does not decode: {error}"))),
        other => Err(corrupt(&format!(
            "a process-terminal wait resolved as {other:?}"
        ))),
    }
}

/// A transaction that lost to lock contention: on PostgreSQL a resolve
/// (wait row, then actor row) and an owner commit (its fenced actor row,
/// then its wait rows) can deadlock, and the database aborts one. Both are
/// conditional, so the loser retries unchanged.
fn contended(error: &DurableError) -> bool {
    matches!(
        error,
        DurableError::Store(StoreFailure {
            kind: StoreFailureKind::Contended,
            ..
        })
    )
}

fn corrupt(message: &str) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message: message.to_owned(),
    })
}

/// The refusal a race answers when the activation's cancel token stopped
/// it: the actor is no longer this activation's.
fn stopped(cx: &ActorContext) -> DurableError {
    DurableError::OwnershipLost(Fenced {
        actor: cx.actor().clone(),
        held: cx.epoch(),
        current: None,
    })
}
