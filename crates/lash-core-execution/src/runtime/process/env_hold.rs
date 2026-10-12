//! The one path a process environment becomes held for a start (D10;
//! final-spec §3.3).
//!
//! A new environment is admitted against the installed process components
//! and then published under the starter's claim; a published one is
//! acquired under it. Nothing else publishes or acquires a process
//! environment: host pins, model starts, start staging, isolated tool runs
//! and nested starts all come through [`hold_process_env`].

use lash_core_store::process_env::{ProcessEnv, ProcessEnvRefusal};

use super::model::{ProcessEnvRef, ProcessEnvStore, process_env_ref_for_bytes};
use crate::plugin::ProcessComponentRegistry;
use crate::{ArtifactStoreError, PluginError, ReferrerClaim};

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

/// What a hold reads and writes through: the environment store, and the
/// installed components a new environment is admitted against.
#[derive(Clone, Copy)]
pub struct ProcessEnvPorts<'a> {
    pub store: &'a dyn ProcessEnvStore,
    pub components: Option<&'a ProcessComponentRegistry>,
}

impl<'a> ProcessEnvPorts<'a> {
    /// Ports that only acquire published environments.
    pub fn published_only(store: &'a dyn ProcessEnvStore) -> Self {
        Self {
            store,
            components: None,
        }
    }

    /// Ports that admit new environments against `registry`.
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
/// `build(process: None)`, a composable set), then publishes; the store
/// admits each component's config format against the fleet writer record
/// under `component:<id>`. A refused environment stores nothing.
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
                .map_err(publish_error)?;
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

/// A publish the fleet writer record refused is the environment's typed
/// refusal; every other store failure is a fault.
fn publish_error(error: ArtifactStoreError) -> HoldEnvError {
    use crate::compat::CompatRefusal;
    let refused = match &error {
        ArtifactStoreError::Incompatible {
            refusal: CompatRefusal::PluginWriterOutsideRange { plugin, writer, .. },
        } => Some((plugin, *writer)),
        ArtifactStoreError::Incompatible {
            refusal: CompatRefusal::PluginWriterUnprovisioned { plugin },
        } => Some((plugin, 0)),
        _ => None,
    };
    match refused.and_then(|(plugin, format)| {
        plugin
            .strip_prefix("component:")
            .map(|id| (id.to_owned(), format))
    }) {
        Some((id, format)) => {
            HoldEnvError::Refused(ProcessEnvRefusal::ConfigWriterOutsideFleetRange { id, format })
        }
        None => HoldEnvError::Fault(error.into()),
    }
}

/// Hold the environment `choice` states under the execution claim of
/// `scope`, with its settings checked against the `kind` engine for
/// `payload`: the sealed launch of a start declared or bound inside that
/// execution. `Inherit` holds `enclosing`'s environment, and is refused
/// outside a process.
pub(crate) async fn hold_launch(
    engines: &super::ProcessEngineRegistry,
    scope: &crate::ExecutionScope,
    enclosing: Option<&super::EnclosingProcess>,
    choice: &super::ProcessStartChoice,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<super::EngineLaunch, HoldEnvError> {
    let selection = match &choice.env {
        super::ProcessEnvChoice::Inherit => match enclosing {
            Some(enclosing) => ProcessEnvSelection::Published(enclosing.env_ref.clone()),
            None => {
                return Err(HoldEnvError::Refused(ProcessEnvRefusal::Policy {
                    reason: "a start outside a process has no environment to inherit".into(),
                }));
            }
        },
        super::ProcessEnvChoice::Published(env_ref) => {
            ProcessEnvSelection::Published(env_ref.clone())
        }
        super::ProcessEnvChoice::New(env) => ProcessEnvSelection::New(env.clone()),
    };
    engines
        .check_settings(kind, payload, choice.engine_settings.as_ref())
        .map_err(|error| settings_error(kind, error))?;
    let ports = engines.env_ports().ok_or_else(|| {
        HoldEnvError::Fault(PluginError::Session(
            "a process start requires the runtime's artifact stores".into(),
        ))
    })?;
    let claim = crate::session::execution_claim_of(scope).map_err(HoldEnvError::Fault)?;
    let env_ref = hold_process_env(&ports, &claim, &selection).await?;
    Ok(super::EngineLaunch::new(
        env_ref,
        choice.engine_settings.clone(),
    ))
}

/// An engine that refuses the stated settings refuses the start; an engine
/// this build does not serve is a fault another build may not meet.
pub(crate) fn settings_error(kind: &str, error: PluginError) -> HoldEnvError {
    if error.class() == crate::plugin::PluginErrorClass::Terminal {
        HoldEnvError::Refused(ProcessEnvRefusal::SettingsRefused {
            engine: kind.to_owned(),
            reason: error.to_string(),
        })
    } else {
        HoldEnvError::Fault(error)
    }
}
