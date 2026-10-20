//! The one path a process environment becomes held for a start (D10;
//! final-spec §3.3).
//!
//! A new environment is admitted against the installed process components
//! and then published under the starter's claim; a published one is
//! acquired under it. Nothing else publishes or acquires a process
//! environment: host pins, model starts, start staging, isolated tool runs
//! and nested starts all come through [`hold_process_env`].

use lash_core_store::process_env::{ProcessEnv, ProcessEnvRefusal};

use super::model::ProcessEnvRef;
use super::model::process_env_ref_for_bytes;
use crate::ReferrerClaim;
use crate::plugin::ProcessComponentRegistry;
use lash_execution_contracts::module_artifacts::ArtifactStoreError;
use lash_execution_contracts::plugin_error::PluginError;
use lash_execution_contracts::process::ProcessEnvStore;

/// Which environment a start holds: one stated now, admitted and published
/// by the hold, or one already published.
#[derive(Clone, Debug, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "a selection is made once per start and moved into its hold"
)]
pub enum ProcessEnvSelection {
    New(ProcessEnv),
    Published(ProcessEnvRef),
}

/// Why a hold did not answer a reference.
#[derive(Debug, thiserror::Error)]
pub enum HoldEnvError {
    /// The environment is refused: a typed, model-visible judgment that a
    /// retry meets again.
    #[error(transparent)]
    Refused(ProcessEnvRefusal),
    /// The hold could not complete now: a store or transport fault, or a
    /// fenced claim. Retryable; never recorded as a refusal.
    #[error(transparent)]
    Fault(PluginError),
}

/// What a hold reads and writes through: the environment store (and the
/// fleet record it publishes against), and, for a hold that admits new
/// environments, the installed components they are admitted against.
#[derive(Clone, Copy)]
pub struct ProcessEnvPorts<'a> {
    store: &'a dyn ProcessEnvStore,
    components: Option<&'a ProcessComponentRegistry>,
}

impl<'a> ProcessEnvPorts<'a> {
    /// Ports that only acquire published environments.
    pub fn published_only(store: &'a dyn ProcessEnvStore) -> Self {
        Self {
            store,
            components: None,
        }
    }

    /// Ports that admit new environments against `registry`, provisioning
    /// its components' writer ranges in `store`'s fleet record.
    pub fn admitting(
        store: &'a dyn ProcessEnvStore,
        registry: &'a ProcessComponentRegistry,
    ) -> Self {
        Self {
            store,
            components: Some(registry),
        }
    }
}

/// Hold `selection` under `claim` and answer its reference.
///
/// `New` runs the full admission in order (ambient tools, unique ids,
/// installed components, readable formats, each `validate`, each
/// `build(process: None)`, a composable set), provisions the installed
/// components' fleet writer ranges when the record does not name one the
/// environment names, then publishes; the store admits each component's
/// config format against the fleet writer record under `component:<id>`. A
/// refused environment stores nothing.
/// `Published` acquires the stored bytes; an absent reference is
/// [`ProcessEnvRefusal::Unpublished`].
///
/// # Errors
///
/// [`HoldEnvError::Refused`] for a typed refusal, and
/// [`HoldEnvError::Fault`] for every store failure, a fenced claim among
/// them.
pub async fn hold_process_env(
    ports: &ProcessEnvPorts<'_>,
    claim: &ReferrerClaim,
    selection: &ProcessEnvSelection,
) -> Result<ProcessEnvRef, HoldEnvError> {
    match selection {
        ProcessEnvSelection::New(env) => {
            let Some(registry) = ports.components else {
                return Err(HoldEnvError::Fault(PluginError::Session(
                    "this start path admits no new process environment".into(),
                )));
            };
            registry.admit(env).map_err(HoldEnvError::Refused)?;
            provision_component_writers(registry, ports.store, env)
                .await
                .map_err(|error| HoldEnvError::Fault(error.into()))?;
            let bytes = env.to_store_bytes().map_err(|error| {
                HoldEnvError::Fault(PluginError::Session(format!(
                    "failed to encode process environment: {error}"
                )))
            })?;
            let env_ref = process_env_ref_for_bytes(&bytes);
            ports
                .store
                .publish_process_env(claim, &env_ref, &bytes)
                .await
                .map_err(|error| publish_error(error, env))?;
            Ok(env_ref)
        }
        ProcessEnvSelection::Published(env_ref) => {
            match ports.store.acquire_process_env(claim, env_ref).await {
                Ok(()) => Ok(env_ref.clone()),
                Err(ArtifactStoreError::ArtifactMissing { .. }) => {
                    Err(HoldEnvError::Refused(ProcessEnvRefusal::Unpublished {
                        env_ref: env_ref.to_string(),
                    }))
                }
                Err(error) => Err(HoldEnvError::Fault(error.into())),
            }
        }
    }
}

/// Provision the fleet writer ranges of `registry`'s components in the
/// record `store` publishes against, when it does not name one `env` names
/// (§3.3 step 8): the components get the
/// provisioning session plugins get from their declarations, so a component
/// writes past format 1 once the fleet permits it, and only its oldest
/// writable format inside a rollback window. A recorded range is never moved.
async fn provision_component_writers(
    registry: &ProcessComponentRegistry,
    store: &dyn ProcessEnvStore,
    env: &ProcessEnv,
) -> Result<(), crate::StoreError> {
    let ranges = store.plugin_writers().await?;
    if env
        .components()
        .iter()
        .any(|entry| ranges.permitted_writer(&entry.id().writer_key()).is_err())
    {
        store
            .provision_plugin_writers(&registry.writer_registrations())
            .await?;
    }
    Ok(())
}

/// A publish the fleet writer record refused is the environment's typed
/// refusal, naming the config format `env` states for the component; every
/// other store failure is a fault.
fn publish_error(error: ArtifactStoreError, env: &ProcessEnv) -> HoldEnvError {
    use crate::compat::CompatRefusal;
    let plugin = match &error {
        ArtifactStoreError::Incompatible {
            refusal:
                CompatRefusal::PluginWriterOutsideRange { plugin, .. }
                | CompatRefusal::PluginWriterUnprovisioned { plugin },
        } => Some(plugin),
        _ => None,
    };
    let refused = plugin.and_then(|plugin| {
        env.components()
            .iter()
            .find(|entry| &entry.id().writer_key() == plugin)
    });
    match refused {
        Some(entry) => HoldEnvError::Refused(ProcessEnvRefusal::ConfigWriterOutsideFleetRange {
            id: entry.id().to_string(),
            format: entry.config().format_version().get(),
        }),
        None => HoldEnvError::Fault(error.into()),
    }
}

/// Hold the environment `choice` states under the execution claim of
/// `scope`, with its settings checked against the `kind` engine for
/// `payload`, and its links checked under `choice.fits` against the engine's
/// performed effects and serving components before sealing the launch. `Restrict` holds `enclosing`'s environment restricted to the
/// effects `payload` performs, and is refused outside a process.
pub(crate) async fn hold_launch(
    engines: &super::ProcessEngineRegistry,
    scope: &crate::ExecutionScope,
    enclosing: Option<&super::EnclosingProcess>,
    choice: &super::ProcessStartChoice,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<super::EngineLaunch, HoldEnvError> {
    let performed = engines
        .performed_effects(kind, payload)
        .await
        .map_err(HoldEnvError::Refused)?;
    hold_launch_performing(engines, scope, enclosing, choice, kind, payload, &performed).await
}

/// [`hold_launch`] for a start whose engine already said what it performs:
/// one carried inline, whose payload no store holds yet.
pub(crate) async fn hold_launch_performing(
    engines: &super::ProcessEngineRegistry,
    scope: &crate::ExecutionScope,
    enclosing: Option<&super::EnclosingProcess>,
    choice: &super::ProcessStartChoice,
    kind: &str,
    payload: &serde_json::Value,
    performed: &super::PerformedEffects,
) -> Result<super::EngineLaunch, HoldEnvError> {
    engines
        .check_settings(kind, payload, choice.engine_settings.as_ref())
        .map_err(|error| settings_error(kind, error))?;
    let selection = match &choice.env {
        super::ProcessEnvChoice::Restrict => {
            let Some(enclosing) = enclosing else {
                return Err(HoldEnvError::Refused(ProcessEnvRefusal::Policy {
                    reason: "a start outside a process has no environment to restrict".into(),
                }));
            };
            if performed.keys().eq(enclosing.env.effects().keys()) {
                ProcessEnvSelection::Published(enclosing.env_ref.clone())
            } else {
                // A new env names no retired component: the child leaves
                // out what its sealed parent still names (FIG-6296).
                ProcessEnvSelection::New(
                    engines.components().without_retired(
                        enclosing
                            .env
                            .restrict(performed.keys())
                            .map_err(HoldEnvError::Refused)?,
                    ),
                )
            }
        }
        super::ProcessEnvChoice::Published(env_ref) => {
            ProcessEnvSelection::Published(env_ref.clone())
        }
        super::ProcessEnvChoice::New(env) => ProcessEnvSelection::New(env.clone()),
    };
    let ports = || {
        engines.env_ports().ok_or_else(|| {
            HoldEnvError::Fault(PluginError::Session(
                "a process start requires the runtime's artifact stores".into(),
            ))
        })
    };
    let (env, held) = match &selection {
        ProcessEnvSelection::New(env) => (env.clone(), None),
        ProcessEnvSelection::Published(env_ref) => {
            let ports = ports()?;
            let claim = crate::session::execution_claim_of(scope).map_err(HoldEnvError::Fault)?;
            let held = hold_process_env(&ports, &claim, &selection).await?;
            let env = lash_execution_contracts::process::load_process_env(ports.store, env_ref)
                .await
                .map_err(|error| HoldEnvError::Fault(error.into()))?;
            (env, Some(held))
        }
    };
    let linked = engines
        .link_package_namespaces(kind, payload, env.clone())
        .await
        .map_err(HoldEnvError::Fault)?;
    let (selection, held) = if linked == env {
        (selection, held)
    } else {
        (ProcessEnvSelection::New(linked.clone()), None)
    };
    engines
        .check_links(
            kind,
            payload,
            &std::sync::Arc::new(linked),
            performed,
            &choice.fits,
        )
        .await
        .map_err(HoldEnvError::Refused)?;
    let env_ref = match held {
        Some(env_ref) => env_ref,
        None => {
            let ports = ports()?;
            let claim = crate::session::execution_claim_of(scope).map_err(HoldEnvError::Fault)?;
            hold_process_env(&ports, &claim, &selection).await?
        }
    };
    Ok(lash_execution_contracts::process::EngineLaunch::new(
        env_ref,
        choice.engine_settings.clone(),
    ))
}

/// An engine that refuses the stated settings refuses the start; an engine
/// this build does not serve is a fault another build may not meet.
pub(crate) fn settings_error(kind: &str, error: PluginError) -> HoldEnvError {
    if error.class() == lash_execution_contracts::plugin_error::PluginErrorClass::Terminal {
        HoldEnvError::Refused(ProcessEnvRefusal::SettingsRefused {
            engine: kind.to_owned(),
            reason: error.to_string(),
        })
    } else {
        HoldEnvError::Fault(error)
    }
}
