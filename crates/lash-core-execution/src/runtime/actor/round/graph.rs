//! Round facts and call outcomes as checkpoint graph nodes (FIG-5887 §3,
//! §4, §10).
//!
//! - **CallOutcome** (kind 20): one immutable node per settled tool call,
//!   installed once at settlement. It holds the call's typed output, its
//!   answer as a separate FrozenValue (kind 8), its provider and intent
//!   facts, the Admission (kind 18) its request lives under when it has
//!   one, and its Presentation (kind 31): the recipe that rebuilds the
//!   model's view of the call exactly from the typed output. It never holds
//!   the call's arguments or a second rendered copy of its answer.
//! - **RoundFact** (kind 21): a run record's typed body, Admit, Start,
//!   Wait, Retry, Decide, Present, Outcome and CellReport (variants 0 to 7,
//!   [`RunFactKind::round_fact_variant`]). A fact that names material
//!   refers to one graph node, the material: a CallOutcome for a tool
//!   call's answer, or a FrozenValue for any other payload. The fact's
//!   material reference is minted over that node's identity, so the owner,
//!   role and digest checks of [`Material`] bind the record to exactly that
//!   node.
//!
//! Nested payloads this lane does not model field by field (a trace scope,
//! a tool declaration, a plugin-state resolution, a policy, an intent
//! outcome, provider replay state, an effect opener) are written as
//! canonical JSON value trees ([`encode_json`]), never as JSON text.
//!
//! [`RunFactKind::round_fact_variant`]: lash_durable::domain::graph::RunFactKind::round_fact_variant

use std::cell::Cell;

use lash_core_store::checkpoint_graph::{
    GraphDecoder, GraphRefusal, GraphStaging, NodeId, NodeKind, TokenCodec, TokenError,
    TokenReader, TokenWriter,
};
use lash_core_store::tool_run::{
    AttemptOutcome, AvailableEvidence, CompletionSource, KnownFailure, KnownFailureReason,
    LimitCause, MaterialDigest, MaterialLocation, MaterialOwner, MaterialRef, MaterialRole,
};
use lash_durable::domain::graph::RunFactKind;
use lash_sansio::tool_output::graph::{
    AnswerSlot, FrozenJson, FrozenToolValue, PresentationRecipe, PresentationRefusal, decode_json,
    decode_output, encode_json, encode_output,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::output::{Material, NamesMaterial};
use super::tools::CompletedCall;
use crate::{ToolCallId, ToolCallOutput, ToolValue};

fn unsupported(variant: u32) -> TokenError {
    TokenError::UnsupportedVariant { variant }
}

fn invalid(message: impl Into<String>) -> TokenError {
    TokenError::Invalid(message.into())
}

/// A first-party nested payload as its canonical JSON value tree.
pub fn encode_tree<T: Serialize>(value: &T, out: &mut TokenWriter) {
    encode_json(
        &serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
        out,
    );
}

/// A first-party nested payload as its canonical JSON value tree.
pub fn decode_tree<T: Serialize + DeserializeOwned>(
    input: &mut TokenReader<'_>,
) -> Result<T, TokenError> {
    let tree = decode_json(input)?;
    let value: T =
        serde_json::from_value(tree.clone()).map_err(|error| invalid(error.to_string()))?;
    if serde_json::to_value(&value).ok().as_ref() != Some(&tree) {
        return Err(invalid("a payload tree is not its value's one spelling"));
    }
    Ok(value)
}

/// A first-party nested payload as its canonical JSON value tree.
pub fn encode_opt_tree<T: Serialize>(value: Option<&T>, out: &mut TokenWriter) {
    match value {
        None => out.none(),
        Some(value) => {
            out.some();
            encode_tree(value, out);
        }
    }
}

/// A first-party nested payload as its canonical JSON value tree.
pub fn decode_opt_tree<T: Serialize + DeserializeOwned>(
    input: &mut TokenReader<'_>,
) -> Result<Option<T>, TokenError> {
    if input.option()? {
        decode_tree(input).map(Some)
    } else {
        Ok(None)
    }
}

fn decode_call_id(input: &mut TokenReader<'_>) -> Result<ToolCallId, TokenError> {
    ToolCallId::parse(input.text()?).map_err(|error| invalid(error.to_string()))
}

// --- Material references -------------------------------------------------

/// Material owners: Run 0 (its opener as a tree), Process 1, Source 2 (its
/// key as a tree).
pub fn encode_material_ref(reference: &MaterialRef, out: &mut TokenWriter) {
    out.seq(4);
    match &reference.owner {
        MaterialOwner::Run { opener } => {
            out.sum(0, 1);
            encode_tree(opener, out);
        }
        MaterialOwner::Process { process_id } => {
            out.sum(1, 1);
            encode_tree(process_id, out);
        }
        MaterialOwner::Source { source } => {
            out.sum(2, 1);
            encode_tree(source, out);
        }
    }
    out.sum(
        match reference.role {
            MaterialRole::PreparedRequest => 0,
            MaterialRole::PluginStateSnapshot => 1,
            MaterialRole::AttemptOutput => 2,
            MaterialRole::CheckContributions => 3,
            MaterialRole::Presentation => 4,
        },
        0,
    );
    match &reference.location {
        MaterialLocation::JournalLocal => out.sum(0, 0),
        MaterialLocation::RetainedArtifact { artifact } => {
            out.sum(1, 1);
            encode_tree(artifact, out);
        }
    }
    out.text(reference.digest.as_str());
}

pub fn decode_material_ref(input: &mut TokenReader<'_>) -> Result<MaterialRef, TokenError> {
    input.expect_struct(4)?;
    let (variant, fields) = input.sum()?;
    TokenReader::expect_fields(fields, 1)?;
    let owner = match variant {
        0 => MaterialOwner::Run {
            opener: decode_tree(input)?,
        },
        1 => MaterialOwner::Process {
            process_id: decode_tree(input)?,
        },
        2 => MaterialOwner::Source {
            source: decode_tree(input)?,
        },
        other => return Err(unsupported(other)),
    };
    let (variant, fields) = input.sum()?;
    TokenReader::expect_fields(fields, 0)?;
    let role = match variant {
        0 => MaterialRole::PreparedRequest,
        1 => MaterialRole::PluginStateSnapshot,
        2 => MaterialRole::AttemptOutput,
        3 => MaterialRole::CheckContributions,
        4 => MaterialRole::Presentation,
        other => return Err(unsupported(other)),
    };
    let (variant, fields) = input.sum()?;
    let location = match variant {
        0 => {
            TokenReader::expect_fields(fields, 0)?;
            MaterialLocation::JournalLocal
        }
        1 => {
            TokenReader::expect_fields(fields, 1)?;
            MaterialLocation::RetainedArtifact {
                artifact: decode_tree(input)?,
            }
        }
        other => return Err(unsupported(other)),
    };
    let digest =
        MaterialDigest::parse(input.text()?).map_err(|error| invalid(error.to_string()))?;
    Ok(MaterialRef {
        owner,
        role,
        location,
        digest,
    })
}

fn encode_evidence(evidence: &AvailableEvidence, out: &mut TokenWriter) {
    out.seq(1);
    match &evidence.retained {
        None => out.none(),
        Some(reference) => {
            out.some();
            encode_material_ref(reference, out);
        }
    }
}

fn decode_evidence(input: &mut TokenReader<'_>) -> Result<AvailableEvidence, TokenError> {
    input.expect_struct(1)?;
    Ok(AvailableEvidence {
        retained: if input.option()? {
            Some(decode_material_ref(input)?)
        } else {
            None
        },
    })
}

fn limit_cause(cause: LimitCause) -> u32 {
    match cause {
        LimitCause::ExecutionSlice => 0,
        LimitCause::ExecutionTotal => 1,
        LimitCause::WaitDeadline => 2,
    }
}

fn decode_limit_cause(input: &mut TokenReader<'_>) -> Result<LimitCause, TokenError> {
    let (variant, fields) = input.sum()?;
    TokenReader::expect_fields(fields, 0)?;
    match variant {
        0 => Ok(LimitCause::ExecutionSlice),
        1 => Ok(LimitCause::ExecutionTotal),
        2 => Ok(LimitCause::WaitDeadline),
        other => Err(unsupported(other)),
    }
}

/// Attempt outcomes: Completed 0, Waiting 1, Failed 2, Interrupted 3,
/// TimedOut 4, Cancelled 5 (`lash_core_store`'s codec table).
fn encode_attempt(outcome: &AttemptOutcome, out: &mut TokenWriter) {
    match outcome {
        AttemptOutcome::Completed(reference) => {
            out.sum(0, 1);
            encode_material_ref(reference, out);
        }
        AttemptOutcome::Waiting(source) => {
            out.sum(1, 3);
            out.text(&source.wait);
            source.terminal.encode(out);
            encode_material_ref(&source.metadata, out);
        }
        AttemptOutcome::Failed(failure) => {
            out.sum(2, 3);
            encode_material_ref(&failure.output, out);
            out.sum(
                match failure.reason {
                    KnownFailureReason::Reported => 0,
                    KnownFailureReason::DeclarationRefused => 1,
                    KnownFailureReason::StartRefused => 2,
                },
                0,
            );
            failure.suggested_delay_ms.encode(out);
        }
        AttemptOutcome::Interrupted => out.sum(3, 0),
        AttemptOutcome::TimedOut { cause, evidence } => {
            out.sum(4, 2);
            out.sum(limit_cause(*cause), 0);
            encode_evidence(evidence, out);
        }
        AttemptOutcome::Cancelled { evidence } => {
            out.sum(5, 1);
            encode_evidence(evidence, out);
        }
    }
}

fn decode_attempt(input: &mut TokenReader<'_>) -> Result<AttemptOutcome, TokenError> {
    let (variant, fields) = input.sum()?;
    let expect = |count| TokenReader::expect_fields(fields, count);
    Ok(match variant {
        0 => {
            expect(1)?;
            AttemptOutcome::Completed(decode_material_ref(input)?)
        }
        1 => {
            expect(3)?;
            AttemptOutcome::Waiting(CompletionSource {
                wait: input.text()?.to_owned(),
                terminal: Option::decode(input)?,
                metadata: decode_material_ref(input)?,
            })
        }
        2 => {
            expect(3)?;
            let output = decode_material_ref(input)?;
            let (reason, reason_fields) = input.sum()?;
            TokenReader::expect_fields(reason_fields, 0)?;
            AttemptOutcome::Failed(KnownFailure {
                output,
                reason: match reason {
                    0 => KnownFailureReason::Reported,
                    1 => KnownFailureReason::DeclarationRefused,
                    2 => KnownFailureReason::StartRefused,
                    other => return Err(unsupported(other)),
                },
                suggested_delay_ms: Option::decode(input)?,
            })
        }
        3 => {
            expect(0)?;
            AttemptOutcome::Interrupted
        }
        4 => {
            expect(2)?;
            AttemptOutcome::TimedOut {
                cause: decode_limit_cause(input)?,
                evidence: decode_evidence(input)?,
            }
        }
        5 => {
            expect(1)?;
            AttemptOutcome::Cancelled {
                evidence: decode_evidence(input)?,
            }
        }
        other => return Err(unsupported(other)),
    })
}

// --- The material a fact names -------------------------------------------

/// The one graph node a fact's material is: a call's outcome (0) or any
/// other payload's frozen value (1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaterialNode {
    Outcome(NodeId),
    Value(NodeId),
}

impl MaterialNode {
    #[must_use]
    pub fn node(&self) -> NodeId {
        match self {
            Self::Outcome(node) | Self::Value(node) => *node,
        }
    }

    #[must_use]
    pub fn kind(&self) -> NodeKind {
        match self {
            Self::Outcome(_) => NodeKind::CALL_OUTCOME,
            Self::Value(_) => NodeKind::FROZEN_VALUE,
        }
    }

    /// The payload a material reference over this node is minted from:
    /// the node's identity, so the reference's digest names exactly it.
    #[must_use]
    pub fn payload(&self) -> String {
        self.node().to_hex()
    }

    /// `owner`'s journal-local material of `role` naming this node.
    #[must_use]
    pub fn material(&self, owner: MaterialOwner, role: MaterialRole) -> Material {
        Material::journal_local(owner, role, self.payload())
    }

    /// Checks `named` names this node: its owner, and its digest minted
    /// over this node's identity.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::PublicationMismatch`] when it names another.
    pub fn check<T: NamesMaterial + Clone>(&self, named: &T) -> Result<(), GraphRefusal> {
        Material::new(named.clone(), self.payload())
            .map(|_| ())
            .map_err(|_| GraphRefusal::PublicationMismatch)
    }
}

impl TokenCodec for MaterialNode {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Outcome(node) => {
                out.sum(0, 1);
                node.encode(out);
            }
            Self::Value(node) => {
                out.sum(1, 1);
                node.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 1)?;
        match variant {
            0 => NodeId::decode(input).map(Self::Outcome),
            1 => NodeId::decode(input).map(Self::Value),
            other => Err(unsupported(other)),
        }
    }
}

/// An attempt's settled output as a fact holds it: the outcome as its
/// record names it and, for an outcome that names material, the node that
/// material is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettledFact {
    pub outcome: AttemptOutcome,
    pub material: Option<MaterialNode>,
}

impl SettledFact {
    /// Checks the material is present exactly when the outcome names one,
    /// and that the outcome's reference names that node.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::PublicationMismatch`].
    pub fn check(&self) -> Result<(), GraphRefusal> {
        let named = match &self.outcome {
            AttemptOutcome::Completed(reference) => Some(reference.clone()),
            AttemptOutcome::Failed(failure) => Some(failure.output.clone()),
            AttemptOutcome::Waiting(source) => Some(source.metadata.clone()),
            AttemptOutcome::Interrupted
            | AttemptOutcome::TimedOut { .. }
            | AttemptOutcome::Cancelled { .. } => None,
        };
        match (named, &self.material) {
            (Some(named), Some(material)) => material.check(&named),
            (None, None) => Ok(()),
            _ => Err(GraphRefusal::PublicationMismatch),
        }
    }
}

impl TokenCodec for SettledFact {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        encode_attempt(&self.outcome, out);
        self.material.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            outcome: decode_attempt(input)?,
            material: Option::decode(input)?,
        })
    }
}

// --- Call outcomes -------------------------------------------------------

/// Kind 20: one settled tool call, installed once. Its output's success
/// answer is a FrozenValue child; its presentation a Presentation child.
#[derive(Clone, Debug, PartialEq)]
pub struct CallOutcome {
    pub call_id: ToolCallId,
    /// The Admission its request lives under; `None` for a turn round's
    /// member, whose request the turn holds.
    pub admission: Option<NodeId>,
    /// The ordinal of the attempt's start.
    pub attempt: u64,
    pub tool_name: String,
    pub output: ToolCallOutput,
    pub provider_call_id: Option<String>,
    pub replay: Option<lash_sansio::llm::types::ProviderReplayMeta>,
    pub intent_outcomes: Vec<crate::ToolIntentExecutionOutcome>,
    pub presentation: PresentationRecipe,
}

/// The success answer written as a FrozenValue child reference, read back
/// as a placeholder whose node a loader then decodes.
struct RefAnswer {
    node: Cell<Option<NodeId>>,
}

impl AnswerSlot for RefAnswer {
    fn write(&self, _value: &ToolValue, out: &mut TokenWriter) {
        if let Some(node) = self.node.get() {
            node.encode(out);
        }
    }

    fn read(&self, input: &mut TokenReader<'_>) -> Result<ToolValue, TokenError> {
        self.node.set(Some(NodeId::decode(input)?));
        Ok(ToolValue::Null)
    }
}

fn encode_outcome_body(
    outcome: &CallOutcome,
    answer: &RefAnswer,
    presentation: NodeId,
    out: &mut TokenWriter,
) {
    out.seq(9);
    out.text(outcome.call_id.as_str());
    outcome.admission.encode(out);
    out.unsigned(outcome.attempt);
    out.text(&outcome.tool_name);
    encode_output(&outcome.output, answer, out);
    outcome.provider_call_id.encode(out);
    encode_opt_tree(outcome.replay.as_ref(), out);
    encode_tree(&outcome.intent_outcomes, out);
    presentation.encode(out);
}

/// The outcome body as stored: its references not yet followed.
struct StoredOutcome {
    outcome: CallOutcome,
    answer: Option<NodeId>,
    presentation: NodeId,
}

impl TokenCodec for StoredOutcome {
    fn encode(&self, out: &mut TokenWriter) {
        let answer = RefAnswer {
            node: Cell::new(self.answer),
        };
        encode_outcome_body(&self.outcome, &answer, self.presentation, out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(9)?;
        let call_id = decode_call_id(input)?;
        let admission = Option::decode(input)?;
        let attempt = input.unsigned()?;
        let tool_name = input.text()?.to_owned();
        let answer = RefAnswer {
            node: Cell::new(None),
        };
        let output = decode_output(input, &answer)?;
        let provider_call_id = Option::decode(input)?;
        let replay = decode_opt_tree(input)?;
        let intent_outcomes = decode_tree(input)?;
        let presentation = NodeId::decode(input)?;
        Ok(Self {
            outcome: CallOutcome {
                call_id,
                admission,
                attempt,
                tool_name,
                output,
                provider_call_id,
                replay,
                intent_outcomes,
                // Read from its own node by the loader.
                presentation: PresentationRecipe {
                    renderer: lash_sansio::tool_output::graph::RendererId([0; 32]),
                    tool_name: String::new(),
                    parts: Vec::new(),
                    notices: Vec::new(),
                    digest: [0; 32],
                    bytes: 0,
                },
            },
            answer: answer.node.get(),
            presentation,
        })
    }
}

impl CallOutcome {
    /// The outcome of `completed`, attempt `attempt`, its request under
    /// `admission` when it has one.
    #[must_use]
    pub fn of(completed: &CompletedCall, admission: Option<NodeId>, attempt: u64) -> Self {
        Self {
            call_id: completed.call_id.clone(),
            admission,
            attempt,
            tool_name: completed.tool_name.clone(),
            output: completed.output.clone(),
            provider_call_id: completed.provider_call_id.clone(),
            replay: completed.replay.clone(),
            intent_outcomes: completed.intent_outcomes.clone(),
            presentation: PresentationRecipe::of(&completed.model_return, &completed.output),
        }
    }

    /// Stages this outcome, its answer and its presentation, and returns
    /// the outcome's node.
    ///
    /// # Errors
    ///
    /// Any refusal of the staging area.
    pub fn stage(&self, staging: &mut GraphStaging) -> Result<NodeId, GraphRefusal> {
        let answer = match &self.output.outcome {
            crate::ToolCallOutcome::Success(value) => Some(
                staging.encode_value(NodeKind::FROZEN_VALUE, &FrozenToolValue(value.clone()))?,
            ),
            crate::ToolCallOutcome::Failure(_) | crate::ToolCallOutcome::Cancelled(_) => None,
        };
        let presentation = staging.encode_value(NodeKind::PRESENTATION, &self.presentation)?;
        staging.encode_value(
            NodeKind::CALL_OUTCOME,
            &StoredOutcome {
                outcome: self.clone(),
                answer,
                presentation,
            },
        )
    }

    /// Reads outcome `id` back whole: its answer from its FrozenValue, its
    /// presentation recipe from its Presentation, each child of its stated
    /// kind.
    ///
    /// # Errors
    ///
    /// Any refusal of a node read, a child of another kind, or
    /// [`GraphRefusal::InvalidBody`] for a success without its answer.
    pub fn load(decoder: &mut GraphDecoder<'_>, id: NodeId) -> Result<Self, GraphRefusal> {
        let stored: StoredOutcome = decoder.decode(id, NodeKind::CALL_OUTCOME)?;
        let mut outcome = stored.outcome;
        if let Some(admission) = outcome.admission {
            decoder.child_kind(id, admission, &[NodeKind::ADMISSION])?;
        }
        match (&mut outcome.output.outcome, stored.answer) {
            (crate::ToolCallOutcome::Success(value), Some(answer)) => {
                let FrozenToolValue(frozen) = decoder.decode(answer, NodeKind::FROZEN_VALUE)?;
                *value = frozen;
            }
            (crate::ToolCallOutcome::Success(_), None) => {
                return Err(GraphRefusal::InvalidBody {
                    kind: NodeKind::CALL_OUTCOME.0,
                    reason: "a successful outcome without its answer".to_owned(),
                });
            }
            _ => {}
        }
        outcome.presentation = decoder.decode(stored.presentation, NodeKind::PRESENTATION)?;
        Ok(outcome)
    }

    /// The model's view of the call, rebuilt exactly from the typed output.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::RendererUnavailable`] or
    /// [`GraphRefusal::PresentationMismatch`]: never a re-rendering.
    pub fn presented(&self) -> Result<crate::ModelToolReturn, GraphRefusal> {
        self.presentation
            .rebuild(&self.output)
            .map_err(|refusal| match refusal {
                PresentationRefusal::RendererUnavailable => GraphRefusal::RendererUnavailable,
                PresentationRefusal::Mismatch => GraphRefusal::PresentationMismatch,
            })
    }

    /// The completed call this outcome answers, with `args`, its request's
    /// arguments: a transient materialized record.
    ///
    /// # Errors
    ///
    /// As [`presented`](Self::presented).
    pub fn completed(&self, args: serde_json::Value) -> Result<CompletedCall, GraphRefusal> {
        Ok(CompletedCall {
            call_id: self.call_id.clone(),
            provider_call_id: self.provider_call_id.clone(),
            tool_name: self.tool_name.clone(),
            args,
            output: self.output.clone(),
            model_return: self.presented()?,
            intent_outcomes: self.intent_outcomes.clone(),
            replay: self.replay.clone(),
        })
    }
}

/// Stages `value` as one FrozenValue holding a JSON value tree: the node a
/// fact's material is when it is not a call's outcome.
///
/// # Errors
///
/// Any refusal of the staging area.
pub fn stage_value(
    staging: &mut GraphStaging,
    value: &serde_json::Value,
) -> Result<MaterialNode, GraphRefusal> {
    staging
        .encode_value(NodeKind::FROZEN_VALUE, &FrozenJson(value.clone()))
        .map(MaterialNode::Value)
}

/// Reads a FrozenValue JSON tree [`stage_value`] staged.
///
/// # Errors
///
/// Any refusal of the node read.
pub fn load_value(
    decoder: &mut GraphDecoder<'_>,
    node: NodeId,
) -> Result<serde_json::Value, GraphRefusal> {
    decoder
        .decode::<FrozenJson>(node, NodeKind::FROZEN_VALUE)
        .map(|FrozenJson(value)| value)
}

// --- Round facts ---------------------------------------------------------

/// One member as a round's admission pinned it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmitMember {
    /// Variant 0: a turn round's member, its pin written out.
    Pinned(Box<PinnedMember>),
    /// Variant 1: an effect admitted under its own Admission node, which
    /// holds its request and pin.
    Admission(NodeId),
}

/// A member's pin, as its admission recorded it: never refreshed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedMember {
    pub call: ToolCallId,
    pub tool: String,
    pub request: MaterialRef,
    pub policy: lash_sansio::ExecutionPolicy,
    pub limit_expires_at_ms: u64,
    pub limit_max_slice_ms: u64,
    /// When its park, if it may defer, times out: an instant (0), or the end
    /// of its scope (1).
    pub park: Option<Option<i64>>,
    /// The completion wait pinned with the admission.
    pub wait_id: Option<String>,
    pub trace: Option<lash_trace::DurableTraceScope>,
    pub declaration: Option<crate::ToolDeclaration>,
}

impl PinnedMember {
    /// The pin `draft` was admitted with.
    #[must_use]
    pub fn of(draft: &super::ExecutionDraft) -> Self {
        Self {
            call: draft.call().clone(),
            tool: draft.tool().as_str().to_owned(),
            request: draft.request().clone(),
            policy: draft.policy(),
            limit_expires_at_ms: draft.limit().expires_at,
            limit_max_slice_ms: u64::try_from(draft.limit().max_slice.as_millis())
                .unwrap_or(u64::MAX),
            park: draft
                .park()
                .map(|park| park.deadline().map(|deadline| deadline.at().0)),
            wait_id: draft.pinned_wait().map(|pinned| pinned.id.to_hex()),
            trace: draft.trace().cloned(),
            declaration: draft.declaration().cloned(),
        }
    }
}

impl TokenCodec for PinnedMember {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(10);
        out.text(self.call.as_str());
        out.text(&self.tool);
        encode_material_ref(&self.request, out);
        encode_tree(&self.policy, out);
        out.unsigned(self.limit_expires_at_ms);
        out.unsigned(self.limit_max_slice_ms);
        match self.park {
            None => out.none(),
            Some(deadline) => {
                out.some();
                match deadline {
                    Some(at) => {
                        out.sum(0, 1);
                        out.signed(at);
                    }
                    None => out.sum(1, 0),
                }
            }
        }
        self.wait_id.encode(out);
        encode_opt_tree(self.trace.as_ref(), out);
        encode_opt_tree(self.declaration.as_ref(), out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(10)?;
        Ok(Self {
            call: decode_call_id(input)?,
            tool: input.text()?.to_owned(),
            request: decode_material_ref(input)?,
            policy: decode_tree(input)?,
            limit_expires_at_ms: input.unsigned()?,
            limit_max_slice_ms: input.unsigned()?,
            park: if input.option()? {
                let (variant, fields) = input.sum()?;
                Some(match variant {
                    0 => {
                        TokenReader::expect_fields(fields, 1)?;
                        Some(input.signed()?)
                    }
                    1 => {
                        TokenReader::expect_fields(fields, 0)?;
                        None
                    }
                    other => return Err(unsupported(other)),
                })
            } else {
                None
            },
            wait_id: Option::decode(input)?,
            trace: decode_opt_tree(input)?,
            declaration: decode_opt_tree(input)?,
        })
    }
}

impl TokenCodec for AdmitMember {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Pinned(member) => {
                out.sum(0, 1);
                member.encode(out);
            }
            Self::Admission(node) => {
                out.sum(1, 1);
                node.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 1)?;
        match variant {
            0 => PinnedMember::decode(input).map(|member| Self::Pinned(Box::new(member))),
            1 => NodeId::decode(input).map(Self::Admission),
            other => Err(unsupported(other)),
        }
    }
}

/// A coordinator decision: TraceExported 0 (the round codec table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    TraceExported,
}

/// Kind 21: one run record's typed body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoundFact {
    /// 0: the round's members, in declared order.
    Admit { members: Vec<AdmitMember> },
    /// 1: member `member`'s attempt `attempt` started.
    Start { member: u64, attempt: u32 },
    /// 2: the attempt started at `start` parked.
    Wait { start: u64, output: SettledFact },
    /// 3: the attempt started at `start` failed repeatably; the next is due
    /// at `due_at_ms`.
    Retry {
        start: u64,
        output: SettledFact,
        due_at_ms: i64,
    },
    /// 4: a coordinator decision.
    Decide(Decision),
    /// 5: the round's presentation, in admitted order.
    Present,
    /// 6: the attempt started at `start` settled the call, with the plugin
    /// state its completion resolved (as a tree).
    Outcome {
        start: u64,
        output: SettledFact,
        state: Vec<lash_core_store::tool_run::StateResolution>,
    },
    /// 7: a cell's report: a reference to its bounded ReportView over the
    /// original outcomes, never a re-admission or a re-settlement.
    CellReport { report: NodeId },
}

impl RoundFact {
    /// The stored kind of a fact of this variant.
    #[must_use]
    pub fn kind(&self) -> RunFactKind {
        match self {
            Self::Admit { .. } => RunFactKind::Admit,
            Self::Start { .. } => RunFactKind::XStart,
            Self::Wait { .. } => RunFactKind::XWait,
            Self::Retry { .. } => RunFactKind::Retry,
            Self::Decide(_) => RunFactKind::Decide,
            Self::Present => RunFactKind::Present,
            Self::Outcome { .. } => RunFactKind::XOutcome,
            Self::CellReport { .. } => RunFactKind::CellReport,
        }
    }

    /// Stages this fact and returns its node, after checking every settled
    /// output it holds names its material node.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::PublicationMismatch`], or any refusal of the staging
    /// area.
    pub fn stage(&self, staging: &mut GraphStaging) -> Result<NodeId, GraphRefusal> {
        self.check()?;
        staging.encode_value(NodeKind::ROUND_FACT, self)
    }

    /// Reads fact `id`, checking every reference it holds names a node of
    /// its stated kind and every settled output names its material.
    ///
    /// # Errors
    ///
    /// Any refusal of a node read, [`GraphRefusal::WrongChildKind`], or
    /// [`GraphRefusal::PublicationMismatch`].
    pub fn load(decoder: &mut GraphDecoder<'_>, id: NodeId) -> Result<Self, GraphRefusal> {
        let fact: Self = decoder.decode(id, NodeKind::ROUND_FACT)?;
        fact.check()?;
        match &fact {
            Self::Admit { members } => {
                for member in members {
                    if let AdmitMember::Admission(node) = member {
                        decoder.child_kind(id, *node, &[NodeKind::ADMISSION])?;
                    }
                }
            }
            Self::Wait { output, .. }
            | Self::Retry { output, .. }
            | Self::Outcome { output, .. } => {
                if let Some(material) = &output.material {
                    decoder.child_kind(id, material.node(), &[material.kind()])?;
                }
            }
            Self::CellReport { report } => {
                decoder.child_kind(id, *report, &[NodeKind::REPORT_VIEW])?;
            }
            Self::Start { .. } | Self::Decide(_) | Self::Present => {}
        }
        Ok(fact)
    }

    fn check(&self) -> Result<(), GraphRefusal> {
        match self {
            Self::Wait { output, .. }
            | Self::Retry { output, .. }
            | Self::Outcome { output, .. } => output.check(),
            _ => Ok(()),
        }
    }
}

impl TokenCodec for RoundFact {
    fn encode(&self, out: &mut TokenWriter) {
        let variant = self.kind().round_fact_variant();
        match self {
            Self::Admit { members } => {
                out.sum(variant, 1);
                members.encode(out);
            }
            Self::Start { member, attempt } => {
                out.sum(variant, 2);
                out.unsigned(*member);
                out.unsigned(u64::from(*attempt));
            }
            Self::Wait { start, output } => {
                out.sum(variant, 2);
                out.unsigned(*start);
                output.encode(out);
            }
            Self::Retry {
                start,
                output,
                due_at_ms,
            } => {
                out.sum(variant, 3);
                out.unsigned(*start);
                output.encode(out);
                out.signed(*due_at_ms);
            }
            Self::Decide(Decision::TraceExported) => {
                out.sum(variant, 1);
                out.sum(0, 0);
            }
            Self::Present => out.sum(variant, 0),
            Self::Outcome {
                start,
                output,
                state,
            } => {
                out.sum(variant, 3);
                out.unsigned(*start);
                output.encode(out);
                encode_tree(state, out);
            }
            Self::CellReport { report } => {
                out.sum(variant, 1);
                report.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        let kind = RunFactKind::ALL
            .into_iter()
            .find(|kind| kind.round_fact_variant() == variant)
            .ok_or_else(|| unsupported(variant))?;
        let expect = |count| TokenReader::expect_fields(fields, count);
        Ok(match kind {
            RunFactKind::Admit => {
                expect(1)?;
                Self::Admit {
                    members: Vec::decode(input)?,
                }
            }
            RunFactKind::XStart => {
                expect(2)?;
                Self::Start {
                    member: input.unsigned()?,
                    attempt: input.unsigned_u32()?,
                }
            }
            RunFactKind::XWait => {
                expect(2)?;
                Self::Wait {
                    start: input.unsigned()?,
                    output: SettledFact::decode(input)?,
                }
            }
            RunFactKind::Retry => {
                expect(3)?;
                Self::Retry {
                    start: input.unsigned()?,
                    output: SettledFact::decode(input)?,
                    due_at_ms: input.signed()?,
                }
            }
            RunFactKind::Decide => {
                expect(1)?;
                let (decision, decision_fields) = input.sum()?;
                TokenReader::expect_fields(decision_fields, 0)?;
                match decision {
                    0 => Self::Decide(Decision::TraceExported),
                    other => return Err(unsupported(other)),
                }
            }
            RunFactKind::Present => {
                expect(0)?;
                Self::Present
            }
            RunFactKind::XOutcome => {
                expect(3)?;
                Self::Outcome {
                    start: input.unsigned()?,
                    output: SettledFact::decode(input)?,
                    state: decode_tree(input)?,
                }
            }
            RunFactKind::CellReport => {
                expect(1)?;
                Self::CellReport {
                    report: NodeId::decode(input)?,
                }
            }
        })
    }
}

// --- Report views --------------------------------------------------------

/// Kind 32: the bounded view of a cell's calls a turn reads, in admission
/// order: each reported call's outcome and its presentation, and how many
/// calls the bound omitted. It references the original outcomes; it never
/// copies one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportView {
    pub entries: Vec<ReportEntry>,
    /// Calls the cell settled that the bound left out.
    pub omitted: u64,
}

/// One reported call: its CallOutcome and its Presentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReportEntry {
    pub outcome: NodeId,
    pub presentation: NodeId,
}

impl TokenCodec for ReportView {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.seq(self.entries.len() as u64);
        for entry in &self.entries {
            out.seq(2);
            entry.outcome.encode(out);
            entry.presentation.encode(out);
        }
        out.unsigned(self.omitted);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let count = input.seq()?;
        let mut entries = Vec::new();
        for _ in 0..count {
            input.expect_struct(2)?;
            entries.push(ReportEntry {
                outcome: NodeId::decode(input)?,
                presentation: NodeId::decode(input)?,
            });
        }
        Ok(Self {
            entries,
            omitted: input.unsigned()?,
        })
    }
}

impl ReportView {
    /// Reads view `id` and every reported call's outcome, each child of its
    /// stated kind and each presentation the one its outcome holds.
    ///
    /// # Errors
    ///
    /// Any refusal of a node read, [`GraphRefusal::WrongChildKind`], or
    /// [`GraphRefusal::PresentationMismatch`] for a presentation its
    /// outcome does not hold.
    pub fn load(
        decoder: &mut GraphDecoder<'_>,
        id: NodeId,
    ) -> Result<(Self, Vec<CallOutcome>), GraphRefusal> {
        let view: Self = decoder.decode(id, NodeKind::REPORT_VIEW)?;
        let mut outcomes = Vec::with_capacity(view.entries.len());
        for entry in &view.entries {
            decoder.child_kind(id, entry.presentation, &[NodeKind::PRESENTATION])?;
            let outcome = CallOutcome::load(decoder, entry.outcome)?;
            let recipe: PresentationRecipe =
                decoder.decode(entry.presentation, NodeKind::PRESENTATION)?;
            if recipe != outcome.presentation {
                return Err(GraphRefusal::PresentationMismatch);
            }
            outcomes.push(outcome);
        }
        Ok((view, outcomes))
    }
}

#[cfg(test)]
#[path = "graph_tests.rs"]
mod tests;
