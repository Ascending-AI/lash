//! A real-host E2E case's scripted fixture: the provider's answers and the
//! tool bodies the case names, read from the file in `E2E_CONSUMER_FIXTURE`.
//!
//! The case owns the fixture's ledgers, so they survive a killed node and a
//! node that resumes its work. A body appends its delivery (tool, call id,
//! attempt and owning run) and syncs it before anything else, so the case
//! counts every body entry, not only the ones whose outcome committed. A
//! held body then waits on the case's control endpoint, which answers once
//! the case releases it. The provider records every request it answers.
use std::io::{Read as _, Seek as _, Write as _};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, ensure};
use lash::direct::LlmOutputPart;
use lash::plugins::{
    PluginDeclaration, PluginError, PluginFactory, PluginRegistrar, PluginSessionContext,
    PluginStateView, SessionPlugin, StateCommands, StateReduction,
};
use lash::provider::{LlmContentBlock, LlmRequest, LlmResponse, LlmRole, ProviderHandle};
use lash::sync::MutexExt as _;
use lash::tools::{
    CancelHint, EmitProcessEventIntent, ExecutionPolicy, PendingCompletion, StaticToolExecute,
    StaticToolProvider, ToolAttemptOutcome, ToolCall, ToolDeclaration, ToolDefinition, ToolFailure,
    ToolFailureClass, ToolIntent, ToolIntents, ToolOutcome, ToolOutcomeDone,
};

use crate::receiver;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    /// The case's tag; every input of the case carries it.
    pub tag: String,
    pub tools: Vec<ToolPlan>,
    /// The provider's answer to each model call of a turn, by position.
    pub steps: Vec<Step>,
    /// Named scripts: a turn whose input carries `@<name>` is answered by
    /// that script instead of `steps`.
    #[serde(default)]
    pub scripts: std::collections::BTreeMap<String, Vec<Step>>,
    pub body_ledger: PathBuf,
    pub provider_ledger: PathBuf,
    /// The case's control endpoint: held bodies and keyed effects.
    pub control_url: String,
    /// Where the case's reducers record each reduction they run.
    pub reducer_ledger: PathBuf,
    /// The file naming the case's intent receiver process, once started.
    pub receiver: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPlan {
    pub name: String,
    pub value: Value,
    #[serde(default)]
    pub policy: ExecutionPolicy,
    /// The body waits for the case to release it.
    #[serde(default)]
    pub hold: bool,
    /// Attempts up to this number fail with a typed, retryable failure.
    #[serde(default)]
    pub fail_attempts: u32,
    /// The body defers: its outcome arrives through its completion key.
    #[serde(default)]
    pub deferred: bool,
    /// The body writes a keyed mutation to the case's effect endpoint.
    #[serde(default)]
    pub effect: bool,
    /// The plugin that owns the tool, and so the namespace its state
    /// commands change: `one` or `two`.
    #[serde(default = "one")]
    pub plugin: String,
    /// The state commands the body returns with its result.
    #[serde(default)]
    pub state: Vec<StatePlan>,
    /// The body's result is its plugin's published namespace.
    #[serde(default)]
    pub reads: bool,
    /// The body declares one event for the case's receiver process.
    #[serde(default)]
    pub emit: bool,
}

fn one() -> String {
    "one".to_owned()
}

/// One state command a body returns.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum StatePlan {
    /// Set `key` to `value`.
    Set { key: String, value: Value },
    /// Append `input` to `key` through the plugin's `append` reducer.
    Append { key: String, input: Value },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum Step {
    /// One tool call per named tool, in order.
    Calls(Vec<String>),
    /// One standard `batch` call over the named tools.
    Batch(Vec<String>),
    /// The final answer.
    Text(String),
}

/// One body entry, as the ledger records it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Delivery {
    pub tool: String,
    pub call_id: String,
    pub attempt: u32,
    pub run: Option<String>,
    pub owner: Value,
    pub completion: Option<String>,
    /// When the body entered, in Unix milliseconds.
    pub at_ms: u128,
}

impl Fixture {
    pub fn from_env(variable: &str) -> Result<Option<Self>> {
        let Some(path) = std::env::var_os(variable) else {
            return Ok(None);
        };
        let bytes = std::fs::read(&path).with_context(|| format!("read {variable}"))?;
        let fixture: Self = serde_json::from_slice(&bytes).context("decode the case fixture")?;
        ensure!(
            !fixture.steps.is_empty(),
            "a fixture needs at least one provider step"
        );
        let url = reqwest::Url::parse(&fixture.control_url)?;
        ensure!(
            url.scheme() == "http" && url.host_str() == Some("127.0.0.1"),
            "the case control endpoint must be a loopback HTTP endpoint"
        );
        Ok(Some(fixture))
    }

    /// The case's two plugins. The fixture's tools are theirs, so a body's
    /// state commands change its plugin's namespace, and each registers the
    /// `append` reducer.
    pub fn plugins(&self) -> Result<Vec<Arc<dyn PluginFactory>>> {
        let open = |path: &PathBuf| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("open {}", path.display()))
        };
        let shared = Arc::new(Shared {
            fixture: self.clone(),
            ledger: Mutex::new(open(&self.body_ledger)?),
            reductions: Mutex::new(open(&self.reducer_ledger)?),
            client: reqwest::Client::builder().no_proxy().build()?,
        });
        Ok(vec![
            Arc::new(CasePlugin::<One>(shared.clone(), std::marker::PhantomData)),
            Arc::new(CasePlugin::<Two>(shared, std::marker::PhantomData)),
        ])
    }

    pub fn provider(&self) -> Result<ProviderHandle> {
        let ledger = Arc::new(Mutex::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(&self.provider_ledger)
                .with_context(|| format!("open {}", self.provider_ledger.display()))?,
        ));
        let fixture = Arc::new(self.clone());
        Ok(lash::testing::TestProvider::builder()
            .kind("e2e-case")
            .complete(move |request| {
                let fixture = fixture.clone();
                let ledger = ledger.clone();
                async move {
                    answer(&fixture, &request, &ledger).map_err(|error| {
                        lash::provider::LlmTransportError::new(format!(
                            "e2e case provider: {error:#}"
                        ))
                    })
                }
            })
            .build()
            .into_handle())
    }
}

/// The answer to one model call: the step after the turn's last genuine user
/// input whose position is the number of model answers already in the turn.
fn answer(
    fixture: &Fixture,
    request: &LlmRequest,
    ledger: &Mutex<std::fs::File>,
) -> Result<LlmResponse> {
    let start = request
        .messages
        .iter()
        .rposition(|message| message.starts_user_segment)
        .ok_or_else(|| anyhow!("request has no genuine user input"))?;
    let input: String = request.messages[start]
        .blocks
        .iter()
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect();
    ensure!(
        input.contains(&fixture.tag),
        "request carries another case's input"
    );
    let step = request.messages[start..]
        .iter()
        .filter(|message| message.role == LlmRole::Assistant)
        .count();
    let mut results = serde_json::Map::new();
    for block in request.messages[start..]
        .iter()
        .flat_map(|message| message.blocks.iter())
    {
        if let LlmContentBlock::ToolResult {
            call_id, content, ..
        } = block
        {
            results.insert(
                call_id.as_str().to_owned(),
                Value::String(
                    content
                        .iter()
                        .map(|part| match part {
                            lash::messages::ModelToolReturnPart::Text { text } => text.as_str(),
                            _ => "<non-text>",
                        })
                        .collect(),
                ),
            );
        }
    }
    let script = fixture
        .scripts
        .iter()
        .find(|(name, _)| input.contains(&format!("@{name}")))
        .map_or(&fixture.steps, |(_, script)| script);
    let plan = script
        .get(step)
        .ok_or_else(|| anyhow!("no provider step {step}"))?;
    let parts = match plan {
        Step::Calls(tools) => tools
            .iter()
            .map(|tool| LlmOutputPart::ToolCall {
                call_id: format!("{tool}-{step}"),
                tool_name: tool.clone(),
                input_json: "{}".to_owned(),
                replay: None,
            })
            .collect(),
        Step::Batch(tools) => vec![LlmOutputPart::ToolCall {
            call_id: format!("batch-{step}"),
            tool_name: "batch".to_owned(),
            input_json: json!({"tool_calls": tools.iter().map(|tool| json!({"tool": tool, "parameters": {}})).collect::<Vec<_>>()}).to_string(),
            replay: None,
        }],
        Step::Text(text) => vec![LlmOutputPart::Text {
            text: text.clone(),
            response_meta: None,
        }],
    };
    let mut file = ledger.lock_recover();
    file.rewind()?;
    let mut previous = String::new();
    file.read_to_string(&mut previous)?;
    let mut line =
        json!({"input": input, "step": step, "results": results, "call": previous.lines().count()})
            .to_string()
            .into_bytes();
    line.push(b'\n');
    file.write_all(&line)?;
    file.sync_data()?;
    let terminal_reason = if matches!(plan, Step::Text(_)) {
        lash::direct::LlmTerminalReason::Stop
    } else {
        lash::direct::LlmTerminalReason::ToolUse
    };
    Ok(LlmResponse {
        parts,
        terminal_reason,
        ..LlmResponse::default()
    })
}

/// What every case plugin and body shares.
struct Shared {
    fixture: Fixture,
    ledger: Mutex<std::fs::File>,
    reductions: Mutex<std::fs::File>,
    client: reqwest::Client,
}

/// A case plugin's identity.
trait CaseId: Send + Sync + 'static {
    const ID: &'static str;
    const PLAN: &'static str;
}

struct One;
impl CaseId for One {
    const ID: &'static str = "e2e-case";
    const PLAN: &'static str = "one";
}

struct Two;
impl CaseId for Two {
    const ID: &'static str = "e2e-case-two";
    const PLAN: &'static str = "two";
}

struct CasePlugin<P: CaseId>(Arc<Shared>, std::marker::PhantomData<P>);

impl<P: CaseId> PluginFactory for CasePlugin<P> {
    fn id(&self) -> &'static str {
        P::ID
    }

    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(Self(self.0.clone(), std::marker::PhantomData)))
    }
}

impl<P: CaseId> lash::plugins::PluginDefinition for CasePlugin<P> {
    fn declaration() -> PluginDeclaration {
        PluginDeclaration::initial(P::ID)
    }
}

impl<P: CaseId> SessionPlugin for CasePlugin<P> {
    fn id(&self) -> &'static str {
        P::ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        let shared = self.0.clone();
        reg.state_reducer(
            "append",
            Arc::new(move |reduction: StateReduction<'_>| {
                let mut list = match reduction.current {
                    Some(Value::Array(items)) => items.clone(),
                    _ => Vec::new(),
                };
                list.push(reduction.input.clone());
                let mut line =
                    json!({"plugin": P::ID, "key": reduction.key, "input": reduction.input})
                        .to_string()
                        .into_bytes();
                line.push(b'\n');
                let mut file = shared.reductions.lock_recover();
                let _ = file.write_all(&line).and_then(|()| file.sync_data());
                Ok(Some(Value::Array(list)))
            }),
        )?;
        let plans: Vec<ToolPlan> = self
            .0
            .fixture
            .tools
            .iter()
            .filter(|plan| plan.plugin == P::PLAN)
            .cloned()
            .collect();
        if plans.is_empty() {
            return Ok(());
        }
        let definitions = plans
            .iter()
            .map(|plan| {
                let declaration = if plan.deferred {
                    ToolDeclaration::deferring()
                } else if plan.emit {
                    ToolDeclaration::default()
                        .with_intents([lash::tools::ToolIntentKind::EmitProcessEvent])
                } else {
                    ToolDeclaration::default()
                };
                Ok(ToolDefinition::raw(
                    format!("tool:e2e.{}", plan.name),
                    &plan.name,
                    "A body the real-host case controls.",
                    json!({"type":"object","properties":{},"additionalProperties":false}),
                    json!({}),
                )
                .map_err(|error| PluginError::Registration(error.to_string()))?
                .with_execution_policy(plan.policy)
                .with_declaration(declaration))
            })
            .collect::<Result<Vec<_>, PluginError>>()?;
        let view = reg.state();
        reg.tools().provider(Arc::new(StaticToolProvider::new(
            definitions,
            Bodies {
                plans,
                shared: self.0.clone(),
                view,
            },
        )))?;
        Ok(())
    }
}

struct Bodies {
    plans: Vec<ToolPlan>,
    shared: Arc<Shared>,
    view: PluginStateView,
}

impl Bodies {
    async fn attempt(&self, call: ToolCall<'_>) -> Result<ToolAttemptOutcome> {
        let plan = self
            .plans
            .iter()
            .find(|plan| plan.name == call.name())
            .ok_or_else(|| anyhow!("unplanned body {}", call.name()))?;
        let completion = plan
            .deferred
            .then(|| call.context.completion_key())
            .transpose()?
            .map(|key| key.as_str().to_owned());
        let delivery = Delivery {
            tool: plan.name.clone(),
            call_id: call.context.call_id().to_string(),
            attempt: call.context.attempt_number(),
            run: call.context.logical_run().map(|run| run.to_string()),
            owner: serde_json::to_value(call.context.owner())?,
            completion,
            at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis(),
        };
        let mut line = serde_json::to_vec(&delivery)?;
        line.push(b'\n');
        {
            let mut file = self.shared.ledger.lock_recover();
            file.write_all(&line)?;
            file.sync_data()?;
        }
        if plan.effect {
            self.post("effect", &delivery).await?;
        }
        if plan.hold {
            self.post("hold", &delivery).await?;
        }
        if delivery.attempt <= plan.fail_attempts {
            return Ok(ToolOutcome::failure(ToolFailure::tool(
                ToolFailureClass::Unavailable,
                "e2e.retryable",
                format!(
                    "{} attempt {} fails as the case planned",
                    plan.name, delivery.attempt
                ),
            ))
            .into());
        }
        if delivery.completion.is_some() {
            let mut pending = PendingCompletion::new();
            pending.on_cancel = CancelHint::Ignore;
            return Ok(ToolAttemptOutcome::pending(pending));
        }
        let value = if plan.reads {
            let namespace: serde_json::Map<String, Value> = self
                .view
                .keys()
                .into_iter()
                .filter_map(|key| self.view.get(&key).map(|value| (key, value)))
                .collect();
            json!({"generation": self.view.generation(), "values": namespace})
        } else {
            plan.value.clone()
        };
        let mut commands = StateCommands::new();
        for command in &plan.state {
            commands = match command {
                StatePlan::Set { key, value } => commands.set(key, value.clone()),
                StatePlan::Append { key, input } => commands.apply(key, "append", input.clone()),
            };
        }
        let intents = if plan.emit {
            let process = receiver::bound(&self.shared.fixture.receiver)?;
            ToolIntents::v3(vec![ToolIntent::EmitProcessEvent(EmitProcessEventIntent {
                owner: call.context.owner().runtime_owner(),
                process_id: process,
                event_type: receiver::EVENT.to_owned(),
                payload: json!({"call_id": delivery.call_id, "value": value}),
            })])
        } else {
            ToolIntents::default()
        };
        Ok(ToolAttemptOutcome::done(
            ToolOutcomeDone::ok(value).with_state(commands),
            intents,
        ))
    }

    async fn post(&self, route: &str, delivery: &Delivery) -> Result<()> {
        self.shared
            .client
            .post(format!("{}/{route}", self.shared.fixture.control_url))
            .json(delivery)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

#[lash::async_trait]
impl StaticToolExecute for Bodies {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        match self.attempt(call).await {
            Ok(outcome) => outcome,
            Err(error) => {
                ToolOutcome::err_fmt(format_args!("e2e case body failed: {error:#}")).into()
            }
        }
    }
}
