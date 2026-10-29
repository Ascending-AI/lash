//! A turn's transcript (kind 45, FIG-6601): the calls its ledger answered,
//! in the order its runs answered them, each by reference to the canonical
//! material the ledger already named. Nothing is copied: a round member's
//! answer is its outcome's archival CallBody (or, for a payload kept as
//! text, its FrozenValue), and a cell's answers are its report's ReportView.
//!
//! The turn's terminal stages its transcript and publishes it as the run's
//! root in the commit that retires the ledger; a settled turn's calls are
//! read from it alone. An open turn's calls are the same derivation over
//! its ledger as it stands.

use std::collections::BTreeMap;

use lash_core_store::checkpoint_graph::GraphDecoder;
use lash_core_store::checkpoint_graph::GraphRefusal;
use lash_core_store::checkpoint_graph::GraphStaging;
use lash_core_store::checkpoint_graph::NodeId;
use lash_core_store::checkpoint_graph::NodeKind;
use lash_durable::domain::{RunFactKind, RunFactRow, RunFacts, RunSeq};
use lash_kernel_value::tokens::TokenCodec;
use lash_kernel_value::tokens::TokenError;
use lash_kernel_value::tokens::TokenReader;
use lash_kernel_value::tokens::TokenWriter;

use super::graph::{self, AdmitMember, CallBody, CallOutcome, MaterialNode, RoundFact};

/// One answered call, or one cell's answered calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptEntry {
    /// 0: a round member's answer: its outcome's archival CallBody.
    Call(NodeId),
    /// 1: a round member's answer whose payload the ledger kept as text:
    /// its FrozenValue.
    Payload(NodeId),
    /// 2: a cell's answered calls: its report's ReportView.
    Report(NodeId),
    /// A recorded model round retains its immutable environment lifetime.
    Environment(NodeId),
    Read(NodeId),
}

/// Kind 45: a turn's answered calls, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TurnTranscript {
    pub entries: Vec<TranscriptEntry>,
}

impl TokenCodec for TranscriptEntry {
    fn encode(&self, out: &mut TokenWriter) {
        let (variant, node) = match self {
            Self::Call(node) => (0, node),
            Self::Payload(node) => (1, node),
            Self::Report(node) => (2, node),
            Self::Environment(node) => (3, node),
            Self::Read(node) => (4, node),
        };
        out.sum(variant, 1);
        node.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 1)?;
        let node = NodeId::decode(input)?;
        match variant {
            0 => Ok(Self::Call(node)),
            1 => Ok(Self::Payload(node)),
            2 => Ok(Self::Report(node)),
            3 => Ok(Self::Environment(node)),
            4 => Ok(Self::Read(node)),
            other => Err(TokenError::UnsupportedVariant { variant: other }),
        }
    }
}

impl TokenCodec for TurnTranscript {
    fn encode(&self, out: &mut TokenWriter) {
        self.entries.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(Self {
            entries: Vec::decode(input)?,
        })
    }
}

fn corrupt(reason: impl Into<String>) -> GraphRefusal {
    GraphRefusal::InvalidBody {
        kind: NodeKind::ROUND_FACT.0,
        reason: reason.into(),
    }
}

impl TurnTranscript {
    /// The transcript of the turn whose ledger `facts` holds: each run in
    /// order, a round's members in declared order with the material of their
    /// final outcomes (a call that settled without material is not an
    /// answer), and a cell report's view. Reads each fact's own node, an
    /// admission's, and a call outcome's to name its archival body; never a
    /// report's calls.
    /// `None` when the turn answered no call.
    ///
    /// # Errors
    ///
    /// A fact or node that does not decode, or a run that does not open with
    /// its admission or its report.
    pub fn of(facts: &RunFacts) -> Result<Option<Self>, GraphRefusal> {
        let mut runs: BTreeMap<RunSeq, Vec<&RunFactRow>> = BTreeMap::new();
        for row in &facts.rows {
            runs.entry(row.run).or_default().push(row);
        }
        let mut decoder = GraphDecoder::new(facts, facts.read_limits);
        let mut entries = Vec::new();
        for (run, mut rows) in runs {
            rows.sort_by_key(|row| row.ordinal);
            let first: RoundFact = decoder.decode(rows[0].record, NodeKind::ROUND_FACT)?;
            let (members, plans) = match first {
                RoundFact::CellReport { report } => {
                    entries.push(TranscriptEntry::Report(report));
                    continue;
                }
                RoundFact::Admit { members, reads } => (members, reads),
                _ => {
                    return Err(corrupt(format!(
                        "run {} opens without its admission",
                        run.0
                    )));
                }
            };
            let mut finals = BTreeMap::new();
            for row in rows.iter().filter(|row| row.kind == RunFactKind::XOutcome) {
                let RoundFact::Outcome { output, .. } =
                    decoder.decode(row.record, NodeKind::ROUND_FACT)?
                else {
                    return Err(corrupt("an outcome row holds another fact"));
                };
                if let Some(call) = &row.call {
                    finals.insert(call.clone(), output.material);
                }
            }
            let mut read_facts = rows
                .iter()
                .filter(|row| row.kind == RunFactKind::Present)
                .map(|row| {
                    let RoundFact::Present { reads } = RoundFact::load(&mut decoder, row.record)?
                    else {
                        return Err(corrupt("a present row holds another fact"));
                    };
                    reads
                        .into_iter()
                        .map(|node| {
                            let fact = graph::load_model_read(&mut decoder, node)?.fact;
                            let plan = plans
                                .iter()
                                .find(|plan| plan.call_id == fact.call_id)
                                .ok_or_else(|| {
                                    corrupt("a saved read has no slot in its model plan")
                                })?;
                            Ok((plan.slot + 1, node))
                        })
                        .collect::<Result<Vec<_>, GraphRefusal>>()
                })
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .flatten()
                .peekable();
            let mut slot = 1;
            for member in members {
                while read_facts
                    .peek()
                    .is_some_and(|(ordinal, _)| *ordinal == slot)
                {
                    if let Some((_, node)) = read_facts.next() {
                        entries.push(TranscriptEntry::Read(node));
                        slot += 1;
                    }
                }
                slot += 1;
                let call = match member {
                    AdmitMember::Pinned(pin) => pin.call,
                    AdmitMember::Admission(node) => {
                        graph::admission_pin(&mut decoder, node)?.pin.call
                    }
                };
                let Some(Some(material)) = finals.remove(&call) else {
                    continue;
                };
                entries.push(match material {
                    MaterialNode::Outcome(node) | MaterialNode::CallPayload(node) => {
                        TranscriptEntry::Call(CallOutcome::body_node(&mut decoder, node)?)
                    }
                    MaterialNode::Value(node) => TranscriptEntry::Payload(node),
                });
            }
            entries.extend(read_facts.map(|(_, node)| TranscriptEntry::Read(node)));
        }
        Ok((!entries.is_empty()).then_some(Self { entries }))
    }

    /// Stages this transcript and returns its node.
    ///
    /// # Errors
    ///
    /// Any refusal of the staging area.
    pub fn stage(&self, staging: &mut GraphStaging) -> Result<NodeId, GraphRefusal> {
        Ok(staging.encode_value(NodeKind::TURN_TRANSCRIPT, self)?)
    }

    /// Reads transcript `id`, checking each entry names a node of its kind.
    ///
    /// # Errors
    ///
    /// Any refusal of a node read, or [`GraphRefusal::WrongChildKind`].
    pub fn load(decoder: &mut GraphDecoder<'_>, id: NodeId) -> Result<Self, GraphRefusal> {
        let transcript: Self = decoder.decode(id, NodeKind::TURN_TRANSCRIPT)?;
        for entry in &transcript.entries {
            let (node, kind) = match entry {
                TranscriptEntry::Call(node) => (node, NodeKind::CALL_BODY),
                TranscriptEntry::Payload(node) | TranscriptEntry::Read(node) => {
                    (node, NodeKind::FROZEN_VALUE)
                }
                TranscriptEntry::Report(node) => (node, NodeKind::REPORT_VIEW),
                TranscriptEntry::Environment(node) => (node, NodeKind::ROUND_ENVIRONMENT),
            };
            decoder.child_kind(id, *node, &[kind])?;
        }
        Ok(transcript)
    }

    /// The calls this transcript answers, in order: a round member's from its
    /// archival body or its payload, a cell's through the registered offline
    /// protocol decoder.
    ///
    /// # Errors
    ///
    /// Any refusal of a node read, or [`GraphRefusal::InvalidBody`] for a
    /// payload that is no completed call.
    pub fn records(
        &self,
        decoder: &mut GraphDecoder<'_>,
        decoders: &lash_core_store::transcript::TranscriptDecoders,
    ) -> Result<Vec<crate::ToolCallRecord>, GraphRefusal> {
        let mut records = Vec::new();
        for entry in &self.entries {
            match *entry {
                TranscriptEntry::Environment(_) => {}
                TranscriptEntry::Read(node) => {
                    records.push(graph::load_model_read(decoder, node)?.fact.record())
                }
                TranscriptEntry::Call(body) => records.push(CallBody::load(decoder, body)?.record),
                TranscriptEntry::Payload(node) => {
                    let serde_json::Value::String(payload) = graph::load_value(decoder, node)?
                    else {
                        return Err(GraphRefusal::InvalidBody {
                            kind: NodeKind::FROZEN_VALUE.0,
                            reason: "a call's payload is text".to_owned(),
                        });
                    };
                    let completed: super::CompletedCall =
                        serde_json::from_str(&payload).map_err(|_| GraphRefusal::InvalidBody {
                            kind: NodeKind::FROZEN_VALUE.0,
                            reason: "a call's payload is no readable tool answer".to_owned(),
                        })?;
                    records.push(crate::ToolCallRecord {
                        call_id: completed.call_id,
                        provider_call_id: completed.provider_call_id,
                        tool: completed.tool_name,
                        args: completed.args,
                        output: completed.output,
                    });
                }
                TranscriptEntry::Report(view) => {
                    records.extend(graph::ReportView::records(decoder, view, decoders)?);
                }
            }
        }
        Ok(records)
    }
}
