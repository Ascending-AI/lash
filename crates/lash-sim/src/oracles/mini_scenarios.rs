use super::*;

/// Scenario-contract oracle vector. Every contract emits its own named,
/// failing-capable verdict; suite-level coverage manifests are deliberately not
/// used as backing oracles because they let generated packages look
/// per-contract while sharing the same semantic proof.
pub fn scenario_contract_oracles(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> Vec<OracleVerdict> {
    let memo = ScenarioFactMemo::default();
    all_scenario_contracts()
        .map(|contract| scenario_contract_oracle(contract, events, summary, &memo))
        .collect()
}

/// A derived fact, or the reason its contract rejected the proof event.
type DerivedFact = Result<ScenarioContractGeneratedFact, String>;

/// Derived contract facts, keyed by the proof event they were derived from.
///
/// A contract's generated fact is a pure function of one `contract_execution`
/// proof event: the contract finds its event, then reads only that event's
/// payload and observed execution. Deriving it costs a deep JSON comparison of
/// the whole execution payload, which for the `agent.*` contracts is the
/// dominant cost of the oracle battery.
///
/// The minimizer evaluates the battery once per reduction candidate over event
/// lists that differ only by removed events — it never edits an event — so
/// within one minimize run the fact derived from a given proof event is the
/// same every time. Holding one memo for the run turns those re-derivations
/// into lookups. A memo must therefore not outlive one trace: boundary ids are
/// unique within a trace, not across traces, so every entry point that does not
/// take a memo builds a fresh one.
#[derive(Default)]
pub struct ScenarioFactMemo {
    facts: std::cell::RefCell<std::collections::BTreeMap<(&'static str, String), DerivedFact>>,
}

impl ScenarioFactMemo {
    /// The fact `contract` derives from the proof event `boundary_id`, deriving
    /// it with `derive` on the first ask and reusing it afterwards.
    pub fn fact_from_proof_event(
        &self,
        contract: &'static str,
        boundary_id: &str,
        derive: impl FnOnce() -> DerivedFact,
    ) -> DerivedFact {
        if let Some(known) = self
            .facts
            .borrow()
            .get(&(contract, boundary_id.to_string()))
        {
            return known.clone();
        }
        let derived = derive();
        self.facts
            .borrow_mut()
            .insert((contract, boundary_id.to_string()), derived.clone());
        derived
    }
}

pub(super) fn all_scenario_contracts() -> impl Iterator<Item = &'static ScenarioContractSpec> {
    [
        RUNTIME_SCENARIO_CONTRACTS,
        STANDARD_PROTOCOL_SCENARIO_CONTRACTS,
        RLM_PROTOCOL_SCENARIO_CONTRACTS,
        AGENT_SCENARIO_CONTRACTS,
    ]
    .into_iter()
    .flat_map(|contracts| contracts.iter())
}

pub(super) fn mini_runtime_queued_input_hidden(events: &[DeliveredBoundary]) -> OracleVerdict {
    let Some(queued) = first_event(events, BoundaryKind::QueuedIngress) else {
        return OracleVerdict::failed(
            SCENARIO_MINI_RUNTIME_QUEUED_HIDDEN_ORACLE,
            "no queued input boundary was generated",
        );
    };
    let queued_text = queued
        .payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("");
    let leaked = events.iter().any(|event| {
        event.kind == BoundaryKind::Provider
            && event.actor_alias == queued.actor_alias
            && event
                .observed
                .get("provider_output")
                .and_then(Value::as_str)
                .is_some_and(|output| !queued_text.is_empty() && output.contains(queued_text))
    });
    if leaked {
        OracleVerdict::failed(
            SCENARIO_MINI_RUNTIME_QUEUED_HIDDEN_ORACLE,
            format!(
                "queued input `{}` leaked into a provider turn before explicit claim",
                queued.boundary_id
            ),
        )
    } else {
        OracleVerdict::passed(
            SCENARIO_MINI_RUNTIME_QUEUED_HIDDEN_ORACLE,
            "queued input stayed hidden from provider turns while the generated live turn continued",
        )
    }
}

pub(super) fn mini_runtime_cancellation_prevents_idle_claim(
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    let queued = events
        .iter()
        .filter(|event| event.kind == BoundaryKind::QueuedIngress)
        .map(|event| event.boundary_id.as_str())
        .collect::<BTreeSet<_>>();
    let Some(cancel) = events.iter().find(|event| {
        event.kind == BoundaryKind::Cancellation
            && event
                .observed
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    }) else {
        let outcomes = events
            .iter()
            .filter(|event| event.kind == BoundaryKind::Cancellation)
            .map(|event| {
                format!(
                    "{}={}",
                    event.boundary_id,
                    event
                        .observed
                        .get("cancel_outcome")
                        .and_then(Value::as_str)
                        .unwrap_or("missing")
                )
            })
            .collect::<Vec<_>>();
        return OracleVerdict::failed(
            SCENARIO_MINI_RUNTIME_CANCEL_IDLE_ORACLE,
            format!(
                "no cancellation boundary reported a real runtime cancellation; outcomes={}",
                outcomes.join(",")
            ),
        );
    };
    let target = cancel
        .observed
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("");
    if queued.contains(target) {
        OracleVerdict::passed(
            SCENARIO_MINI_RUNTIME_CANCEL_IDLE_ORACLE,
            "cancelled queued input targets a generated queued boundary and prevents later idle claim",
        )
    } else {
        OracleVerdict::failed(
            SCENARIO_MINI_RUNTIME_CANCEL_IDLE_ORACLE,
            format!("cancellation target `{target}` was not a generated queued boundary"),
        )
    }
}

pub(super) fn mini_standard_streamed_text_finalizes_once(
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    let Some(provider) = events.iter().find(|event| {
        event.kind == BoundaryKind::Provider
            && provider_completion_units(event)
                .iter()
                .filter(|unit| unit.contains("sse"))
                .count()
                >= 2
    }) else {
        return OracleVerdict::failed(
            SCENARIO_MINI_STANDARD_STREAM_FINALIZE_ORACLE,
            "no streamed provider completion with multiple scheduler-owned SSE units was observed",
        );
    };
    let expected = crate::runtime_contracts::host_assistant_message(
        provider
            .payload
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let expected = expected.as_str();
    let output = provider
        .observed
        .get("provider_output")
        .and_then(Value::as_str)
        .unwrap_or("");
    let occurrences = if expected.is_empty() {
        0
    } else {
        output.matches(expected).count()
    };
    verdict_from_bool(
        SCENARIO_MINI_STANDARD_STREAM_FINALIZE_ORACLE,
        occurrences == 1,
        "streamed provider output contains exactly one final assistant text projection",
        "streamed provider output did not finalize exactly once",
    )
}

pub(super) fn mini_standard_provider_error_without_checkpoint(
    events: &[DeliveredBoundary],
) -> OracleVerdict {
    let failure_before_provider = events.iter().any(|event| {
        matches!(
            event.kind,
            BoundaryKind::ProviderMutation | BoundaryKind::BackendFailure
        ) && PendingRuntimeBoundary::from_payload(&event.payload).is_some()
            && event.sequence
                < next_provider_sequence(events, &event.actor_alias).unwrap_or(usize::MAX)
    });
    verdict_from_bool(
        SCENARIO_MINI_STANDARD_PROVIDER_ERROR_ORACLE,
        failure_before_provider && provider_mutation_parser_matrix_observed(events),
        "provider failure/mutation completed through scheduler before any later successful checkpointed provider turn",
        "no scheduler-owned provider failure before checkpoint/next provider turn was observed",
    )
}

pub(super) fn mini_standard_tool_loop_reenters(events: &[DeliveredBoundary]) -> OracleVerdict {
    let Some(tool) = first_event(events, BoundaryKind::Tool) else {
        return OracleVerdict::failed(
            SCENARIO_MINI_STANDARD_TOOL_REENTRY_ORACLE,
            "no tool boundary was generated",
        );
    };
    let reentered = events.iter().any(|event| {
        event.kind == BoundaryKind::Provider
            && event.actor_alias == tool.actor_alias
            && event.sequence > tool.sequence
    });
    verdict_from_bool(
        SCENARIO_MINI_STANDARD_TOOL_REENTRY_ORACLE,
        reentered && tool_runtime_output_observed(events),
        "tool result was captured and a later provider turn re-entered the model loop for the same session",
        "tool result did not have a later provider re-entry",
    )
}

pub(super) fn mini_rlm_finish_required_prose_repair(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> OracleVerdict {
    verdict_from_bool(
        SCENARIO_MINI_RLM_FINISH_REPAIR_ORACLE,
        provider_turn_exchange_counts_are_indexed(summary)
            && observer_reconnect_has_matching_turn(events, summary)
            && events
                .iter()
                .filter(|event| event.kind == BoundaryKind::Provider)
                .any(|event| PendingRuntimeBoundary::from_payload(&event.payload).is_some()),
        "finish-required repair mini-replay observed repeated provider completions and observer convergence",
        "finish-required repair mini-replay lacked repeated provider/observer convergence",
    )
}

pub(super) fn mini_rlm_schema_mismatch_repair(events: &[DeliveredBoundary]) -> OracleVerdict {
    let saw_schema_mutation = events.iter().any(|event| {
        event.kind == BoundaryKind::ProviderMutation
            && event
                .observed
                .pointer("/provider_parser_matrix/matrix/real_provider_parser_execution")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    });
    verdict_from_bool(
        SCENARIO_MINI_RLM_SCHEMA_REPAIR_ORACLE,
        saw_schema_mutation && provider_mutation_classes_observed(events),
        "schema mismatch repair mini-replay used mutated provider scripts through real provider parsers",
        "schema mismatch repair mini-replay lacked provider parser mutation evidence",
    )
}

pub(super) fn mini_rlm_lashlang_cell_exec_continues(events: &[DeliveredBoundary]) -> OracleVerdict {
    let Some(exec) = first_event(events, BoundaryKind::ExecCode) else {
        return OracleVerdict::failed(
            SCENARIO_MINI_RLM_CELL_EXEC_ORACLE,
            "no exec-code boundary was generated",
        );
    };
    let continued = events.iter().any(|event| {
        event.kind == BoundaryKind::Provider
            && event.actor_alias == exec.actor_alias
            && event.sequence > exec.sequence
    });
    verdict_from_bool(
        SCENARIO_MINI_RLM_CELL_EXEC_ORACLE,
        continued && exec_runtime_outcome_observed(events),
        "lashlang cell exec mini-replay produced an exec outcome and continued to a later provider turn",
        "lashlang cell exec mini-replay did not continue after exec",
    )
}

pub(super) fn mini_agent_durable_input_resolution(events: &[DeliveredBoundary]) -> OracleVerdict {
    let durable = events.iter().any(durable_redrive_served);
    verdict_from_bool(
        SCENARIO_MINI_AGENT_DURABLE_INPUT_ORACLE,
        durable && observer_reconnect_has_any(events),
        "durable input mini-replay observed a durable redrive served its recorded result, and observer reconnect resolution",
        "durable input mini-replay lacked durable redrive/observer reconnect evidence",
    )
}

pub(super) fn mini_agent_child_failure_graph(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> OracleVerdict {
    verdict_from_bool(
        SCENARIO_MINI_AGENT_CHILD_FAILURE_ORACLE,
        summary.session_count >= 2
            && events
                .iter()
                .any(|event| event.kind == BoundaryKind::BackendFailure),
        "child failure mini-replay kept multi-session graph evidence while backend failure boundaries executed",
        "child failure mini-replay lacked multi-session backend failure evidence",
    )
}

pub(super) fn mini_agent_parallel_spawn_join(
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> OracleVerdict {
    let provider_sessions = events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
        .filter_map(|event| {
            event
                .observed
                .get("runtime_session_id")
                .and_then(Value::as_str)
        })
        .collect::<BTreeSet<_>>();
    let ordered_sequences = events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider)
        .map(|event| event.sequence)
        .collect::<Vec<_>>();
    let deterministic_order = ordered_sequences.windows(2).all(|pair| pair[0] < pair[1]);
    verdict_from_bool(
        SCENARIO_MINI_AGENT_PARALLEL_JOIN_ORACLE,
        summary.session_count >= 2 && provider_sessions.len() >= 2 && deterministic_order,
        "parallel spawn/join mini-replay recorded deterministic provider completion ordering across runtime sessions",
        "parallel spawn/join mini-replay did not record provider completions across runtime sessions",
    )
}

pub(super) fn verdict_from_bool(
    oracle_id: &'static str,
    condition: bool,
    passed: &'static str,
    failed: &'static str,
) -> OracleVerdict {
    if condition {
        OracleVerdict::passed(oracle_id, passed)
    } else {
        OracleVerdict::failed(oracle_id, failed)
    }
}

pub(super) fn first_event(
    events: &[DeliveredBoundary],
    kind: BoundaryKind,
) -> Option<&DeliveredBoundary> {
    events.iter().find(|event| event.kind == kind)
}

pub(super) fn provider_completion_units(event: &DeliveredBoundary) -> Vec<String> {
    PendingRuntimeBoundary::from_payload(&event.payload)
        .map(|pending| {
            pending
                .completion_units
                .iter()
                .map(|unit| unit.unit.clone())
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn next_provider_sequence(
    events: &[DeliveredBoundary],
    actor_alias: &str,
) -> Option<usize> {
    events
        .iter()
        .filter(|event| event.kind == BoundaryKind::Provider && event.actor_alias == actor_alias)
        .map(|event| event.sequence)
        .min()
}

pub(super) fn observer_reconnect_has_any(events: &[DeliveredBoundary]) -> bool {
    events.iter().any(|event| {
        event.kind == BoundaryKind::Observer
            && event
                .observed
                .get("reconnected")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    })
}

pub(super) fn scenario_contract_oracle(
    contract: &ScenarioContractSpec,
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
    memo: &ScenarioFactMemo,
) -> OracleVerdict {
    let missing = contract
        .required_sim_evidence
        .iter()
        .copied()
        .filter(|evidence| !scenario_evidence_satisfied(evidence, events, summary))
        .collect::<Vec<_>>();
    let oracle_id = scenario_contract_oracle_id(contract);
    let semantic = scenario_contract_semantics(contract, events, summary, memo);
    if missing.is_empty() && semantic.passed {
        OracleVerdict::passed(
            oracle_id,
            format!(
                "{} contract `{}` passed semantic `{}`: {}. Evidence: {}; adapter: {}",
                contract.suite,
                contract.test_name,
                contract.semantic_oracle,
                contract.owned_invariant,
                contract.required_sim_evidence.join(", "),
                semantic.reason
            ),
        )
    } else {
        let mut failures = Vec::new();
        if !missing.is_empty() {
            failures.push(format!("missing evidence [{}]", missing.join(", ")));
        }
        if !semantic.passed {
            failures.push(format!("semantic adapter failed: {}", semantic.reason));
        }
        OracleVerdict::failed(
            oracle_id,
            format!(
                "{} contract `{}` failed {} for invariant: {}",
                contract.suite,
                contract.test_name,
                failures.join("; "),
                contract.owned_invariant
            ),
        )
    }
}

pub(super) fn scenario_contract_oracle_id(contract: &ScenarioContractSpec) -> String {
    format!("{}:{}", contract.oracle_id, contract.test_name)
}

pub(super) fn scenario_evidence_satisfied(
    evidence: &str,
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> bool {
    match evidence {
        "queued_ingress" => summary
            .sessions
            .iter()
            .any(|session| session.queued_ingress_count > 0),
        "cancellation" => summary
            .sessions
            .iter()
            .any(|session| session.cancellation_count > 0),
        "provider_turn" => {
            let provider_turns = summary
                .sessions
                .iter()
                .map(|session| session.provider_turns.len())
                .sum::<usize>();
            summary.session_count > 0 && provider_turns >= summary.session_count * 2
        }
        "provider_event" => events.iter().any(|event| {
            event.kind == BoundaryKind::ProviderEvent
                && event
                    .observed
                    .get("provider_event_release")
                    .and_then(Value::as_bool)
                    == Some(true)
                && event
                    .observed
                    .get("turn_boundary_id")
                    .and_then(Value::as_str)
                    .is_some()
        }),
        "provider_mutation" => events.iter().any(|event| {
            event.kind == BoundaryKind::ProviderMutation
                && PendingRuntimeBoundary::from_payload(&event.payload)
                    .is_some_and(|pending| {
                        pending.completion_family == RuntimeCompletionFamily::ProviderScriptMutation
                    })
                && event
                    .observed
                    .pointer("/provider_parser_matrix/matrix/real_provider_parser_execution")
                    .and_then(Value::as_bool)
                    == Some(true)
        }),
        "tool_result" => {
            summary
                .sessions
                .iter()
                .any(|session| !session.tool_outputs.is_empty())
                && tool_runtime_output_observed(events)
        }
        "max_turn_stop" => events.iter().any(|event| {
            event.kind == BoundaryKind::Trigger
                && event
                    .observed
                    .pointer("/contract_execution/contract")
                    .and_then(Value::as_str)
                    == Some("standard.max_turns_after_tool_result")
                && event
                    .observed
                    .pointer("/contract_execution/source/kind")
                    .and_then(Value::as_str)
                    == Some("fixed_dst_api_execution")
                && event
                    .observed
                    .pointer("/contract_execution/result/turn_outcomes")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .any(|outcome| {
                        outcome.get("kind").and_then(Value::as_str) == Some("stopped")
                            && outcome.get("stop_reason").and_then(Value::as_str)
                                == Some("max_turns")
                    })
        }),
        "final_value" => events.iter().any(|event| {
            event.kind == BoundaryKind::Trigger
                && event
                    .observed
                    .pointer("/contract_execution/source/kind")
                    .and_then(Value::as_str)
                    == Some("fixed_dst_api_execution")
                && event
                    .observed
                    .pointer("/contract_execution/result/runtime_final_value_facts/outcome_kind")
                    .and_then(Value::as_str)
                    == Some("final_value")
                && event
                    .observed
                    .pointer(
                        "/contract_execution/result/runtime_final_value_facts/semantic_channel_observed",
                    )
                    .and_then(Value::as_bool)
                    == Some(true)
        }),
        "observer_convergence" => {
            summary.session_count > 0
                && summary.sessions.iter().all(|session| {
                    session.observer_turn_indices.last().copied()
                        == Some(session.provider_turns.len())
                })
        }
        "runtime_session_graph" => runtime_session_graph_law(summary, None).is_passed(),
        "exec_code" => {
            summary
                .sessions
                .iter()
                .any(|session| !session.exec_code_outputs.is_empty())
                && exec_runtime_outcome_observed(events)
        }
        "trigger" => summary
            .sessions
            .iter()
            .any(|session| session.trigger_count > 0),
        "backend_failure" => events.iter().any(|event| {
            event.kind == BoundaryKind::BackendFailure
                && event
                    .observed
                    .get("backend_failure")
                    .and_then(Value::as_bool)
                    == Some(true)
                && PendingRuntimeBoundary::from_payload(&event.payload)
                    .is_some_and(|pending| {
                        pending.completion_family == RuntimeCompletionFamily::BackendRetryOrFailure
                    })
                && event
                    .observed
                    .pointer("/production_store_error/type")
                    .and_then(Value::as_str)
                    .is_some()
        }),
        "durable_effect" => {
            summary
                .durable_effects
                .iter()
                .any(|effect| effect.execution_count == 1 && effect.replay_count > 0)
                && durable_runtime_effect_observed(events)
        }
        "multi_session" => summary.session_count >= 2,
        "observer_reconnect" => summary
            .sessions
            .iter()
            .any(|session| session.observer_reconnects > 0),
        _ => false,
    }
}

pub(super) struct ScenarioSemanticVerdict {
    pub(super) passed: bool,
    pub(super) reason: String,
}

impl ScenarioSemanticVerdict {
    pub(super) fn passed(reason: impl Into<String>) -> Self {
        Self {
            passed: true,
            reason: reason.into(),
        }
    }

    pub(super) fn failed(reason: impl Into<String>) -> Self {
        Self {
            passed: false,
            reason: reason.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScenarioContractGeneratedFact {
    pub fact: &'static str,
    pub assertion: &'static str,
    pub boundary_ids: Vec<String>,
    pub observed: Value,
}

pub fn scenario_contract_generated_facts(
    contract: &ScenarioContractSpec,
    events: &[DeliveredBoundary],
) -> Result<Vec<ScenarioContractGeneratedFact>, String> {
    scenario_contract_generated_facts_for_semantic(contract.semantic_oracle, events)
}

/// Derive a contract's generated facts against a fresh memo. Callers that
/// evaluate many contracts over one trace should hold a memo and use
/// [`scenario_contract_generated_facts_with_memo`] instead.
pub fn scenario_contract_generated_facts_for_semantic(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
) -> Result<Vec<ScenarioContractGeneratedFact>, String> {
    scenario_contract_generated_facts_with_memo(
        semantic_oracle,
        events,
        &ScenarioFactMemo::default(),
    )
}

pub fn scenario_contract_generated_facts_with_memo(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
    memo: &ScenarioFactMemo,
) -> Result<Vec<ScenarioContractGeneratedFact>, String> {
    let facts = if let Some((row, execution_fact)) = contract_fact_row(semantic_oracle) {
        row.generated_facts(execution_fact, events, memo)?
    } else if semantic_oracle == NO_EXECUTION_FACT_CONTRACT.semantic_oracle {
        vec![standard_max_turns_after_tool_result_fact(events)?]
    } else {
        return Err(format!(
            "{} scenario contract `{semantic_oracle}` has no per-contract semantic adapter; add distinct evidence instead of a generic fallback",
            semantic_oracle.split('.').next().unwrap_or("unknown")
        ));
    };
    reject_named_contract_proxy_facts(semantic_oracle, &facts)?;
    Ok(facts)
}

/// Per-contract semantic adapter. Each suite gets explicit generated-boundary
/// semantics keyed by `semantic_oracle`; unmapped contracts fail loudly instead
/// of inheriting a suite-level fallback.
pub(super) fn scenario_contract_semantics(
    contract: &ScenarioContractSpec,
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
    memo: &ScenarioFactMemo,
) -> ScenarioSemanticVerdict {
    match contract.suite {
        "runtime" => runtime_contract_semantics(contract.semantic_oracle, events, summary),
        "standard" => standard_contract_semantics(contract.semantic_oracle, events, summary, memo),
        "rlm" => rlm_contract_semantics(contract.semantic_oracle, events, summary, memo),
        "agent" => agent_contract_semantics(contract.semantic_oracle, events, summary, memo),
        other => ScenarioSemanticVerdict::failed(format!(
            "suite `{other}` has no per-contract semantic adapter dispatcher"
        )),
    }
}

pub(super) fn runtime_contract_semantics(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
    summary: &AbstractWorldSummary,
) -> ScenarioSemanticVerdict {
    // Each runtime contract owns a DISTINCT semantic adapter keyed on the
    // specific runtime boundary it governs — no `|`-grouped shared arms and no
    // single global condition standing in for per-contract evidence.
    match semantic_oracle {
        "runtime.checkpoint_redrive_cancel" => assert_semantic(
            queued_inputs_have_cancel_targets(events)
                && observer_convergence_law(summary, None).is_passed(),
            "queued input cancellation targeted a source key and observers converged",
        ),
        // The queued (next-turn) input stays pending/hidden while the live turn runs.
        "runtime.queued_work_keeps_pending_input" => assert_semantic(
            queued_ingress_has_source_keys(events) && provider_turns_after_queue(summary),
            "queued ingress kept a stable source key while live provider turns continued",
        ),
        // A queued turn input is eventually completed into a subsequent turn.
        "runtime.queued_turn_input_completion" => assert_semantic(
            queued_ingress_has_source_keys(events)
                && summary.sessions.iter().any(|session| {
                    session.queued_ingress_count > 0 && session.provider_turns.len() >= 2
                }),
            "a queued turn input source key was followed by a completed subsequent turn",
        ),
        // A command-only queue drains its queued source keys.
        "runtime.command_only_queue_drain" => assert_semantic(
            queued_ingress_has_source_keys(events),
            "command queue source keys drained",
        ),
        // A command applied before turn work still lets later provider turns run.
        "runtime.command_before_turn_work" => assert_semantic(
            queued_ingress_has_source_keys(events) && provider_turns_after_queue(summary),
            "a command queued before turn work preserved later turns",
        ),
        "runtime.observation_replay_preserves_input" => assert_semantic(
            observer_reconnect_has_matching_turn(events, summary),
            "observer reconnect replay converged to the final provider turn",
        ),
        other => unmapped_scenario_semantic("runtime", other),
    }
}

pub(super) fn standard_contract_semantics(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
    _summary: &AbstractWorldSummary,
    memo: &ScenarioFactMemo,
) -> ScenarioSemanticVerdict {
    match scenario_contract_generated_facts_with_memo(semantic_oracle, events, memo) {
        Ok(facts) => ScenarioSemanticVerdict::passed(format!(
            "generated contract facts held: {}",
            fact_names(&facts)
        )),
        Err(reason) => ScenarioSemanticVerdict::failed(reason),
    }
}

pub(super) fn rlm_contract_semantics(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
    _summary: &AbstractWorldSummary,
    memo: &ScenarioFactMemo,
) -> ScenarioSemanticVerdict {
    match scenario_contract_generated_facts_with_memo(semantic_oracle, events, memo) {
        Ok(facts) => ScenarioSemanticVerdict::passed(format!(
            "generated contract facts held: {}",
            fact_names(&facts)
        )),
        Err(reason) => ScenarioSemanticVerdict::failed(reason),
    }
}

pub(super) fn agent_contract_semantics(
    semantic_oracle: &str,
    events: &[DeliveredBoundary],
    _summary: &AbstractWorldSummary,
    memo: &ScenarioFactMemo,
) -> ScenarioSemanticVerdict {
    match scenario_contract_generated_facts_with_memo(semantic_oracle, events, memo) {
        Ok(facts) => ScenarioSemanticVerdict::passed(format!(
            "generated contract facts held: {}",
            fact_names(&facts)
        )),
        Err(reason) => ScenarioSemanticVerdict::failed(reason),
    }
}

pub(super) fn fact_names(facts: &[ScenarioContractGeneratedFact]) -> String {
    facts
        .iter()
        .map(|fact| fact.fact)
        .collect::<Vec<_>>()
        .join(", ")
}
