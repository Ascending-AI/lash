use std::collections::HashMap;

use super::*;

/// Mutable canonical history. Appends touch one node's bounded occurrence index;
/// snapshot reads pay for sorting and projecting the retained observations.
pub struct TraceLashlangGraphAccumulator {
    history_limit: usize,
    identity: Option<LanguageIdentity>,
    execution_map: Option<LanguageExecutionMap>,
    status: Option<LanguageExecutionStatus>,
    settlement: Option<TraceLashlangGraphSettlement>,
    execution_history: BTreeMap<TraceLashlangEventIdentity, HistoryEntry>,
    nodes: HashMap<String, NodeHistory>,
}

#[derive(Default)]
struct NodeHistory {
    occurrences: BTreeMap<u64, BTreeMap<TraceLashlangEventIdentity, HistoryEntry>>,
    retention: Option<TraceLashlangNodeRetention>,
}

struct HistoryEntry {
    event: TraceLashlangGraphHistoryEvent,
    variants: BTreeSet<String>,
}

fn insert_event(
    history: &mut BTreeMap<TraceLashlangEventIdentity, HistoryEntry>,
    candidate: TraceLashlangGraphHistoryEvent,
) {
    match history.entry(candidate.identity.clone()) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(HistoryEntry {
                event: candidate,
                variants: BTreeSet::new(),
            });
        }
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            let current = entry.get_mut();
            if current.event != candidate {
                insert_bounded_variant(&mut current.variants, history_digest(&current.event));
                insert_bounded_variant(&mut current.variants, history_digest(&candidate));
                current.event = canonical_history_event(current.event.clone(), candidate);
            }
        }
    }
}

impl Default for TraceLashlangGraphAccumulator {
    fn default() -> Self {
        Self::new(DEFAULT_LASHLANG_GRAPH_HISTORY_LIMIT)
    }
}

impl TraceLashlangGraphAccumulator {
    /// Retain at most `history_limit` occurrences per node (at least one).
    /// The standard preset is 256; no workload measurements justify that value.
    pub fn new(history_limit: usize) -> Self {
        Self {
            history_limit: history_limit.max(1),
            identity: None,
            execution_map: None,
            status: None,
            settlement: None,
            execution_history: Default::default(),
            nodes: Default::default(),
        }
    }
    /// Fold a batch into one graph without cloning its retained history.
    /// Invalid graph keys refuse the whole batch before changing the accumulator.
    pub fn fold(&mut self, records: &[TraceRecord]) -> Result<(), TraceLashlangGraphFoldError> {
        let incoming = records.iter().filter_map(|record| match &record.event {
            TraceEvent::LanguageExecution { event, .. } => Some((record.timestamp, event)),
            _ => None,
        });
        let Some((_, first)) = incoming.clone().next() else {
            return if self.identity.is_some() {
                Ok(())
            } else {
                Err(TraceLashlangGraphFoldError::NoLanguageExecutionEvents)
            };
        };
        let graph_key = first.identity.graph_key();
        if let Some(previous) = &self.identity
            && previous.graph_key() != graph_key
        {
            return Err(TraceLashlangGraphFoldError::PreviousGraphMismatch {
                previous: previous.graph_key(),
                event: graph_key,
            });
        }
        for (_, event) in incoming.clone() {
            let other = event.identity.graph_key();
            if other != graph_key {
                return Err(TraceLashlangGraphFoldError::MixedGraphKeys {
                    first: graph_key,
                    other,
                });
            }
        }
        for (timestamp, event) in incoming {
            self.append(timestamp, event);
        }
        Ok(())
    }

    /// Fold a canonical language observation without a diagnostic trace envelope.
    pub fn observe(
        &mut self,
        observation: &crate::LanguageExecutionObservation,
    ) -> Result<(), TraceLashlangGraphFoldError> {
        let timestamp = i64::try_from(observation.observed_at_ms)
            .ok()
            .and_then(DateTime::from_timestamp_millis)
            .ok_or(TraceLashlangGraphFoldError::InvalidObservationTimestamp {
                observed_at_ms: observation.observed_at_ms,
            })?;
        let event = &observation.execution;
        if let Some(previous) = &self.identity
            && previous.graph_key() != event.identity.graph_key()
        {
            return Err(TraceLashlangGraphFoldError::PreviousGraphMismatch {
                previous: previous.graph_key(),
                event: event.identity.graph_key(),
            });
        }
        self.append(timestamp, event);
        Ok(())
    }

    /// Reconcile committed or snapshot terminal evidence, even before replay arrives.
    pub fn settle(&mut self, settlement: TraceLashlangGraphSettlement) {
        self.settlement = Some(settlement.refine(self.settlement));
    }

    /// Discard provisional continuity after a gap or reexecution boundary.
    /// The static definition and durable terminal authority survive the reset.
    pub fn reset_live(&mut self) {
        self.execution_history.clear();
        self.nodes.clear();
        self.status = None;
    }

    pub(super) fn identity(&self) -> Option<&LanguageIdentity> {
        self.identity.as_ref()
    }

    pub(super) fn append(&mut self, timestamp: DateTime<Utc>, event: &TraceLanguageExecution) {
        let identity = match self.identity.take() {
            Some(previous) => canonical_identity_fields(previous, event.identity.clone()),
            None => event.identity.clone(),
        };
        let status = self.status.unwrap_or(LanguageExecutionStatus::Running);
        self.status = Some(observed_execution_status(status, event));
        if let TraceLanguageExecutionPayload::ExecutionStarted { execution_map: map } =
            &event.payload
        {
            self.execution_map = Some(match self.execution_map.take() {
                None => map.clone(),
                Some(current) if current == *map => current,
                Some(current) => canonical_value(current, map.clone()),
            });
        }
        let mut event = event.clone();
        event.event_key.clear();
        let event_identity = event_identity(&event);
        if let Some((node_id, occurrence)) = node_occurrence(&event_identity) {
            let node_id = node_id.to_string();
            let node = self.nodes.entry(node_id.clone()).or_default();
            if let Some(retention) = &mut node.retention
                && occurrence <= retention.truncation_watermark
            {
                merge_late_retained_event(retention, timestamp, &event);
            } else {
                insert_event(
                    node.occurrences.entry(occurrence).or_default(),
                    TraceLashlangGraphHistoryEvent {
                        identity: event_identity,
                        timestamp,
                        event,
                    },
                );
                if node.occurrences.len() > self.history_limit
                    && let Some((occurrence, dropped)) = node.occurrences.pop_first()
                {
                    let dropped: Vec<_> = dropped.into_values().map(|entry| entry.event).collect();
                    node.retention = Some(merge_node_retention(
                        node.retention.take(),
                        &node_id,
                        occurrence,
                        &dropped,
                        self.execution_map.as_ref(),
                        &identity,
                    ));
                }
            }
        } else {
            insert_event(
                &mut self.execution_history,
                TraceLashlangGraphHistoryEvent {
                    identity: event_identity,
                    timestamp,
                    event,
                },
            );
        }
        self.identity = Some(identity);
    }

    /// Materialize the pure fold's canonical snapshot without inferring child links.
    pub fn snapshot(&self) -> Option<TraceLashlangGraph> {
        let identity = self.identity.as_ref()?;
        let entries = self.execution_history.values().chain(
            self.nodes
                .values()
                .flat_map(|node| node.occurrences.values().flat_map(BTreeMap::values)),
        );
        let mut history = Vec::new();
        let mut conflicts = Vec::new();
        for entry in entries {
            history.push(entry.event.clone());
            if !entry.variants.is_empty() {
                conflicts.push(TraceLashlangGraphConflict {
                    identity: entry.event.identity.clone(),
                    kind: TraceLashlangGraphConflictKind::ConflictingDuplicate,
                    variants: entry.variants.iter().cloned().collect(),
                });
            }
        }
        history.sort_by(|left, right| left.identity.cmp(&right.identity));
        conflicts.sort_by(|left, right| left.identity.cmp(&right.identity));
        let mut retention: Vec<_> = self
            .nodes
            .values()
            .filter_map(|node| node.retention.clone())
            .collect();
        retention.sort_by(|left, right| left.node_id.cmp(&right.node_id));
        Some(materialize_graph(
            identity.clone(),
            self.execution_map.clone(),
            history,
            conflicts,
            self.history_limit,
            retention,
            ExecutionProjection {
                status: self.status.unwrap_or(LanguageExecutionStatus::Running),
                settlement: self.settlement,
            },
        ))
    }
}
