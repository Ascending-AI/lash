//! Observation workloads: process observation, session observation, the host
//! trace sink and the workflow execution overlay, each over served durable
//! nodes. A node's replay stores are the product stores behind counting
//! probes; SQLite nodes keep the built-in memory replay, PostgreSQL nodes use
//! the PostgreSQL replay stores.
mod overlay;
mod probes;
mod process;
mod session;
mod trace;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use lash::process::{
    HostArtifactPin, Lifetime, ProcessDefinition, ProcessExecutionEnvRef, ProcessExecutionEnvSpec,
    ProcessOriginator, ProcessStartRequest, ProcessStartTarget,
};
use lash_core::ToolDefinitionBindingExt;

use super::{Args, Case, Meter, Receipt, facade};
use probes::{LiveReplayProbe, ProcessReplayProbe};

pub(super) async fn run(args: &Args) -> Result<Receipt> {
    match args.case {
        Case::ProcessDispatcher
        | Case::ProcessFeeds
        | Case::ProcessBurst
        | Case::ProcessConvergence
        | Case::ProcessReconcile
        | Case::ProcessRoster => Box::pin(process::run(args)).await,
        Case::SessionReplay | Case::SessionResume => Box::pin(session::run(args)).await,
        Case::TraceSinkOmitted
        | Case::TraceSinkCaptured
        | Case::TraceSinkCustom
        | Case::TraceSinkOtel
        | Case::TraceSinkSlow => Box::pin(trace::run(args)).await,
        Case::OverlayFold | Case::OverlayAttribution => Box::pin(overlay::run(args)).await,
        other => anyhow::bail!("{other:?} is not an observation workload"),
    }
}

/// One served durable node and the probes its replay stores sit behind.
pub(super) struct Node {
    pub(super) core: lash::LashCore,
    /// The node's product stores, for a workload that writes facts through a
    /// store port instead of executing them.
    pub(super) stores: Arc<dyn lash_core::StoreSet>,
    pub(super) process_replay: Arc<ProcessReplayProbe>,
    pub(super) live_replay: Arc<LiveReplayProbe>,
}

/// What a node's sessions and processes run.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Flavor {
    /// Workflow processes in the VM, with the `tools.mark` step.
    Workflow,
    /// Standard-protocol turns answered by the synthetic provider.
    Chat,
}

/// The nodes of one workload over one product store.
pub(super) struct Fleet {
    pub(super) nodes: Vec<Node>,
    pub(super) store: &'static str,
    /// The durable node name each member registered under.
    pub(super) node_names: Vec<String>,
    hosts: Vec<lash::postgres::PostgresHost>,
}

/// One node's backend, product stores and replay stores.
type Wiring = (
    lash::Backend,
    Arc<dyn lash_core::StoreSet>,
    Arc<dyn lash_core::ProcessReplayStore>,
    Arc<dyn lash_core::LiveReplayStore>,
);

/// The lease identity of fleet member `index`. The owner id is the durable
/// node name, so each member has its own; a node registering under a name
/// already in use fences that name's earlier boot. The run is the boot.
fn node_identity(index: usize, run: &str) -> lash::persistence::LeaseOwnerIdentity {
    lash::persistence::LeaseOwnerIdentity::opaque(
        lash::persistence::LeaseOwnerId::new(format!("observation-node-{index}")),
        lash::persistence::LeaseIncarnationId::new(format!("run-{run}")),
    )
}

/// The tools every observation workload's processes and turns may call.
struct Tools {
    mark: lash_core::ToolDefinition,
    meter: Meter,
}

impl Tools {
    fn new(meter: &Meter) -> Result<Self> {
        Ok(Self {
            mark: lash_core::ToolDefinition::raw(
                "tool:mark",
                "mark",
                "Synthetic observation workload step",
                serde_json::json!({"type":"object","properties":{"key":{"type":"string"}},
                    "required":["key"],"additionalProperties":false}),
                serde_json::json!({"type":"object"}),
            )?
            .with_execution(Duration::from_secs(120))
            .with_tool_binding(
                lash_core::ToolBinding::new(["tools"], "mark").with_authority_type("Tools"),
            ),
            meter: meter.clone(),
        })
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Tools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![self.mark.manifest()]
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "mark").then(|| Arc::new(self.mark.contract()))
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let start = Instant::now();
        let output =
            lash_core::ToolCallOutput::success(serde_json::json!({"key": call.args["key"]}));
        self.meter
            .operation("tool.mark", call.context.call_id(), "ok", start);
        lash_core::ToolOutcome::from_output(output).into()
    }
}

impl Fleet {
    /// `nodes` served nodes over one product store. `configure` finishes each
    /// node's builder; it receives the node's index.
    pub(super) async fn open(
        args: &Args,
        meter: &Meter,
        nodes: usize,
        flavor: Flavor,
        configure: impl Fn(lash::LashCoreBuilder, usize) -> Result<lash::LashCoreBuilder>,
    ) -> Result<Self> {
        let mut fleet = Self {
            nodes: Vec::new(),
            store: if args.postgres_url.is_some() {
                "postgres18-product+postgres-replay"
            } else {
                "sqlite-file-product+memory-replay"
            },
            node_names: Vec::new(),
            hosts: Vec::new(),
        };
        // One replay schema pair per run, shared by its replicas.
        let run = uuid::Uuid::new_v4().simple().to_string();
        for index in 0..nodes {
            let (backend, stores, process_replay, live_replay): Wiring =
                if let Some(url) = args.postgres_url.as_deref() {
                    let host = postgres_host(url, &run).await?;
                    let backend = lash::durable::DurableBackendBuilder::postgres(
                        &host,
                        Arc::new(lash_core::attachments::UnavailableAttachmentStore),
                    )
                    .build()?;
                    let process = host.process_replay.clone().context("process replay")?;
                    let live = host.live_replay.clone().context("live replay")?;
                    let stores = Arc::new(lash::postgres::PostgresStoreSet::new(
                        &host.storage,
                        Arc::new(lash_core::attachments::UnavailableAttachmentStore),
                    ));
                    fleet.hosts.push(host);
                    (backend, stores, process, live)
                } else {
                    let stores: Arc<dyn lash_core::StoreSet> = Arc::new(
                        lash_sqlite_store::SqliteStoreSet::open(
                            args.store_dir.join("lash.db"),
                            lash_sqlite_store::SqliteSynchronous::Normal,
                        )
                        .await?,
                    );
                    (
                        facade::backend(stores.clone())?,
                        stores,
                        Arc::new(lash_core::InMemoryProcessReplayStore::new(
                            lash_core::InMemoryProcessReplayStoreConfig::standard(),
                        )),
                        Arc::new(lash_core::facade_support::InMemoryLiveReplayStore::new(
                            lash_core::facade_support::InMemoryLiveReplayStoreConfig::standard(),
                        )),
                    )
                };
            let process_replay = ProcessReplayProbe::new(process_replay, meter);
            let live_replay = LiveReplayProbe::new(live_replay, meter);
            let builder = match flavor {
                Flavor::Workflow => {
                    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
                        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                            .channel(lash_protocol_rlm::RlmChannel::Cell)
                            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(
                                50_000_000,
                            ))
                            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                            .build(),
                        Arc::new(lash_protocol_rlm::TypescriptDialect),
                        &backend,
                    );
                    lash::LashCore::rlm_builder(backend, factory)
                        .tools(Arc::new(Tools::new(meter)?))
                }
                Flavor::Chat => lash::LashCore::standard_builder(backend)
                    .serve_test_llm_profile(facade::provider(meter, false), facade::metadata()?),
            }
            .process_replay_store(process_replay.clone())
            .live_replay_store(live_replay.clone())
            .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
            .data_retention(lash::DataRetention::standard())
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended());
            let identity = node_identity(index, &run);
            fleet.node_names.push(identity.owner_id.clone());
            let core = configure(builder, index)?.build(identity)?;
            fleet.nodes.push(Node {
                core,
                stores,
                process_replay,
                live_replay,
            });
        }
        // A node name registered twice fences its earlier boot.
        let distinct: std::collections::BTreeSet<_> = fleet.node_names.iter().collect();
        anyhow::ensure!(
            distinct.len() == nodes,
            "the fleet's {nodes} nodes share node names: {:?}",
            fleet.node_names
        );
        Ok(fleet)
    }

    pub(super) async fn close(self) -> Result<()> {
        for node in &self.nodes {
            node.core.shutdown().await?;
        }
        for host in self.hosts {
            host.storage.pool().close().await;
        }
        Ok(())
    }
}

async fn postgres_host(url: &str, run: &str) -> Result<lash::postgres::PostgresHost> {
    use lash::postgres::{
        LiveReplayPolicy, PostgresEndpoints, PostgresHost, PostgresHostConfig, ProcessReplayPolicy,
        ReplaySchemaMode,
    };
    let mut process = ProcessReplayPolicy::default();
    process.data.schema = format!("perf_process_{run}");
    process.data.schema_mode = ReplaySchemaMode::Install;
    let mut live = LiveReplayPolicy::default();
    live.data.schema = format!("perf_live_{run}");
    live.data.schema_mode = ReplaySchemaMode::Install;
    let config = PostgresHostConfig {
        process_replay: Some(process),
        live_replay: Some(live),
        ..PostgresHostConfig::default()
    };
    let host = PostgresHost::connect(
        &PostgresEndpoints::from_url(url)?,
        &config,
        Default::default(),
    )
    .await?;
    let version: String = sqlx::query_scalar("SHOW server_version_num")
        .fetch_one(host.storage.pool())
        .await?;
    ensure!(
        version.parse::<u32>()? / 10000 == 18,
        "observation workloads require PostgreSQL 18"
    );
    Ok(host)
}

/// `name` made unique to this OS process: a PostgreSQL database outlives one
/// population, and session ids and start keys are never reused.
pub(super) fn unique(name: &str) -> String {
    static RUN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let run = RUN.get_or_init(|| uuid::Uuid::new_v4().simple().to_string()[..12].to_owned());
    format!("{name}-{run}")
}

/// The environment a workload's processes run under and are admitted against.
fn environment() -> ProcessExecutionEnvSpec {
    ProcessExecutionEnvSpec::new(
        lash::plugins::AdmittedPluginConfig::default(),
        lash::runtime::SessionPolicy::new(
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1_000_000),
            lash::NoProgressBudget::bounded(12),
        ),
        lash::plugins::SessionToolAccess::ambient(),
    )
}

/// A workflow lash admitted from TypeScript source and holds under a pin.
pub(super) struct Published {
    definition: ProcessDefinition,
    env_ref: ProcessExecutionEnvRef,
}

pub(super) async fn publish(core: &lash::LashCore, source: &str) -> Result<Published> {
    use lash::workflow::{WorkflowDraft, WorkflowEntry, WorkflowPublish};
    let graph = lash_typescript::workflow_graph::workflow_graph_from_source(source)
        .map_err(|error| anyhow::anyhow!("workflow source: {error}"))?;
    let draft = WorkflowDraft::open(&graph)
        .map_err(|error| anyhow::anyhow!("workflow draft: {error:?}"))?;
    let pin = HostArtifactPin::mint();
    let environment = environment();
    let artifacts = core.host_artifacts();
    let publication = match within(
        "workflow publication",
        artifacts.publish_workflow(&pin, &draft, WorkflowEntry::Sole, &environment),
    )
    .await??
    {
        WorkflowPublish::Published(publication) => *publication,
        other => anyhow::bail!("workflow was not admitted: {other:?}"),
    };
    let env_ref = artifacts.publish_process_env(&pin, &environment).await?;
    Ok(Published {
        definition: publication.definition,
        env_ref,
    })
}

pub(super) async fn start(
    core: &lash::LashCore,
    published: &Published,
    key: &str,
) -> Result<lash::ProcessId> {
    let request = ProcessStartRequest::new(
        ProcessStartTarget::Definition {
            definition_id: published.definition.id.clone(),
            signature_claim: Some(published.definition.signature.clone()),
            args: Default::default(),
        },
        ProcessOriginator::host(),
        Lifetime::Detached,
    )
    .with_host_start_key(unique(key))
    .with_env_ref(published.env_ref.clone());
    Ok(within(
        "process start",
        Box::pin(core.processes().start(request, core.effect_host())),
    )
    .await??
    .process_id)
}

/// A process of `iterations` loop turns, each of `steps` tool calls. A step
/// is a committed effect the VM observes at its own site.
pub(super) fn loop_source(iterations: usize, steps: usize) -> String {
    let items = (0..iterations)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let mut source =
        format!("const observed = async () => {{\n  for (const item of [{items}]) {{\n");
    for _ in 0..steps {
        source.push_str("    await tools.mark({ key: \"step\" });\n");
    }
    source.push_str("  }\n  return 0;\n};\n");
    source
}

/// A process that commits about `commits` effect facts. A site's committed
/// outcomes stop at the effect summary's occurrence cap, so the facts are
/// spread over as many step sites as that takes.
pub(super) fn commit_source(commits: usize) -> String {
    let cap = usize::try_from(lash::process::PROCESS_EFFECT_OCCURRENCE_CAP).unwrap_or(8);
    loop_source(commits.min(cap), commits.div_ceil(cap))
}

/// A process that waits until it is cancelled.
pub(super) const QUIET_SOURCE: &str =
    "const quiet = async () => {\n  await sleep(\"1h\");\n  return 0;\n};\n";

/// A process that ends at once.
pub(super) const BLANK_SOURCE: &str = "const blank = async () => {\n  return 0;\n};\n";

pub(super) const SETTLE: Duration = Duration::from_secs(60);
/// A whole population's bound: long runs settle well inside it.
pub(super) const POPULATION: Duration = Duration::from_secs(1800);

/// Await a population-sized `work`, naming it when it does not settle.
pub(super) async fn within_population<T>(label: &str, work: impl Future<Output = T>) -> Result<T> {
    tokio::time::timeout(POPULATION, work)
        .await
        .map_err(|_| anyhow::anyhow!("{label} did not settle within {POPULATION:?}"))
}

/// Await a process's successful terminal.
pub(super) async fn settled(core: &lash::LashCore, process: &lash::ProcessId) -> Result<()> {
    let output =
        within_population("process terminal", core.processes().await_output(process)).await??;
    ensure!(
        matches!(&output, lash_core::ProcessAwaitOutput::Settled { output }
            if output.status() == lash_sansio::ToolCallStatus::Success),
        "the observed process failed: {output:?}"
    );
    Ok(())
}

/// Await `work`, naming the boundary that hung when it does not settle.
pub(super) async fn within<T>(label: &str, work: impl Future<Output = T>) -> Result<T> {
    tokio::time::timeout(SETTLE, work)
        .await
        .map_err(|_| anyhow::anyhow!("{label} did not settle within {SETTLE:?}"))
}

#[cfg(test)]
mod tests;
