use crate::support::{EmbedError, LashCore, Result};
use lash_protocol_types::ProcessId;
// `PluginQuery` / `PluginCommand` / `PluginTask` bound the plugin operation runners, but their home is `crate::plugins`: authoring surface a plugin
// implements, not a name a host writes to invoke one (ADR 0051, FIG-1921).
pub use lash_core::facade_support::AcceptedInjectedTurnInput;
pub use lash_core::waits::PinnedEngineKey;
pub use lash_core::{CallOwner, ParkedCall};

#[derive(Clone)]
pub struct Completions {
    pub(crate) core: LashCore,
}

impl Completions {
    /// Snapshot the owner's pending admitted calls and their pinned identities and deadlines.
    ///
    /// Returned keys are bearer capabilities. The host authorizes this read and
    /// shares keys only with trusted callers. A concurrent resolver may settle a
    /// returned key, so callers handle `AlreadyResolved`, `Conflict` and `Revoked`.
    pub async fn parked(&self, owner: CallOwner) -> Result<Vec<ParkedCall>> {
        lash_core::parked(self.core.env.core.control.effect_host.backend(), owner)
            .await
            .map_err(EmbedError::from)
    }

    /// Snapshot the pending keys `process`'s engine pinned, with the name it pinned each under
    /// and its deadline.
    ///
    /// The keys are read from the process's durable waits, so any node answers, before and
    /// after a restart or a handover. They are bearer capabilities under the same rules as
    /// [`parked`](Self::parked).
    pub async fn pinned_keys(&self, process: &ProcessId) -> Result<Vec<PinnedEngineKey>> {
        lash_core::waits::pinned_keys(self.core.env.core.control.effect_host.backend(), process)
            .await
            .map_err(EmbedError::from)
    }

    /// Resolve the wait `key` names, first writer wins.
    ///
    /// A completion key is its wait's random 128-bit id, and a bearer
    /// capability: whoever holds it may resolve its wait. Lash keeps no
    /// completion secret and does not decide who may call this; the host
    /// authenticates and authorizes its callers (its API authentication, its
    /// webhook signatures) and hands a key only to callers it has
    /// authorized.
    ///
    /// A key that names no wait answers `Unknown`; a wait that was revoked or
    /// timed out answers `Revoked`; a key of a kind a host may not resolve
    /// (anything but `tool_completion` and `engine_key`) answers `ReservedKind`.
    /// None of them writes anything.
    pub async fn resolve(
        &self,
        key: &str,
        resolution: lash_core::Resolution,
    ) -> Result<lash_core::ResolveAnswer> {
        let backend = self.core.env.core.control.effect_host.backend();
        lash_core::waits::resolve_host(backend, key, resolution)
            .await
            .map_err(|error| durable_error(error.to_string()))
    }
}

fn durable_error(message: String) -> EmbedError {
    EmbedError::Runtime(lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::ToolCompletionResolve,
        message,
    ))
}

/// A host mutation either applied or was durably accepted for later settlement.
/// A pending receipt can be followed through [`SessionCommandAdmin::settle`].
#[derive(Clone, Debug, PartialEq)]
pub enum AdminMutation<T> {
    Applied(T),
    Pending(lash_core::runtime::SessionCommandReceipt),
}

impl<T> AdminMutation<T> {
    /// Explicitly await this mutation, decoding its recorded command outcome.
    /// The host can wrap this wait in its own timeout; dropping it withdraws nothing.
    pub async fn settle_with(
        self,
        commands: &SessionCommandAdmin,
        decode: impl FnOnce(lash_core::runtime::SessionCommandOutcome) -> Result<T>,
    ) -> Result<T> {
        match self {
            Self::Applied(value) => Ok(value),
            Self::Pending(receipt) => match commands.settle(receipt).await? {
                lash_core::runtime::SessionCommandSettlement::Applied { outcome, .. } => {
                    decode(outcome)
                }
                settlement => Err(unsettled_command_error(settlement)),
            },
        }
    }

    fn try_map<U>(self, map: impl FnOnce(T) -> Result<U>) -> Result<AdminMutation<U>> {
        match self {
            Self::Applied(value) => map(value).map(AdminMutation::Applied),
            Self::Pending(receipt) => Ok(AdminMutation::Pending(receipt)),
        }
    }
}

#[derive(Clone)]
/// Accessor for the session administration capabilities.
pub struct SessionAdmin {
    pub(crate) context: AdminContext,
    pub(crate) processes: SessionProcessAdmin,
}

impl SessionAdmin {
    pub fn config(&self) -> SessionConfigAdmin {
        SessionConfigAdmin {
            context: self.context.clone(),
        }
    }

    /// The session's prompt sections: the recorded plan, the registered
    /// catalog and an unadmitted preview of their resolution (ADR 0133).
    pub fn prompt(&self) -> SessionPromptAdmin {
        SessionPromptAdmin {
            context: self.context.clone(),
        }
    }

    pub fn commands(&self) -> SessionCommandAdmin {
        SessionCommandAdmin {
            context: self.context.clone(),
        }
    }

    pub fn state(&self) -> SessionStateAdmin {
        SessionStateAdmin {
            context: self.context.clone(),
        }
    }

    /// Returns the turn-input injection administration facade.
    pub fn injection(&self) -> InjectionAdmin {
        InjectionAdmin {
            context: self.context.clone(),
            trace: lash_core::TraceCall::default(),
        }
    }

    pub fn protocol(&self) -> ProtocolAdmin {
        ProtocolAdmin {
            context: self.context.clone(),
        }
    }

    pub fn processes(&self) -> SessionProcessAdmin {
        self.processes.clone()
    }

    /// The tools the session's providers offer under its current config,
    /// by model-facing name: what its next run is offered before that run's
    /// own options. It reads the live providers and records nothing; which
    /// tools a provider offers is that provider's config, and a committed
    /// run keeps the surface it recorded.
    pub async fn offered_tools(&self) -> Result<Vec<lash_core::ToolManifest>> {
        self.context
            .with_writer(async |runtime: &mut crate::support::LashRuntime| {
                runtime.reload_invalidated_resident_session_state().await?;
                let resolved = runtime.offered_tool_surface()?;
                Ok(resolved
                    .surface
                    .definitions()
                    .map(|definition| definition.manifest.clone())
                    .collect())
            })
            .await
    }
}

mod context;
pub(crate) use context::AdminContext;
pub(crate) mod config_transactions;
use config_transactions::SessionConfigAdmin;
pub(crate) mod host_commands;
use host_commands::{SessionCommandAdmin, unsettled_command_error};
pub(crate) mod process_admin;
use process_admin::SessionProcessAdmin;
pub(crate) mod prompt;
pub(crate) use prompt::SessionPromptAdmin;
mod state;
pub use state::SessionStateAdmin;
mod injection;
pub use injection::InjectionAdmin;
mod protocol;
pub use protocol::ProtocolAdmin;
mod plugin_operations;
pub use plugin_operations::{PluginOperations, PluginTaskResultError};
