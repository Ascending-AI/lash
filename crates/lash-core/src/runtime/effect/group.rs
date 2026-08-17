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

use serde::{Deserialize, Serialize};

use super::envelope::{RuntimeEffectOutcome, RuntimeInvocation};
use super::{RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor};

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
    /// `{scope_id}:group:{batch_id}:{occurrence}`.
    ///
    /// The occurrence ordinal is load-bearing: `batch_id` is a content hash of
    /// the calls, so two textually identical `race` calls in one protocol
    /// iteration hash identically and would otherwise share a group.
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
    pub loser_disposition: LoserDisposition,
}

/// A group of independently journaled child effects opened at the effect-host
/// seam (FIG-1416).
///
/// The children are ordinary envelopes carrying ordinary
/// [`RuntimeEffectCommand`](super::envelope::RuntimeEffectCommand)s — groups
/// introduce no new command variant, because what is new is the *composition
/// above* attempts, not the attempts.
///
/// Construct with [`try_new`](Self::try_new). The fields are readable but not
/// publicly writable, because every durability claim in ADR 0065 reduces to the
/// group key, wake rule, and child positions *agreeing* across the group record
/// and each child's [`EffectGroupMembership`] — and hand-stamped copies make
/// disagreement both representable and invisible until a production replay
/// refuses.
#[derive(Clone, Debug)]
pub struct RuntimeEffectGroup {
    invocation: RuntimeInvocation,
    group_key: String,
    children: Vec<RuntimeEffectEnvelope>,
    wake: GroupWakePolicy,
    loser_disposition: LoserDisposition,
}

impl RuntimeEffectGroup {
    /// Validates and assembles a durable effect group, stamping each child's
    /// membership from its own index and the group's key and wake rule.
    ///
    /// This is the only constructor, so a host handed a `RuntimeEffectGroup` may
    /// rely on all of the following rather than re-deriving them:
    ///
    /// * `children` is non-empty;
    /// * every child's `group_key` is this group's key;
    /// * every child's `position` is its index in `children`;
    /// * every child's `wake` is this group's wake rule;
    /// * every child's `loser_disposition` is this group's declared disposition.
    ///
    /// Children that arrive already stamped are checked rather than trusted;
    /// children that arrive unstamped are stamped here. Either way one group has
    /// one key, so a host never fishes identity out of `children[0]`.
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
    /// `allSettled` mean [`LoserDisposition::RunToCompletion`], and `race`/`any`
    /// take whichever the ratified race semantics specify — and it is the same
    /// class of per-group durable fact as [`wake`](Self::wake). Leaving it as a
    /// close-time argument meant a caller that crashed after opening and before
    /// closing left the host no record of which disposition applied, so the
    /// group-drain path had to invent one, silently running a deadline arm's
    /// losers to completion on exactly the failure path this contract exists for.
    pub fn try_new(
        invocation: RuntimeInvocation,
        group_key: impl Into<String>,
        children: Vec<RuntimeEffectEnvelope>,
        wake: GroupWakePolicy,
        loser_disposition: LoserDisposition,
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
    pub fn invocation(&self) -> &RuntimeInvocation {
        &self.invocation
    }

    /// `{scope_id}:group:{batch_id}:{occurrence}` — the key the host records the
    /// group and its settlement counter under.
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
    pub fn loser_disposition(&self) -> LoserDisposition {
        self.loser_disposition
    }

    /// Refuses an executor vec that is not positionally aligned with this
    /// group's children.
    ///
    /// Private on purpose: the only way to reach it is
    /// [`CheckedEffectGroup::try_new`], so the check is structural rather than
    /// an obligation each host is asked to remember.
    fn check_executors(
        &self,
        executors: &[RuntimeEffectLocalExecutor<'static>],
    ) -> Result<(), RuntimeEffectControllerError> {
        if executors.len() == self.children.len() {
            return Ok(());
        }
        Err(group_shape_error(format!(
            "durable effect group {} has {} children but was opened with {} \
             executors; executor i runs child i, so an unaligned vec either \
             panics on an index or journals a group whose missing children can \
             never settle. A child with no runnable executor is \
             RuntimeEffectLocalExecutor::unavailable, not a missing element",
            self.group_key,
            self.children.len(),
            executors.len()
        )))
    }
}

fn group_shape_error(message: impl Into<String>) -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(crate::RuntimeErrorCode::RuntimeEffectGroupShape, message)
}

/// A durable effect group paired with one `'static` executor per child, the
/// alignment already checked — the sole parameter of
/// [`open_effect_group`](super::RuntimeEffectController::open_effect_group).
///
/// The pairing is a type rather than two parameters because "executor `i` runs
/// child `i`" is the load-bearing claim and a two-parameter signature makes
/// checking it an obligation on every implementation. An implementation that
/// skipped the check would resolve a short vec by panicking on an index or by
/// silently running a prefix of the group — journaling a group whose missing
/// children can never settle, so every rank above the truncation is unservable
/// and the caller waits forever. Since [`try_new`](Self::try_new) is the only
/// constructor and `open_effect_group` takes nothing else, an unaligned pair
/// cannot reach a host at all: the check happens once, at the boundary, and no
/// host is asked to remember it.
///
/// A child with no runnable executor is expressed as
/// [`RuntimeEffectLocalExecutor::unavailable`], not as a missing element, which
/// is why the alignment is an equality rather than a lower bound.
pub struct CheckedEffectGroup {
    group: RuntimeEffectGroup,
    executors: Vec<RuntimeEffectLocalExecutor<'static>>,
}

impl std::fmt::Debug for CheckedEffectGroup {
    /// Hand-written because `RuntimeEffectLocalExecutor` is not `Debug`. The
    /// count is the only interesting thing about the vec anyway — it is the half
    /// of the invariant this type exists to hold.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckedEffectGroup")
            .field("group", &self.group)
            .field("executors", &self.executors.len())
            .finish()
    }
}

impl CheckedEffectGroup {
    /// Pairs a group with its executors, refusing any vec not positionally
    /// aligned with [`RuntimeEffectGroup::children`] with a typed group-shape
    /// error.
    pub fn try_new(
        group: RuntimeEffectGroup,
        executors: Vec<RuntimeEffectLocalExecutor<'static>>,
    ) -> Result<Self, RuntimeEffectControllerError> {
        group.check_executors(&executors)?;
        Ok(Self { group, executors })
    }

    /// The group, for a host that needs its key, wake rule, or children before
    /// taking ownership of the executors.
    #[must_use]
    pub fn group(&self) -> &RuntimeEffectGroup {
        &self.group
    }

    /// Splits the checked pair into its parts, for a host that dispatches the
    /// children.
    ///
    /// By value because [`RuntimeEffectLocalExecutor::execute`] consumes its
    /// executor: the vec funds one attempt per child and no retry, and a host
    /// that retries takes a fresh executor from
    /// [`EffectHost::scoped_static`](super::EffectHost::scoped_static).
    #[must_use]
    pub fn into_parts(self) -> (RuntimeEffectGroup, Vec<RuntimeEffectLocalExecutor<'static>>) {
        (self.group, self.executors)
    }
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

    /// Whether every child's settlement has been consumed, so a further await
    /// would have no rank to serve.
    ///
    /// Exhaustion is the caller's arithmetic — it is knowable from the handle
    /// alone without a round trip, which is why the await has no `Option` in its
    /// return type.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.consumed >= self.children
    }

    /// Records one delivered settlement, for effect-host implementors.
    ///
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

/// What becomes of a group's remaining children once the caller stops consuming.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoserDisposition {
    /// Losers run to completion under host ownership and journal their own
    /// settlements. Node-exact for `race`/`any`: a losing promise keeps running
    /// and its side effects still happen.
    RunToCompletion,
    /// Losers are cancelled and each cancellation is journaled as that child's
    /// terminal. The correct disposition for a deadline arm.
    Cancel,
}

impl LoserDisposition {
    /// Resolves the disposition a close should apply, from the one the group
    /// `declared` at open and the one the closing caller `requested`.
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

#[cfg(test)]
mod effect_group_contract_tests {
    use super::*;
    use crate::runtime::effect::envelope::{RuntimeEffectCommand, RuntimeEffectKind, RuntimeScope};

    fn invocation(kind: RuntimeEffectKind) -> RuntimeInvocation {
        RuntimeInvocation::effect(RuntimeScope::new("session"), "effect", kind, "replay")
    }

    fn await_event_key() -> crate::AwaitEventKey {
        crate::AwaitEventKey {
            scope: crate::ExecutionScope::Turn {
                session_id: "session".to_string(),
                turn_id: "turn".to_string(),
            },
            wait: crate::AwaitEventWaitIdentity::ToolCompletion {
                tool_call_id: "call".to_string(),
            },
            key_id: "key".to_string(),
            signature: "signature".to_string(),
        }
    }

    /// One envelope per cheaply-constructible non-group command variant.
    ///
    /// The corpus deliberately spans every command *shape* the encoding could
    /// perturb — unit-ish payloads, string payloads, keyed payloads, and the
    /// nested `ToolAttempt`/`ToolBatch` payloads — because the field being
    /// guarded sits on the envelope rather than inside any one command.
    ///
    /// Nine of the fourteen `RuntimeEffectCommand` variants are covered. The
    /// five omissions are named rather than implied: `LlmCall`, `Direct` and
    /// `AssistantResponseHooks` need a full `LlmRequestSpec`/`LlmResponse`,
    /// `Trigger` a `TriggerCommand`, and `Process` a `ProcessCommand` — payloads
    /// whose construction cost buys nothing here, because the field under guard
    /// sits on the *envelope*, so its omission is variant-independent and one
    /// covered variant already proves the encoding. The corpus width is a
    /// belt-and-braces pin on future edits, not the argument.
    fn ungrouped_corpus() -> Vec<(&'static str, RuntimeEffectEnvelope)> {
        let prepared = crate::PreparedToolCall::from_parts(
            "call",
            "tool:test",
            "test",
            serde_json::json!({}),
            None,
            serde_json::Value::Null,
        );
        let commands: Vec<(&'static str, RuntimeEffectKind, RuntimeEffectCommand)> = vec![
            (
                "sleep",
                RuntimeEffectKind::Sleep,
                RuntimeEffectCommand::Sleep { duration_ms: 1_000 },
            ),
            (
                "exec_code",
                RuntimeEffectKind::ExecCode,
                RuntimeEffectCommand::ExecCode {
                    language: "typescript".to_string(),
                    code: "1 + 1".to_string(),
                },
            ),
            (
                "sync_execution_environment",
                RuntimeEffectKind::SyncExecutionEnvironment,
                RuntimeEffectCommand::SyncExecutionEnvironment {
                    update_machine_config: true,
                },
            ),
            (
                "language_runtime_value",
                RuntimeEffectKind::LanguageRuntimeValue,
                RuntimeEffectCommand::LanguageRuntimeValue {
                    operation: "read".to_string(),
                },
            ),
            (
                "tool_attempt",
                RuntimeEffectKind::ToolAttempt,
                RuntimeEffectCommand::ToolAttempt {
                    call: prepared.clone(),
                    execution_grant: None,
                    attempt: 1,
                    max_attempts: 1,
                },
            ),
            (
                "tool_batch",
                RuntimeEffectKind::ToolBatch,
                RuntimeEffectCommand::ToolBatch {
                    batch: crate::PreparedToolBatch::new("batch", vec![prepared]),
                },
            ),
            (
                "checkpoint",
                RuntimeEffectKind::Checkpoint,
                RuntimeEffectCommand::Checkpoint {
                    checkpoint: crate::CheckpointKind::AfterWork,
                },
            ),
            (
                "await_event",
                RuntimeEffectKind::AwaitEvent,
                RuntimeEffectCommand::AwaitEvent {
                    key: await_event_key(),
                },
            ),
            (
                "peek_await_event",
                RuntimeEffectKind::PeekAwaitEvent,
                RuntimeEffectCommand::PeekAwaitEvent {
                    key: await_event_key(),
                },
            ),
        ];
        commands
            .into_iter()
            .map(|(name, kind, command)| {
                (name, RuntimeEffectEnvelope::new(invocation(kind), command))
            })
            .collect()
    }

    /// N2: an ungrouped effect's canonical encoding — and therefore its
    /// recorded `envelope_hash` — must be byte-identical to what it was before
    /// effect groups added a field to the envelope.
    ///
    /// Provenance, stated precisely because it is the whole value of the test:
    /// the first six hashes were generated on the pre-change tree (base
    /// `eb0935293`) and are unchanged by the added field, so they are genuine
    /// before/after evidence. The last three variants were added afterwards and
    /// their constants come from the post-change tree; what makes *those* sound
    /// is the structural argument pinned by
    /// [`an_ungrouped_envelope_omits_the_group_field_entirely`] — an omitted
    /// field cannot perturb a preimage — with the constants serving as a
    /// regression pin on future edits rather than as before/after evidence.
    ///
    /// They are golden constants rather than a self-consistency check on
    /// purpose: the defect
    /// this guards is a *mass* `ReplayMismatch` on a live Postgres upgrade,
    /// where every in-flight effect — grouped or not — comes back refused
    /// because a field those effects never use perturbed their preimage.
    /// Postgres is not reject-and-recreate, so nothing else catches it before a
    /// customer's cutover.
    ///
    /// If a legitimate future encoding change moves one of these, that change
    /// must be paired with a drain, exactly as ADR 0055 requires.
    #[test]
    fn ungrouped_envelope_hashes_are_unchanged_by_the_group_field() {
        let golden = [
            (
                "sleep",
                "d96891003664a61763bb49284f8dd1d0b35a64aa15d01127bb754150b42ecb09",
            ),
            (
                "exec_code",
                "9ffc0e3ba1eb152f1e60ba4edce481582dd4b8e925862ac97782f19f625f0fee",
            ),
            (
                "sync_execution_environment",
                "07a4d4add2e90b0ce01aecd5d0cf974219f83952d3cb690e0a1e74cf7c09243e",
            ),
            (
                "language_runtime_value",
                "4455bba80fe1d30eef4d39c65f4c3022bce004b3c13ab2e88d1df39f57cae1b9",
            ),
            (
                "tool_attempt",
                "a13d8587fef4c3a19f4ca8b5b2d42e5007ba80b466b130cd6487d6d6ce8832e1",
            ),
            (
                "tool_batch",
                "7430bb8de237ee74cfc8d6e26393af1ef65dc4f0ef22a83ad5d9bd75b1945e5e",
            ),
            (
                "checkpoint",
                "9502f267e1461eb176dd29da35d919ec9f56fa723b367da6bdd3ea3b038863b7",
            ),
            (
                "await_event",
                "0ff3f3b752fa9a9407430ccea9f0f9689fc966356f63a55d64524c0f4532d424",
            ),
            (
                "peek_await_event",
                "a142ddb9de882b71b028178d4d4cd698256d5029835ad786061a758b384ec3d2",
            ),
        ];
        let corpus = ungrouped_corpus();
        assert_eq!(
            corpus.len(),
            golden.len(),
            "every corpus entry needs a golden hash"
        );
        for ((name, envelope), (golden_name, golden_hash)) in corpus.iter().zip(golden) {
            assert_eq!(
                *name, golden_name,
                "corpus and golden list must stay aligned"
            );
            let hash = envelope.stable_hash().expect("envelope hashes");
            assert_eq!(
                hash, golden_hash,
                "the canonical encoding of an ungrouped `{name}` effect moved; \
                 every recorded envelope_hash on a live Postgres journal just \
                 became a ReplayMismatch"
            );
        }
    }

    /// The structural reason the golden hashes above hold: the field is omitted
    /// entirely rather than encoded as `null`.
    ///
    /// Kept beside the golden test because it is the invariant a future editor
    /// would break — dropping `skip_serializing_if` still encodes *validly*, so
    /// only this assertion names the mistake.
    #[test]
    fn an_ungrouped_envelope_omits_the_group_field_entirely() {
        for (name, envelope) in ungrouped_corpus() {
            let json = crate::stable_hash::stable_json_string(&envelope).expect("envelope encodes");
            // Structural, not a substring search: a corpus payload that merely
            // *contained* the text "group" would otherwise read as a hash
            // regression.
            let decoded = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&json)
                .expect("an envelope encodes as a JSON object");
            assert!(
                !decoded.contains_key("group"),
                "an ungrouped `{name}` envelope must not encode a top-level group key at all: {json}"
            );
        }
    }

    /// A group child's membership folds into its hash, which is what makes
    /// "replay cannot silently change the wake rule" backed rather than
    /// asserted — it is the only mechanism available on engine tiers that keep
    /// no group row.
    #[test]
    fn group_membership_and_wake_drift_change_the_child_hash() {
        let base = RuntimeEffectEnvelope::new(
            invocation(RuntimeEffectKind::Sleep),
            RuntimeEffectCommand::Sleep { duration_ms: 1 },
        );
        let ungrouped = base.stable_hash().expect("hashes");
        let first = base
            .clone()
            .in_effect_group(
                "scope:group:batch:0",
                0,
                GroupWakePolicy::First,
                LoserDisposition::RunToCompletion,
            )
            .stable_hash()
            .expect("hashes");
        let wake_drifted = base
            .clone()
            .in_effect_group(
                "scope:group:batch:0",
                0,
                GroupWakePolicy::All,
                LoserDisposition::RunToCompletion,
            )
            .stable_hash()
            .expect("hashes");
        let position_drifted = base
            .clone()
            .in_effect_group(
                "scope:group:batch:0",
                1,
                GroupWakePolicy::First,
                LoserDisposition::RunToCompletion,
            )
            .stable_hash()
            .expect("hashes");
        let group_drifted = base
            .in_effect_group(
                "scope:group:batch:1",
                0,
                GroupWakePolicy::First,
                LoserDisposition::RunToCompletion,
            )
            .stable_hash()
            .expect("hashes");

        let distinct = std::collections::HashSet::from([
            ungrouped.clone(),
            first.clone(),
            wake_drifted.clone(),
            position_drifted.clone(),
            group_drifted.clone(),
        ]);
        assert_eq!(
            distinct.len(),
            5,
            "membership, wake rule, position, and group key must each move the hash"
        );
        assert_ne!(
            first, wake_drifted,
            "a replay whose wake rule changed must be refused by the envelope-hash fence"
        );
    }

    /// `GroupWakePolicy` carries no serde default, mirroring
    /// `settlement_order`'s fail-closed discipline: a group child recorded
    /// without a wake rule is refused rather than replayed under a guessed one.
    #[test]
    fn a_group_membership_without_a_wake_rule_fails_closed() {
        let legacy = serde_json::json!({
            "group_key": "scope:group:batch:0",
            "position": 0,
        });
        let error = serde_json::from_value::<EffectGroupMembership>(legacy)
            .expect_err("a membership without a wake rule must not decode");
        assert!(
            error.to_string().contains("wake"),
            "the refusal must name the missing wake rule: {error}"
        );
    }

    #[test]
    fn group_membership_round_trips_through_the_envelope() {
        let envelope = RuntimeEffectEnvelope::new(
            invocation(RuntimeEffectKind::Sleep),
            RuntimeEffectCommand::Sleep { duration_ms: 1 },
        )
        .in_effect_group(
            "scope:group:batch:2",
            3,
            GroupWakePolicy::FirstSuccess,
            LoserDisposition::Cancel,
        );
        let encoded = serde_json::to_string(&envelope).expect("envelope encodes");
        let decoded =
            serde_json::from_str::<RuntimeEffectEnvelope>(&encoded).expect("envelope decodes");
        let membership = decoded.group.expect("membership survives the round trip");
        assert_eq!(membership.group_key, "scope:group:batch:2");
        assert_eq!(membership.position, 3);
        assert_eq!(membership.wake, GroupWakePolicy::FirstSuccess);
    }

    /// The handle is durable continuation state, so its consumed cursor must
    /// survive encoding: a restored frame that lost it would re-consume rank 1
    /// and observe the winner twice.
    #[test]
    fn an_effect_group_handle_round_trips_its_consumed_cursor() {
        let handle = EffectGroupHandle::restored("scope:group:batch:0", 3, 2)
            .expect("a sane cursor restores");
        let encoded = serde_json::to_string(&handle).expect("handle encodes");
        let decoded = serde_json::from_str::<EffectGroupHandle>(&encoded).expect("handle decodes");
        assert_eq!(decoded, handle);
        assert_eq!(decoded.consumed(), 2);
        assert!(!decoded.is_exhausted());
    }

    /// A corrupt continuation must fail closed rather than resume as a finished
    /// group. Both malformed shapes below satisfy `is_exhausted()`, which is the
    /// one state indistinguishable from orderly completion, so an unvalidated
    /// restore would silently drop every settlement still owed.
    ///
    /// Asserted on `Deserialize` as well as the constructor, because a durable
    /// continuation arrives by decoding: a derived impl would admit exactly the
    /// handles `restored` refuses.
    #[test]
    fn a_corrupt_effect_group_handle_is_refused_rather_than_read_as_exhausted() {
        let cases = [
            ("cursor past the child count", 3usize, 4usize),
            ("a group with no children", 0, 0),
        ];
        for (name, children, consumed) in cases {
            let error = EffectGroupHandle::restored("scope:group:batch:0", children, consumed)
                .expect_err(&format!("{name} must not restore"));
            assert_eq!(
                error.code,
                crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                "{name} must be a typed group-shape refusal"
            );

            let encoded = serde_json::json!({
                "group_key": "scope:group:batch:0",
                "children": children,
                "consumed": consumed,
            });
            let decode_error = serde_json::from_value::<EffectGroupHandle>(encoded)
                .expect_err(&format!("{name} must not decode either"));
            assert!(
                decode_error.to_string().contains("scope:group:batch:0"),
                "the decode refusal must carry the constructor's reason: {decode_error}"
            );
        }
    }

    #[test]
    fn a_handle_reports_exhaustion_once_every_child_is_consumed() {
        let mut handle = EffectGroupHandle::new(&group_of(2));
        assert_eq!(handle.consumed(), 0, "a fresh handle has consumed nothing");
        assert!(!handle.is_exhausted());
        handle.advance().expect("rank 1 of 2");
        assert!(!handle.is_exhausted());
        handle.advance().expect("rank 2 of 2");
        assert!(
            handle.is_exhausted(),
            "exhaustion is the caller's arithmetic, knowable without a round trip"
        );
    }

    /// The write side of the same fence
    /// [`a_corrupt_effect_group_handle_is_refused_rather_than_read_as_exhausted`]
    /// holds on decode. Validating only the read side left both corrupt shapes
    /// *mintable*: a host could construct them, serialize them into a
    /// continuation, and only discover the problem when that continuation failed
    /// to load — surfacing an arithmetic slip as an unresumable session, far from
    /// the slip. Refused rather than clamped, because clamping is what hides the
    /// lost settlements.
    #[test]
    fn a_handle_cannot_mint_the_corrupt_states_decode_refuses() {
        // Probe 1 — a zero-child handle. Unrepresentable rather than refused:
        // `new` derives the count from the group, and `try_new` refuses an empty
        // group, so there is no argument that produces one.
        RuntimeEffectGroup::try_new(
            invocation(RuntimeEffectKind::Sleep),
            "scope:group:batch:0",
            Vec::new(),
            GroupWakePolicy::First,
            LoserDisposition::RunToCompletion,
        )
        .expect_err("an empty group is the only source of a zero-child handle");
        let single = EffectGroupHandle::new(&group_of(1));
        assert_eq!(
            single.children(),
            1,
            "the child count comes from the group, so it cannot be zero or wrong"
        );

        // Probe 2 — a cursor past the child count, previously reachable by
        // advancing twice on a one-child group.
        let mut handle = EffectGroupHandle::new(&group_of(1));
        handle.advance().expect("rank 1 of 1");
        let error = handle
            .advance()
            .expect_err("advancing past the last child must be refused");
        assert_eq!(error.code, crate::RuntimeErrorCode::RuntimeEffectGroupShape);
        assert!(
            error.message.contains("scope:group:batch:0"),
            "the refusal must name the group: {error}"
        );
        assert_eq!(
            handle.consumed(),
            1,
            "a refused advance must leave the cursor where it was, not clamp past it"
        );

        // The legitimate exhausted state — children == consumed > 0 — is still a
        // first-class shape on both sides of the seam.
        assert!(handle.is_exhausted());
        let encoded = serde_json::to_string(&handle).expect("an exhausted handle encodes");
        let decoded =
            serde_json::from_str::<EffectGroupHandle>(&encoded).expect("and decodes again");
        assert_eq!(decoded, handle);
        assert!(decoded.is_exhausted());
    }

    fn child(position: usize, group_key: &str, wake: GroupWakePolicy) -> RuntimeEffectEnvelope {
        RuntimeEffectEnvelope::new(
            invocation(RuntimeEffectKind::Sleep),
            RuntimeEffectCommand::Sleep {
                duration_ms: position as u64 + 1,
            },
        )
        .in_effect_group(group_key, position, wake, LoserDisposition::RunToCompletion)
    }

    fn group_of(children: usize) -> RuntimeEffectGroup {
        RuntimeEffectGroup::try_new(
            invocation(RuntimeEffectKind::Sleep),
            "scope:group:batch:0",
            (0..children).map(unstamped_child).collect(),
            GroupWakePolicy::First,
            LoserDisposition::RunToCompletion,
        )
        .expect("a non-empty group assembles")
    }

    fn unstamped_child(position: usize) -> RuntimeEffectEnvelope {
        RuntimeEffectEnvelope::new(
            invocation(RuntimeEffectKind::Sleep),
            RuntimeEffectCommand::Sleep {
                duration_ms: position as u64 + 1,
            },
        )
    }

    /// The group is the one place the key, wake rule, and positions are made to
    /// agree. Every durability claim in ADR 0065 reduces to that agreement, and
    /// before `try_new` existed every disagreement below was representable,
    /// silent, and diagnosable only as a `ReplayMismatch` in production.
    #[test]
    fn assembling_a_group_stamps_unstamped_children_from_their_own_index() {
        let group = RuntimeEffectGroup::try_new(
            invocation(RuntimeEffectKind::Sleep),
            "scope:group:batch:0",
            vec![unstamped_child(0), unstamped_child(1)],
            GroupWakePolicy::First,
            LoserDisposition::RunToCompletion,
        )
        .expect("a group of unstamped children assembles");
        assert_eq!(group.group_key(), "scope:group:batch:0");
        assert_eq!(group.wake(), GroupWakePolicy::First);
        for (index, child) in group.children().iter().enumerate() {
            let membership = child.group.as_deref().expect("every child is stamped");
            assert_eq!(membership.group_key, "scope:group:batch:0");
            assert_eq!(membership.position, index);
            assert_eq!(membership.wake, GroupWakePolicy::First);
        }
    }

    #[test]
    fn assembling_a_group_refuses_children_that_disagree_with_it() {
        let key = "scope:group:batch:0";
        let cases: Vec<(&str, Vec<RuntimeEffectEnvelope>)> = vec![
            ("empty", vec![]),
            (
                "foreign key",
                vec![
                    child(0, key, GroupWakePolicy::First),
                    child(1, "scope:group:batch:1", GroupWakePolicy::First),
                ],
            ),
            (
                "permuted position",
                vec![
                    child(1, key, GroupWakePolicy::First),
                    child(0, key, GroupWakePolicy::First),
                ],
            ),
            (
                "drifted wake",
                vec![
                    child(0, key, GroupWakePolicy::First),
                    child(1, key, GroupWakePolicy::All),
                ],
            ),
        ];
        for (name, children) in cases {
            let error = RuntimeEffectGroup::try_new(
                invocation(RuntimeEffectKind::Sleep),
                key,
                children,
                GroupWakePolicy::First,
                LoserDisposition::RunToCompletion,
            )
            .expect_err(&format!("a group with a {name} child must not assemble"));
            assert_eq!(
                error.code,
                crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                "a {name} disagreement must be a typed group-shape refusal"
            );
        }
    }

    #[test]
    fn assembling_a_group_refuses_an_empty_group_key() {
        let error = RuntimeEffectGroup::try_new(
            invocation(RuntimeEffectKind::Sleep),
            "   ",
            vec![unstamped_child(0)],
            GroupWakePolicy::First,
            LoserDisposition::RunToCompletion,
        )
        .expect_err("a group without an identity must not assemble");
        assert_eq!(error.code, crate::RuntimeErrorCode::RuntimeEffectGroupShape);
    }

    /// Close may narrow the declared disposition but never widen it. Widening
    /// would make the losers' fate depend on whether the caller reached its close
    /// at all — a crash-drain applies the *declared* disposition — which is the
    /// divergence that declaring at open exists to remove.
    #[test]
    fn a_close_may_narrow_the_declared_loser_disposition_but_not_widen_it() {
        use LoserDisposition::{Cancel, RunToCompletion};
        assert_eq!(
            LoserDisposition::resolve_close(RunToCompletion, RunToCompletion).expect("same"),
            RunToCompletion
        );
        assert_eq!(
            LoserDisposition::resolve_close(Cancel, Cancel).expect("same"),
            Cancel
        );
        assert_eq!(
            LoserDisposition::resolve_close(RunToCompletion, Cancel)
                .expect("narrowing a declared RunToCompletion to Cancel is allowed"),
            Cancel
        );
        let error = LoserDisposition::resolve_close(Cancel, RunToCompletion)
            .expect_err("widening a declared Cancel must be refused");
        assert_eq!(error.code, crate::RuntimeErrorCode::RuntimeEffectGroupShape);
    }

    /// The declared disposition is journaled with the group *and* folded into
    /// every child's hash, so a replay under a drifted disposition is refused on
    /// engine tiers that keep no group row too.
    #[test]
    fn a_drifted_loser_disposition_changes_the_child_hash_and_fails_assembly() {
        let key = "scope:group:batch:0";
        let run = RuntimeEffectEnvelope::new(
            invocation(RuntimeEffectKind::Sleep),
            RuntimeEffectCommand::Sleep { duration_ms: 1 },
        )
        .in_effect_group(
            key,
            0,
            GroupWakePolicy::First,
            LoserDisposition::RunToCompletion,
        );
        let cancel = RuntimeEffectEnvelope::new(
            invocation(RuntimeEffectKind::Sleep),
            RuntimeEffectCommand::Sleep { duration_ms: 1 },
        )
        .in_effect_group(key, 0, GroupWakePolicy::First, LoserDisposition::Cancel);
        assert_ne!(
            run.stable_hash().expect("hashes"),
            cancel.stable_hash().expect("hashes"),
            "loser disposition must move the child hash, or a drifted replay is \
             unfenced wherever no group row exists"
        );

        let error = RuntimeEffectGroup::try_new(
            invocation(RuntimeEffectKind::Sleep),
            key,
            vec![cancel],
            GroupWakePolicy::First,
            LoserDisposition::RunToCompletion,
        )
        .expect_err("a child whose disposition disagrees must not assemble");
        assert_eq!(error.code, crate::RuntimeErrorCode::RuntimeEffectGroupShape);
    }

    /// The executor-vec alignment is held by a type rather than by each host's
    /// prose: without the check a short vec journals a group whose missing
    /// children can never settle, so every rank above the truncation is
    /// unservable and the caller waits forever.
    ///
    /// Asserted through `CheckedEffectGroup::try_new` because that is the only
    /// path — it is the type's sole constructor and `open_effect_group` takes
    /// nothing else, so an unaligned pair cannot reach a host regardless of what
    /// the host remembers to do.
    #[test]
    fn an_unaligned_executor_vec_cannot_be_paired_with_a_group() {
        let checked = CheckedEffectGroup::try_new(
            group_of(2),
            vec![
                RuntimeEffectLocalExecutor::unavailable(),
                RuntimeEffectLocalExecutor::unavailable(),
            ],
        )
        .expect("one executor per child is the aligned shape");
        let (group, executors) = checked.into_parts();
        assert_eq!(
            executors.len(),
            group.children().len(),
            "into_parts hands the host a pair it can index one-to-one"
        );

        for (name, executors) in [
            ("short", vec![RuntimeEffectLocalExecutor::unavailable()]),
            ("empty", Vec::new()),
            (
                "long",
                vec![
                    RuntimeEffectLocalExecutor::unavailable(),
                    RuntimeEffectLocalExecutor::unavailable(),
                    RuntimeEffectLocalExecutor::unavailable(),
                ],
            ),
        ] {
            let error = CheckedEffectGroup::try_new(group_of(2), executors)
                .expect_err(&format!("a {name} executor vec must be refused"));
            assert_eq!(
                error.code,
                crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                "a {name} executor vec must be a typed group-shape refusal"
            );
            assert!(
                error.message.contains("scope:group:batch:0"),
                "the refusal must name the group: {error}"
            );
        }
    }

    /// A grouped child on a path with no membership slot must be refused, never
    /// silently stripped: those paths record no canonical envelope, so the wake
    /// rule loses the only fence that makes "replay cannot silently change it"
    /// backed rather than asserted.
    #[test]
    fn an_unhonored_group_membership_is_refused_rather_than_dropped() {
        assert!(
            super::refuse_unhonored_group_membership(None, "trigger").is_ok(),
            "an ungrouped effect passes every shape unchanged"
        );
        let membership = EffectGroupMembership {
            group_key: "scope:group:batch:0".to_string(),
            position: 1,
            wake: GroupWakePolicy::First,
            loser_disposition: LoserDisposition::RunToCompletion,
        };
        let error = super::refuse_unhonored_group_membership(Some(&membership), "trigger")
            .expect_err("a grouped child on a membership-less path must be refused");
        assert_eq!(error.code, crate::RuntimeErrorCode::RuntimeEffectGroupShape);
        assert!(
            error.message.contains("trigger") && error.message.contains("scope:group:batch:0"),
            "the refusal must name the path and the group: {error}"
        );
    }
}
