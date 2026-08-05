use super::RuntimePerfScenario;

pub(super) fn allocation_budget_bytes(scenario: RuntimePerfScenario) -> f64 {
    match scenario {
        RuntimePerfScenario::ProcessListStress => 45_000_000_000.0,
        RuntimePerfScenario::RlmAsyncToolCompletion => 145_000_000.0,
        RuntimePerfScenario::RlmTriggerMailPipeline => 260_000_000.0,
        RuntimePerfScenario::RlmProcessAsyncToolCompletion => 290_000_000.0,
        RuntimePerfScenario::TurnStartGate => 90_000_000.0,
        RuntimePerfScenario::TurnCancelRoundTrip => 85_000_000.0,
        RuntimePerfScenario::IngressClaimProjection => 180_000_000.0,
        RuntimePerfScenario::ToolDiscoverySearch => 2_300_000_000.0,
        RuntimePerfScenario::RlmLargeToolCatalog => 5_600_000_000.0,
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
        RuntimePerfScenario::RlmAsyncToolCompletion => 11_000_000.0,
        RuntimePerfScenario::RlmTriggerMailPipeline => 20_000_000.0,
        RuntimePerfScenario::RlmProcessAsyncToolCompletion => 23_000_000.0,
        RuntimePerfScenario::TurnStartGate | RuntimePerfScenario::TurnCancelRoundTrip => {
            6_000_000.0
        }
        RuntimePerfScenario::IngressClaimProjection => 14_000_000.0,
        RuntimePerfScenario::ToolDiscoverySearch => 170_000_000.0,
        RuntimePerfScenario::RlmLargeToolCatalog => 430_000_000.0,
        RuntimePerfScenario::RlmLargePrint => 750_000_000.0,
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
        // The five-run single-thread baseline was 32.6 s median, 20.9-42.1 s.
        RuntimePerfScenario::ProcessListStress => 75_000.0,
        RuntimePerfScenario::RlmAsyncToolCompletion => 1_000.0,
        RuntimePerfScenario::RlmTriggerMailPipeline => 520.0,
        RuntimePerfScenario::RlmProcessAsyncToolCompletion => 2_000.0,
        RuntimePerfScenario::TurnStartGate => 1_600.0,
        RuntimePerfScenario::TurnCancelRoundTrip => 310.0,
        RuntimePerfScenario::IngressClaimProjection => 460.0,
        RuntimePerfScenario::ToolDiscoverySearch
        | RuntimePerfScenario::RlmLargeToolCatalog
        | RuntimePerfScenario::RlmObliqueStackMix => 20_000.0,
        RuntimePerfScenario::LiveReplayPressure => 5_000.0,
        _ => 10_000.0,
    }
}

pub(super) fn phase_wall_clock_budget_ms(phase: &str) -> Option<f64> {
    let budget = match phase {
        // Baseline maxima up to 4.4 ms; 25 ms preserves at least 5x headroom.
        "before_turn_hooks"
        | "committed_turn"
        | "context_transform"
        | "direct_llm_client.complete"
        | "live_replay.append"
        | "live_replay.current_cursor_parse"
        | "live_replay.gap_handling"
        | "live_replay.replay_after_cursor"
        | "live_replay.subscribe_buffered"
        | "live_replay.trim_by_capacity"
        | "openai_responses_sse_parse.project_parts"
        | "plugin_hook.context_transform.rlm_protocol"
        | "plugin_hook.context_transform.rolling_history"
        | "prompt_build"
        | "queued_work.abandon_join_claim"
        | "queued_work.claim_exclusive_turn_work"
        | "queued_work.claim_join_turn_work"
        | "queued_work.claim_session_command"
        | "queued_work.claim_session_lease"
        | "queued_work.complete_exclusive_turn_work"
        | "queued_work.complete_join_turn_work"
        | "queued_work.complete_session_command"
        | "queued_work.enqueue_mixed_batch"
        | "queued_work.list_pending"
        | "queued_work.reclaim_by_batch_ids"
        | "rlm_exec_checkpoint"
        | "rlm_lashlang.compile_link"
        | "rlm_lashlang.print_project"
        | "rlm_lashlang.resolve_projected_bindings"
        | "rlm_lashlang.store_module_artifact"
        | "rlm_process.build_context"
        | "rlm_process.compile"
        | "rlm_process.load_artifact"
        | "rlm_process.prepare_start"
        | "standard_llm_checkpoint"
        | "standard_parallel_tools_checkpoint"
        | "store_reopen.persisted_load"
        | "store_reopen.store_factory_create"
        | "trace_jsonl.inspect_files"
        | "turn_input_ingress.abandon_active_claim"
        | "turn_input_ingress.abandon_next_claim"
        | "turn_input_ingress.claim_active_initial"
        | "turn_input_ingress.claim_next_turn_inputs"
        | "turn_input_ingress.claim_session_lease"
        | "turn_input_ingress.complete_active_and_defer"
        | "turn_input_ingress.complete_next_turn_inputs"
        | "turn_input_ingress.enqueue_active"
        | "turn_input_ingress.enqueue_next"
        | "turn_input_ingress.list_pending"
        | "turn_input_ingress.reclaim_active_inputs"
        | "turn_input_ingress.reclaim_next_turn_inputs" => 25.0,

        // Baseline maxima 5.6-9.7 ms.
        "openai_responses_sse_parse.parse_payload"
        | "process_list_stress.render_live_json"
        | "process_list_stress.signal_append"
        | "process_list_stress.wait_roundtrip"
        | "process_list_stress.env_spec_hash"
        | "rlm_lashlang.rehydrate_projected_globals"
        | "rlm_process.shutdown" => 75.0,

        // Baseline maxima 16-35.5 ms.
        "process_list_stress.list_all"
        | "process_list_stress.list_global"
        | "process_list_stress.list_live"
        | "process_list_stress.render_all_json"
        | "post_commit_delivery"
        | "store_reopen.runtime_hydration" => 200.0,

        // Baseline maxima 55-81 ms.
        "prepared_turn"
        | "rlm_process.resolve_environment"
        | "sqlite_store_reopen.runtime_reopen" => 500.0,

        // Baseline maxima 112-424 ms.
        "process.await_handle"
        | "process.start_child"
        | "rlm_process.await_handle"
        | "rlm_process.execute"
        | "rlm_process.start" => 1_500.0,
        "rlm_lashlang.execute" => 2_500.0,
        "effect_loop" => 8_000.0,

        // Existing targeted latency guards retained unchanged.
        "trigger.occurrence_to_delivery" => 55.0,
        "turn_cancel.start_gate" => 1_200.0,
        "turn_cancel.request_to_token_to_seal" => 60.0,
        "turn_input_ingress.enqueue_to_claim_to_projection" => 195.0,

        // Store-hardening baseline: aggregate over 12 operations per run.
        "store_hardening.identity.process_registration"
        | "store_hardening.identity.trigger_occurrence"
        | "store_hardening.identity.usage_delta"
        | "store_hardening.memory.claim_session_lease"
        | "store_hardening.memory.claim_queued_work"
        | "store_hardening.memory.complete_queued_work"
        | "store_hardening.memory.attachment_intent"
        | "store_hardening.memory.attachment_adopt"
        | "store_hardening.memory.append_receipt_usage_fresh"
        | "store_hardening.memory.append_receipt_replay"
        | "store_hardening.memory.prune_terminal_processes" => 50.0,
        "store_hardening.sqlite.claim_session_lease"
        | "store_hardening.sqlite.claim_queued_work"
        | "store_hardening.sqlite.complete_queued_work"
        | "store_hardening.sqlite.attachment_intent"
        | "store_hardening.sqlite.attachment_adopt"
        | "store_hardening.sqlite.append_receipt_usage_fresh"
        | "store_hardening.sqlite.append_receipt_replay"
        | "store_hardening.sqlite.prune_terminal_processes" => 100.0,
        "store_hardening.postgres.open_preverified"
        | "store_hardening.postgres.claim_session_lease"
        | "store_hardening.postgres.claim_queued_work"
        | "store_hardening.postgres.complete_queued_work"
        | "store_hardening.postgres.attachment_intent"
        | "store_hardening.postgres.attachment_adopt"
        | "store_hardening.postgres.append_receipt_usage_fresh"
        | "store_hardening.postgres.append_receipt_replay" => 300.0,
        "store_hardening.postgres.open_enforce" => 1_000.0,
        "store_hardening.postgres.prune_terminal_processes" => 4_000.0,
        _ => return None,
    };
    Some(budget)
}
