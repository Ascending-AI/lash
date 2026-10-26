//! Declared reads (ADR 0144): what answers a call of a tool whose manifest
//! declares [`ExecutionPolicy::Read`](lash_kernel_value::tool_contract::ExecutionPolicy::Read) inside
//! a code cell.
//!
//! A provider that serves `Read` tools registers a reader beside itself
//! ([`ToolProviderRegistration::with_reads`]). A cell's read of one of
//! those tools runs inside the statement that performs it: the cell's host
//! hands the reader the tool, its arguments, the run's configuration, a
//! cancellation and a deadline, and the answer goes straight back into the
//! cell. Nothing about the call is recorded. The answer survives only as
//! part of the cell's next saved state; a crash before that save reads it
//! again, and the answer may differ.
//!
//! The reader is a narrow interface on purpose: it gets no attempt context,
//! no completion client, no intents and no call identity, since a read has
//! none of them. A provider whose call needs any of them declares its tool
//! `Once` or `Repeatable`.

use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::ProviderKey;
use crate::ToolProvider;
use crate::plugin::OwnerConfigView;
use lash_vocabulary::tool_contract::ToolDefinition;
use lash_vocabulary::tool_contract::ToolManifest;

/// One read a cell performs: the tool as its run's surface retained it, the
/// arguments it projected from the guest's, the run's configuration, and
/// what bounds it.
pub struct ReadCall<'a> {
    /// The cell's immutable run history; absent for workflow reads.
    pub history: Option<&'a crate::ReadHistory>,
    pub clock: &'a dyn lash_core_ids::clock::Clock,
    /// The tool's manifest as the run's surface retained it.
    pub manifest: &'a ToolManifest,
    /// The tool's arguments, projected from the guest's as a recorded
    /// call's are.
    pub args: &'a serde_json::Value,
    /// The configuration the run was resolved under: what the provider
    /// advertised the tool under.
    pub config: &'a OwnerConfigView,
    /// Cancelled when the run stops or its owner loses the run.
    pub cancel: &'a CancellationToken,
    /// When the read must have answered: the sooner of the tool's
    /// execution bound and how long the cell's worker waits on its host.
    pub deadline: Instant,
}

/// A read's own failure: raised in the guest at its `perform`, which may
/// catch it. It is answered once; nothing retries it.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct ReadError {
    pub message: String,
    /// What the guest's error carries as its data.
    pub data: serde_json::Value,
}

impl ReadError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            data: serde_json::Value::Null,
        }
    }

    /// This error, carrying `data` to the guest.
    #[must_use]
    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = data;
        self
    }
}

/// What answers the calls of a provider's `Read` tools inside a code cell.
/// It must change nothing outside: after a crash, a read the cell had not
/// saved is made again, and its answer may differ.
#[async_trait::async_trait]
pub trait ToolReads: Send + Sync {
    /// One read: the tool's result as JSON, or its failure.
    async fn read(&self, call: ReadCall<'_>) -> Result<serde_json::Value, ReadError>;
}

/// A tool provider as a plugin registers it, with the reader its `Read`
/// tools need.
#[derive(Clone)]
pub struct ToolProviderRegistration {
    pub(crate) provider: Arc<dyn ToolProvider>,
    pub(crate) reads: Option<Arc<dyn ToolReads>>,
}

impl ToolProviderRegistration {
    /// `provider`, with no reader yet.
    #[must_use]
    pub fn new(provider: Arc<dyn ToolProvider>) -> Self {
        Self {
            provider,
            reads: None,
        }
    }

    /// This registration with `reads` answering its `Read` tools.
    #[must_use]
    pub fn with_reads(mut self, reads: Arc<dyn ToolReads>) -> Self {
        self.reads = Some(reads);
        self
    }
}

/// Where a read of one tool goes on this node: the definition its run's
/// surface retained, and the reader the provider that entry names
/// registered here.
#[derive(Clone)]
pub struct ReadRoute {
    /// The provider the run's surface retained for the tool.
    pub provider: ProviderKey,
    /// The tool as the run's surface retained it.
    pub definition: ToolDefinition,
    reads: Arc<dyn ToolReads>,
    /// The failure every read raises when the provider serves the tool
    /// under another contract than the run pinned: nothing is read.
    contract_changed: Option<crate::ToolFailure>,
}

impl ReadRoute {
    pub(crate) fn new(
        provider: ProviderKey,
        definition: ToolDefinition,
        reads: Arc<dyn ToolReads>,
    ) -> Self {
        Self {
            provider,
            definition,
            reads,
            contract_changed: None,
        }
    }

    /// This route, whose reads raise `contract_changed` when it is set.
    pub(crate) fn with_drift(mut self, contract_changed: Option<crate::ToolFailure>) -> Self {
        self.contract_changed = contract_changed;
        self
    }

    /// The `operation_contract_changed` failure a read through this route
    /// raises, when the provider serves the tool under another contract
    /// than the run pinned.
    pub fn contract_changed(&self) -> Option<&crate::ToolFailure> {
        self.contract_changed.as_ref()
    }

    /// One read of the tool with `args`, under `config`, bounded by
    /// `cancel` and `deadline`.
    ///
    /// # Errors
    ///
    /// The read's own failure.
    pub async fn read(
        &self,
        history: Option<&crate::ReadHistory>,
        args: &serde_json::Value,
        config: &OwnerConfigView,
        clock: &dyn lash_core_ids::clock::Clock,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<serde_json::Value, ReadError> {
        self.reads
            .read(ReadCall {
                history,
                clock,
                manifest: &self.definition.manifest,
                args,
                config,
                cancel,
                deadline,
            })
            .await
    }
}
