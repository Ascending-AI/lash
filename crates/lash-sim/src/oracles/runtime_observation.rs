use super::*;

pub fn cross_session_isolation(summary: &AbstractWorldSummary) -> OracleVerdict {
    if summary.session_count < 2 {
        return OracleVerdict::failed(
            CROSS_SESSION_ISOLATION_ORACLE,
            "workload did not contain at least two sessions",
        );
    }
    let aliases = summary
        .sessions
        .iter()
        .map(|session| session.alias.as_str())
        .collect::<BTreeSet<_>>();
    for session in &summary.sessions {
        if !session.opened {
            return OracleVerdict::failed(
                CROSS_SESSION_ISOLATION_ORACLE,
                format!("session `{}` was never opened", session.alias),
            );
        }
        for other_alias in aliases
            .iter()
            .copied()
            .filter(|alias| *alias != session.alias)
        {
            let leaked_provider = session
                .provider_turns
                .iter()
                .any(|turn| turn.output.contains(other_alias));
            let leaked_tool = session
                .tool_outputs
                .iter()
                .any(|output| output.contains(other_alias));
            if leaked_provider || leaked_tool {
                return OracleVerdict::failed(
                    CROSS_SESSION_ISOLATION_ORACLE,
                    format!(
                        "session `{}` observed output from `{other_alias}`",
                        session.alias
                    ),
                );
            }
        }
    }
    OracleVerdict::passed(
        CROSS_SESSION_ISOLATION_ORACLE,
        "all generated session outputs stayed scoped to their session alias",
    )
}

pub fn ingress_sessions_opened(
    summary: &AbstractWorldSummary,
    expectations: &WorkloadExpectations,
) -> OracleVerdict {
    if let Some(shortfall) = declared_coverage_shortfall(
        INGRESS_SESSION_OPENED_ORACLE,
        "session(s)",
        expectations.session_count(),
        summary.sessions.len(),
    ) {
        return shortfall;
    }
    for session in &summary.sessions {
        if !session.opened || session.ingress_count != 1 {
            return OracleVerdict::failed(
                INGRESS_SESSION_OPENED_ORACLE,
                format!(
                    "session `{}` expected exactly one ingress opening, got opened={} ingress_count={}",
                    session.alias, session.opened, session.ingress_count
                ),
            );
        }
    }
    OracleVerdict::passed(
        INGRESS_SESSION_OPENED_ORACLE,
        format!(
            "each of {} observed generated session(s) (workload declared {}) opened through an ingress boundary exactly once",
            summary.sessions.len(),
            expectations.session_count()
        ),
    )
}

pub fn observer_convergence(
    summary: &AbstractWorldSummary,
    expectations: &WorkloadExpectations,
) -> OracleVerdict {
    if let Some(shortfall) = declared_coverage_shortfall(
        OBSERVER_CONVERGENCE_ORACLE,
        "session(s) whose observer must converge",
        expectations.session_count(),
        summary.sessions.len(),
    ) {
        return shortfall;
    }
    observer_convergence_law(summary, Some(expectations))
}

/// The convergence law itself, without the declared-coverage floor. Scenario
/// evidence predicates use this: they ask whether the sessions they *did*
/// observe converged, and the workload-level coverage floor is the oracle's job.
pub(super) fn observer_convergence_law(
    summary: &AbstractWorldSummary,
    expectations: Option<&WorkloadExpectations>,
) -> OracleVerdict {
    for session in &summary.sessions {
        let expected_turns = session.provider_turns.len();
        let Some(last_observed_turn) = session.observer_turn_indices.last().copied() else {
            return OracleVerdict::failed(
                OBSERVER_CONVERGENCE_ORACLE,
                format!("session `{}` had no observer snapshot", session.alias),
            );
        };
        if last_observed_turn != expected_turns {
            return OracleVerdict::failed(
                OBSERVER_CONVERGENCE_ORACLE,
                format!(
                    "session `{}` observer saw turn {}, expected {}",
                    session.alias, last_observed_turn, expected_turns
                ),
            );
        }
    }
    OracleVerdict::passed(
        OBSERVER_CONVERGENCE_ORACLE,
        format!(
            "observer snapshots converged to the generated runtime turn count in all {} observed session(s) (workload declared {})",
            summary.sessions.len(),
            declared_session_count(expectations)
        ),
    )
}

/// Build a failing-capable coverage verdict: the boundary kind must be present
/// AND the runtime invariant its reason claims must actually hold in the events.
/// A present-but-broken boundary fails loudly instead of passing on presence.
pub(super) fn coverage_invariant_verdict(
    oracle_id: &'static str,
    present: bool,
    missing_reason: &'static str,
    invariant_holds: bool,
    invariant_failed_reason: &'static str,
    passed_reason: &'static str,
) -> OracleVerdict {
    if !present {
        OracleVerdict::failed(oracle_id, missing_reason)
    } else if !invariant_holds {
        OracleVerdict::failed(oracle_id, invariant_failed_reason)
    } else {
        OracleVerdict::passed(oracle_id, passed_reason)
    }
}

pub fn queued_ingress_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        QUEUED_INGRESS_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| session.queued_ingress_count > 0),
        "no queued ingress boundary was observed",
        queued_ingress_has_source_keys(events),
        "queued ingress boundary was observed but it lacked a stable payload/observed source key",
        "generated workload queued turn input through an explicit ingress boundary carrying a stable source key",
    )
}

pub fn cancellation_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        CANCELLATION_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| session.cancellation_count > 0),
        "no cancellation boundary was observed",
        cancellation_terminalizes_pending_input(events),
        "cancellation boundary was observed but it did not terminalize the pending queued input",
        "generated workload cancelled a pending boundary and terminalized its queued input",
    )
}

pub fn trigger_delivery_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        TRIGGER_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| session.trigger_count > 0),
        "no trigger boundary was observed",
        trigger_delivery_runtime_observed(events),
        "trigger boundary was observed but it did not route through a runtime trigger DTO with a stable source identity",
        "generated workload delivered a trigger boundary routed through a runtime trigger DTO with stable source identity",
    )
}

pub fn observer_reconnect_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        OBSERVER_RECONNECT_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| session.observer_reconnects > 0),
        "no observer reconnect boundary was observed",
        observer_reconnect_has_matching_turn(events, summary),
        "observer reconnect boundary was observed but its replayed snapshot did not converge to the session's final provider turn",
        "observer reconnect boundary converged on the same session state (final provider turn)",
    )
}

pub fn backend_failure_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        BACKEND_FAILURE_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| session.backend_failure_count > 0),
        "no backend failure boundary was observed",
        backend_retry_terminalization_semantics(events),
        "backend failure boundary was observed but it did not terminalize through the retry/terminalization path",
        "generated workload injected a backend failure boundary that terminalized through the retry path",
    )
}

pub fn provider_mutation_rejected(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        PROVIDER_MUTATION_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| session.provider_mutation_count > 0),
        "no provider/script mutation boundary was observed",
        provider_mutation_parser_matrix_observed(events),
        "provider/script mutation boundary was observed but it did not run through the real provider parser matrix",
        "provider/script mutation boundary was rejected through the real provider parser matrix without changing runtime state",
    )
}

pub fn generated_runtime_provider_matrix(events: &[DeliveredBoundary]) -> OracleVerdict {
    let expected = MIGRATED_RUNTIME_PROVIDER_KINDS
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut observed = BTreeSet::new();
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
    {
        let payload_kind = event
            .payload
            .get("provider_kind")
            .and_then(Value::as_str)
            .unwrap_or("");
        let observed_kind = event
            .observed
            .get("provider_kind")
            .and_then(Value::as_str)
            .unwrap_or("");
        if payload_kind.is_empty() || observed_kind.is_empty() || payload_kind != observed_kind {
            return OracleVerdict::failed(
                GENERATED_PROVIDER_MATRIX_ORACLE,
                format!(
                    "provider event `{}` payload kind `{payload_kind}` did not match observed kind `{observed_kind}`",
                    event.boundary_id
                ),
            );
        }
        observed.insert(observed_kind);
    }
    if observed != expected {
        return OracleVerdict::failed(
            GENERATED_PROVIDER_MATRIX_ORACLE,
            format!(
                "generated runtime provider kinds {:?} did not cover {:?}",
                observed, expected
            ),
        );
    }
    OracleVerdict::passed(
        GENERATED_PROVIDER_MATRIX_ORACLE,
        "generated runtime turns covered OpenAI-compatible, direct OpenAI, Anthropic, and Google provider crates through ScriptedLlmHttpTransport",
    )
}

/// Peak number of provider turns that were simultaneously live during the run,
/// derived purely from the delivered boundary stream so the value is identical
/// on the generation path and on cross-backend replay.
///
/// A provider turn is "live" in the delivered event stream from the first
/// `ProviderEvent` released for its turn (the turn future is parked on the
/// scripted transport gate at that point) until its `Provider` completion
/// boundary is delivered. The number of distinct turns simultaneously in that
/// window is the interleaving depth; its maximum is the highwater.
pub fn peak_concurrent_live_turns(events: &[DeliveredBoundary]) -> usize {
    let mut live = BTreeSet::<String>::new();
    let mut peak = 0usize;
    for event in events {
        match event.kind {
            BoundaryKind::ProviderEvent => {
                if let Some(turn_boundary_id) = event
                    .payload
                    .get("turn_boundary_id")
                    .and_then(Value::as_str)
                {
                    live.insert(turn_boundary_id.to_string());
                    peak = peak.max(live.len());
                }
            }
            BoundaryKind::Provider => {
                live.remove(&event.boundary_id);
            }
            _ => {}
        }
    }
    peak
}

/// Count of distinct sessions that ran at least one provider turn. Interleaving
/// is structurally impossible below two such sessions, but the exemption is
/// decided by the *declared* session count — this number only proves the
/// declared sessions actually ran.
pub(super) fn provider_turn_session_count(events: &[DeliveredBoundary]) -> usize {
    events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
        .map(|event| event.actor_alias.as_str())
        .collect::<BTreeSet<_>>()
        .len()
}

/// Make interleaving load-bearing: whenever a workload *declares* provider turns
/// in at least two sessions, the scheduler must actually drive at least two of
/// those turns concurrently. A multi-session workload that never interleaves is
/// a real scheduling regression and fails this oracle.
///
/// The single-session exemption is proved from the declared workload, never
/// inferred from the observed sessions: "the workload declared one session" and
/// "the run produced no sessions at all" are opposite facts, and conflating them
/// is what let this oracle pass on an empty event set. A workload that declared
/// two or more sessions but ran provider turns in fewer fails as a coverage
/// shortfall before interleaving is even considered.
pub fn provider_turn_interleaving_depth(
    events: &[DeliveredBoundary],
    expectations: &WorkloadExpectations,
) -> OracleVerdict {
    let peak = peak_concurrent_live_turns(events);
    let sessions = provider_turn_session_count(events);
    let declared_sessions = expectations.session_count();
    if let Some(shortfall) = declared_coverage_shortfall(
        PROVIDER_TURN_INTERLEAVING_ORACLE,
        "session(s) running provider turns",
        declared_sessions,
        sessions,
    ) {
        return shortfall;
    }
    if declared_sessions < 2 {
        return OracleVerdict::passed(
            PROVIDER_TURN_INTERLEAVING_ORACLE,
            format!(
                "interleaving does not apply: the workload declared {declared_sessions} session(s) and {sessions} ran provider turns (peak concurrent live turns {peak})"
            ),
        );
    }
    if peak >= 2 {
        OracleVerdict::passed(
            PROVIDER_TURN_INTERLEAVING_ORACLE,
            format!(
                "scheduler drove {peak} provider turns concurrently across {sessions} sessions (workload declared {declared_sessions})"
            ),
        )
    } else {
        OracleVerdict::failed(
            PROVIDER_TURN_INTERLEAVING_ORACLE,
            format!(
                "{sessions} sessions ran provider turns but peak concurrent live turns was {peak}; the scheduler never interleaved live provider turns"
            ),
        )
    }
}

/// Each generator-driven transport/HTTP mutation must land on its own named
/// failure path through the real provider: a mid-stream disconnect classifies
/// as a retryable stream fault, response-start and chunk timeouts as retryable
/// timeouts, and a 5xx as a retryable HTTP status carrying its status code.
/// This proves the new mutation classes drive distinct, executable behaviors
/// rather than collapsing into a single generic parser error. A workload that
/// declared no transport mutation imposes no floor; one that declared them and
/// delivered fewer fails, so a mutation-delivery break can no longer read as a
/// clean classification.
pub fn provider_transport_mutation_classified(
    events: &[DeliveredBoundary],
    expectations: &WorkloadExpectations,
) -> OracleVerdict {
    let mut observed_classes = BTreeSet::new();
    let mut classified_mutations = 0usize;
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::ProviderMutation)
    {
        let Some(mutation) = event
            .observed
            .get("mutation")
            .or_else(|| event.payload.get("mutation"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if !is_transport_provider_mutation(mutation) {
            continue;
        }
        let Some(proof) = event
            .observed
            .pointer("/provider_parser_matrix/matrix/proofs")
            .and_then(Value::as_array)
            .and_then(|proofs| proofs.first())
        else {
            return OracleVerdict::failed(
                PROVIDER_TRANSPORT_MUTATION_ORACLE,
                format!(
                    "transport mutation `{mutation}` on `{}` recorded no real provider parser proof",
                    event.boundary_id
                ),
            );
        };
        let raw_kind = proof.get("kind").and_then(Value::as_str).unwrap_or("");
        let retryable = proof
            .pointer("/classification/retryable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let status = proof.get("status").and_then(Value::as_u64);
        let classified = match mutation {
            "mid_stream_disconnect" => raw_kind == "Stream" && retryable,
            "response_start_timeout" | "stream_chunk_timeout" => raw_kind == "Timeout" && retryable,
            "retryable_server_error_sequence" => status == Some(503) && retryable,
            _ => false,
        };
        if !classified {
            return OracleVerdict::failed(
                PROVIDER_TRANSPORT_MUTATION_ORACLE,
                format!(
                    "transport mutation `{mutation}` on `{}` classified incorrectly: raw_kind={raw_kind} retryable={retryable} status={status:?}",
                    event.boundary_id
                ),
            );
        }
        observed_classes.insert(mutation.to_string());
        classified_mutations += 1;
    }
    if let Some(shortfall) = declared_coverage_shortfall(
        PROVIDER_TRANSPORT_MUTATION_ORACLE,
        "transport mutation boundary(ies)",
        expectations.transport_mutation_count,
        classified_mutations,
    ) {
        return shortfall;
    }
    OracleVerdict::passed(
        PROVIDER_TRANSPORT_MUTATION_ORACLE,
        format!(
            "{classified_mutations} transport mutation boundary(ies) (workload declared {}) classified on distinct failure paths: {observed_classes:?}",
            expectations.transport_mutation_count
        ),
    )
}

pub fn process_wake_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        PROCESS_WAKE_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| session.process_wake_count > 0),
        "no process wake boundary was observed",
        process_wake_runtime_dto_observed(events),
        "process wake boundary was observed but it did not materialize a runtime DTO",
        "process wake boundary materialized a runtime DTO",
    )
}

/// Assert across the whole generated schedule that one logical process wake
/// materializes into at most one runtime turn. The harness records a stable
/// `runtime_turn_id` when the real queued-work claim materializes a wake;
/// grouping those ids by durable source key makes duplicate materialization
/// visible even when the duplicate boundary is later settled or deduplicated.
pub fn process_wake_at_most_once(events: &[DeliveredBoundary]) -> OracleVerdict {
    let mut turns_by_wake = BTreeMap::<String, BTreeSet<String>>::new();
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::ProcessWake)
    {
        let Some(queued_work) = event.observed.get("runtime_queued_work") else {
            continue;
        };
        if queued_work.get("enqueued").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let Some(source_key) = queued_work.get("source_key").and_then(Value::as_str) else {
            return OracleVerdict::failed(
                PROCESS_WAKE_AT_MOST_ONCE_ORACLE,
                format!(
                    "enqueued process wake `{}` recorded no queued-work source key",
                    event.boundary_id
                ),
            );
        };
        let turns = turns_by_wake.entry(source_key.to_string()).or_default();
        if queued_work.get("claimed").and_then(Value::as_bool) == Some(true) {
            let Some(turn_id) = queued_work.get("runtime_turn_id").and_then(Value::as_str) else {
                return OracleVerdict::failed(
                    PROCESS_WAKE_AT_MOST_ONCE_ORACLE,
                    format!(
                        "claimed process wake `{}` recorded no runtime-turn materialization id",
                        event.boundary_id
                    ),
                );
            };
            turns.insert(turn_id.to_string());
            if turns.len() > 1 {
                return OracleVerdict::failed(
                    PROCESS_WAKE_AT_MOST_ONCE_ORACLE,
                    format!(
                        "process wake `{source_key}` materialized into {} runtime turns: {:?}",
                        turns.len(),
                        turns
                    ),
                );
            }
        }
    }
    if turns_by_wake.is_empty() {
        return OracleVerdict::failed(
            PROCESS_WAKE_AT_MOST_ONCE_ORACLE,
            "generated schedule recorded no enqueued process wakes",
        );
    }
    let materialized = turns_by_wake
        .values()
        .filter(|turns| !turns.is_empty())
        .count();
    OracleVerdict::passed(
        PROCESS_WAKE_AT_MOST_ONCE_ORACLE,
        format!(
            "{} enqueued logical process wake(s) materialized into at most one runtime turn each ({materialized} materialized)",
            turns_by_wake.len()
        ),
    )
}

pub fn exec_code_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        EXEC_CODE_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| !session.exec_code_outputs.is_empty()),
        "no exec-code boundary was observed",
        exec_runtime_outcome_observed(events),
        "exec-code boundary was observed but it did not produce a captured RuntimeEffectOutcome",
        "exec-code boundary produced a captured RuntimeEffectOutcome result",
    )
}

pub fn tool_boundary_observed(
    summary: &AbstractWorldSummary,
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    coverage_invariant_verdict(
        TOOL_BOUNDARY_ORACLE,
        summary
            .sessions
            .iter()
            .any(|session| !session.tool_outputs.is_empty()),
        "no tool boundary was observed",
        tool_runtime_output_observed(events),
        "tool boundary was observed but it did not produce a captured runtime tool-output DTO",
        "tool boundary produced a captured runtime tool-output DTO",
    )
}

pub fn runtime_session_graph_contract(
    summary: &AbstractWorldSummary,
    expectations: &WorkloadExpectations,
) -> OracleVerdict {
    if let Some(shortfall) = declared_coverage_shortfall(
        RUNTIME_SESSION_GRAPH_ORACLE,
        "runtime-backed session(s)",
        expectations.session_count(),
        summary.sessions.len(),
    ) {
        return shortfall;
    }
    runtime_session_graph_law(summary, Some(expectations))
}

/// The session-graph advancement law without the declared-coverage floor, for
/// scenario evidence predicates that only judge the sessions they observed.
pub(super) fn runtime_session_graph_law(
    summary: &AbstractWorldSummary,
    expectations: Option<&WorkloadExpectations>,
) -> OracleVerdict {
    for session in &summary.sessions {
        if session.provider_turns.len() < 2 {
            return OracleVerdict::failed(
                RUNTIME_SESSION_GRAPH_ORACLE,
                format!(
                    "session `{}` ran only {} provider turns",
                    session.alias,
                    session.provider_turns.len()
                ),
            );
        }
        for (index, turn) in session.provider_turns.iter().enumerate() {
            let turn_index = index as u64 + 1;
            let expected_exchange_count = turn_index;
            if turn.exchange_count != Some(expected_exchange_count) {
                return OracleVerdict::failed(
                    RUNTIME_SESSION_GRAPH_ORACLE,
                    format!(
                        "session `{}` turn {turn_index} provider exchanges did not converge to {expected_exchange_count}",
                        session.alias
                    ),
                );
            }
            let expected_graph_min_count = turn_index * 2 + 1;
            if turn.graph_node_count.unwrap_or(0) < expected_graph_min_count {
                return OracleVerdict::failed(
                    RUNTIME_SESSION_GRAPH_ORACLE,
                    format!(
                        "session `{}` turn {turn_index} graph had fewer than {expected_graph_min_count} nodes",
                        session.alias
                    ),
                );
            }
            let expected_transcript_min_count = turn_index * 2;
            if turn.transcript_message_count.unwrap_or(0) < expected_transcript_min_count {
                return OracleVerdict::failed(
                    RUNTIME_SESSION_GRAPH_ORACLE,
                    format!(
                        "session `{}` turn {turn_index} transcript had fewer than {expected_transcript_min_count} messages",
                        session.alias
                    ),
                );
            }
            if !turn.output.contains(&session.alias) {
                return OracleVerdict::failed(
                    RUNTIME_SESSION_GRAPH_ORACLE,
                    format!(
                        "session `{}` turn {turn_index} provider output did not identify its session",
                        session.alias
                    ),
                );
            }
        }
    }
    OracleVerdict::passed(
        RUNTIME_SESSION_GRAPH_ORACLE,
        format!(
            "all {} observed runtime-backed generated session(s) (workload declared {}) advanced provider exchanges, graph nodes, and transcript messages across multiple turns",
            summary.sessions.len(),
            declared_session_count(expectations)
        ),
    )
}

#[cfg(test)]
pub(super) fn runtime_graph_projection_acyclic(events: &[DeliveredBoundary]) -> OracleVerdict {
    let mut checked = 0;
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
    {
        let Some(facts) = runtime_observed_fact::<RuntimeGraphInvariantFacts>(event, "graph")
        else {
            return OracleVerdict::failed(
                RUNTIME_GRAPH_ACYCLIC_ORACLE,
                format!(
                    "provider boundary `{}` did not expose real graph invariant facts",
                    event.boundary_id
                ),
            );
        };
        checked += 1;
        if !facts.passed {
            return OracleVerdict::failed(
                RUNTIME_GRAPH_ACYCLIC_ORACLE,
                format!(
                    "provider boundary `{}` observed graph duplicates={:?} missing_parents={:?} cycles={:?} leaf_exists={}",
                    event.boundary_id,
                    facts.duplicate_node_ids,
                    facts.missing_parent_links,
                    facts.cycle_node_ids,
                    facts.leaf_exists
                ),
            );
        }
    }
    if checked == 0 {
        return OracleVerdict::failed(
            RUNTIME_GRAPH_ACYCLIC_ORACLE,
            "no provider turn exposed runtime graph invariant facts",
        );
    }
    OracleVerdict::passed(
        RUNTIME_GRAPH_ACYCLIC_ORACLE,
        format!(
            "{checked} real provider turn graphs had unique nodes, valid parents, and no cycles"
        ),
    )
}

/// Validate graph integrity from accepted checkpoint raw rows. This deliberately
/// bypasses `SessionGraph` and its read-model projection so duplicate durable
/// rows cannot disappear before the oracle observes them.
pub fn runtime_graph_acyclic(writes: &[crate::store::CheckpointWriteEvent]) -> OracleVerdict {
    let mut checked = 0usize;
    for write in writes
        .iter()
        .filter(|write| write.cause_boundary_id.is_none())
    {
        let Some(state) = &write.state else {
            // Promoted v1/v2 fixtures predate accepted raw-row observations.
            continue;
        };
        let session_id = write.attributed_session();
        let Some(raw) = state.accepted_raw_rows.as_ref() else {
            return OracleVerdict::failed(
                RUNTIME_GRAPH_ACYCLIC_ORACLE,
                format!(
                    "session `{session_id}` commit {} exposed no accepted raw rows",
                    write.commit_index
                ),
            );
        };
        let Some(rows) = raw.get("graph_nodes").and_then(Value::as_array) else {
            return OracleVerdict::failed(
                RUNTIME_GRAPH_ACYCLIC_ORACLE,
                format!(
                    "session `{session_id}` commit {} exposed no raw graph rows",
                    write.commit_index
                ),
            );
        };
        if let Err(message) = validate_raw_graph_rows(
            rows,
            raw.get("graph_leaf_node_id").and_then(Value::as_str),
            &SessionId::from(session_id),
            write.commit_index,
        ) {
            return OracleVerdict::failed(RUNTIME_GRAPH_ACYCLIC_ORACLE, message);
        }
        checked += 1;
    }
    if checked == 0 {
        return OracleVerdict::failed(
            RUNTIME_GRAPH_ACYCLIC_ORACLE,
            "no checkpoint commit exposed accepted raw graph rows",
        );
    }
    OracleVerdict::passed(
        RUNTIME_GRAPH_ACYCLIC_ORACLE,
        format!(
            "{checked} accepted raw graph snapshots had unique rows, valid parents, and no cycles"
        ),
    )
}

pub(super) fn validate_raw_graph_rows(
    rows: &[Value],
    leaf_node_id: Option<&str>,
    session_id: &SessionId,
    commit_index: usize,
) -> Result<(), String> {
    let context = format!("session `{session_id}` commit {commit_index}");
    let mut seen = BTreeSet::new();
    let mut parents = BTreeMap::<String, Option<String>>::new();
    for row in rows {
        let node_id = row
            .get("node_id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{context} raw graph row has no node id"))?;
        if !seen.insert(node_id.to_string()) {
            return Err(format!(
                "{context} accepted raw graph contains duplicate row `{node_id}`"
            ));
        }
        parents.insert(
            node_id.to_string(),
            row.get("parent_node_id")
                .and_then(Value::as_str)
                .map(ToString::to_string),
        );
    }
    for (node_id, parent) in &parents {
        if let Some(parent) = parent
            && !parents.contains_key(parent)
        {
            return Err(format!(
                "{context} raw graph row `{node_id}` references missing parent `{parent}`"
            ));
        }
    }
    for start in parents.keys() {
        let mut path = BTreeSet::new();
        let mut current = Some(start.as_str());
        while let Some(node_id) = current {
            if !path.insert(node_id.to_string()) {
                return Err(format!(
                    "{context} raw graph contains a cycle through `{node_id}`"
                ));
            }
            current = parents.get(node_id).and_then(Option::as_deref);
        }
    }
    if leaf_node_id.is_some_and(|leaf| !parents.contains_key(leaf)) {
        return Err(format!(
            "{context} raw graph leaf `{}` has no row",
            leaf_node_id.unwrap_or_default()
        ));
    }
    Ok(())
}

pub fn runtime_single_active_agent_frame(events: &[DeliveredBoundary]) -> OracleVerdict {
    let mut checked = 0;
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
    {
        let Some(facts) =
            runtime_observed_fact::<RuntimeAgentFrameInvariantFacts>(event, "agent_frame")
        else {
            return OracleVerdict::failed(
                RUNTIME_SINGLE_ACTIVE_AGENT_FRAME_ORACLE,
                format!(
                    "provider boundary `{}` did not expose real Agent Frame facts",
                    event.boundary_id
                ),
            );
        };
        checked += 1;
        if !facts.passed {
            return OracleVerdict::failed(
                RUNTIME_SINGLE_ACTIVE_AGENT_FRAME_ORACLE,
                format!(
                    "provider boundary `{}` observed current_frame=`{}` active={:?} current_exists={} current_active={} unknown_node_frames={:?}",
                    event.boundary_id,
                    facts.current_frame_node_id,
                    facts.active_frame_ids,
                    facts.current_frame_exists,
                    facts.current_frame_active,
                    facts.node_agent_frame_ids_without_record
                ),
            );
        }
    }
    if checked == 0 {
        return OracleVerdict::failed(
            RUNTIME_SINGLE_ACTIVE_AGENT_FRAME_ORACLE,
            "no provider turn exposed runtime Agent Frame facts",
        );
    }
    OracleVerdict::passed(
        RUNTIME_SINGLE_ACTIVE_AGENT_FRAME_ORACLE,
        format!(
            "{checked} real provider turn snapshots had exactly one active current Agent Frame"
        ),
    )
}

pub fn runtime_usage_monotonic(events: &[DeliveredBoundary]) -> OracleVerdict {
    let mut checked = 0;
    let mut last_total_by_session = BTreeMap::<String, i64>::new();
    for event in events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
    {
        let Some(facts) = runtime_observed_fact::<RuntimeUsageInvariantFacts>(event, "usage")
        else {
            return OracleVerdict::failed(
                RUNTIME_USAGE_MONOTONIC_ORACLE,
                format!(
                    "provider boundary `{}` did not expose real usage facts",
                    event.boundary_id
                ),
            );
        };
        checked += 1;
        if !facts.non_negative {
            return OracleVerdict::failed(
                RUNTIME_USAGE_MONOTONIC_ORACLE,
                format!(
                    "provider boundary `{}` observed negative token fields {:?}",
                    event.boundary_id, facts.negative_fields
                ),
            );
        }
        if !facts.usage_events_monotonic {
            return OracleVerdict::failed(
                RUNTIME_USAGE_MONOTONIC_ORACLE,
                format!(
                    "provider boundary `{}` emitted non-monotonic usage activity cumulatives",
                    event.boundary_id
                ),
            );
        }
        let current = facts.token_ledger_total.total_tokens;
        if let Some(previous) = last_total_by_session.insert(event.actor_alias.clone(), current)
            && current < previous
        {
            return OracleVerdict::failed(
                RUNTIME_USAGE_MONOTONIC_ORACLE,
                format!(
                    "session `{}` cumulative token ledger total regressed from {previous} to {current} at `{}`",
                    event.actor_alias, event.boundary_id
                ),
            );
        }
    }
    if checked == 0 {
        return OracleVerdict::failed(
            RUNTIME_USAGE_MONOTONIC_ORACLE,
            "no provider turn exposed runtime usage facts",
        );
    }
    OracleVerdict::passed(
        RUNTIME_USAGE_MONOTONIC_ORACLE,
        format!(
            "{checked} real provider turns had non-negative usage and non-decreasing session ledger totals"
        ),
    )
}
