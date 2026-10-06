//! The process-operations companion's public API checks across worker replacement.

use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use lash::plugins::{
    PluginDeclaration, PluginError, PluginFactory, PluginOperation, PluginQuery, PluginRegistrar,
    PluginSessionContext, ProcessEngine, ProcessEngineContributionContext,
    ProcessEngineRegistration, ProcessEngineRunContext, ProcessInfraError, ProcessRunOutcome,
    SessionParam, SessionPlugin, StateCommands,
};
use lash::process::{
    Lifetime, ObservedProcessEvent, ProcessAwaitOutput, ProcessCursor, ProcessEventPageEvents,
    ProcessEventPageMore, ProcessEventQueryMode, ProcessEventReadOutcome,
    ProcessEventSemanticsSpec, ProcessEventType, ProcessEventsFrom, ProcessInput,
    ProcessOriginator, ProcessRegistrationOutcome, ProcessSignal, ProcessSignalIdentity,
    ProcessStartReceipt, ProcessStartRequest,
};
use lash::runtime::ScopedEffectController;
use lash::{Backend, LashCore, SessionCreation, SessionSpec, TurnInput, TurnOutcome};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MODEL: &str = "process-operations-mock";
pub const SESSION_ID: &str = "process-operations-plugin-state";
const START_KEY: &str = "process-operations-replacement-start";
const STATE_KEY: &str = "replacement-value";
const REPLACEMENT_ENGINE: &str = "process-operations-replacement";
const SIGNAL_EVENT: &str = "signal.replacement";

/// The replacement process's engine: it runs until it is cancelled, receiving
/// the signals the runbook sends it across a worker replacement.
struct ReplacementEngine;

#[lash::async_trait]
impl ProcessEngine for ReplacementEngine {
    fn kind(&self) -> &'static str {
        REPLACEMENT_ENGINE
    }

    async fn run(
        &self,
        context: ProcessEngineRunContext<'_>,
        _payload: Value,
    ) -> Result<ProcessRunOutcome, ProcessInfraError> {
        context.cancellation_token().cancelled().await;
        Ok(
            ProcessAwaitOutput::from_tool_output(lash::tools::ToolCallOutput::cancelled(
                lash::tools::ToolCancellation::runtime("the replacement process was cancelled"),
            ))
            .into(),
        )
    }

    fn start_artifacts(
        &self,
        _payload: &Value,
    ) -> Result<Vec<lash::persistence::ArtifactName>, PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash::persistence::ResolvedArtifactCleanup,
    ) -> Result<(), lash::persistence::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash::persistence::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), PluginError> {
        Err(PluginError::Session(format!(
            "the replacement engine stores no artifact `{artifact_ref}`"
        )))
    }
}

#[derive(Clone, Default)]
pub struct StatePlugin {}

struct StateSnapshot;

impl PluginOperation for StateSnapshot {
    const NAME: &'static str = "process_operations.state_snapshot";
    const DESCRIPTION: &'static str = "Read the session's published replacement state.";
    const SESSION_PARAM: SessionParam = SessionParam::Required;
    type Args = Value;
    type Output = Value;
    type Error = String;
    const ERROR_TYPE: &'static str = Self::NAME;
    const ERROR_VERSION: lash::plugins::FormatVersion = lash::plugins::FormatVersion::ONE;
    fn error_class(_: &Self::Error) -> lash::plugins::PluginFailureClass {
        lash::plugins::PluginFailureClass::Terminal
    }
}

impl PluginQuery for StateSnapshot {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StateEvidence {
    pub value: Option<Value>,
    pub generation: u64,
}

impl StatePlugin {
    async fn snapshot(&self, session: &lash::LashSession) -> Result<StateEvidence> {
        let state = session
            .plugin_operations()
            .query::<StateSnapshot>(Value::Null)
            .await?;
        serde_json::from_value(state).context("decode the published plugin state")
    }
}

impl PluginFactory for StatePlugin {
    fn id(&self) -> &'static str {
        "process-operations-state"
    }

    fn process_engine_contributions(
        &self,
        _context: &ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<ProcessEngineRegistration>, PluginError> {
        Ok(vec![ProcessEngineRegistration::accepting(Arc::new(
            ReplacementEngine,
        ))])
    }

    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

impl lash::plugins::PluginDefinition for StatePlugin {
    fn declaration() -> PluginDeclaration {
        PluginDeclaration::initial("process-operations-state")
    }
}

impl SessionPlugin for StatePlugin {
    fn id(&self) -> &'static str {
        PluginFactory::id(self)
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        let state = reg.state();
        reg.operations()
            .typed_query::<StateSnapshot, _, _>(move |_, _| {
                let state = state.clone();
                async move {
                    Ok(json!({
                        "value": state.get(STATE_KEY),
                        "generation": state.generation(),
                    }))
                }
            })?;
        reg.turn().before(
            lash::hook_key!("process-operations"),
            Arc::new(move |_| {
                Box::pin(async move {
                    Ok(lash::plugins::TurnContributions {
                        state: StateCommands::new()
                            .set(STATE_KEY, json!({"value": "survives replacement"})),
                        ..Default::default()
                    })
                })
            }),
        )?;
        Ok(())
    }
}

pub fn core(backend: Backend, plugin: StatePlugin) -> Result<LashCore> {
    let provider = crate::scripted_provider::ScriptedProvider::builder()
        .kind("process-operations-mock")
        .complete(|_| async {
            Ok(lash::provider::LlmResponse {
                parts: vec![lash::direct::LlmOutputPart::Text {
                    text: "plugin state committed".into(),
                    response_meta: None,
                }],
                ..Default::default()
            })
        })
        .build()
        .into_handle();
    Ok(LashCore::standard_builder(backend)
        .commit_budget(lash::CommitBudget::bounded(4 * 1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(64))
        .llm_profiles(Arc::new(
            lash::LlmProfileRegistry::new().register(
                MODEL,
                lash::RegisteredLlmProfile::new(
                    lash::LlmProfileMetadata::builder(MODEL)
                        .context_window_tokens(200_000)
                        .build()
                        .map_err(anyhow::Error::msg)?,
                    provider,
                ),
            )?,
        ))
        .plugin(Arc::new(plugin))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "process-operations",
            format!("worker:{}", std::process::id()),
        ))?)
}

/// The replacement start, under the execution environment the host publishes
/// for it. The environment's reference is its content digest, so every worker
/// presents the same start under the key.
async fn start_request(core: &LashCore) -> Result<ProcessStartRequest> {
    // A host pin keeps the environment alive for the process (ADR 0113).
    let pin = lash::process::HostArtifactPin::mint();
    let env_ref = core
        .host_artifacts()
        .publish_process_env(
            &pin,
            &lash::process::ProcessExecutionEnvSpec::new(
                lash::plugins::AdmittedPluginConfig::default(),
                lash::runtime::SessionPolicy::new(
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(1024),
                ),
            ),
        )
        .await?;
    Ok(ProcessStartRequest::new(
        ProcessInput::Engine {
            kind: REPLACEMENT_ENGINE.into(),
            payload: json!({"runbook": "process-operations", "phase": "replacement"}),
        },
        ProcessOriginator::host(),
        Lifetime::Detached,
    )
    .with_host_start_key(START_KEY)
    .with_env_ref(env_ref)
    .with_extra_event_types([ProcessEventType {
        name: SIGNAL_EVENT.into(),
        payload_schema: lash::schema::JsonSchema::any(),
        semantics: ProcessEventSemanticsSpec::default(),
    }]))
}

async fn signal(
    core: &LashCore,
    process: &lash::ProcessId,
    id: &str,
    scoped: ScopedEffectController<'_>,
) -> Result<()> {
    core.processes()
        .signal(
            ProcessSignal::new(
                ProcessSignalIdentity::new(process.clone(), "replacement", id)?,
                marker(id),
            ),
            scoped,
        )
        .await?;
    Ok(())
}

/// The payload a signal of marker `id` carries.
fn marker(id: &str) -> Value {
    json!({"marker": id})
}

/// The runbook's signal payloads, in feed order. The engine's own lifecycle
/// events interleave with them wherever its run lands.
fn signal_markers(events: &[ObservedProcessEvent]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event.event_type == SIGNAL_EVENT)
        .map(|event| event.payload.clone())
        .collect()
}

async fn events(
    core: &LashCore,
    mut from: ProcessEventsFrom,
) -> Result<(Vec<ObservedProcessEvent>, ProcessCursor)> {
    let mut events = Vec::new();
    loop {
        let read = core
            .processes()
            .events(from, NonZeroUsize::MIN, ProcessEventQueryMode::Full)
            .await?;
        let ProcessEventReadOutcome::Retained(page) = read.outcome else {
            anyhow::bail!(
                "replacement process history is not retained: {:?}",
                read.outcome
            );
        };
        let ProcessEventPageEvents::Full(page_events) = page.events else {
            anyhow::bail!("full process read returned a lite page");
        };
        events.extend(page_events);
        let cursor = read.cursor.context("retained history has no cursor")?;
        match page.more {
            ProcessEventPageMore::Complete => return Ok((events, cursor)),
            ProcessEventPageMore::More { .. } => from = ProcessEventsFrom::After(cursor),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplacementBaseline {
    pub first: ProcessStartReceipt,
    pub repeated: ProcessStartReceipt,
    pub cursor: ProcessCursor,
    pub events: Vec<ObservedProcessEvent>,
    pub state: StateEvidence,
}

pub async fn prepare(
    core: &LashCore,
    plugin: &StatePlugin,
    scoped: ScopedEffectController<'_>,
) -> Result<ReplacementBaseline> {
    let first = core
        .processes()
        .start(start_request(core).await?, scoped.clone())
        .await?;
    let repeated = core
        .processes()
        .start(start_request(core).await?, scoped.clone())
        .await?;
    ensure!(first.disposition == ProcessRegistrationOutcome::Created);
    ensure!(first.start_key.is_some());
    ensure!(
        repeated
            == ProcessStartReceipt {
                disposition: ProcessRegistrationOutcome::Existing,
                ..first.clone()
            },
        "the same start key did not return the first process handle"
    );
    for id in ["before-1", "before-2"] {
        signal(core, &first.process_id, id, scoped.clone()).await?;
    }
    let (events, cursor) = events(core, ProcessEventsFrom::Start(first.process_id.clone())).await?;
    ensure!(
        signal_markers(&events) == ["before-1", "before-2"].map(marker)
            && events.last().map(|last| last.sequence) == Some(cursor.sequence()),
        "expected the two pre-replacement signals, in order, and the cursor at the feed's end"
    );
    ensure!(
        events[0].event_type == "process.external_ref_set"
            && events[0].payload["external_ref"]["backend"] == "restate"
            && events[0].payload["external_ref"]["segment_ordinal"] == 0,
        "start did not record its Restate reference"
    );
    core.session(lash::SessionId::parse(SESSION_ID)?)
        .create(SessionCreation::root(SessionSpec::new(
            MODEL,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(8),
        )))
        .await?;
    let session = core
        .session(lash::SessionId::parse(SESSION_ID)?)
        .open()
        .await?;
    let output = session
        .send(TurnInput::text("write plugin state"))
        .id(lash::TurnId::parse("replacement-write")?)
        .output()
        .await?;
    ensure!(matches!(output.result.outcome, TurnOutcome::Finished(_)));
    let state = plugin.snapshot(&session).await?;
    ensure!(state.value == Some(json!({"value": "survives replacement"})) && state.generation > 0);
    let baseline = ReplacementBaseline {
        first,
        repeated,
        cursor,
        events,
        state,
    };
    println!(
        "{}",
        json!({"checkpoint": "replacement_prepared", "evidence": baseline})
    );
    Ok(baseline)
}

pub async fn recover(
    core: &LashCore,
    plugin: &StatePlugin,
    before: &ReplacementBaseline,
    scoped: ScopedEffectController<'_>,
) -> Result<()> {
    let repeated = core
        .processes()
        .start(start_request(core).await?, scoped.clone())
        .await?;
    ensure!(
        repeated == before.repeated,
        "replacement changed the start-key receipt"
    );
    let distinct_process_ids = [&before.first, &before.repeated, &repeated]
        .map(|receipt| &receipt.process_id)
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    println!(
        "{}",
        json!({
            "checkpoint": "start_key_reused_after_replacement",
            "first": before.first, "second": before.repeated, "after": repeated,
            "distinct_process_ids": distinct_process_ids,
        })
    );

    for id in ["after-1", "after-2"] {
        signal(core, &repeated.process_id, id, scoped.clone()).await?;
    }
    let saved_cursor: ProcessCursor =
        serde_json::from_str(&serde_json::to_string(&before.cursor)?)?;
    let (tail, cursor) = events(core, ProcessEventsFrom::After(saved_cursor)).await?;
    let after_count = signal_markers(&tail).len();
    ensure!(
        after_count == 2,
        "expected exactly two post-replacement signals"
    );
    let mut all = before.events.clone();
    all.extend(tail);
    ensure!(
        all.iter()
            .map(|event| event.sequence)
            .eq(1..=cursor.sequence()),
        "event feed has a gap or duplicate"
    );
    ensure!(
        signal_markers(&all) == ["before-1", "before-2", "after-1", "after-2"].map(marker),
        "event feed changed or reordered its payloads"
    );
    let (empty, end) = events(core, ProcessEventsFrom::After(cursor.clone())).await?;
    ensure!(
        empty.is_empty() && end.sequence() == cursor.sequence(),
        "cursor did not stay at the feed end"
    );
    println!(
        "{}",
        json!({
            "checkpoint": "process_feed_resumed_after_replacement",
            "process_id": repeated.process_id, "saved_cursor": before.cursor,
            "resumed_cursor": cursor, "events": all, "before_count": before.events.len(),
            "after_count": after_count, "total_count": all.len(), "end_count": empty.len(),
        })
    );

    let session = core
        .session(lash::SessionId::parse(SESSION_ID)?)
        .open()
        .await?;
    let restored = plugin.snapshot(&session).await?;
    ensure!(
        restored == before.state,
        "replacement lost plugin value or generation"
    );
    let output = session
        .send(TurnInput::text("advance plugin generation"))
        .id(lash::TurnId::parse("replacement-next-write")?)
        .output()
        .await?;
    ensure!(matches!(output.result.outcome, TurnOutcome::Finished(_)));
    let next = plugin.snapshot(&session).await?;
    ensure!(
        next.value == before.state.value && next.generation > restored.generation,
        "the next accepted plugin write did not advance its generation: restored={restored:?}, next={next:?}"
    );
    println!(
        "{}",
        json!({
            "checkpoint": "plugin_state_survived_replacement", "session_id": SESSION_ID,
            "plugin_id": PluginFactory::id(plugin), "before": before.state,
            "restored": restored, "next": next,
        })
    );
    Ok(())
}
