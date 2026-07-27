use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::trace::{OracleStatus, OracleVerdict};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeTurnObservation {
    pub session_id: String,
    pub turn_index: usize,
    pub assistant_message: String,
    pub graph_node_count: usize,
    pub transcript_message_count: usize,
    pub activity_count: usize,
    pub provider_exchange_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_invariant: Option<RuntimeGraphInvariantFacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_frame_invariant: Option<RuntimeAgentFrameInvariantFacts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_invariant: Option<RuntimeUsageInvariantFacts>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeGraphInvariantFacts {
    pub node_count: usize,
    pub edge_count: usize,
    pub duplicate_node_ids: Vec<String>,
    pub missing_parent_links: Vec<RuntimeGraphMissingParent>,
    pub cycle_node_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leaf_node_id: Option<String>,
    pub leaf_exists: bool,
    pub passed: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeGraphMissingParent {
    pub node_id: String,
    pub parent_node_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeAgentFrameInvariantFacts {
    pub current_frame_node_id: String,
    pub frame_count: usize,
    pub active_frame_ids: Vec<String>,
    pub current_frame_exists: bool,
    pub current_frame_active: bool,
    pub nodes_without_agent_frame: Vec<String>,
    pub node_agent_frame_ids_without_record: Vec<String>,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_limit: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeUsageInvariantFacts {
    pub turn_usage: RuntimeUsageTotals,
    pub total_usage: RuntimeUsageTotals,
    pub token_ledger_total: RuntimeUsageTotals,
    pub child_usage_total: RuntimeUsageTotals,
    pub token_ledger_entry_count: usize,
    pub child_usage_entry_count: usize,
    pub usage_event_count: usize,
    pub usage_event_cumulative_totals: Vec<RuntimeUsageTotals>,
    pub non_negative: bool,
    pub usage_events_monotonic: bool,
    pub negative_fields: Vec<String>,
    pub passed: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RuntimeUsageTotals {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RuntimeFinalValueInvariantFacts {
    pub outcome_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_value: Option<Value>,
    pub terminal_event_count: usize,
    pub assistant_prose_delta_count: usize,
    pub assistant_output_text: String,
    pub semantic_channel_observed: bool,
    pub transcript_inference_required: bool,
    pub passed: bool,
}

pub fn runtime_turn_contract(
    observation: &RuntimeTurnObservation,
    expected_session_id: &str,
    expected_turn_index: usize,
    expected_assistant_message: &str,
    expected_provider_exchange_count: usize,
) -> OracleVerdict {
    if observation.session_id != expected_session_id {
        return OracleVerdict::failed(
            "runtime.turn_contract",
            format!(
                "session id diverged: expected `{expected_session_id}`, got `{}`",
                observation.session_id
            ),
        );
    }
    if observation.turn_index != expected_turn_index {
        return OracleVerdict::failed(
            "runtime.turn_contract",
            format!(
                "turn index diverged: expected {expected_turn_index}, got {}",
                observation.turn_index
            ),
        );
    }
    if observation.assistant_message != expected_assistant_message {
        return OracleVerdict::failed(
            "runtime.turn_contract",
            format!(
                "provider output diverged: expected `{expected_assistant_message}`, got `{}`",
                observation.assistant_message
            ),
        );
    }
    if observation.provider_exchange_count != expected_provider_exchange_count {
        return OracleVerdict::failed(
            "runtime.turn_contract",
            format!(
                "provider exchange count diverged: expected {expected_provider_exchange_count}, got {}",
                observation.provider_exchange_count
            ),
        );
    }
    let expected_graph_min_count = expected_turn_index * 2 + 1;
    if observation.graph_node_count < expected_graph_min_count {
        return OracleVerdict::failed(
            "runtime.turn_contract",
            format!(
                "session graph too small for turn {expected_turn_index}: {} nodes",
                observation.graph_node_count
            ),
        );
    }
    let expected_transcript_min_count = expected_turn_index * 2;
    if observation.transcript_message_count < expected_transcript_min_count {
        return OracleVerdict::failed(
            "runtime.turn_contract",
            format!(
                "transcript too small for turn {expected_turn_index}: {} messages",
                observation.transcript_message_count
            ),
        );
    }
    if observation.activity_count == 0 {
        return OracleVerdict::failed(
            "runtime.turn_contract",
            "turn emitted no runtime activities".to_string(),
        );
    }
    if let Some(graph) = &observation.graph_invariant
        && !graph.passed
    {
        return OracleVerdict::failed(
            "runtime.turn_contract",
            format!("session graph invariant failed: {graph:?}"),
        );
    }
    if let Some(agent_frame) = &observation.agent_frame_invariant
        && !agent_frame.passed
    {
        return OracleVerdict::failed(
            "runtime.turn_contract",
            format!("agent frame invariant failed: {agent_frame:?}"),
        );
    }
    if let Some(usage) = &observation.usage_invariant
        && !usage.passed
    {
        return OracleVerdict::failed(
            "runtime.turn_contract",
            format!("usage invariant failed: {usage:?}"),
        );
    }
    OracleVerdict::passed(
        "runtime.turn_contract",
        format!(
            "turn {expected_turn_index} matched session, transcript, graph, and provider output contracts"
        ),
    )
}

pub fn require_passed(verdict: &OracleVerdict) -> Result<(), String> {
    match verdict.status {
        OracleStatus::Passed => Ok(()),
        OracleStatus::Failed => Err(verdict.message.clone()),
    }
}

pub fn runtime_graph_invariant_facts(
    graph: &lash_core::SessionGraph,
) -> RuntimeGraphInvariantFacts {
    let mut seen = BTreeSet::new();
    let mut duplicate_node_ids = BTreeSet::new();
    let mut parent_by_node = BTreeMap::<String, Option<String>>::new();
    for node in &graph.nodes {
        if !seen.insert(node.node_id.clone()) {
            duplicate_node_ids.insert(node.node_id.clone());
        }
        parent_by_node
            .entry(node.node_id.clone())
            .or_insert_with(|| node.parent_node_id.clone());
    }
    let missing_parent_links = graph
        .nodes
        .iter()
        .filter_map(|node| {
            let parent_node_id = node.parent_node_id.as_ref()?;
            (!parent_by_node.contains_key(parent_node_id)).then(|| RuntimeGraphMissingParent {
                node_id: node.node_id.clone(),
                parent_node_id: parent_node_id.clone(),
            })
        })
        .collect::<Vec<_>>();
    let mut cycle_node_ids = BTreeSet::new();
    for start in parent_by_node.keys() {
        let mut path = BTreeSet::new();
        let mut current = Some(start.as_str());
        while let Some(node_id) = current {
            if !path.insert(node_id.to_string()) {
                cycle_node_ids.insert(node_id.to_string());
                break;
            }
            current = parent_by_node
                .get(node_id)
                .and_then(Option::as_deref)
                .filter(|parent| parent_by_node.contains_key(*parent));
        }
    }
    let edge_count = graph
        .nodes
        .iter()
        .filter(|node| node.parent_node_id.is_some())
        .count();
    let leaf_exists = graph
        .leaf_node_id
        .as_ref()
        .is_none_or(|leaf| parent_by_node.contains_key(leaf));
    let duplicate_node_ids = duplicate_node_ids.into_iter().collect::<Vec<_>>();
    let cycle_node_ids = cycle_node_ids.into_iter().collect::<Vec<_>>();
    let passed = duplicate_node_ids.is_empty()
        && missing_parent_links.is_empty()
        && cycle_node_ids.is_empty()
        && leaf_exists;
    RuntimeGraphInvariantFacts {
        node_count: graph.nodes.len(),
        edge_count,
        duplicate_node_ids,
        missing_parent_links,
        cycle_node_ids,
        leaf_node_id: graph.leaf_node_id.clone(),
        leaf_exists,
        passed,
    }
}

pub fn runtime_agent_frame_invariant_facts(
    snapshot: &lash_core::SessionSnapshot,
) -> RuntimeAgentFrameInvariantFacts {
    let frame_ids = snapshot
        .session_graph
        .nodes
        .iter()
        .filter(|node| {
            matches!(
                node.payload,
                lash_core::SessionNodePayload::FrameOpen { .. }
            )
        })
        .map(|node| node.node_id.clone())
        .collect::<BTreeSet<_>>();
    let active_frame_ids = snapshot
        .current_frame_node_id
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    let current_frame_exists = snapshot
        .current_frame_node_id
        .as_ref()
        .is_some_and(|frame| frame_ids.contains(frame));
    let canonical_frame_node_id = snapshot
        .session_graph
        .nearest_frame_node_id(snapshot.session_graph.leaf_node_id.as_deref());
    let current_frame_active = snapshot.current_frame_node_id.as_deref() == canonical_frame_node_id;
    let nodes_without_agent_frame = snapshot
        .session_graph
        .nodes
        .iter()
        .filter(|node| {
            snapshot
                .session_graph
                .nearest_frame_node_id(Some(&node.node_id))
                .is_none()
        })
        .map(|node| node.node_id.clone())
        .collect::<Vec<_>>();
    let node_agent_frame_ids_without_record = snapshot
        .session_graph
        .nodes
        .iter()
        .filter_map(|node| {
            snapshot
                .session_graph
                .nearest_frame_node_id(Some(&node.node_id))
        })
        .filter(|frame_id| !frame_ids.contains(*frame_id))
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let passed = active_frame_ids.len() == 1
        && active_frame_ids.first() == snapshot.current_frame_node_id.as_ref()
        && current_frame_exists
        && current_frame_active
        && nodes_without_agent_frame.is_empty()
        && node_agent_frame_ids_without_record.is_empty();
    RuntimeAgentFrameInvariantFacts {
        current_frame_node_id: snapshot.current_frame_node_id.clone().unwrap_or_default(),
        frame_count: frame_ids.len(),
        active_frame_ids,
        current_frame_exists,
        current_frame_active,
        nodes_without_agent_frame,
        node_agent_frame_ids_without_record,
        passed,
        observation_limit: None,
    }
}

pub fn runtime_usage_invariant_facts(
    result: &lash::TurnResult,
    activities: &[lash::TurnActivity],
) -> RuntimeUsageInvariantFacts {
    let turn_usage = RuntimeUsageTotals::from_usage(&result.usage);
    let total_usage = RuntimeUsageTotals::from_usage(&result.total_usage());
    let token_ledger_total =
        RuntimeUsageTotals::sum(result.state.token_ledger.iter().map(|entry| &entry.usage));
    let child_usage_total =
        RuntimeUsageTotals::sum(result.children_usage.iter().map(|entry| &entry.usage));
    let usage_event_cumulative_totals = activities
        .iter()
        .filter_map(|activity| match &activity.event {
            lash::TurnEvent::Usage { cumulative, .. }
            | lash::TurnEvent::ChildUsage { cumulative, .. } => {
                Some(RuntimeUsageTotals::from_usage(cumulative))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    // Cumulative usage events carry lifetime totals, so `total_tokens` must never
    // regress from one event to the next.
    let usage_events_monotonic = usage_event_cumulative_totals
        .windows(2)
        .all(|window| window[1].total_tokens >= window[0].total_tokens);
    let mut negative_fields = Vec::new();
    collect_negative_usage_fields("turn_usage", &turn_usage, &mut negative_fields);
    collect_negative_usage_fields("total_usage", &total_usage, &mut negative_fields);
    collect_negative_usage_fields(
        "token_ledger_total",
        &token_ledger_total,
        &mut negative_fields,
    );
    collect_negative_usage_fields(
        "child_usage_total",
        &child_usage_total,
        &mut negative_fields,
    );
    for (index, entry) in result.state.token_ledger.iter().enumerate() {
        collect_negative_usage_fields(
            &format!("token_ledger[{index}]"),
            &RuntimeUsageTotals::from_usage(&entry.usage),
            &mut negative_fields,
        );
    }
    for (index, entry) in result.children_usage.iter().enumerate() {
        collect_negative_usage_fields(
            &format!("children_usage[{index}]"),
            &RuntimeUsageTotals::from_usage(&entry.usage),
            &mut negative_fields,
        );
    }
    let non_negative = negative_fields.is_empty();
    RuntimeUsageInvariantFacts {
        turn_usage,
        total_usage,
        token_ledger_total,
        child_usage_total,
        token_ledger_entry_count: result.state.token_ledger.len(),
        child_usage_entry_count: result.children_usage.len(),
        usage_event_count: usage_event_cumulative_totals.len(),
        usage_event_cumulative_totals,
        non_negative,
        usage_events_monotonic,
        negative_fields,
        passed: non_negative && usage_events_monotonic,
    }
}

pub fn runtime_final_value_invariant_facts(
    result: &lash::TurnResult,
    activities: &[lash::TurnActivity],
) -> RuntimeFinalValueInvariantFacts {
    let (outcome_kind, semantic_value) = match &result.outcome {
        lash_core::TurnOutcome::Finished(lash_core::TurnFinish::FinalValue { value }) => {
            ("final_value".to_string(), Some(value.clone()))
        }
        lash_core::TurnOutcome::Finished(lash_core::TurnFinish::ToolValue { value, .. }) => {
            ("tool_value".to_string(), Some(value.clone()))
        }
        lash_core::TurnOutcome::Finished(lash_core::TurnFinish::AssistantMessage { .. }) => {
            ("assistant_message".to_string(), None)
        }
        lash_core::TurnOutcome::AgentFrameSwitch { .. } => ("agent_frame_switch".to_string(), None),
        lash_core::TurnOutcome::Stopped(_) => ("stopped".to_string(), None),
    };
    let terminal_event_count = activities
        .iter()
        .filter(|activity| {
            matches!(
                activity.event,
                lash::TurnEvent::FinalValue { .. } | lash::TurnEvent::ToolValue { .. }
            )
        })
        .count();
    let assistant_prose_delta_count = activities
        .iter()
        .filter(|activity| matches!(activity.event, lash::TurnEvent::AssistantProseDelta { .. }))
        .count();
    let semantic_channel_observed = semantic_value.is_some()
        && terminal_event_count > 0
        && activities.iter().any(|activity| match &activity.event {
            lash::TurnEvent::FinalValue { value } | lash::TurnEvent::ToolValue { value, .. } => {
                Some(value) == semantic_value.as_ref()
            }
            _ => false,
        });
    RuntimeFinalValueInvariantFacts {
        outcome_kind,
        semantic_value,
        terminal_event_count,
        assistant_prose_delta_count,
        assistant_output_text: result.assistant_output.safe_text.clone(),
        semantic_channel_observed,
        transcript_inference_required: !semantic_channel_observed,
        passed: semantic_channel_observed,
    }
}

impl RuntimeUsageTotals {
    pub fn from_usage(usage: &lash_core::TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_write_input_tokens: usage.cache_write_input_tokens,
            reasoning_output_tokens: usage.reasoning_output_tokens,
            total_tokens: usage.total(),
        }
    }

    fn sum<'a>(usages: impl IntoIterator<Item = &'a lash_core::TokenUsage>) -> Self {
        let mut usage = lash_core::TokenUsage::default();
        for item in usages {
            usage.add(item);
        }
        Self::from_usage(&usage)
    }

    pub fn is_non_negative(&self) -> bool {
        self.input_tokens >= 0
            && self.output_tokens >= 0
            && self.cache_read_input_tokens >= 0
            && self.cache_write_input_tokens >= 0
            && self.reasoning_output_tokens >= 0
            && self.total_tokens >= 0
    }
}

fn collect_negative_usage_fields(
    prefix: &str,
    totals: &RuntimeUsageTotals,
    negative_fields: &mut Vec<String>,
) {
    for (field, value) in [
        ("input_tokens", totals.input_tokens),
        ("output_tokens", totals.output_tokens),
        ("cache_read_input_tokens", totals.cache_read_input_tokens),
        ("cache_write_input_tokens", totals.cache_write_input_tokens),
        ("reasoning_output_tokens", totals.reasoning_output_tokens),
        ("total_tokens", totals.total_tokens),
    ] {
        if value < 0 {
            negative_fields.push(format!("{prefix}.{field}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn observation() -> RuntimeTurnObservation {
        RuntimeTurnObservation {
            session_id: "session-001".to_string(),
            turn_index: 2,
            assistant_message: "answer".to_string(),
            graph_node_count: 5,
            transcript_message_count: 4,
            activity_count: 3,
            provider_exchange_count: 2,
            graph_invariant: Default::default(),
            agent_frame_invariant: Default::default(),
            usage_invariant: Default::default(),
        }
    }

    #[test]
    fn runtime_turn_contract_accepts_realistic_observation() {
        let verdict = runtime_turn_contract(&observation(), "session-001", 2, "answer", 2);
        assert_eq!(verdict.status, OracleStatus::Passed);
    }

    #[test]
    fn runtime_turn_contract_rejects_graph_regression() {
        let mut observed = observation();
        observed.graph_node_count = 1;
        let verdict = runtime_turn_contract(&observed, "session-001", 2, "answer", 2);
        assert_eq!(verdict.status, OracleStatus::Failed);
    }

    #[test]
    fn graph_invariant_rejects_missing_parent_and_cycle() {
        let missing_parent: lash_core::SessionGraph = serde_json::from_value(json!({
            "nodes": [
                {
                    "node_id": "child",
                    "parent_node_id": "missing",
                    "timestamp": "2026-01-01T00:00:00Z",
                    "kind": "plugin",
                    "plugin_type": "sim",
                    "body": {}
                }
            ],
            "leaf_node_id": "child"
        }))
        .expect("graph");
        let facts = runtime_graph_invariant_facts(&missing_parent);
        assert!(!facts.passed);
        assert_eq!(facts.missing_parent_links[0].parent_node_id, "missing");

        let cycle: lash_core::SessionGraph = serde_json::from_value(json!({
            "nodes": [
                {
                    "node_id": "a",
                    "parent_node_id": "b",
                    "timestamp": "2026-01-01T00:00:00Z",
                    "kind": "plugin",
                    "plugin_type": "sim",
                    "body": {}
                },
                {
                    "node_id": "b",
                    "parent_node_id": "a",
                    "timestamp": "2026-01-01T00:00:01Z",
                    "kind": "plugin",
                    "plugin_type": "sim",
                    "body": {}
                }
            ],
            "leaf_node_id": "b"
        }))
        .expect("graph");
        let facts = runtime_graph_invariant_facts(&cycle);
        assert!(!facts.passed);
        assert!(!facts.cycle_node_ids.is_empty());
    }

    #[test]
    fn usage_totals_reject_negative_fields() {
        let totals = RuntimeUsageTotals::from_usage(&lash_core::TokenUsage {
            input_tokens: -1,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
        });
        assert!(!totals.is_non_negative());
    }

    #[test]
    fn final_value_facts_require_semantic_channel() {
        let facts = RuntimeFinalValueInvariantFacts {
            outcome_kind: "assistant_message".to_string(),
            semantic_value: None,
            terminal_event_count: 0,
            assistant_prose_delta_count: 1,
            assistant_output_text: "looks final".to_string(),
            semantic_channel_observed: false,
            transcript_inference_required: true,
            passed: false,
        };
        assert!(!facts.passed);
        assert!(facts.transcript_inference_required);
    }
}
