use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use lash::usage::SessionUsageReport;
use lash_core::TestProcessRegistryWriteExt;
use lash_core::llm::types::{LlmResponse, LlmUsage};
use lash_core::runtime::{
    DeliveryPolicy, QueuedWorkBatchDraft, QueuedWorkClaimBoundary, QueuedWorkCompletion,
    RuntimeScope, RuntimeSubject, RuntimeTurnPhase, RuntimeTurnPhaseProbe, SessionCommand,
};
use lash_core::sansio::{
    ChatContextProjector, CompletedToolCall, PendingToolCall, ProtocolDriverHandle,
    WaitingExecState, WaitingLlmState,
};
use lash_core::store::GraphAppend;
use lash_core::{
    AttachmentIntent, AttachmentOwnerKind, DriverAction, DriverContextView, Effect, ExecResponse,
    LiveReplayOutcome, LiveReplayStore, LiveReplaySubscribeOutcome, Message, MessageRole, Part,
    ProtocolTurnOptions, QueuedWorkStore, RuntimeCommit, RuntimeSessionState, SessionCommitStore,
    SessionExecutionLeaseStore, SessionObservationEventPayload, SessionRevision,
    SessionStoreFactory, TokenUsage, ToolCallOutput, ToolCancellation, ToolFailure,
    ToolFailureClass, TurnInput, TurnInputStore, TurnMachine, TurnMachineConfig,
    facade_support::ModelToolReturn, facade_support::Response, facade_support::TurnFinish,
    facade_support::TurnOutcome, facade_support::shared_parts,
};
use lash_sansio::sync::MutexExt;
use serde::Serialize;
use stats_alloc::Stats;
use tokio_util::sync::CancellationToken;

use crate::perf_support::memory::{ProcessMemorySample, diff_opt_i64, process_memory_sample};
use crate::perf_support::metrics::BasicMetricSummary as RuntimePerfMetricSummary;
use crate::perf_support::scheduler::RuntimeSchedulerSample;
use crate::perf_support::stack::StackProfile;
use crate::perf_support::tempdir::make_temp_bench_dir;
use crate::perf_support::time::{elapsed_ms, round3};

use super::harness::{
    RuntimePerfTraceConfig, build_embed_core, build_runtime_with_postgres_store,
    build_runtime_with_sqlite_store, build_runtime_with_store,
    durable_postgres_session_store_factory_without_commit_measurement,
    durable_sqlite_session_store_factory_without_commit_measurement, seed_runtime_state,
    validate_runtime_perf_turn,
};
use super::prompt::benchmark_prompt;
use super::scenarios::RuntimePerfScenario;
use super::store::{RuntimePerfStore, RuntimePerfStoreTiming};

mod types;
pub(crate) use types::*;
mod phase_probe;
pub(crate) use phase_probe::*;
mod contention;
pub(crate) use contention::*;
mod live_replay;
use live_replay::*;
mod provider_scenarios;
use provider_scenarios::*;
mod process_stress;
use process_stress::*;
mod queued_work;
use queued_work::*;
mod checkpoint;
pub(crate) use checkpoint::*;
mod checkpoint_curve;
pub(crate) use checkpoint_curve::*;
mod store_hardening;
pub(crate) use store_hardening::*;
mod high_traffic;
use high_traffic::*;
