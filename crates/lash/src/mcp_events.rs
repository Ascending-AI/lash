//! The core port MCP event subscriptions reach lash through: keyed process
//! starts and cancels, keyed sends, argument checks, and the reads of a
//! subscription's status and resume key.
//!
//! A host binds its plugin's handle once its core is built:
//! `core.bind_mcp_events(&factory.events())`. The plugin's shutdown unbinds
//! it, so the core and the plugin it holds do not keep each other alive.

use std::sync::Arc;

use lash_core::{
    ArgsMismatch, ArgsMode, EngineLaunch, EngineStatus, LifetimeDecision, ProcessDefinition,
    ProcessId, ProcessStartRequest, Resolution, ResolveAnswer, SessionId, TurnId, TurnInput,
};
use lash_plugin_mcp::events::{MCP_EVENTS_ENGINE, McpEvents, McpEventsCore, McpEventsPortError};
use serde_json::{Map, Value};

use crate::{EmbedError, LashCore};

/// How many subscriptions one roster page reads.
const ROSTER_PAGE: std::num::NonZeroUsize = std::num::NonZeroUsize::new(256).unwrap();

impl LashCore {
    /// Bind `events`, an MCP plugin's event handle, to this core: its
    /// subscriptions start, deliver and read through it from now on.
    pub fn bind_mcp_events(&self, events: &McpEvents) {
        events.bind(Arc::new(CorePort {
            core: self.clone(),
            pin: crate::process::HostArtifactPin::mint(),
            launch: tokio::sync::OnceCell::new(),
        }));
    }
}

struct CorePort {
    core: LashCore,
    /// The pin the subscriptions' environment is published under.
    pin: crate::process::HostArtifactPin,
    launch: tokio::sync::OnceCell<EngineLaunch>,
}

/// A lash error as the port classes it.
fn port_error(error: EmbedError) -> McpEventsPortError {
    let message = error.to_string();
    match &error {
        EmbedError::Store(
            lash_core::StoreError::PendingTurnInputIdConflict { .. }
            | lash_core::StoreError::PendingTurnInputSourceKeyConflict { .. },
        )
        | EmbedError::Plugin(lash_core::PluginError::StartKeyConflict { .. }) => {
            McpEventsPortError::Conflict { message }
        }
        _ if error.is_retryable() => McpEventsPortError::Unavailable { message },
        _ => McpEventsPortError::Refused { message },
    }
}

impl CorePort {
    /// The context a host operation of the subscriptions runs under.
    async fn operation(
        &self,
        name: &str,
    ) -> Result<crate::runtime::ActorContext, McpEventsPortError> {
        self.core
            .session_administration()
            .await
            .effect_host()
            .scoped(crate::runtime::AdmittedScope::runtime_operation(format!(
                "mcp-events:{name}"
            )))
            .map_err(|error| port_error(EmbedError::from(error)))
    }
}

#[async_trait::async_trait]
impl McpEventsCore for CorePort {
    async fn subscription_launch(&self) -> Result<EngineLaunch, McpEventsPortError> {
        self.launch
            .get_or_try_init(|| async {
                let env = crate::process::ProcessEnv::builder(crate::MaxToolCalls::new(1))
                    .build()
                    .map_err(|refusal| McpEventsPortError::Refused {
                        message: refusal.to_string(),
                    })?;
                let env_ref = self
                    .core
                    .host_artifacts()
                    .publish_process_env(&self.pin, &env)
                    .await
                    .map_err(port_error)?;
                Ok(EngineLaunch::new(env_ref, None))
            })
            .await
            .cloned()
    }

    async fn session_lifetime(
        &self,
        session: &SessionId,
    ) -> Result<LifetimeDecision, McpEventsPortError> {
        let scope = self
            .core
            .processes()
            .session_scope(session)
            .await
            .map_err(port_error)?;
        Ok(crate::process::Lifetime::Until(scope).into())
    }

    async fn start(&self, request: ProcessStartRequest) -> Result<ProcessId, McpEventsPortError> {
        let operation = self.operation("start").await?;
        let receipt = self
            .core
            .processes()
            .start(request, operation)
            .await
            .map_err(port_error)?;
        Ok(receipt.process_id)
    }

    async fn cancel(&self, process: &ProcessId) -> Result<(), McpEventsPortError> {
        let operation = self.operation("cancel").await?;
        self.core
            .processes()
            .cancel(process, operation)
            .await
            .map_err(port_error)?;
        Ok(())
    }

    async fn send(
        &self,
        session: &SessionId,
        turn: TurnId,
        input: TurnInput,
    ) -> Result<(), McpEventsPortError> {
        let durable = self
            .core
            .session(session.clone())
            .durable()
            .await
            .map_err(port_error)?;
        durable.send(input).id(turn).await.map_err(port_error)?;
        Ok(())
    }

    async fn check_args(
        &self,
        definition: &ProcessDefinition,
        args: &Map<String, Value>,
        mode: ArgsMode,
    ) -> Result<Result<(), ArgsMismatch>, McpEventsPortError> {
        self.core
            .process_definitions()
            .check_args(definition, args, mode)
            .await
            .map_err(port_error)
    }

    async fn subscriptions(
        &self,
        observer: Option<&SessionId>,
        ended: bool,
    ) -> Result<Vec<ProcessId>, McpEventsPortError> {
        let filter = lash_core::ProcessListFilter {
            identity_kind: Some(MCP_EVENTS_ENGINE.to_owned()),
            status: if ended {
                lash_core::ProcessStatusFilter::Any
            } else {
                lash_core::ProcessStatusFilter::default()
            },
            ..lash_core::ProcessListFilter::default()
        };
        let processes = self.core.processes();
        if let Some(observer) = observer {
            let observed = processes
                .list_observed_by(&lash_core::SessionScope::new(observer.clone()), &filter)
                .await
                .map_err(port_error)?;
            return Ok(observed
                .into_iter()
                .map(|process| process.process_id)
                .collect());
        }
        let mut out = Vec::new();
        let mut continuation = None;
        loop {
            let page = processes
                .list(&filter, ROSTER_PAGE, continuation)
                .await
                .map_err(port_error)?;
            out.extend(page.processes.into_iter().map(|process| process.process_id));
            match page.continuation {
                Some(next) => continuation = Some(next),
                None => return Ok(out),
            }
        }
    }

    async fn engine_status(
        &self,
        process: &ProcessId,
    ) -> Result<Option<EngineStatus>, McpEventsPortError> {
        self.core
            .processes()
            .engine_status(process)
            .await
            .map_err(port_error)
    }

    async fn pinned_keys(
        &self,
        process: &ProcessId,
    ) -> Result<Vec<lash_core::waits::PinnedEngineKey>, McpEventsPortError> {
        self.core
            .completions()
            .pinned_keys(process)
            .await
            .map_err(port_error)
    }

    async fn resolve(
        &self,
        key: &str,
        resolution: Resolution,
    ) -> Result<ResolveAnswer, McpEventsPortError> {
        self.core
            .completions()
            .resolve(key, resolution)
            .await
            .map_err(port_error)
    }
}
