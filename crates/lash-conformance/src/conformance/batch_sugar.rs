//! The `batch` sugar laws (ADR 0116 §7.2).
//!
//! `batch` is protocol sugar, not a tool: the standard driver expands each
//! wrapper into the Run's round beside the response's native calls
//! and folds the members' results back into one batch result. These laws execute
//! real turns on a tier, through its [`crate::ConformanceTurnRunner`], and pin
//! what that means where a tier can get it wrong: member admission and
//! identity, source-order folding, preparation refusal, and the transcript
//! the model and host see. Run laws own replay, cancellation and protected drain.
//!
//! The registering crate hands in the standard protocol twice — with `batch`
//! offered at its default maximum, and with it withheld — because this crate
//! does not construct protocols.

use crate::ActorContext;
use crate::admit;
use lash_core::testing::TestTurnExecution as _;
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
    call_id: crate::ToolCallId,
}

/// What the law's tools share with the law.
#[derive(Default)]
struct Witness {
    executions: Mutex<Vec<Execution>>,
    /// Every tool name a before-tool hook saw.
    hooked: Mutex<Vec<String>>,
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
}

fn value_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { "value": { "type": "string" } },
        "required": ["value"],
        "additionalProperties": false
    })
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn sugar_tool(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "A batch sugar law tool: records each execution and answers its value.",
        value_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
}

const TOOLS: [&str; 2] = ["echo", "guarded"];

/// `echo` answers at once and `guarded` is denied by the before-tool check.
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
            call_id: call.context.call_id().clone(),
        });
        crate::ToolOutcome::ok(serde_json::json!({ "tool": call.name(), "value": value })).into()
    }
}

/// The law's tools and its before-check: the check records every tool name
/// it is asked about and denies `guarded`.
fn sugar_plugin(witness: Arc<Witness>) -> Arc<dyn crate::facade_support::PluginFactory> {
    let hooked = Arc::clone(&witness);
    let spec = crate::facade_support::PluginSpec::new()
        .with_tool_provider(Arc::new(SugarTools { witness }))
        .with_tool_args_check(
            crate::hook_key!("guard"),
            Arc::new(move |input| {
                let tool_name = input.prepared.tool_name().to_string();
                hooked.hooked.lock_recover().push(tool_name.clone());
                Box::pin(async move {
                    Ok(if tool_name == "guarded" {
                        crate::facade_support::BeforeToolDecision::Deny(crate::ToolFailure::tool(
                            crate::ToolFailureClass::PermissionDenied,
                            "approval_denied",
                            "the law denies `guarded`",
                        ))
                    } else {
                        crate::facade_support::BeforeToolDecision::Allow
                    })
                })
            }),
        );
    Arc::new(crate::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("conformance-batch-sugar"),
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
            std::future::ready(Ok(next.unwrap_or_else(|| text("sugar laws complete"))))
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

/// Everything one law turn is built from. Each execution of the turn builds
/// its runtime afresh from these, which outlive it.
#[derive(Clone)]
struct SugarTurn {
    stores: Arc<dyn crate::StoreSet>,
    session_id: SessionId,
    turn_id: TurnId,
    factories: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    witness: Arc<Witness>,
    script: Vec<crate::LlmResponse>,
    on_call: Arc<dyn Fn(usize) + Send + Sync>,
}

impl SugarTurn {
    fn new(
        prefix: &str,
        name: &str,
        _host: &ActorContext,
        stores: &Arc<dyn crate::StoreSet>,
        factories: &[Arc<dyn crate::facade_support::PluginFactory>],
        script: Vec<crate::LlmResponse>,
    ) -> Self {
        let session_id = SessionId::fixture(format!("{prefix}-batch-sugar-{name}"));
        let witness = Arc::new(Witness::default());
        Self {
            stores: Arc::clone(stores),
            turn_id: TurnId::fixture(format!("{session_id}-turn")),
            session_id,
            factories: factories.to_vec(),
            witness,
            script,
            on_call: Arc::new(|_| {}),
        }
    }

    fn admitted(&self) -> crate::AdmittedScope {
        admit(crate::ExecutionScope::turn(&self.session_id, &self.turn_id))
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn shift(
        &self,
        scope: crate::ActorContext,
    ) -> Option<Result<crate::AssembledTurn, crate::RuntimeError>> {
        let mut config = crate::LawBackend::over_stores(Arc::clone(&self.stores)).host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
        config.providers.models = crate::testing::standard_test_llm_profiles(
            scripted_model(self.script.clone(), Arc::clone(&self.on_call)).into_handle(),
        );
        let policy = crate::testing::mock_session_policy();
        let state = crate::RuntimeSessionState {
            session_id: self.session_id.clone(),
            policy: policy.clone(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::bounded(8),
                crate::MaxToolCalls::new(1024),
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
                .build(),
        )
        .await
        .expect("build the batch sugar conformance runtime");
        let options = crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope);
        let mut input = crate::TurnInput::text("run the batch sugar law");
        input.trace_turn_id = Some(self.turn_id.clone());
        tokio::time::timeout(TURN_BUDGET, runtime.execute_turn(input, options))
            .await
            .ok()
    }

    /// The attempt a runner executes: every execution builds its runtime afresh
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
                let Some(assembled) = turn.shift(scope).await else {
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
                     member executions: {:?}",
                    self.session_id,
                    self.witness.executions(),
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
        .filter(|record| record.provider_call_id.as_deref() == Some(call_id))
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

/// Every tool result the transcript holds, as `(provider call id, tool,
/// content)`: a result answers the provider correlation of the call it pairs
/// with by `ToolCallId`, and is empty when it pairs with none.
fn transcript_results(turn: &crate::AssembledTurn) -> Vec<(String, String, String)> {
    let view = turn.state.read_view();
    let parts = view
        .messages()
        .iter()
        .flat_map(|message| message.parts.iter())
        .collect::<Vec<_>>();
    let provider = |call_id: Option<&crate::ToolCallId>| {
        parts
            .iter()
            .find(|part| part.kind() == crate::PartKind::ToolCall && part.call_id() == call_id)
            .and_then(|part| part.provider_call_id())
            .unwrap_or_default()
            .to_string()
    };
    parts
        .iter()
        .filter(|part| part.kind() == crate::PartKind::ToolResult)
        .map(|part| {
            (
                provider(part.call_id()),
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
                part.provider_call_id().unwrap_or_default().to_string(),
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

/// A member is named under its wrapper and carries no provider correlation:
/// every host record, transcript call and transcript result is a call the
/// model issued.
fn assert_no_member_is_a_call(context: &str, turn: &crate::AssembledTurn) {
    let members = records(turn)
        .into_iter()
        .filter(|record| record.provider_call_id.is_none())
        .map(|record| format!("record {}", record.call_id))
        .chain(
            transcript_results(turn)
                .into_iter()
                .filter(|(call_id, _, _)| call_id.is_empty())
                .map(|(_, tool, _)| format!("result of {tool}")),
        )
        .chain(
            transcript_calls(turn)
                .into_iter()
                .filter(|(call_id, _, _)| call_id.is_empty())
                .map(|(_, tool, _)| format!("call of {tool}")),
        )
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
    host: ActorContext,
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
        calls[0].provider_call_id.clone()
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

/// L05/L21: singleton rounds and expanded batches preserve their logical
/// calls and source slots while executing in the opener's Run, without a
/// ToolInvocation child group.
pub async fn standard_rounds_and_batches_use_the_run(
    prefix: &str,
    host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    factories: BatchSugarFactories,
) {
    let law = SugarTurn::new(
        prefix,
        "run-route",
        &host,
        &stores,
        &factories.enabled,
        vec![
            response(vec![native("scalar", "echo", "scalar")]),
            response(vec![
                native("native", "echo", "native"),
                wrapper(
                    "batch",
                    serde_json::json!([
                        member("echo", serde_json::json!("first")),
                        member("echo", serde_json::json!(5)),
                        member("guarded", serde_json::json!("denied")),
                        member("echo", serde_json::json!("twin")),
                        member("echo", serde_json::json!("twin")),
                    ]),
                ),
            ]),
        ],
    );
    let turn = law.run(&runner).await;
    let context = format!("{prefix}/standard-run-route");
    assert_finished(&context, &turn);
    for value in ["scalar", "native", "first"] {
        assert_eq!(law.witness.executed("echo", value), 1, "{context}: {value}");
    }
    assert_eq!(law.witness.executed("echo", "twin"), 2);
    assert_eq!(law.witness.executed("guarded", "denied"), 0);
    let executions = law.witness.executions();
    let identities = executions
        .iter()
        .map(|execution| &execution.call_id)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        identities.len(),
        5,
        "{context}: equal operands remain distinct calls"
    );
    let batch = record(&turn, "batch");
    assert_eq!(batch.len(), 1);
    for (value, members) in [("first", vec![0]), ("twin", vec![3, 4])] {
        let actual = executions
            .iter()
            .filter(|execution| execution.value == value)
            .map(|execution| execution.call_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let expected = members
            .into_iter()
            .map(|member| batch[0].call_id.child(member))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            actual, expected,
            "{context}: leaf identity uses its original member index"
        );
    }
    assert_eq!(
        rows(batch[0]),
        vec![
            (0, "echo".to_string(), true),
            (1, "echo".to_string(), false),
            (2, "guarded".to_string(), false),
            (3, "echo".to_string(), true),
            (4, "echo".to_string(), true),
        ],
        "{context}: preparation failures retain their original source slots"
    );
    assert_no_member_is_a_call(&context, &turn);
}

/// The transcript and the host's records show one call and one result per
/// wrapper, under the provider's call id and replay metadata, and no member
/// call.
pub async fn batch_folds_to_one_transcript_call(
    prefix: &str,
    host: ActorContext,
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
        .map(|record| (record.provider_call_id.unwrap_or_default(), record.tool))
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
