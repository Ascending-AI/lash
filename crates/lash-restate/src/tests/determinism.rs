//! The Restate leg of the determinism harness (FIG-3672), over the in-tree
//! Endpoint double.
//!
//! [`backend`] is shaped like `lash_restate_test::backend(seed, config)`
//! (FIG-3665): a seed and a config in, an engine leg out. Its
//! [`DeterminismEngine`] implementation is the seam the server double plugs
//! into when it lands, so a check written against this constructor moves to
//! the double by changing the constructor it calls.
//!
//! The drive runs inside a real `restate_sdk` workflow handler, on the
//! handler's own `RestateRuntimeEffectController`, so the SDK's journal and
//! lash-restate's envelope fence are the code under test:
//!
//! - **fresh**: the invocation is driven round by round, each round replaying
//!   the journal so far with every proposed run acknowledged, until the
//!   handler writes its output;
//! - **cold replay**: the whole journal is replayed on the same endpoint;
//! - **separate worker**: the whole journal is replayed on a freshly built
//!   endpoint, whose drive the config's worker builds anew, on its own thread
//!   and runtime;
//! - **perturbed**: a cold replay whose completion notifications are delivered
//!   in a seeded order.
//!
//! A replay's transcript is the journal it accepted — the SDK's positional
//! check and the envelope fence refuse any replay that issues something else —
//! plus any command the replay journaled anew, plus its committed output.

use lash_core::engine::testing::{
    DeterminismCheck, DeterminismEngine, DriveTranscript, EngineRun, FailureCause, ReplayMode,
    RunFailure, RunMode, SeededRng, TranscriptEntry,
};

use super::endpoint_protocol::{restate_run_proposals, split_frames};
use super::*;

const HOST: &str = "LashDeterminismHost";
const MAX_ROUNDS: usize = 64;

/// A drive run on one handler execution: effects through the handler's scoped
/// controller, returning the bytes it commits.
type RestateDrive = Arc<
    dyn for<'a> Fn(
            ScopedEffectController<'a>,
        )
            -> Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send + 'a>>
        + Send
        + Sync,
>;

fn restate_drive<F>(drive: F) -> RestateDrive
where
    F: for<'a> Fn(
            ScopedEffectController<'a>,
        )
            -> Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send + 'a>>
        + Send
        + Sync
        + 'static,
{
    Arc::new(drive)
}

/// What one Restate leg runs.
#[derive(Clone)]
struct EndpointConfig {
    /// The scope the drive's controller is admitted for.
    scope: ExecutionScope,
    /// Builds one worker's drive, with whatever process-local state it holds.
    worker: Arc<dyn Fn() -> RestateDrive + Send + Sync>,
}

/// The Restate leg under `seed`.
fn backend(seed: u64, config: EndpointConfig) -> EndpointBackend {
    EndpointBackend {
        workflow_key: format!("determinism-{seed:x}"),
        endpoint: determinism_endpoint(&config),
        config,
    }
}

struct EndpointBackend {
    workflow_key: String,
    endpoint: Endpoint,
    config: EndpointConfig,
}

/// The fresh run's handler outputs, in round order: its journal.
struct EndpointHistory {
    outputs: Vec<Bytes>,
}

#[restate_sdk::workflow]
trait LashDeterminismHost {
    async fn run(input: Json<String>) -> HandlerResult<Json<serde_json::Value>>;
}

struct LashDeterminismHostImpl {
    scope: ExecutionScope,
    drive: RestateDrive,
}

impl LashDeterminismHost for LashDeterminismHostImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(_key): Json<String>,
    ) -> HandlerResult<Json<serde_json::Value>> {
        let controller = RestateRuntimeEffectController::new(ctx, test_restate_authority_id());
        let scoped = controller
            .scoped_effect_controller(durable_admission(&self.scope))
            .map_err(TerminalError::from_error)?;
        let committed = (self.drive)(scoped).await.map_err(TerminalError::new)?;
        Ok(Json(committed))
    }
}

fn determinism_endpoint(config: &EndpointConfig) -> Endpoint {
    Endpoint::builder()
        .bind(
            LashDeterminismHostImpl {
                scope: config.scope.clone(),
                drive: (config.worker)(),
            }
            .serve(),
        )
        .build()
}

impl EndpointBackend {
    fn block_on<T>(&self, work: impl Future<Output = T>) -> Result<T, RunFailure> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| engine_failure(format!("could not build a runtime: {error}")))?;
        Ok(runtime.block_on(work))
    }

    /// The journal replayed with every run acknowledged. The handler's own
    /// output is left out, so a replay of a finished run re-runs the handler to
    /// its end and must write the same output again.
    fn replay_body(
        &self,
        history: &EndpointHistory,
        perturb: Option<u64>,
    ) -> Result<Bytes, RunFailure> {
        let journal = history
            .outputs
            .iter()
            .map(|output| without_handler_output(output))
            .collect::<Result<Vec<_>, _>>()?;
        let outputs = journal.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let body = encode_recorded_commands_replay(
            &self.workflow_key,
            &self.workflow_key,
            &outputs,
            |_| None,
        )
        .map_err(|error| engine_failure(format!("could not encode the replay: {error:?}")))?;
        match perturb {
            None => Ok(body),
            Some(seed) => reorder_notifications(&body, seed),
        }
    }
}

impl DeterminismEngine for EndpointBackend {
    type History = EndpointHistory;

    fn fresh(&self, _seed: u64) -> Result<EngineRun<EndpointHistory>, RunFailure> {
        let history = self.block_on(async {
            let mut history = EndpointHistory {
                outputs: Vec::new(),
            };
            for _ in 0..MAX_ROUNDS {
                let body = self.replay_body(&history, None)?;
                let output = invoke(&self.endpoint, body).await?;
                let done = handler_finished(&output)?;
                history.outputs.push(output);
                if done {
                    return Ok(history);
                }
            }
            Err(engine_failure(format!(
                "the handler did not finish in {MAX_ROUNDS} rounds"
            )))
        })??;
        let last = history.outputs.last().cloned().unwrap_or_default();
        Ok(EngineRun {
            transcript: transcript(&history.outputs, &last)?,
            history,
        })
    }

    fn replay(
        &self,
        history: &EndpointHistory,
        mode: ReplayMode,
    ) -> Result<DriveTranscript, RunFailure> {
        let output = match mode {
            ReplayMode::Cold => {
                let body = self.replay_body(history, None)?;
                self.block_on(invoke(&self.endpoint, body))??
            }
            ReplayMode::Perturbed { seed } => {
                let body = self.replay_body(history, Some(seed))?;
                self.block_on(invoke(&self.endpoint, body))??
            }
            ReplayMode::SeparateWorker => {
                let body = self.replay_body(history, None)?;
                let config = self.config.clone();
                std::thread::Builder::new()
                    .name("lash-restate-determinism-worker".to_string())
                    .spawn(move || {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(|error| engine_failure(error.to_string()))?;
                        let endpoint = determinism_endpoint(&config);
                        runtime.block_on(invoke(&endpoint, body))
                    })
                    .map_err(|error| engine_failure(error.to_string()))?
                    .join()
                    .map_err(|_| engine_failure("the separate worker panicked".to_string()))??
            }
        };
        if !handler_finished(&output)? {
            return Err(engine_failure(
                "the replay suspended instead of finishing".to_string(),
            ));
        }
        let mut outputs = history.outputs.clone();
        outputs.push(output.clone());
        transcript(&outputs, &output)
    }
}

async fn invoke(endpoint: &Endpoint, body: Bytes) -> Result<Bytes, RunFailure> {
    invoke_endpoint_body(endpoint, HOST, "run", body)
        .await
        .map_err(|error| engine_failure(format!("{error:?}")))
}

/// Whether `output` ends the invocation with the handler's output. A handler
/// error is the engine refusing the run.
fn handler_finished(output: &[u8]) -> Result<bool, RunFailure> {
    if let Some(message) = restate_error_message(output) {
        return Err(engine_failure(message));
    }
    let types = restate_message_types(output)
        .ok_or_else(|| engine_failure("the handler output did not decode".to_string()))?;
    Ok(types.contains(&RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE))
}

/// The command stream every output journaled, then the committed output.
fn transcript(outputs: &[Bytes], last: &[u8]) -> Result<DriveTranscript, RunFailure> {
    let undecodable = || engine_failure("a handler output did not decode".to_string());
    let mut entries = Vec::new();
    for output in outputs {
        for command in restate_recorded_commands(output).ok_or_else(undecodable)? {
            if command.message_type == RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE {
                continue;
            }
            entries.push(TranscriptEntry::Command {
                key: format!("journal/{}", entries.len()),
                kind: format!("{:#06x}", command.message_type),
                bytes: hex(&command.frame),
            });
        }
        for (completion_id, value) in restate_run_proposals(output).ok_or_else(undecodable)? {
            entries.push(TranscriptEntry::Command {
                key: format!("run-completion/{completion_id}"),
                kind: "proposed-run-value".to_string(),
                bytes: String::from_utf8_lossy(&value).into_owned(),
            });
        }
    }
    let committed = restate_output_json::<serde_json::Value>(last).ok_or_else(undecodable)?;
    entries.push(TranscriptEntry::Commit {
        bytes: committed.to_string(),
    });
    Ok(DriveTranscript { entries })
}

fn without_handler_output(output: &[u8]) -> Result<Vec<u8>, RunFailure> {
    let frames = split_frames(output)
        .ok_or_else(|| engine_failure("a handler output did not split".to_string()))?;
    Ok(frames
        .into_iter()
        .filter(|frame| message_type(frame) != Some(RESTATE_OUTPUT_COMMAND_MESSAGE_TYPE))
        .flatten()
        .copied()
        .collect())
}

fn message_type(frame: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes([*frame.first()?, *frame.get(1)?]))
}

/// Deliver the replay's completion notifications in a seeded order. Restate
/// answers a completion by id, not by position, so a deterministic handler
/// cannot tell the orders apart.
fn reorder_notifications(body: &[u8], seed: u64) -> Result<Bytes, RunFailure> {
    let frames = split_frames(body)
        .ok_or_else(|| engine_failure("the replay body did not split".to_string()))?;
    let is_notification =
        |frame: &&[u8]| message_type(frame).is_some_and(|message_type| message_type >= 0x8000);
    let mut notifications = frames
        .iter()
        .copied()
        .filter(is_notification)
        .collect::<Vec<_>>();
    SeededRng::new(seed).shuffle(&mut notifications);
    let mut notifications = notifications.into_iter();
    let mut reordered = Vec::with_capacity(body.len());
    for frame in frames {
        let frame = if is_notification(&frame) {
            notifications.next().unwrap_or(frame)
        } else {
            frame
        };
        reordered.extend_from_slice(frame);
    }
    Ok(Bytes::from(reordered))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn engine_failure(message: String) -> RunFailure {
    RunFailure::Engine { message }
}

// ---------------------------------------------------------------------------
// The leg proves itself.
// ---------------------------------------------------------------------------

const SEED: u64 = 0x5EED_3672;

fn scope() -> ExecutionScope {
    ExecutionScope::turn("determinism-session", "determinism-turn")
}

fn tool_attempt(key: &str, call_id: &str) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(scope(), key).expect("valid determinism address"),
            lash_core::RuntimeAttribution::for_turn(
                "determinism-session",
                "determinism-turn",
                0,
                0,
            ),
            key,
        ),
        RuntimeEffectCommand::ToolAttempt {
            call: prepared_tool_call_with(call_id, "determinism_tool"),
            execution_grant: None,
            attempt: 1,
            max_attempts: 1,
        },
    )
}

async fn run_tool(
    controller: &ScopedEffectController<'_>,
    key: &str,
    call_id: &str,
    executions: &Arc<AtomicUsize>,
) -> Result<String, String> {
    let executions = Arc::clone(executions);
    let owned_call_id = call_id.to_string();
    let outcome = controller
        .controller()
        .execute_effect(
            tool_attempt(key, call_id),
            RuntimeEffectLocalExecutor::testing(move |_| async move {
                executions.fetch_add(1, Ordering::SeqCst);
                Ok(RuntimeEffectOutcome::ToolAttempt {
                    launch: Box::new(lash_core::ToolAttemptLaunch::Done {
                        record: Box::new(completed_tool_record(&owned_call_id, "determinism_tool")),
                        intents: lash_core::ToolIntents::default(),
                    }),
                    triggers: Vec::new(),
                    capture: None,
                })
            }),
        )
        .await
        .map_err(|error| error.to_string())?;
    match outcome {
        RuntimeEffectOutcome::ToolAttempt { launch, .. } => match *launch {
            lash_core::ToolAttemptLaunch::Done { record, .. } => Ok(record.tool.clone()),
            _ => Err("the tool attempt did not finish".to_string()),
        },
        _ => Err("the tool attempt returned another effect".to_string()),
    }
}

/// Two tool attempts, the second named from the first's outcome, and a commit
/// built from both.
fn deterministic_worker(
    executions: Arc<AtomicUsize>,
) -> Arc<dyn Fn() -> RestateDrive + Send + Sync> {
    Arc::new(move || {
        let executions = Arc::clone(&executions);
        restate_drive(move |controller| {
            let executions = Arc::clone(&executions);
            Box::pin(async move {
                let first = run_tool(&controller, "tool-1", "call-1", &executions).await?;
                let second = run_tool(
                    &controller,
                    &format!("tool-2-after-{first}"),
                    "call-2",
                    &executions,
                )
                .await?;
                Ok(serde_json::json!({ "committed": [first, second] }))
            })
        })
    })
}

#[test]
fn a_deterministic_drive_replays_on_the_restate_endpoint() {
    let executions = Arc::new(AtomicUsize::new(0));
    let engine = backend(
        SEED,
        EndpointConfig {
            scope: scope(),
            worker: deterministic_worker(Arc::clone(&executions)),
        },
    );
    let report = DeterminismCheck::new(SEED)
        .perturbed_replays(4)
        .run(&engine)
        .unwrap_or_else(|failure| panic!("{failure}"));

    assert_eq!(
        executions.load(Ordering::SeqCst),
        2,
        "the fresh run executes each tool once; every replay is served from the journal"
    );
    assert_eq!(
        report.transcript.commits().collect::<Vec<_>>(),
        vec![r#"{"committed":["determinism_tool","determinism_tool"]}"#]
    );
    assert_eq!(
        report
            .transcript
            .commands()
            .filter(|entry| matches!(entry, TranscriptEntry::Command { kind, .. } if kind == "proposed-run-value"))
            .count(),
        2,
        "each tool attempt journals one run value"
    );
}

static RESTATE_WORKERS: AtomicU64 = AtomicU64::new(0);

/// A worker-minted value in an envelope: stable across every suspension and
/// replay on one deployment, different on another. On Restate each round of
/// the fresh run is itself a replay on the same worker, so it passes there and
/// is refused by the envelope fence on the separate worker.
#[test]
fn worker_local_state_in_an_envelope_is_refused_on_a_separate_worker() {
    let worker: Arc<dyn Fn() -> RestateDrive + Send + Sync> = Arc::new(|| {
        let worker_ref = RESTATE_WORKERS.fetch_add(1, Ordering::SeqCst);
        restate_drive(move |controller| {
            Box::pin(async move {
                let executions = Arc::new(AtomicUsize::new(0));
                let tool = run_tool(
                    &controller,
                    "tool-1",
                    &format!("call-worker-{worker_ref}"),
                    &executions,
                )
                .await?;
                Ok(serde_json::json!({ "committed": tool }))
            })
        })
    });
    let engine = backend(
        SEED,
        EndpointConfig {
            scope: scope(),
            worker,
        },
    );
    let failure = match DeterminismCheck::new(SEED).run(&engine) {
        Ok(report) => panic!("the check passed: {:?}", report.transcript),
        Err(failure) => failure,
    };

    assert_eq!(failure.mode, RunMode::Replay(ReplayMode::SeparateWorker));
    assert!(
        matches!(&failure.cause, FailureCause::Run(RunFailure::Engine { .. })),
        "{failure}"
    );
}
