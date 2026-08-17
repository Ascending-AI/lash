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
}

/// A group of independently journaled child effects opened at the effect-host
/// seam (FIG-1416).
///
/// The children are ordinary envelopes carrying ordinary
/// [`RuntimeEffectCommand`](super::envelope::RuntimeEffectCommand)s — groups
/// introduce no new command variant, because what is new is the *composition
/// above* attempts, not the attempts.
#[derive(Clone, Debug)]
pub struct RuntimeEffectGroup {
    /// The group's durable identity. Children derive their replay keys from it
    /// exactly as batch leaves already do.
    pub invocation: RuntimeInvocation,
    /// The children in source order. Each carries its own stable replay key and
    /// its own [`EffectGroupMembership`].
    pub children: Vec<RuntimeEffectEnvelope>,
    /// Recorded in the group's journal identity.
    pub wake: GroupWakePolicy,
}

/// The caller's handle on an open effect group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupHandle {
    pub group_key: String,
    pub children: usize,
    /// Settlements already consumed by this caller. Part of the durable
    /// continuation, so a restored VM frame resumes at the right rank.
    pub consumed: u32,
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
    pub sequence: u32,
    pub outcome: Result<RuntimeEffectOutcome, RuntimeEffectControllerError>,
}

impl std::fmt::Debug for GroupSettlement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupSettlement")
            .field("position", &self.position)
            .field("sequence", &self.sequence)
            .field("settled_ok", &self.outcome.is_ok())
            .finish()
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

#[cfg(test)]
mod effect_group_contract_tests {
    use super::*;
    use crate::runtime::effect::envelope::{RuntimeEffectCommand, RuntimeEffectKind, RuntimeScope};

    fn invocation(kind: RuntimeEffectKind) -> RuntimeInvocation {
        RuntimeInvocation::effect(RuntimeScope::new("session"), "effect", kind, "replay")
    }

    /// One envelope per cheaply-constructible non-group command variant.
    ///
    /// The corpus deliberately spans every command *shape* the encoding could
    /// perturb — unit-ish payloads, string payloads, keyed payloads, and the
    /// nested `ToolAttempt`/`ToolBatch` payloads — because the field being
    /// guarded sits on the envelope rather than inside any one command.
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
    /// These hashes were generated from the pre-change tree (base
    /// `eb0935293`) and are unchanged by the added field. They are golden
    /// constants rather than a self-consistency check on purpose: the defect
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
            assert!(
                !json.contains("group"),
                "an ungrouped `{name}` envelope must not encode a group field at all: {json}"
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
            .in_effect_group("scope:group:batch:0", 0, GroupWakePolicy::First)
            .stable_hash()
            .expect("hashes");
        let wake_drifted = base
            .clone()
            .in_effect_group("scope:group:batch:0", 0, GroupWakePolicy::All)
            .stable_hash()
            .expect("hashes");
        let position_drifted = base
            .clone()
            .in_effect_group("scope:group:batch:0", 1, GroupWakePolicy::First)
            .stable_hash()
            .expect("hashes");
        let group_drifted = base
            .in_effect_group("scope:group:batch:1", 0, GroupWakePolicy::First)
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
        .in_effect_group("scope:group:batch:2", 3, GroupWakePolicy::FirstSuccess);
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
        let handle = EffectGroupHandle {
            group_key: "scope:group:batch:0".to_string(),
            children: 3,
            consumed: 2,
        };
        let encoded = serde_json::to_string(&handle).expect("handle encodes");
        let decoded = serde_json::from_str::<EffectGroupHandle>(&encoded).expect("handle decodes");
        assert_eq!(decoded, handle);
    }
}
