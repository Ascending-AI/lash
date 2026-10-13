//! FIG-6171's byte audit against the pre-change committed report fixture.
//! Only the test oracle encodes the removed copy; production stores references.

use lash_core_execution::runtime::actor::round::graph::RoundFact;
use lash_core_store::checkpoint_graph::{
    EncodeBudget, ExplicitLimit, GraphDecoder, GraphRefusal, GraphStaging, NodeId, NodeKind,
    TokenCodec, TokenError, TokenReader, TokenWriter, closure,
};
use lash_durable::domain::{RunFactKind, RunFacts};

struct PreviousReport {
    entries: Vec<(NodeId, NodeId, serde_json::Value)>,
    omitted: Option<serde_json::Value>,
}

impl TokenCodec for PreviousReport {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.seq(self.entries.len() as u64);
        for (outcome, presentation, args) in &self.entries {
            out.seq(3);
            outcome.encode(out);
            presentation.encode(out);
            args.encode(out);
        }
        self.omitted.encode(out);
    }
    fn decode(_input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Err(TokenError::Invalid(
            "the byte audit never resumes the previous report".to_owned(),
        ))
    }
}

struct PreviousAnswer(Option<NodeId>);
impl lash_sansio::tool_output::graph::AnswerSlot for PreviousAnswer {
    fn write(&self, _value: &crate::ToolValue, out: &mut TokenWriter) {
        if let Some(node) = self.0 {
            node.encode(out);
        }
    }
    fn read(&self, _input: &mut TokenReader<'_>) -> Result<crate::ToolValue, TokenError> {
        Err(TokenError::Invalid("the byte audit only writes".to_owned()))
    }
}
struct PreviousOutcome<'a> {
    record: &'a crate::ToolCallRecord,
    answer: Option<NodeId>,
    presentation: NodeId,
}
impl TokenCodec for PreviousOutcome<'_> {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(9);
        out.text(self.record.call_id.as_str());
        Option::<NodeId>::None.encode(out);
        out.unsigned(0);
        out.text(&self.record.tool);
        lash_sansio::tool_output::graph::encode_output(
            &self.record.output,
            &PreviousAnswer(self.answer),
            out,
        );
        self.record.provider_call_id.encode(out);
        out.none();
        serde_json::json!([]).encode(out);
        self.presentation.encode(out);
    }
    fn decode(_input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Err(TokenError::Invalid("the byte audit only writes".to_owned()))
    }
}

/// The reference report's own bytes, its entire retained closure's bytes,
/// and the previous copied report's closure bytes for the same cell.
///
/// # Errors
///
/// Any refusal of the graph read or the audit fixture's staging.
pub fn cell_ledger_bytes(
    facts: &RunFacts,
    records: &[crate::ToolCallRecord],
) -> Result<(u64, u64, u64), GraphRefusal> {
    let mut decoder = GraphDecoder::new(facts, facts.read_limits);
    let mut report_bytes = 0;
    let mut held = std::collections::BTreeMap::new();
    for row in facts
        .rows
        .iter()
        .filter(|row| row.kind == RunFactKind::CellReport)
    {
        if let RoundFact::CellReport { report } = RoundFact::load(&mut decoder, row.record)? {
            let nodes = closure(facts, report, &facts.read_limits)?;
            if let Some(root) = nodes.get(&report) {
                report_bytes += root.bytes.len() as u64;
            }
            held.extend(nodes);
        }
    }
    let held_bytes = held.values().map(|node| node.bytes.len() as u64).sum();
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let mut entries = Vec::new();
    for record in records.iter().take(128) {
        let model_return = crate::ModelToolReturn::from_output(record.tool.clone(), &record.output);
        let recipe =
            lash_sansio::tool_output::graph::PresentationRecipe::of(&model_return, &record.output);
        let presentation = staging.encode_value(NodeKind::PRESENTATION, &recipe)?;
        let answer = match &record.output.outcome {
            crate::ToolCallOutcome::Success(value) => Some(staging.encode_value(
                NodeKind::FROZEN_VALUE,
                &lash_sansio::tool_output::graph::FrozenToolValue(value.clone()),
            )?),
            _ => None,
        };
        let outcome = staging.encode_value(
            NodeKind::CALL_OUTCOME,
            &PreviousOutcome {
                record,
                answer,
                presentation,
            },
        )?;
        entries.push((outcome, presentation, record.args.clone()));
    }
    let omitted = (records.len() > 128).then(|| {
        serde_json::json!({
            "count": records.len() - 128,
            "failures": records[128..].iter().filter(|record| !record.output.is_success()).count(),
            "attachments": [],
        })
    });
    let previous =
        staging.encode_value(NodeKind::REPORT_VIEW, &PreviousReport { entries, omitted })?;
    let previous_bytes = closure(&staging, previous, &facts.read_limits)?
        .values()
        .map(|node| node.bytes.len() as u64)
        .sum();
    Ok((report_bytes, held_bytes, previous_bytes))
}
