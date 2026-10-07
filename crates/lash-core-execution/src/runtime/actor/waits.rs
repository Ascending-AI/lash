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
//! - A durable backend built without completion secrets is refused
//!   (`DurableBuildError::MissingCompletionSecrets`).
//! - Every await races the awaiter's own cancel mail; there is no
//!   `HandedOver`: failover keeps the same row, key and deadline.
//! - A due wait settles in an owner transaction (`wait.timeout`) before
//!   anything acts on it, and that transaction re-checks the row under the
//!   same lock, so a resolution committed first wins.

use std::collections::BTreeMap;
use std::time::Duration;

use lash_durable::domain::{
    CANCEL_MAIL, DomainWrite, MailAnswer, MailDomainWrite, ScopeKey, TIMER_DIGEST, WaitResolution,
    WaitRow, WaitState, WaitWrite,
};
use lash_durable::{
    ActorKind, ActorTx, CommitLabel, DueSource, DurableError, DurableInstant, Fenced, MailTx,
    StoreFailure, StoreFailureKind,
};
use sha2::{Digest as _, Sha256};

use super::ActorContext;
use crate::{Backend, ExecutionScope, ProcessId, ProcessOutcome};

pub use lash_core_effect::Resolution;
pub use lash_durable::domain::{KeyVersion, ResolveAnswer, WaitId, WaitKind};

/// The key format's version prefix.
const KEY_PREFIX: &str = "wk1";

/// The shortest completion secret: one SHA-256 block's worth of key
/// material is what HMAC-SHA256 is keyed with.
const MIN_SECRET_BYTES: usize = 32;

/// How many times a resolve or a due settlement tries a contended
/// transaction.
const RESOLVE_ATTEMPTS: usize = 3;

/// A host-resolvable wait's key: `wk1.<wait_id>.<mac>`, where the MAC is
/// HMAC-SHA256 under the deployment's completion secret of the row's key
/// version over `"wk1" ‖ wait_id ‖ kind`. It carries no scope or kind in
/// plaintext.
#[derive(Clone, PartialEq, Eq)]
pub struct PinnedKey(String);

impl PinnedKey {
    /// A key as minted.
    #[must_use]
    pub fn new(key: String) -> Self {
        Self(key)
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

/// One completion secret's bytes. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    /// Wrap secret bytes.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// The bytes, for the MAC.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretBytes(..)")
    }
}

/// The deployment's completion secrets: one or more versions, one current.
/// New keys are minted under the current version; a live wait verifies under
/// its stored version for as long as that version is configured.
#[derive(Clone, PartialEq, Eq)]
pub struct CompletionKeySecrets {
    current: KeyVersion,
    secrets: BTreeMap<KeyVersion, SecretBytes>,
}

impl std::fmt::Debug for CompletionKeySecrets {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompletionKeySecrets")
            .field("current", &self.current)
            .field("versions", &self.secrets.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Refused completion secrets.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SecretsRefusal {
    /// The current version has no secret.
    #[error("the current key version {0:?} has no secret")]
    CurrentMissing(KeyVersion),
    /// A version is configured twice.
    #[error("key version {0:?} is configured twice")]
    DuplicateVersion(KeyVersion),
    /// A secret is too short to key an HMAC-SHA256 safely.
    #[error("the secret of key version {0:?} is too short")]
    TooShort(KeyVersion),
}

impl CompletionKeySecrets {
    /// Validate `secrets`, with `current` the version new keys mint under.
    /// Each secret is at least 32 bytes; there is no default and no derived
    /// fallback.
    ///
    /// # Errors
    ///
    /// [`SecretsRefusal`].
    pub fn new(
        current: KeyVersion,
        secrets: Vec<(KeyVersion, SecretBytes)>,
    ) -> Result<Self, SecretsRefusal> {
        let mut versions = BTreeMap::new();
        for (version, secret) in secrets {
            if secret.expose().len() < MIN_SECRET_BYTES {
                return Err(SecretsRefusal::TooShort(version));
            }
            if versions.insert(version, secret).is_some() {
                return Err(SecretsRefusal::DuplicateVersion(version));
            }
        }
        if !versions.contains_key(&current) {
            return Err(SecretsRefusal::CurrentMissing(current));
        }
        Ok(Self {
            current,
            secrets: versions,
        })
    }

    /// One fixed 32-byte secret under version 1, for tests: never a
    /// deployment's.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn for_testing() -> Self {
        Self {
            current: KeyVersion(1),
            secrets: BTreeMap::from([(KeyVersion(1), SecretBytes::new(vec![7; 32]))]),
        }
    }

    /// The version new keys mint under.
    #[must_use]
    pub fn current(&self) -> KeyVersion {
        self.current
    }

    /// The secret of `version`, if it is configured.
    #[must_use]
    pub fn secret(&self, version: KeyVersion) -> Option<&SecretBytes> {
        self.secrets.get(&version)
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
    /// For a process-terminal wait, the process.
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
    /// A process-terminal wait names no process, or another kind names one.
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
    Ok(match row.state {
        WaitState::Pending => None,
        WaitState::Resolved => Some(WaitSettled::Resolved(decode_resolution(row)?)),
        WaitState::TimedOut => Some(WaitSettled::TimedOut),
        WaitState::Revoked => Some(WaitSettled::Revoked),
    })
}

/// What checking a host key found, before anything is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyCheck {
    /// Not a `wk1.<wait_id>.<mac>` key.
    Malformed,
    /// No wait has the key's id.
    UnknownWait,
    /// The wait's key version is not configured any more.
    UnknownVersion(KeyVersion),
    /// The MAC is not the wait's under its key version.
    MacMismatch,
    /// The MAC verified: the key names this wait.
    Verified(Box<WaitRow>),
}

/// Mint a wait on `tx`: insert its row pending, and for a host-resolvable
/// kind mint its key under `secrets`' current version. The key exists only
/// once `tx` commits, before any step that submits it.
///
/// # Errors
///
/// [`PinRefusal`]; nothing is recorded.
pub fn pin(
    tx: &mut ActorTx,
    secrets: &CompletionKeySecrets,
    spec: WaitSpec,
) -> Result<(WaitRef, Option<PinnedKey>), PinRefusal> {
    if spec.target_process.is_some() != (spec.kind == WaitKind::ProcessTerminal) {
        return Err(PinRefusal::TargetMismatch(spec.kind));
    }
    if spec.kind == WaitKind::Timer && spec.deadline.is_none() {
        return Err(PinRefusal::TimerWithoutDeadline);
    }
    let id = fresh_wait_id();
    let key_version = spec.kind.host_resolvable().then(|| secrets.current());
    let key = key_version.and_then(|version| {
        secrets
            .secret(version)
            .map(|secret| PinnedKey::new(render_key(&id, &mac(secret, &id, spec.kind))))
    });
    tx.write(DomainWrite::Wait(WaitWrite::Pin {
        id,
        scope: spec.scope,
        kind: spec.kind,
        target_process: spec.target_process,
        deadline: spec.deadline.map(WaitDeadline::at),
        key_version,
    }));
    Ok((WaitRef::new(id, spec.kind), key))
}

/// Check `key` against its wait row: parse it, load the row, and verify the
/// MAC in constant time under the row's key version (a kind that carries no
/// key version is checked under the current one). Nothing is written.
///
/// # Errors
///
/// A store failure.
pub async fn check_host_key(backend: &Backend, key: &str) -> Result<HostKeyCheck, DurableError> {
    let Some((id, offered)) = parse_key(key) else {
        return Ok(HostKeyCheck::Malformed);
    };
    let Some(row) = backend.durable().wait(&id).await? else {
        return Ok(HostKeyCheck::UnknownWait);
    };
    let secrets = backend.completion_secrets();
    let version = row.key_version.unwrap_or_else(|| secrets.current());
    let Some(secret) = secrets.secret(version) else {
        return Ok(HostKeyCheck::UnknownVersion(version));
    };
    if !constant_time_eq(&mac(secret, &id, row.kind), &offered) {
        return Ok(HostKeyCheck::MacMismatch);
    }
    Ok(HostKeyCheck::Verified(Box::new(row)))
}

/// A host's resolve of `key`: parse it, verify its MAC in constant time under
/// its row's key version, refuse a reserved kind, then resolve from
/// `pending` and wake the owner, in one mailbox transaction.
///
/// A key that does not verify answers `UnknownOrRevoked`: it learns nothing
/// about which waits exist.
///
/// # Errors
///
/// A store failure.
pub async fn resolve_host(
    backend: &Backend,
    key: &str,
    resolution: Resolution,
) -> Result<ResolveAnswer, DurableError> {
    let row = match check_host_key(backend, key).await? {
        HostKeyCheck::Verified(row) => row,
        HostKeyCheck::Malformed
        | HostKeyCheck::UnknownWait
        | HostKeyCheck::UnknownVersion(_)
        | HostKeyCheck::MacMismatch => return Ok(ResolveAnswer::UnknownOrRevoked),
    };
    if !row.kind.host_resolvable() {
        return Ok(ResolveAnswer::ReservedKind);
    }
    let (digest, resolution_ref) = encode_resolution(&resolution)?;
    resolve_row(
        backend,
        WaitResolution {
            id: row.id,
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
        let earliest = rows.iter().filter_map(|(_, row)| row.deadline).min();
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
            if let Some(due) = row.deadline {
                cx.note_due(due_source(row.kind), due);
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
        .filter(|row| row.deadline.is_some_and(|deadline| deadline <= now))
        .map(|row| WaitRef::new(row.id, row.kind))
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
        if row.state == WaitState::Pending && row.deadline.is_some_and(|due| due <= now) {
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
        if let Some(due) = row.deadline {
            cx.note_due(due_source(row.kind), due);
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
        .filter(|(_, row)| row.state != WaitState::Pending)
        .min_by_key(|(_, row)| row.resolved_at);
    let Some((wait, row)) = first else {
        return Ok(None);
    };
    Ok(Some(match row.state {
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
        cx.backend().completion_secrets(),
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
/// owner; a wait already settled, or one on a process still running, is left
/// as it is.
///
/// # Errors
///
/// A store or registry failure.
pub async fn resolve_ended_terminal(backend: &Backend, wait: &WaitRef) -> Result<(), DurableError> {
    let Some(row) = backend.durable().wait(&wait.id()).await? else {
        return Ok(());
    };
    let Some(process) = row
        .target_process
        .filter(|_| row.state == WaitState::Pending)
    else {
        return Ok(());
    };
    let record = backend
        .process_registry()
        .get_process(&process)
        .await
        .map_err(|error| {
            DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Unavailable,
                message: error.to_string(),
            })
        })?;
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

/// Revoke every pending wait of `scope`. Called by L6 and L6b.
pub fn revoke_scope(tx: &mut ActorTx, scope: &ScopeKey) {
    tx.write(DomainWrite::Wait(WaitWrite::RevokeScope(scope.clone())));
}

/// The host key of `wait`, minted under `version`, rebuilt as [`pin`]
/// minted it: what an owner hands a body that runs again under the wait it
/// pinned before. `None` for a kind no host resolves, or a version that is
/// no longer configured.
#[must_use]
pub fn host_key(
    secrets: &CompletionKeySecrets,
    wait: &WaitRef,
    version: KeyVersion,
) -> Option<PinnedKey> {
    if !wait.kind().host_resolvable() {
        return None;
    }
    let secret = secrets.secret(version)?;
    Some(PinnedKey::new(render_key(
        &wait.id(),
        &mac(secret, &wait.id(), wait.kind()),
    )))
}

/// The host-resolvable keys of `owner`'s pending waits, rebuilt from their
/// rows under each row's key version: what an operator lists. A wait whose
/// version is no longer configured has no key to list.
///
/// # Errors
///
/// A store failure.
pub async fn outstanding_keys(
    backend: &Backend,
    owner: &lash_durable::ActorKey,
) -> Result<Vec<PinnedKey>, DurableError> {
    let secrets = backend.completion_secrets();
    Ok(backend
        .durable()
        .pending_waits(owner)
        .await?
        .into_iter()
        .filter(|row| row.kind.host_resolvable())
        .filter_map(|row| {
            let secret = secrets.secret(row.key_version?)?;
            Some(PinnedKey::new(render_key(
                &row.id,
                &mac(secret, &row.id, row.kind),
            )))
        })
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

/// 128 random bits: two v4 UUIDs' randomness, hashed down.
fn fresh_wait_id() -> WaitId {
    let mut hasher = Sha256::new();
    hasher.update(uuid::Uuid::new_v4().as_bytes());
    hasher.update(uuid::Uuid::new_v4().as_bytes());
    let digest = hasher.finalize();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    WaitId(id)
}

/// The key MAC: HMAC-SHA256 under `secret` over `"wk1" ‖ wait_id ‖ kind`.
pub(super) fn mac(secret: &SecretBytes, id: &WaitId, kind: WaitKind) -> [u8; 32] {
    hmac_sha256(
        secret.expose(),
        &[KEY_PREFIX.as_bytes(), &id.0, kind.as_str().as_bytes()],
    )
}

/// HMAC-SHA256 (RFC 2104) of the concatenated `message` parts.
fn hmac_sha256(secret: &[u8], message: &[&[u8]]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key = [0_u8; BLOCK];
    if secret.len() > BLOCK {
        key[..32].copy_from_slice(&Sha256::digest(secret));
    } else {
        key[..secret.len()].copy_from_slice(secret);
    }
    let pad = |byte: u8| key.map(|k| k ^ byte);
    let mut inner = Sha256::new();
    inner.update(pad(0x36));
    for part in message {
        inner.update(part);
    }
    let mut outer = Sha256::new();
    outer.update(pad(0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

pub(super) fn render_key(id: &WaitId, mac: &[u8]) -> String {
    format!("{KEY_PREFIX}.{}.{}", id.to_hex(), hex(mac))
}

/// A key's wait id and offered MAC bytes; `None` when it is not a `wk1` key.
fn parse_key(key: &str) -> Option<(WaitId, Vec<u8>)> {
    let mut parts = key.split('.');
    let (Some(KEY_PREFIX), Some(id), Some(mac), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    let id = WaitId::parse_hex(id)?;
    if mac.len() % 2 != 0 || !mac.is_ascii() {
        return None;
    }
    let bytes = (0..mac.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(mac.get(at..at + 2)?, 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    Some((id, bytes))
}

/// Compare authentication bytes without branching on their contents. Length
/// is folded into the result and the loop covers the longer input.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let left_byte = left.get(index).copied().unwrap_or_default();
        let right_byte = right.get(index).copied().unwrap_or_default();
        difference |= usize::from(left_byte ^ right_byte);
    }
    difference == 0
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
    if row.kind == WaitKind::Timer && row.resolution_digest.as_deref() == Some(TIMER_DIGEST) {
        return Ok(Resolution::Ok(serde_json::Value::Null));
    }
    let stored = row
        .resolution_ref
        .as_deref()
        .ok_or_else(|| corrupt(&format!("resolved wait {} has no resolution", row.id)))?;
    serde_json::from_str(stored)
        .map_err(|error| corrupt(&format!("wait {}'s resolution: {error}", row.id)))
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

#[cfg(test)]
#[path = "waits_tests.rs"]
mod tests;
