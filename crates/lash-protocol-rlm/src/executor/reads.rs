//! A cell's planned reads (ADR 0144): the `perform`s its read plan binds,
//! answered inside the statement that makes them.
//!
//! A read is no admitted execution. Its call identity is observational; it mints no wait,
//! Its caller saves its fact. It runs no hook: the machine asks its host, the host
//! asks the reader the plan's provider registered, and the answer goes
//! straight back into the statement. What survives of it is what the
//! cell's next save holds; a crash before that save makes the read again,
//! and its answer may differ.
//!
//! The host answers a read the way the cell would see the same call
//! recorded: its arguments are the guest's one input record under the
//! call projection, validated against the tool's input schema; a failure,
//! `operation_contract_changed` included, is a `tool_failed` error carrying
//! its data; and every read counts
//! against `max_tool_calls` beside the cell's recorded calls, in the count
//! the cell's saved state holds. A read is bounded by the sooner of its
//! tool's execution bound and how long the worker waits on its parent, and
//! is stopped when the cell's run is.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lash_kernel_doc::{Datum, ErrorDatum};

use lash_kernel_value::names::OperationUse;
use lash_protocol_types::sync::MutexExt;
use lash_vm_broker::ParentFault;
use lash_vm_runtime::EffectReads;

use super::host::{CellArgs, CellHostLedgers, TOOL_CALL_LIMIT, refused};

/// The reads one activation of a cell answers.
pub(super) struct CellReads {
    /// Each planned effect's route on this node.
    pub routes: BTreeMap<OperationUse, lash_core::ReadRoute>,
    pub history: Arc<lash_core::ReadHistory>,
    pub config: lash_core::plugin::OwnerConfigView,
    pub clock: Arc<dyn lash_core::Clock>,
    /// Cancelled when the cell's run is stopped.
    pub cancel: lash_core::CancellationToken,
    pub ledgers: Arc<Mutex<CellHostLedgers>>,
    pub limit: lash_core::MaxToolCalls,
    /// The cell, as its observations name it.
    pub caller: lash_core::ToolReadCaller,
    pub observe: Arc<dyn Fn(lash_core::TurnEvent) + Send + Sync>,
}

impl CellReads {
    /// Counts one read against `max_tool_calls`: its ordinal among the
    /// cell's reads, or the refusal its `perform` raises.
    fn count(&self) -> (u64, Result<(), ErrorDatum>) {
        let mut ledgers = self.ledgers.lock_recover();
        let counted = ledgers.counted();
        ledgers.reads += 1;
        if counted.saturating_add(1) > self.limit.get() {
            let exceeded = lash_core::ToolCallLimitExceeded {
                scope: lash_core::ToolCallLimitScope::Cell,
                limit: self.limit,
                counted,
                requested: 1,
            };
            ledgers.tool_call_limit.get_or_insert(exceeded);
            return (
                ledgers.reads,
                Err(refused(TOOL_CALL_LIMIT, exceeded.to_string())),
            );
        }
        (ledgers.reads, Ok(()))
    }
}

#[async_trait::async_trait]
impl EffectReads for CellReads {
    async fn perform(
        &self,
        effect: &OperationUse,
        args: &[Datum],
        site: &lash_kernel_value::sites::PositionLabel,
        origin: &lash_kernel_value::sites::Site,
        parent_remaining: std::time::Duration,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault> {
        // The plan names every effect the machine binds as a read, and the
        // activation routed each before the run started.
        let route = self.routes.get(effect).ok_or_else(|| {
            ParentFault(format!(
                "the cell performs `{effect}` as a read it did not plan"
            ))
        })?;
        use lash_core::facade_support::{ReadContext, ReadTarget, run_read};
        use lash_protocol_types::ReadFailure;
        let (ordinal, count) = self.count();
        let input = CellArgs::Direct
            .args(args)
            .map_err(|refusal| ReadFailure::Arguments(refusal.error(effect).message));
        let metadata = lash_core::ToolReadMetadata {
            caller: self.caller.clone(),
            provider: route.provider.clone(),
            ordinal,
            site: site.to_string(),
            origin: origin.to_string(),
            elapsed_us: 0,
            args_bytes: input.as_ref().map_or(0, |args| args.to_string().len()),
            answer_bytes: 0,
        };
        let target = match count {
            Ok(()) => ReadTarget::Ready(route),
            Err(error) => ReadTarget::Refused {
                tool: route.definition.manifest.id.to_string(),
                failure: ReadFailure::Tool(Box::new(lash_core::ToolFailure::new(
                    error.kind,
                    error.message,
                ))),
            },
        };
        let result = run_read(
            target,
            input,
            ReadContext {
                history: Some(&self.history),
                config: &self.config,
                clock: self.clock.as_ref(),
                cancel: &self.cancel,
                parent_remaining,
                metadata,
                observe: Some(self.observe.as_ref()),
            },
        )
        .await;
        self.ledgers.lock_recover().read_facts.push(result.fact);
        let signature = lash_kernel_value::json_schema::tool_signature(
            &route.definition.contract.input_schema.canonical,
            &route.definition.contract.output_schema.canonical,
        );
        match lash_vm_broker::kernel::value::read_answer(&signature.result, result.answer) {
            Ok(answer) => Ok(answer),
            Err(panic) => {
                self.ledgers.lock_recover().read_panic = Some(panic.clone());
                Err(ParentFault(panic.message))
            }
        }
    }
}
