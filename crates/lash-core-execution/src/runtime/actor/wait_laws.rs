//! The laws of waits and completion keys (L5, FIG-5173; ADR 0132 §6, §11),
//! written once over a [`Backend`] and run by each dialect's tests over its
//! own store. Each law takes a fresh backend whose claim poll is short and
//! returns the first rule it saw broken. Time is the store's real clock:
//! deadlines are a few hundred milliseconds out.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use lash_durable::domain::{
    CANCEL_MAIL, MailAnswer, MailDomainWrite, WaitResolution, WaitRow, WaitState,
};
use lash_durable::runner::{Activation, Exit, Owned, Runner, RunnerConfig};
use lash_durable::{
    ActorKey, ClaimCause, CommitLabel, DurableError, DurableInstant, Epoch, FormatSet, MailKind,
    MailTx, NodeId, NodeLease, NodeSpec, Release,
};
use tokio_util::sync::CancellationToken;

use super::ActorContext;
use super::waits::{
    self, PinnedKey, ProcessWaitOutcome, RaceWinner, ResolveAnswer, WaitDeadline, WaitId, WaitKind,
    WaitRef, WaitSpec,
};
use crate::{AdmittedScope, Backend, DurableSettings, ProcessId, Resolution};

/// A broken law: what was expected, and what happened.
#[derive(Debug)]
pub struct LawBroken(pub String);

impl std::fmt::Display for LawBroken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<DurableError> for LawBroken {
    fn from(error: DurableError) -> Self {
        Self(format!("unexpected refusal: {error}"))
    }
}

/// The outcome of one law.
pub type LawResult = Result<(), LawBroken>;

macro_rules! ensure {
    ($condition:expr, $($message:tt)+) => {
        if !$condition {
            return Err(LawBroken(format!($($message)+)));
        }
    };
}

/// The claim poll every law's backend runs with.
pub const CLAIM_POLL: Duration = Duration::from_millis(25);

/// How far past its due time a claimed wait may settle: scheduling slack on
/// a loaded test host.
const EPSILON: Duration = Duration::from_millis(750);

const SHORT: Duration = Duration::from_millis(300);
const LONG: Duration = Duration::from_secs(60);
const TTL_MILLIS: i64 = 15_000;

/// The settings every law's backend is assembled with: the defaults, with a
/// short claim poll.
#[must_use]
pub fn settings() -> DurableSettings {
    let mut settings = DurableSettings::default();
    settings.lease.claim_poll = CLAIM_POLL;
    settings
}

fn formats() -> FormatSet {
    FormatSet::new("wait-laws")
}

async fn node(backend: &Backend, name: &str, ttl_millis: i64) -> Result<NodeLease, LawBroken> {
    Ok(backend
        .durable()
        .register_node(&NodeSpec {
            node: NodeId::new(name),
            decodes: vec![formats()],
            ttl_millis,
        })
        .await?)
}

fn session_actor(id: &str) -> Result<ActorKey, LawBroken> {
    ActorKey::session(id).map_err(|error| LawBroken(error.to_string()))
}

fn turn_scope(session: &'static str) -> AdmittedScope {
    AdmittedScope::turn(session, "run-1")
}

/// Create `actor` and claim it on `lease`: the owned context.
async fn own(
    backend: &Backend,
    lease: &NodeLease,
    actor: &ActorKey,
    admitted: AdmittedScope,
) -> Result<ActorContext, LawBroken> {
    let mut tx = MailTx::new();
    tx.create_actor(actor.clone(), formats());
    backend
        .durable()
        .commit_mail(tx, CommitLabel::new("law.create"))
        .await?;
    claim(backend, lease, actor, admitted).await
}

/// Claim `actor` on `lease` and hand back its context.
async fn claim(
    backend: &Backend,
    lease: &NodeLease,
    actor: &ActorKey,
    admitted: AdmittedScope,
) -> Result<ActorContext, LawBroken> {
    let claimed = backend.durable().claim(lease, 16).await?;
    let epoch = claimed
        .iter()
        .find(|claimed| &claimed.actor == actor)
        .map(|claimed| claimed.epoch)
        .ok_or_else(|| LawBroken(format!("{actor} was not claimable")))?;
    Ok(context(backend, actor, epoch, admitted))
}

fn context(
    backend: &Backend,
    actor: &ActorKey,
    epoch: Epoch,
    admitted: AdmittedScope,
) -> ActorContext {
    ActorContext::new(
        backend.clone(),
        actor.clone(),
        epoch,
        admitted,
        CancellationToken::new(),
        Arc::new(lash_durable::NoProbe),
    )
}

/// Pin one wait of `kind` on `cx`, committed (`wait.mint`).
async fn pin(
    cx: &ActorContext,
    kind: WaitKind,
    target: Option<ProcessId>,
    deadline: Option<Duration>,
) -> Result<(WaitRef, Option<PinnedKey>), LawBroken> {
    let now = cx.backend().durable().now().await?;
    let mut tx = cx.begin().await?;
    let pinned = waits::pin(
        &mut tx,
        WaitSpec {
            kind,
            scope: waits::wait_scope(cx)?,
            target_process: target,
            deadline: deadline.map(|after| {
                WaitDeadline::at_instant(
                    now.after_millis(i64::try_from(after.as_millis()).unwrap_or(i64::MAX)),
                )
            }),
        },
    )
    .map_err(|refusal| LawBroken(refusal.to_string()))?;
    cx.commit(tx, CommitLabel::WAIT_MINT).await?;
    Ok(pinned)
}

async fn row(backend: &Backend, wait: &WaitRef) -> Result<WaitRow, LawBroken> {
    backend
        .durable()
        .wait(&wait.id())
        .await?
        .ok_or_else(|| LawBroken(format!("wait {} is not stored", wait.id())))
}

fn key_of(pinned: Option<PinnedKey>) -> Result<String, LawBroken> {
    pinned
        .map(|key| key.as_str().to_owned())
        .ok_or_else(|| LawBroken("a host-resolvable wait was pinned without a key".into()))
}

fn answer(value: &str) -> Resolution {
    Resolution::Ok(serde_json::json!({ "answer": value }))
}

async fn send_cancel(backend: &Backend, actor: &ActorKey) -> LawResult {
    let mut tx = MailTx::new();
    tx.append(actor.clone(), MailKind::new(CANCEL_MAIL), "{}");
    backend
        .durable()
        .commit_mail(tx, CommitLabel::new("law.cancel"))
        .await?;
    Ok(())
}

/// K1 (FIG-5161, FIG-5217): a completion key is its wait's id, and a key
/// that is not an issued wait id is refused `Unknown` and writes nothing: an
/// id never issued, the key with one digit changed, a truncated key, an
/// empty key and a spelling that is no id at all. A key of a kind a host
/// may not resolve (a process terminal, a timer, a child session)
/// answers `ReservedKind` and writes nothing, at the host resolve and at the
/// store. The genuine key still resolves afterwards.
///
/// # Errors
///
/// The first rule broken.
pub async fn k1_a_key_that_is_not_an_issued_wait_id_is_refused_and_writes_nothing(
    backend: &Backend,
) -> LawResult {
    let lease = node(backend, "k1", TTL_MILLIS).await?;
    let cx = own(backend, &lease, &session_actor("k1")?, turn_scope("k1")).await?;
    let (tool, key) = pin(&cx, WaitKind::ToolCompletion, None, Some(LONG)).await?;
    let key = key_of(key)?;
    ensure!(
        WaitId::parse_hex(&key) == Some(tool.id()),
        "the key {key} is not its wait's id {}",
        tool.id()
    );
    let before = row(backend, &tool).await?;
    let never_issued = WaitId([0x5a; 16]);
    let mut altered = key.clone().into_bytes();
    let last = altered.len() - 1;
    altered[last] = if altered[last] == b'0' { b'1' } else { b'0' };
    let forgeries = [
        ("an id never issued", never_issued.to_hex()),
        (
            "one digit changed",
            String::from_utf8(altered).map_err(|error| LawBroken(error.to_string()))?,
        ),
        ("a truncated key", key[..key.len() - 2].to_owned()),
        ("an empty key", String::new()),
        ("no id at all", format!("wk1.{key}.00")),
    ];
    for (case, forged) in &forgeries {
        let answered = waits::resolve_host(backend, forged, answer("forged")).await?;
        ensure!(
            answered == ResolveAnswer::Unknown,
            "{case}: answered {answered:?}"
        );
        ensure!(
            row(backend, &tool).await? == before,
            "{case}: the wait row changed"
        );
    }
    let mut tx = MailTx::new();
    tx.write(MailDomainWrite::ResolveWait(WaitResolution {
        id: never_issued,
        by_host: true,
        digest: "forged".into(),
        resolution_ref: "{}".into(),
    }));
    let commit = backend
        .durable()
        .commit_mail(tx, CommitLabel::WAIT_RESOLVE)
        .await?;
    ensure!(
        commit.answers == [MailAnswer::ResolveWait(ResolveAnswer::Unknown)],
        "the store answered an id never issued with {:?}",
        commit.answers
    );
    ensure!(
        backend.durable().wait(&never_issued).await?.is_none(),
        "resolving an id never issued stored a wait"
    );
    let target = ProcessId::fixture("k1-target");
    for (kind, target) in [
        (WaitKind::ProcessTerminal, Some(target)),
        (WaitKind::Timer, None),
        (
            WaitKind::ChildSession,
            Some(ProcessId::fixture("k1-child-turn")),
        ),
    ] {
        let (wait, minted) = pin(&cx, kind, target, Some(LONG)).await?;
        ensure!(minted.is_none(), "a {kind:?} wait was minted a host key");
        let reserved_before = row(backend, &wait).await?;
        let answered = waits::resolve_host(backend, &wait.id().to_hex(), answer("forged")).await?;
        ensure!(
            answered == ResolveAnswer::ReservedKind,
            "a host resolve of a {kind:?} wait answered {answered:?}"
        );
        let mut tx = MailTx::new();
        tx.write(MailDomainWrite::ResolveWait(WaitResolution {
            id: wait.id(),
            by_host: true,
            digest: "forged".into(),
            resolution_ref: "{}".into(),
        }));
        let commit = backend
            .durable()
            .commit_mail(tx, CommitLabel::WAIT_RESOLVE)
            .await?;
        ensure!(
            commit.answers == [MailAnswer::ResolveWait(ResolveAnswer::ReservedKind)],
            "the store let a host resolve a {kind:?} wait: {:?}",
            commit.answers
        );
        ensure!(
            row(backend, &wait).await? == reserved_before,
            "a refused {kind:?} resolve changed the wait row"
        );
    }
    let answered = waits::resolve_host(backend, &key, answer("genuine")).await?;
    ensure!(
        answered == ResolveAnswer::Resolved,
        "the genuine key answered {answered:?} after the forgeries"
    );
    Ok(())
}

/// First winner: a resolution before, during and after the await counts
/// once. A repeat with the same digest answers `AlreadyResolved`, with
/// another `Conflict`; a resolution after a timeout won answers
/// `Revoked`; and a timeout racing a resolution leaves one
/// committed winner that both sides agree on.
///
/// # Errors
///
/// The first rule broken.
pub async fn the_first_resolution_wins(backend: &Backend) -> LawResult {
    let lease = node(backend, "first-winner", TTL_MILLIS).await?;
    let cx = own(
        backend,
        &lease,
        &session_actor("first-winner")?,
        turn_scope("first-winner"),
    )
    .await?;

    // Before the await.
    let (before, key) = pin(&cx, WaitKind::ToolCompletion, None, Some(LONG)).await?;
    let key = key_of(key)?;
    let first = waits::resolve_host(backend, &key, answer("first")).await?;
    ensure!(first == ResolveAnswer::Resolved, "before: {first:?}");
    let repeat = waits::resolve_host(backend, &key, answer("first")).await?;
    ensure!(
        repeat == ResolveAnswer::AlreadyResolved,
        "a same-digest repeat answered {repeat:?}"
    );
    let other = waits::resolve_host(backend, &key, answer("second")).await?;
    ensure!(
        other == ResolveAnswer::Conflict,
        "another digest answered {other:?}"
    );
    let won = waits::race(&cx, &[before]).await?;
    ensure!(
        won == RaceWinner::Resolved {
            wait: before,
            resolution: answer("first"),
        },
        "before: the race saw {won:?}"
    );

    // During the await.
    let (during, key) = pin(&cx, WaitKind::Custom, None, Some(LONG)).await?;
    let key = key_of(key)?;
    let racing = crate::task::spawn({
        let cx = cx.clone();
        async move { waits::race(&cx, &[during]).await }
    });
    tokio::time::sleep(CLAIM_POLL * 4).await;
    ensure!(
        !racing.is_finished(),
        "the race ended before any resolution"
    );
    let resolved = waits::resolve_host(backend, &key, answer("during")).await?;
    ensure!(resolved == ResolveAnswer::Resolved, "during: {resolved:?}");
    let won = racing
        .await
        .map_err(|error| LawBroken(error.to_string()))??;
    ensure!(
        won == RaceWinner::Resolved {
            wait: during,
            resolution: answer("during"),
        },
        "during: the race saw {won:?}"
    );

    // After the await's timeout won.
    let (after, key) = pin(&cx, WaitKind::ToolCompletion, None, Some(SHORT)).await?;
    let key = key_of(key)?;
    let won = waits::race(&cx, &[after]).await?;
    ensure!(
        won == RaceWinner::TimedOut(after),
        "after: the race saw {won:?}"
    );
    let late = waits::resolve_host(backend, &key, answer("late")).await?;
    ensure!(
        late == ResolveAnswer::Revoked,
        "a resolution after the timeout answered {late:?}"
    );
    ensure!(
        row(backend, &after).await?.lifecycle.state() == WaitState::TimedOut,
        "a late resolution overwrote the timeout"
    );

    // A timeout racing a resolution at the deadline.
    for round in 0..8_u64 {
        let (wait, key) = pin(&cx, WaitKind::ToolCompletion, None, Some(SHORT)).await?;
        let key = key_of(key)?;
        tokio::time::sleep(SHORT + Duration::from_millis(round)).await;
        let settling = crate::task::spawn({
            let cx = cx.clone();
            async move { waits::settle_due(&cx).await }
        });
        let resolved = waits::resolve_host(backend, &key, answer("at the deadline")).await?;
        settling
            .await
            .map_err(|error| LawBroken(error.to_string()))??;
        let stored = row(backend, &wait).await?;
        let agreed = match resolved {
            ResolveAnswer::Resolved => stored.lifecycle.state() == WaitState::Resolved,
            ResolveAnswer::Revoked => stored.lifecycle.state() == WaitState::TimedOut,
            _ => false,
        };
        ensure!(
            agreed,
            "round {round}: the resolve answered {resolved:?} but the row is {:?}",
            stored.lifecycle.state()
        );
    }
    Ok(())
}

/// What a T1 activation saw.
#[derive(Default)]
struct Seen {
    claims: Vec<ClaimCause>,
    settled: Vec<WaitRow>,
}

struct SettleOnClaim {
    backend: Backend,
    seen: Arc<Mutex<Seen>>,
}

#[async_trait::async_trait]
impl Activation for SettleOnClaim {
    async fn activate(&self, owned: Owned) -> Exit {
        let cx = context(
            &self.backend,
            owned.actor(),
            owned.epoch(),
            turn_scope("t1"),
        );
        let settled = waits::settle_due(&cx).await.unwrap_or_default();
        let mut rows = Vec::new();
        for wait in &settled {
            if let Ok(Some(row)) = self.backend.durable().wait(&wait.id()).await {
                rows.push(row);
            }
        }
        {
            let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
            seen.claims.push(owned.cause());
            seen.settled.extend(rows);
        }
        if let Ok(mut tx) = cx.begin().await {
            tx.ack_seen().give_up(Release::Idle);
            let _ = cx.commit(tx, CommitLabel::new("law.release")).await;
        }
        Exit::Released
    }
}

/// T1: an actor released as `waiting` past a wait's deadline is claimed by
/// the ordinary claim and settles `TimedOut` within the claim poll plus ε;
/// the timeout is committed in the activation's first owner transaction,
/// before it does anything else.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_waiting_actor_past_its_deadline_times_out_within_the_claim_poll(
    backend: &Backend,
) -> LawResult {
    let actor = session_actor("t1")?;
    let minting = node(backend, "t1-minting", TTL_MILLIS).await?;
    let cx = own(backend, &minting, &actor, turn_scope("t1")).await?;
    let (wait, _) = pin(&cx, WaitKind::ToolCompletion, None, Some(SHORT)).await?;
    let deadline = row(backend, &wait)
        .await?
        .purpose
        .deadline()
        .ok_or_else(|| LawBroken("the wait has no deadline".into()))?;
    let mut tx = cx.begin().await?;
    tx.give_up(Release::Waiting {
        next_due: Some(deadline),
    });
    cx.commit(tx, CommitLabel::new("law.release")).await?;
    backend.durable().release_node(&minting).await?;

    let seen = Arc::new(Mutex::new(Seen::default()));
    let runner = Runner::new(
        Arc::clone(backend.durable()),
        backend.clock(),
        RunnerConfig {
            node: NodeId::new("t1-serving"),
            decodes: vec![formats()],
            lease: backend.config().lease(),
            max_active: 4,
            claim_batch: 4,
        },
        Arc::new(SettleOnClaim {
            backend: backend.clone(),
            seen: Arc::clone(&seen),
        }),
    );
    let stop = CancellationToken::new();
    let serving = crate::task::spawn({
        let stop = stop.clone();
        async move { runner.run(async move { stop.cancelled().await }).await }
    });
    let give_up_at = std::time::Instant::now() + SHORT + CLAIM_POLL + EPSILON * 4;
    while seen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .settled
        .is_empty()
        && std::time::Instant::now() < give_up_at
    {
        tokio::time::sleep(CLAIM_POLL).await;
    }
    stop.cancel();
    let _ = serving.await;
    let seen = seen.lock().unwrap_or_else(PoisonError::into_inner);
    ensure!(
        seen.claims.first() == Some(&ClaimCause::Due),
        "the actor was not claimed as due: {:?}",
        seen.claims
    );
    let settled = seen
        .settled
        .first()
        .ok_or_else(|| LawBroken("no wait settled".into()))?;
    ensure!(
        settled.id == wait.id() && settled.lifecycle.state() == WaitState::TimedOut,
        "the claimed wait settled as {:?}",
        settled.lifecycle.state()
    );
    let resolved_at = settled
        .lifecycle
        .resolved_at()
        .ok_or_else(|| LawBroken("a timed-out wait has no settle time".into()))?;
    let bound = deadline
        .after_millis(i64::try_from((CLAIM_POLL + EPSILON).as_millis()).unwrap_or(i64::MAX));
    ensure!(
        resolved_at >= deadline && resolved_at <= bound,
        "the wait due at {deadline:?} settled at {resolved_at:?}, past {bound:?}"
    );
    Ok(())
}

/// L-F1: an unresolved wait suspends (the actor releases as `waiting` and
/// holds nothing) and resumes on its resolution, which wakes the actor.
///
/// # Errors
///
/// The first rule broken.
pub async fn an_unresolved_wait_suspends_and_resumes_on_resolution(backend: &Backend) -> LawResult {
    let actor = session_actor("suspend")?;
    let lease = node(backend, "suspend", TTL_MILLIS).await?;
    let cx = own(backend, &lease, &actor, turn_scope("suspend")).await?;
    let (wait, key) = pin(&cx, WaitKind::ToolCompletion, None, Some(LONG)).await?;
    let key = key_of(key)?;
    let racing = crate::task::spawn({
        let cx = cx.clone();
        async move { waits::race(&cx, &[wait]).await }
    });
    tokio::time::sleep(CLAIM_POLL * 4).await;
    ensure!(
        !racing.is_finished(),
        "the race ended with nothing resolved"
    );
    racing.abort();
    let mut tx = cx.begin().await?;
    tx.give_up(Release::Waiting {
        next_due: cx.next_due(),
    });
    cx.commit(tx, CommitLabel::new("law.release")).await?;
    let suspended = backend.durable().actor(&actor).await?;
    ensure!(
        suspended.as_ref().map(|actor| actor.state) == Some(lash_durable::ActorState::Waiting),
        "the suspended actor is {suspended:?}"
    );
    let resolved = waits::resolve_host(backend, &key, answer("resumed")).await?;
    ensure!(resolved == ResolveAnswer::Resolved, "resolve: {resolved:?}");
    let woken = backend.durable().actor(&actor).await?;
    ensure!(
        woken.as_ref().map(|actor| actor.state) == Some(lash_durable::ActorState::Ready),
        "the resolution did not ready the actor: {woken:?}"
    );
    let resumed = claim(backend, &lease, &actor, turn_scope("suspend")).await?;
    let won = waits::race(&resumed, &[wait]).await?;
    ensure!(
        won == RaceWinner::Resolved {
            wait,
            resolution: answer("resumed"),
        },
        "the resumed race saw {won:?}"
    );
    Ok(())
}

/// L-F1: a completion that arrives before the await is already resolved
/// when the await reads it.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_completion_before_the_await_is_already_resolved(backend: &Backend) -> LawResult {
    let lease = node(backend, "early", TTL_MILLIS).await?;
    let cx = own(
        backend,
        &lease,
        &session_actor("early")?,
        turn_scope("early"),
    )
    .await?;
    let (wait, key) = pin(&cx, WaitKind::Custom, None, Some(LONG)).await?;
    let resolved = waits::resolve_host(backend, &key_of(key)?, answer("early")).await?;
    ensure!(resolved == ResolveAnswer::Resolved, "resolve: {resolved:?}");
    let won = waits::await_external(&cx, &wait).await?;
    ensure!(
        won == waits::ExternalWaitOutcome::Resolved(answer("early")),
        "the await saw {won:?}"
    );
    Ok(())
}

/// L-F1: a duplicate resolution leaves the first one the winner.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_duplicate_resolution_keeps_the_first(backend: &Backend) -> LawResult {
    let lease = node(backend, "duplicate", TTL_MILLIS).await?;
    let cx = own(
        backend,
        &lease,
        &session_actor("duplicate")?,
        turn_scope("duplicate"),
    )
    .await?;
    let (wait, key) = pin(&cx, WaitKind::ToolCompletion, None, Some(LONG)).await?;
    let key = key_of(key)?;
    let (first, second) = tokio::join!(
        waits::resolve_host(backend, &key, answer("one")),
        waits::resolve_host(backend, &key, answer("two")),
    );
    let answers = [first?, second?];
    ensure!(
        answers
            .iter()
            .filter(|answer| **answer == ResolveAnswer::Resolved)
            .count()
            == 1
            && answers.contains(&ResolveAnswer::Conflict),
        "two resolutions answered {answers:?}"
    );
    let winner = if answers[0] == ResolveAnswer::Resolved {
        "one"
    } else {
        "two"
    };
    let won = waits::race(&cx, &[wait]).await?;
    ensure!(
        won == RaceWinner::Resolved {
            wait,
            resolution: answer(winner),
        },
        "the race saw {won:?}, not the first winner `{winner}`"
    );
    Ok(())
}

/// L-F1: the awaiter's own cancel mail ends its wait `Cancelled`; the wait
/// stays pending for whoever resolves it.
///
/// # Errors
///
/// The first rule broken.
pub async fn the_awaiters_cancel_ends_its_wait(backend: &Backend) -> LawResult {
    let actor = session_actor("cancel")?;
    let lease = node(backend, "cancel", TTL_MILLIS).await?;
    let cx = own(backend, &lease, &actor, turn_scope("cancel")).await?;
    let (wait, _) = pin(&cx, WaitKind::ToolCompletion, None, Some(LONG)).await?;
    let racing = crate::task::spawn({
        let cx = cx.clone();
        async move { waits::await_external(&cx, &wait).await }
    });
    tokio::time::sleep(CLAIM_POLL * 4).await;
    ensure!(!racing.is_finished(), "the await ended before its cancel");
    send_cancel(backend, &actor).await?;
    let won = racing
        .await
        .map_err(|error| LawBroken(error.to_string()))??;
    ensure!(
        won == waits::ExternalWaitOutcome::Cancelled,
        "the cancelled await saw {won:?}"
    );
    ensure!(
        row(backend, &wait).await?.lifecycle.state() == WaitState::Pending,
        "the awaiter's cancel settled the wait itself"
    );
    Ok(())
}

/// L-F1: a timeout racing a completion has exactly one winner, which the
/// race and the resolver agree on.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_timeout_racing_a_completion_has_one_winner(backend: &Backend) -> LawResult {
    let lease = node(backend, "timeout-race", TTL_MILLIS).await?;
    let cx = own(
        backend,
        &lease,
        &session_actor("timeout-race")?,
        turn_scope("timeout-race"),
    )
    .await?;
    for round in 0..8_u64 {
        let (wait, key) = pin(&cx, WaitKind::Custom, None, Some(SHORT)).await?;
        let key = key_of(key)?;
        let racing = crate::task::spawn({
            let cx = cx.clone();
            async move { waits::race(&cx, &[wait]).await }
        });
        tokio::time::sleep(SHORT - CLAIM_POLL + Duration::from_millis(round * 6)).await;
        let resolved = waits::resolve_host(backend, &key, answer("racing")).await?;
        let won = racing
            .await
            .map_err(|error| LawBroken(error.to_string()))??;
        let agreed = match (&won, resolved) {
            (RaceWinner::Resolved { resolution, .. }, ResolveAnswer::Resolved) => {
                *resolution == answer("racing")
            }
            (RaceWinner::TimedOut(_), ResolveAnswer::Revoked) => true,
            _ => false,
        };
        ensure!(
            agreed,
            "round {round}: the race saw {won:?} but the resolve answered {resolved:?}"
        );
    }
    Ok(())
}

/// L-F1: when the owner dies, another node claims the actor and its wait
/// keeps the same row, key and deadline; the dead owner can no longer
/// commit.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_wait_survives_its_owners_death_with_the_same_key_and_deadline(
    backend: &Backend,
) -> LawResult {
    let actor = session_actor("failover")?;
    let dying = node(backend, "failover-dying", 200).await?;
    let cx = own(backend, &dying, &actor, turn_scope("failover")).await?;
    let (wait, key) = pin(&cx, WaitKind::ToolCompletion, None, Some(LONG)).await?;
    let key = key_of(key)?;
    let minted = row(backend, &wait).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let surviving = node(backend, "failover-surviving", TTL_MILLIS).await?;
    let reaped = backend.durable().reap(&surviving).await?;
    ensure!(
        reaped.iter().any(|reaped| reaped.actor == actor),
        "the dead owner's actor was not reaped: {reaped:?}"
    );
    let resumed = claim(backend, &surviving, &actor, turn_scope("failover")).await?;
    let pending = backend.durable().pending_waits(&actor).await?;
    ensure!(
        pending == [minted.clone()],
        "the new owner sees {pending:?}, not the minted wait"
    );
    ensure!(
        waits::outstanding_keys(backend, &actor)
            .await?
            .iter()
            .map(PinnedKey::as_str)
            .eq([key.as_str()]),
        "the new owner's key differs from the minted one"
    );
    let resolved = waits::resolve_host(backend, &key, answer("after failover")).await?;
    ensure!(resolved == ResolveAnswer::Resolved, "resolve: {resolved:?}");
    let won = waits::race(&resumed, &[WaitRef::new(minted.id, minted.purpose.kind())]).await?;
    ensure!(
        won == RaceWinner::Resolved {
            wait,
            resolution: answer("after failover"),
        },
        "the new owner's race saw {won:?}"
    );
    ensure!(
        matches!(cx.begin().await, Err(DurableError::OwnershipLost(_))),
        "the dead owner can still open a transaction"
    );
    ensure!(
        row(backend, &wait).await?.purpose.deadline() == minted.purpose.deadline(),
        "failover moved the deadline"
    );
    Ok(())
}

/// L-F2: a key that never resolves settles `TimedOut` at its deadline, in a
/// committed `wait.timeout`.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_key_that_never_resolves_times_out(backend: &Backend) -> LawResult {
    let lease = node(backend, "never", TTL_MILLIS).await?;
    let cx = own(
        backend,
        &lease,
        &session_actor("never")?,
        turn_scope("never"),
    )
    .await?;
    let (wait, _) = pin(&cx, WaitKind::ToolCompletion, None, Some(SHORT)).await?;
    let won = waits::await_external(&cx, &wait).await?;
    ensure!(
        won == waits::ExternalWaitOutcome::TimedOut,
        "the await saw {won:?}"
    );
    let stored = row(backend, &wait).await?;
    ensure!(
        stored.lifecycle.state() == WaitState::TimedOut
            && stored.lifecycle.resolved_at() >= stored.purpose.deadline()
            && stored.lifecycle.resolved_at().is_some(),
        "the timed-out wait is stored as {stored:?}"
    );
    Ok(())
}

/// W1 (part): `await_process` resolves with the process's outcome when its
/// terminal transaction resolves the wait, times out at its deadline, and
/// ends `Cancelled` on the awaiter's cancel. The A↔B cycle is L6's.
///
/// # Errors
///
/// The first rule broken.
pub async fn await_process_is_bounded_and_cancellable(backend: &Backend) -> LawResult {
    let actor = session_actor("awaiter")?;
    let lease = node(backend, "w1", TTL_MILLIS).await?;
    let cx = own(backend, &lease, &actor, turn_scope("awaiter")).await?;
    let process = ProcessId::fixture("w1-child");
    let process_actor =
        ActorKey::process(process.as_str()).map_err(|error| LawBroken(error.to_string()))?;
    let child = own(
        backend,
        &lease,
        &process_actor,
        AdmittedScope::process(process.clone()),
    )
    .await?;
    let deadline = |after: Duration| async move {
        let now: DurableInstant = backend.durable().now().await?;
        Ok::<_, LawBroken>(WaitDeadline::at_instant(
            now.after_millis(i64::try_from(after.as_millis()).unwrap_or(i64::MAX)),
        ))
    };
    let outcome = crate::ProcessAwaitOutput::NoLongerRetained {
        terminal_label: crate::RetiredProcessStatus::Completed,
        pruned_at_ms: 1,
    };

    let awaiting = crate::task::spawn({
        let cx = cx.clone();
        let process = process.clone();
        let deadline = deadline(LONG).await?;
        async move { waits::await_process(&cx, &process, deadline).await }
    });
    tokio::time::sleep(CLAIM_POLL * 4).await;
    let mut tx = child.begin().await?;
    waits::resolve_process_terminal_waits(&mut tx, &process, &outcome)?;
    child.commit(tx, CommitLabel::new("law.terminal")).await?;
    let ended = awaiting
        .await
        .map_err(|error| LawBroken(error.to_string()))??;
    ensure!(
        ended == ProcessWaitOutcome::Resolved(outcome),
        "the resolved await saw {ended:?}"
    );

    let other = ProcessId::fixture("w1-silent");
    let ended = waits::await_process(&cx, &other, deadline(SHORT).await?).await?;
    ensure!(
        ended == ProcessWaitOutcome::TimedOut,
        "the bounded await saw {ended:?}"
    );

    let awaiting = crate::task::spawn({
        let cx = cx.clone();
        let deadline = deadline(LONG).await?;
        async move { waits::await_process(&cx, &other, deadline).await }
    });
    tokio::time::sleep(CLAIM_POLL * 4).await;
    ensure!(!awaiting.is_finished(), "the await ended before its cancel");
    send_cancel(backend, &actor).await?;
    let ended = awaiting
        .await
        .map_err(|error| LawBroken(error.to_string()))??;
    ensure!(
        ended == ProcessWaitOutcome::Cancelled,
        "the cancelled await saw {ended:?}"
    );
    Ok(())
}
