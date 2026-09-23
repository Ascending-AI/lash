//! The durable journal shape a SQL substrate implements effect groups with
//! (FIG-1416, ADR 0065).
//!
//! [`group`](super::group) owns the *contract* types — what a caller declares
//! and what a host promises. This module owns the *store* types: the group
//! record a substrate writes before any child claims, what a guarded finalize
//! did, and a settled child read back by rank. They are separate because the
//! tiers are: Restate and Temporal implement the same contract with no group
//! row at all, and nothing here is visible to them.
//!
//! # Why a group row exists
//!
//! A group's settlement order is a journal fact, and on the SQL tiers that fact
//! is a monotonic counter. It cannot be `COALESCE(MAX(settlement_seq), 0) + 1`
//! over the group's children: the five-column lease fence
//! ([`EffectLeaseFence`](super::effect_replay_driver::EffectLeaseFence)) guards
//! `(scope_id, replay_key)` — one child's own row — so a `MAX` over siblings
//! reads outside the fence, and under `READ COMMITTED` two children finalizing
//! concurrently both read `MAX = k` and both write `k + 1`. That is a lost
//! update on the exact fact replay determinism rests on.
//!
//! The counter therefore lives on one row of its own, allocated by a single
//! `UPDATE … SET next_seq = next_seq + 1 … RETURNING next_seq`, which takes a
//! row lock and is correct under `READ COMMITTED`. `UNIQUE (group_key,
//! settlement_seq)` on the child table is the belt-and-braces backstop: a
//! regression to a read-then-max allocator seating two children at one rank
//! fails closed on a constraint violation instead of silently succeeding.
//!
//! # The three normative rules
//!
//! These are ADR 0065's, restated where an implementor reads them. Every SQL
//! backend must hold all three; the conformance tests in each store crate are
//! what hold them to it.
//!
//! **N1 — finalize ordering.** One transaction, in this order: perform the
//! existing fenced `UPDATE` on the child row; if its rowcount is 0, roll back
//! and report [`EffectFinalizeOutcome::FenceMoved`] with *no counter bump*;
//! only on rowcount 1, bump the group counter and write the returned value into
//! the child's `settlement_seq`; commit. Bumping first — or bumping
//! unconditionally and committing while reporting the fence miss — lets a
//! taken-over driver permanently advance a live group's counter, and the
//! `UNIQUE` index does **not** catch it, because the burned number is never
//! written to a child row. Under [rank](StoredGroupSettlement) a burned number
//! is harmless, but a taken-over driver burning numbers in a loop is unbounded
//! counter growth against a group it does not own.
//!
//! **N2 — lock order.** Any transaction touching both tables takes the **child
//! row before the group row**, without exception. [`EffectGroupRecord`] is
//! written and committed in its *own* transaction before any child claim is
//! issued, so the open path never holds a group-row lock while acquiring a
//! child-row lock. The asymmetry is otherwise an ABBA deadlock — a detected
//! abort rather than corruption, but one that surfaces as intermittent
//! group-open failures under concurrency and is expensive to diagnose for a
//! constraint that costs one sentence to state.
//!
//! **N3 — group-atomic retirement.** A group retires whole or not at all. Rank
//! is stable because allocation is monotonic and therefore only ever appends
//! *above* a consumed rank; a *deletion* below a consumed rank would shift
//! ranks even though allocation never does. Retiring a group's row and its
//! children in one transaction means no partially-retired group ever exists for
//! rank to be computed over.

use serde::{Deserialize, Serialize};

use super::effect_replay_driver::{EffectRowState, EffectTerminal};
use super::group::{GroupWakePolicy, LoserPolicy, RuntimeEffectGroup};
use crate::SessionId;

/// The durable group record a substrate writes before any of its children
/// claim.
///
/// Carries the group's identity, its journaled wake rule and declared loser
/// disposition, and the counter's home. `wake` and `loser_disposition` are also
/// folded into each child's envelope hash
/// ([`EffectGroupMembership`](super::group::EffectGroupMembership)), which is
/// what makes "replay cannot silently change the wake rule" hold on engine
/// tiers that keep no row like this one; the columns here are the SQL tiers'
/// second, independent home for the same two facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectGroupRecord {
    /// `{scope_id}:group:{batch_id}:{occurrence}`, the group's primary key.
    pub group_key: String,
    /// Durable journal identity of the scope that opened the group.
    pub scope_id: String,
    /// Owning session, when the scope has one. `NULL` rows are session-free.
    pub session_id: Option<SessionId>,
    /// The group's wake rule, persisted with no default: a group record written
    /// without one is refused rather than replayed under a guessed rule.
    pub wake: GroupWakePolicy,
    /// The disposition declared at open, which a crash-drain of this group
    /// applies rather than inventing one.
    pub loser_disposition: LoserPolicy,
    /// How many children the opener declared at write time. Persisted so a
    /// drain knows the group's membership without reconstructing the caller's
    /// envelopes — the write-time expectation, named distinctly from the
    /// actual cardinality `COUNT(membership rows)` answers.
    pub expected_children: usize,
    /// The lifecycle the group's `lifecycle` column holds (ADR 0099 §7):
    /// `live` while children may claim, `closing` once close is durably
    /// recorded, `settled` once finalization finished. Decode failures surface
    /// as corrupt-row errors; the column is never defaulted on read.
    pub lifecycle: EffectGroupLifecycle,
    /// The open instant, for the row's `created_at_ms`.
    pub created_at_ms: u64,
}

impl EffectGroupRecord {
    /// Derives the row from the group it records, taking only the two facts the
    /// group does not know: the journal scope the driver claims children under,
    /// and the open instant.
    ///
    /// This is the constructor a backend's `open_group` caller uses, and the
    /// reason it exists is that the four derived fields are *the same journaled
    /// facts the children already hashed*. `wake` and `loser_disposition` are
    /// folded into every child's
    /// [`EffectGroupMembership`](super::group::EffectGroupMembership), and
    /// [`RuntimeEffectGroup::try_new`] is what makes group and children agree
    /// about them. A driver that hand-stamped the record could write a row
    /// disagreeing with what its own children hashed — and nothing would notice
    /// until a production replay refused a group whose wake rule had silently
    /// become something else. Constructing from the group closes that path
    /// instead of asking each backend to remember it.
    ///
    /// `scope_id` is a parameter because it is not a group fact: it is the
    /// driver's journal identity, the same key its children's claims are fenced
    /// on. `session_id` is separately `Option`, because the record's column is
    /// nullable for session-free scopes.
    ///
    /// The fields stay public and readable; what this removes is the
    /// *construction* path that could make them disagree.
    #[must_use]
    pub fn from_group(
        group: &RuntimeEffectGroup,
        scope_id: impl Into<String>,
        session_id: Option<SessionId>,
        created_at_ms: u64,
    ) -> Self {
        Self {
            group_key: group.group_key().to_string(),
            scope_id: scope_id.into(),
            session_id,
            wake: group.wake(),
            loser_disposition: group.loser_disposition(),
            expected_children: group.children().len(),
            lifecycle: EffectGroupLifecycle::Live,
            created_at_ms,
        }
    }

    /// The disposition now in force: the lifecycle's recorded closing
    /// disposition once close has committed one, else the declared column.
    ///
    /// Once `closing` is durable this — not `loser_disposition` — is the
    /// authority a drain pass or a reopen must apply, because the close's
    /// `resolve_close` output is what the row committed to and the declared
    /// column is only what the opener asked for.
    #[must_use]
    pub fn effective_loser_disposition(&self) -> LoserPolicy {
        self.lifecycle
            .closing_disposition()
            .unwrap_or(self.loser_disposition)
    }
}

/// How far the four-step close has run, recorded on the group's `closing`
/// lifecycle (ADR 0099 §7).
///
/// The cursor counts *completed* steps in finalization order: `0` — none yet,
/// `1` — obligations drained (every accepted child ranked), `2` — outcome and
/// accounting committed, `3` — parent end recorded. Completing the fourth
/// step (retiring the group's live retention) is what turns the lifecycle
/// `settled`, so the durable range is `0..=3`.
///
/// The cursor is what makes the sequence resumable: a finalizer restarted
/// after a crash skips every step the cursor already records, and each step
/// is idempotent so a crash between the step and its recording re-runs the
/// step without harm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FinalizationStep(u8);

impl FinalizationStep {
    /// The cursor a freshly closed group opens with: no step has completed.
    pub const NONE: Self = Self(0);

    /// The largest cursor the column may hold.
    pub const LAST: u8 = 3;

    /// A recorded cursor value, or `None` above [`LAST`](Self::LAST).
    #[must_use]
    pub fn new(value: u8) -> Option<Self> {
        (value <= Self::LAST).then_some(Self(value))
    }

    /// The number of completed steps this cursor records.
    #[must_use]
    pub fn completed(self) -> u8 {
        self.0
    }

    /// The cursor one step past this one, or `None` at [`LAST`](Self::LAST).
    #[must_use]
    pub fn next(self) -> Option<Self> {
        Self::new(self.0 + 1)
    }
}

/// The lifecycle phase tag a group row's `lifecycle` column currently holds —
/// the `type` field of the encoded [`EffectGroupLifecycle`]. The CAS guards
/// and phase-filtered reads match on this string, so it is spelled once here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EffectGroupLifecyclePhase {
    /// Accepted and running; new children may claim.
    Live,
    /// Close is durably recorded; admission is refused and finalization is in
    /// progress.
    Closing,
    /// Finalization finished; the row awaits group retirement.
    Settled,
}

impl EffectGroupLifecyclePhase {
    /// The persisted `type` value.
    #[must_use]
    pub fn column(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Closing => "closing",
            Self::Settled => "settled",
        }
    }
}

/// The durable lifecycle of a recorded effect group (ADR 0099 §7, FIG-3410).
///
/// This is the JSON the `lifecycle` column carries: `{"type":"live"}`,
/// `{"type":"closing","disposition":..,"finalized":0..3}`, or
/// `{"type":"settled","disposition":..}`. `disposition` encodes through
/// [`LoserPolicy`]'s serde strings, the same values its own column uses.
///
/// `Closing` is the fact §7 requires to be durable *before* any cancel
/// decision is issued: the group's authority has stopped admitting, and the
/// recorded `disposition` is the effective one — `resolve_close` output at
/// the moment closing was written — so a reopened caller narrows nothing.
/// A column value that does not decode is a corrupt row, never defaulted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupLifecycle {
    /// The group is live.
    Live,
    /// Closing was durably recorded; `finalized` counts completed steps.
    Closing {
        /// The effective loser disposition the close committed to.
        disposition: LoserPolicy,
        /// Steps of the four-step finalization completed so far.
        finalized: FinalizationStep,
    },
    /// Finalization completed; the row awaits group-level retirement.
    Settled {
        /// The effective loser disposition the group closed under.
        disposition: LoserPolicy,
    },
}

impl EffectGroupLifecycle {
    /// `Closing` with the cursor at `0`.
    #[must_use]
    pub fn closing(disposition: LoserPolicy) -> Self {
        Self::Closing {
            disposition,
            finalized: FinalizationStep::NONE,
        }
    }

    /// `Settled` under the given effective disposition.
    #[must_use]
    pub fn settled(disposition: LoserPolicy) -> Self {
        Self::Settled { disposition }
    }

    /// The phase tag the encoded form carries.
    #[must_use]
    pub fn phase(&self) -> EffectGroupLifecyclePhase {
        match self {
            Self::Live => EffectGroupLifecyclePhase::Live,
            Self::Closing { .. } => EffectGroupLifecyclePhase::Closing,
            Self::Settled { .. } => EffectGroupLifecyclePhase::Settled,
        }
    }

    /// The effective disposition once closing committed one, else `None` for
    /// `Live` (whose declared disposition lives on the record's own column).
    #[must_use]
    pub fn closing_disposition(&self) -> Option<LoserPolicy> {
        match self {
            Self::Closing { disposition, .. } | Self::Settled { disposition } => Some(*disposition),
            Self::Live => None,
        }
    }
}

/// One accepted child of a group, retained before the open is acknowledged
/// (ADR 0099 §3).
///
/// This is the row that makes an accepted group *recoverable*.
/// [`EffectGroupRecord`] carries `children`, a count, which tells a drain how
/// many children exist and nothing about what any of them is; a child that has
/// not claimed yet has no journal row at all. §3 requires "a reconstructible
/// request for every unique child, including unclaimed children", and this is
/// it: the child's position, its durable identity, and the canonical envelope
/// that rebuilds it — command, group membership and all.
///
/// The envelope is the whole reconstruction. It already carries the child's
/// `EffectAddress` (its scope and replay key), its lineage, its
/// `EffectGroupMembership`, and — for a tool child — the
/// [`ToolChildRequest`](super::ToolChildRequest) that FIG-3408's first part
/// minted. Nothing is copied out of it into a column except `replay_key`,
/// which is the lookup key a drain needs without decoding every row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedGroupChild {
    /// This child's index in the group, and half its primary key. Rank is
    /// defined over this order, so the membership is read back ordered by it.
    pub position: usize,
    /// The child's replay key, its durable identity within the scope.
    pub replay_key: String,
    /// The recorded accepted envelope JSON — the raw `RuntimeEffectEnvelope`,
    /// which rebuilds the child whole. Not the canonical `{json, hash}` form
    /// the replay column stores; a writer needing that form captures it from
    /// the decoded envelope.
    pub envelope_json: String,
    /// The command format version the retained envelope was minted under,
    /// checked at decode: a build that cannot read it refuses rather than
    /// guesses.
    pub command_version: u16,
}

/// The persisted `wake` and `loser_disposition` column values.
///
/// Both enums are `#[serde(rename_all = "snake_case")]`, and these are the same
/// strings — journal bytes, so the mapping lives here once rather than as
/// literals in each backend's SQL.
pub trait EffectGroupColumn: Sized {
    /// The persisted column value.
    fn column(self) -> &'static str;

    /// The value a persisted column names, or `None` when no version of this
    /// runtime wrote it.
    ///
    /// The inverse of [`column`](Self::column) and deliberately beside it: a
    /// group row is read back to fence a reopen, so the mapping is now used in
    /// both directions and a backend that hand-rolled the read half could drift
    /// from the write half one string at a time. `None` rather than a default,
    /// because a group whose wake rule cannot be read is refused, never replayed
    /// under a guessed one.
    fn from_column(value: &str) -> Option<Self>;
}

impl EffectGroupColumn for GroupWakePolicy {
    fn column(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::FirstSuccess => "first_success",
            Self::All => "all",
        }
    }

    fn from_column(value: &str) -> Option<Self> {
        match value {
            "first" => Some(Self::First),
            "first_success" => Some(Self::FirstSuccess),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

impl EffectGroupColumn for LoserPolicy {
    fn column(self) -> &'static str {
        match self {
            Self::RunToCompletion => "run_to_completion",
            Self::Cancel => "cancel",
        }
    }

    fn from_column(value: &str) -> Option<Self> {
        match value {
            "run_to_completion" => Some(Self::RunToCompletion),
            "cancel" => Some(Self::Cancel),
            _ => None,
        }
    }
}

/// What a guarded [`finalize`](super::effect_replay_driver::EffectReplayRowStore::finalize)
/// did.
///
/// A richer answer than the `bool` it replaces, because N1's defect is
/// invisible to a boolean: "the fence moved" and "the fence moved *and nothing
/// was allocated*" are the same `false`, and the second is the property with no
/// other backstop. Making the allocated position part of the answer means a
/// backend that bumps on the miss cannot report conformantly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectFinalizeOutcome {
    /// The guarded write matched the row and the terminal is committed.
    ///
    /// For a grouped child the same transaction also won the group's §4
    /// linearization point: `commit_seq` is the child's durable position in
    /// the group's final-commit order, allocated from the group's second
    /// counter (`next_commit_seq`). It is **not** a settlement rank — rank is
    /// allocated only when the child's drain discharges
    /// ([`EffectReplayRowStore::discharge_child`](super::effect_replay_driver::EffectReplayRowStore::discharge_child)),
    /// because a rank a consumer could observe before the child's declared
    /// intents landed would be observable-before-durable (ADR 0099 §5).
    /// `None` for an ungrouped effect.
    Written {
        /// The child's durable final-commit position within its group.
        commit_seq: Option<u64>,
    },
    /// The guarded write matched no row: the fence moved and this driver no
    /// longer owns the effect. Nothing was written and, for a grouped child,
    /// **no commit position was allocated** (N1).
    FenceMoved,
    /// The child's replay row already carries `commit_state = 'cancel_decided'`:
    /// the cancel disposition won the §4 linearization point first, so this
    /// late final record is refused and **nothing was journaled** — the
    /// terminal write, the commit-state CAS and both counters all rolled back
    /// (ADR 0099 §4, crash window W17).
    CancelDecided,
}

/// The durable phase of one group child's §4/§5 commit protocol — the value
/// the replay row's `commit_state` column carries.
///
/// One enum rather than a decision flag plus a drain flag, because each phase
/// is a CAS arm with its own guards: `pending` while neither side holds the
/// §4 point, `committed` when the child's final record won it (with
/// `commit_seq`, the durable position in the group's final-commit order),
/// `drained` once §5's discharge seated the rank, and `cancel_decided` when
/// the cancel disposition won the point instead. "Both" or "neither" is not
/// a representable state.
///
/// An ungrouped replay row's column is vacuous: it goes `pending` →
/// `committed` with its terminal and never `drained` or `cancel_decided` —
/// there is no group arbitration for it to carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectCommitState {
    /// Neither contestant holds the §4 point; the row is `in_progress`.
    Pending,
    /// The final record won the §4 point and awaits its §5 discharge.
    Committed,
    /// The committed child's drain completed and its rank is seated.
    Drained,
    /// The cancel disposition won the §4 point.
    CancelDecided,
}

impl EffectCommitState {
    /// The persisted column value.
    pub fn column(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Committed => "committed",
            Self::Drained => "drained",
            Self::CancelDecided => "cancel_decided",
        }
    }

    /// The value a persisted column names, or `None` when no version of this
    /// runtime wrote it.
    pub fn from_column(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "committed" => Some(Self::Committed),
            "drained" => Some(Self::Drained),
            "cancel_decided" => Some(Self::CancelDecided),
            _ => None,
        }
    }
}

/// What [`decide_cancel`](super::effect_replay_driver::EffectReplayRowStore::decide_cancel)
/// did with a cancel disposition offered for one group child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectCancelOutcome {
    /// The decision committed: the child's replay row now carries
    /// `commit_state = 'cancel_decided'`, and the same transaction wrote the
    /// cancelled terminal and seated the child at `settlement_seq`, so no
    /// later claim, dispatch or finalize can move it.
    Decided {
        /// The rank the cancelled child was seated at.
        settlement_seq: u64,
    },
    /// The cancel disposition already held for this child. Idempotent: a
    /// retried close or a second cancel observes the first decision's rank.
    AlreadyDecided {
        /// The rank the first decision seated the child at.
        settlement_seq: u64,
    },
    /// Refused: the child's final record already won the linearization point
    /// (`commit_state = 'committed'`), so the cancel decision may not commit.
    /// The child is protected and proceeds through drain to its real rank.
    FinalCommitted {
        /// The commit-order position the winning final record holds.
        commit_seq: u64,
    },
}

/// The cancellation request one group child's arbitration point answers.
///
/// `terminal` is the cancelled terminal the decision journals — the caller
/// builds it (`child_cancelled_error`) because its content is the contract's,
/// not the store's. `envelope_json`/`envelope_hash` are the child's canonical
/// envelope and its hash — the same pair the child's claim would have
/// recorded — supplied by the caller, which owns envelope hashing and holds
/// the only copy of the canonical wire form: the membership row retains the
/// *raw* accepted envelope, not the `{json, hash}` wrapper the replay column's
/// readers decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectCancelRequest {
    /// The group the child belongs to.
    pub group_key: String,
    /// The child's replay key — its durable identity.
    pub replay_key: String,
    /// The cancelled terminal to journal with the decision.
    pub terminal: EffectTerminal,
    /// The child's canonical envelope JSON, exactly as its claim would have
    /// recorded it.
    pub envelope_json: String,
    /// Hash of `envelope_json`'s canonical payload.
    pub envelope_hash: String,
    /// The completion key a deferrable tool child parks on, as the promise
    /// row the decision closes in the same transaction (ADR 0099 §4, W17):
    /// completion delivery is one of the sinks the cancel fence covers, so a
    /// resolve after the decision is refused, typed, and writes nothing.
    /// `None` for a child that takes no completion key.
    pub completion_fence: Option<super::await_event_coordinator::AwaitEventCancelFence>,
}

/// What [`discharge_child`](super::effect_replay_driver::EffectReplayRowStore::discharge_child)
/// did with one committed child's drain completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectDischargeOutcome {
    /// The discharge committed: the replay row's `commit_state` is `drained`
    /// and the child was seated at `settlement_seq` — rank allocated only
    /// now, after the drain, which is what makes rank order agree with commit
    /// order (ADR 0099 §5).
    Discharged {
        /// The rank the child was seated at.
        settlement_seq: u64,
    },
    /// The child was already discharged. Idempotent, so a crashed drain step
    /// retried after recovery observes the first discharge's rank.
    AlreadyDischarged {
        /// The rank the first discharge seated the child at.
        settlement_seq: u64,
    },
    /// Refused for now: a committed sibling at a lower commit position is
    /// still undrained. Drains proceed in commit order, so the caller waits
    /// and retries — the barrier is durable and recovery finishes the missing
    /// drain rather than letting this child jump it.
    Blocked,
}

/// The discharge request one committed child's drain completion answers.
///
/// `terminal` is `Some` exactly when the child's §4 commit ran at its
/// final-attempt boundary rather than at the older finalize path: a
/// boundary-committed row holds no terminal — the commit journals only the
/// decision, its position, and the drain input — so the discharge is the
/// write that seats the projected outcome, moves the row to its terminal,
/// and marks it `drained` in one transaction. `None` keeps the legacy shape:
/// the row's terminal was journaled with its commit and the discharge writes
/// only rank and drain state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectDischargeRequest {
    /// The group the child belongs to.
    pub group_key: String,
    /// The journal scope the child's replay row lives under.
    pub scope_id: String,
    /// The child's replay key — its durable identity.
    pub replay_key: String,
    /// The terminal to journal with the discharge, when the row's was not
    /// written at commit.
    pub terminal: Option<EffectTerminal>,
}

/// What a tool child's final-attempt boundary asks its controller to commit.
///
/// Identity and drain input only. The lease owner is the substrate's own
/// fact, so the request does not carry it and no caller can claim another
/// owner's fence — and the group is *resolved* from the durable record rather
/// than asserted by the caller, so `group_key` is an output of the decision,
/// not an input a caller could get wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupChildFinalCommit {
    /// The journal scope the child's replay row lives under.
    pub scope_id: String,
    /// The child's replay key — its durable identity.
    pub replay_key: String,
    /// The sealed drain input — the declared intents and projection data a
    /// recovery replays instead of re-running the attempt.
    pub drain_input: String,
}

/// The §4 commit request one group child's final-attempt boundary answers.
///
/// This is the final record's durable commit at the linearization point —
/// deliberately *not* the finalize-time CAS the older path used. What the
/// commit persists is the arbitration result and everything a recovered
/// drain needs to finish the child's obligations without re-executing its
/// attempt: the allocated `commit_seq` (the order nested semantic commands
/// may drain in) and `drain_input` (the sealed attempt outcome plus its
/// declared intents). The projected outcome itself journals at discharge,
/// after the obligations finish (ADR 0099 §5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectGroupChildCommitRequest {
    /// The group the caller believes the row belongs to, when it has a
    /// durable answer already — the driver's own group membership from the
    /// minted claim. `None` means "resolve it": the row's own `group_key`
    /// column is the authority either way, and a `Some` that disagrees is
    /// corruption, not a different group.
    pub group_key: Option<String>,
    /// The journal scope the child's replay row lives under.
    pub scope_id: String,
    /// The child's replay key — its durable identity.
    pub replay_key: String,
    /// The serialized drain input the commit records: what a recovered pass
    /// drains and projects rather than re-executes.
    pub drain_input: String,
    /// The lease owner the CAS must still find on the row. A boundary is
    /// built by the claiming process, so its commit is fenced to that claim —
    /// a row reclaimed under another owner cannot be committed by a stale
    /// executor.
    pub owner_id: String,
}

/// What [`commit_group_child`](super::effect_replay_driver::EffectReplayRowStore::commit_group_child)
/// decided for one replay key's final record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectGroupChildCommitOutcome {
    /// The replay key names no group child: an ordinary effect, which takes
    /// the process-local drain path and owes no durable discharge.
    Ungrouped,
    /// The final record won the §4 point: `commit_state` is `committed`, the
    /// commit position `commit_seq` is allocated, and the drain input is
    /// durable. The caller may drain once every lower commit position has
    /// drained.
    Committed {
        /// The group the row resolved to.
        group_key: String,
        /// The child's durable position in the group's final-commit order.
        commit_seq: u64,
    },
    /// The point already holds this child's final record — a crashed or
    /// retried executor reaching the boundary again. `drain_input` is the
    /// recorded obligation set the winner committed, so recovery drains the
    /// recorded intents rather than whatever a re-execution re-declared.
    AlreadyCommitted {
        /// The group the row resolved to.
        group_key: String,
        /// The winning commit's durable position.
        commit_seq: u64,
        /// The drain input the winning commit recorded.
        drain_input: Option<String>,
    },
    /// Refused: the cancel disposition already committed at the §4 point
    /// (W6/W7). The late final may journal nothing — no terminal, no
    /// position, no drain input — and the caller surfaces the typed
    /// cancel-decided error rather than an outcome.
    CancelDecided {
        /// The group the row resolved to.
        group_key: String,
        /// The position the cancel decision seated the child at: the
        /// settlement rank `decide_cancel` allocated, not a commit-order
        /// position — a cancel-decided row carries no `commit_seq`.
        commit_seq: u64,
    },
}

/// The arbitration and discharge state of one group child's replay row, read
/// back for the barrier wait, the §4 admission fence, and recovery's "what is
/// still owed" question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredChildArbitration {
    /// The group this arbitration belongs to.
    pub group_key: String,
    /// The row's commit-protocol phase.
    pub commit_state: EffectCommitState,
    /// The child's durable position in the group's final-commit order, held
    /// only under `committed`/`drained` — the CHECK on the column pair makes
    /// the two disagreeing unrepresentable.
    pub commit_seq: Option<u64>,
}

/// A settled child of a group, read back by **rank**.
///
/// Rank is the position of a child's `settlement_seq` in the ascending order of
/// the group's recorded sequences — the child holding the `(consumed + 1)`-th
/// smallest value — never a lookup by literal sequence equality. Sequences are
/// monotonic and unique within a group but deliberately **not gapless**: a
/// rolled-back finalize burns a value, and so does the backend-local escape
/// hatch ADR 0065 pre-identifies for wide-group contention (a per-group
/// sequence generator, which does not participate in transaction rollback).
/// Rank is immune to both, because it counts recorded children rather than
/// counting up through integers.
///
/// The child's *position* is deliberately not returned, and is not a column.
/// ADR 0065 fixes the child table's growth at exactly two columns, and a
/// position column would be a third copy of a fact the group's children already
/// carry — one more place for the copies to disagree. The settlement names its
/// child by `replay_key`, which is already the child's durable identity and
/// already a column; a host that opened the group holds its children in order
/// and maps the key back to a position exactly, with nothing to keep in sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredGroupSettlement {
    /// The rank's allocated sequence value.
    pub sequence: u64,
    /// The settled child's replay key, unique within the scope.
    pub replay_key: String,
    /// The status and payload columns, decoded once by the store.
    pub state: EffectRowState,
}

/// A child of a group whose settlement rank has **not** been allocated, read
/// back by [`read_unsettled_group_children`](super::effect_replay_driver::EffectReplayRowStore::read_unsettled_group_children).
///
/// "Unsettled" is `settlement_seq IS NULL`. Since ADR 0099 §4/§5 (FIG-3409)
/// that is **not** the same set as "non-terminal": a rank is allocated at
/// discharge, after the child's drain, so a child whose final record committed
/// but whose drain has not finished is terminal *and* unsettled — the exact
/// state a recovery drain exists to finish (W7, W19). The `commit_state` this
/// row carries is what lets the reader tell that state apart from the torn
/// row N1 once made impossible: terminal with no rank and no `committed`
/// state is corrupt; `committed` with no rank is a live recovery obligation.
///
/// The read is the exact complement of the rank read
/// ([`StoredGroupSettlement`]), whose predicate is `settlement_seq IS NOT
/// NULL` — which is why a group host could not previously ask the question at
/// all, and had to infer "how much of this group is still outstanding" by
/// walking ranks until one came back `None`.
///
/// Two consumers, both named because they decide the shape:
///
/// * A group host closing a group needs to know whether the group is
///   *complete* — no unsettled children — because retention of its in-process
///   state is bounded by close **and** completion, and completion is a durable
///   fact rather than a count this process kept.
/// * The group-drain driver (FIG-1536) uses the same rows as its queue: the
///   journal's own unranked grouped children *are* the drain queue, so the
///   row carries what re-executing, cancelling or discharging a child needs —
///   the child's journal identity, its recorded canonical envelope, the lease
///   boundary that says whether the drain may take it over, and its §4
///   arbitration state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsettledGroupChild {
    /// Durable journal identity of the scope the child was claimed under; one
    /// half of its `(scope_id, replay_key)` key.
    pub scope_id: String,
    /// The child's accepted position in the group — its index into the
    /// declared membership, and the ordinal the cancelled terminal's message
    /// carries.
    pub position: u64,
    /// The child's replay key, unique within its scope. A host that opened the
    /// group maps it back to a position through the children it holds.
    pub replay_key: String,
    /// The recorded accepted envelope JSON — the raw `RuntimeEffectEnvelope`,
    /// so a drain can rebuild the child's effect without reconstructing the
    /// caller's frame. Sourced from the retained membership row, so it is
    /// present for a never-claimed child too; the canonical `{json, hash}`
    /// form is captured from the decoded envelope when a write needs it.
    pub envelope_json: String,
    /// The status and payload columns, decoded once by the store — `None` for
    /// a child that never claimed a replay row, which is a normal pre-claim
    /// state rather than corruption. On a healthy journal a `Some` value is
    /// [`EffectRowState::InProgress`] or a terminal under a `committed`
    /// state awaiting its drain; anything else is reported rather than
    /// filtered, so corruption stays visible to the reader.
    pub state: Option<EffectRowState>,
    /// Lease expiry of the child's current claim, against which a drain decides
    /// whether the child is still owned by a live driver.
    pub lease_expires_at_ms: u64,
    /// The replay row's commit-protocol phase, `None` for a child that never
    /// claimed a replay row. `committed` is the undrained-commit case a
    /// recovery drain discharges; `pending` and `None` are the work the
    /// disposition decides over.
    pub commit_state: Option<EffectCommitState>,
    /// The child's position in the group's final-commit order — `Some` only
    /// under `commit_state = 'committed'` here, since a `drained` child holds
    /// a rank and is no longer unsettled.
    pub commit_seq: Option<u64>,
    /// The command format version the retained envelope was minted under —
    /// the membership row's own fact, carried so a drain refuses a command
    /// encoding it cannot read rather than guessing at the envelope.
    pub command_version: u16,
}

#[cfg(test)]
mod tests {
    use super::super::envelope::{
        RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectInvocation,
    };
    use super::*;

    fn group_of(children: usize, wake: GroupWakePolicy, loser: LoserPolicy) -> RuntimeEffectGroup {
        // Siblings carry distinct replay keys, because one replay key is one
        // journaled child and a group of duplicates is refused at construction.
        let invocation = |replay_key: String| {
            RuntimeEffectInvocation::new(
                crate::EffectAddress::new(
                    crate::ExecutionScope::turn("session", "turn"),
                    replay_key,
                )
                .expect("valid group journal address"),
                crate::RuntimeAttribution::for_session("session"),
                "effect",
            )
        };
        RuntimeEffectGroup::try_new(
            invocation("replay".to_string()),
            "scope:group:batch:0",
            (0..children)
                .map(|position| {
                    RuntimeEffectEnvelope::new(
                        invocation(format!("replay-{position}")),
                        RuntimeEffectCommand::Sleep {
                            spec: crate::SleepSpec::For {
                                duration_ms: position as u64 + 1,
                            },
                        },
                    )
                })
                .collect(),
            wake,
            loser,
        )
        .expect("a non-empty group assembles")
    }

    /// Every field the group knows comes from the group. The two that are not
    /// group facts — the driver's journal scope and the open instant — are the
    /// only arguments, so there is no parameter a caller could use to write a
    /// wake rule or disposition disagreeing with what the children hashed.
    #[test]
    fn a_record_derives_every_group_fact_from_the_group() {
        let group = group_of(3, GroupWakePolicy::FirstSuccess, LoserPolicy::Cancel);
        let record = EffectGroupRecord::from_group(
            &group,
            "scope-journal-key",
            Some(SessionId::from("session")),
            1_700_000_000_000,
        );
        assert_eq!(record.group_key, group.group_key());
        assert_eq!(record.wake, group.wake());
        assert_eq!(record.loser_disposition, group.loser_disposition());
        assert_eq!(record.expected_children, group.children().len());
        assert_eq!(record.scope_id, "scope-journal-key");
        assert_eq!(record.session_id.as_deref(), Some("session"));
        assert_eq!(record.created_at_ms, 1_700_000_000_000);
    }

    /// The wake rule and disposition are folded into each child's envelope hash,
    /// so a record that disagreed with the group would disagree with the
    /// children too. Changing the group has to change the record.
    #[test]
    fn a_records_journaled_facts_track_the_group_they_came_from() {
        let first = EffectGroupRecord::from_group(
            &group_of(1, GroupWakePolicy::First, LoserPolicy::RunToCompletion),
            "scope",
            None,
            0,
        );
        let second = EffectGroupRecord::from_group(
            &group_of(2, GroupWakePolicy::All, LoserPolicy::Cancel),
            "scope",
            None,
            0,
        );
        assert_eq!(first.wake, GroupWakePolicy::First);
        assert_eq!(second.wake, GroupWakePolicy::All);
        assert_eq!(first.loser_disposition, LoserPolicy::RunToCompletion);
        assert_eq!(second.loser_disposition, LoserPolicy::Cancel);
        assert_eq!(first.expected_children, 1);
        assert_eq!(second.expected_children, 2);
        assert!(
            first.session_id.is_none(),
            "a session-free scope writes NULL"
        );
    }

    /// Both directions, in one test, because the defect the round trip guards
    /// is a read half that drifts from the write half one string at a time.
    #[test]
    fn group_columns_round_trip_through_their_persisted_bytes() {
        for wake in [
            GroupWakePolicy::First,
            GroupWakePolicy::FirstSuccess,
            GroupWakePolicy::All,
        ] {
            assert_eq!(GroupWakePolicy::from_column(wake.column()), Some(wake));
        }
        for disposition in [LoserPolicy::RunToCompletion, LoserPolicy::Cancel] {
            assert_eq!(
                LoserPolicy::from_column(disposition.column()),
                Some(disposition)
            );
        }
        assert_eq!(
            GroupWakePolicy::from_column("first_settlement"),
            None,
            "a value no version of this runtime wrote must be refused, not \
             defaulted"
        );
        assert_eq!(LoserPolicy::from_column(""), None);
    }

    #[test]
    fn wake_columns_are_the_persisted_journal_bytes() {
        assert_eq!(GroupWakePolicy::First.column(), "first");
        assert_eq!(GroupWakePolicy::FirstSuccess.column(), "first_success");
        assert_eq!(GroupWakePolicy::All.column(), "all");
    }

    #[test]
    fn loser_disposition_columns_are_the_persisted_journal_bytes() {
        assert_eq!(LoserPolicy::RunToCompletion.column(), "run_to_completion");
        assert_eq!(LoserPolicy::Cancel.column(), "cancel");
    }

    /// The column strings are the serde representation, so a future `serde`
    /// rename cannot silently diverge the JSON a child's envelope hashes from
    /// the TEXT its group row stores.
    #[test]
    fn columns_agree_with_the_serde_representation() {
        for wake in [
            GroupWakePolicy::First,
            GroupWakePolicy::FirstSuccess,
            GroupWakePolicy::All,
        ] {
            assert_eq!(
                serde_json::to_string(&wake).expect("wake policies serialize"),
                format!("\"{}\"", wake.column())
            );
        }
        for disposition in [LoserPolicy::RunToCompletion, LoserPolicy::Cancel] {
            assert_eq!(
                serde_json::to_string(&disposition).expect("dispositions serialize"),
                format!("\"{}\"", disposition.column())
            );
        }
    }

    /// A fence miss and an ungrouped write are both "no rank", but only one of
    /// them wrote the terminal — which is why the outcome is two variants and
    /// not the `Option<u64>` that would collapse them.
    #[test]
    fn a_fence_miss_is_distinguishable_from_an_ungrouped_write() {
        assert_ne!(
            EffectFinalizeOutcome::FenceMoved,
            EffectFinalizeOutcome::Written { commit_seq: None }
        );
    }
}
