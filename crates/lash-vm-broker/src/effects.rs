//! What an admitted effect is to the durable engine: its execution draft
//! and the request its body is built from.
//!
//! The parent's effect bodies stay opaque parent code (ADR 0116). The broker
//! never runs a tool itself, and nothing runs a body whose admission did not
//! commit.

use lash_core_execution::runtime::actor::round::ExecutionDraft;
use lash_vm_protocol::EncodedPayload;

/// One admitted effect's execution: its draft, and the request its body is
/// built from again on any owner, which the ledger keeps for as long as the
/// execution is open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberDraft {
    /// The execution: its call, tool, request digest, policy, limit and
    /// completion wait.
    pub draft: ExecutionDraft,
    /// The request, as the host encodes it.
    pub request: EncodedPayload,
}

impl MemberDraft {
    /// The member `call` over `request`, owned by `opener`'s run, admitted
    /// as `pin` declares it: its tool, policy, limit and completion wait.
    ///
    /// # Errors
    ///
    /// [`ParentFault`] when the digest is refused.
    pub fn pinned(
        call: lash_sansio::ToolCallId,
        request: EncodedPayload,
        opener: &lash_core_store::effect_opener::EffectOpener,
        pin: lash_core_execution::runtime::actor::round::MemberPin,
    ) -> Result<Self, ParentFault> {
        Self::new(
            call,
            pin.tool,
            request,
            opener,
            (pin.policy, pin.limit),
            pin.park,
        )
    }

    /// The member `call` of `tool` over `request`, owned by `opener`'s run,
    /// under `policy` and `limit`, and parking as `park` pins when it may
    /// defer. Its request material is the digest of `request`.
    ///
    /// # Errors
    ///
    /// [`ParentFault`] when the digest is refused.
    pub fn new(
        call: lash_sansio::ToolCallId,
        tool: lash_sansio::ToolId,
        request: EncodedPayload,
        opener: &lash_core_store::effect_opener::EffectOpener,
        pin: (lash_sansio::ExecutionPolicy, lash_sansio::ExecutionLimit),
        park: Option<lash_core_execution::runtime::actor::waits::ParkDeadline>,
    ) -> Result<Self, ParentFault> {
        use lash_core_store::tool_run::{
            MaterialDigest, MaterialLocation, MaterialOwner, MaterialRef, MaterialRole,
        };
        let digest = MaterialDigest::parse(blake3::hash(&request.0).to_hex().as_str())
            .map_err(|error| ParentFault(error.to_string()))?;
        let (policy, limit) = pin;
        Ok(Self {
            draft: ExecutionDraft::new(
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
                park,
            ),
            request,
        })
    }
}

/// A fault of the parent's own store or host: nothing about the worker or
/// the guest. The run stops, and resumes from its latest saved state.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the parent could not perform the operation: {0}")]
pub struct ParentFault(pub String);
