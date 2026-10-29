//! A cell's planned reads (ADR 0144): the `perform`s its read plan binds,
//! answered inside the statement that makes them.
//!
//! A read is no admitted execution. Its call identity is observational; it mints no wait,
//! commits nothing, and runs no hook: the machine asks its host, the host
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
use std::time::Duration;

use lash_kernel_doc::{Datum, ErrorDatum};

use lash_kernel_value::names::OperationUse;
use lash_protocol_types::sync::MutexExt;
use lash_vm_broker::ParentFault;
use lash_vm_broker::kernel::{EFFECT_CANCELLED, EFFECT_TIMED_OUT, datum_from_json};
use lash_vm_runtime::EffectReads;

use super::host::{CellArgs, CellHostLedgers, TOOL_CALL_LIMIT, TOOL_FAILED, refused, tool_failed};

/// The reads one activation of a cell answers.
pub(super) struct CellReads {
    /// Each planned effect's route on this node.
    pub routes: BTreeMap<OperationUse, lash_core::ReadRoute>,
    pub history: Arc<lash_core::ReadHistory>,
    pub config: lash_core::plugin::OwnerConfigView,
    pub clock: Arc<dyn lash_core::Clock>,
    /// Cancelled when the cell's run is stopped.
    pub cancel: lash_core::CancellationToken,
    /// How long the worker waits on its parent's answer.
    pub parent_wait: Duration,
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

    async fn answer(
        &self,
        effect: &OperationUse,
        route: &lash_core::ReadRoute,
        args: &serde_json::Value,
    ) -> Result<(Datum, serde_json::Value), ErrorDatum> {
        let bound = route.definition.manifest.execution().min(self.parent_wait);
        let deadline = self.clock.now() + bound;
        let read = route.read(
            Some(&self.history),
            args,
            &self.config,
            self.clock.as_ref(),
            &self.cancel,
            deadline,
        );
        let value = tokio::select! {
            biased;
            () = self.cancel.cancelled() => {
                return Err(refused(
                    EFFECT_CANCELLED,
                    format!("the read of `{effect}` was stopped with the cell"),
                ));
            }
            () = self.clock.sleep_until(deadline) => {
                return Err(refused(
                    EFFECT_TIMED_OUT,
                    format!("the read of `{effect}` took longer than {bound:?}"),
                ));
            }
            answered = read => answered,
        };
        let value = value.map_err(|error| ErrorDatum {
            kind: TOOL_FAILED.to_owned(),
            message: error.message,
            data: datum_from_json(&error.data.to_string()).unwrap_or(Datum::Null),
        })?;
        let signature = lash_kernel_value::json_schema::tool_signature(
            &route.definition.contract.input_schema.canonical,
            &route.definition.contract.output_schema.canonical,
        );
        let datum = lash_vm_broker::kernel::value::datum_from_tool_json(&value, &signature.result)
            .map_err(|refusal| {
                refusal
                    .error_datum()
                    .unwrap_or_else(|error| refused("diagnostic_encoding", error.to_string()))
            })?;
        Ok((datum, value))
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
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault> {
        // The plan names every effect the machine binds as a read, and the
        // activation routed each before the run started.
        let route = self.routes.get(effect).ok_or_else(|| {
            ParentFault(format!(
                "the cell performs `{effect}` as a read it did not plan"
            ))
        })?;
        let (ordinal, count) = self.count();
        let projected = CellArgs::Direct
            .args(args)
            .map_err(|refusal| refusal.error(effect));
        let args = projected
            .as_ref()
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let args_bytes = projected.as_ref().map_or(0, |args| args.to_string().len());
        let read = lash_core::ToolReadMetadata {
            caller: self.caller.clone(),
            provider: route.provider.clone(),
            ordinal,
            site: site.to_string(),
            origin: origin.to_string(),
            elapsed_us: 0,
            args_bytes,
            answer_bytes: 0,
        };
        let answer = lash_core::observe_tool_read(
            Some(self.observe.as_ref()),
            read,
            route.definition.manifest.id.to_string(),
            args,
            async {
                count?;
                if let Some(failure) = route.contract_changed() {
                    return Err(tool_failed(failure));
                }
                let args = projected?;
                lash_vocabulary::tool_contract::validate_tool_input(
                    &route.definition.contract,
                    &args,
                )
                .map_err(|error| {
                    tool_failed(
                        &lash_core::ToolFailure::runtime(
                            lash_core::ToolFailureClass::InvalidRequest,
                            "invalid_tool_args",
                            error.to_string(),
                        )
                        .with_cause(lash_core::ToolFailureCause::ValueMismatch { source: error }),
                    )
                })?;
                self.answer(effect, route, &args).await
            },
            |answer| match answer {
                Ok((_, value)) => (
                    lash_core::ToolCallOutput::success(value.clone()),
                    value.to_string().len(),
                ),
                Err(error) => {
                    let mut failure =
                        lash_core::ToolFailure::new(&error.kind, error.message.clone());
                    if let Ok(text) = lash_vm_broker::kernel::datum_to_json(&error.data)
                        && let Ok(data) = serde_json::from_str::<serde_json::Value>(&text)
                        && !data.is_null()
                    {
                        failure = failure.with_raw(lash_core::ToolValue::untrusted_json(data));
                    }
                    (lash_core::ToolCallOutput::failure(failure), 0)
                }
            },
        )
        .await;
        Ok(answer.map(|(value, _)| value))
    }
}
