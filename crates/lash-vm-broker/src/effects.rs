//! What the broker asks of the parent: the admission and the body behind
//! every operation a VM issues.
//!
//! The parent's effect bodies stay opaque parent code (ADR 0116): the broker
//! hands an operation, with the identities the parent derived for it, to
//! [`ParentEffects`]. The parent says what admitting it records
//! ([`ParentEffects::admission`]); the broker commits that admission with the
//! VM's snapshot, and only then asks the parent to perform the operation
//! ([`ParentEffects::perform`]). The broker never runs a tool itself, and
//! nothing performs an operation whose admission did not commit.

use lash_core_execution::runtime::actor::round::ExecutionDraft;
use lash_core_execution::runtime::actor::waits::{PinnedKey, WaitRef, WaitSpec};
use lash_vm_protocol::{EffectOutcome, EncodedPayload};
use serde::{Deserialize, Serialize};

use crate::ledger::AdmittedOperation;

/// What performing an admitted operation answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Performed {
    /// The outcome the worker is answered with.
    pub outcome: EffectOutcome,
    /// A handle the operation's outcome granted (a deferred call), which the
    /// parent scopes to the current frame.
    pub granted: Option<String>,
}

impl Performed {
    pub fn outcome(outcome: EffectOutcome) -> Self {
        Self {
            outcome,
            granted: None,
        }
    }
}

/// What admitting an operation records with the snapshot of the VM that
/// issued it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Admission {
    /// The execution its body is: its call, tool, request, policy and
    /// limit. `None` for an operation whose body only reads committed state
    /// (a wait on pinned rows), which a restore performs again.
    pub draft: Option<ExecutionDraft>,
    /// The waits it pins, in the same transaction.
    pub waits: Vec<WaitSpec>,
}

/// A fault of the parent's own store or host: nothing about the worker or
/// the guest. The run stops, and the execution resumes from its latest
/// snapshot.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the parent could not perform the operation: {0}")]
pub struct ParentFault(pub String);

/// The parent's half of the broker (see the module docs).
#[async_trait::async_trait]
pub trait ParentEffects: Send + Sync {
    /// Resolve using the parent's admitted routes. Runtime adapters preserve
    /// their existing full authority checks at the effect-body boundary.
    fn resolve(
        &self,
        context: &crate::AdmittedContext,
        grants: &std::collections::BTreeMap<String, crate::HandleGrant>,
        frame: lash_vm_protocol::FrameEpoch,
        request: &lash_vm_protocol::EffectRequest,
    ) -> Result<crate::authority::ResolvedRequest, crate::AuthorityRefusal> {
        crate::authority::resolve(context, grants, frame, request.kind, &request.payload)
    }

    /// The parent's segment decision after a completed effect.
    fn boundary(&self) -> bool {
        false
    }

    /// Answers a worker's projection read: awaited, because the provider
    /// that answers it is async (ADR 0132 §9). A read is pure and recorded
    /// nowhere.
    async fn projection(&self, _payload: &EncodedPayload) -> Result<EncodedPayload, ParentFault> {
        Err(ParentFault("no projected bindings were admitted".into()))
    }

    fn observe(&self, _payload: &EncodedPayload) -> Result<(), ParentFault> {
        Ok(())
    }

    fn park_declined(&self, _reason: &str) {}

    /// What admitting `operation` records with the snapshot of the VM that
    /// issued it. Called before anything of it is committed or performed.
    fn admission(&self, operation: &AdmittedOperation) -> Result<Admission, ParentFault>;

    /// The host's own state for the execution at this quiet point, opaque to
    /// the broker: it commits with the VM's snapshot, and a restore hands it
    /// back with the checkpoint.
    fn host_state(&self) -> Result<Option<EncodedPayload>, ParentFault> {
        Ok(None)
    }

    /// Performs `operation`, whose admission committed, over the waits its
    /// quiet point pinned: its body runs. Called once per admission, and
    /// again on restore only for a `Repeatable` body that never answered or
    /// an operation admitted as no execution.
    async fn perform(
        &self,
        operation: &AdmittedOperation,
        waits: &[(WaitRef, Option<PinnedKey>)],
    ) -> Result<Performed, ParentFault>;

    /// What the VM is answered with for `operation`, a `Once` whose body
    /// started and never answered: it was interrupted, and is never entered
    /// again (ADR 0132 §5).
    fn interrupted(&self, operation: &AdmittedOperation) -> EffectOutcome;

    /// Whether the run is cancelled at instruction checkpoint `checkpoint`
    /// (ADR 0039): read from the parent's committed state.
    async fn observe_cancellation(&self, checkpoint: u64) -> Result<bool, ParentFault>;
}

/// The execution a VM operation is admitted as: `call` of `tool` under
/// `policy` and `limit`, its request material the digest of the request the
/// VM sent, owned by `opener`'s run.
pub fn operation_draft(
    call: lash_sansio::ToolCallId,
    tool: lash_sansio::ToolId,
    request: &EncodedPayload,
    opener: &lash_core_store::effect_opener::EffectOpener,
    policy: lash_sansio::ExecutionPolicy,
    limit: lash_sansio::ExecutionLimit,
) -> Result<ExecutionDraft, ParentFault> {
    use lash_core_store::tool_run::{
        MaterialDigest, MaterialLocation, MaterialOwner, MaterialRef, MaterialRole,
    };
    let digest = MaterialDigest::parse(blake3::hash(&request.0).to_hex().as_str())
        .map_err(|error| ParentFault(error.to_string()))?;
    Ok(ExecutionDraft::new(
        call,
        tool,
        MaterialRef {
            owner: MaterialOwner::Run {
                opener: opener.clone(),
            },
            role: MaterialRole::PreparedRequest,
            location: MaterialLocation::JournalLocal,
            digest,
        },
        policy,
        limit,
        None,
    ))
}

/// Whether a VM operation only waits on committed state (an await, a sleep,
/// a signal wait): it is admitted as no execution, and a restore performs it
/// again.
pub fn waits_only(request: &crate::OperationRequest) -> bool {
    matches!(
        request,
        crate::OperationRequest::Await(_)
            | crate::OperationRequest::Sleep(_)
            | crate::OperationRequest::WaitSignal { .. }
    )
}
