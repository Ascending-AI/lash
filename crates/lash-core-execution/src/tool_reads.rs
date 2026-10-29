//! Declared reads (ADR 0144): what answers a call of a tool whose manifest
//! declares [`ExecutionPolicy::Read`](lash_kernel_value::tool_contract::ExecutionPolicy::Read) inside
//! a code cell.
//!
//! A provider that serves `Read` tools registers a reader beside itself
//! ([`ToolProviderRegistration::with_reads`]). A cell's read of one of
//! those tools runs inside the statement that performs it: the cell's host
//! hands the reader the tool, its arguments, the run's configuration, a
//! cancellation and a deadline, and the answer goes straight back into the
//! cell. The caller retains the read fact with its next saved state; a
//! crash before that save reads it
//! again, and the answer may differ.
//!
//! The reader is a narrow interface on purpose: it gets no attempt context,
//! no completion client, no intents and no admitted call identity. Its
//! live activity has only an observation identity derived from caller and ordinal. A provider whose call needs any of them declares its tool
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

/// Observe an unrecorded read on the ordinary tool-call activity lane.
/// This function alone emits the read start/return pair, never a completion.
/// The shared read runner supplies the admitted answer and caller-saved fact.
/// `project` borrows the answer only for observation and does not change it.
pub async fn observe_tool_read<T>(
    observe: Option<&(dyn Fn(crate::TurnEvent) + Send + Sync)>,
    mut read: crate::ToolReadMetadata,
    name: String,
    args: serde_json::Value,
    answer: impl std::future::Future<Output = T>,
    project: impl FnOnce(&T) -> (crate::ToolCallOutput, usize),
) -> T {
    let started = Instant::now();
    let call_id = read.call_id();
    if let Some(observe) = observe {
        observe(crate::TurnEvent::ToolCallStarted {
            execution: crate::ToolCallExecution::Read,
            read: Some(Box::new(read.clone())),
            call_id: call_id.clone(),
            provider_call_id: None,
            name: name.clone(),
            args: args.clone(),
            graph_key: read.graph_key(),
        });
    }
    let result = answer.await;
    if let Some(observe) = observe {
        let (output, answer_bytes) = project(&result);
        read.elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        read.answer_bytes = answer_bytes;
        observe(crate::TurnEvent::ToolCallReturned {
            execution: crate::ToolCallExecution::Read,
            call_id,
            provider_call_id: None,
            name,
            args,
            output,
            duration_ms: Some(read.elapsed_us / 1000),
            graph_key: read.graph_key(),
            read: Some(Box::new(read)),
        });
    }
    result
}

pub use lash_protocol_types::{ReadFailure, ReadResult};

/// Per-invocation inputs supplied by the saving owner.
pub struct ReadContext<'a> {
    pub history: Option<&'a crate::ReadHistory>,
    pub config: &'a OwnerConfigView,
    pub clock: &'a dyn crate::Clock,
    pub cancel: &'a CancellationToken,
    pub parent_remaining: std::time::Duration,
    pub metadata: crate::ToolReadMetadata,
    pub observe: Option<&'a (dyn Fn(crate::TurnEvent) + Send + Sync)>,
}

/// A resolved reader, or the refusal of the pinned route before provider entry.
pub enum ReadTarget<'a> {
    Ready(&'a ReadRoute),
    Refused { tool: String, failure: ReadFailure },
}

/// Execute a read once. Only its owner saves the returned fact.
/// The observation function is FIG-7547's sole read-event producer.
pub async fn run_read(
    target: ReadTarget<'_>,
    input: Result<serde_json::Value, ReadFailure>,
    context: ReadContext<'_>,
) -> ReadResult {
    let args = input.as_ref().cloned().unwrap_or(serde_json::Value::Null);
    let name = match &target {
        ReadTarget::Ready(route) => route.definition.manifest.id.to_string(),
        ReadTarget::Refused { tool, .. } => tool.clone(),
    };
    let read = context.metadata.clone();
    let answer = observe_tool_read(context.observe, read.clone(), name.clone(), args.clone(), async {
        let args = input?;
        let route = match target {
            ReadTarget::Ready(route) => route,
            ReadTarget::Refused { failure, .. } => return Err(failure),
        };
        if let Some(failure) = route.contract_changed() { return Err(ReadFailure::Tool(Box::new(failure.clone()))); }
        lash_vocabulary::validate_tool_input(&route.definition.contract, &args).map_err(|source| {
            ReadFailure::Tool(Box::new(crate::ToolFailure::runtime(crate::ToolFailureClass::InvalidRequest, "invalid_tool_args", source.to_string())
                .with_cause(crate::ToolFailureCause::ValueMismatch { source })))
        })?;
        let deadline = context.clock.now() + route.definition.manifest.execution().min(context.parent_remaining);
        use futures_util::FutureExt as _;
        let provider = std::panic::AssertUnwindSafe(async {
            route.read(context.history, &args, context.config, context.clock, context.cancel, deadline).await
        }).catch_unwind();
        let value = tokio::select! {
            biased;
            () = context.cancel.cancelled() => return Err(ReadFailure::Cancelled),
            () = context.clock.sleep_until(deadline) => return Err(ReadFailure::TimedOut),
            answer = provider => match answer {
                Ok(answer) => answer.map_err(|error| ReadFailure::Reader { message: error.message, data: error.data })?,
                Err(payload) => return Err(ReadFailure::Panicked(lash_protocol_types::ReadPanic {
                    tool_name: name.clone(), message: crate::panic_containment::payload_message(payload.as_ref()),
                })),
            },
        };
        admit_read_output(&route.definition, &args, value)
    }, |answer| match answer {
        Ok(value) => (crate::ToolCallOutput::success(value.clone()), value.to_string().len()),
        Err(failure) => (failure.output(), 0),
    }).await;
    let output = match &answer {
        Ok(value) => crate::ToolCallOutput::success(value.clone()),
        Err(failure) => failure.output(),
    };
    ReadResult {
        fact: lash_protocol_types::ReadFact {
            call_id: read.call_id(),
            caller: read.caller,
            ordinal: read.ordinal,
            provider: read.provider,
            tool: name,
            args,
            outcome: output.outcome,
            site: read.site,
            origin: read.origin,
        },
        answer,
    }
}

fn admit_read_output(
    definition: &ToolDefinition,
    args: &serde_json::Value,
    value: serde_json::Value,
) -> Result<serde_json::Value, ReadFailure> {
    crate::tool_dispatch::output::settle(
        crate::ToolCallOutput::success(value.clone()),
        definition.manifest.declaration(),
        &definition.contract,
        args,
        &definition.manifest.tool(),
    )
    .map_err(ReadFailure::Tool)?;
    Ok(value)
}
