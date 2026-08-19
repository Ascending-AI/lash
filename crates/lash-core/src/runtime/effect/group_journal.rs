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

use super::group::{GroupWakePolicy, LoserDisposition, RuntimeEffectGroup};

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
    pub session_id: Option<String>,
    /// The group's wake rule, persisted with no default: a group record written
    /// without one is refused rather than replayed under a guessed rule.
    pub wake: GroupWakePolicy,
    /// The disposition declared at open, which a crash-drain of this group
    /// applies rather than inventing one.
    pub loser_disposition: LoserDisposition,
    /// How many children the group has. Persisted so a drain knows the group's
    /// membership without reconstructing the caller's envelopes.
    pub children: usize,
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
        session_id: Option<String>,
        created_at_ms: u64,
    ) -> Self {
        Self {
            group_key: group.group_key().to_string(),
            scope_id: scope_id.into(),
            session_id,
            wake: group.wake(),
            loser_disposition: group.loser_disposition(),
            children: group.children().len(),
            created_at_ms,
        }
    }
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

impl EffectGroupColumn for LoserDisposition {
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

/// What a guarded [`finalize`](super::effect_replay_driver::EffectReplayPersistence::finalize)
/// did.
///
/// A richer answer than the `bool` it replaces, because N1's defect is
/// invisible to a boolean: "the fence moved" and "the fence moved *and nothing
/// was allocated*" are the same `false`, and the second is the property with no
/// other backstop. Making the allocated rank part of the answer means a backend
/// that bumps on the miss cannot report conformantly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectFinalizeOutcome {
    /// The guarded write matched the row and the terminal is committed.
    Written {
        /// The rank allocated from the group counter, `None` for an ungrouped
        /// effect. Monotonic and unique within the group, never gapless.
        settlement_seq: Option<u64>,
    },
    /// The guarded write matched no row: the fence moved and this driver no
    /// longer owns the effect. Nothing was written and, for a grouped child,
    /// **no rank was allocated** (N1).
    FenceMoved,
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
    /// Raw `status` column; an unrecognized value is a corrupt row.
    pub status: String,
    /// Recorded success outcome, present when `status` is `completed`.
    pub outcome_json: Option<String>,
    /// Recorded failure, present when `status` is `failed`.
    pub error_json: Option<String>,
}

/// A child of a group whose settlement rank has **not** been allocated, read
/// back by [`read_unsettled_group_children`](super::effect_replay_driver::EffectReplayPersistence::read_unsettled_group_children).
///
/// "Unsettled" is `settlement_seq IS NULL`, which for a grouped child is the
/// same set as "non-terminal": a rank is allocated in the same transaction that
/// writes the terminal (N1), so a child cannot hold one without the other. The
/// read is the exact complement of the rank read
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
///   journal's own non-terminal grouped children *are* the drain queue, so the
///   row carries what re-executing or cancelling a child needs — the child's
///   journal identity, its recorded canonical envelope, and the lease boundary
///   that says whether the drain may take it over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsettledGroupChild {
    /// Durable journal identity of the scope the child was claimed under; one
    /// half of its `(scope_id, replay_key)` key.
    pub scope_id: String,
    /// The child's replay key, unique within its scope. A host that opened the
    /// group maps it back to a position through the children it holds.
    pub replay_key: String,
    /// The recorded canonical envelope JSON, so a drain can rebuild the child's
    /// effect without reconstructing the caller's frame.
    pub envelope_json: String,
    /// Raw `status` column. Always `in_progress` on a healthy journal, since a
    /// terminal and a rank are written together; any other value is a corrupt
    /// row and is reported rather than filtered, so the corruption is visible
    /// to the reader instead of silently shrinking the queue.
    pub status: String,
    /// Lease expiry of the child's current claim, against which a drain decides
    /// whether the child is still owned by a live driver.
    pub lease_expires_at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::super::envelope::{
        RuntimeEffectCommand, RuntimeEffectEnvelope, RuntimeEffectKind, RuntimeInvocation,
        RuntimeScope,
    };
    use super::*;

    fn group_of(
        children: usize,
        wake: GroupWakePolicy,
        loser: LoserDisposition,
    ) -> RuntimeEffectGroup {
        // Siblings carry distinct replay keys, because one replay key is one
        // journaled child and a group of duplicates is refused at construction.
        let invocation = |replay_key: String| {
            RuntimeInvocation::effect(
                RuntimeScope::new("session"),
                "effect",
                RuntimeEffectKind::Sleep,
                replay_key,
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
                            duration_ms: position as u64 + 1,
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
        let group = group_of(3, GroupWakePolicy::FirstSuccess, LoserDisposition::Cancel);
        let record = EffectGroupRecord::from_group(
            &group,
            "scope-journal-key",
            Some("session".to_string()),
            1_700_000_000_000,
        );
        assert_eq!(record.group_key, group.group_key());
        assert_eq!(record.wake, group.wake());
        assert_eq!(record.loser_disposition, group.loser_disposition());
        assert_eq!(record.children, group.children().len());
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
            &group_of(1, GroupWakePolicy::First, LoserDisposition::RunToCompletion),
            "scope",
            None,
            0,
        );
        let second = EffectGroupRecord::from_group(
            &group_of(2, GroupWakePolicy::All, LoserDisposition::Cancel),
            "scope",
            None,
            0,
        );
        assert_eq!(first.wake, GroupWakePolicy::First);
        assert_eq!(second.wake, GroupWakePolicy::All);
        assert_eq!(first.loser_disposition, LoserDisposition::RunToCompletion);
        assert_eq!(second.loser_disposition, LoserDisposition::Cancel);
        assert_eq!(first.children, 1);
        assert_eq!(second.children, 2);
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
        for disposition in [LoserDisposition::RunToCompletion, LoserDisposition::Cancel] {
            assert_eq!(
                LoserDisposition::from_column(disposition.column()),
                Some(disposition)
            );
        }
        assert_eq!(
            GroupWakePolicy::from_column("first_settlement"),
            None,
            "a value no version of this runtime wrote must be refused, not \
             defaulted"
        );
        assert_eq!(LoserDisposition::from_column(""), None);
    }

    #[test]
    fn wake_columns_are_the_persisted_journal_bytes() {
        assert_eq!(GroupWakePolicy::First.column(), "first");
        assert_eq!(GroupWakePolicy::FirstSuccess.column(), "first_success");
        assert_eq!(GroupWakePolicy::All.column(), "all");
    }

    #[test]
    fn loser_disposition_columns_are_the_persisted_journal_bytes() {
        assert_eq!(
            LoserDisposition::RunToCompletion.column(),
            "run_to_completion"
        );
        assert_eq!(LoserDisposition::Cancel.column(), "cancel");
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
        for disposition in [LoserDisposition::RunToCompletion, LoserDisposition::Cancel] {
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
            EffectFinalizeOutcome::Written {
                settlement_seq: None,
            }
        );
    }
}
