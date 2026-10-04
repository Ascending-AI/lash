//! `plugin-state`: one build publishing and reading a plugin's state
//! namespace through admitted callbacks on the Restate server double (FIG-4858).
//!
//! The probe plugin's format differs between the two builds: N reads and
//! writes format 1 (`{"count": n}`), and the synthetic N+1 reads format 2
//! natively (`{"total": n}`) while it can still write format 1. Every step
//! goes through the build's own plugin host (the stamp check, the migrate
//! and the encode) and its own store, so a namespace one build publishes is
//! read by the other build's real decoder, and a publication the fleet
//! record does not permit is refused by the store's guarded transaction.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, ValueEnum};
use lash_core::facade_support::PluginHost;
use lash_core::plugin::{PluginDeclaration, PluginFactory, PluginSessionContext, SessionPlugin};
use lash_core::store::plugin_writers::PluginWriterRegistration;
use lash_core::store::{RuntimeCommit, RuntimeStore};
use lash_core::{
    FormatNamespace, FormatRefusal, FormatVersion, PluginError, PluginNamespaceState, PluginState,
    RuntimeSessionState, SessionId, StoreError,
};
use serde::{Deserialize, Serialize};

use crate::identity::BuildLabel;

/// The probe plugin's id, the same in both builds.
pub const PLUGIN: &str = "upgrade-probe";

#[derive(Clone, Debug, Args)]
pub struct PluginStateArgs {
    /// The SQLite store directory.
    #[arg(long)]
    pub store_dir: PathBuf,
    #[arg(long)]
    pub session: String,
    #[arg(long, value_enum)]
    pub action: PluginStateAction,
    /// The PostgreSQL overlap store; omitted for SQLite.
    #[arg(long)]
    pub database_url: Option<String>,
    /// Read a retained admission head instead of the current head.
    #[arg(long)]
    pub history_head: Option<String>,
    /// The generation `finalize` retires.
    #[arg(long)]
    pub generation: Option<lash_core::engine::BuildGeneration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum PluginStateAction {
    /// Read the stored namespace through this build's plugin host.
    Read,
    /// Admit a turn whose callback increments in the recorded writer format.
    Write,
    /// Add one to the counter and publish it in this build's native format,
    /// whatever the fleet record permits.
    WriteNative,
    /// Finalize the store with this build's plugin registration.
    Finalize,
}

/// What one `plugin-state` step found and did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginStateReport {
    pub build: BuildLabel,
    /// The fleet epoch the store records after the step.
    pub fleet: u32,
    /// The writer range the fleet record carries for the probe plugin.
    pub permitted: Option<lash_core::compat::VersionRange>,
    /// The format the stored namespace is stamped with after the step.
    pub stored_format: Option<u32>,
    /// The counter, read through this build's plugin host.
    pub value: Option<u64>,
    /// The store's typed refusal of the step's publication.
    pub refusal: Option<lash_core::compat::CompatRefusal>,
    /// The fleet epoch that fenced this build's write.
    pub fenced_at: Option<u32>,
    /// This build's plugin host refusing a stored format it cannot read.
    pub unreadable: Option<FormatRefusal>,
    pub callbacks: usize,
    pub generation: Option<String>,
    pub config: Option<lash_core::PluginConfig>,
    pub model_route: Option<lash_core::LlmProfileConfig>,
    pub head: Option<lash_core::store::SessionHeadRef>,
    pub namespace_bytes: Option<Vec<u8>>,
}

#[derive(Clone)]
struct ProbePlugin {
    native: FormatVersion,
    calls: Arc<AtomicUsize>,
}

impl Default for ProbePlugin {
    fn default() -> Self {
        Self {
            native: FormatVersion::new(if cfg!(feature = "synthetic-next") {
                2
            } else {
                1
            })
            .unwrap_or(FormatVersion::ONE),
            calls: Arc::default(),
        }
    }
}

impl ProbePlugin {
    /// The key the counter lives under in this build's native format.
    const fn native_key(&self) -> &'static str {
        if self.native.get() == 2 {
            "total"
        } else {
            "count"
        }
    }

    fn refusal(&self, namespace: FormatNamespace, stored: FormatVersion) -> FormatRefusal {
        FormatRefusal {
            plugin: PLUGIN.into(),
            namespace,
            stored,
            readable: self.declaration().format_version,
        }
    }
}

fn rename(mut value: serde_json::Value, from: &str, to: &str) -> serde_json::Value {
    if let Some(map) = value.as_object_mut()
        && let Some(counter) = map.remove(from)
    {
        map.insert(to.to_owned(), counter);
    }
    value
}

#[lash_core::async_trait]
impl PluginFactory for ProbePlugin {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn declaration(&self) -> PluginDeclaration {
        let mut declaration = PluginDeclaration::initial(PLUGIN);
        if self.native.get() == 2 {
            let native = self.native;
            declaration.behavior_revision =
                lash_core::plugin::BehaviorRevision::from(std::num::NonZeroU32::from(native));
            declaration.format_version = native;
            declaration.writable_formats = vec![FormatVersion::ONE, native];
        }
        declaration
    }

    fn migrate_format(
        &self,
        from: FormatVersion,
        namespace: FormatNamespace,
        value: serde_json::Value,
    ) -> Result<serde_json::Value, FormatRefusal> {
        let native = self.declaration().format_version;
        if from == native {
            Ok(value)
        } else if from == FormatVersion::ONE {
            Ok(rename(value, "count", "total"))
        } else {
            Err(self.refusal(namespace, from))
        }
    }

    fn encode_format(
        &self,
        to: FormatVersion,
        namespace: FormatNamespace,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, FormatRefusal> {
        let native = self.declaration().format_version;
        if to == native {
            Ok(value.clone())
        } else if to == FormatVersion::ONE && self.declaration().writable_formats.contains(&to) {
            Ok(rename(value.clone(), "total", "count"))
        } else {
            Err(self.refusal(namespace, to))
        }
    }

    fn register_config(
        &self,
        registrar: &mut lash_core::ConfigRegistrar,
    ) -> Result<(), lash_core::ConfigRegistrationError> {
        registrar.owner(ProbeConfigOwner {
            default_step: if self.native.get() == 1 { 1 } else { 100 },
        })
    }

    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, lash_core::facade_support::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
struct ProbeConfig {
    seed: u64,
    step: u64,
    label: String,
}

struct ProbeConfigOwner {
    default_step: u64,
}

impl lash_core::ConfigOwner for ProbeConfigOwner {
    type Create = ProbeConfig;
    type Recorded = ProbeConfig;
    type Refusal = String;
    type RunOptions = lash_core::NoRunOptions;

    fn create(
        &self,
        input: Option<ProbeConfig>,
        _: lash_core::CreationFacts<'_, ProbeConfig>,
    ) -> Result<Option<ProbeConfig>, String> {
        Ok(Some(input.unwrap_or(ProbeConfig {
            seed: 6,
            step: self.default_step,
            label: "recorded at creation".into(),
        })))
    }

    fn validate(
        &self,
        _: &ProbeConfig,
        _: Option<&ProbeConfig>,
        _: &lash_core::CandidateFacts<'_>,
    ) -> Result<(), String> {
        Ok(())
    }

    fn apply_run_options(
        &self,
        recorded: &ProbeConfig,
        _: lash_core::NoRunOptions,
    ) -> Result<ProbeConfig, String> {
        Ok(recorded.clone())
    }
}

impl SessionPlugin for ProbePlugin {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn register(
        &self,
        registrar: &mut lash_core::plugin::PluginRegistrar,
    ) -> Result<(), PluginError> {
        // The counter's read-modify-write is a pure reducer the after-turn
        // callback names; its resolution is recorded with the callback.
        registrar.state_reducer(
            "increment",
            Arc::new(|reduction: lash_core::plugin::StateReduction<'_>| {
                let seed = reduction.input["seed"].as_u64().unwrap_or(0);
                let step = reduction.input["step"].as_u64().unwrap_or(0);
                let current = reduction.current.and_then(serde_json::Value::as_u64);
                Ok(Some(serde_json::json!(current.unwrap_or(seed) + step)))
            }),
        )?;
        let key = self.native_key();
        let calls = Arc::clone(&self.calls);
        registrar.turn().after(
            lash_core::hook_key!("counter"),
            Arc::new(move |ctx| {
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    let value = ctx.plugin_config.config.get(PLUGIN).ok_or_else(|| {
                        PluginError::Session("missing recorded counter config".into())
                    })?;
                    let config: ProbeConfig = serde_json::from_value(value.clone())
                        .map_err(|error| PluginError::Session(error.to_string()))?;
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(lash_core::plugin::AfterTurnContributions {
                        state: lash_core::plugin::StateCommands::new().apply(
                            key,
                            "increment",
                            serde_json::json!({"seed": config.seed, "step": config.step}),
                        ),
                        ..Default::default()
                    })
                })
            }),
        )?;
        Ok(())
    }
}

/// This build's registration of the probe plugin, as the store sees it.
fn registration() -> PluginWriterRegistration {
    let declaration = ProbePlugin::default().declaration();
    PluginWriterRegistration {
        plugin: PLUGIN.to_owned(),
        native: declaration.format_version,
        writable: declaration.writable_formats,
    }
}

async fn load<S: RuntimeStore + ?Sized>(
    store: &Arc<S>,
    session: &SessionId,
) -> Result<RuntimeSessionState> {
    if let Some(read) = store
        .load_session_window(session, lash_core::store::WindowSelector::Current)
        .await?
    {
        return Ok(lash_core::store::window_state(read, store.fleet_format())?.state);
    }
    store
        .admit_session(&lash_core::testing::store_fixtures::root_session_request(
            session,
        ))
        .await?;
    let read = store
        .load_session_window(session, lash_core::store::WindowSelector::Current)
        .await?
        .ok_or_else(|| anyhow!("the admitted session has no window"))?;
    Ok(lash_core::store::window_state(read, store.fleet_format())?.state)
}

/// The counter of a namespace decoded into this build's native format.
fn counter(plugin: &ProbePlugin, state: &PluginState) -> Option<u64> {
    state
        .plugins
        .get(PLUGIN)
        .and_then(|namespace| namespace.values.get(plugin.native_key()))
        .and_then(serde_json::Value::as_u64)
}

pub async fn run(args: PluginStateArgs) -> Result<PluginStateReport> {
    use lash_core::ClockWallTime as _;
    use lash_core::FleetFormatStore as _;
    let stores: Arc<dyn lash::StoreSet> = if let Some(url) = &args.database_url {
        super::open_stores(&super::StoreArgs {
            store: super::StoreSpec::Postgres(url.clone()),
            data_dir: args.store_dir.clone(),
        })
        .await?
    } else {
        Arc::new(super::open_sqlite(&args.store_dir).await?)
    };
    let store: Arc<dyn RuntimeStore> = stores.session_store_factory();
    let host = PluginHost::new(vec![Arc::new(ProbePlugin::default())]);
    let session = SessionId::parse(args.session.as_str())?;
    let mut report = PluginStateReport {
        build: BuildLabel::current(),
        fleet: store.fleet_format().version(),
        permitted: None,
        stored_format: None,
        value: None,
        refusal: None,
        fenced_at: None,
        unreadable: None,
        callbacks: 0,
        generation: None,
        config: None,
        model_route: None,
        head: None,
        namespace_bytes: None,
    };

    if args.action == PluginStateAction::Finalize {
        let generation = args
            .generation
            .context("finalize names the generation it retires")?;
        let now = lash_core::facade_support::SystemClock.timestamp_ms();
        stores
            .generation_drain()
            .mark_draining(&generation, now)
            .await?;
        let flip = if let Some(url) = &args.database_url {
            lash_postgres_store::PostgresStorage::connect(url)
                .await?
                .finalize(
                    &generation,
                    &lash_core::store::fleet_finalize::NoDeployments,
                    lash_core::store::fleet_finalize::FinalizeMode::Automatic,
                    &[registration()],
                    now,
                )
                .await?
                .flip
        } else {
            // Release the read set before acquiring SQLite's exclusive owner.
            drop(stores);
            drop(store);
            let sqlite = super::open_sqlite(&args.store_dir).await?;
            let flip = sqlite
                .finalize(
                    &generation,
                    &lash_core::store::fleet_finalize::NoDeployments,
                    &[registration()],
                    now,
                )
                .await?;
            report.permitted = sqlite
                .session_store_factory()
                .plugin_writers()
                .await?
                .permitted_writer(PLUGIN)
                .ok();
            report.fleet = flip.fleet();
            return Ok(report);
        };
        report.fleet = flip.fleet();
        report.permitted = store.plugin_writers().await?.permitted_writer(PLUGIN).ok();
        return Ok(report);
    }

    if args.action == PluginStateAction::Write {
        let plugin = ProbePlugin::default();
        let calls = Arc::clone(&plugin.calls);
        report.generation = Some(
            callback_on(Arc::clone(&stores), &session, plugin)
                .await?
                .to_string(),
        );
        report.callbacks = calls.load(Ordering::SeqCst);
        let state = load(&store, &session).await?;
        let stored = state.plugin_state().cloned().unwrap_or_default();
        report.value = counter(&ProbePlugin::default(), &host.decode_state(&stored)?);
        report.config = Some(state.authority.plugin_config.clone());
        report.model_route = state.policy.model.clone();
        store
            .pin(&session, &lash_core::Target::Revision(state.head_revision))
            .await?;
        capture_history(&store, &session, &mut report).await?;
        report.stored_format = stored
            .plugins
            .get(PLUGIN)
            .map(|namespace| namespace.format_version.get());
        report.permitted = Some(store.plugin_writers().await?.permitted_writer(PLUGIN)?);
        return Ok(report);
    }

    let mut state = if let Some(head) = &args.history_head {
        let read = store
            .load_session_window(
                &session,
                lash_core::store::WindowSelector::Admitted(serde_json::from_str(head)?),
            )
            .await?
            .context("retained plugin history")?;
        report.namespace_bytes = read
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| {
                checkpoint.component_body(lash_core::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
            })
            .map(<[u8]>::to_vec);
        lash_core::store::window_state(read, store.fleet_format())?.state
    } else {
        load(&store, &session).await?
    };
    let stored = state.plugin_state().cloned().unwrap_or_default();
    report.stored_format = stored
        .plugins
        .get(PLUGIN)
        .map(|namespace| namespace.format_version.get());
    // The read: the stamp is checked and an older format migrated by this
    // build's own plugin host.
    let decoded = match host.decode_state(&stored) {
        Ok(decoded) => decoded,
        Err(PluginError::Format(refusal)) => {
            report.unreadable = Some(refusal);
            report.permitted = store.plugin_writers().await?.permitted_writer(PLUGIN).ok();
            return Ok(report);
        }
        Err(error) => bail!("decode the stored plugin state: {error}"),
    };
    report.value = counter(&ProbePlugin::default(), &decoded);
    report.config = Some(state.authority.plugin_config.clone());
    report.model_route = state.policy.model.clone();
    if args.history_head.is_none() {
        capture_history(&store, &session, &mut report).await?;
    }
    if args.action == PluginStateAction::Read {
        report.permitted = store.plugin_writers().await?.permitted_writer(PLUGIN).ok();
        return Ok(report);
    }

    // The fleet record is provisioned from this build's registration; a
    // recorded range is left as it is.
    let registered = registration();
    let permitted = match store
        .provision_plugin_writers(std::slice::from_ref(&registered))
        .await
    {
        Ok(ranges) => ranges.permitted_writer(PLUGIN)?,
        Err(StoreError::WriterFenced { recorded, .. }) => {
            report.fenced_at = Some(recorded);
            return Ok(report);
        }
        Err(error) => return Err(error.into()),
    };
    report.permitted = Some(permitted);
    let writer = registered.native;

    let next = report.value.unwrap_or(0) + 1;
    let mut native = decoded;
    let namespace =
        native
            .plugins
            .entry(PLUGIN.to_owned())
            .or_insert_with(|| PluginNamespaceState {
                format_version: registered.native,
                generation: 0,
                publication: Default::default(),
                values: BTreeMap::new(),
            });
    namespace.values.insert(
        ProbePlugin::default().native_key().to_owned(),
        serde_json::json!(next),
    );
    namespace.generation += 1;
    let mut writers = native
        .plugins
        .iter()
        .map(|(id, namespace)| (id.clone(), namespace.format_version))
        .collect::<BTreeMap<_, _>>();
    writers.insert(PLUGIN.to_owned(), writer);
    let encoded = host
        .encode_state(&native, &writers)
        .map_err(|error| anyhow!("encode the plugin state at format {writer}: {error}"))?;
    state.set_plugin_state(Some(encoded));
    // The commit is stamped under the store's recorded epoch, as every
    // durable writer's is: inside the window N+1 writes what N reads.
    let operation = lash_core::OperationId::new(
        lash_core::ExecutionScope::runtime_operation(format!(
            "plugin-state:{}:{}",
            args.session, state.head_revision
        )),
        "commit",
    );
    let (commit, _) = RuntimeCommit::persisted_state_with_operation_and_budget(
        &mut state,
        operation,
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        store.fleet_format(),
    )?;
    match store.commit_runtime_state(commit).await {
        Ok(_) => {
            report.stored_format = Some(writer.get());
            report.value = Some(next);
        }
        Err(StoreError::Incompatible { refusal }) => report.refusal = Some(refusal),
        Err(StoreError::WriterFenced { recorded, .. }) => report.fenced_at = Some(recorded),
        Err(error) => return Err(error.into()),
    }
    report.fleet = store.fleet_format().version();
    Ok(report)
}

async fn capture_history<S: RuntimeStore + ?Sized>(
    store: &Arc<S>,
    session: &SessionId,
    report: &mut PluginStateReport,
) -> Result<()> {
    let read = store
        .load_session_window(session, lash_core::store::WindowSelector::Current)
        .await?
        .context("stored callback head")?;
    report.head = Some(lash_core::store::SessionHeadRef {
        generation: store.read_session_state_version(session).await?,
        revision: read.head_revision,
        leaf: read.window.nodes.last().map(|node| node.node_id.clone()),
        checkpoint: read.checkpoint_ref.clone(),
    });
    report.namespace_bytes = read
        .checkpoint
        .as_ref()
        .and_then(|checkpoint| {
            checkpoint.component_body(lash_core::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
        })
        .map(<[u8]>::to_vec);
    Ok(())
}

const SEED: u64 = 0x4858;

fn callback_core(backend: lash::Backend, plugin: ProbePlugin) -> Result<lash::LashCore> {
    super::core_builder(backend, &super::provider::ProviderArgs::default())?
        .plugin(Arc::new(plugin))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "plugin-rollback",
            "callback",
        ))
        .map_err(anyhow::Error::from)
}

async fn callback_on(
    stores: Arc<dyn lash::StoreSet>,
    session: &SessionId,
    plugin: ProbePlugin,
) -> Result<lash_core::engine::BuildGeneration> {
    let engine = super::engine(
        Arc::clone(&stores),
        &super::RestateArgs {
            ingress_url: "http://127.0.0.1:9".into(),
            admin_url: "http://127.0.0.1:9".into(),
            authority: format!("lash-restate-test-{SEED}"),
            namespace: String::new(),
        },
    )?;
    let generation = callback_core(lash::Backend::new(engine), plugin.clone())?
        .build_generation()
        .clone();
    let double = lash_restate_test::backend_with_store_set(
        SEED,
        lash_restate_test::ServerConfig {
            build_generation: generation.clone(),
            ..Default::default()
        },
        lash_restate_test::DeploymentHooks::default(),
        |_| async move { Ok(stores) },
    )
    .await?;
    let core = callback_core(double.lash_backend(), plugin)?;
    match core
        .session(session.clone())
        .create(lash::SessionCreation::root(super::session_spec()))
        .await
    {
        Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    let handle = core
        .session(session.clone())
        .durable()
        .await?
        .send(lash::TurnInput::text("increment the recorded counter"))
        .into_future()
        .await?;
    let outcome =
        tokio::time::timeout(std::time::Duration::from_secs(60), handle.outcome()).await??;
    anyhow::ensure!(
        matches!(outcome.status(), lash::TurnStatus::Answered),
        "callback turn: {outcome:?}"
    );
    anyhow::ensure!(
        outcome
            .output()
            .and_then(|output| output.assistant_message())
            == Some(super::served_by(BuildLabel::current(), &generation.to_string()).as_str()),
        "the admitted callback ran on another generation's route"
    );
    Ok(generation)
}

#[cfg(test)]
mod tests;
