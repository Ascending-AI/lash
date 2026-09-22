//! The durable effect-group contract types (FIG-1416).
//!
//! Groups are the composition *above* attempts: a structured set of
//! independently journaled children, a durable wake rule, and a settlement
//! order that is a journal fact rather than a scheduler artifact. The methods
//! that operate on them live on `RuntimeEffectController`; ADR 0065 records the
//! normative obligations a host implementation must satisfy.
//!
//! These types live beside `envelope.rs` rather than inside it only because the
//! two together outgrew the production file-size budget.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::envelope::{RuntimeEffectInvocation, RuntimeEffectOutcome};
use super::{RuntimeEffectControllerError, RuntimeEffectEnvelope};

/// Wake rule of a durable effect group, recorded in the group's journal
/// identity so replay cannot silently change it (FIG-1416).
///
/// Deliberately three variants, not four. `Promise.all` and `Promise.allSettled`
/// both take [`All`](Self::All) because they ask the host for exactly the same
/// thing — deliver settlements in durable rank order and keep the rest running.
/// They differ only in how far the *caller* consumes: `all` stops at its first
/// rejection, `allSettled` consumes every settlement. That early exit is a
/// caller-side loop decision, never a host obligation, so encoding it here would
/// make journaled identity pin a distinction no host acts on.
///
/// No serde default, matching the fail-closed discipline of
/// [`ToolBatchEffectOutcome::settlement_order`](super::envelope::ToolBatchEffectOutcome::settlement_order):
/// a group record written without a wake rule is refused rather than replayed
/// under a guessed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupWakePolicy {
    /// `Promise.race`, and the FIG-1150 signal/deadline select: the first
    /// settlement of any kind wakes the caller.
    First,
    /// `Promise.any`: the first *successful* settlement wakes the caller and
    /// failures accumulate until one succeeds or all fail.
    FirstSuccess,
    /// `Promise.all` and `Promise.allSettled`: the caller consumes settlements
    /// in rank order and decides for itself when to stop.
    All,
}

/// A child effect's membership in a durable effect group.
///
/// Rides [`RuntimeEffectEnvelope`] as an optional, omitted-when-absent field so
/// an ungrouped effect's canonical encoding — and therefore its
/// `envelope_hash` — stays byte-identical to what it was before groups existed.
/// That is a blocking constraint rather than a style preference: Postgres is not
/// reject-and-recreate, so a live `lash_runtime_effect_replay` survives the
/// upgrade with all its recorded hashes, and an unconditional encoding change
/// would return every in-flight effect — not just grouped ones — as a replay
/// mismatch.
///
/// Folding [`wake`](Self::wake) into the child's hash is also what makes "replay
/// cannot silently change the wake rule" backed rather than asserted: it is the
/// only mechanism available on engine tiers that keep no group row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupMembership {
    /// `{scope_id}:group:[{parent_effect_id}:]{batch_id}`.
    ///
    /// The occurrence ordinal is load-bearing and rides inside `batch_id`:
    /// the batch id is a content hash of the calls *and* their
    /// `ToolBatchOccurrence` under `TOOL_BATCH_FAMILY_VERSION` 2 (FIG-3394),
    /// so two textually identical `race` calls in one protocol iteration mint
    /// different batch ids and would otherwise share a group.
    pub group_key: String,
    /// This child's position in [`RuntimeEffectGroup::children`].
    pub position: usize,
    /// The group's wake rule, folded into this child's canonical hash.
    pub wake: GroupWakePolicy,
    /// The group's declared loser disposition, folded into this child's
    /// canonical hash for the same reason [`wake`](Self::wake) is: it is a
    /// per-group durable fact, and on engine tiers that keep no group row the
    /// child hash is the only fence that can refuse a replay whose disposition
    /// drifted.
    pub loser_disposition: LoserPolicy,
}

/// The group child every semantic admission made through a bound controller is
/// minted under (ADR 0099 §4, FIG-3470).
///
/// A controller minted by
/// [`EffectHost::scoped_for_group_child`](super::executor::EffectHost::scoped_for_group_child)
/// carries this pair, and every `execute_effect` it serves is admitted — or
/// refused — under the substrate's own arbitration for *this* child: the SQL
/// claim fences its insert on the minting replay row's commit state, the
/// native controller serializes the admission against the group mutex, and the
/// Restate handler asks the serialized group index. The `caused_by` lineage a
/// nested envelope happens to carry is deliberately not consulted: it names a
/// parent, not the child whose §4 decision owns this admission.
///
/// `child` is the group child's own `ToolInvocation` envelope address — the
/// replay row its cancel disposition is decided on. `membership` is the
/// retained membership the same envelope carries: the pair comes from one
/// journaled fact, so a caller cannot bind a controller to a child and a group
/// that were never recorded together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupChildBinding {
    /// The child's own `ToolInvocation` envelope address: the replay row its
    /// cancel disposition is decided on.
    pub child: crate::EffectAddress,
    /// The retained membership the child's envelope carries.
    pub membership: EffectGroupMembership,
}

/// A group of independently journaled child effects opened at the effect-host
/// seam (FIG-1416).
///
/// The children are ordinary envelopes carrying ordinary
/// [`RuntimeEffectCommand`](super::envelope::RuntimeEffectCommand)s. For every
/// child the journal could already name — a sleep, a process command, an await
/// — that is the whole story, and ADR 0065 recorded the reason: what is new is
/// the *composition above* attempts, not the attempts.
///
/// **A tool child is the exception, and it is a named one** (ADR 0099 §2, §3;
/// FIG-3408). A tool group child is a replayable invocation driver, and neither
/// existing tool command is that:
/// [`ToolAttempt`](super::envelope::RuntimeEffectCommand::ToolAttempt) is the
/// atomic body of one attempt, so it cannot carry retry, and
/// [`ToolBatch`](super::envelope::RuntimeEffectCommand::ToolBatch) is the whole
/// batch a group replaces. It is named by
/// [`ToolInvocation`](super::envelope::RuntimeEffectCommand::ToolInvocation),
/// whose payload is the retained request that reconstructs the child from the
/// journal alone.
///
/// The fields are readable but not publicly writable, because every durability claim in ADR
/// 0065 reduces to the group key, wake rule, and child positions *agreeing* across the group
/// record and each child's [`EffectGroupMembership`] — and hand-stamped copies make
/// disagreement both representable and invisible until a production replay refuses.
#[derive(Clone, Debug)]
pub struct RuntimeEffectGroup {
    invocation: RuntimeEffectInvocation,
    group_key: String,
    children: Vec<RuntimeEffectEnvelope>,
    wake: GroupWakePolicy,
    loser_disposition: LoserPolicy,
}

impl RuntimeEffectGroup {
    /// Validates and assembles a durable effect group, stamping each child's
    /// membership from its own index and the group's key and wake rule.
    ///
    /// This is the only constructor, so a host handed a `RuntimeEffectGroup` may
    /// rely on all of the following rather than re-deriving them:
    ///
    /// * `children` is non-empty;
    /// * no two children share a replay key;
    /// * every child's `group_key` is this group's key;
    /// * every child's `position` is its index in `children`;
    /// * every child's `wake` is this group's wake rule;
    /// * every child's `loser_disposition` is this group's declared disposition.
    ///
    /// Children that arrive already stamped are checked rather than trusted;
    /// children that arrive unstamped are stamped here. Either way one group has
    /// one key, so a host never fishes identity out of `children[0]`.
    ///
    /// **Distinct replay keys are required, deliberately.** A replay key is the
    /// journal's identity for a child: two children carrying the same one are
    /// one row, one claim and one rank, so the second child replays the first's
    /// terminal, the group's last rank is never allocated, and a caller waiting
    /// on it parks forever. The failure is a silent permanent hang rather than a
    /// refusal, and the rank-to-position lookup would mis-attribute the one
    /// settlement that did land, so the collision is refused here — where every
    /// host, durable or native, is already handed a checked group — rather than
    /// trusted to be unreachable because ordinal minting happens to make it so.
    ///
    /// **Empty groups are rejected, deliberately.** `Promise.all([])` resolves
    /// immediately with `[]` and `Promise.race([])` never settles; neither has a
    /// child to journal, so neither is a durable fact and neither should reach
    /// this seam. The dialect layer resolves the former locally, and the latter
    /// is a never-settling program that must not become an unbounded durable
    /// await — an empty group would make the key underivable and
    /// [`await_next_settlement`](super::RuntimeEffectController::await_next_settlement)
    /// unbounded.
    ///
    /// `loser_disposition` is declared **here, at open**, rather than chosen at
    /// close. It is statically known when the group is built — `all` and
    /// `allSettled` mean [`LoserPolicy::RunToCompletion`], and `race`/`any`
    /// take whichever the ratified race semantics specify — and it is the same
    /// class of per-group durable fact as [`wake`](Self::wake). Leaving it as a
    /// close-time argument meant a caller that crashed after opening and before
    /// closing left the host no record of which disposition applied, so the
    /// group-drain path had to invent one, silently running a deadline arm's
    /// losers to completion on exactly the failure path this contract exists for.
    pub fn try_new(
        invocation: RuntimeEffectInvocation,
        group_key: impl Into<String>,
        children: Vec<RuntimeEffectEnvelope>,
        wake: GroupWakePolicy,
        loser_disposition: LoserPolicy,
    ) -> Result<Self, RuntimeEffectControllerError> {
        let group_key = group_key.into();
        if group_key.trim().is_empty() {
            return Err(group_shape_error(
                "a durable effect group requires a non-empty group key",
            ));
        }
        if children.is_empty() {
            return Err(group_shape_error(format!(
                "durable effect group {group_key} requires at least one child; an \
                 empty group has no settlement to journal and no derivable identity"
            )));
        }
        let children = children
            .into_iter()
            .enumerate()
            .map(|(index, child)| match child.group.as_deref() {
                None => {
                    Ok(child.in_effect_group(group_key.clone(), index, wake, loser_disposition))
                }
                Some(membership) => {
                    if membership.group_key != group_key {
                        return Err(group_shape_error(format!(
                            "child {index} of durable effect group {group_key} claims \
                             group {}; one group has one key",
                            membership.group_key
                        )));
                    }
                    if membership.position != index {
                        return Err(group_shape_error(format!(
                            "child {index} of durable effect group {group_key} claims \
                             position {}; settlement rank assumes position equals index",
                            membership.position
                        )));
                    }
                    if membership.wake != wake {
                        return Err(group_shape_error(format!(
                            "child {index} of durable effect group {group_key} hashes \
                             wake rule {:?} but the group records {wake:?}; the two are \
                             one journaled fact",
                            membership.wake
                        )));
                    }
                    if membership.loser_disposition != loser_disposition {
                        return Err(group_shape_error(format!(
                            "child {index} of durable effect group {group_key} hashes \
                             loser disposition {:?} but the group declares \
                             {loser_disposition:?}; the two are one journaled fact",
                            membership.loser_disposition
                        )));
                    }
                    Ok(child)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut first_seen_at: HashMap<&str, usize> = HashMap::with_capacity(children.len());
        for (index, child) in children.iter().enumerate() {
            let replay_key = child.invocation.replay_key();
            if let Some(first) = first_seen_at.insert(replay_key, index) {
                return Err(group_shape_error(format!(
                    "children {first} and {index} of durable effect group {group_key} share \
                     replay key {replay_key}; one replay key is one journaled child, so the \
                     group could never allocate both ranks"
                )));
            }
        }
        Ok(Self {
            invocation,
            group_key,
            children,
            wake,
            loser_disposition,
        })
    }

    /// The group's durable identity. Children derive their replay keys from it
    /// exactly as batch leaves already do.
    #[must_use]
    pub fn invocation(&self) -> &RuntimeEffectInvocation {
        &self.invocation
    }

    /// `{scope_id}:group:[{parent_effect_id}:]{batch_id}` — the key the host
    /// records the group and its settlement counter under. The occurrence
    /// ordinal rides inside `batch_id` (FIG-3394); it is not a separate
    /// segment.
    #[must_use]
    pub fn group_key(&self) -> &str {
        &self.group_key
    }

    /// The children in source order. Each carries its own stable replay key and
    /// its own [`EffectGroupMembership`], agreeing with this group by
    /// construction.
    #[must_use]
    pub fn children(&self) -> &[RuntimeEffectEnvelope] {
        &self.children
    }

    /// Recorded in the group's journal identity, and folded into every child's
    /// envelope hash.
    #[must_use]
    pub fn wake(&self) -> GroupWakePolicy {
        self.wake
    }

    /// The disposition the group's losers are subject to, journaled with the
    /// group row so an abandoned group is drained under the caller's declared
    /// intent rather than a policy the drain path invented.
    #[must_use]
    pub fn loser_disposition(&self) -> LoserPolicy {
        self.loser_disposition
    }

    /// Proves that both the group header and every child belong to the scope a
    /// controller is about to admit. Composite admission must run before any
    /// resolver, index, journal, or local-execution side effect.
    pub fn validate_execution_scope(
        &self,
        admitted_scope: &crate::ExecutionScope,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.invocation.validate_execution_scope(admitted_scope)?;
        for child in &self.children {
            child.invocation.validate_execution_scope(admitted_scope)?;
        }
        Ok(())
    }
}

/// The common group facts a host records and later fences on reopen.
pub(crate) trait EffectGroupRecordAccessor {
    /// The group's durable identity.
    fn group_key(&self) -> &str;

    /// The number of children recorded for the group.
    fn children(&self) -> usize;

    /// The wake rule recorded for the group.
    fn wake(&self) -> GroupWakePolicy;

    /// The loser disposition recorded for the group.
    fn loser_disposition(&self) -> LoserPolicy;
}

impl EffectGroupRecordAccessor for RuntimeEffectGroup {
    fn group_key(&self) -> &str {
        self.group_key()
    }

    fn children(&self) -> usize {
        self.children().len()
    }

    fn wake(&self) -> GroupWakePolicy {
        self.wake()
    }

    fn loser_disposition(&self) -> LoserPolicy {
        self.loser_disposition()
    }
}

/// Refuses a reopen whose recorded group facts disagree with the offered ones.
pub(crate) fn fence_reopen<Opening, Persisted>(
    opening: &Opening,
    persisted: &Persisted,
) -> Result<(), RuntimeEffectControllerError>
where
    Opening: EffectGroupRecordAccessor,
    Persisted: EffectGroupRecordAccessor,
{
    if opening.children() != persisted.children() {
        return Err(group_shape_error(format!(
            "durable effect group {} is recorded with {} children but was reopened \
             with {}; a changed child count renumbers every rank above the change, \
             so the settlements already consumed would no longer name the children \
             that produced them",
            opening.group_key(),
            persisted.children(),
            opening.children()
        )));
    }
    if opening.wake() != persisted.wake() {
        return Err(group_shape_error(format!(
            "durable effect group {} is recorded under wake rule {:?} but was \
             reopened under {:?}; the wake rule is journaled identity and a reopen \
             may not change it",
            opening.group_key(),
            persisted.wake(),
            opening.wake()
        )));
    }
    if opening.loser_disposition() != persisted.loser_disposition() {
        return Err(group_shape_error(format!(
            "durable effect group {} declared loser disposition {:?} at open but was \
             reopened declaring {:?}; the declared disposition is what a drain of \
             this group applies, so a reopen may not restate it",
            opening.group_key(),
            persisted.loser_disposition(),
            opening.loser_disposition()
        )));
    }
    Ok(())
}

pub(crate) fn group_shape_error(message: impl Into<String>) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(crate::RuntimeErrorCode::RuntimeEffectGroupShape, message)
}

/// Refuses an await after the caller consumed every recorded settlement.
pub(crate) fn exhausted_group_error(handle: &EffectGroupHandle) -> RuntimeEffectControllerError {
    group_shape_error(format!(
        "durable effect group {} has all {} settlements consumed; check \
         is_exhausted() before awaiting rather than awaiting past the group",
        handle.group_key(),
        handle.children()
    ))
}

/// Refuses an await cancellation without advancing the caller's cursor.
pub(crate) fn await_cancelled_error(group_key: &str, rank: usize) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
        format!(
            "the await of settlement {rank} of durable effect group {group_key} \
             was cancelled; the group's rank is untouched and a later await resumes \
             at the same settlement"
        ),
    )
}

/// Reports the terminal recorded for a child cancelled by its group policy.
pub(crate) fn child_cancelled_error(
    group_key: &str,
    position: usize,
) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelled,
        format!(
            "child {position} of durable effect group {group_key} was cancelled by \
             the group's declared loser disposition; the cancellation is this \
             child's terminal"
        ),
    )
}

/// Refuses access to a group that is closed to its caller.
pub(crate) fn closed_group_error(group_key: &str) -> RuntimeEffectControllerError {
    group_shape_error(format!(
        "durable effect group {group_key} is closed to its caller; a closed group's \
         remaining children settle under host ownership and the caller may not \
         observe them"
    ))
}

/// Refuses an effect that carries group membership on a path that cannot honor
/// it, for effect-host implementors dispatching on command shape.
///
/// Some command shapes are rebuilt into targets with no slot for membership —
/// Restate's timer and await-event executions, for instance, record no canonical
/// envelope at all. Dropping the field there would be silent, and rustc cannot
/// warn because sibling arms use the binding. A grouped child reaching such a
/// path loses the envelope-hash fence that makes "replay cannot silently change
/// the wake rule" backed rather than asserted, so the honest answer is a typed
/// refusal.
pub fn refuse_unhonored_group_membership(
    group: Option<&EffectGroupMembership>,
    shape: &str,
) -> Result<(), RuntimeEffectControllerError> {
    match group {
        None => Ok(()),
        Some(membership) => Err(group_shape_error(format!(
            "child {} of durable effect group {} reached the {shape} path, which \
             cannot carry group membership; refusing rather than dropping the \
             membership and losing the envelope-hash fence",
            membership.position, membership.group_key
        ))),
    }
}

/// The caller's handle on an open effect group, and **the sole cursor of record**
/// for how far that caller has consumed the group's settlement order.
///
/// Normative, because two plausible implementations otherwise disagree
/// silently: the handle owns `consumed`; hosts keep **no** per-caller
/// consumption state.
/// [`await_next_settlement`](super::RuntimeEffectController::await_next_settlement)
/// takes it by `&mut` and advances it on exactly the deliveries it returns, so
/// awaiting rank `consumed + 1` twice — once before a crash and once after —
/// yields the same settlement rather than skipping one. It follows that a host
/// must not implement the await as "take the next journal entry / next durable
/// future", which would advance regardless of the cursor.
///
/// On reopen the **caller's** cursor wins. A host knows how many children have
/// *settled*; only the caller knows how many it has *consumed*, so
/// [`open_effect_group`](super::RuntimeEffectController::open_effect_group)
/// returns a handle at `consumed = 0` and a VM frame restoring a group from its
/// continuation must use [`restored`](Self::restored) with the cursor it saved.
///
/// Deliberately not `Clone`: two handles on one group are two cursors on a
/// single-consumer sequence, and both receiving rank `n` would be
/// representable.
///
/// Both ways in validate the cursor against the child count, including
/// `Deserialize` — which is the path a durable continuation actually arrives on,
/// so a derived decode would have let a corrupt handle in behind the
/// constructor's back. See [`restored`](Self::restored) for the rule.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RestoredEffectGroupHandleRepr")]
pub struct EffectGroupHandle {
    group_key: String,
    children: usize,
    consumed: usize,
}

impl EffectGroupHandle {
    /// A fresh handle on a newly opened group, for effect-host implementors
    /// returning from `open_effect_group`.
    ///
    /// Takes the group rather than a key and a count, so the two cannot
    /// disagree and the count cannot be zero: [`RuntimeEffectGroup`] refuses an
    /// empty group at construction, and a zero-child handle is precisely the
    /// corrupt shape [`restored`](Self::restored) refuses on the way back in.
    /// Minting one here and only discovering it at continuation *load* would
    /// surface a host's arithmetic slip as an unresumable continuation, far from
    /// the slip.
    #[must_use]
    pub fn new(group: &RuntimeEffectGroup) -> Self {
        Self {
            group_key: group.group_key().to_string(),
            children: group.children().len(),
            consumed: 0,
        }
    }

    /// A handle restored from a durable continuation at the cursor the caller
    /// saved.
    ///
    /// Fallible, because the inputs come off a durable record rather than out of
    /// this process, and both malformed shapes read as *exhausted* through
    /// [`is_exhausted`](Self::is_exhausted) — the one state that looks like
    /// orderly completion. A cursor past the child count, or a group claiming no
    /// children at all (impossible by construction: [`RuntimeEffectGroup`]
    /// refuses an empty group), would therefore make a corrupt continuation
    /// resume as a group that had finished, dropping every settlement still owed
    /// with no error anywhere. Refusing is the only reading that stays honest.
    pub fn restored(
        group_key: impl Into<String>,
        children: usize,
        consumed: usize,
    ) -> Result<Self, RuntimeEffectControllerError> {
        let group_key = group_key.into();
        if children == 0 {
            return Err(group_shape_error(format!(
                "effect group handle for {group_key} claims zero children; an \
                 empty group is refused at construction, so this is a corrupt \
                 continuation and not an exhausted group"
            )));
        }
        if consumed > children {
            return Err(group_shape_error(format!(
                "effect group handle for {group_key} claims {consumed} settlements \
                 consumed of {children} children; a cursor past the child count is \
                 a corrupt continuation, and reading it as an exhausted group \
                 would drop the settlements still owed"
            )));
        }
        Ok(Self {
            group_key,
            children,
            consumed,
        })
    }

    /// The group this handle refers to.
    #[must_use]
    pub fn group_key(&self) -> &str {
        &self.group_key
    }

    /// How many children the group has.
    #[must_use]
    pub fn children(&self) -> usize {
        self.children
    }

    /// Settlements already consumed by this caller. The next await serves rank
    /// `consumed + 1`.
    #[must_use]
    pub fn consumed(&self) -> usize {
        self.consumed
    }

    /// Exhaustion is the caller's arithmetic — it is knowable from the handle
    /// alone without a round trip, which is why the await has no `Option` in its
    /// return type.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.consumed >= self.children
    }

    /// Called by `await_next_settlement` on exactly the settlements it returns;
    /// a cancelled or failed await must not advance the cursor.
    ///
    /// Refuses to advance past [`is_exhausted`](Self::is_exhausted) rather than
    /// clamping, because there is no rank `children + 1` to have delivered: the
    /// call is a host serving a settlement it cannot have, and clamping would
    /// hide exactly that. Refusing also keeps the cursor a shape
    /// [`restored`](Self::restored) accepts, so an arithmetic slip surfaces at
    /// the slip instead of as an unresumable continuation at load time — the
    /// caller checks `is_exhausted` before awaiting, and this is what holds it
    /// to that.
    pub fn advance(&mut self) -> Result<(), RuntimeEffectControllerError> {
        if self.is_exhausted() {
            return Err(group_shape_error(format!(
                "effect group handle for {} advanced past its last child: all {} \
                 settlements are already consumed, so there is no rank {} to have \
                 been delivered. Check is_exhausted() before awaiting rather than \
                 awaiting past the group",
                self.group_key,
                self.children,
                self.consumed + 1
            )));
        }
        self.consumed += 1;
        Ok(())
    }
}

/// Decode target routing `EffectGroupHandle`'s `Deserialize` through
/// [`EffectGroupHandle::restored`], so the continuation path cannot admit a
/// handle the constructor would refuse.
#[derive(Deserialize)]
struct RestoredEffectGroupHandleRepr {
    group_key: String,
    children: usize,
    consumed: usize,
}

impl TryFrom<RestoredEffectGroupHandleRepr> for EffectGroupHandle {
    type Error = String;

    fn try_from(repr: RestoredEffectGroupHandleRepr) -> Result<Self, Self::Error> {
        Self::restored(repr.group_key, repr.children, repr.consumed)
            .map_err(|error| error.message.clone())
    }
}

/// One settlement delivered by
/// [`await_next_settlement`](super::RuntimeEffectController::await_next_settlement).
pub struct GroupSettlement {
    /// Position of the settled child in [`RuntimeEffectGroup::children`].
    pub position: usize,
    /// Allocated once per child at finalize: monotonic and unique within the
    /// group, and deliberately **not** gapless — journal retirement and
    /// rolled-back finalizes both remove values without shifting any rank.
    /// Consumers therefore order by rank, never by literal equality.
    ///
    /// `u64` to match the `BIGINT` counter ADR 0065 specifies, whose remedy for
    /// a rolled-back finalize is to *burn* values: a narrower width would force
    /// a fallible cast at the store seam on a counter designed to skip.
    pub sequence: u64,
    pub outcome: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
}

impl std::fmt::Debug for GroupSettlement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = f.debug_struct("GroupSettlement");
        out.field("position", &self.position)
            .field("sequence", &self.sequence);
        // Keep the error text: a failed settlement is the one case an operator
        // reads this for, and `settled_ok: false` alone discards the reason.
        match &self.outcome {
            Ok(_) => out.field("settled_ok", &true),
            Err(error) => out.field("settled_ok", &false).field("error", &error),
        };
        out.finish()
    }
}

/// One rank of the incorporation prefix a group-settlement record journaled
/// (ADR 0099 §6): the durable rank and the settled child's replay key — its
/// identity in [`SettlementSource::GroupRank`], not its position.
///
/// [`SettlementSource::GroupRank`]: crate::session::SettlementSource::GroupRank
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncorporatedGroupRank {
    pub rank: u64,
    pub child_replay_key: String,
}

/// A group's settlement at a durable rank, read back without advancing any
/// caller cursor (ADR 0099 §8): the allocated sequence, the settled child's
/// replay key, and its recorded terminal. Unlike [`GroupSettlement`] — the
/// consume view — this names the child the rank belongs to, which is what a
/// prefix incorporation needs to build its [`SettlementSource::GroupRank`]
/// identity. Position is deliberately absent: incorporation orders by rank and
/// never needs the declared slot.
///
/// [`SettlementSource::GroupRank`]: crate::session::SettlementSource::GroupRank
#[derive(Clone, Debug)]
pub struct RankedGroupSettlement {
    pub sequence: u64,
    pub child_replay_key: String,
    pub outcome: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
}

/// What becomes of a group's remaining children once the caller stops consuming.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoserPolicy {
    /// Losers run to completion under host ownership and journal their own
    /// settlements. Node-exact for `race`/`any`: a losing promise keeps running
    /// and its side effects still happen.
    RunToCompletion,
    /// Losers are cancelled and each cancellation is journaled as that child's
    /// terminal. The correct disposition for a deadline arm.
    Cancel,
}

impl LoserPolicy {
    /// Resolves the disposition a close should apply, from the one the group
    /// `declared` at open — or, once the durable lifecycle is `closing`, the
    /// one the group row already committed — and the one the closing caller
    /// `requested`.
    ///
    /// An associated function taking both arguments in that order, rather than a
    /// method: either operand is a plausible receiver, so `requested.method(declared)`
    /// and `declared.method(requested)` read the same and mean opposite things.
    ///
    /// Close may only **narrow**: `RunToCompletion` may be tightened to `Cancel`
    /// by a caller that has learned it no longer wants the losers, but a
    /// declared `Cancel` may not be widened back to `RunToCompletion`. Widening
    /// is refused rather than honored because the declared disposition is what a
    /// crash-drain of the same group will apply, so permitting it would make the
    /// losers' fate depend on whether the caller happened to reach its close —
    /// which is the divergence declaring at open exists to remove.
    pub fn resolve_close(
        declared: Self,
        requested: Self,
    ) -> Result<Self, RuntimeEffectControllerError> {
        match (declared, requested) {
            (Self::Cancel, Self::RunToCompletion) => Err(group_shape_error(
                "a durable effect group that declared Cancel at open may not be \
                 closed as RunToCompletion; close may only narrow, because a \
                 crash-drain of the same group applies the declared disposition",
            )),
            (_, requested) => Ok(requested),
        }
    }
}

/// How long a group's finalization waits for a cancel-decided child's attempt
/// body after that child's decision has committed (ADR 0099 §7, FIG-3410).
///
/// The clock starts at the decision, not at the close: the decision is the
/// durable fact — the child's rank is already seated by it — and what the
/// budget bounds is how long the finalizer keeps waiting for the attempt body
/// to return before it proceeds with the rank it already owns. A body that
/// ignores its token past the budget is logically cancelled: its task is left
/// to run, and nothing it can write afterward is a write the journal will
/// accept, because its seat is taken.
///
/// The default is set at controller construction — beside the effect-budget
/// options — and changing it is an operational choice, never a semantic one:
/// the committed obligations a group owes do not move when the bound does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectGroupDrainBudget(Duration);

impl EffectGroupDrainBudget {
    /// The shipped bound: thirty seconds for a cancelled attempt body to
    /// return once its decision is durable.
    pub const DEFAULT: Self = Self(Duration::from_secs(30));

    /// A caller-chosen bound.
    #[must_use]
    pub fn new(duration: Duration) -> Self {
        Self(duration)
    }

    /// The bound itself.
    #[must_use]
    pub fn duration(self) -> Duration {
        self.0
    }
}

impl Default for EffectGroupDrainBudget {
    fn default() -> Self {
        Self::DEFAULT
    }
}
