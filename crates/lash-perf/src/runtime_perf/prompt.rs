use super::scenarios::RuntimePerfScenario;

const DEFAULT_PROMPT: &str =
    "Inspect the current state and reply with exactly: runtime perf benchmark ok";

pub(crate) fn benchmark_prompt(scenario: RuntimePerfScenario, turn_index: usize) -> String {
    match scenario {
        RuntimePerfScenario::CheckpointStateHotPaths => {
            unreachable!("checkpoint-state hot paths use their dedicated measurement")
        }
        RuntimePerfScenario::Standard | RuntimePerfScenario::EmbedStandard => format!(
            "Turn {} of a longer runtime benchmark conversation. Inspect the state and reply with exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::Rlm | RuntimePerfScenario::EmbedRlm => format!(
            "Turn {} in RLM mode. Continue the benchmark chat and reply with exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::RlmLargeToolCatalog => format!(
            "Turn {} in RLM mode with a Gmail-sized callable tool catalog. Do not call a Gmail tool; reply with exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::RlmObliqueStackMix => format!(
            "Turn {} in RLM mode. Exercise the OBLIQ-style search, subagent, live-handle, direct judge, trace, and large print paths, then finish exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::RlmToolCalls
        | RuntimePerfScenario::DurableRlmCheckpointTurnSqlite
        | RuntimePerfScenario::DurableRlmCheckpointTurnPostgres => format!(
            "Turn {} in RLM mode. Exercise the benchmark_echo tool path and reply with exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::RlmAsyncToolCompletion => format!(
            "Turn {} in RLM mode. Exercise the pending benchmark_async tool completion path, then finish exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::StandardToolCalls
        | RuntimePerfScenario::DurableStandardToolTurnSqlite
        | RuntimePerfScenario::DurableStandardToolTurnPostgres => format!(
            "Turn {} in standard mode. Use the batch tool to exercise parallel benchmark_echo calls, then reply with exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::StandardAsyncToolCompletion => format!(
            "Turn {} in standard mode. Launch the async benchmark tool completion, then reply with exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::StandardShellOutput => format!(
            "Turn {} in standard mode. Exercise shell.exec output capture, then reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::ToolDiscoverySearch => format!(
            "Turn {} in standard mode. Search the catalog for Gmail email tools, then reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::OpenAiResponsesSseParse => format!(
            "Turn {} in OpenAI Responses SSE parser benchmark mode. Parse a local Responses stream and verify the benchmark marker.",
            turn_index + 1
        ),
        RuntimePerfScenario::DirectLlmClient => format!(
            "Turn {} in direct LLM client benchmark mode. Run a direct structured completion and verify the benchmark marker.",
            turn_index + 1
        ),
        RuntimePerfScenario::ProcessListStress => format!(
            "Turn {} in process-list stress benchmark mode. Compare live process listing with explicit full history and verify the benchmark marker.",
            turn_index + 1
        ),
        RuntimePerfScenario::RlmProcessHandles => format!(
            "Turn {} in RLM mode. Exercise start/await/cancel process handles, then finish exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::RlmTriggerMailPipeline => format!(
            "Turn {} in RLM mode. Ensure a mail trigger is registered, send through inbox.test, let the forwarder process run, and finish exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::RlmProcessAsyncToolCompletion => format!(
            "Turn {} in RLM mode. Exercise pending benchmark_async completion inside a started process, then finish exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::RlmSubagentSpawn
        | RuntimePerfScenario::DurableAgentChildTurnSqlite
        | RuntimePerfScenario::DurableAgentChildTurnPostgres => format!(
            "Turn {} in RLM mode. Start a process that spawns a default subagent with seeded input, await it, then finish exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::RlmLlmQuery => format!(
            "Turn {} in RLM mode. Exercise llm_query direct completion, then finish exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::RlmGlobals => format!(
            "Turn {} in RLM mode with bound variables updated for this turn. Inspect the current state and reply with exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::RlmLargePrint => format!(
            "Turn {} in RLM mode. Print a large structured tool result to exercise host-owned print projection, then finish exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::RlmStreamedPairedLashlang => format!(
            "Turn {} in RLM mode. Stream visible prose before a paired <lashlang> block, close it, ignore any suffix after the close tag, and finish exactly: {}",
            turn_index + 1,
            expected_reply()
        ),
        RuntimePerfScenario::OpenAiCompatStream => format!(
            "Turn {} in OpenAI-compatible streaming benchmark mode. Continue the benchmark chat and reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::TurnCheckpoint => format!(
            "Turn {} in sans-IO turn checkpoint benchmark mode. Checkpoint and restore pending effects, then reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::ScopedEffectController => format!(
            "Turn {} in scoped effect-controller benchmark mode. Continue the benchmark chat and reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::StoreReopen | RuntimePerfScenario::SqliteStoreReopen => format!(
            "Turn {} in store reopen benchmark mode. Continue after persisted reload and reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::LiveReplayPressure => format!(
            "Turn {} in live replay pressure benchmark mode. Append, replay, subscribe, trim, and verify gap handling.",
            turn_index + 1
        ),
        RuntimePerfScenario::TraceJsonlStandard => format!(
            "Turn {} in standard JSONL trace benchmark mode. Continue the benchmark chat and reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::TraceJsonlExtended => format!(
            "Turn {} in extended JSONL trace benchmark mode. Run the Lashlang block and finish exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::QueuedWorkClaimStress => format!(
            "Turn {} in queued-work claim stress benchmark mode. Claim, renew, complete, and verify queued work.",
            turn_index + 1
        ),
        RuntimePerfScenario::TurnInputIngressInterrupt => format!(
            "Turn {} in turn-input ingress interrupt benchmark mode. Claim, defer, reclaim, complete, and verify pending turn input.",
            turn_index + 1
        ),
        RuntimePerfScenario::DeepTurnComposition => format!(
            "Turn {} in the deep-composition stack benchmark. Run the parent process/tool loop and child session, then incorporate the injected active-turn input.",
            turn_index + 1
        ),
        RuntimePerfScenario::TurnStartGate => format!(
            "Turn {} in the cancellation start-gate benchmark. Reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::TurnCancelRoundTrip => format!(
            "Turn {} in the cancellation round-trip benchmark. Wait for the exact-turn cancellation request.",
            turn_index + 1
        ),
        RuntimePerfScenario::IngressClaimProjection => format!(
            "Turn {} in the active-ingress projection benchmark. Continue after the checkpoint and incorporate the injected marker.",
            turn_index + 1
        ),
        RuntimePerfScenario::WriterContention2Workers
        | RuntimePerfScenario::WriterContention8Workers => format!(
            "Turn {} in the writer-contention benchmark. Reply with exactly: runtime perf benchmark ok",
            turn_index + 1
        ),
        RuntimePerfScenario::AsyncProcessSettlement2Children
        | RuntimePerfScenario::AsyncProcessSettlement8Children => format!(
            "Turn {} in the async process settlement benchmark. Spawn the gated child processes and return before their settlement.",
            turn_index + 1
        ),
        RuntimePerfScenario::HighTrafficLoadSqlite
        | RuntimePerfScenario::HighTrafficLoadPostgres
        | RuntimePerfScenario::HighTrafficKneeSqlite
        | RuntimePerfScenario::HighTrafficKneePostgres => {
            unreachable!("high-traffic scenarios construct per-operation prompts")
        }
        RuntimePerfScenario::DurableCheckpointCurveSqlite
        | RuntimePerfScenario::DurableCheckpointCurvePostgres => format!(
            "Turn {} in the durable checkpoint curve. Persist checkpoint body bytes {} inside this live RLM turn, then finish exactly: runtime perf benchmark ok",
            turn_index + 1,
            scenario
                .checkpoint_curve_bytes(turn_index)
                .expect("checkpoint curve scenario")
        ),
        RuntimePerfScenario::StoreHardeningHotPaths => {
            format!("Turn {} in the store-hardening benchmark.", turn_index + 1)
        }
    }
}

fn expected_reply() -> &'static str {
    DEFAULT_PROMPT
        .rsplit_once(": ")
        .map_or("runtime perf benchmark ok", |(_, text)| text)
}
