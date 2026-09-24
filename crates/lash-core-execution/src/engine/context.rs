//! The engine context: the only futures a drive may await (ADR 0105 §1, §3).

use std::future::Future;
use std::pin::Pin;

use futures_util::future::FusedFuture;
use serde::{Deserialize, Serialize};

use super::contracts::{ChangeId, VersionRange};
use crate::TurnEvent;

/// What an engine supplies to drive code, implemented once per engine.
///
/// It has no `Send` or `Sync` bound. [`Op`](Self::Op) is the engine's own
/// concrete future type, so auto traits leak through monomorphization: a
/// drive instantiated over an engine whose ops are `Send` is `Send`, and one
/// over an engine whose ops are `!Send` is `!Send`. No `dyn Future` appears
/// anywhere on the workflow-side chain (ADR 0105 §8).
pub trait EngineContext {
    /// The one future type every operation of this engine returns.
    type Op<'a, T: 'a>: DurableOp<T> + 'a
    where
        Self: 'a;

    /// Deterministic time: recorded on first execution and returned unchanged
    /// on replay. Every deadline a command carries is taken from here.
    fn now_ms(&self) -> Self::Op<'_, EpochMs>;

    /// A neutral version decision (ADR 0105 §7): a fresh execution records
    /// `supported.max()`, and a replay returns the recorded value.
    fn version(&self, change: &'static ChangeId, supported: VersionRange) -> Self::Op<'_, u32>;

    /// Synchronous. It never wakes the drive, is keyed by replay key, and is
    /// suppressed or deduplicated on replay.
    fn observe(&self, observation: DriveObservation);

    /// A durable race that keeps its loser (ADR 0105 §3).
    ///
    /// Both arms are reborrowed, so the losing op stays owned by the caller,
    /// still pending and still durable. The first arm the engine records as
    /// completed wins; when both are ready at the same engine poll on a fresh
    /// execution, `first` wins. A replay returns the recorded winner.
    fn race<'r, 'a: 'r, 'b: 'r, A: 'a, B: 'b>(
        &'r self,
        first: Pin<&'r mut Self::Op<'a, A>>,
        second: Pin<&'r mut Self::Op<'b, B>>,
    ) -> Self::Op<'r, Winner<A, B>>
    where
        Self: 'a + 'b;

    /// The only way to give up an op. Dropping an op without disposing of it
    /// means [`Disposition::Abandon`].
    fn dispose<'a, T: 'a>(&self, op: Self::Op<'a, T>, how: Disposition) -> Self::Op<'_, Disposed>
    where
        Self: 'a;
}

/// A future an engine records, failing only with an [`EngineFault`].
///
/// It is fused, so a race may poll a finished arm again without panicking,
/// and `Unpin`, so a drive can race it through `Pin::new(&mut op)` and still
/// hand the same op to [`dispose`](EngineContext::dispose) by value. An engine
/// whose native futures are not `Unpin` boxes its concrete future type; that
/// is a box, not a `dyn Future`.
pub trait DurableOp<T>: Future<Output = Result<T, EngineFault>> + FusedFuture + Unpin {}

impl<T, F> DurableOp<T> for F where F: Future<Output = Result<T, EngineFault>> + FusedFuture + Unpin {}

/// Which arm of a [`race`](EngineContext::race) the engine recorded first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Winner<A, B> {
    First(A),
    Second(B),
}

/// How [`dispose`](EngineContext::dispose) gives up an op.
///
/// | Disposition | Step | Key wait | Child |
/// |---|---|---|---|
/// | `Abandon` | keeps running; its result is ignored and stays recorded | unregistered | the parent-close policy abandons it |
/// | `RequestCancel` | engine cooperative cancel; the external operation is idempotent under its operation id | resolved `Cancelled`, first writer wins | cancel requested |
/// | `AwaitCancelled` | as `RequestCancel`, then waits for the recorded terminal | as `RequestCancel` | as `RequestCancel`, then waits |
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Abandon,
    RequestCancel,
    AwaitCancelled,
}

/// What a [`dispose`](EngineContext::dispose) recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposed {
    /// The op was abandoned; whatever it records later is ignored.
    Abandoned,
    /// Cancellation was requested and not awaited.
    CancelRequested,
    /// Cancellation was awaited and the op's recorded terminal is a cancel.
    Cancelled,
    /// The op had already recorded its own terminal before the disposal
    /// reached it; that terminal is ignored.
    AlreadyCompleted,
}

/// A failure of the engine itself. The engine retries these and never
/// records one as a domain outcome: domain failures ride
/// [`EffectResult`](super::EffectResult).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EngineFault {
    /// The execution suspends here and resumes by replay.
    #[error("engine suspended the drive")]
    Suspended,
    /// A transient engine failure the engine retries under its own policy.
    #[error("retryable engine fault: {0}")]
    Retryable(EngineRetry),
    /// An engine failure no retry can recover.
    #[error("terminal engine fault: {0}")]
    Terminal(EngineTerminal),
}

/// A retryable engine failure. The message is diagnostic only.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct EngineRetry {
    pub message: String,
}

/// A terminal engine failure. The message is diagnostic only.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct EngineTerminal {
    pub message: String,
}

/// One observation a drive or step publishes, keyed by the replay key it
/// belongs to and its ordinal under that key, so a replay can suppress or
/// deduplicate it. Observation is never a decision input (ADR 0105 §1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DriveObservation {
    pub key: ReplayKey,
    pub ordinal: u32,
    pub event: TurnEvent,
}

/// Epoch milliseconds read from [`EngineContext::now_ms`]. Every deadline in a
/// command is one of these; no `Instant` crosses the command surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EpochMs(pub u64);

/// The durable address of one recorded operation: an effect's replay key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReplayKey(String);

impl ReplayKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ReplayKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
