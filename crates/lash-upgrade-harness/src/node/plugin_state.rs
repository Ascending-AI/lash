//! `plugin-state`: one build publishing and reading a plugin's state
//! namespace over a SQLite store (FIG-4746).
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
    /// The generation `finalize` retires.
    #[arg(long)]
    pub generation: Option<lash_core::engine::BuildGeneration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum PluginStateAction {
    /// Read the stored namespace through this build's plugin host.
    Read,
    /// Add one to the counter and publish it in the newest format this build
    /// can write that the fleet record permits.
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
}

struct ProbePlugin;

impl ProbePlugin {
    /// The key the counter lives under in this build's native format.
    const fn native_key() -> &'static str {
        if cfg!(feature = "synthetic-next") {
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
        if cfg!(feature = "synthetic-next") {
            let native = FormatVersion::new(2).unwrap_or(FormatVersion::ONE);
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

    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Err(PluginError::Session(
            "the probe plugin builds no session: its state is published directly".into(),
        ))
    }
}

/// This build's registration of the probe plugin, as the store sees it.
fn registration() -> PluginWriterRegistration {
    let declaration = ProbePlugin.declaration();
    PluginWriterRegistration {
        plugin: PLUGIN.to_owned(),
        native: declaration.format_version,
        writable: declaration.writable_formats,
    }
}

async fn load(store: &Arc<dyn RuntimeStore>, session: &SessionId) -> Result<RuntimeSessionState> {
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
fn counter(state: &PluginState) -> Option<u64> {
    state
        .plugins
        .get(PLUGIN)
        .and_then(|namespace| namespace.values.get(ProbePlugin::native_key()))
        .and_then(serde_json::Value::as_u64)
}

pub async fn run(args: PluginStateArgs) -> Result<PluginStateReport> {
    use lash::StoreSet as _;
    use lash_core::ClockWallTime as _;
    let stores = super::open_sqlite(&args.store_dir).await?;
    let store: Arc<dyn RuntimeStore> = stores.session_store_factory();
    let host = PluginHost::new(vec![Arc::new(ProbePlugin)]);
    let session = SessionId::from(args.session.as_str());
    let mut report = PluginStateReport {
        build: BuildLabel::current(),
        fleet: store.fleet_format().version(),
        permitted: None,
        stored_format: None,
        value: None,
        refusal: None,
        fenced_at: None,
        unreadable: None,
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
        let flip = stores
            .finalize(
                &generation,
                &lash_core::store::fleet_finalize::NoDeployments,
                &[registration()],
                now,
            )
            .await?;
        report.fleet = flip.fleet();
        report.permitted = store.plugin_writers().await?.permitted_writer(PLUGIN).ok();
        return Ok(report);
    }

    let mut state = load(&store, &session).await?;
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
    report.value = counter(&decoded);
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
    let writer = match args.action {
        PluginStateAction::WriteNative => registered.native,
        _ => registered
            .writable
            .iter()
            .copied()
            .filter(|format| permitted.contains(format.get()))
            .max()
            .ok_or_else(|| anyhow!("this build writes no format inside {permitted}"))?,
    };

    let next = report.value.unwrap_or(0) + 1;
    let mut native = decoded;
    let namespace =
        native
            .plugins
            .entry(PLUGIN.to_owned())
            .or_insert_with(|| PluginNamespaceState {
                format_version: registered.native,
                generation: 0,
                values: BTreeMap::new(),
            });
    namespace.values.insert(
        ProbePlugin::native_key().to_owned(),
        serde_json::json!(next),
    );
    namespace.generation += 1;
    let encoded = host
        .encode_state(&native, &BTreeMap::from([(PLUGIN.to_owned(), writer)]))
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
