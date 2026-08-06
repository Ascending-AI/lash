use super::RuntimePerfScenario;

pub(super) fn allocation_budget_bytes(scenario: RuntimePerfScenario) -> f64 {
    match scenario {
        RuntimePerfScenario::RlmAsyncToolCompletion => 145_000_000.0,
        RuntimePerfScenario::RlmTriggerMailPipeline => 260_000_000.0,
        RuntimePerfScenario::RlmProcessAsyncToolCompletion => 290_000_000.0,
        RuntimePerfScenario::TurnStartGate => 90_000_000.0,
        RuntimePerfScenario::TurnCancelRoundTrip => 85_000_000.0,
        RuntimePerfScenario::IngressClaimProjection => 180_000_000.0,
        RuntimePerfScenario::ToolDiscoverySearch => 630_000_000.0,
        RuntimePerfScenario::RlmLargeToolCatalog => 1_200_000_000.0,
        RuntimePerfScenario::RlmLargePrint => 1_000_000_000.0,
        RuntimePerfScenario::RlmObliqueStackMix => 1_950_000_000.0,
        RuntimePerfScenario::RlmStreamedPairedLashlang => 128_000_000.0,
        RuntimePerfScenario::LiveReplayPressure => 128_000_000.0,
        RuntimePerfScenario::OpenAiResponsesSseParse
        | RuntimePerfScenario::DirectLlmClient
        | RuntimePerfScenario::TurnCheckpoint => 256_000_000.0,
        _ => 1_000_000_000.0,
    }
}

pub(super) fn steady_state_turn_allocation_budget_bytes(scenario: RuntimePerfScenario) -> f64 {
    match scenario {
        // Fixture population is intentionally outside this guard. The test-only
        // registry clones its full map on each seed write; the product-facing
        // steady-state work measured 19.6 MB/turn in the five-run baseline.
        RuntimePerfScenario::ProcessListStress => 25_000_000.0,
        RuntimePerfScenario::RlmAsyncToolCompletion => 11_000_000.0,
        RuntimePerfScenario::RlmTriggerMailPipeline => 20_000_000.0,
        RuntimePerfScenario::RlmProcessAsyncToolCompletion => 23_000_000.0,
        RuntimePerfScenario::TurnStartGate | RuntimePerfScenario::TurnCancelRoundTrip => {
            6_000_000.0
        }
        RuntimePerfScenario::IngressClaimProjection => 14_000_000.0,
        RuntimePerfScenario::ToolDiscoverySearch => 48_000_000.0,
        RuntimePerfScenario::RlmLargeToolCatalog => 95_000_000.0,
        RuntimePerfScenario::RlmLargePrint => 750_000_000.0,
        RuntimePerfScenario::RlmSubagentSpawn => 45_000_000.0,
        RuntimePerfScenario::RlmObliqueStackMix => 165_000_000.0,
        RuntimePerfScenario::RlmStreamedPairedLashlang => 64_000_000.0,
        RuntimePerfScenario::LiveReplayPressure => 96_000_000.0,
        RuntimePerfScenario::OpenAiResponsesSseParse
        | RuntimePerfScenario::DirectLlmClient
        | RuntimePerfScenario::TurnCheckpoint => 192_000_000.0,
        _ => 750_000_000.0,
    }
}

pub(super) fn wall_clock_budget_ms(scenario: RuntimePerfScenario) -> f64 {
    match scenario {
        RuntimePerfScenario::RlmAsyncToolCompletion => 1_000.0,
        RuntimePerfScenario::RlmTriggerMailPipeline => 520.0,
        RuntimePerfScenario::RlmProcessAsyncToolCompletion => 2_000.0,
        RuntimePerfScenario::TurnStartGate => 1_600.0,
        RuntimePerfScenario::TurnCancelRoundTrip => 310.0,
        RuntimePerfScenario::IngressClaimProjection => 460.0,
        RuntimePerfScenario::ToolDiscoverySearch => 1_200.0,
        RuntimePerfScenario::RlmLargeToolCatalog => 2_500.0,
        RuntimePerfScenario::RlmObliqueStackMix => 20_000.0,
        RuntimePerfScenario::LiveReplayPressure => 5_000.0,
        _ => 10_000.0,
    }
}

pub(super) fn process_list_run_allocation_budget_bytes() -> f64 {
    300_000_000.0
}

pub(super) fn process_list_run_wall_clock_budget_ms() -> f64 {
    // The five-run median sits near 224 ms but the per-run spread on a shared
    // machine reaches ~1 s; the ceiling guards the median with headroom for
    // that observed noise envelope rather than the quiet-machine median.
    1_500.0
}

#[derive(Clone, Copy)]
pub(super) struct PhaseBudget {
    pub(super) phase: &'static str,
    pub(super) budget_ms: f64,
}

// These names have one envelope across their emitting scenarios, or are unique
// to one scenario. High-variance shared phases live in
// `scenario_phase_wall_clock_budget_ms` below instead.
pub(super) const GLOBAL_PHASE_BUDGETS: &[PhaseBudget] = &[
    PhaseBudget {
        phase: "before_turn_hooks",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "committed_turn",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "context_transform",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "direct_llm_client.complete",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "live_replay.append",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "live_replay.current_cursor_parse",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "live_replay.gap_handling",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "live_replay.replay_after_cursor",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "live_replay.subscribe_buffered",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "live_replay.trim_by_capacity",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "openai_responses_sse_parse.project_parts",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "plugin_hook.context_transform.rlm_protocol",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "plugin_hook.context_transform.rolling_history",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "prompt_build",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "queued_work.abandon_join_claim",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "queued_work.claim_exclusive_turn_work",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "queued_work.claim_join_turn_work",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "queued_work.claim_session_command",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "queued_work.claim_session_lease",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "queued_work.complete_exclusive_turn_work",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "queued_work.complete_join_turn_work",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "queued_work.complete_session_command",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "queued_work.enqueue_mixed_batch",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "queued_work.list_pending",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "queued_work.reclaim_by_batch_ids",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "rlm_exec_checkpoint",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "rlm_lashlang.compile_link",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "rlm_lashlang.print_project",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "rlm_lashlang.rehydrate_projected_globals",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "rlm_lashlang.resolve_projected_bindings",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "rlm_lashlang.store_module_artifact",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "rlm_process.build_context",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "rlm_process.compile",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "rlm_process.load_artifact",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "rlm_process.prepare_start",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "rlm_process.shutdown",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "standard_llm_checkpoint",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "standard_parallel_tools_checkpoint",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "store_reopen.persisted_load",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "store_reopen.store_factory_create",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "trace_jsonl.inspect_files",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "turn_input_ingress.abandon_active_claim",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "turn_input_ingress.abandon_next_claim",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "turn_input_ingress.claim_active_initial",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "turn_input_ingress.claim_next_turn_inputs",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "turn_input_ingress.claim_session_lease",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "turn_input_ingress.complete_active_and_defer",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "turn_input_ingress.complete_next_turn_inputs",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "turn_input_ingress.enqueue_active",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "turn_input_ingress.enqueue_next",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "turn_input_ingress.list_pending",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "turn_input_ingress.reclaim_active_inputs",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "turn_input_ingress.reclaim_next_turn_inputs",
        budget_ms: 25.0,
    },
    PhaseBudget {
        phase: "openai_responses_sse_parse.parse_payload",
        budget_ms: 75.0,
    },
    PhaseBudget {
        phase: "process_list_stress.render_live_json",
        budget_ms: 75.0,
    },
    PhaseBudget {
        phase: "process_list_stress.signal_append",
        budget_ms: 75.0,
    },
    PhaseBudget {
        phase: "process_list_stress.wait_roundtrip",
        budget_ms: 75.0,
    },
    PhaseBudget {
        phase: "process_list_stress.env_spec_hash",
        budget_ms: 75.0,
    },
    PhaseBudget {
        phase: "process_list_stress.list_all",
        budget_ms: 200.0,
    },
    PhaseBudget {
        phase: "process_list_stress.list_global",
        budget_ms: 200.0,
    },
    PhaseBudget {
        phase: "process_list_stress.list_live",
        budget_ms: 200.0,
    },
    PhaseBudget {
        phase: "process_list_stress.render_all_json",
        budget_ms: 200.0,
    },
    PhaseBudget {
        phase: "post_commit_delivery",
        budget_ms: 200.0,
    },
    PhaseBudget {
        phase: "store_reopen.runtime_hydration",
        budget_ms: 200.0,
    },
    PhaseBudget {
        phase: "sqlite_store_reopen.runtime_reopen",
        budget_ms: 500.0,
    },
    PhaseBudget {
        phase: "trigger.occurrence_to_delivery",
        budget_ms: 55.0,
    },
    PhaseBudget {
        phase: "turn_cancel.start_gate",
        budget_ms: 1_200.0,
    },
    PhaseBudget {
        phase: "turn_cancel.request_to_token_to_seal",
        budget_ms: 60.0,
    },
    PhaseBudget {
        phase: "turn_input_ingress.enqueue_to_claim_to_projection",
        budget_ms: 195.0,
    },
    PhaseBudget {
        phase: "store_hardening.identity.process_registration",
        budget_ms: 50.0,
    },
    PhaseBudget {
        phase: "store_hardening.identity.trigger_occurrence",
        budget_ms: 50.0,
    },
    PhaseBudget {
        phase: "store_hardening.identity.usage_delta",
        budget_ms: 50.0,
    },
    PhaseBudget {
        phase: "store_hardening.memory.claim_session_lease",
        budget_ms: 50.0,
    },
    PhaseBudget {
        phase: "store_hardening.memory.claim_queued_work",
        budget_ms: 50.0,
    },
    PhaseBudget {
        phase: "store_hardening.memory.complete_queued_work",
        budget_ms: 50.0,
    },
    PhaseBudget {
        phase: "store_hardening.memory.attachment_intent",
        budget_ms: 50.0,
    },
    PhaseBudget {
        phase: "store_hardening.memory.attachment_adopt",
        budget_ms: 50.0,
    },
    PhaseBudget {
        phase: "store_hardening.memory.append_receipt_usage_fresh",
        budget_ms: 50.0,
    },
    PhaseBudget {
        phase: "store_hardening.memory.append_receipt_replay",
        budget_ms: 50.0,
    },
    PhaseBudget {
        phase: "store_hardening.memory.prune_terminal_processes",
        budget_ms: 50.0,
    },
    PhaseBudget {
        phase: "store_hardening.sqlite.claim_session_lease",
        budget_ms: 100.0,
    },
    PhaseBudget {
        phase: "store_hardening.sqlite.claim_queued_work",
        budget_ms: 100.0,
    },
    PhaseBudget {
        phase: "store_hardening.sqlite.complete_queued_work",
        budget_ms: 100.0,
    },
    PhaseBudget {
        phase: "store_hardening.sqlite.attachment_intent",
        budget_ms: 100.0,
    },
    PhaseBudget {
        phase: "store_hardening.sqlite.attachment_adopt",
        budget_ms: 100.0,
    },
    PhaseBudget {
        phase: "store_hardening.sqlite.append_receipt_usage_fresh",
        budget_ms: 100.0,
    },
    PhaseBudget {
        phase: "store_hardening.sqlite.append_receipt_replay",
        budget_ms: 100.0,
    },
    PhaseBudget {
        phase: "store_hardening.sqlite.prune_terminal_processes",
        budget_ms: 100.0,
    },
    PhaseBudget {
        phase: "store_hardening.postgres.open_preverified",
        budget_ms: 300.0,
    },
    PhaseBudget {
        phase: "store_hardening.postgres.claim_session_lease",
        budget_ms: 300.0,
    },
    PhaseBudget {
        phase: "store_hardening.postgres.claim_queued_work",
        budget_ms: 300.0,
    },
    PhaseBudget {
        phase: "store_hardening.postgres.complete_queued_work",
        budget_ms: 300.0,
    },
    PhaseBudget {
        phase: "store_hardening.postgres.attachment_intent",
        budget_ms: 300.0,
    },
    PhaseBudget {
        phase: "store_hardening.postgres.attachment_adopt",
        budget_ms: 300.0,
    },
    PhaseBudget {
        phase: "store_hardening.postgres.append_receipt_usage_fresh",
        budget_ms: 300.0,
    },
    PhaseBudget {
        phase: "store_hardening.postgres.append_receipt_replay",
        budget_ms: 300.0,
    },
    PhaseBudget {
        phase: "store_hardening.postgres.open_enforce",
        budget_ms: 1_000.0,
    },
    PhaseBudget {
        phase: "store_hardening.postgres.prune_terminal_processes",
        budget_ms: 300.0,
    },
];

#[cfg(test)]
pub(super) const SCENARIO_KEYED_PHASES: &[&str] = &[
    "effect_loop",
    "prepared_turn",
    "process.await_handle",
    "process.start_child",
    "rlm_lashlang.execute",
    "rlm_process.await_handle",
    "rlm_process.execute",
    "rlm_process.resolve_environment",
    "rlm_process.start",
];

pub(super) fn phase_wall_clock_budget_ms(
    scenario: RuntimePerfScenario,
    phase: &str,
) -> Option<f64> {
    scenario_phase_wall_clock_budget_ms(scenario, phase).or_else(|| {
        GLOBAL_PHASE_BUDGETS
            .iter()
            .find(|budget| budget.phase == phase)
            .map(|budget| budget.budget_ms)
    })
}

fn scenario_phase_wall_clock_budget_ms(scenario: RuntimePerfScenario, phase: &str) -> Option<f64> {
    use RuntimePerfScenario::*;

    let budget = match (scenario, phase) {
        // Aggregate phase totals vary with each scenario's protocol workload.
        (
            Standard
            | StandardAsyncToolCompletion
            | StandardToolCalls
            | ScopedEffectController
            | StoreReopen
            | TraceJsonlStandard
            | TurnCancelRoundTrip
            | TurnStartGate,
            "effect_loop",
        ) => 50.0,
        (
            StandardShellOutput
            | SqliteStoreReopen
            | OpenAiCompatStream
            | Rlm
            | RlmStreamedPairedLashlang
            | RlmGlobals
            | RlmLlmQuery
            | RlmAsyncToolCompletion
            | TraceJsonlExtended
            | RlmToolCalls,
            "effect_loop",
        ) => 125.0,
        (IngressClaimProjection | RlmLargePrint | RlmTriggerMailPipeline, "effect_loop") => 200.0,
        (DeepTurnComposition | RlmProcessAsyncToolCompletion, "effect_loop") => 400.0,
        (RlmProcessHandles | RlmSubagentSpawn, "effect_loop") => 750.0,
        (RlmObliqueStackMix, "effect_loop") => 2_000.0,
        (ToolDiscoverySearch, "effect_loop") => 500.0,
        (RlmLargeToolCatalog, "effect_loop") => 1_000.0,

        // Push CI uses the debug quick profile; these two projector-heavy
        // scenarios are materially slower there than in the release baseline.
        (RlmLargePrint, "prepared_turn") => 200.0,
        (RlmObliqueStackMix, "prepared_turn") => 250.0,
        (_, "prepared_turn") => 25.0,

        (
            Rlm | RlmLargeToolCatalog | RlmStreamedPairedLashlang | TraceJsonlExtended,
            "rlm_lashlang.execute",
        ) => 5.0,
        (IngressClaimProjection | RlmGlobals, "rlm_lashlang.execute") => 10.0,
        (
            RlmAsyncToolCompletion | RlmTriggerMailPipeline | RlmLlmQuery | RlmToolCalls,
            "rlm_lashlang.execute",
        ) => 30.0,
        (RlmLargePrint, "rlm_lashlang.execute") => 75.0,
        (DeepTurnComposition, "rlm_lashlang.execute") => 250.0,
        (RlmProcessAsyncToolCompletion | RlmProcessHandles, "rlm_lashlang.execute") => 350.0,
        (RlmSubagentSpawn, "rlm_lashlang.execute") => 750.0,
        (RlmObliqueStackMix, "rlm_lashlang.execute") => 1_200.0,

        (DeepTurnComposition, "process.start_child" | "rlm_process.start") => 10.0,
        (
            RlmProcessAsyncToolCompletion
            | RlmProcessHandles
            | RlmSubagentSpawn
            | RlmObliqueStackMix,
            "process.start_child" | "rlm_process.start",
        ) => 200.0,
        (DeepTurnComposition, "process.await_handle" | "rlm_process.await_handle") => 250.0,
        (
            RlmProcessHandles | RlmProcessAsyncToolCompletion,
            "process.await_handle" | "rlm_process.await_handle",
        ) => 200.0,
        (
            RlmSubagentSpawn | RlmObliqueStackMix,
            "process.await_handle" | "rlm_process.await_handle",
        ) => 750.0,

        (
            RlmTriggerMailPipeline | RlmProcessAsyncToolCompletion | RlmProcessHandles,
            "rlm_process.execute",
        ) => 75.0,
        (DeepTurnComposition, "rlm_process.execute") => 200.0,
        (RlmSubagentSpawn | RlmObliqueStackMix, "rlm_process.execute") => 650.0,
        (DeepTurnComposition, "rlm_process.resolve_environment") => 10.0,
        (
            RlmSubagentSpawn | RlmObliqueStackMix | RlmTriggerMailPipeline,
            "rlm_process.resolve_environment",
        ) => 75.0,
        (RlmProcessAsyncToolCompletion | RlmProcessHandles, "rlm_process.resolve_environment") => {
            150.0
        }
        _ => return None,
    };
    Some(budget)
}
