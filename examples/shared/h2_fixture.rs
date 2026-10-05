//! H2 scripted provider and real tool bodies, composed by the product host.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, anyhow, ensure};
use serde::Deserialize;

#[path = "h2_tool_bodies.rs"]
mod bodies;
use bodies::{BodyResult, ToolBodies, ToolDelivery};
#[path = "h2_provider.rs"]
pub(crate) mod provider;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureConfig {
    scenario: String,
    delivery_ledger: PathBuf,
    provider_ledger: PathBuf,
    body_callback_url: String,
    #[serde(default)]
    deferred_loser: bool,
    #[serde(default)]
    receiver_hold: bool,
    #[serde(default)]
    intent_process: Option<lash::ProcessId>,
    #[serde(default = "event_type")]
    intent_event_type: String,
    /// The PID marker every isolated worker appends to (S19/S20).
    #[serde(default)]
    worker_marker: Option<PathBuf>,
}

/// The process engine kind an isolated fixture tool binds to.
const WORKER_ENGINE: &str = "e2e-h2-worker";

fn event_type() -> String {
    "h2_mutation".to_owned()
}

pub(crate) struct Fixture {
    config: FixtureConfig,
    receiver: Arc<OnceLock<lash::ProcessId>>,
    /// One engine per deployment; its marker's durable ownership directory
    /// lets a cold replacement adopt the same worker or termination receipt.
    worker: Option<Arc<dyn lash::plugins::ProcessEngine>>,
}

impl Fixture {
    pub(crate) fn from_env(variable: &str) -> Result<Option<Self>> {
        let Some(path) = std::env::var_os(variable) else {
            return Ok(None);
        };
        let bytes = std::fs::read(Path::new(&path)).with_context(|| format!("read {variable}"))?;
        let config: FixtureConfig = serde_json::from_slice(&bytes)?;
        ensure!(
            matches!(
                config.scenario.as_str(),
                "S01"
                    | "S02"
                    | "S05"
                    | "S08"
                    | "S09"
                    | "S10"
                    | "S11"
                    | "S12"
                    | "S23"
                    | "S31"
                    | "S32"
                    | "S19"
                    | "S20"
            ),
            "unknown H2 fixture scenario"
        );
        let worker = match (config.scenario.as_str(), &config.worker_marker) {
            ("S19" | "S20", Some(marker)) => {
                Some(Arc::new(lash::plugins::WorkerProcessEngine::new(
                    WORKER_ENGINE,
                    lash::plugins::WorkerCommand {
                        program: "/bin/sh".into(),
                        args: vec![
                            "-c".into(),
                            "echo $$ >> \"$1\"; exec sleep 600".into(),
                            "sh".into(),
                            marker.clone().into_os_string(),
                        ],
                    },
                    marker.with_extension("ownership"),
                )))
            }
            ("S19" | "S20", None) => {
                return Err(anyhow!("an isolated scenario needs a worker marker"));
            }
            _ => None,
        };
        let worker: Option<Arc<dyn lash::plugins::ProcessEngine>> = worker.map(|engine| {
            if config.scenario == "S20" {
                Arc::new(TerminationGate {
                    engine,
                    marker: config
                        .worker_marker
                        .clone()
                        .expect("validated worker marker"),
                }) as Arc<dyn lash::plugins::ProcessEngine>
            } else {
                engine as Arc<dyn lash::plugins::ProcessEngine>
            }
        });
        let url = reqwest::Url::parse(&config.body_callback_url)?;
        ensure!(
            url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")),
            "body controls must be a case-owned loopback HTTP endpoint"
        );
        let receiver = Arc::new(OnceLock::new());
        let retained_path = config.delivery_ledger.with_extension("receiver.json");
        let retained = match &config.intent_process {
            Some(id) => Some(id.clone()),
            None if retained_path.exists() => {
                Some(serde_json::from_slice(&std::fs::read(retained_path)?)?)
            }
            None => None,
        };
        if let Some(id) = retained {
            receiver
                .set(id)
                .map_err(|_| anyhow!("receiver already bound"))?;
        }
        Ok(Some(Self {
            config,
            receiver,
            worker,
        }))
    }

    /// The plugin contributing the scenario's worker engine, when it isolates.
    pub(crate) fn worker_engine(&self) -> Option<Arc<dyn lash::plugins::PluginFactory>> {
        self.worker.clone().map(|engine| {
            Arc::new(WorkerEnginePlugin(engine)) as Arc<dyn lash::plugins::PluginFactory>
        })
    }

    fn labels(&self) -> &'static [&'static str] {
        match self.config.scenario.as_str() {
            "S01" => &["echo"],
            "S02" => &["a", "b"],
            "S05" => &["a", "b", "c"],
            "S08" | "S09" => &["intent"],
            "S10" => &["rank_one", "rank_two", "rank_three"],
            "S11" => &["winner", "loser", "after"],
            "S12" => &["gate", "source"],
            "S23" => &["winner", "source", "gate", "later"],
            "S32" => &["winner", "source", "gate"],
            "S31" => &["winner", "loser", "gate"],
            "S19" | "S20" => &["isolated", "unbound"],
            _ => &[],
        }
    }

    pub(crate) fn tools(&self) -> Result<Arc<dyn lash::tools::ToolProvider>> {
        let mut plan = BTreeMap::new();
        for label in self.labels() {
            let value = serde_json::json!(match *label {
                "a" => "A",
                "b" => "B",
                "c" => "C",
                value => value,
            });
            let result = if self.worker.is_some() {
                BodyResult::Isolated
            } else if (*label == "loser" && self.config.deferred_loser)
                || matches!(*label, "source" | "later")
            {
                BodyResult::Deferred
            } else if matches!(*label, "intent" | "rank_one" | "rank_three") {
                BodyResult::EmitToReceiver {
                    value,
                    receiver: self.receiver.clone(),
                    event_type: self.config.intent_event_type.clone(),
                }
            } else {
                BodyResult::Inline {
                    value,
                    intents: Default::default(),
                }
            };
            plan.insert((*label).to_owned(), result);
        }
        let client = reqwest::Client::new();
        let url = self.config.body_callback_url.clone();
        let barrier = Arc::new(move |delivery: ToolDelivery| -> bodies::BodyStep {
            let client = client.clone();
            let url = url.clone();
            Box::pin(async move {
                client
                    .post(url)
                    .json(&delivery)
                    .send()
                    .await?
                    .error_for_status()?;
                Ok(())
            })
        });
        let provider = ToolBodies::open(&self.config.delivery_ledger, plan, barrier)?.provider()?;
        Ok(if self.worker.is_some() {
            Arc::new(IsolatedBinding(provider))
        } else {
            provider
        })
    }

    pub(crate) fn receiver_binding(&self) -> (Arc<OnceLock<lash::ProcessId>>, PathBuf, String) {
        (
            self.receiver.clone(),
            self.config.delivery_ledger.with_extension("receiver.json"),
            self.config.intent_event_type.clone(),
        )
    }

    /// The receiver-side declaration hold this fixture was configured with:
    /// `None` unless the case asked for it.
    pub(crate) fn receiver_hold(&self) -> Result<Option<ReceiverHold>> {
        if !self.config.receiver_hold {
            return Ok(None);
        }
        let mut url = reqwest::Url::parse(&self.config.body_callback_url)?;
        url.set_path("/DeclarationIssued");
        Ok(Some(ReceiverHold {
            event_type: self.config.intent_event_type.clone(),
            delivery_ledger: self.config.delivery_ledger.clone(),
            url,
            client: reqwest::Client::new(),
        }))
    }

    pub(crate) fn provider(
        &self,
        protocol: provider::FixtureProtocol,
    ) -> Result<lash::provider::ProviderHandle> {
        provider::scripted_provider(
            &self.config.scenario,
            self.labels(),
            protocol,
            &self.config.provider_ledger,
        )
    }
}

/// A declaration hold taken at the receiver's event append rather than on the
/// wire. The append posts the emitting body delivery to the case's
/// body-callback endpoint; the callback publishes the `reached` proof and
/// answers only once the controller releases the barrier, so the response
/// arriving *is* the release.
pub(crate) struct ReceiverHold {
    event_type: String,
    delivery_ledger: PathBuf,
    url: reqwest::Url,
    client: reqwest::Client,
}

impl ReceiverHold {
    pub(crate) async fn before_append(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Result<()> {
        if event_type != self.event_type {
            return Ok(());
        }
        let call_id = payload
            .get("call_id")
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow!("held receiver append lacks a call_id payload"))?;
        let delivery = bodies::deliveries(&self.delivery_ledger)?
            .into_iter()
            .filter(|delivery| delivery.call_id.as_str() == call_id)
            .max_by_key(|delivery| delivery.ordinal)
            .ok_or_else(|| anyhow!("held receiver append names an undelivered call"))?;
        self.client
            .post(self.url.clone())
            .json(&delivery)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// The fixture's tools, with `isolated` bound to the scenario's worker
/// engine at admission and `unbound` left without a binding, so its round
/// refuses typed before any body.
struct IsolatedBinding(Arc<dyn lash::tools::ToolProvider>);

#[async_trait::async_trait]
impl lash::tools::ToolProvider for IsolatedBinding {
    fn tool_manifests(&self) -> Vec<lash::tools::ToolManifest> {
        self.0.tool_manifests()
    }
    fn resolve_manifest(&self, name: &str) -> Option<lash::tools::ToolManifest> {
        self.0.resolve_manifest(name)
    }
    fn resolve_manifest_by_id(
        &self,
        id: &lash::tools::ToolId,
    ) -> Option<lash::tools::ToolManifest> {
        self.0.resolve_manifest_by_id(id)
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash::tools::ToolContract>> {
        self.0.resolve_contract(name)
    }
    fn resolve_contract_by_id(
        &self,
        id: &lash::tools::ToolId,
    ) -> Option<Arc<lash::tools::ToolContract>> {
        self.0.resolve_contract_by_id(id)
    }
    async fn prepare_tool_call(
        &self,
        call: lash::tools::ToolPrepareCall<'_>,
    ) -> std::result::Result<lash::tools::PreparedToolCall, lash::tools::ToolOutcome> {
        self.0.prepare_tool_call(call).await
    }
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        self.0.execute(call).await
    }
    fn isolated_process(
        &self,
        call: lash::tools::IsolatedProcessRequest<'_>,
    ) -> Option<lash::tools::IsolatedProcessBinding> {
        (call.tool_id.as_str() == "tool:e2e.h2.isolated").then(|| {
            lash::tools::IsolatedProcessBinding {
                engine: WORKER_ENGINE.to_owned(),
                payload: call.args.clone(),
                boundary: lash::plugins::ProcessExecutionBoundary::WorkerProcess,
            }
        })
    }
}

/// Contributes the fixture's one worker engine from every call.
struct WorkerEnginePlugin(Arc<dyn lash::plugins::ProcessEngine>);

impl lash::plugins::PluginFactory for WorkerEnginePlugin {
    fn id(&self) -> &'static str {
        WORKER_ENGINE
    }

    fn process_engine_contributions(
        &self,
        _context: &lash::plugins::ProcessEngineContributionContext<'_>,
    ) -> std::result::Result<
        Vec<lash::plugins::ProcessEngineRegistration>,
        lash::plugins::PluginError,
    > {
        Ok(vec![lash::plugins::ProcessEngineRegistration::accepting(
            self.0.clone(),
        )])
    }
    fn build(
        &self,
        _context: &lash::plugins::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash::plugins::SessionPlugin>, lash::plugins::PluginError>
    {
        Ok(Arc::new(WorkerEngineSession))
    }
}

impl lash::plugins::PluginDefinition for WorkerEnginePlugin {
    fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(WORKER_ENGINE)
    }
}

struct WorkerEngineSession;

impl lash::plugins::SessionPlugin for WorkerEngineSession {
    fn id(&self) -> &'static str {
        WORKER_ENGINE
    }
    fn register(
        &self,
        _reg: &mut lash::plugins::PluginRegistrar,
    ) -> std::result::Result<(), lash::plugins::PluginError> {
        Ok(())
    }
}

/// S20 holds the physical termination call before delegating to the production
/// worker. The case can observe a live PID and the still-retained consumer hold
/// despite the durable cancel request. Execution and reaping remain owned by
/// the one production engine; the files are out-of-journal fixture controls.
struct TerminationGate {
    engine: Arc<lash::plugins::WorkerProcessEngine>,
    marker: PathBuf,
}

#[async_trait::async_trait]
impl lash::plugins::PhysicalProcessWorker for TerminationGate {
    async fn terminate_worker(
        &self,
        process: &lash::ProcessId,
    ) -> std::result::Result<lash::plugins::WorkerTerminationReceipt, lash::plugins::PluginError>
    {
        std::fs::write(
            self.marker.with_extension("terminate-entered"),
            process.as_str(),
        )
        .map_err(|error| lash::plugins::PluginError::Session(error.to_string()))?;
        let release = self.marker.with_extension("terminate-release");
        while !release.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        lash::plugins::PhysicalProcessWorker::terminate_worker(self.engine.as_ref(), process).await
    }
}

#[async_trait::async_trait]
impl lash::plugins::ProcessEngine for TerminationGate {
    fn kind(&self) -> &'static str {
        lash::plugins::ProcessEngine::kind(self.engine.as_ref())
    }
    fn physical_worker(&self) -> Option<&dyn lash::plugins::PhysicalProcessWorker> {
        Some(self)
    }
    async fn run(
        &self,
        context: lash::plugins::ProcessEngineRunContext<'_>,
        payload: serde_json::Value,
    ) -> std::result::Result<lash::plugins::ProcessRunOutcome, lash::plugins::ProcessInfraError>
    {
        lash::plugins::ProcessEngine::run(self.engine.as_ref(), context, payload).await
    }
    fn start_artifacts(
        &self,
        payload: &serde_json::Value,
    ) -> std::result::Result<Vec<lash::persistence::ArtifactName>, lash::plugins::PluginError> {
        lash::plugins::ProcessEngine::start_artifacts(self.engine.as_ref(), payload)
    }
    async fn end_artifact_referrer(
        &self,
        cleanup: &lash::persistence::ResolvedArtifactCleanup,
    ) -> std::result::Result<(), lash::persistence::ArtifactStoreError> {
        lash::plugins::ProcessEngine::end_artifact_referrer(self.engine.as_ref(), cleanup).await
    }
    async fn acquire_engine_artifact(
        &self,
        claim: &lash::persistence::ReferrerClaim,
        artifact: &str,
    ) -> std::result::Result<(), lash::plugins::PluginError> {
        lash::plugins::ProcessEngine::acquire_engine_artifact(self.engine.as_ref(), claim, artifact)
            .await
    }
}
