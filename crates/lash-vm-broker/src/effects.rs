//! What an admitted effect is to the durable engine: its execution draft
//! and the frozen request its body is built from.
//!
//! The parent's effect bodies stay opaque parent code (ADR 0116). The broker
//! never runs a tool itself, and nothing runs a body whose admission did not
//! commit.

use lash_core_execution::runtime::actor::round::ExecutionDraft;
use lash_core_store::checkpoint_graph::{EncodeBudget, ExplicitLimit, GraphStaging};

use crate::kernel::graph::{FrozenRequest, request_material};

/// One admitted effect's execution: its draft, and the request its body is
/// built from again on any owner, frozen once and stored as its admission's.
#[derive(Clone, Debug, PartialEq)]
pub struct MemberDraft {
    /// The execution: its call, tool, request material, policy, limit and
    /// completion wait.
    pub draft: ExecutionDraft,
    /// The request, frozen from the guest's own arguments.
    pub request: FrozenRequest,
}

impl MemberDraft {
    /// The member `call` over `request`, owned by `opener`'s run, admitted
    /// as `pin` declares it: its tool, policy, limit and completion wait.
    ///
    /// # Errors
    ///
    /// [`ParentFault`] when the request does not stage.
    pub fn pinned(
        call: lash_protocol_types::ToolCallId,
        request: FrozenRequest,
        opener: &lash_core_store::effect_opener::EffectOpener,
        pin: lash_core_execution::runtime::actor::round::MemberPin,
    ) -> Result<Self, ParentFault> {
        let mut member = Self::new(
            call,
            pin.tool,
            request,
            opener,
            (pin.policy, pin.limit),
            pin.park,
        )?;
        member.draft = member.draft.with_binding(pin.binding);
        Ok(member)
    }

    /// The member `call` of `tool` over `request`, owned by `opener`'s run,
    /// under `policy` and `limit`, and parking as `park` pins when it may
    /// defer. Its request material names the request's node, which its
    /// admission stages.
    ///
    /// # Errors
    ///
    /// [`ParentFault`] when the request does not stage.
    pub fn new(
        call: lash_protocol_types::ToolCallId,
        tool: lash_protocol_types::ToolId,
        request: FrozenRequest,
        opener: &lash_core_store::effect_opener::EffectOpener,
        pin: (
            lash_protocol_types::ExecutionPolicy,
            lash_protocol_types::ExecutionLimit,
        ),
        park: Option<lash_core_execution::runtime::actor::waits::ParkDeadline>,
    ) -> Result<Self, ParentFault> {
        let mut scratch = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
        let node = request
            .stage(&mut scratch)
            .map_err(|refusal| ParentFault(refusal.to_string()))?;
        let (policy, limit) = pin;
        Ok(Self {
            draft: ExecutionDraft::new(
                call,
                tool,
                request_material(opener, node),
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
