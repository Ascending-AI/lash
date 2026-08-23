use lash_standard_plugins::{RollingHistoryConfig, StandardContextApproach};

pub(crate) const DURABLE_CHECKPOINT_CURVE_BYTES: [usize; 5] =
    [256 * 1024, 512 * 1024, 768 * 1024, 1024 * 1024, 1280 * 1024];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ExecutionMode {
    Standard,
    Rlm,
}

impl ExecutionMode {
    pub(crate) fn is_standard(self) -> bool {
        matches!(self, Self::Standard)
    }

    pub(crate) fn is_rlm(self) -> bool {
        matches!(self, Self::Rlm)
    }
}

// The shared `Scenario` suffix names the distinct perf harness kinds and reads
// naturally at the ~50 macro-driven call sites; it is not redundant with the
// `ScenarioHarnessKind` type name.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub(crate) enum ScenarioHarnessKind {
    RuntimeScenario,
    StandardProtocolScenario,
    RlmProtocolScenario,
    AgentScenario,
}

impl ScenarioHarnessKind {
    pub(crate) const ALL: [Self; 4] = [
        Self::RuntimeScenario,
        Self::StandardProtocolScenario,
        Self::RlmProtocolScenario,
        Self::AgentScenario,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::RuntimeScenario => "Runtime Scenario",
            Self::StandardProtocolScenario => "Standard Protocol Scenario",
            Self::RlmProtocolScenario => "RLM Protocol Scenario",
            Self::AgentScenario => "Agent Scenario",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RuntimePerfScenario {
    Standard,
    Rlm,
    StandardToolCalls,
    StandardAsyncToolCompletion,
    RlmToolCalls,
    RlmAsyncToolCompletion,
    RlmProcessHandles,
    RlmTriggerMailPipeline,
    RlmProcessAsyncToolCompletion,
    RlmSubagentSpawn,
    RlmLlmQuery,
    RlmGlobals,
    RlmLargePrint,
    RlmStreamedPairedLashlang,
    RlmLargeToolCatalog,
    RlmObliqueStackMix,
    OpenAiCompatStream,
    StandardShellOutput,
    ToolDiscoverySearch,
    OpenAiResponsesSseParse,
    DirectLlmClient,
    ProcessListStress,
    EmbedStandard,
    EmbedRlm,
    ScopedEffectController,
    StoreReopen,
    SqliteStoreReopen,
    TurnCheckpoint,
    CheckpointStateHotPaths,
    LiveReplayPressure,
    TraceJsonlStandard,
    TraceJsonlExtended,
    QueuedWorkClaimStress,
    TurnInputIngressInterrupt,
    DeepTurnComposition,
    TurnStartGate,
    TurnCancelRoundTrip,
    IngressClaimProjection,
    StoreHardeningHotPaths,
    DurableStandardToolTurnSqlite,
    DurableStandardToolTurnPostgres,
    DurableRlmCheckpointTurnSqlite,
    DurableRlmCheckpointTurnPostgres,
    DurableAgentChildTurnSqlite,
    DurableAgentChildTurnPostgres,
    DurableCheckpointCurveSqlite,
    DurableCheckpointCurvePostgres,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RuntimePerfScenarioMetadata {
    pub(crate) scenario: RuntimePerfScenario,
    pub(crate) name: &'static str,
    pub(crate) execution_mode: ExecutionMode,
    pub(crate) scenario_harness: ScenarioHarnessKind,
    pub(crate) harness_rationale: &'static str,
    pub(crate) correctness_coverage_ids: &'static [&'static str],
}

macro_rules! runtime_perf_metadata {
    ($scenario:ident, $name:literal, $mode:ident, $harness:ident, $rationale:literal) => {
        runtime_perf_metadata!($scenario, $name, $mode, $harness, $rationale, [])
    };
    ($scenario:ident, $name:literal, $mode:ident, $harness:ident, $rationale:literal, [$($coverage_id:literal),* $(,)?]) => {
        RuntimePerfScenarioMetadata {
            scenario: RuntimePerfScenario::$scenario,
            name: $name,
            execution_mode: ExecutionMode::$mode,
            scenario_harness: ScenarioHarnessKind::$harness,
            harness_rationale: $rationale,
            correctness_coverage_ids: &[$($coverage_id),*],
        }
    };
}

impl RuntimePerfScenario {
    pub(crate) const METADATA: [RuntimePerfScenarioMetadata; 47] = [
        runtime_perf_metadata!(
            Standard,
            "standard",
            Standard,
            StandardProtocolScenario,
            "Measures the Standard protocol loop without facade plugins or process orchestration.",
            ["standard_protocol_scenario_projects_initial_request"]
        ),
        runtime_perf_metadata!(
            Rlm,
            "rlm",
            Rlm,
            RlmProtocolScenario,
            "Measures the RLM protocol loop and response classification without facade process graphs.",
            ["rlm_protocol_scenario_prose_only_response_finishes_by_default"]
        ),
        runtime_perf_metadata!(
            StandardToolCalls,
            "standard_tool_calls",
            Standard,
            StandardProtocolScenario,
            "Measures Standard protocol native tool-call projection and continuation.",
            [
                "standard_protocol_scenario_native_tool_loop_reenters_model_after_checkpoint",
                "standard_protocol_scenario_parallel_tool_results_checkpoint_once",
            ]
        ),
        runtime_perf_metadata!(
            StandardAsyncToolCompletion,
            "standard_async_tool_completion",
            Standard,
            StandardProtocolScenario,
            "Measures Standard protocol async tool completion feedback and continuation."
        ),
        runtime_perf_metadata!(
            RlmToolCalls,
            "rlm_tool_calls",
            Rlm,
            RlmProtocolScenario,
            "Measures RLM protocol tool-call classification and feedback."
        ),
        runtime_perf_metadata!(
            RlmAsyncToolCompletion,
            "rlm_async_tool_completion",
            Rlm,
            RlmProtocolScenario,
            "Measures RLM protocol async tool completion feedback and repair/continuation."
        ),
        runtime_perf_metadata!(
            RlmProcessHandles,
            "rlm_process_handles",
            Rlm,
            AgentScenario,
            "Measures facade process-handle orchestration across RLM and Lashlang, beyond protocol-only behavior.",
            ["agent_scenario_nested_process_start_await"]
        ),
        runtime_perf_metadata!(
            RlmTriggerMailPipeline,
            "rlm_trigger_mail_pipeline",
            Rlm,
            AgentScenario,
            "Measures facade/plugin process pipeline behavior initiated through an RLM agent turn."
        ),
        runtime_perf_metadata!(
            RlmProcessAsyncToolCompletion,
            "rlm_process_async_tool_completion",
            Rlm,
            AgentScenario,
            "Measures async process/tool orchestration through the facade, not only RLM response handling.",
            ["agent_scenario_started_process_labeled_tool_call"]
        ),
        runtime_perf_metadata!(
            RlmSubagentSpawn,
            "rlm_subagent_spawn",
            Rlm,
            AgentScenario,
            "Measures subagent facade composition and child-session behavior.",
            ["agent_scenario_started_process_labeled_subagent_spawn"]
        ),
        runtime_perf_metadata!(
            RlmLlmQuery,
            "rlm_llm_query",
            Rlm,
            AgentScenario,
            "Measures facade-level LLM query tool behavior from an RLM agent flow."
        ),
        runtime_perf_metadata!(
            RlmGlobals,
            "rlm_globals",
            Rlm,
            RlmProtocolScenario,
            "Measures RLM protocol prompt/context handling for global bindings."
        ),
        runtime_perf_metadata!(
            RlmLargePrint,
            "rlm_large_print",
            Rlm,
            RlmProtocolScenario,
            "Measures RLM Lashlang cell output projection and print handling."
        ),
        runtime_perf_metadata!(
            RlmStreamedPairedLashlang,
            "rlm_streamed_paired_lashlang",
            Rlm,
            RlmProtocolScenario,
            "Measures RLM streaming cell parsing and protocol continuation."
        ),
        runtime_perf_metadata!(
            RlmLargeToolCatalog,
            "rlm_large_tool_catalog",
            Rlm,
            RlmProtocolScenario,
            "Measures RLM protocol prompt pressure from a large tool catalog."
        ),
        runtime_perf_metadata!(
            RlmObliqueStackMix,
            "rlm_oblique_stack_mix",
            Rlm,
            RlmProtocolScenario,
            "Measures mixed RLM protocol/Lashlang execution pressure without facade subagent ownership."
        ),
        runtime_perf_metadata!(
            OpenAiCompatStream,
            "openai_compat_stream",
            Standard,
            StandardProtocolScenario,
            "Measures Standard protocol streaming provider compatibility as model-response projection."
        ),
        runtime_perf_metadata!(
            StandardShellOutput,
            "standard_shell_output",
            Standard,
            StandardProtocolScenario,
            "Measures Standard protocol handling of native shell output feedback."
        ),
        runtime_perf_metadata!(
            ToolDiscoverySearch,
            "tool_discovery_search",
            Standard,
            StandardProtocolScenario,
            "Measures Standard protocol pressure from tool discovery and request projection."
        ),
        runtime_perf_metadata!(
            OpenAiResponsesSseParse,
            "openai_responses_sse_parse",
            Standard,
            RuntimeScenario,
            "Measures provider/client parser support below protocol and facade ownership."
        ),
        runtime_perf_metadata!(
            DirectLlmClient,
            "direct_llm_client",
            Standard,
            RuntimeScenario,
            "Measures direct LLM client transport below protocol and facade ownership."
        ),
        runtime_perf_metadata!(
            ProcessListStress,
            "process_list_stress",
            Standard,
            RuntimeScenario,
            "Measures process registry/listing runtime behavior below protocol and facade ownership."
        ),
        runtime_perf_metadata!(
            EmbedStandard,
            "embed_standard",
            Standard,
            AgentScenario,
            "Measures embedded facade behavior for a Standard agent flow."
        ),
        runtime_perf_metadata!(
            EmbedRlm,
            "embed_rlm",
            Rlm,
            AgentScenario,
            "Measures embedded facade behavior for an RLM agent flow."
        ),
        runtime_perf_metadata!(
            ScopedEffectController,
            "scoped_effect_controller",
            Standard,
            RuntimeScenario,
            "Measures core scoped-effect runtime behavior below protocol and facade ownership."
        ),
        runtime_perf_metadata!(
            StoreReopen,
            "store_reopen",
            Standard,
            RuntimeScenario,
            "Measures runtime persistence reopen behavior below protocol and facade ownership."
        ),
        runtime_perf_metadata!(
            SqliteStoreReopen,
            "sqlite_store_reopen",
            Standard,
            RuntimeScenario,
            "Measures SQLite runtime persistence reopen behavior below protocol and facade ownership."
        ),
        runtime_perf_metadata!(
            TurnCheckpoint,
            "turn_checkpoint",
            Standard,
            RuntimeScenario,
            "Measures runtime checkpoint paths shared across protocols and below facade ownership.",
            ["runtime_scenario_drains_command_before_turn_work_and_commits_checkpoint"]
        ),
        runtime_perf_metadata!(
            CheckpointStateHotPaths,
            "checkpoint_state_hot_paths",
            Rlm,
            RuntimeScenario,
            "Measures RLM execution-state capture plus keyed checkpoint hydration, budget accounting, and commit below protocol and facade ownership."
        ),
        runtime_perf_metadata!(
            LiveReplayPressure,
            "live_replay_pressure",
            Standard,
            RuntimeScenario,
            "Measures runtime live-replay machinery below protocol and facade ownership."
        ),
        runtime_perf_metadata!(
            TraceJsonlStandard,
            "trace_jsonl_standard",
            Standard,
            StandardProtocolScenario,
            "Measures Standard protocol trace JSONL output for protocol-level turns."
        ),
        runtime_perf_metadata!(
            TraceJsonlExtended,
            "trace_jsonl_extended",
            Rlm,
            RlmProtocolScenario,
            "Measures RLM protocol trace JSONL output for protocol-level turns."
        ),
        runtime_perf_metadata!(
            QueuedWorkClaimStress,
            "queued_work_claim_stress",
            Standard,
            RuntimeScenario,
            "Measures core queued-work claim/renew/complete invariants below protocol and facade ownership.",
            ["runtime_scenario_queued_work_claim_keeps_pending_next_turn_input"]
        ),
        runtime_perf_metadata!(
            TurnInputIngressInterrupt,
            "turn_input_ingress_interrupt",
            Standard,
            RuntimeScenario,
            "Measures core turn-input ingress, interrupt, reclaim, and completion below protocol and facade ownership.",
            ["runtime_scenario_defers_checkpoint_turn_input_and_respects_cancel"]
        ),
        runtime_perf_metadata!(
            DeepTurnComposition,
            "deep_turn_composition",
            Rlm,
            AgentScenario,
            "Measures the composed parent/child turn future with active ingress, tool and process loops, cancellation observation, and timer/await-event durable waits.",
            ["agent_scenario_nested_process_start_await"]
        ),
        runtime_perf_metadata!(
            TurnStartGate,
            "turn_start_gate",
            Standard,
            RuntimeScenario,
            "Measures the inline turn-cancel gate peek through the bounded retry wrapper below protocol and facade ownership."
        ),
        runtime_perf_metadata!(
            TurnCancelRoundTrip,
            "turn_cancel_round_trip",
            Standard,
            RuntimeScenario,
            "Measures request-to-token-to-terminal-seal cancellation below protocol and facade ownership."
        ),
        runtime_perf_metadata!(
            IngressClaimProjection,
            "ingress_claim_projection",
            Rlm,
            RlmProtocolScenario,
            "Measures active-turn input enqueue, checkpoint claim, and next-request RLM projection."
        ),
        runtime_perf_metadata!(
            StoreHardeningHotPaths,
            "store_hardening_hot_paths",
            Standard,
            RuntimeScenario,
            "Measures hardening-era store operations below protocol and facade ownership on the in-memory floor and real SQLite/PostgreSQL backends."
        ),
        runtime_perf_metadata!(
            DurableStandardToolTurnSqlite,
            "durable_standard_tool_turn_sqlite",
            Standard,
            RuntimeScenario,
            "Measures a complete Standard tool turn through the runtime against the decorated SQLite persistence boundary."
        ),
        runtime_perf_metadata!(
            DurableStandardToolTurnPostgres,
            "durable_standard_tool_turn_postgres",
            Standard,
            RuntimeScenario,
            "Measures a complete Standard tool turn through the runtime against the decorated PostgreSQL persistence boundary."
        ),
        runtime_perf_metadata!(
            DurableRlmCheckpointTurnSqlite,
            "durable_rlm_checkpoint_turn_sqlite",
            Rlm,
            RuntimeScenario,
            "Measures a complete RLM checkpoint-producing turn through the runtime against the decorated SQLite persistence boundary."
        ),
        runtime_perf_metadata!(
            DurableRlmCheckpointTurnPostgres,
            "durable_rlm_checkpoint_turn_postgres",
            Rlm,
            RuntimeScenario,
            "Measures a complete RLM checkpoint-producing turn through the runtime against the decorated PostgreSQL persistence boundary."
        ),
        runtime_perf_metadata!(
            DurableAgentChildTurnSqlite,
            "durable_agent_child_turn_sqlite",
            Rlm,
            RuntimeScenario,
            "Measures a complete parent and child agent turn through the runtime against the decorated SQLite persistence boundary."
        ),
        runtime_perf_metadata!(
            DurableAgentChildTurnPostgres,
            "durable_agent_child_turn_postgres",
            Rlm,
            RuntimeScenario,
            "Measures a complete parent and child agent turn through the runtime against the decorated PostgreSQL persistence boundary."
        ),
        runtime_perf_metadata!(
            DurableCheckpointCurveSqlite,
            "durable_checkpoint_curve_sqlite",
            Rlm,
            RuntimeScenario,
            "Measures checkpoint-size attribution inside complete RLM turns against the decorated SQLite persistence boundary."
        ),
        runtime_perf_metadata!(
            DurableCheckpointCurvePostgres,
            "durable_checkpoint_curve_postgres",
            Rlm,
            RuntimeScenario,
            "Measures checkpoint-size attribution inside complete RLM turns against the decorated PostgreSQL persistence boundary."
        ),
    ];
    pub(crate) const KNOWN: [Self; 47] = runtime_perf_known_scenarios();
    // Durable scenarios are intentionally opt-in (or selected by `all`) so the
    // main-push quick profile remains provider- and database-free.
    pub(crate) const DEFAULTS: [Self; 38] = runtime_perf_default_scenarios();

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Self::METADATA
            .iter()
            .find(|metadata| metadata.name == value)
            .map(|metadata| metadata.scenario)
    }

    pub(crate) fn name(self) -> &'static str {
        self.metadata().name
    }

    pub(crate) fn execution_mode(self) -> ExecutionMode {
        self.metadata().execution_mode
    }

    pub(crate) fn scenario_harness(self) -> ScenarioHarnessKind {
        self.metadata().scenario_harness
    }

    pub(crate) fn scenario_harness_rationale(self) -> &'static str {
        self.metadata().harness_rationale
    }

    pub(crate) fn correctness_coverage_ids(self) -> &'static [&'static str] {
        self.metadata().correctness_coverage_ids
    }

    pub(crate) fn is_durable(self) -> bool {
        matches!(
            self,
            Self::DurableStandardToolTurnSqlite
                | Self::DurableStandardToolTurnPostgres
                | Self::DurableRlmCheckpointTurnSqlite
                | Self::DurableRlmCheckpointTurnPostgres
                | Self::DurableAgentChildTurnSqlite
                | Self::DurableAgentChildTurnPostgres
                | Self::DurableCheckpointCurveSqlite
                | Self::DurableCheckpointCurvePostgres
        )
    }

    pub(crate) fn uses_postgres(self) -> bool {
        matches!(
            self,
            Self::DurableStandardToolTurnPostgres
                | Self::DurableRlmCheckpointTurnPostgres
                | Self::DurableAgentChildTurnPostgres
                | Self::DurableCheckpointCurvePostgres
        )
    }

    pub(crate) fn is_checkpoint_curve(self) -> bool {
        matches!(
            self,
            Self::DurableCheckpointCurveSqlite | Self::DurableCheckpointCurvePostgres
        )
    }

    pub(crate) fn has_guard_budget(self) -> bool {
        !self.is_durable()
    }

    pub(crate) fn checkpoint_curve_bytes(self, turn_index: usize) -> Option<usize> {
        self.is_checkpoint_curve().then(|| {
            DURABLE_CHECKPOINT_CURVE_BYTES[turn_index % DURABLE_CHECKPOINT_CURVE_BYTES.len()]
        })
    }

    fn metadata(self) -> &'static RuntimePerfScenarioMetadata {
        Self::METADATA
            .iter()
            .find(|metadata| metadata.scenario == self)
            .expect("runtime perf scenario metadata missing")
    }

    pub(crate) fn standard_context_approach(self) -> Option<StandardContextApproach> {
        if self.execution_mode().is_standard() {
            Some(StandardContextApproach::RollingHistory(
                RollingHistoryConfig,
            ))
        } else {
            None
        }
    }
}

const fn runtime_perf_known_scenarios() -> [RuntimePerfScenario; 47] {
    [
        RuntimePerfScenario::METADATA[0].scenario,
        RuntimePerfScenario::METADATA[1].scenario,
        RuntimePerfScenario::METADATA[2].scenario,
        RuntimePerfScenario::METADATA[3].scenario,
        RuntimePerfScenario::METADATA[4].scenario,
        RuntimePerfScenario::METADATA[5].scenario,
        RuntimePerfScenario::METADATA[6].scenario,
        RuntimePerfScenario::METADATA[7].scenario,
        RuntimePerfScenario::METADATA[8].scenario,
        RuntimePerfScenario::METADATA[9].scenario,
        RuntimePerfScenario::METADATA[10].scenario,
        RuntimePerfScenario::METADATA[11].scenario,
        RuntimePerfScenario::METADATA[12].scenario,
        RuntimePerfScenario::METADATA[13].scenario,
        RuntimePerfScenario::METADATA[14].scenario,
        RuntimePerfScenario::METADATA[15].scenario,
        RuntimePerfScenario::METADATA[16].scenario,
        RuntimePerfScenario::METADATA[17].scenario,
        RuntimePerfScenario::METADATA[18].scenario,
        RuntimePerfScenario::METADATA[19].scenario,
        RuntimePerfScenario::METADATA[20].scenario,
        RuntimePerfScenario::METADATA[21].scenario,
        RuntimePerfScenario::METADATA[22].scenario,
        RuntimePerfScenario::METADATA[23].scenario,
        RuntimePerfScenario::METADATA[24].scenario,
        RuntimePerfScenario::METADATA[25].scenario,
        RuntimePerfScenario::METADATA[26].scenario,
        RuntimePerfScenario::METADATA[27].scenario,
        RuntimePerfScenario::METADATA[28].scenario,
        RuntimePerfScenario::METADATA[29].scenario,
        RuntimePerfScenario::METADATA[30].scenario,
        RuntimePerfScenario::METADATA[31].scenario,
        RuntimePerfScenario::METADATA[32].scenario,
        RuntimePerfScenario::METADATA[33].scenario,
        RuntimePerfScenario::METADATA[34].scenario,
        RuntimePerfScenario::METADATA[35].scenario,
        RuntimePerfScenario::METADATA[36].scenario,
        RuntimePerfScenario::METADATA[37].scenario,
        RuntimePerfScenario::METADATA[38].scenario,
        RuntimePerfScenario::METADATA[39].scenario,
        RuntimePerfScenario::METADATA[40].scenario,
        RuntimePerfScenario::METADATA[41].scenario,
        RuntimePerfScenario::METADATA[42].scenario,
        RuntimePerfScenario::METADATA[43].scenario,
        RuntimePerfScenario::METADATA[44].scenario,
        RuntimePerfScenario::METADATA[45].scenario,
        RuntimePerfScenario::METADATA[46].scenario,
    ]
}

const fn runtime_perf_default_scenarios() -> [RuntimePerfScenario; 38] {
    [
        RuntimePerfScenario::METADATA[0].scenario,
        RuntimePerfScenario::METADATA[1].scenario,
        RuntimePerfScenario::METADATA[2].scenario,
        RuntimePerfScenario::METADATA[3].scenario,
        RuntimePerfScenario::METADATA[4].scenario,
        RuntimePerfScenario::METADATA[5].scenario,
        RuntimePerfScenario::METADATA[6].scenario,
        RuntimePerfScenario::METADATA[7].scenario,
        RuntimePerfScenario::METADATA[8].scenario,
        RuntimePerfScenario::METADATA[9].scenario,
        RuntimePerfScenario::METADATA[10].scenario,
        RuntimePerfScenario::METADATA[11].scenario,
        RuntimePerfScenario::METADATA[12].scenario,
        RuntimePerfScenario::METADATA[13].scenario,
        RuntimePerfScenario::METADATA[14].scenario,
        RuntimePerfScenario::METADATA[15].scenario,
        RuntimePerfScenario::METADATA[16].scenario,
        RuntimePerfScenario::METADATA[17].scenario,
        RuntimePerfScenario::METADATA[18].scenario,
        RuntimePerfScenario::METADATA[19].scenario,
        RuntimePerfScenario::METADATA[20].scenario,
        RuntimePerfScenario::METADATA[21].scenario,
        RuntimePerfScenario::METADATA[22].scenario,
        RuntimePerfScenario::METADATA[23].scenario,
        RuntimePerfScenario::METADATA[24].scenario,
        RuntimePerfScenario::METADATA[25].scenario,
        RuntimePerfScenario::METADATA[26].scenario,
        RuntimePerfScenario::METADATA[27].scenario,
        RuntimePerfScenario::METADATA[28].scenario,
        RuntimePerfScenario::METADATA[29].scenario,
        RuntimePerfScenario::METADATA[30].scenario,
        RuntimePerfScenario::METADATA[31].scenario,
        RuntimePerfScenario::METADATA[32].scenario,
        RuntimePerfScenario::METADATA[33].scenario,
        RuntimePerfScenario::METADATA[34].scenario,
        RuntimePerfScenario::METADATA[35].scenario,
        RuntimePerfScenario::METADATA[36].scenario,
        RuntimePerfScenario::METADATA[37].scenario,
    ]
}
