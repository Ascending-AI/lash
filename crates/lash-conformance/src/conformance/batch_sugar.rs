//! The `batch` sugar laws (ADR 0116 §7.2).
//!
//! `batch` is protocol sugar, not a tool: the standard driver expands each
//! wrapper into the step's one tool group beside the response's native calls
//! and folds the members' results back into one batch result. These laws drive
//! real turns on a tier, through its [`crate::ConformanceTurnRunner`], and pin
//! what that means where a tier can get it wrong: member admission and
//! identity, the fold's stability across replay and a crash, cancellation,
//! the fully refused wrapper, and the transcript the model and host see.
//!
//! The registering crate hands in the standard protocol twice — with `batch`
//! offered at its default maximum, and with it withheld — because this crate
//! does not construct protocols.

use crate::admit;
use lash_core::testing::TestTurnDrive as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_sansio::sync::MutexExt as _;
use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

/// The deadlock budget of one law turn: only a turn that can never settle
/// reaches it. A 64-member turn on the e2e replay leg, where every await
/// suspends and replays, takes about a minute.
const TURN_BUDGET: Duration = Duration::from_secs(180);

/// The `batch` ceiling: the most members one call takes (ADR 0116 §2).
const CEILING: usize = 64;

/// The standard protocol, handed in by the registering crate.
#[derive(Clone)]
pub struct BatchSugarFactories {
    /// `batch` offered at its default maximum of 64 members.
    pub enabled: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    /// `batch` withheld: a call named `batch` is an ordinary unknown tool.
    pub disabled: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
}

/// One executed member body, as the tool saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Execution {
    tool: String,
    value: String,
    attempt: u32,
    call_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum GateEvent {
    Started(String),
    Answered(String),
}

/// What the law's tools share with the law.
#[derive(Default)]
struct Witness {
    executions: Mutex<Vec<Execution>>,
    /// Every tool name a before-tool hook saw.
    hooked: Mutex<Vec<String>>,
    /// Gate members of the current run that must all start before any answers.
    barrier: Mutex<Vec<String>>,
    /// Gate members that park until [`Witness::release_held`].
    held: Mutex<Vec<String>>,
    released: AtomicBool,
    gate_log: Mutex<Vec<GateEvent>>,
    notify: tokio::sync::Notify,
}

impl Witness {
    fn executions(&self) -> Vec<Execution> {
        self.executions.lock_recover().clone()
    }

    fn executed(&self, tool: &str, value: &str) -> usize {
        self.executions()
            .iter()
            .filter(|execution| execution.tool == tool && execution.value == value)
            .count()
    }

    fn started(&self, value: &str) -> bool {
        self.gate_log
            .lock_recover()
            .iter()
            .any(|event| *event == GateEvent::Started(value.to_string()))
    }

    fn set_barrier(&self, members: &[&str]) {
        *self.barrier.lock_recover() = members.iter().map(|member| member.to_string()).collect();
        self.gate_log.lock_recover().clear();
    }

    fn hold(&self, members: &[&str]) {
        *self.held.lock_recover() = members.iter().map(|member| member.to_string()).collect();
        self.released.store(false, Ordering::SeqCst);
    }

    fn release_held(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn gate_log(&self) -> Vec<GateEvent> {
        self.gate_log.lock_recover().clone()
    }

    async fn gate(&self, value: &str) {
        self.gate_log
            .lock_recover()
            .push(GateEvent::Started(value.to_string()));
        self.notify.notify_waiters();
        loop {
            let notified = self.notify.notified();
            let held = self
                .held
                .lock_recover()
                .iter()
                .any(|member| member == value)
                && !self.released.load(Ordering::SeqCst);
            let barrier = self.barrier.lock_recover().clone();
            let log = self.gate_log.lock_recover().clone();
            let waiting = barrier.iter().any(|member| {
                !log.iter()
                    .any(|event| *event == GateEvent::Started(member.clone()))
            }) && barrier.iter().any(|member| member == value);
            if !held && !waiting {
                break;
            }
            notified.await;
        }
        self.gate_log
            .lock_recover()
            .push(GateEvent::Answered(value.to_string()));
        self.notify.notify_waiters();
    }
}

fn value_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { "value": { "type": "string" } },
        "required": ["value"],
        "additionalProperties": false
    })
}

fn sugar_tool(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "A batch sugar law tool: records each execution and answers its value.",
        value_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

const TOOLS: [&str; 3] = ["echo", "gate", "guarded"];

/// `echo` answers at once, `gate` answers once the law's barrier and holds
/// let it, and `guarded` is always denied by the law's before-tool hook.
struct SugarTools {
    witness: Arc<Witness>,
}

#[async_trait::async_trait]
impl crate::ToolProvider for SugarTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        TOOLS
            .iter()
            .map(|name| sugar_tool(name).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        TOOLS
            .contains(&name)
            .then(|| Arc::new(sugar_tool(name).contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let value = call
            .args
            .get("value")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        self.witness.executions.lock_recover().push(Execution {
            tool: call.name().to_string(),
            value: value.clone(),
            attempt: call.context.attempt_number(),
            call_id: call.context.tool_call_id().map(str::to_string),
        });
        if call.name() == "gate" {
            self.witness.gate(&value).await;
        }
        crate::ToolOutcome::ok(serde_json::json!({ "tool": call.name(), "value": value })).into()
    }
}

/// The law's tools and its before-tool hook: the hook records every tool name
/// it is asked about and denies `guarded`.
fn sugar_plugin(witness: Arc<Witness>) -> Arc<dyn crate::facade_support::PluginFactory> {
    let hooked = Arc::clone(&witness);
    let spec = crate::facade_support::PluginSpec::new()
        .with_tool_provider(Arc::new(SugarTools { witness }))
        .with_before_tool_call(Arc::new(move |context| {
            hooked.hooked.lock_recover().push(context.tool_name.clone());
            let deny = context.tool_name == "guarded";
            Box::pin(async move {
                Ok(if deny {
                    vec![
                        crate::facade_support::BeforeToolCallPluginDirective::ShortCircuitTool(
                            crate::facade_support::ShortCircuitToolDirective::new(
                                crate::ToolOutcome::failure(crate::ToolFailure::tool(
                                    crate::ToolFailureClass::PermissionDenied,
                                    "approval_denied",
                                    "the law denies `guarded`",
                                )),
                            ),
                        ),
                    ]
                } else {
                    Vec::new()
                })
            })
        }));
    Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-batch-sugar",
        spec,
    ))
}

/// A model that answers the turn's n-th step with `responses[n]`, and with a
/// closing text past the script; `on_call` sees each call's step before it is
/// answered. The step is read off the request — how many assistant messages
/// it already carries — not counted per execution: a recovered execution
/// replays the steps its journal holds without asking the model and asks
/// again only for the step that was in flight, which must get that step's
/// answer, not the turn's first.
fn scripted_model(
    responses: Vec<crate::LlmResponse>,
    on_call: Arc<dyn Fn(usize) + Send + Sync>,
) -> crate::testing::TestProvider {
    crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |request| {
            let step = request
                .messages
                .iter()
                .filter(|message| matches!(message.role, lash_core::llm::types::LlmRole::Assistant))
                .count();
            on_call(step);
            let next = responses.get(step).cloned();
            async move { Ok(next.unwrap_or_else(|| text("sugar laws complete"))) }
        })
        .build()
}

fn text(text: &str) -> crate::LlmResponse {
    crate::LlmResponse {
        parts: vec![crate::LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        ..crate::LlmResponse::default()
    }
}

fn response(parts: Vec<crate::LlmOutputPart>) -> crate::LlmResponse {
    crate::LlmResponse {
        parts,
        ..crate::LlmResponse::default()
    }
}

fn native(call_id: &str, tool: &str, value: &str) -> crate::LlmOutputPart {
    crate::LlmOutputPart::ToolCall {
        call_id: call_id.to_string(),
        tool_name: tool.to_string(),
        input_json: serde_json::json!({ "value": value }).to_string(),
        replay: None,
    }
}

fn wrapper(call_id: &str, tool_calls: serde_json::Value) -> crate::LlmOutputPart {
    crate::LlmOutputPart::ToolCall {
        call_id: call_id.to_string(),
        tool_name: "batch".to_string(),
        input_json: serde_json::json!({ "tool_calls": tool_calls }).to_string(),
        replay: Some(lash_core::llm::types::ProviderReplayMeta {
            item_id: Some(format!("provider-{call_id}")),
            ..lash_core::llm::types::ProviderReplayMeta::default()
        }),
    }
}

fn member(tool: &str, value: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "tool": tool, "parameters": { "value": value } })
}

fn members(tool: &str, values: &[&str]) -> serde_json::Value {
    serde_json::Value::Array(
        values
            .iter()
            .map(|value| member(tool, serde_json::json!(value)))
            .collect(),
    )
}

/// Everything one law turn is built from. Restate re-runs a turn's handler
/// from the top on every replay, so each execution builds its runtime afresh
/// from these and reaches the same journaled commands.
#[derive(Clone)]
struct SugarTurn {
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    session_id: SessionId,
    turn_id: TurnId,
    factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    witness: Arc<Witness>,
    script: Vec<crate::LlmResponse>,
    on_call: Arc<dyn Fn(usize) + Send + Sync>,
    /// A layer over the turn's own scope, for a law that counts the groups
    /// the turn opens.
    layer: Option<Arc<dyn crate::testing::EffectLayer>>,
}

impl SugarTurn {
    fn new(
        prefix: &str,
        name: &str,
        host: &Arc<dyn crate::EffectHost>,
        stores: &Arc<dyn crate::StoreSet>,
        factories: &[Arc<dyn crate::facade_support::PluginFactory>],
        script: Vec<crate::LlmResponse>,
    ) -> Self {
        let session_id = SessionId::from(format!("{prefix}-batch-sugar-{name}"));
        Self {
            host: Arc::clone(host),
            stores: Arc::clone(stores),
            turn_id: TurnId::from(format!("{session_id}-turn")),
            session_id,
            factories: factories.to_vec(),
            witness: Arc::new(Witness::default()),
            script,
            on_call: Arc::new(|_| {}),
            layer: None,
        }
    }

    fn admitted(&self) -> crate::AdmittedScope {
        admit(crate::ExecutionScope::turn(&self.session_id, &self.turn_id))
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn drive(
        &self,
        scope: crate::ScopedEffectController<'_>,
    ) -> Option<Result<crate::AssembledTurn, crate::RuntimeError>> {
        let mut config =
            crate::LawBackend::over_stores(Arc::clone(&self.stores), Arc::clone(&self.host))
                .host_config(
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1),
                );
        config.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(
            scripted_model(self.script.clone(), Arc::clone(&self.on_call)).into_handle(),
        ));
        let mut policy = crate::testing::mock_session_policy();
        policy.session_id = Some(self.session_id.clone());
        let state = crate::RuntimeSessionState {
            session_id: self.session_id.clone(),
            policy: policy.clone(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::bounded(8),
            ))
        };
        let mut runtime = Box::pin(
            crate::LashRuntime::builder(config, crate::testing::runtime_lease_owner())
                .with_session_id(&self.session_id)
                .with_policy(policy)
                .with_initial_state(state)
                .with_plugin_factories(
                    self.factories
                        .iter()
                        .cloned()
                        .chain([sugar_plugin(Arc::clone(&self.witness))])
                        .collect(),
                )
                .with_store(crate::conformance::helpers::session_view(
                    &crate::conformance::law_session_store(self.stores.as_ref(), &self.session_id)
                        .await,
                    self.session_id.clone(),
                ))
                .with_process_work(crate::testing::process_work_wiring_for_registry(
                    self.stores.process_registry(),
                ))
                .with_queued_work(Arc::new(crate::NoSessionWork::new()))
                .build(),
        )
        .await
        .expect("build the batch sugar conformance runtime");
        let scope = match &self.layer {
            Some(layer) => {
                crate::testing::LayeredEffectHost::layer_scoped(scope, Arc::clone(layer))
                    .expect("layer the law turn's scope")
            }
            None => scope,
        };
        let options = crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope);
        let mut input = crate::TurnInput::text("run the batch sugar law");
        input.trace_turn_id = Some(self.turn_id.clone());
        tokio::time::timeout(TURN_BUDGET, runtime.drive_turn(input, options))
            .await
            .ok()
    }

    /// The attempt a runner drives: every execution builds its runtime afresh
    /// and reports its turn on `turns`.
    fn attempt(
        &self,
        turns: tokio::sync::mpsc::UnboundedSender<
            Option<Result<crate::AssembledTurn, crate::RuntimeError>>,
        >,
    ) -> crate::ConformanceTurnAttempt {
        let turn = self.clone();
        Arc::new(move |scope| {
            let turn = turn.clone();
            let turns = turns.clone();
            Box::pin(async move {
                let Some(assembled) = turn.drive(scope).await else {
                    let _ = turns.send(None);
                    return crate::ConformanceTurnEnd::Settled;
                };
                let end = crate::ConformanceTurnEnd::of(&assembled);
                let _ = turns.send(Some(assembled));
                end
            })
        })
    }

    /// Runs the turn to its end on `runner` and returns it.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: a runner that drops the turn is a fixture defect"
    )]
    async fn run(&self, runner: &Arc<dyn crate::ConformanceTurnRunner>) -> crate::AssembledTurn {
        let (turns, mut ran) = tokio::sync::mpsc::unbounded_channel();
        runner.run_turn(self.admitted(), self.attempt(turns)).await;
        let mut last = None;
        while let Ok(turn) = ran.try_recv() {
            last = Some(turn);
        }
        last.expect("the tier's turn runner ran the law's turn")
            .unwrap_or_else(|| {
                panic!(
                    "the batch sugar turn of `{}` did not settle within {TURN_BUDGET:?}; \
                     member executions: {:?}; gate log: {:?}",
                    self.session_id,
                    self.witness.executions(),
                    self.witness.gate_log(),
                )
            })
            .unwrap_or_else(|error| {
                panic!(
                    "the batch sugar turn of `{}` failed: {error}",
                    self.session_id
                )
            })
    }
}

/// A turn's tool records: the calls the host saw, one per stream event.
fn records(turn: &crate::AssembledTurn) -> Vec<crate::ToolCallRecord> {
    turn.tool_calls.clone()
}

fn record<'a>(turn: &'a crate::AssembledTurn, call_id: &str) -> Vec<&'a crate::ToolCallRecord> {
    turn.tool_calls
        .iter()
        .filter(|record| record.call_id.as_deref() == Some(call_id))
        .collect()
}

/// A wrapper record's rows, as `(index, tool, success)`.
fn rows(record: &crate::ToolCallRecord) -> Vec<(u64, String, bool)> {
    record.output.value_for_projection()["results"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    (
                        row["index"].as_u64().unwrap_or(u64::MAX),
                        row["tool"].as_str().unwrap_or_default().to_string(),
                        row["success"].as_bool().unwrap_or(false),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Every tool result the transcript holds, as `(call id, tool, content)`.
fn transcript_results(turn: &crate::AssembledTurn) -> Vec<(String, String, String)> {
    let view = turn.state.read_view();
    view.messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.kind() == crate::PartKind::ToolResult)
        .map(|part| {
            (
                part.tool_call_id().unwrap_or_default().to_string(),
                part.tool_name().unwrap_or_default().to_string(),
                part.content().into_owned(),
            )
        })
        .collect()
}

/// Every tool call the transcript's assistant turns hold, as `(call id, tool,
/// replay item id)`.
fn transcript_calls(turn: &crate::AssembledTurn) -> Vec<(String, String, Option<String>)> {
    let view = turn.state.read_view();
    view.messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.kind() == crate::PartKind::ToolCall)
        .map(|part| {
            (
                part.tool_call_id().unwrap_or_default().to_string(),
                part.tool_name().unwrap_or_default().to_string(),
                part.tool_replay().and_then(|replay| replay.item_id.clone()),
            )
        })
        .collect()
}

fn assert_finished(context: &str, turn: &crate::AssembledTurn) {
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "{context}: the turn must finish: {:?}; issues: {:?}",
        turn.outcome,
        turn.errors,
    );
}

fn assert_no_member_is_a_call(context: &str, turn: &crate::AssembledTurn) {
    let members = records(turn)
        .into_iter()
        .filter_map(|record| record.call_id)
        .chain(
            transcript_results(turn)
                .into_iter()
                .map(|(call_id, _, _)| call_id),
        )
        .chain(
            transcript_calls(turn)
                .into_iter()
                .map(|(call_id, _, _)| call_id),
        )
        .filter(|call_id| call_id.contains("/batch/"))
        .collect::<Vec<_>>();
    assert!(
        members.is_empty(),
        "{context}: no member is a call of its own: {members:?}"
    );
}

/// 64 members run; 65 refuse the whole wrapper and start nothing; an empty or
/// malformed list refuses the wrapper; a nested `batch`, an unavailable tool,
/// a schema failure and a denied approval are refused rows; the wrapper runs
/// no hook, so its approval grants nothing; repeated model call ids,
/// identical arguments and a reused frame alias no member; and a disabled
/// `batch` is an unknown tool.
pub async fn batch_admission_and_identity_contract(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    factories: BatchSugarFactories,
) {
    let max = CEILING;
    let full = (0..max)
        .map(|index| format!("m{index}"))
        .collect::<Vec<_>>();
    let full = full.iter().map(String::as_str).collect::<Vec<_>>();
    let over = vec!["over"; max + 1];
    let script = vec![
        response(vec![
            native("same", "echo", "native-1"),
            native("same", "echo", "native-2"),
            wrapper("wfull", members("echo", &full)),
            wrapper("wover", members("echo", &over)),
            wrapper("wempty", serde_json::json!([])),
            wrapper("wmalformed", serde_json::json!("echo")),
            wrapper(
                "wmixed",
                serde_json::json!([
                    { "tool": "batch", "parameters": { "tool_calls": [] } },
                    member("ghost", serde_json::json!("ghost")),
                    member("echo", serde_json::json!(5)),
                    member("guarded", serde_json::json!("guarded")),
                    member("echo", serde_json::json!("twin")),
                    member("echo", serde_json::json!("twin")),
                ]),
            ),
        ]),
        // A later step reuses the wrapper's provider call id.
        response(vec![wrapper("wmixed", members("echo", &["again"]))]),
    ];
    let law = SugarTurn::new(
        prefix,
        "admission",
        &host,
        &stores,
        &factories.enabled,
        script,
    );
    let turn = law.run(&runner).await;
    let context = format!("{prefix}/batch-admission");
    assert_finished(&context, &turn);
    let witness = &law.witness;

    for value in &full {
        assert_eq!(
            witness.executed("echo", value),
            1,
            "{context}: member `{value}` runs once"
        );
    }
    assert_eq!(
        witness.executed("echo", "over"),
        0,
        "{context}: a wrapper over {max} starts nothing"
    );
    assert_eq!(
        witness.executed("echo", "native-1"),
        1,
        "{context}: a repeated call id aliases nothing"
    );
    assert_eq!(
        witness.executed("echo", "native-2"),
        1,
        "{context}: a repeated call id aliases nothing"
    );
    assert_eq!(
        witness.executed("echo", "twin"),
        2,
        "{context}: identical arguments alias no member"
    );
    assert_eq!(
        witness.executed("echo", "again"),
        1,
        "{context}: a reused frame aliases no member"
    );
    assert_eq!(
        witness.executed("guarded", "guarded"),
        0,
        "{context}: a denied member never runs"
    );
    assert!(
        witness
            .executions()
            .iter()
            .all(|execution| execution.value != "5"),
        "{context}: a schema failure never runs"
    );
    assert!(
        !witness
            .hooked
            .lock_recover()
            .iter()
            .any(|tool| tool == "batch"),
        "{context}: the wrapper is not a tool invocation and runs no hook, so it grants nothing"
    );
    assert!(
        witness
            .hooked
            .lock_recover()
            .iter()
            .any(|tool| tool == "guarded"),
        "{context}: every member is admitted on its own"
    );

    let wfull = record(&turn, "wfull");
    assert_eq!(wfull.len(), 1, "{context}: one record per wrapper");
    assert!(wfull[0].output.is_success());
    let wfull_rows = rows(wfull[0]);
    assert_eq!(wfull_rows.len(), max);
    assert!(
        wfull_rows
            .iter()
            .enumerate()
            .all(|(index, (row, tool, ok))| { *row == index as u64 && tool == "echo" && *ok })
    );
    for refused in ["wover", "wempty", "wmalformed"] {
        let wrapper = record(&turn, refused);
        assert_eq!(wrapper.len(), 1, "{context}: `{refused}` answers once");
        assert!(
            !wrapper[0].output.is_success(),
            "{context}: `{refused}` is refused whole"
        );
    }
    let mixed = record(&turn, "wmixed");
    assert_eq!(
        mixed.len(),
        2,
        "{context}: each step's wrapper answers on its own"
    );
    assert_eq!(
        rows(mixed[0]),
        vec![
            (0, "batch".to_string(), false),
            (1, "ghost".to_string(), false),
            (2, "echo".to_string(), false),
            (3, "guarded".to_string(), false),
            (4, "echo".to_string(), true),
            (5, "echo".to_string(), true),
        ],
        "{context}: refused members are rows at their index, in member order"
    );
    assert!(
        mixed[0].output.is_success(),
        "{context}: a wrapper succeeds even when rows fail"
    );
    assert_eq!(rows(mixed[1]), vec![(0, "echo".to_string(), true)]);
    // ADR 0117 §8: a provider id repeated within one response is repaired at
    // the boundary. The first call keeps it, the later one takes a
    // deterministic correlation id, and both are recorded.
    let same = ["native-1", "native-2"].map(|value| {
        let calls = turn
            .tool_calls
            .iter()
            .filter(|record| record.tool == "echo" && record.args["value"] == value)
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 1, "{context}: `{value}` is recorded once");
        calls[0].call_id.clone()
    });
    assert_eq!(
        same[0].as_deref(),
        Some("same"),
        "{context}: the first call keeps its provider id"
    );
    assert!(
        same[1]
            .as_deref()
            .is_some_and(|call_id| call_id.starts_with("lashcall_")),
        "{context}: the repeated provider id is repaired to a correlation id: {same:?}"
    );
    assert_no_member_is_a_call(&context, &turn);

    // Withheld, `batch` is an unknown tool: nothing expands, nothing runs.
    let law = SugarTurn::new(
        prefix,
        "disabled",
        &host,
        &stores,
        &factories.disabled,
        vec![response(vec![wrapper("w", members("echo", &["hidden"]))])],
    );
    let turn = law.run(&runner).await;
    let context = format!("{prefix}/batch-disabled");
    assert_finished(&context, &turn);
    assert_eq!(
        law.witness.executed("echo", "hidden"),
        0,
        "{context}: nothing expands"
    );
    let wrapper = record(&turn, "w");
    assert_eq!(wrapper.len(), 1);
    assert!(
        !wrapper[0].output.is_success() && rows(wrapper[0]).is_empty(),
        "{context}: a disabled `batch` is an ordinary unknown tool: {:?}",
        wrapper[0].output
    );
}

/// The distinct groups a turn opened, by group key: a Restate replay re-runs
/// the turn's handler and passes its recorded opens through the layer again.
#[derive(Default)]
struct GroupOpens {
    opened: Mutex<std::collections::BTreeSet<String>>,
}

#[async_trait::async_trait]
impl crate::testing::EffectLayer for GroupOpens {
    async fn open_effect_group(
        &self,
        inner: &dyn crate::RuntimeEffectController,
        group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        self.opened
            .lock_recover()
            .insert(group.group_key().to_string());
        inner.open_effect_group(group).await
    }
}

/// A response whose only call is a fully refused wrapper opens no group and
/// answers the folded rows. The same wrapper with one admitted member opens
/// one, so the count is the turn's own.
pub async fn batch_all_refused_opens_no_group(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    factories: BatchSugarFactories,
) {
    for (name, tool_calls, groups) in [
        (
            "all-refused",
            serde_json::json!([
                { "tool": "batch", "parameters": {} },
                { "tool": "batch", "parameters": {} },
            ]),
            0,
        ),
        (
            "one-admitted",
            serde_json::json!([
                { "tool": "batch", "parameters": {} },
                member("echo", serde_json::json!("admitted")),
            ]),
            1,
        ),
    ] {
        let context = format!("{prefix}/batch-{name}");
        let opens = Arc::new(GroupOpens::default());
        let mut law = SugarTurn::new(
            prefix,
            name,
            &host,
            &stores,
            &factories.enabled,
            vec![response(vec![wrapper("w", tool_calls)])],
        );
        law.layer = Some(Arc::clone(&opens) as Arc<dyn crate::testing::EffectLayer>);
        let turn = law.run(&runner).await;
        assert_finished(&context, &turn);
        assert_eq!(
            opens.opened.lock_recover().len(),
            groups,
            "{context}: the step opens a group only for admitted members"
        );
        let wrapper = record(&turn, "w");
        assert_eq!(
            wrapper.len(),
            1,
            "{context}: the wrapper answers its folded rows"
        );
        assert_eq!(rows(wrapper[0]).len(), 2);
        assert_eq!(rows(wrapper[0])[0], (0, "batch".to_string(), false));
    }
}

/// The transcript and the host's records show one call and one result per
/// wrapper, under the provider's call id and replay metadata, and no member
/// call.
pub async fn batch_folds_to_one_transcript_call(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    factories: BatchSugarFactories,
) {
    let context = format!("{prefix}/batch-transcript");
    let law = SugarTurn::new(
        prefix,
        "transcript",
        &host,
        &stores,
        &factories.enabled,
        vec![response(vec![
            native("native", "echo", "native"),
            wrapper("wrapper", members("echo", &["a", "b", "c"])),
        ])],
    );
    let turn = law.run(&runner).await;
    assert_finished(&context, &turn);
    assert_eq!(
        transcript_calls(&turn),
        vec![
            ("native".to_string(), "echo".to_string(), None),
            (
                "wrapper".to_string(),
                "batch".to_string(),
                Some("provider-wrapper".to_string())
            ),
        ],
        "{context}: the assistant turn keeps the provider's calls and replay metadata"
    );
    let results = transcript_results(&turn)
        .into_iter()
        .map(|(call_id, tool, _)| (call_id, tool))
        .collect::<Vec<_>>();
    assert_eq!(
        results,
        vec![
            ("native".to_string(), "echo".to_string()),
            ("wrapper".to_string(), "batch".to_string()),
        ],
        "{context}: one result per call, the wrapper's under its own id"
    );
    let reported = records(&turn)
        .into_iter()
        .map(|record| (record.call_id.unwrap_or_default(), record.tool))
        .collect::<Vec<_>>();
    assert_eq!(
        reported, results,
        "{context}: the host's records are the folded calls"
    );
    let (_, _, presented) = transcript_results(&turn)
        .into_iter()
        .find(|(call_id, _, _)| call_id == "wrapper")
        .unwrap_or_default();
    let presented: serde_json::Value =
        serde_json::from_str(&presented).unwrap_or(serde_json::Value::Null);
    let values = presented["results"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row["result"]["value"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert_eq!(
        values,
        vec!["a", "b", "c"],
        "{context}: rows in member order"
    );
    assert_no_member_is_a_call(&context, &turn);
}

/// The wrapper's folded result as the transcript and the host's record hold
/// it, and the member executions behind it.
#[derive(Debug, PartialEq)]
struct Fold {
    presented: String,
    rows: Vec<(u64, String, bool)>,
}

fn fold_of(turn: &crate::AssembledTurn, call_id: &str) -> Fold {
    let presented = transcript_results(turn)
        .into_iter()
        .find(|(id, _, _)| id == call_id)
        .map(|(_, _, content)| content)
        .unwrap_or_default();
    Fold {
        presented,
        rows: record(turn, call_id)
            .first()
            .map(|record| rows(record))
            .unwrap_or_default(),
    }
}

/// The child replay keys the tier journaled for `scope`, with the session
/// named generically, when the runner can read them.
async fn child_keys(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    law: &SugarTurn,
) -> Option<Vec<String>> {
    let scope = crate::ExecutionScope::turn(&law.session_id, &law.turn_id);
    let session = law.session_id.to_string();
    let turn = law.turn_id.to_string();
    runner.recorded_replay_keys(&scope).await.map(|keys| {
        let mut keys = keys
            .into_iter()
            .filter(|key| key.contains(":child:"))
            .map(|key| key.replace(&turn, "{turn}").replace(&session, "{session}"))
            .collect::<Vec<_>>();
        keys.sort();
        keys
    })
}

/// A cold replay and a perturbed-schedule replay of one `batch` produce the
/// identical child keys, rows and presentation, and incorporate each
/// settlement once.
///
/// The reference run settles its members in input order. The perturbed run
/// settles them in reverse. The cold run crashes after the group settled,
/// inside the next model call, and its recovery replays the recorded group:
/// no member runs again.
pub async fn batch_replay_preserves_fold_and_ranks(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    factories: BatchSugarFactories,
) {
    let context = format!("{prefix}/batch-replay");
    let names = ["r0", "r1", "r2", "r3"];
    let script = || vec![response(vec![wrapper("w", members("gate", &names))])];

    let reference = SugarTurn::new(
        prefix,
        "replay-reference",
        &host,
        &stores,
        &factories.enabled,
        script(),
    );
    reference.witness.set_barrier(&names);
    let reference_turn = reference.run(&runner).await;
    assert_finished(&context, &reference_turn);
    let reference_fold = fold_of(&reference_turn, "w");
    assert_eq!(reference_fold.rows.len(), 4);

    // Perturbed: every member starts, then they settle last to first.
    let perturbed = SugarTurn::new(
        prefix,
        "replay-perturbed",
        &host,
        &stores,
        &factories.enabled,
        script(),
    );
    perturbed.witness.set_barrier(&names);
    perturbed.witness.hold(&names[..3]);
    let release = {
        let witness = Arc::clone(&perturbed.witness);
        crate::task::spawn(async move {
            // Let the last member answer, then each held one, last first.
            for member in names[..3].iter().rev() {
                let answered = |witness: &Witness, value: &str| {
                    witness
                        .gate_log()
                        .contains(&GateEvent::Answered(value.to_string()))
                };
                let next = names[names.iter().position(|name| name == member).unwrap_or(0) + 1];
                while !answered(&witness, next) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let mut held = witness.held.lock_recover();
                held.retain(|name| name != member);
                drop(held);
                witness.notify.notify_waiters();
            }
        })
    };
    let perturbed_turn = perturbed.run(&runner).await;
    let _ = release.await;
    assert_finished(&context, &perturbed_turn);
    let answered = perturbed
        .witness
        .gate_log()
        .into_iter()
        .filter_map(|event| match event {
            GateEvent::Answered(value) => Some(value),
            GateEvent::Started(_) => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        answered,
        vec!["r3", "r2", "r1", "r0"],
        "{context}: the schedule was perturbed"
    );
    assert_eq!(
        fold_of(&perturbed_turn, "w"),
        reference_fold,
        "{context}: a perturbed schedule folds the identical rows and presentation"
    );

    // Cold: crash once the group settled, inside the next model call, then
    // recover the turn from what the tier recorded.
    let mut cold = SugarTurn::new(
        prefix,
        "replay-cold",
        &host,
        &stores,
        &factories.enabled,
        script(),
    );
    cold.witness.set_barrier(&names);
    let crash = crate::ConformanceCrash::new();
    cold.on_call = {
        let crash = crash.clone();
        Arc::new(move |step| {
            if step == 1 {
                crash.fire();
            }
        })
    };
    let (turns, _ignored) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn_until_crash(cold.admitted(), cold.attempt(turns), crash)
        .await;
    for name in names {
        assert_eq!(
            cold.witness.executed("gate", name),
            1,
            "{context}: `{name}` ran before the crash"
        );
    }
    cold.on_call = Arc::new(|_| {});
    let cold_turn = cold.run(&runner).await;
    assert_finished(&context, &cold_turn);
    for name in names {
        assert_eq!(
            cold.witness.executed("gate", name),
            1,
            "{context}: the recovery incorporates `{name}`'s recorded settlement once"
        );
    }
    assert_eq!(
        fold_of(&cold_turn, "w"),
        reference_fold,
        "{context}: a cold replay folds the identical rows and presentation"
    );

    // Child keys, where the tier can read its journal.
    let keys = [
        child_keys(&runner, &reference).await,
        child_keys(&runner, &perturbed).await,
        child_keys(&runner, &cold).await,
    ];
    if let [Some(reference), Some(perturbed), Some(cold)] = keys {
        assert_eq!(perturbed, reference, "{context}: identical child keys");
        assert_eq!(cold, reference, "{context}: identical child keys");
    }
}

/// A crash after one member's final and before the fold: the recovery reuses
/// the settled member, incorporates each unfinished member once — settled in
/// the child invocation that outlived the crash, or run again under a fresh
/// attempt — and a following barrier batch still overlaps, so cached replies
/// cannot mask serialization.
pub async fn batch_redrive_reuses_children(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    factories: BatchSugarFactories,
) {
    let context = format!("{prefix}/batch-redrive");
    let script = vec![
        response(vec![wrapper(
            "w",
            serde_json::json!([
                member("echo", serde_json::json!("settled")),
                member("gate", serde_json::json!("open-a")),
                member("gate", serde_json::json!("open-b")),
            ]),
        )]),
        response(vec![wrapper(
            "after",
            members("gate", &["after-a", "after-b", "after-c"]),
        )]),
    ];
    let mut law = SugarTurn::new(
        prefix,
        "redrive",
        &host,
        &stores,
        &factories.enabled,
        script,
    );
    // The unfinished members park until the crash; the settled one answers.
    law.witness.hold(&["open-a", "open-b"]);
    let crash = crate::ConformanceCrash::new();
    let fire = {
        let witness = Arc::clone(&law.witness);
        let crash = crash.clone();
        crate::task::spawn(async move {
            while !(witness.executed("echo", "settled") == 1
                && witness.started("open-a")
                && witness.started("open-b"))
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // The settled member's final commits once its attempt returns;
            // the unfinished members cannot settle while they are held.
            tokio::time::sleep(Duration::from_millis(500)).await;
            crash.fire();
        })
    };
    let (turns, _ignored) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn_until_crash(law.admitted(), law.attempt(turns), crash)
        .await;
    let _ = fire.await;

    // Recovery. The unfinished members' child invocations outlive the
    // opener's execution: released now, each settles in its own invocation
    // or runs again under a fresh attempt, and the opener incorporates it
    // once either way.
    law.witness.hold(&[]);
    law.witness.release_held();
    let following = Arc::clone(&law.witness);
    law.on_call = Arc::new(move |index| {
        if index == 1 {
            following.set_barrier(&["after-a", "after-b", "after-c"]);
        }
    });
    let turn = law.run(&runner).await;
    assert_finished(&context, &turn);
    assert_eq!(
        law.witness.executed("echo", "settled"),
        1,
        "{context}: the recovery reuses the member that settled before the crash: {:?} {:?}",
        law.witness.executions(),
        law.witness.gate_log()
    );
    for member in ["open-a", "open-b"] {
        let runs = law
            .witness
            .executions()
            .iter()
            .filter(|execution| execution.value == member)
            .map(|execution| execution.attempt)
            .collect::<Vec<_>>();
        assert!(
            (1..=2).contains(&runs.len()),
            "{context}: `{member}` settles in its surviving invocation or runs once more: {runs:?}"
        );
    }
    let after = law.witness.gate_log();
    let first_answer = after
        .iter()
        .position(
            |event| matches!(event, GateEvent::Answered(value) if value.starts_with("after-")),
        )
        .unwrap_or(after.len());
    let started_first = after[..first_answer]
        .iter()
        .filter(|event| matches!(event, GateEvent::Started(value) if value.starts_with("after-")))
        .count();
    assert_eq!(
        started_first, 3,
        "{context}: the following batch still overlaps, so no cached reply masks \
         serialization: {after:?}"
    );
    assert_eq!(rows(record(&turn, "w")[0]).len(), 3);
    assert!(rows(record(&turn, "w")[0]).iter().all(|(_, _, ok)| *ok));
}

/// A cancel after every member started: the member whose final committed
/// keeps its row, the undecided members settle cancelled and their rows say
/// so, and settlements arriving after the cancel change no row and run
/// nothing twice.
pub async fn batch_cancel_preserves_committed_drains(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    factories: BatchSugarFactories,
) {
    let context = format!("{prefix}/batch-cancel");
    let law = SugarTurn::new(
        prefix,
        "cancel",
        &host,
        &stores,
        &factories.enabled,
        vec![response(vec![wrapper(
            "w",
            serde_json::json!([
                member("echo", serde_json::json!("committed")),
                member("gate", serde_json::json!("undecided-a")),
                member("gate", serde_json::json!("undecided-b")),
            ]),
        )])],
    );
    law.witness.hold(&["undecided-a", "undecided-b"]);
    let store = crate::conformance::law_session_store(stores.as_ref(), &law.session_id).await;
    let (turns, mut ran) = tokio::sync::mpsc::unbounded_channel();
    let running = {
        let runner = Arc::clone(&runner);
        let admitted = law.admitted();
        let attempt = law.attempt(turns);
        crate::task::spawn(async move { runner.run_turn(admitted, attempt).await })
    };
    while !(law.witness.executed("echo", "committed") == 1
        && law.witness.started("undecided-a")
        && law.witness.started("undecided-b"))
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The committed member's final lands once its attempt returns.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let driver =
        crate::TurnWorkDriver::for_session(Arc::clone(&host), law.session_id.clone(), store);
    driver
        .request_cancel(crate::TurnCancelRequest::new(
            crate::TurnAddress::new(law.session_id.clone(), law.turn_id.clone()),
            "cancel-batch",
            None,
        ))
        .await
        .unwrap_or_else(|error| panic!("{context}: request the cancel: {error}"));
    let _ = tokio::time::timeout(TURN_BUDGET, running).await;
    let mut turn = None;
    while let Ok(next) = ran.try_recv() {
        turn = next;
    }
    let turn = turn
        .unwrap_or_else(|| panic!("{context}: the cancelled turn assembles"))
        .unwrap_or_else(|error| panic!("{context}: the cancelled turn assembles: {error}"));
    // Late settlements: the undecided members may now answer.
    law.witness.release_held();
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        matches!(
            turn.outcome,
            crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled { .. })
        ),
        "{context}: the turn stops cancelled: {:?}",
        turn.outcome
    );
    assert_eq!(
        law.witness.executed("echo", "committed"),
        1,
        "{context}: nothing runs twice"
    );
    assert!(law.witness.executed("gate", "undecided-a") <= 1);
    assert!(law.witness.executed("gate", "undecided-b") <= 1);
    let wrapper = record(&turn, "w");
    {
        let wrapper = wrapper.first().unwrap_or_else(|| {
            panic!(
                "{context}: the cancelled step still answers its wrapper: {:?}",
                turn.tool_calls
            )
        });
        let rows = rows(wrapper);
        assert_eq!(
            rows,
            vec![
                (0, "echo".to_string(), true),
                (1, "gate".to_string(), false),
                (2, "gate".to_string(), false),
            ],
            "{context}: the committed row stands and the undecided rows say cancelled"
        );
        let undecided = &wrapper.output.value_for_projection()["results"][1]["error"];
        assert!(
            undecided.to_string().to_lowercase().contains("cancel"),
            "{context}: an undecided row says it was cancelled: {undecided}"
        );
    }
    assert_no_member_is_a_call(&context, &turn);
}
