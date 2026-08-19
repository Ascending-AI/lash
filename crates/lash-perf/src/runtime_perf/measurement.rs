use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use lash::usage::SessionUsageReport;
use lash_core::llm::types::{LlmResponse, LlmUsage};
use lash_core::runtime::{
    DeliveryPolicy, QueuedWorkBatchDraft, QueuedWorkClaimBoundary, QueuedWorkCompletion,
    QueuedWorkPayload, RuntimeScope, RuntimeSubject, RuntimeTurnPhase, RuntimeTurnPhaseProbe,
    SessionCommand,
};
use lash_core::sansio::{
    ChatContextProjector, CompletedToolCall, PendingToolCall, ProtocolDriverHandle,
    WaitingExecState, WaitingLlmState,
};
use lash_core::store::GraphAppend;
use lash_core::{
    AttachmentIntent, AttachmentOwnerKind, DriverAction, DriverContextView, Effect, ExecResponse,
    InputItem, LiveReplayResult, LiveReplayStore, LiveReplaySubscribeOutcome, Message, MessageRole,
    Part, ProtocolTurnOptions, QueuedWorkStore, RuntimeCommit, RuntimeSessionState,
    SessionCommitStore, SessionExecutionLeaseStore, SessionObservationEventPayload,
    SessionRevision, SessionStoreFactory, TokenUsage, ToolCallOutput, ToolCancellation,
    ToolFailure, ToolFailureClass, TurnInput, TurnInputStore, TurnMachine, TurnMachineConfig,
    facade_support::ModelToolReturn, facade_support::Response, facade_support::TurnFinish,
    facade_support::TurnOutcome, facade_support::shared_parts,
};
use lash_protocol_rlm::RlmTurnInputExt;
use serde::Serialize;
use stats_alloc::Stats;
use tokio_util::sync::CancellationToken;

use crate::perf_support::memory::{ProcessMemorySample, diff_opt_i64, process_memory_sample};
use crate::perf_support::metrics::BasicMetricSummary as RuntimePerfMetricSummary;
use crate::perf_support::stack::StackProfile;
use crate::perf_support::tempdir::make_temp_bench_dir;
use crate::perf_support::time::{elapsed_ms, round3};

use super::harness::{
    RuntimePerfTraceConfig, benchmark_prompt, build_embed_core, build_runtime_with_sqlite_store,
    build_runtime_with_store, prepare_turn, rlm_perf_projected_bindings, seed_runtime_state,
    validate_runtime_perf_turn,
};
use super::scenarios::RuntimePerfScenario;
use super::store::RuntimePerfStore;

include!("measurement/types.rs");
include!("measurement/phase_probe.rs");
include!("measurement/live_replay.rs");
include!("measurement/provider_scenarios.rs");
include!("measurement/process_stress.rs");
include!("measurement/queued_work.rs");
include!("measurement/checkpoint.rs");
include!("measurement/store_hardening.rs");
