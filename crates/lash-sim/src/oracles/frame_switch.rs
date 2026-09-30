use super::*;

/// Require every frame-switch seed to materialize in full. Comparing canonical
/// node values catches both the historical empty-frame regression and partial
/// application/reordering of producer-validated nodes.
pub fn frame_switch_seeds(observations: &[FrameSwitchSeedObservation]) -> OracleVerdict {
    for observation in observations {
        if observation.expected_nodes.is_empty() {
            return OracleVerdict::failed(
                FRAME_SWITCH_SEED_ORACLE,
                format!(
                    "{} frame-switch observation carried no seed nodes",
                    observation.protocol
                ),
            );
        }
        if observation.observed_nodes != observation.expected_nodes {
            return OracleVerdict::failed(
                FRAME_SWITCH_SEED_ORACLE,
                format!(
                    "{} frame seed changed: expected {:?}, observed {:?}",
                    observation.protocol, observation.expected_nodes, observation.observed_nodes
                ),
            );
        }
    }
    if observations.is_empty() {
        return OracleVerdict::failed(
            FRAME_SWITCH_SEED_ORACLE,
            "no frame-switch seed observations were supplied",
        );
    }
    OracleVerdict::passed(
        FRAME_SWITCH_SEED_ORACLE,
        format!(
            "{} frame-switch seeds materialized completely",
            observations.len()
        ),
    )
}

/// Match the runtime's admission and settlement trace records by root and
/// row (FIG-3927), and require every admitted row to be admitted once and
/// settled once under the root that admitted it: completed as delivered, or
/// handed back open or dropped by that root's commit.
pub fn logical_turn_admissions_settle_exactly_once(
    records: &[lash_core::facade_support::TraceRecord],
) -> OracleVerdict {
    type RowKey = (String, String);
    fn row_ids(payload: &Value, fields: &[&str]) -> Vec<String> {
        fields
            .iter()
            .filter_map(|field| payload.get(*field).and_then(Value::as_array))
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    }
    let mut admitted = BTreeMap::<RowKey, usize>::new();
    let mut settled = BTreeMap::<RowKey, usize>::new();
    for record in records {
        let lash_core::TraceEvent::Custom { name, payload } = &record.event else {
            continue;
        };
        let (counts, fields): (&mut BTreeMap<RowKey, usize>, &[&str]) = match name.as_str() {
            "ingress.admitted" => (&mut admitted, &["input_ids", "batch_ids"]),
            "ingress.settled" => (
                &mut settled,
                &["input_ids", "batch_ids", "released", "dropped"],
            ),
            _ => continue,
        };
        let Some(root) = payload.get("root").and_then(Value::as_str) else {
            return OracleVerdict::failed(
                LOGICAL_TURN_ADMISSION_EXACTLY_ONCE_ORACLE,
                format!("{name} trace omitted root"),
            );
        };
        for row in row_ids(payload, fields) {
            *counts.entry((root.to_string(), row)).or_default() += 1;
        }
    }
    if admitted.is_empty() {
        return OracleVerdict::failed(
            LOGICAL_TURN_ADMISSION_EXACTLY_ONCE_ORACLE,
            "no admitted ingress was observed",
        );
    }
    for (row, admission_count) in &admitted {
        let settlement_count = settled.get(row).copied().unwrap_or_default();
        if *admission_count != 1 || settlement_count != 1 {
            return OracleVerdict::failed(
                LOGICAL_TURN_ADMISSION_EXACTLY_ONCE_ORACLE,
                format!(
                    "row `{}` of root `{}` was admitted {admission_count} times and settled \
                     {settlement_count} times",
                    row.1, row.0
                ),
            );
        }
    }
    if let Some((row, count)) = settled
        .iter()
        .find(|(row, count)| !admitted.contains_key(*row) || **count != 1)
    {
        return OracleVerdict::failed(
            LOGICAL_TURN_ADMISSION_EXACTLY_ONCE_ORACLE,
            format!(
                "row `{}` of root `{}` had {count} settlements without one matching admission",
                row.1, row.0
            ),
        );
    }
    OracleVerdict::passed(
        LOGICAL_TURN_ADMISSION_EXACTLY_ONCE_ORACLE,
        format!("{} admitted rows settled exactly once", admitted.len()),
    )
}

pub fn frame_switch_follow_on_is_atomic(
    observations: &[FrameSwitchCommitObservation],
) -> OracleVerdict {
    if observations.is_empty() {
        return OracleVerdict::failed(
            FRAME_SWITCH_FOLLOW_ON_ATOMICITY_ORACLE,
            "no admitted frame-switch commit was observed",
        );
    }
    if let Some(observation) = observations
        .iter()
        .find(|observation| !observation.inbound_admission_completed || !observation.follow_on_owed)
    {
        return OracleVerdict::failed(
            FRAME_SWITCH_FOLLOW_ON_ATOMICITY_ORACLE,
            format!(
                "switch commit `{}` exposed inbound_completed={} follow_on_owed={}",
                observation.turn_id,
                observation.inbound_admission_completed,
                observation.follow_on_owed
            ),
        );
    }
    OracleVerdict::passed(
        FRAME_SWITCH_FOLLOW_ON_ATOMICITY_ORACLE,
        format!(
            "{} switch commits exposed admission completion and the owed follow-on together",
            observations.len()
        ),
    )
}

pub fn frame_switch_follow_on_precedes_pending(
    completion_order: &[String],
    follow_on: &str,
    pending_at_chain_start: &[String],
) -> OracleVerdict {
    let Some(follow_index) = completion_order.iter().position(|item| item == follow_on) else {
        return OracleVerdict::failed(
            FRAME_SWITCH_ORDERING_ORACLE,
            format!("follow-on `{follow_on}` never completed"),
        );
    };
    for pending in pending_at_chain_start {
        let Some(pending_index) = completion_order.iter().position(|item| item == pending) else {
            return OracleVerdict::failed(
                FRAME_SWITCH_ORDERING_ORACLE,
                format!("pending item `{pending}` never completed"),
            );
        };
        if pending_index < follow_index {
            return OracleVerdict::failed(
                FRAME_SWITCH_ORDERING_ORACLE,
                format!("pending item `{pending}` completed before follow-on `{follow_on}`"),
            );
        }
    }
    OracleVerdict::passed(
        FRAME_SWITCH_ORDERING_ORACLE,
        format!(
            "follow-on `{follow_on}` completed before {} pre-existing queued items",
            pending_at_chain_start.len()
        ),
    )
}

pub(super) fn is_suspend_resume(event: &DeliveredBoundary) -> bool {
    event
        .payload
        .get("suspend_resume")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Assert that every generated suspend turn genuinely parked mid-flight and was
/// resumed only by the scheduler-delivered completion boundary — never ran
/// synchronously. This generalizes the fixed pending-tool proof into the live
/// generated search. Workloads with no suspend boundary (e.g. minimized
/// fixtures) pass vacuously.
pub fn generated_suspend_resume(events: &[DeliveredBoundary]) -> OracleVerdict {
    let mut checked = 0usize;
    for event in events.iter().filter(|event| is_suspend_resume(event)) {
        let Some(suspend) = event.observed.get("runtime_suspend") else {
            return OracleVerdict::failed(
                GENERATED_SUSPEND_RESUME_ORACLE,
                format!(
                    "suspend resume `{}` recorded no runtime_suspend evidence",
                    event.boundary_id
                ),
            );
        };
        let suspended_before = suspend
            .get("turn_suspended_before_completion")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let scheduler_delivered = suspend
            .get("scheduler_delivered_completion")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let resolve_accepted = suspend
            .get("resolve_accepted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let resumed_after = suspend
            .get("resumed_after_completion")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let before = suspend
            .get("completed_event_count_before_resolution")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX);
        let after = suspend
            .get("completed_event_count_after_resolution")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if !(suspended_before
            && scheduler_delivered
            && resolve_accepted
            && resumed_after
            && before == 0
            && after > before)
        {
            return OracleVerdict::failed(
                GENERATED_SUSPEND_RESUME_ORACLE,
                format!(
                    "suspend `{}` ({}) did not park-then-resume: suspended_before={suspended_before} scheduler_delivered={scheduler_delivered} resolve_accepted={resolve_accepted} resumed_after={resumed_after} completed_before={before} completed_after={after}",
                    event.boundary_id,
                    suspend
                        .get("suspend_kind")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                ),
            );
        }
        checked += 1;
    }
    if checked == 0 {
        // Anti-vacuity: every generated workload plants suspend sessions, so a run
        // with zero suspend-resume boundaries means the class was dropped — the
        // oracle must fail rather than pass on an absent class.
        return OracleVerdict::failed(
            GENERATED_SUSPEND_RESUME_ORACLE,
            "no suspend-resume boundary was observed; the suspend/resume class is absent",
        );
    }
    OracleVerdict::passed(
        GENERATED_SUSPEND_RESUME_ORACLE,
        format!(
            "{checked} generated suspend turn(s) parked mid-flight and resumed only after a scheduler-delivered completion"
        ),
    )
}

/// Run the final-value (semantic-channel / distinct-from-transcript) invariant
/// on every generated provider turn, not just the fixed RLM proof. Generated
/// turns are assistant-message turns, so the invariant asserts the dual of the
/// proof: the assistant prose was NOT mis-projected as a semantic final value —
/// `outcome_kind` is `assistant_message`, no `semantic_value` leaked, and no
/// terminal FinalValue/ToolValue event was emitted. A turn that smuggled a
/// final value through transcript inference would fail.
/// Reject a universally-quantified law that was evaluated over fewer
/// observations than the workload declared.
///
/// Every law in this module is vacuously true over an empty set, so a broken
/// generator, delivery path or projection that produces nothing would otherwise
/// read as compliance — and the further upstream the break, the more oracles go
/// green together. Comparing against the workload's *declared* count instead of
/// an ad hoc `is_empty()` guard turns that into a provable statement: "the
/// workload declared 5 sessions and the law saw 0".
/// The declared session count, or zero when the law is being evaluated as a
/// scenario evidence predicate with no workload declaration in scope.
pub(super) fn declared_session_count(expectations: Option<&WorkloadExpectations>) -> usize {
    expectations.map_or(0, WorkloadExpectations::session_count)
}

pub(super) fn declared_coverage_shortfall(
    oracle_id: &'static str,
    observation_class: &str,
    declared: usize,
    observed: usize,
) -> Option<OracleVerdict> {
    (observed < declared).then(|| {
        OracleVerdict::failed(
            oracle_id,
            format!(
                "workload declared {declared} {observation_class} but the law was evaluated over {observed}; the declared observation class is absent or incomplete"
            ),
        )
    })
}

pub fn generated_final_value_semantic_channel(
    events: &[DeliveredBoundary],
    expectations: &WorkloadExpectations,
) -> OracleVerdict {
    let mut checked = 0usize;
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
    {
        let Some(facts) = event.observed.get("runtime_final_value_facts") else {
            return OracleVerdict::failed(
                GENERATED_FINAL_VALUE_ORACLE,
                format!(
                    "provider turn `{}` recorded no runtime_final_value_facts",
                    event.boundary_id
                ),
            );
        };
        let outcome_kind = facts
            .get("outcome_kind")
            .and_then(Value::as_str)
            .unwrap_or("");
        let has_semantic_value = facts
            .get("semantic_value")
            .map(|value| !value.is_null())
            .unwrap_or(false);
        let terminal_event_count = facts
            .get("terminal_event_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let semantic_channel_observed = facts
            .get("semantic_channel_observed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if outcome_kind != "assistant_message"
            || has_semantic_value
            || terminal_event_count != 0
            || semantic_channel_observed
        {
            return OracleVerdict::failed(
                GENERATED_FINAL_VALUE_ORACLE,
                format!(
                    "provider turn `{}` leaked a semantic final value: outcome_kind={outcome_kind} has_semantic_value={has_semantic_value} terminal_events={terminal_event_count} semantic_channel_observed={semantic_channel_observed}",
                    event.boundary_id
                ),
            );
        }
        checked += 1;
    }
    if let Some(shortfall) = declared_coverage_shortfall(
        GENERATED_FINAL_VALUE_ORACLE,
        "provider turn(s)",
        expectations.provider_turn_count,
        checked,
    ) {
        return shortfall;
    }
    OracleVerdict::passed(
        GENERATED_FINAL_VALUE_ORACLE,
        format!(
            "{checked} generated assistant-message turn(s) (workload declared {}) kept the semantic final-value channel empty (no transcript-inferred final values)",
            expectations.provider_turn_count
        ),
    )
}

#[cfg(test)]
mod admission_identity_tests {
    use super::*;

    fn record(name: &str, payload: Value) -> lash_core::facade_support::TraceRecord {
        lash_core::facade_support::TraceRecord {
            schema_version: 1,
            id: format!("{name}-record"),
            timestamp: chrono::DateTime::<chrono::Utc>::UNIX_EPOCH,
            context: lash_core::TraceContext::default(),
            event: lash_core::TraceEvent::Custom {
                name: name.to_string(),
                payload,
            },
        }
    }

    fn admit_and_settle(root: &str, batch: &str) -> [lash_core::facade_support::TraceRecord; 2] {
        [
            record(
                "ingress.admitted",
                serde_json::json!({"root": root, "admitted_by": "admit", "batch_ids": [batch]}),
            ),
            record(
                "ingress.settled",
                serde_json::json!({"root": root, "batch_ids": [batch]}),
            ),
        ]
    }

    /// A row one root released and another admitted is two admissions, each
    /// settled once.
    #[test]
    fn a_row_readmitted_by_another_root_is_a_distinct_admission() {
        let mut records = admit_and_settle("root-a", "qwb:a").to_vec();
        records.push(record(
            "ingress.admitted",
            serde_json::json!({"root": "root-b", "admitted_by": "admit", "batch_ids": ["qwb:b"]}),
        ));
        records.push(record(
            "ingress.settled",
            serde_json::json!({"root": "root-b", "released": ["qwb:b"]}),
        ));
        let verdict = logical_turn_admissions_settle_exactly_once(&records);
        assert!(verdict.is_passed(), "{verdict:?}");
    }

    /// The same row admitted and settled twice under one root is a double
    /// settlement.
    #[test]
    fn the_same_row_settled_twice_under_one_root_is_refused() {
        let records = [
            admit_and_settle("root-a", "qwb:a"),
            admit_and_settle("root-a", "qwb:a"),
        ]
        .concat();
        let verdict = logical_turn_admissions_settle_exactly_once(&records);
        assert!(!verdict.is_passed(), "{verdict:?}");
    }
}
