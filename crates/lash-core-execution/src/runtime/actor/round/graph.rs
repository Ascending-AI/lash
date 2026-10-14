//! Round facts and call outcomes as checkpoint graph nodes (FIG-5887 §3,
//! §4, §10).
//!
//! - **CallBody** (kind 40): one archival typed request/output component,
//!   shared by the live call's outcome and its consuming turn. Success values
//!   are shared FrozenValue children; it retains no execution pins or model
//!   presentation. Arguments are the exact transport projection recorded at
//!   settlement, checked against the guest request on snapshot restoration.
//! - **CallOutcome** (kind 20): the live outcome's archival body reference,
//!   its admission, attempt, replay/intent data, and presentation recipe.
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
    DecodeLimits, GraphDecoder, GraphRefusal, GraphStaging, NodeId, NodeKind, TokenCodec,
    TokenError, TokenReader, TokenWriter, TypedBody,
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

#[path = "call_body.rs"]
mod call_body;
pub use call_body::CallBody;

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
        serde::Deserialize::deserialize(&tree).map_err(|error| invalid(error.to_string()))?;
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
    CallPayload(NodeId),
}

impl MaterialNode {
    #[must_use]
    pub fn node(&self) -> NodeId {
        match self {
            Self::Outcome(node) | Self::Value(node) | Self::CallPayload(node) => *node,
        }
    }

    #[must_use]
    pub fn kind(&self) -> NodeKind {
        match self {
            Self::Outcome(_) => NodeKind::CALL_OUTCOME,
            Self::Value(_) => NodeKind::FROZEN_VALUE,
            Self::CallPayload(_) => NodeKind::CALL_OUTCOME,
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
            Self::CallPayload(node) => {
                out.sum(2, 1);
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
            2 => NodeId::decode(input).map(Self::CallPayload),
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
    /// and that it is the node the outcome's reference names: minted over
    /// the node's identity, or, for a value holding a settled output's
    /// payload text, over that text, read through `decoder`.
    ///
    /// # Errors
    ///
    /// [`GraphRefusal::PublicationMismatch`].
    pub fn check(&self, decoder: &mut GraphDecoder<'_>) -> Result<(), GraphRefusal> {
        let named = match &self.outcome {
            AttemptOutcome::Completed(reference) => Some(reference.clone()),
            AttemptOutcome::Failed(failure) => Some(failure.output.clone()),
            AttemptOutcome::Waiting(source) => Some(source.metadata.clone()),
            AttemptOutcome::Interrupted
            | AttemptOutcome::TimedOut { .. }
            | AttemptOutcome::Cancelled { .. } => None,
        };
        match (named, &self.material) {
            (Some(named), Some(material @ MaterialNode::Outcome(_))) => material.check(&named),
            (
                Some(named),
                Some(material @ (MaterialNode::Value(_) | MaterialNode::CallPayload(_))),
            ) => material.check(&named).or_else(|_| {
                let payload = load_payload(decoder, *material)
                    .map_err(|_| GraphRefusal::PublicationMismatch)?;
                Material::new(named, payload)
                    .map(|_| ())
                    .map_err(|_| GraphRefusal::PublicationMismatch)
            }),
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

/// Kind 20: one settled tool call's live metadata and shared archival body.
#[derive(Clone, Debug, PartialEq)]
pub struct CallOutcome {
    pub call_id: ToolCallId,
    /// The Admission its request lives under; `None` for a turn round's
    /// member, whose request the turn holds.
    pub admission: Option<NodeId>,
    /// The archival request projection, materialized only in memory.
    pub args: serde_json::Value,
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

struct StoredOutcome {
    body: NodeId,
    admission: Option<NodeId>,
    attempt: u64,
    replay: Option<lash_sansio::llm::types::ProviderReplayMeta>,
    intent_outcomes: Vec<crate::ToolIntentExecutionOutcome>,
    presentation: NodeId,
}

fn encode_live_outcome(
    body: NodeId,
    admission: Option<NodeId>,
    attempt: u64,
    replay: Option<&lash_sansio::llm::types::ProviderReplayMeta>,
    intents: &[crate::ToolIntentExecutionOutcome],
    presentation: NodeId,
    out: &mut TokenWriter,
) {
    out.seq(6);
    body.encode(out);
    admission.encode(out);
    out.unsigned(attempt);
    encode_opt_tree(replay, out);
    encode_tree(&intents, out);
    presentation.encode(out);
}

impl TokenCodec for StoredOutcome {
    fn encode(&self, out: &mut TokenWriter) {
        encode_live_outcome(
            self.body,
            self.admission,
            self.attempt,
            self.replay.as_ref(),
            &self.intent_outcomes,
            self.presentation,
            out,
        );
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(6)?;
        Ok(Self {
            body: NodeId::decode(input)?,
            admission: Option::decode(input)?,
            attempt: input.unsigned()?,
            replay: decode_opt_tree(input)?,
            intent_outcomes: decode_tree(input)?,
            presentation: NodeId::decode(input)?,
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
            args: completed.args.clone(),
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
        let body = CallBody::stage_fields(
            staging,
            &self.call_id,
            &self.provider_call_id,
            &self.tool_name,
            &self.args,
            &self.output,
        )?;
        let presentation = staging.encode_value(NodeKind::PRESENTATION, &self.presentation)?;
        let mut encoded = TokenWriter::new();
        encode_live_outcome(
            body,
            self.admission,
            self.attempt,
            self.replay.as_ref(),
            &self.intent_outcomes,
            presentation,
            &mut encoded,
        );
        staging.encode(
            NodeKind::CALL_OUTCOME,
            TypedBody::logical(&encoded.into_bytes()),
        )
    }

    /// Load the live outcome and its shared archival body.
    ///
    /// # Errors
    /// Any node decoding or child-kind refusal.
    pub fn load(decoder: &mut GraphDecoder<'_>, id: NodeId) -> Result<Self, GraphRefusal> {
        let stored: StoredOutcome = decoder.decode(id, NodeKind::CALL_OUTCOME)?;
        if let Some(admission) = stored.admission {
            decoder.child_kind(id, admission, &[NodeKind::ADMISSION])?;
        }
        let record = CallBody::load(decoder, stored.body)?.record;
        Ok(Self {
            call_id: record.call_id,
            admission: stored.admission,
            args: record.args,
            attempt: stored.attempt,
            tool_name: record.tool,
            output: record.output,
            provider_call_id: record.provider_call_id,
            replay: stored.replay,
            intent_outcomes: stored.intent_outcomes,
            presentation: decoder.decode(stored.presentation, NodeKind::PRESENTATION)?,
        })
    }

    /// The existing archival component this outcome names: never re-encoded
    /// by a consuming turn.
    ///
    /// # Errors
    /// Any node decoding or child-kind refusal.
    pub fn body_node(decoder: &mut GraphDecoder<'_>, id: NodeId) -> Result<NodeId, GraphRefusal> {
        let stored: StoredOutcome = decoder.decode(id, NodeKind::CALL_OUTCOME)?;
        decoder.child_kind(id, stored.body, &[NodeKind::CALL_BODY])?;
        Ok(stored.body)
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
    let mut body = TokenWriter::new();
    body.sum(lash_sansio::tool_output::graph::FROZEN_JSON, 1);
    encode_json(value, &mut body);
    staging
        .encode(
            NodeKind::FROZEN_VALUE,
            TypedBody::logical(&body.into_bytes()),
        )
        .map(MaterialNode::Value)
}

/// Stages a material's payload as one FrozenValue holding its exact text,
/// placed in shared leaves when large: what a fact's settled output names.
///
/// # Errors
///
/// Any refusal of the staging area.
pub fn stage_payload(
    staging: &mut GraphStaging,
    payload: &str,
) -> Result<MaterialNode, GraphRefusal> {
    if let Some(completed) = super::decode_completed(payload) {
        // Admit this encoding only when decoding and re-serializing preserves
        // every byte the material digest names; other payloads remain values.
        if serde_json::to_string(&completed).ok().as_deref() == Some(payload) {
            return CallOutcome::of(&completed, None, 0)
                .stage(staging)
                .map(MaterialNode::CallPayload);
        }
    }
    stage_value(staging, &serde_json::Value::String(payload.to_owned()))
}

/// Reads back the payload [`stage_payload`] staged.
///
/// # Errors
///
/// Any refusal of the node read, or [`GraphRefusal::InvalidBody`] for a
/// value that is not a payload's text.
pub fn load_payload(
    decoder: &mut GraphDecoder<'_>,
    material: MaterialNode,
) -> Result<String, GraphRefusal> {
    if let MaterialNode::CallPayload(node) = material {
        let call = CallOutcome::load(decoder, node)?;
        return serde_json::to_string(&call.completed(call.args.clone())?).map_err(|error| {
            GraphRefusal::InvalidBody {
                kind: NodeKind::CALL_OUTCOME.0,
                reason: error.to_string(),
            }
        });
    }
    let MaterialNode::Value(node) = material else {
        return Err(GraphRefusal::InvalidBody {
            kind: NodeKind::ROUND_FACT.0,
            reason: "a settled output's payload is a frozen value".to_owned(),
        });
    };
    match load_value(decoder, node)? {
        serde_json::Value::String(payload) => Ok(payload),
        _ => Err(GraphRefusal::InvalidBody {
            kind: NodeKind::FROZEN_VALUE.0,
            reason: "a settled output's payload is text".to_owned(),
        }),
    }
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
    /// The completion wait pinned with the admission, by its id's hex: a
    /// member that may park names it and nothing of that wait's, so the
    /// park's deadline is the wait row's alone.
    pub wait_id: Option<String>,
    pub trace: Option<lash_trace::DurableTraceScope>,
    pub declaration: Option<crate::ToolDeclaration>,
    /// The callbacks the admission bound ([`ExecutionDraft::binding`]).
    ///
    /// [`ExecutionDraft::binding`]: super::ExecutionDraft::binding
    pub binding: Option<crate::tool_run::AdmittedBinding>,
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
            wait_id: draft.pinned_wait().map(|pinned| pinned.id.to_hex()),
            trace: draft.trace().cloned(),
            declaration: draft.declaration().cloned(),
            binding: draft.binding().cloned(),
        }
    }
}

impl PinnedMember {
    /// The completion wait the admission pinned, when it names one that
    /// decodes: what a fold reads the member's park from.
    #[must_use]
    pub fn pinned_wait(&self) -> Option<crate::runtime::actor::waits::WaitId> {
        self.wait_id
            .as_deref()
            .and_then(crate::runtime::actor::waits::WaitId::parse_hex)
    }

    /// The draft `owner`'s admission pinned, exactly: never refreshed. A
    /// member with a pinned wait parks as that wait's row in `waits` says:
    /// until its deadline, or until its scope ends when it has none.
    ///
    /// # Errors
    ///
    /// A pinned wait whose id does not decode, or one `waits` does not bind
    /// to this owner, call and tool.
    pub fn draft(
        &self,
        owner: &lash_durable::domain::OwnerKey,
        waits: &super::PinnedWaits,
    ) -> Result<super::ExecutionDraft, &'static str> {
        let tool = crate::ToolId::new(self.tool.clone());
        let pinned = self
            .wait_id
            .as_deref()
            .map(|id| {
                crate::runtime::actor::waits::WaitId::parse_hex(id)
                    .map(|id| super::PinnedWait { id })
                    .ok_or("a pinned wait id is not a wait id")
            })
            .transpose()?;
        let park = pinned
            .map(|pinned| waits.park(owner, &self.call, &tool, pinned))
            .transpose()?
            .flatten();
        Ok(super::ExecutionDraft::new(
            self.call.clone(),
            tool,
            self.request.clone(),
            self.policy,
            lash_sansio::ExecutionLimit {
                expires_at: self.limit_expires_at_ms,
                max_slice: std::time::Duration::from_millis(self.limit_max_slice_ms),
            },
            park,
        )
        .with_pinned_wait(pinned)
        .with_trace(self.trace.clone())
        .with_declaration(self.declaration.clone())
        .with_binding(self.binding.clone()))
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
        self.wait_id.encode(out);
        encode_opt_tree(self.trace.as_ref(), out);
        encode_opt_tree(self.declaration.as_ref(), out);
        encode_opt_tree(self.binding.as_ref(), out);
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
            wait_id: Option::decode(input)?,
            trace: decode_opt_tree(input)?,
            declaration: decode_opt_tree(input)?,
            binding: decode_opt_tree(input)?,
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

/// An execution's pin as its Admission node (kind 18) holds it: the run
/// and ordinal the admitting park minted, and the pin. A round admitted by
/// admissions names each member by its Admission, and its fold reads the
/// member back from it; the kernel broker owns the rest of the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmittedPin {
    pub run: u64,
    pub ordinal: u64,
    pub pin: PinnedMember,
}

/// The fields of an Admission body a round's fold reads: its execution
/// identity, effect identity, wait and effect name are skipped as whole
/// trees, its request and disposition read.
struct AdmissionPinView(Option<AdmittedPin>);

impl TokenCodec for AdmissionPinView {
    fn encode(&self, _out: &mut TokenWriter) {}

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(6)?;
        input.text()?;
        input.skip_tree()?;
        input.unsigned()?;
        input.skip_tree()?;
        NodeId::decode(input)?;
        let (variant, fields) = input.sum()?;
        Ok(Self(match variant {
            0 => {
                TokenReader::expect_fields(fields, 3)?;
                Some(AdmittedPin {
                    run: input.unsigned()?,
                    ordinal: input.unsigned()?,
                    pin: PinnedMember::decode(input)?,
                })
            }
            1 | 2 => {
                TokenReader::expect_fields(fields, 0)?;
                None
            }
            other => return Err(unsupported(other)),
        }))
    }
}

/// The execution pin Admission `node` holds.
///
/// # Errors
///
/// Any refusal of the node read, or [`GraphRefusal::InvalidBody`] for an
/// admission of a sleep or a refusal, which no round admits.
pub fn admission_pin(
    decoder: &mut GraphDecoder<'_>,
    node: NodeId,
) -> Result<AdmittedPin, GraphRefusal> {
    let AdmissionPinView(pin) = decoder.decode(node, NodeKind::ADMISSION)?;
    pin.ok_or_else(|| GraphRefusal::InvalidBody {
        kind: NodeKind::ADMISSION.0,
        reason: "a round admits only an execution's admission".to_owned(),
    })
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
    /// state its completion resolved (as a tree) and the CheckpointRoot of
    /// the heap pages it published, which the fact owns (FIG-6193).
    Outcome {
        start: u64,
        output: SettledFact,
        state: Vec<lash_core_store::tool_run::StateResolution>,
        pages: Option<NodeId>,
    },
    /// 7: a cell's report: a reference to its ReportView over the
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
        self.check(&mut GraphDecoder::new(
            &*staging,
            DecodeLimits::within_total(1 << 32),
        ))?;
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
        fact.check(decoder)?;
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
                if let Self::Outcome {
                    pages: Some(pages), ..
                } = &fact
                {
                    decoder.child_kind(id, *pages, &[NodeKind::CHECKPOINT_ROOT])?;
                }
            }
            Self::CellReport { report } => {
                decoder.child_kind(id, *report, &[NodeKind::REPORT_VIEW])?;
            }
            Self::Start { .. } | Self::Decide(_) | Self::Present => {}
        }
        Ok(fact)
    }

    fn check(&self, decoder: &mut GraphDecoder<'_>) -> Result<(), GraphRefusal> {
        match self {
            Self::Wait { output, .. }
            | Self::Retry { output, .. }
            | Self::Outcome { output, .. } => output.check(decoder),
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
                pages,
            } => {
                out.sum(variant, 4);
                out.unsigned(*start);
                output.encode(out);
                encode_tree(state, out);
                pages.encode(out);
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
                expect(4)?;
                Self::Outcome {
                    start: input.unsigned()?,
                    output: SettledFact::decode(input)?,
                    state: decode_tree(input)?,
                    pages: Option::decode(input)?,
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

/// Kind 32: call-order references to the original archival bodies.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReportView {
    pub entries: Vec<NodeId>,
}
impl TokenCodec for ReportView {
    fn encode(&self, out: &mut TokenWriter) {
        self.entries.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(Self {
            entries: Vec::decode(input)?,
        })
    }
}
impl ReportView {
    /// Stage only references to existing archival call bodies.
    pub fn stage(&self, staging: &mut GraphStaging) -> Result<NodeId, GraphRefusal> {
        staging.encode_value(NodeKind::REPORT_VIEW, self)
    }
    /// Read and validate every archival reference.
    pub fn load(decoder: &mut GraphDecoder<'_>, id: NodeId) -> Result<Self, GraphRefusal> {
        let view: Self = decoder.decode(id, NodeKind::REPORT_VIEW)?;
        for body in &view.entries {
            decoder.child_kind(id, *body, &[NodeKind::CALL_BODY])?;
        }
        Ok(view)
    }
    /// Read typed bodies through the registered offline protocol decoder.
    pub fn records(
        decoder: &mut GraphDecoder<'_>,
        id: NodeId,
        decoders: &lash_core_store::transcript::TranscriptDecoders,
    ) -> Result<Vec<crate::ToolCallRecord>, GraphRefusal> {
        Self::load(decoder, id)?
            .entries
            .into_iter()
            .map(|body| decoders.decode_cell_call(decoder, body))
            .collect()
    }
}

#[cfg(test)]
#[path = "graph_tests.rs"]
mod tests;
