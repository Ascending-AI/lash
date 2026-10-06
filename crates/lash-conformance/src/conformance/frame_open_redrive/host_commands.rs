//! FIG-4202: a host's head write from outside a turn is a session command
//! the shift applies at a turn boundary.
//!
//! The bound turn owns the session head. A host's append, plugin command,
//! plugin task and durable frame open are each submitted to the session's
//! command lane while a run holds its journaled pressure summary: none of
//! them moves the head, none of a plugin's code runs, and a lane-less write
//! that tries to go around the lane is refused typed. Released, the bound
//! turn commits its pressure frame and its turn, and the next shift applies
//! each command at the boundary, before the input queued after it, and
//! settles it with its typed outcome in the commit that makes its head
//! write. Killed after the shift read the command, before its commit, or
//! after it, and redriven, the command applies once.
//!
//! Beside the command laws: a dirty park while the bound turn owns the head is
//! refused busy, keeps its runtime and loses nothing, and lands once the
//! boundary passed, while a clean park writes nothing; a plugin-state-dirty
//! park re-parks from the recorded head once the bound turn moved it
//! (FIG-4392); and a command withdrawn before the shift read it never
//! applies, while one the shift already read is no longer withdrawn. A
//! host's cancel still reaches a plugin task the shift admitted, through the
//! task's cancel signal: the shift stops the task's code and settles the
//! command cancelled, with nothing of the task committed (FIG-4391), and a
//! task whose shift died after its code returned and before its settlement
//! runs again under a signal the cancel still reaches (FIG-4453).

use crate::ActorContext;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pretty_assertions::assert_eq;

use super::{
    LawSession, ModelScript, PRESSURE_THRESHOLD_TOKENS, StandardFrameLawProtocol, SummaryHold,
    active_path, build_runtime, frame_chain, law_model,
};
use crate::admit;
use crate::plugin::PluginFactory;

/// Where a host-command law kills the shift that applies the command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostCommandCrash {
    /// The shift read the command; nothing of it ran.
    AfterLaneRead,
    /// The command applied in resident state (a plugin's code ran); its
    /// settling commit has not run.
    BeforeCommit,
    /// The command's settling commit landed; its command run has not gone
    /// on.
    AfterCommit,
}

impl HostCommandCrash {
    /// The named runtime phase the crash fires at.
    fn phase(self) -> &'static str {
        match self {
            Self::AfterLaneRead => lash_core::runtime::SESSION_COMMAND_APPLYING_PHASE,
            Self::BeforeCommit => lash_core::runtime::SESSION_COMMAND_STAGED_PHASE,
            Self::AfterCommit => lash_core::runtime::SESSION_COMMAND_COMMITTED_PHASE,
        }
    }
}

/// Panics at the first host command phase the law's crash names.
struct CommandCrashProbe {
    crash: HostCommandCrash,
}

impl lash_core::runtime::RuntimeTurnPhaseProbe for CommandCrashProbe {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, phase: &str) {
        if phase == self.crash.phase() {
            panic!("injected crash at the host command's {phase}");
        }
    }
}

/// The text a host's append writes.
const HOST_APPEND_NOTE: &str = "a note the host appended";
/// The text a host's plugin command appends.
const PLUGIN_COMMAND_NOTE: &str = "a note the host's plugin command appended";
/// The text a host's plugin task appends.
const PLUGIN_TASK_NOTE: &str = "a note the host's plugin task appended";
/// The text a host's cancellable plugin task would append after its
/// cancellation.
const CANCELLED_TASK_NOTE: &str = "a note the host's cancelled plugin task appended";
/// The seed a host's frame open carries.
const HOST_FRAME_SEED: &str = "the seed of the host's frame";
/// The reason a host's frame open names.
const HOST_FRAME_REASON: &str = "host_open";

#[derive(Debug, serde::Serialize, serde::Deserialize, lash_core::facade_support::JsonSchema)]
#[schemars(crate = "lash_core::facade_support::schemars")]
enum HostNoteFailure {
    MissingSession,
    MissingText,
    Runtime {
        failure: Box<crate::plugin::PluginOperationFailure>,
    },
}

impl std::fmt::Display for HostNoteFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingSession => formatter.write_str("the note names no session"),
            Self::MissingText => formatter.write_str("the note has no text"),
            Self::Runtime { failure } => std::fmt::Display::fmt(failure, formatter),
        }
    }
}

/// A plugin command that appends the note its arguments name.
struct HostNoteCommand;

impl crate::plugin::PluginOperation for HostNoteCommand {
    const NAME: &'static str = "conformance_host_note";
    const DESCRIPTION: &'static str = "Append a note to the session, as a host's plugin command.";
    const SESSION_PARAM: crate::plugin::SessionParam = crate::plugin::SessionParam::Required;
    type Args = serde_json::Value;
    type Output = serde_json::Value;
    type Error = HostNoteFailure;
    const ERROR_TYPE: &'static str = Self::NAME;
    const ERROR_VERSION: lash_core::FormatVersion = lash_core::FormatVersion::ONE;
    fn error_class(error: &Self::Error) -> lash_sansio::PluginFailureClass {
        match error {
            HostNoteFailure::Runtime { failure } => failure.class,
            _ => lash_sansio::PluginFailureClass::Terminal,
        }
    }
}

impl crate::plugin::PluginCommand for HostNoteCommand {}

struct HostQueueCommand;

impl crate::plugin::PluginOperation for HostQueueCommand {
    const NAME: &'static str = "conformance_host_queue";
    const DESCRIPTION: &'static str = "Queue inputs from a host command.";
    const SESSION_PARAM: crate::plugin::SessionParam = crate::plugin::SessionParam::Required;
    type Args = Vec<Option<String>>;
    type Output = serde_json::Value;
    type Error = String;
    const ERROR_TYPE: &'static str = Self::NAME;
    const ERROR_VERSION: lash_core::FormatVersion = lash_core::FormatVersion::ONE;
    fn error_class(_: &Self::Error) -> lash_sansio::PluginFailureClass {
        lash_sansio::PluginFailureClass::Terminal
    }
}

impl crate::plugin::PluginCommand for HostQueueCommand {}

/// A command's queued inputs use input keys, and a reserved-key refusal is
/// retained as a typed settlement. A mixed directive request admits nothing.
#[expect(clippy::expect_used, reason = "conformance-law fixture")]
pub async fn plugin_queued_turns_preserve_reserved_source_key_refusals(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let protocol = StandardFrameLawProtocol::shared();
    let model = law_model(ModelScript {
        turns: vec![(protocol.answer("queued answer"), 1); 3],
    });
    let mut law = LawSession::open(
        prefix,
        "reserved-plugin-input",
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts
        .host_plugins
        .push(Arc::new(crate::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("reserved-plugin-input"),
            crate::facade_support::PluginSpec::new()
                .with_plugin_command_typed::<HostQueueCommand, _, _>(|_, keys| async move {
                    Ok(
                        crate::plugin::PluginOperationOutcome::new(serde_json::Value::Null)
                            .with_directives(
                                keys.into_iter()
                                    .map(|source_key| {
                                        crate::plugin::PluginRuntimeDirective::QueueTurn {
                                            input: crate::TurnInput::text("plugin input"),
                                            source_key,
                                        }
                                    })
                                    .collect(),
                            ),
                    )
                }),
        )));
    for (index, key) in [
        "command:refresh_tool_catalog:foreign",
        "process:foreign:event:1:wake",
    ]
    .into_iter()
    .enumerate()
    {
        let receipt = law
            .submit_command(
                crate::SessionCommand::RunPluginCommand {
                    name: <HostQueueCommand as crate::plugin::PluginOperation>::NAME.into(),
                    args: serde_json::json!(["host:must-roll-back", key]),
                },
                &format!("refused-{index}"),
            )
            .await;
        law.enqueue("shift after refusal").await;
        law.execute_run(&format!("refused-{index}")).await;
        let outcome = law
            .command_outcome(&receipt)
            .await
            .expect("command settled");
        let recorded = serde_json::to_value(&outcome).expect("encode durable settlement");
        assert_eq!(
            recorded["outcome"]["kind"], "refused",
            "the refusal stays typed: {recorded}"
        );
        assert_eq!(
            recorded["outcome"]["error"]["code"],
            "ingress_reserved_source_key"
        );
        assert_eq!(recorded["outcome"]["error"]["cause"]["source_key"], key);
        assert!(
            law.store
                .list_pending_turn_inputs(&law.session_id)
                .await
                .expect("list inputs")
                .is_empty(),
            "no input from the mixed directive request survives"
        );
    }
    let receipt = law
        .submit_command(
            crate::SessionCommand::RunPluginCommand {
                name: <HostQueueCommand as crate::plugin::PluginOperation>::NAME.into(),
                args: serde_json::json!([null]),
            },
            "generated-key",
        )
        .await;
    law.execute_run("generated-key").await;
    let Some(crate::SessionCommandOutcome::PluginOperation {
        outcome:
            crate::PluginOperationCommandOutcome::Completed {
                pending_turn_inputs,
                ..
            },
    }) = law.command_outcome(&receipt).await
    else {
        panic!("a generated input key is accepted");
    };
    assert_eq!(pending_turn_inputs.len(), 1);
    assert!(
        pending_turn_inputs[0]
            .source_key
            .as_deref()
            .expect("generated input key")
            .starts_with("input:")
    );
}

/// A plugin task that appends the note its arguments name.
struct HostNoteTask;

impl crate::plugin::PluginOperation for HostNoteTask {
    const NAME: &'static str = "conformance_host_note_task";
    const DESCRIPTION: &'static str = "Append a note to the session, as a host's plugin task.";
    const SESSION_PARAM: crate::plugin::SessionParam = crate::plugin::SessionParam::Required;
    type Args = serde_json::Value;
    type Output = serde_json::Value;
    type Error = HostNoteFailure;
    const ERROR_TYPE: &'static str = Self::NAME;
    const ERROR_VERSION: lash_core::FormatVersion = lash_core::FormatVersion::ONE;
    fn error_class(error: &Self::Error) -> lash_sansio::PluginFailureClass {
        match error {
            HostNoteFailure::Runtime { failure } => failure.class,
            _ => lash_sansio::PluginFailureClass::Terminal,
        }
    }
}

impl crate::plugin::PluginTask for HostNoteTask {}

/// A plugin task that runs until its cancellation, and then appends the
/// note its arguments name.
struct HostCancellableTask;

impl crate::plugin::PluginOperation for HostCancellableTask {
    const NAME: &'static str = "conformance_host_cancellable_task";
    const DESCRIPTION: &'static str =
        "Run until cancelled, then append a note to the session, as a host's plugin task.";
    const SESSION_PARAM: crate::plugin::SessionParam = crate::plugin::SessionParam::Required;
    type Args = serde_json::Value;
    type Output = serde_json::Value;
    type Error = HostNoteFailure;
    const ERROR_TYPE: &'static str = Self::NAME;
    const ERROR_VERSION: lash_core::FormatVersion = lash_core::FormatVersion::ONE;
    fn error_class(error: &Self::Error) -> lash_sansio::PluginFailureClass {
        match error {
            HostNoteFailure::Runtime { failure } => failure.class,
            _ => lash_sansio::PluginFailureClass::Terminal,
        }
    }
}

impl crate::plugin::PluginTask for HostCancellableTask {}

/// A plugin task that appends the same note on every execution. Its first
/// execution returns at once; a rerun waits for a late cancellation before
/// reproducing the same output and graph draft.
struct HostReturnsOnceTask;

impl crate::plugin::PluginOperation for HostReturnsOnceTask {
    const NAME: &'static str = "conformance_host_returns_once_task";
    const DESCRIPTION: &'static str = "Append a note, waiting for cancellation on a rerun.";
    const SESSION_PARAM: crate::plugin::SessionParam = crate::plugin::SessionParam::Required;
    type Args = serde_json::Value;
    type Output = serde_json::Value;
    type Error = HostNoteFailure;
    const ERROR_TYPE: &'static str = Self::NAME;
    const ERROR_VERSION: lash_core::FormatVersion = lash_core::FormatVersion::ONE;
    fn error_class(error: &Self::Error) -> lash_sansio::PluginFailureClass {
        match error {
            HostNoteFailure::Runtime { failure } => failure.class,
            _ => lash_sansio::PluginFailureClass::Terminal,
        }
    }
}

impl crate::plugin::PluginTask for HostReturnsOnceTask {}

/// What the host-command plugin saw, shared by every runtime a law builds.
#[derive(Clone, Default)]
struct HostPluginProbe {
    /// How many times the plugin command's code ran.
    command_runs: Arc<AtomicUsize>,
    /// How many times the plugin task's code ran.
    task_runs: Arc<AtomicUsize>,
    /// How many times the cancellable plugin task's code ran.
    cancellable_task_runs: Arc<AtomicUsize>,
    /// Notified each time the cancellable plugin task's code starts.
    cancellable_task_entered: Arc<tokio::sync::Notify>,
    /// How many times the returns-once plugin task's code ran.
    returns_once_task_runs: Arc<AtomicUsize>,
    /// Notified each time a later run of the returns-once plugin task's code
    /// starts.
    returns_once_task_rerun: Arc<tokio::sync::Notify>,
    /// Holds the plugin command's code after it counted its run.
    command_hold: Option<SummaryHold>,
}

/// Appends `text` through `graph`, the services a plugin operation's code
/// holds: in a session command they join the command's commit.
async fn append_note(
    graph: &Arc<dyn crate::plugin::SessionGraphService>,
    session_id: Option<crate::SessionId>,
    args: &serde_json::Value,
) -> Result<serde_json::Value, HostNoteFailure> {
    let session_id = session_id.ok_or(HostNoteFailure::MissingSession)?;
    let text = args
        .get("text")
        .and_then(serde_json::Value::as_str)
        .ok_or(HostNoteFailure::MissingText)?;
    let outcome = graph
        .append_session_nodes(
            &session_id,
            crate::AppendSessionNodesRequest {
                operation_id: format!("host-note:{text}"),
                nodes: vec![crate::SessionAppendNode::message(
                    crate::PluginMessage::text(crate::MessageRole::Assistant, text),
                )],
                requires_ancestor_node_id: None,
            },
        )
        .await
        .map_err(|error| HostNoteFailure::Runtime {
            failure: Box::new(error.into()),
        })?;
    Ok(serde_json::json!({
        "text": text,
        "appended": matches!(outcome, crate::AppendSessionNodesOutcome::Appended { .. })
    }))
}

/// The laws' plugin: a note-appending command and task, and a terminal
/// callback that appends a note on every persisted turn once armed.
fn host_plugin(probe: &HostPluginProbe) -> Arc<dyn PluginFactory> {
    let command_probe = probe.clone();
    let task_probe = probe.clone();
    let cancellable_probe = probe.clone();
    let returns_once_probe = probe.clone();
    Arc::new(crate::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial(HOST_PLUGIN_ID),
        crate::facade_support::PluginSpec::new()
            .with_plugin_command_value::<HostNoteCommand, _, _>(move |ctx, args| {
                let probe = command_probe.clone();
                async move {
                    probe.command_runs.fetch_add(1, Ordering::SeqCst);
                    if let Some(hold) = &probe.command_hold {
                        hold.wait().await;
                    }
                    append_note(&ctx.session_graph, ctx.session_id, &args).await
                }
            })
            .with_plugin_task_value::<HostNoteTask, _, _>(move |ctx, args| {
                let probe = task_probe.clone();
                async move {
                    probe.task_runs.fetch_add(1, Ordering::SeqCst);
                    append_note(&ctx.session_graph, ctx.session_id, &args).await
                }
            })
            .with_plugin_task_value::<HostCancellableTask, _, _>(move |ctx, args| {
                let probe = cancellable_probe.clone();
                async move {
                    probe.cancellable_task_runs.fetch_add(1, Ordering::SeqCst);
                    probe.cancellable_task_entered.notify_one();
                    ctx.cancellation_token.cancelled().await;
                    append_note(&ctx.session_graph, ctx.session_id, &args).await
                }
            })
            .with_plugin_task_value::<HostReturnsOnceTask, _, _>(move |ctx, args| {
                let probe = returns_once_probe.clone();
                async move {
                    if probe.returns_once_task_runs.fetch_add(1, Ordering::SeqCst) != 0 {
                        probe.returns_once_task_rerun.notify_one();
                        ctx.cancellation_token.cancelled().await;
                    }
                    append_note(&ctx.session_graph, ctx.session_id, &args).await
                }
            }),
    ))
}

/// A host's append of `text`.
fn append_command(text: &str) -> crate::SessionCommand {
    crate::SessionCommand::AppendSessionNodes {
        request: Box::new(crate::AppendSessionNodesRequest {
            operation_id: format!("host-append:{text}"),
            nodes: vec![crate::SessionAppendNode::message(
                crate::PluginMessage::text(crate::MessageRole::Assistant, text),
            )],
            requires_ancestor_node_id: None,
        }),
    }
}

/// One attempt at a shift of the law's session, killed at `crash` when it
/// names one. A crashing attempt must die at its crash point; the others
/// send back how their shift ended.
fn command_attempt(
    law: &LawSession,
    crash: Option<HostCommandCrash>,
    result_tx: Option<super::ShiftResultTx>,
) -> crate::ConformanceTurnAttempt {
    let parts = law.parts.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let result_tx = result_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(&parts, None).await;
            if let Some(crash) = crash {
                runtime.set_turn_phase_probe(Arc::new(CommandCrashProbe { crash }));
            }
            let shift = Box::pin(runtime.execute_next_queued_run(crate::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scope,
            )))
            .await;
            let Some(result_tx) = result_tx else {
                panic!(
                    "the crash at {crash:?} did not fire: {:?}",
                    shift.map(crate::facade_support::QueuedTurnDrain::ran)
                );
            };
            let end = crate::ConformanceTurnEnd::of(&shift);
            let _ = result_tx.send(shift);
            end
        })
    })
}

/// Runs the law's next shift, killing it at `crash` and redriving it, and
/// answers the turn outcome of the run the redrive ran.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn shift_crashed_at(
    law: &LawSession,
    shift: &str,
    crash: HostCommandCrash,
) -> crate::TurnOutcome {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        law.runner.run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(
                &law.session_id,
                crate::TurnId::fixture(format!("{}-{shift}", law.prefix)),
            )),
            command_attempt(law, Some(crash), None),
            command_attempt(law, None, Some(tx)),
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("the redrive after {crash:?} ends"));
    rx.recv()
        .await
        .expect("the tier's runner ran the redriven shift")
        .unwrap_or_else(|error| panic!("the redrive after {crash:?} replays: {error:?}"))
        .ran()
        .expect("the redriven shift executes the run queued after the command")
        .outcome
}

/// Runs the law's next shift to its end, killed at `crash` and redriven when
/// it names one.
async fn work_next(law: &LawSession, shift: &str, crash: Option<HostCommandCrash>) {
    match crash {
        Some(crash) => {
            shift_crashed_at(law, shift, crash).await;
        }
        None => {
            law.execute_run(shift).await;
        }
    }
}

/// A lane-less write another runtime makes around the command lane, while
/// the bound turn owns the head, is refused typed with nothing written.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn a_lane_less_write_is_refused(law: &LawSession) {
    let before = law.head().await.head_revision;
    let runtime = build_runtime(&law.parts, None).await;
    let refused = runtime
        .session_graph_service()
        .expect("session graph service")
        .append_session_nodes(
            &law.session_id,
            crate::AppendSessionNodesRequest {
                operation_id: "host-command-law-lane-less-append".to_string(),
                nodes: vec![crate::SessionAppendNode::message(
                    crate::PluginMessage::text(crate::MessageRole::Assistant, "around the lane"),
                )],
                requires_ancestor_node_id: None,
            },
        )
        .await
        .expect_err("the bound turn owns the head");
    let bound = law
        .store
        .unfinished_run(&law.session_id)
        .await
        .expect("read the bound run")
        .expect("a run is bound")
        .run;
    assert!(
        matches!(
            &refused,
            crate::PluginError::SessionHeadOwned { session_id, owner }
                if *session_id == law.session_id
                    && *owner == crate::store::SessionHeadOwner::Run { run: bound }
        ),
        "the typed refusal names the session and the bound run: {refused:?}"
    );
    assert_eq!(
        law.head().await.head_revision,
        before,
        "the refused write wrote nothing"
    );
}

/// A lane-less append during a bound turn returns the typed ownership
/// refusal with the exact session and run, without changing the head.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the bound turn is released once the refusal is checked"
)]
pub async fn a_lane_less_append_names_the_bound_head_owner(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let (law, _, hold) = bound_turn_session(
        prefix,
        "lane-less-append-owner",
        effect_host,
        stores,
        runner,
        &HostPluginProbe::default(),
    )
    .await;
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        futures_util::future::join(
            law.execute_run("run-2"),
            hold.while_held(a_lane_less_write_is_refused(&law)),
        ),
    )
    .await
    .expect("the bound turn ends after the typed refusal");
}

/// The position of the one committed message on `path` whose text is
/// `text`.
fn position_of_once(path: &[String], text: &str) -> usize {
    let positions = path
        .iter()
        .enumerate()
        .filter(|(_, candidate)| *candidate == text)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(positions.len(), 1, "`{text}` lands once: {path:?}");
    positions[0]
}

/// A session whose first run committed usage over the pressure threshold,
/// and a second input queued, so the next run's pressure hook compacts and
/// holds its journaled summary.
async fn bound_turn_session(
    prefix: &str,
    law_name: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    probe: &HostPluginProbe,
) -> (LawSession, super::LawModel, SummaryHold) {
    let protocol = StandardFrameLawProtocol::shared();
    let model = law_model(ModelScript {
        turns: vec![
            // The first run's usage crosses the pressure hook's threshold,
            // so the bound turn opens a pressure frame.
            (protocol.answer("answer 1"), PRESSURE_THRESHOLD_TOKENS),
            (protocol.answer("answer 2"), 1),
            (protocol.answer("answer 3"), 1),
        ],
    });
    let mut law = LawSession::open(
        prefix,
        law_name,
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    let hold = SummaryHold::default();
    law.parts.compaction.pressure_hold = Some(hold.clone());
    law.parts.host_plugins.push(host_plugin(probe));
    law.enqueue("first question").await;
    law.execute_run("run-1").await;
    law.enqueue("second question").await;
    (law, model, hold)
}

/// Submits `commands` while the bound turn holds its pressure summary,
/// checks nothing of them moved the head, and runs the bound turn to its
/// end. Answers their receipts and the head revision after the bound turn.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn submit_while_bound(
    law: &LawSession,
    hold: &SummaryHold,
    commands: Vec<(crate::SessionCommand, &'static str)>,
    during: impl std::future::Future<Output = ()>,
) -> (Vec<crate::SessionCommandReceipt>, u64) {
    let before = law.head().await.head_revision;
    let submitted = std::sync::OnceLock::new();
    let while_held = hold.while_held(async {
        let mut receipts = Vec::new();
        for (command, key) in commands {
            receipts.push(law.submit_command(command, key).await);
        }
        law.receipts
            .assert_since(before, law.head().await.head_revision, 0, 0, 0, 1);
        for receipt in &receipts {
            assert!(
                law.command_outcome(receipt).await.is_none(),
                "the command waits on the command lane"
            );
        }
        a_lane_less_write_is_refused(law).await;
        during.await;
        submitted
            .set(receipts)
            .expect("the commands are submitted once");
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        futures_util::future::join(law.execute_run("run-2"), while_held),
    )
    .await
    .expect("the bound turn's shift ends");
    let receipts = submitted
        .into_inner()
        .expect("the commands were submitted while the pressure summary was held");
    let after = law.head().await.head_revision;
    law.receipts.assert_since(before, after, 1, 1, 0, 1);
    for receipt in &receipts {
        assert!(
            law.command_outcome(receipt).await.is_none(),
            "the command waits for the bound turn's boundary"
        );
    }
    (receipts, after)
}

/// The bound turn owns the head (FIG-4202): a host's append submitted while
/// a run holds its pressure summary waits on the command lane, and a
/// lane-less write around the lane is refused typed. The next shift applies
/// the append at the boundary, after everything the bound turn committed and
/// before the input queued after it, and settles it `Appended` in the one
/// commit that lands its node. Killed at `crash` and redriven, the append
/// lands once.
pub async fn host_append_waits_for_the_bound_turn(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: Option<HostCommandCrash>,
) {
    let probe = HostPluginProbe::default();
    let (law, model, hold) = bound_turn_session(
        prefix,
        &format!("host-append-{crash:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        &probe,
    )
    .await;
    let (receipts, after_bound_turn) = submit_while_bound(
        &law,
        &hold,
        vec![(append_command(HOST_APPEND_NOTE), "append")],
        async {},
    )
    .await;

    law.enqueue("third question").await;
    work_next(&law, "run-3", crash).await;

    let head = law.head().await;
    let Some(crate::SessionCommandOutcome::AppendSessionNodes {
        outcome: crate::AppendSessionNodesOutcome::Appended { node_ids, .. },
    }) = law.command_outcome(&receipts[0]).await
    else {
        panic!("the append settles appended");
    };
    assert_eq!(node_ids.len(), 1, "the append landed its one node");
    law.receipts
        .assert_since(after_bound_turn, head.head_revision, 0, 1, 1, 2);
    let path = active_path(&head.graph);
    let note = position_of_once(&path, HOST_APPEND_NOTE);
    assert!(
        position_of_once(&path, "answer 2") < note
            && note < position_of_once(&path, "third question"),
        "the append lands after the bound turn and before the next input: {path:?}"
    );
    assert_eq!(model.summary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(model.turn_calls.load(Ordering::SeqCst), 3);
}

/// A host's plugin command and plugin task submitted while a run holds its
/// pressure summary run none of their plugin's code: the next shift admits
/// them at the boundary, runs each, and settles each `Completed` with its
/// output in the commit that lands what its services appended. Killed at
/// `crash` and redriven, each settles once; a crash before the commit runs
/// the command's code again (a plugin's code before its commit is
/// at-least-once), and its note still lands once. A host's cancel of the
/// settled task finds its settlement and reaches nothing (FIG-4453).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn host_plugin_command_applies_at_the_boundary(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: Option<HostCommandCrash>,
) {
    let probe = HostPluginProbe::default();
    let host_effects = effect_host.clone();
    let (law, model, hold) = bound_turn_session(
        prefix,
        &format!("host-plugin-command-{crash:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        &probe,
    )
    .await;
    let not_yet_run = async {
        assert_eq!(
            probe.command_runs.load(Ordering::SeqCst),
            0,
            "no plugin code runs before admission"
        );
        assert_eq!(probe.task_runs.load(Ordering::SeqCst), 0);
    };
    let (receipts, after_bound_turn) = submit_while_bound(
        &law,
        &hold,
        vec![
            (
                crate::SessionCommand::RunPluginCommand {
                    name: <HostNoteCommand as crate::plugin::PluginOperation>::NAME.to_string(),
                    args: serde_json::json!({"text": PLUGIN_COMMAND_NOTE}),
                },
                "plugin-command",
            ),
            (
                crate::SessionCommand::RunPluginTask {
                    name: <HostNoteTask as crate::plugin::PluginOperation>::NAME.to_string(),
                    args: serde_json::json!({"text": PLUGIN_TASK_NOTE}),
                },
                "plugin-task",
            ),
        ],
        not_yet_run,
    )
    .await;
    assert_eq!(
        probe.command_runs.load(Ordering::SeqCst),
        0,
        "the bound turn's shift runs no command"
    );

    law.enqueue("third question").await;
    work_next(&law, "run-3", crash).await;

    for (receipt, text) in receipts.iter().zip([PLUGIN_COMMAND_NOTE, PLUGIN_TASK_NOTE]) {
        match law.command_outcome(receipt).await {
            Some(crate::SessionCommandOutcome::PluginOperation {
                outcome: crate::PluginOperationCommandOutcome::Completed { output, .. },
            }) => assert_eq!(
                output,
                serde_json::json!({"text": text, "appended": true}),
                "the operation settles with its output"
            ),
            other => panic!("the operation settles completed: {other:?}"),
        }
    }
    assert_eq!(
        probe.command_runs.load(Ordering::SeqCst),
        1 + usize::from(crash == Some(HostCommandCrash::BeforeCommit)),
        "the command's code ran after admission, again only after a crash before its commit"
    );
    assert_eq!(probe.task_runs.load(Ordering::SeqCst), 1);
    assert_eq!(
        lash_core::runtime::request_plugin_task_cancel(
            law.store.as_ref(),
            &host_effects,
            &receipts[1]
        )
        .await
        .expect("cancel the settled task"),
        lash_core::runtime::PluginTaskCancelRequest::AlreadySettled,
        "a cancel after the task settled finds its settlement (FIG-4453)"
    );
    let head = law.head().await;
    law.receipts
        .assert_since(after_bound_turn, head.head_revision, 0, 1, 2, 3);
    let path = active_path(&head.graph);
    let answer = position_of_once(&path, "answer 2");
    let next = position_of_once(&path, "third question");
    for text in [PLUGIN_COMMAND_NOTE, PLUGIN_TASK_NOTE] {
        let note = position_of_once(&path, text);
        assert!(
            answer < note && note < next,
            "{text} at the boundary: {path:?}"
        );
    }
    assert_eq!(model.turn_calls.load(Ordering::SeqCst), 3);
}

/// A host's durable frame open submitted while a run holds its pressure
/// summary waits on the command lane. The next shift opens the frame at the
/// boundary, after the bound turn's pressure frame, in the commit that
/// settles it `Opened` with the frame it opened, and the run queued after
/// it runs in that frame, from its seed. Killed at `crash` and redriven, the
/// frame opens once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn host_frame_open_applies_at_the_boundary(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: Option<HostCommandCrash>,
) {
    let probe = HostPluginProbe::default();
    let (law, model, hold) = bound_turn_session(
        prefix,
        &format!("host-frame-open-{crash:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        &probe,
    )
    .await;
    let open = crate::SessionCommand::OpenAgentFrame {
        request: Box::new(
            crate::OpenAgentFrameRequest::new(
                crate::FrameKey::from_caller_material("host-command-law-frame")
                    .expect("non-empty frame material"),
                crate::AgentFrameReason::new(HOST_FRAME_REASON),
            )
            .with_initial_nodes(vec![crate::SessionAppendNode::message(
                crate::PluginMessage::text(crate::MessageRole::User, HOST_FRAME_SEED),
            )]),
        ),
    };
    let (receipts, after_bound_turn) =
        submit_while_bound(&law, &hold, vec![(open, "frame-open")], async {}).await;
    let chain = frame_chain(&law.head().await, &law.session_id);
    assert_eq!(chain.len(), 2, "the bound turn's pressure frame: {chain:?}");

    law.enqueue("third question").await;
    work_next(&law, "run-3", crash).await;

    let head = law.head().await;
    let chain = frame_chain(&head, &law.session_id);
    assert_eq!(
        chain.len(),
        3,
        "the first frame, the pressure frame, then the host's frame: {chain:?}"
    );
    assert_eq!(chain[2].0, HOST_FRAME_REASON);
    assert_eq!(
        chain[2].1.as_ref(),
        Some(&chain[1].2),
        "the host's frame follows the pressure frame"
    );
    match law.command_outcome(&receipts[0]).await {
        Some(crate::SessionCommandOutcome::OpenAgentFrame {
            outcome: crate::OpenAgentFrameCommandOutcome::Opened { outcome },
        }) => {
            assert!(outcome.opened);
            assert_eq!(outcome.frame_node_id, chain[2].2.to_string());
            assert_eq!(outcome.initial_node_ids.len(), 1, "the seed landed");
        }
        other => panic!("the open settles opened: {other:?}"),
    }
    law.receipts
        .assert_since(after_bound_turn, head.head_revision, 0, 1, 1, 2);
    let path = active_path(&head.graph);
    let frame_at = path
        .iter()
        .rposition(|text| text == "FrameOpen")
        .expect("the host's frame is on the path");
    let seed = position_of_once(&path, HOST_FRAME_SEED);
    let next = position_of_once(&path, "third question");
    assert!(
        frame_at < seed && seed < next,
        "the run queued after the open runs in its frame, from its seed: {path:?}"
    );
    assert_eq!(model.summary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(model.turn_calls.load(Ordering::SeqCst), 3);
}

/// A park is recoverable (FIG-4202). While a run holds its pressure
/// summary, a clean host runtime parks without writing anything, and one
/// holding a pending note is refused busy, naming the bound run as the
/// head's owner, with nothing written: the refusal hands its runtime back.
/// Once the bound turn's boundary passed, the same runtime parks, and
/// everything the bound turn committed stands. A host runtime holds no
/// pending billing data to lose: hosts meter provider attempts at the
/// Provider seam (ADR 0127).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn dirty_park_while_busy_is_recoverable_and_loses_nothing(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    const PENDING_NOTE: &str = "the refused park's pending note";
    let probe = HostPluginProbe::default();
    let (law, model, hold) = bound_turn_session(
        prefix,
        "dirty-park-while-busy",
        effect_host,
        stores,
        runner,
        &probe,
    )
    .await;
    let refused_runtime = std::sync::OnceLock::new();
    let while_held = hold.while_held(async {
        let before = law.head().await.head_revision;
        let clean = build_runtime(&law.parts, None).await;
        Box::pin(clean.park())
            .await
            .expect("a clean park is never busy");
        assert_eq!(
            law.head().await.head_revision,
            before,
            "a clean park writes nothing"
        );

        let mut dirty = build_runtime(&law.parts, None).await;
        dirty.edit_resident_state_for_test(|state| {
            state.append_active_conversation_messages(&[crate::Message {
                id: "dirty-park-pending-note".to_string(),
                role: crate::MessageRole::Assistant,
                parts: vec![crate::Part::text(
                    "dirty-park-pending-note.p0".to_string(),
                    PENDING_NOTE.to_string(),
                    None,
                )]
                .into(),
                origin: None,
                reply_marker: None,
            }]);
        });
        let Err(refused) = Box::pin(dirty.park()).await else {
            panic!("a dirty park while the bound turn owns the head is busy");
        };
        let bound = law
            .store
            .unfinished_run(&law.session_id)
            .await
            .expect("read the bound run")
            .expect("a run is bound")
            .run;
        assert_eq!(
            refused.busy_owner(),
            Some(&crate::store::SessionHeadOwner::Run { run: bound }),
            "the refusal names the bound run: {:?}",
            refused.error
        );
        assert_eq!(
            law.head().await.head_revision,
            before,
            "the refused park wrote nothing"
        );
        assert!(
            refused_runtime.set(refused.runtime).is_ok(),
            "one refused park"
        );
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        futures_util::future::join(law.execute_run("run-2"), while_held),
    )
    .await
    .expect("the bound turn's shift ends");

    let runtime = refused_runtime
        .into_inner()
        .expect("the dirty park was refused while the turn was bound");
    Box::pin((*runtime).park())
        .await
        .expect("the refused runtime parks once the boundary passed");

    let path = active_path(&law.head().await.graph);
    position_of_once(&path, "answer 2");
    assert_eq!(model.turn_calls.load(Ordering::SeqCst), 2);
}

/// The plugin namespace of the laws' host plugin.
const HOST_PLUGIN_ID: &str = "conformance-host-commands";

/// `runtime`'s own view of the laws' host plugin namespace: the one the
/// plugin receives.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn host_plugin_state(runtime: &crate::LashRuntime) -> lash_core::PluginStateView {
    lash_core::testing::runtime_internals::plugin_state_view(
        &runtime
            .plugin_session()
            .expect("a law runtime has a plugin session"),
        HOST_PLUGIN_ID,
    )
}

/// Publish `key` = `value` in `runtime`'s host plugin namespace as a body of
/// the plugin's tool does: a recorded publication the runtime holds
/// uncommitted until its next boundary.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn publish_host_plugin_state(
    runtime: &crate::LashRuntime,
    step: &str,
    key: &str,
    value: serde_json::Value,
) {
    let plugins = runtime
        .plugin_session()
        .expect("a law runtime has a plugin session");
    let address = crate::EffectAddress::new(
        crate::ExecutionScope::turn(
            plugins
                .owner()
                .session_id()
                .expect("a law runtime is a session")
                .clone(),
            "plugin-state-dirty-park",
        ),
        step,
    )
    .expect("a valid fixture address");
    lash_core::testing::runtime_internals::publish_plugin_state(
        &plugins,
        HOST_PLUGIN_ID,
        address,
        lash_core::StateCommands::new().set(key, value),
    )
    .await
    .expect("the plugin publication is accepted");
}

/// The laws' host plugin namespace the session's recorded head carries.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn recorded_host_plugin_namespace(law: &LawSession) -> lash_core::PluginNamespaceState {
    crate::conformance::helpers::load_window_state(&law.store, &law.session_id)
        .await
        .expect("read the recorded head")
        .expect("the session committed")
        .plugin_state()
        .and_then(|state| state.plugins.get(HOST_PLUGIN_ID).cloned())
        .unwrap_or_default()
}

/// A plugin-state-dirty park never wedges (FIG-4392). A host runtime holding
/// an accepted, uncommitted plugin write is refused busy while the bound
/// turn owns the head, and hands its runtime back. Once the bound turn's
/// boundary passed, the head it committed has moved past the runtime: the
/// same runtime re-parks by rehydrating from the recorded head, whose plugin
/// namespace does not carry the uncommitted write, as a cold rebuild would,
/// and writes nothing. A runtime built on that head parks clean, and one
/// whose accepted write sits on the recorded head commits it at its park.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn plugin_state_dirty_park_reparks_from_the_recorded_head(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    const TAIL_KEY: &str = "uncommitted-tail";
    const LANDED_KEY: &str = "landed-on-the-recorded-head";
    let probe = HostPluginProbe::default();
    let (law, model, hold) = bound_turn_session(
        prefix,
        "plugin-state-dirty-park",
        effect_host,
        stores,
        runner,
        &probe,
    )
    .await;
    let recorded_before = recorded_host_plugin_namespace(&law).await;
    let refused_runtime = std::sync::OnceLock::new();
    let refused_state = std::sync::OnceLock::new();
    let while_held = hold.while_held(async {
        let before = law.head().await.head_revision;
        let dirty = build_runtime(&law.parts, None).await;
        publish_host_plugin_state(
            &dirty,
            "dirty-publication",
            TAIL_KEY,
            serde_json::json!("accepted while the turn was bound"),
        )
        .await;
        let accepted = host_plugin_state(&dirty).generation();
        assert_eq!(accepted, recorded_before.generation + 1);
        assert!(
            refused_state
                .set((host_plugin_state(&dirty), accepted))
                .is_ok()
        );
        let Err(refused) = Box::pin(dirty.park()).await else {
            panic!("a plugin-state-dirty park while the bound turn owns the head is busy");
        };
        let bound = law
            .store
            .unfinished_run(&law.session_id)
            .await
            .expect("read the bound run")
            .expect("a run is bound")
            .run;
        assert_eq!(
            refused.busy_owner(),
            Some(&crate::store::SessionHeadOwner::Run { run: bound }),
            "the refusal names the bound run: {:?}",
            refused.error
        );
        assert_eq!(
            law.head().await.head_revision,
            before,
            "the refused park wrote nothing"
        );
        assert!(
            refused_runtime.set(refused.runtime).is_ok(),
            "one refused park"
        );
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        futures_util::future::join(law.execute_run("run-2"), while_held),
    )
    .await
    .expect("the bound turn's shift ends");

    let runtime = refused_runtime
        .into_inner()
        .expect("the dirty park was refused while the turn was bound");
    let after_turn = law.head().await.head_revision;
    let parked = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        Box::pin((*runtime).park()),
    )
    .await
    .expect("the re-park settles");
    let parked = match parked {
        Ok(parked) => parked,
        Err(refused) => panic!(
            "the refused runtime re-parks from the recorded head once the boundary passed: {:?}",
            refused.error
        ),
    };
    assert_eq!(parked.session_id(), &law.session_id);
    assert_eq!(
        law.head().await.head_revision,
        after_turn,
        "a re-park rehydrated from the recorded head writes nothing"
    );
    let (resident, accepted) = refused_state
        .into_inner()
        .expect("the refused runtime's state");
    let recorded = recorded_host_plugin_namespace(&law).await;
    assert_eq!(
        resident.generation(),
        recorded.generation,
        "adopting a head adopts its applied frontier"
    );
    assert!(accepted > recorded.generation);
    assert_eq!(resident.get(TAIL_KEY), None);
    assert_eq!(
        (recorded.values.get(TAIL_KEY), recorded.generation),
        (None, recorded_before.generation),
        "the recorded head does not carry the uncommitted write"
    );
    let clean = build_runtime(&law.parts, None).await;
    assert_eq!(
        host_plugin_state(&clean).generation(),
        recorded.generation,
        "a runtime built on the recorded head holds its namespace"
    );
    Box::pin(clean.park())
        .await
        .expect("a clean park is never busy");
    assert_eq!(
        law.head().await.head_revision,
        after_turn,
        "a clean park writes nothing"
    );

    let on_the_head = build_runtime(&law.parts, None).await;
    publish_host_plugin_state(
        &on_the_head,
        "landed-publication",
        LANDED_KEY,
        serde_json::json!("accepted on the recorded head"),
    )
    .await;
    Box::pin(on_the_head.park())
        .await
        .expect("a plugin-state-dirty park on the recorded head lands");
    assert_eq!(
        law.head().await.head_revision,
        after_turn + 1,
        "the park commits the accepted write once"
    );
    let landed = recorded_host_plugin_namespace(&law).await;
    assert_eq!(
        (landed.values.get(LANDED_KEY), landed.generation),
        (
            Some(&serde_json::json!("accepted on the recorded head")),
            recorded.generation + 1
        ),
        "the park committed the write it accepted on the recorded head"
    );

    let path = active_path(&law.head().await.graph);
    position_of_once(&path, "answer 2");
    assert_eq!(model.turn_calls.load(Ordering::SeqCst), 2);
}

/// A host withdraws a command transactionally until the shift admits it
/// (FIG-4202): an append submitted while the bound turn holds its pressure
/// summary, and withdrawn before any shift read it, never applies, and a
/// host runtime settles it `Cancelled`. A
/// plugin command the shift already read is being applied: while its code
/// runs, a withdrawal no longer reaches it, and it settles `Completed`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn command_cancellation_before_admission_withdraws_it(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let command_hold = SummaryHold::default();
    let probe = HostPluginProbe {
        command_hold: Some(command_hold.clone()),
        ..HostPluginProbe::default()
    };
    let (law, _model, hold) = bound_turn_session(
        prefix,
        "command-cancellation",
        effect_host,
        stores,
        runner,
        &probe,
    )
    .await;
    let withdrawn_note = "a note the host withdrew";
    let (receipts, _) = submit_while_bound(
        &law,
        &hold,
        vec![(append_command(withdrawn_note), "withdrawn-append")],
        async {},
    )
    .await;
    let withdrawn = receipts[0].clone();
    assert!(
        law.store
            .cancel_queued_work_batch(&law.session_id, withdrawn.batch_id.as_str())
            .await
            .expect("withdraw the append")
            .is_some(),
        "an unadmitted command withdraws"
    );

    let admitted = law
        .submit_command(
            crate::SessionCommand::RunPluginCommand {
                name: <HostNoteCommand as crate::plugin::PluginOperation>::NAME.to_string(),
                args: serde_json::json!({"text": PLUGIN_COMMAND_NOTE}),
            },
            "admitted-command",
        )
        .await;
    law.enqueue("third question").await;
    let during = command_hold.while_held(async {
        assert_eq!(probe.command_runs.load(Ordering::SeqCst), 1);
        assert!(
            law.store
                .cancel_queued_work_batch(&law.session_id, admitted.batch_id.as_str())
                .await
                .expect("try to withdraw the admitted command")
                .is_none(),
            "a command the shift read is no longer withdrawn"
        );
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        futures_util::future::join(law.execute_run("run-3"), during),
    )
    .await
    .expect("the shift applying the admitted command ends");

    assert!(
        law.command_outcome(&withdrawn).await.is_none(),
        "the withdrawn append never settled"
    );
    let mut host = build_runtime(&law.parts, None).await;
    assert!(
        matches!(
            host.settle_session_command(withdrawn)
                .await
                .expect("settle the withdrawn append"),
            crate::SessionCommandSettlement::Cancelled(_)
        ),
        "the withdrawn append settles cancelled"
    );
    assert!(
        matches!(
            law.command_outcome(&admitted).await,
            Some(crate::SessionCommandOutcome::PluginOperation {
                outcome: crate::PluginOperationCommandOutcome::Completed { .. }
            })
        ),
        "the admitted command settles completed"
    );
    assert_eq!(probe.command_runs.load(Ordering::SeqCst), 1);
    let path = active_path(&law.head().await.graph);
    assert!(
        !path.iter().any(|text| text == withdrawn_note),
        "the withdrawn append never landed: {path:?}"
    );
    position_of_once(&path, PLUGIN_COMMAND_NOTE);
}

/// FIG-4391: a host's cancel reaches a plugin task a shift already admitted.
/// The task runs until its cancellation; once the shift runs its code, a
/// withdrawal no longer reaches it, and the host's cancel resolves the
/// task's cancel signal instead. The shift stops the task's code through its
/// cancellation token and settles the command `Cancelled` in the one commit
/// that makes its settlement, with nothing of the task committed (the note
/// it appends after its cancellation never lands), and a submitter reads
/// that settlement back by the command's receipt; a later cancel finds that
/// settlement. The lane goes on: the input queued after the task runs in the
/// same shift. Killed at `crash` and redriven, the task settles cancelled
/// once. FIG-4893 records the pre-run peek: a redrive after that peek replays
/// the task even when the live signal now holds a cancel. The recorded
/// completion peek keeps the cancelled outcome and discards both drafts.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn host_cancel_settles_an_admitted_plugin_task_cancelled(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    crash: Option<HostCommandCrash>,
) {
    let probe = HostPluginProbe::default();
    let protocol = StandardFrameLawProtocol::shared();
    let model = law_model(ModelScript {
        turns: vec![
            (protocol.answer("answer 1"), 1),
            (protocol.answer("answer 2"), 1),
        ],
    });
    let host_effects = effect_host.clone();
    let mut law = LawSession::open(
        prefix,
        &format!("host-task-cancel-{crash:?}").to_lowercase(),
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.host_plugins.push(host_plugin(&probe));
    law.enqueue("first question").await;
    law.execute_run("run-1").await;
    let before = law.head().await.head_revision;

    let receipt = law
        .submit_command(
            crate::SessionCommand::RunPluginTask {
                name: <HostCancellableTask as crate::plugin::PluginOperation>::NAME.to_string(),
                args: serde_json::json!({"text": CANCELLED_TASK_NOTE}),
            },
            "cancelled-task",
        )
        .await;
    law.enqueue("second question").await;
    let mut host = build_runtime(&law.parts, None).await;
    let cancel = async {
        probe.cancellable_task_entered.notified().await;
        assert!(
            law.store
                .cancel_queued_work_batch(&law.session_id, receipt.batch_id.as_str())
                .await
                .expect("try to withdraw the admitted task")
                .is_none(),
            "a task the shift admitted is no longer withdrawn"
        );
        assert_eq!(
            lash_core::runtime::request_plugin_task_cancel(
                law.store.as_ref(),
                &host_effects,
                &receipt
            )
            .await
            .expect("cancel the admitted task"),
            lash_core::runtime::PluginTaskCancelRequest::Requested,
            "the host's cancel reaches the admitted task's cancel signal"
        );
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        futures_util::future::join(work_next(&law, "run-2", crash), cancel),
    )
    .await
    .expect("the shift applying the cancelled task ends");

    assert!(
        matches!(
            law.command_outcome(&receipt).await,
            Some(crate::SessionCommandOutcome::PluginOperation {
                outcome: crate::PluginOperationCommandOutcome::Cancelled,
            })
        ),
        "the admitted task settles cancelled through its command's commit"
    );
    assert!(
        matches!(
            host.settle_session_command(receipt.clone())
                .await
                .expect("settle the cancelled task"),
            crate::SessionCommandSettlement::Applied {
                outcome: crate::SessionCommandOutcome::PluginOperation {
                    outcome: crate::PluginOperationCommandOutcome::Cancelled,
                },
                ..
            }
        ),
        "a submitter reads the cancelled settlement back by the command's receipt"
    );
    assert_eq!(
        lash_core::runtime::request_plugin_task_cancel(law.store.as_ref(), &host_effects, &receipt)
            .await
            .expect("cancel the settled task again"),
        lash_core::runtime::PluginTaskCancelRequest::AlreadySettled,
        "a cancel after the task settled finds its settlement"
    );
    assert_eq!(
        probe.cancellable_task_runs.load(Ordering::SeqCst),
        match crash {
            Some(HostCommandCrash::BeforeCommit | HostCommandCrash::AfterCommit) => 2,
            Some(HostCommandCrash::AfterLaneRead) | None => 1,
        },
        "the task replays its recorded pre-run peek; a cut before that peek runs its code once"
    );
    let head = law.head().await;
    assert_eq!(
        head.head_revision,
        before + 4,
        "the operation and next turn each publish one plugin transition and commit once"
    );
    let path = active_path(&head.graph);
    assert!(
        !path.iter().any(|text| text == CANCELLED_TASK_NOTE),
        "nothing of the cancelled task commits: {path:?}"
    );
    assert!(
        position_of_once(&path, "answer 1") < position_of_once(&path, "second question"),
        "the input queued after the task runs once the task settled: {path:?}"
    );
    assert_eq!(model.turn_calls.load(Ordering::SeqCst), 2);
}

/// The note a task reproduces on replay, even when a late cancel arrives
/// after its recorded completion decision (FIG-4893).
const RERUN_TASK_NOTE: &str = "a note the host's plugin task appended after its rerun";

/// A cancel reaches a task rerun after a crash before settlement, but cannot
/// change its recorded completion decision. FIG-4893 (196ff1c4a9) records
/// the final peek before the settling commit, amending FIG-4453's earlier
/// live-peek contract. The live signal still answers `Requested` and stops
/// the rerun's code; the replayed final peek keeps the original completed
/// result and commits its reproduced graph draft exactly once (ADR 0105).
/// The input queued after the task runs in the same shift.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn host_cancel_reaches_a_plugin_task_rerun_after_a_crash_before_its_settlement(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let probe = HostPluginProbe::default();
    let protocol = StandardFrameLawProtocol::shared();
    let model = law_model(ModelScript {
        turns: vec![
            (protocol.answer("answer 1"), 1),
            (protocol.answer("answer 2"), 1),
        ],
    });
    let host_effects = effect_host.clone();
    let mut law = LawSession::open(
        prefix,
        "host-task-cancel-rerun",
        effect_host,
        stores,
        runner,
        protocol,
        model.provider.clone(),
    )
    .await;
    law.parts.host_plugins.push(host_plugin(&probe));
    law.enqueue("first question").await;
    law.execute_run("run-1").await;
    let before = law.head().await.head_revision;

    let receipt = law
        .submit_command(
            crate::SessionCommand::RunPluginTask {
                name: <HostReturnsOnceTask as crate::plugin::PluginOperation>::NAME.to_string(),
                args: serde_json::json!({"text": RERUN_TASK_NOTE}),
            },
            "rerun-task",
        )
        .await;
    law.enqueue("second question").await;
    let cancel = async {
        probe.returns_once_task_rerun.notified().await;
        assert_eq!(
            lash_core::runtime::request_plugin_task_cancel(
                law.store.as_ref(),
                &host_effects,
                &receipt
            )
            .await
            .expect("cancel the rerun task"),
            lash_core::runtime::PluginTaskCancelRequest::Requested,
            "a cancel during the rerun reaches the unsettled task"
        );
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        futures_util::future::join(
            shift_crashed_at(&law, "run-2", HostCommandCrash::BeforeCommit),
            cancel,
        ),
    )
    .await
    .expect("the redrive applying the completed task ends");

    assert!(
        matches!(
            law.command_outcome(&receipt).await,
            Some(crate::SessionCommandOutcome::PluginOperation {
                outcome: crate::PluginOperationCommandOutcome::Completed {
                    plugin_id,
                    output,
                    events,
                    pending_turn_inputs,
                },
            }) if plugin_id == HOST_PLUGIN_ID
                && output == serde_json::json!({"text": RERUN_TASK_NOTE, "appended": true})
                && events.is_empty()
                && pending_turn_inputs.is_empty()
        ),
        "the recorded completion decision survives a late cancel during replay"
    );
    assert_eq!(
        probe.returns_once_task_runs.load(Ordering::SeqCst),
        2,
        "the task's code ran again after the crash: its first return settled nothing"
    );
    let head = law.head().await;
    assert_eq!(
        head.head_revision,
        before + 4,
        "the operation and next turn each publish one plugin transition and commit once"
    );
    let path = active_path(&head.graph);
    let note = position_of_once(&path, RERUN_TASK_NOTE);
    assert!(
        position_of_once(&path, "answer 1") < note
            && note < position_of_once(&path, "second question"),
        "the input queued after the task runs once the task settled: {path:?}"
    );
    assert_eq!(model.turn_calls.load(Ordering::SeqCst), 2);
}
