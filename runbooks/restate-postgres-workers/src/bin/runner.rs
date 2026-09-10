use anyhow::{Context, Result};
use lash::TurnId;
use lash::sync::MutexExt;
use lash::triggers::{TriggerOccurrenceRequest, empty_trigger_source_key};
use lash_core::AwaitEventResolver as _;
use lash_core::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, Resolution, ScopedEffectController,
    SessionCommitStore, facade_support::NativeRuntimeEffectController, facade_support::TurnAddress,
    facade_support::TurnCancelOutcome, facade_support::TurnCancelRequest,
    facade_support::TurnOutcome, facade_support::TurnStop, facade_support::TurnTerminal,
    facade_support::TurnWorkDriver,
};
use lash_postgres_store::PostgresStorage;
use lash_restate::{
    RestateAdminClient, RestateConnection, RestateEffectHost, RestateIngressClient,
    RestateInvocationId, RestateInvocationStatus, RestateProcessDeployment, RestateTurnDeployment,
};
use lash_restate_postgres_workers_e2e::{
    ATTACHMENT_MIME, BUTTON_SOURCE_TYPE, DEFAULT_SESSION_ID, DirectDurableWaitAwaitRequest,
    DirectDurableWaitAwaitResponse, DirectDurableWaitResolveRequest,
    DirectDurableWaitResolveResponse, EXPECTED_ASYNC_TEXT, EXPECTED_DURABLE_INPUT_TEXT,
    EXPECTED_FINAL_TEXT, EXPECTED_FRAME_SWITCH_CANCEL_TEXT, EXPECTED_FRAME_SWITCH_TEXT,
    EXPECTED_PARENT_DURABLE_INPUT_TEXT, EXPECTED_SEGMENT_LOOP_TEXT, EXPECTED_TOOL_BATCH_TEXT,
    ProcessSignalRequest, TURN_WORKFLOW_NAME, TurnRequest, TurnResponse, TurnScenario,
    build_e2e_core, e2e_tokio_thread_stack_bytes, ensure_e2e_schema, env,
    expected_attachment_bytes, process_registry_from_storage, record_terminal_result,
    reset_e2e_rows, s3_store_from_env, turn_session_id,
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;

const DEFAULT_RUNNER_STALL_TIMEOUT: Duration = Duration::from_secs(240);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkflowSegment {
    One,
    Two,
}

impl WorkflowSegment {
    const fn number(self) -> u8 {
        match self {
            Self::One => 1,
            Self::Two => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SegmentSelection {
    All,
    One,
    Two,
}

impl SegmentSelection {
    fn from_env() -> Result<Self> {
        match std::env::var("LASH_E2E_WORKFLOW_SEGMENT") {
            Err(std::env::VarError::NotPresent) => Ok(Self::All),
            Ok(raw) if raw.is_empty() => Ok(Self::All),
            Ok(raw) if raw == "1" => Ok(Self::One),
            Ok(raw) if raw == "2" => Ok(Self::Two),
            Ok(raw) => anyhow::bail!("LASH_E2E_WORKFLOW_SEGMENT must be `1` or `2`, got `{raw}`"),
            Err(error) => Err(error).context("read LASH_E2E_WORKFLOW_SEGMENT"),
        }
    }

    const fn includes(self, segment: WorkflowSegment) -> bool {
        matches!(self, Self::All)
            || matches!(
                (self, segment),
                (Self::One, WorkflowSegment::One) | (Self::Two, WorkflowSegment::Two)
            )
    }

    const fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::One => "1",
            Self::Two => "2",
        }
    }
}

#[derive(Clone, Copy)]
struct WorkflowSpec {
    id: &'static str,
    segment: WorkflowSegment,
}

// FIG-2040 dependency map and measured split (2026-08-24 baseline progress log):
//
// Segment 1 (~3m22s) owns the cold-process AwaitEvent vectors and these chains:
// - e2e-main -> queued work -> e2e-main-wake
// - e2e-trigger-setup -> button delivery -> trigger process terminal
// - e2e-signal-suspend-setup -> e2e-failover -> e2e-failover-wake ->
//   e2e-signal-first -> e2e-signal-second -> signal process terminal
// - e2e-process-llm-query -> e2e-process-llm-query-replay
// - e2e-durable-input -> e2e-parent-durable-input-after-child
// The async-completion workflow is independent and stays in its original position.
//
// Segment 2 (~4m31s) starts with the independent engine promise conformance and
// owns these chains:
// - e2e-tool-batch -> e2e-tool-batch-failover
// - the four frame-switch workflows, in their current order
// - e2e-suspended-sleep-cancel
// - e2e-engine-restart-{cancel,suspended-sleep,complete}, including the complete
//   engine-restart-ready/engine-restart-complete shell handshake
// - the four ordered turn-control workflows, then the durable-wait index gates
//   and e2e-turn-break-glass (last because it strands a shared lease); the
//   index gates and break-glass ride segment 2 but are NOT inventory members —
//   they produce no terminal-result row and are covered by their own driver
//   assertions, not the completion manifest
//
// Each CI leg has a fresh stack, so schema/reset/readiness/deployment bootstrap is
// intentionally repeated. No workflow setup is duplicated between segments.
// This is the single authoritative workflow inventory used by execution manifests
// and the CI coverage summary; workflow ids must not be copied into those layers.
// Pin the inventory size: the runner emits both the inventory and the
// manifests, so shrinking the workflow set would otherwise pass the CI
// coverage summary silently. Removing or adding a workflow must touch this
// pin, forcing the change into reviewer view.
const EXPECTED_WORKFLOW_INVENTORY_LEN: usize = 28;
const _: () = assert!(WORKFLOW_INVENTORY.len() == EXPECTED_WORKFLOW_INVENTORY_LEN);

const WORKFLOW_INVENTORY: &[WorkflowSpec] = &[
    WorkflowSpec {
        id: "e2e-main",
        segment: WorkflowSegment::One,
    },
    WorkflowSpec {
        id: "e2e-main-wake",
        segment: WorkflowSegment::One,
    },
    WorkflowSpec {
        id: "e2e-trigger-setup",
        segment: WorkflowSegment::One,
    },
    WorkflowSpec {
        id: "e2e-signal-suspend-setup",
        segment: WorkflowSegment::One,
    },
    WorkflowSpec {
        id: "e2e-failover",
        segment: WorkflowSegment::One,
    },
    WorkflowSpec {
        id: "e2e-failover-wake",
        segment: WorkflowSegment::One,
    },
    WorkflowSpec {
        id: "e2e-signal-first",
        segment: WorkflowSegment::One,
    },
    WorkflowSpec {
        id: "e2e-signal-second",
        segment: WorkflowSegment::One,
    },
    WorkflowSpec {
        id: "e2e-async-completion",
        segment: WorkflowSegment::One,
    },
    WorkflowSpec {
        id: "e2e-process-llm-query",
        segment: WorkflowSegment::One,
    },
    WorkflowSpec {
        id: "e2e-process-llm-query-replay",
        segment: WorkflowSegment::One,
    },
    WorkflowSpec {
        id: "e2e-durable-input",
        segment: WorkflowSegment::One,
    },
    WorkflowSpec {
        id: "e2e-parent-durable-input-after-child",
        segment: WorkflowSegment::One,
    },
    WorkflowSpec {
        id: "e2e-tool-batch",
        segment: WorkflowSegment::Two,
    },
    WorkflowSpec {
        id: "e2e-tool-batch-failover",
        segment: WorkflowSegment::Two,
    },
    WorkflowSpec {
        id: "e2e-segment-loop",
        segment: WorkflowSegment::Two,
    },
    WorkflowSpec {
        id: "e2e-frame-switch-queued",
        segment: WorkflowSegment::Two,
    },
    WorkflowSpec {
        id: "e2e-frame-switch-prepared",
        segment: WorkflowSegment::Two,
    },
    WorkflowSpec {
        id: "e2e-frame-switch-crash",
        segment: WorkflowSegment::Two,
    },
    WorkflowSpec {
        id: "e2e-frame-switch-cancel",
        segment: WorkflowSegment::Two,
    },
    WorkflowSpec {
        id: "e2e-suspended-sleep-cancel",
        segment: WorkflowSegment::Two,
    },
    WorkflowSpec {
        id: "e2e-engine-restart-suspended-sleep",
        segment: WorkflowSegment::Two,
    },
    WorkflowSpec {
        id: "e2e-engine-restart-cancel",
        segment: WorkflowSegment::Two,
    },
    WorkflowSpec {
        id: "e2e-engine-restart-complete",
        segment: WorkflowSegment::Two,
    },
    WorkflowSpec {
        id: "e2e-turn-cancel-before-start",
        segment: WorkflowSegment::Two,
    },
    WorkflowSpec {
        id: "e2e-turn-cancel-cross-process",
        segment: WorkflowSegment::Two,
    },
    WorkflowSpec {
        id: "e2e-turn-cancel-seal-race",
        segment: WorkflowSegment::Two,
    },
    WorkflowSpec {
        id: "e2e-turn-cancel-crash-recovery",
        segment: WorkflowSegment::Two,
    },
];

struct RunnerProgress {
    last_update: Instant,
    description: String,
    completed_workflows: BTreeSet<String>,
}

fn runner_progress() -> &'static Mutex<RunnerProgress> {
    static PROGRESS: OnceLock<Mutex<RunnerProgress>> = OnceLock::new();
    PROGRESS.get_or_init(|| {
        Mutex::new(RunnerProgress {
            last_update: Instant::now(),
            description: "runner startup".to_string(),
            completed_workflows: BTreeSet::new(),
        })
    })
}

fn report_workflow_progress(workflow_id: &str, phase: &str) {
    let description = format!("workflow={workflow_id} phase={phase}");
    {
        let mut progress = runner_progress().lock_recover();
        progress.last_update = Instant::now();
        progress.description.clone_from(&description);
        if phase == "completed" {
            progress.completed_workflows.insert(workflow_id.to_string());
        }
    }
    eprintln!(
        "[{}] workers-e2e progress: {description}",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    );
}

fn runner_stall_timeout() -> Result<Duration> {
    let Some(raw) = std::env::var("LASH_E2E_STALL_TIMEOUT_SECS").ok() else {
        return Ok(DEFAULT_RUNNER_STALL_TIMEOUT);
    };
    let seconds = raw
        .parse::<u64>()
        .with_context(|| format!("LASH_E2E_STALL_TIMEOUT_SECS must be seconds, got `{raw}`"))?;
    anyhow::ensure!(seconds > 0, "LASH_E2E_STALL_TIMEOUT_SECS must be positive");
    Ok(Duration::from_secs(seconds))
}

fn selected_workflow_ids(selection: SegmentSelection) -> Vec<&'static str> {
    WORKFLOW_INVENTORY
        .iter()
        .filter(|workflow| selection.includes(workflow.segment))
        .map(|workflow| workflow.id)
        .collect()
}

fn completed_workflow_manifest_path(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.filter(|path| !path.is_empty()).map(PathBuf::from)
}

fn write_completed_workflow_manifest(selection: SegmentSelection) -> Result<()> {
    let expected = selected_workflow_ids(selection)
        .into_iter()
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    let completed = runner_progress().lock_recover().completed_workflows.clone();
    anyhow::ensure!(
        completed == expected,
        "completed workflow set did not match segment {} inventory: missing={:?}, unexpected={:?}",
        selection.label(),
        expected.difference(&completed).collect::<Vec<_>>(),
        completed.difference(&expected).collect::<Vec<_>>()
    );

    let Some(path) =
        completed_workflow_manifest_path(std::env::var_os("LASH_E2E_COMPLETED_WORKFLOW_MANIFEST"))
    else {
        return Ok(());
    };
    let contents = WORKFLOW_INVENTORY
        .iter()
        .filter(|workflow| selection.includes(workflow.segment))
        .map(|workflow| workflow.id)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, format!("{contents}\n"))
        .with_context(|| format!("write completed workflow manifest `{}`", path.display()))?;
    Ok(())
}

fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--workflow-inventory") {
        for workflow in WORKFLOW_INVENTORY {
            println!("{}\t{}", workflow.segment.number(), workflow.id);
        }
        return Ok(());
    }
    lash_core::panic_containment::set_loud(true);
    let stack_bytes = e2e_tokio_thread_stack_bytes()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(stack_bytes)
        .build()
        .context("build e2e runner Tokio runtime")?
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let database_url = lash_restate_postgres_workers_e2e::required_env("DATABASE_URL")?;
    let storage = wait_for_postgres(&database_url).await?;
    ensure_e2e_schema(storage.pool()).await?;
    reset_e2e_rows(storage.pool()).await?;

    let trace_dir = std::env::var("LASH_E2E_TRACE_DIR").ok().map(PathBuf::from);
    if let Some(dir) = &trace_dir {
        reset_trace_dir(dir)?;
    }

    let attachment_store = s3_store_from_env()?;
    wait_for_minio(&attachment_store).await?;
    let mock_provider_base_url = env("MOCK_PROVIDER_BASE_URL", "http://mock-provider:18001");
    wait_for_mock_provider(&mock_provider_base_url).await?;

    let admin_url = env("RESTATE_ADMIN_URL", "http://restate:9070");
    let deployment_url = env("WORKER_DEPLOYMENT_URL", "http://worker-proxy:18100");
    register_restate_deployment(&admin_url, &deployment_url).await?;
    let ingress_url = env("RESTATE_INGRESS_URL", "http://restate:8080");
    let watchdog = tokio::spawn(runner_stall_watchdog(
        storage.pool().clone(),
        admin_url.clone(),
        runner_stall_timeout()?,
    ));

    let selection = SegmentSelection::from_env()?;
    let segment_one = if selection.includes(WorkflowSegment::One) {
        Some(
            run_workflow_segment_one(
                &storage,
                &admin_url,
                &ingress_url,
                &mock_provider_base_url,
                trace_dir.clone(),
            )
            .await?,
        )
    } else {
        None
    };

    if std::env::var("LASH_E2E_WAKE_RCA_ONLY").as_deref() == Ok("1") {
        anyhow::ensure!(
            selection.includes(WorkflowSegment::One),
            "LASH_E2E_WAKE_RCA_ONLY requires workflow segment 1"
        );
        run_engine_promise_gates(&admin_url, &ingress_url).await?;
        assert_durable_input_attempts(storage.pool()).await?;
        println!(
            "wake RCA soak passed: failover; queued-drain; waiter-before-resolution; resolution-before-waiter"
        );
        watchdog.abort();
        return Ok(());
    }
    if selection == SegmentSelection::One {
        assert_no_active_lash_restate_invocations(&admin_url).await?;
        assert_no_problem_lash_restate_invocations(&admin_url).await?;
    }

    if selection.includes(WorkflowSegment::Two) {
        run_workflow_segment_two(&storage, &ingress_url, &admin_url).await?;
    }

    let expected_workflows = selected_workflow_ids(selection);
    let responses = wait_for_terminal_results(storage.pool(), &expected_workflows).await?;

    if selection.includes(WorkflowSegment::One) {
        assert_processes_terminal(storage.pool()).await?;
    }
    assert_no_duplicate_runtime_rows(storage.pool()).await?;
    assert_worker_distribution(storage.pool()).await?;
    assert_failover(storage.pool(), selection).await?;
    assert_provider_calls(storage.pool(), selection).await?;
    if selection.includes(WorkflowSegment::Two) {
        assert_frame_switch_provider_order(storage.pool()).await?;
    }
    assert_tool_and_turn_telemetry(storage.pool(), selection).await?;
    if selection.includes(WorkflowSegment::Two) {
        assert_tool_batch_side_effects(storage.pool()).await?;
    }
    if selection.includes(WorkflowSegment::One) {
        assert_durable_input_attempts(storage.pool()).await?;
        let output = segment_one.as_ref().context("segment 1 output missing")?;
        assert_trigger_delivery(storage.pool(), &output.trigger_process_id).await?;
    }
    assert_attachments_round_trip(storage.pool(), &attachment_store, &responses).await?;
    if selection.includes(WorkflowSegment::One) {
        assert_reopened_session_agrees(
            &storage,
            &mock_provider_base_url,
            trace_dir.clone(),
            &ingress_url,
            &responses,
        )
        .await?;
    }
    if let Some(dir) = &trace_dir {
        assert_traces(dir, selection).await?;
    }
    if selection.includes(WorkflowSegment::Two) {
        drive_break_glass_scenario(&storage, &ingress_url, &admin_url).await?;
    }
    assert_no_active_lash_restate_invocations(&admin_url).await?;
    write_completed_workflow_manifest(selection)?;

    if selection == SegmentSelection::All {
        let output = segment_one.as_ref().context("segment 1 output missing")?;
        println!(
            "restate-postgres-workers e2e passed: {} workflows; suspended-sleep gates: post-suspension-cancel; engine-restart gates: journal-replay, suspended-sleep-cancel, post-restart-cancel-evidence, post-restart-completion; turn-control gates: cross-process, before-start, seal-race, crash-recovery, terminal-attach, break-glass-negative; trigger process {}; signal process {}; traces {}",
            responses.len(),
            output.trigger_process_id,
            output.signal_process_id,
            trace_dir
                .as_ref()
                .map(|dir| dir.display().to_string())
                .unwrap_or_else(|| "disabled".to_string())
        );
    } else {
        println!(
            "restate-postgres-workers e2e segment {} passed: {} workflows; traces {}",
            selection.label(),
            responses.len(),
            trace_dir
                .as_ref()
                .map(|dir| dir.display().to_string())
                .unwrap_or_else(|| "disabled".to_string())
        );
    }
    watchdog.abort();
    Ok(())
}

#[path = "runner/control_scenarios.rs"]
mod control_scenarios;
#[path = "runner/environment.rs"]
mod environment;
#[path = "runner/process_assertions.rs"]
mod process_assertions;
#[path = "runner/response_assertions.rs"]
mod response_assertions;
#[path = "runner/segment_one.rs"]
mod segment_one;

#[cfg(test)]
#[path = "runner/tests.rs"]
mod tests;

use control_scenarios::*;
use environment::*;
use process_assertions::*;
use response_assertions::*;
use segment_one::*;
