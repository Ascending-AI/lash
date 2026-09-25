//! The logical root's durable record (FIG-3600 S7, FIG-3607 contract 2 and
//! item 8): one row per `(session, root)` holding the root's terminal
//! evidence, beside the bindings of the accepted inputs the root drove.
//!
//! A logical root is what a host names a turn by. Its physical turns (a frame
//! switch's follow-on, an S4 follow-on, a redrive) all derive from it, and
//! exactly one of them ends it. The evidence of that end is root-addressed and
//! written in the transaction that makes it true: the head commit of the
//! root's final physical turn, or the settlement of a queued run that ended
//! without one. Once written it is never replaced: a second, different
//! terminal is refused with [`StoreError::RootAlreadyTerminal`] and the first
//! stands (ADR 0105 law L-S6), while rewriting the same terminal is a no-op.
//!
//! The evidence outlives the session's ingress rows (vacuum prunes those) and
//! lasts until the session is deleted. After deletion the factory still
//! answers every root of the session from its retained deletion intent
//! ([`RootTerminalCause::SessionDeleted`]).
//!
//! [`TurnCommitId`] lives here, below the engine contract, because the stores
//! write it; `lash_core::engine` re-exports it unchanged, as it does
//! [`DriveFence`](super::DriveFence).

use serde::{Deserialize, Serialize};

use super::StoreError;
use super::control_intent::ControlIntentId;
use crate::{InputId, RuntimeErrorCode, SessionId, TurnId};
use lash_sansio::TurnStop;

/// A turn commit's identity: the logical root and the physical ordinal of
/// the attempt that commits it. Derived, never minted: the ordinal is the
/// physical turn's position within its root
/// ([`QueuedRunPosition::derive_turn_id`](super::QueuedRunPosition::derive_turn_id)).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnCommitId {
    root: TurnId,
    ordinal: u32,
}

impl TurnCommitId {
    pub fn new(root: TurnId, ordinal: u32) -> Self {
        Self { root, ordinal }
    }

    pub fn root(&self) -> &TurnId {
        &self.root
    }

    pub fn ordinal(&self) -> u32 {
        self.ordinal
    }

    /// The commit id of physical turn `turn` of `root`: `Some` exactly when
    /// `turn` is one of `root`'s physical turns.
    #[must_use]
    pub fn of_physical_turn(root: &TurnId, turn: &TurnId) -> Option<Self> {
        let ordinal = super::QueuedRunPosition::physical_ordinal_of(root, turn)?;
        Some(Self::new(root.clone(), u32::try_from(ordinal).ok()?))
    }
}

/// How a logical root ended, as a host reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootTerminalKind {
    Answered,
    Failed,
    Cancelled,
}

impl RootTerminalKind {
    /// The stored code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// The kind a stored code names.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "answered" => Some(Self::Answered),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// The kind a physical turn's stop names: none is an answer, a cancelled
    /// stop is a cancellation, and every other stop is a failure.
    #[must_use]
    pub fn of_stop(stop: Option<&TurnStop>) -> Self {
        match stop {
            None => Self::Answered,
            Some(TurnStop::Cancelled { .. }) => Self::Cancelled,
            Some(_) => Self::Failed,
        }
    }
}

/// Why a root is terminal: the transaction that wrote its evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum RootTerminalCause {
    /// The root's final physical turn committed (the head-commit
    /// transaction).
    Committed {
        commit: TurnCommitId,
        turn: TurnId,
        stop: Option<TurnStop>,
    },
    /// A queued run settled failed without a head commit.
    SettledFailed {
        code: RuntimeErrorCode,
        message: String,
    },
    /// A queued run settled empty: admitted, and nothing ran.
    SettledEmpty,
    /// An operator cancelled the parked root (its control intent).
    OperatorCancelled { intent: ControlIntentId },
    /// An operator forked the parked root; its held inputs now drive
    /// `new_root`.
    Forked {
        intent: ControlIntentId,
        new_root: Option<TurnId>,
    },
    /// The session was deleted (its `CloseSession` intent).
    SessionDeleted { intent: ControlIntentId },
}

impl RootTerminalCause {
    /// The kind this cause answers.
    #[must_use]
    pub fn kind(&self) -> RootTerminalKind {
        match self {
            Self::Committed { stop, .. } => RootTerminalKind::of_stop(stop.as_ref()),
            Self::SettledFailed { .. } => RootTerminalKind::Failed,
            Self::SettledEmpty => RootTerminalKind::Answered,
            Self::OperatorCancelled { .. } | Self::Forked { .. } | Self::SessionDeleted { .. } => {
                RootTerminalKind::Cancelled
            }
        }
    }
}

/// A logical root's terminal evidence, as the store answers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootTerminal {
    pub session_id: SessionId,
    pub root: TurnId,
    pub kind: RootTerminalKind,
    pub cause: RootTerminalCause,
    /// The head revision of the terminal transaction; `None` when the
    /// transaction moved no head (a settlement, an intent) or the answer comes
    /// from a deletion tombstone.
    pub head_revision: Option<u64>,
    pub at_ms: u64,
}

impl RootTerminal {
    /// The commit that ended the root, when a head commit did.
    #[must_use]
    pub fn commit(&self) -> Option<&TurnCommitId> {
        match &self.cause {
            RootTerminalCause::Committed { commit, .. } => Some(commit),
            _ => None,
        }
    }

    /// Whether `other` is this same terminal: one cause and one kind. The
    /// instant and head revision are the writer's, never part of identity,
    /// so a retried writer's rewrite is recognized as the same terminal.
    #[must_use]
    pub fn same_terminal(&self, other: &Self) -> bool {
        self.session_id == other.session_id
            && self.root == other.root
            && self.kind == other.kind
            && self.cause == other.cause
    }
}

/// What a head commit writes for its root: present exactly on the commit of
/// a root's final physical turn. Replaces P0's `TurnTerminalEvidence`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootTerminalWrite {
    pub root: TurnId,
    pub commit: TurnCommitId,
    /// The physical turn that reached the terminal.
    pub turn: TurnId,
    /// Why the turn stopped; `None` when it completed.
    pub stop: Option<TurnStop>,
}

impl RootTerminalWrite {
    /// The kind this write answers.
    #[must_use]
    pub fn kind(&self) -> RootTerminalKind {
        RootTerminalKind::of_stop(self.stop.as_ref())
    }

    /// The cause this write records.
    #[must_use]
    pub fn cause(&self) -> RootTerminalCause {
        RootTerminalCause::Committed {
            commit: self.commit.clone(),
            turn: self.turn.clone(),
            stop: self.stop.clone(),
        }
    }

    /// The evidence this write becomes in session `session_id`'s transaction
    /// at `head_revision`.
    #[must_use]
    pub fn into_terminal(
        self,
        session_id: SessionId,
        head_revision: u64,
        at_ms: u64,
    ) -> RootTerminal {
        RootTerminal {
            session_id,
            kind: self.kind(),
            cause: self.cause(),
            root: self.root,
            head_revision: Some(head_revision),
            at_ms,
        }
    }
}

/// The evidence the settlement of queued run `root` writes, when it writes
/// any: a failed or empty settlement ends the root without a head commit. A
/// completed settlement rides its head commit's
/// [`RootTerminalWrite`], and forgetting an unworked run ends nothing a host
/// could name.
#[must_use]
pub fn settled_queued_root_cause(terminal: &super::QueuedRunTerminal) -> Option<RootTerminalCause> {
    match terminal {
        super::QueuedRunTerminal::Failed { code, message } => {
            Some(RootTerminalCause::SettledFailed {
                code: code.clone(),
                message: message.clone(),
            })
        }
        super::QueuedRunTerminal::Empty => Some(RootTerminalCause::SettledEmpty),
        super::QueuedRunTerminal::Completed { .. } => None,
    }
}

/// Decide one terminal write against the stored evidence of its root.
///
/// No stored evidence: write. The same terminal: a retried writer, answer
/// without writing. Any other terminal: refuse, and the stored one stands
/// (ADR 0105 law L-S6).
pub fn decide_root_terminal_write(
    stored: Option<&RootTerminal>,
    write: &RootTerminal,
) -> Result<RootTerminalWriteDecision, StoreError> {
    match stored {
        None => Ok(RootTerminalWriteDecision::Write),
        Some(stored) if stored.same_terminal(write) => {
            Ok(RootTerminalWriteDecision::AlreadyWritten)
        }
        Some(stored) => Err(StoreError::RootAlreadyTerminal {
            session_id: write.session_id.clone(),
            root: write.root.clone(),
            by: Box::new(stored.cause.clone()),
        }),
    }
}

/// What a backend does for one terminal write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootTerminalWriteDecision {
    Write,
    AlreadyWritten,
}

/// The stored form of a root's terminal: the columns a backend writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRootTerminal {
    pub kind: &'static str,
    pub cause_json: String,
    pub head_revision: Option<u64>,
    pub at_ms: u64,
}

impl RootTerminal {
    /// The columns a backend stores.
    pub fn to_stored(&self) -> Result<StoredRootTerminal, StoreError> {
        Ok(StoredRootTerminal {
            kind: self.kind.as_str(),
            cause_json: serde_json::to_string(&self.cause).map_err(|error| {
                StoreError::RecordEncodingFailed {
                    record_kind: "RootTerminal".to_string(),
                    message: error.to_string(),
                }
            })?,
            head_revision: self.head_revision,
            at_ms: self.at_ms,
        })
    }

    /// Decode the columns a backend stored for `root` of `session_id`.
    pub fn from_stored(
        session_id: SessionId,
        root: TurnId,
        kind: &str,
        cause_json: &str,
        head_revision: Option<u64>,
        at_ms: u64,
    ) -> Result<Self, StoreError> {
        let corrupt = |message: String| StoreError::StoredDataCorrupt {
            record_kind: "RootTerminal",
            message,
        };
        let kind = RootTerminalKind::from_code(kind)
            .ok_or_else(|| corrupt(format!("unknown root terminal kind `{kind}`")))?;
        let cause: RootTerminalCause = serde_json::from_str(cause_json)
            .map_err(|error| corrupt(format!("root terminal cause: {error}")))?;
        if cause.kind() != kind {
            return Err(corrupt(format!(
                "root terminal kind `{}` disagrees with its cause's `{}`",
                kind.as_str(),
                cause.kind().as_str()
            )));
        }
        Ok(Self {
            session_id,
            root,
            kind,
            cause,
            head_revision,
            at_ms,
        })
    }
}

/// The per-session store half of logical roots: the terminal read, and the
/// bindings of accepted inputs to the roots that drive them.
#[async_trait::async_trait]
pub trait RootStore: Send + Sync {
    /// The terminal evidence of `root` in `session_id`, if it has any.
    async fn root_terminal(
        &self,
        session_id: &SessionId,
        root: &TurnId,
    ) -> Result<Option<RootTerminal>, StoreError>;

    /// The root that took accepted input `input`: the root its claim bound it
    /// to, or the queued run it is a member of. `None` while it is pending.
    async fn root_of_input(
        &self,
        session_id: &SessionId,
        input: &InputId,
    ) -> Result<Option<TurnId>, StoreError>;

    /// The root input `input` is bound to, if any: admission drives a bound
    /// input under that root (a fork binds held inputs to its new root).
    async fn root_binding(
        &self,
        session_id: &SessionId,
        input: &InputId,
    ) -> Result<Option<TurnId>, StoreError>;

    /// Bind each of `inputs` to `root`, set-if-absent, and open `root`'s
    /// record if it has none. The recorded claim step binds the rows it
    /// claimed. A binding to another root is refused and nothing is written.
    async fn bind_root_inputs(
        &self,
        session_id: &SessionId,
        root: &TurnId,
        inputs: &[InputId],
    ) -> Result<(), StoreError>;
}

/// An in-memory root ledger for store doubles that keep no SQL rows. It
/// decides every write with [`decide_root_terminal_write`], exactly as a SQL
/// backend does inside its transaction.
#[derive(Debug, Default)]
pub struct InMemoryRootLedger {
    state: std::sync::Mutex<InMemoryRoots>,
}

#[derive(Debug, Default)]
struct InMemoryRoots {
    terminals: std::collections::BTreeMap<(SessionId, TurnId), RootTerminal>,
    bindings: std::collections::BTreeMap<(SessionId, InputId), TurnId>,
}

impl InMemoryRootLedger {
    fn state(&self) -> std::sync::MutexGuard<'_, InMemoryRoots> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Write `terminal`, deciding it against the stored evidence.
    pub fn write_terminal(&self, terminal: RootTerminal) -> Result<(), StoreError> {
        let mut state = self.state();
        let key = (terminal.session_id.clone(), terminal.root.clone());
        if decide_root_terminal_write(state.terminals.get(&key), &terminal)?
            == RootTerminalWriteDecision::Write
        {
            state.terminals.insert(key, terminal);
        }
        Ok(())
    }

    /// [`RootStore::root_terminal`] over this ledger.
    #[must_use]
    pub fn terminal(&self, session_id: &SessionId, root: &TurnId) -> Option<RootTerminal> {
        self.state()
            .terminals
            .get(&(session_id.clone(), root.clone()))
            .cloned()
    }

    /// [`RootStore::root_binding`] over this ledger.
    #[must_use]
    pub fn binding(&self, session_id: &SessionId, input: &InputId) -> Option<TurnId> {
        self.state()
            .bindings
            .get(&(session_id.clone(), input.clone()))
            .cloned()
    }

    /// [`RootStore::bind_root_inputs`] over this ledger.
    pub fn bind(
        &self,
        session_id: &SessionId,
        root: &TurnId,
        inputs: &[InputId],
    ) -> Result<(), StoreError> {
        let mut state = self.state();
        for input in inputs {
            if let Some(bound) = state.bindings.get(&(session_id.clone(), input.clone()))
                && bound != root
            {
                return Err(root_binding_conflict(session_id, input, bound, root));
            }
        }
        for input in inputs {
            state
                .bindings
                .insert((session_id.clone(), input.clone()), root.clone());
        }
        Ok(())
    }

    /// Forget everything session `session_id` holds (its deletion).
    pub fn forget_session(&self, session_id: &SessionId) {
        let mut state = self.state();
        state
            .terminals
            .retain(|(session, _), _| session != session_id);
        state
            .bindings
            .retain(|(session, _), _| session != session_id);
    }
}

/// The refusal of a binding that names another root than the one the input
/// is already bound to.
#[must_use]
pub fn root_binding_conflict(
    session_id: &SessionId,
    input: &InputId,
    bound: &TurnId,
    requested: &TurnId,
) -> StoreError {
    StoreError::Backend(format!(
        "accepted input `{input}` of session `{session_id}` is bound to root `{bound}`; \
         it cannot be bound to root `{requested}`"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed(root: &str, ordinal: u32, stop: Option<TurnStop>) -> RootTerminal {
        RootTerminalWrite {
            root: TurnId::from(root),
            commit: TurnCommitId::new(TurnId::from(root), ordinal),
            turn: crate::store::QueuedRunPosition::derive_turn_id(
                &TurnId::from(root),
                u64::from(ordinal),
            ),
            stop,
        }
        .into_terminal(SessionId::from("s"), 3, 10)
    }

    #[test]
    fn a_terminal_is_written_once_and_a_different_one_is_refused() {
        let first = committed("r", 0, None);
        assert_eq!(
            decide_root_terminal_write(None, &first).expect("first write"),
            RootTerminalWriteDecision::Write
        );
        let mut retried = first.clone();
        retried.at_ms = 99;
        retried.head_revision = Some(7);
        assert_eq!(
            decide_root_terminal_write(Some(&first), &retried).expect("a retried write"),
            RootTerminalWriteDecision::AlreadyWritten
        );
        let other = committed("r", 1, Some(TurnStop::ToolFailure));
        assert!(matches!(
            decide_root_terminal_write(Some(&first), &other),
            Err(StoreError::RootAlreadyTerminal { .. })
        ));
    }

    #[test]
    fn stored_columns_round_trip_and_refuse_a_kind_that_disagrees_with_its_cause() {
        let terminal = committed("r", 1, Some(TurnStop::ToolFailure));
        assert_eq!(terminal.kind, RootTerminalKind::Failed);
        let stored = terminal.to_stored().expect("encode");
        let decoded = RootTerminal::from_stored(
            terminal.session_id.clone(),
            terminal.root.clone(),
            stored.kind,
            &stored.cause_json,
            stored.head_revision,
            stored.at_ms,
        )
        .expect("decode");
        assert_eq!(decoded, terminal);
        assert!(
            RootTerminal::from_stored(
                terminal.session_id.clone(),
                terminal.root.clone(),
                "answered",
                &stored.cause_json,
                None,
                0,
            )
            .is_err()
        );
    }

    #[test]
    fn a_commit_id_names_only_its_roots_physical_turns() {
        let root = TurnId::from("host:1");
        assert_eq!(
            TurnCommitId::of_physical_turn(&root, &root),
            Some(TurnCommitId::new(root.clone(), 0))
        );
        let follow_on = crate::store::QueuedRunPosition::derive_turn_id(&root, 2);
        assert_eq!(
            TurnCommitId::of_physical_turn(&root, &follow_on),
            Some(TurnCommitId::new(root.clone(), 2))
        );
        assert_eq!(
            TurnCommitId::of_physical_turn(&root, &TurnId::from("other")),
            None
        );
    }
}
