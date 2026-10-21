//! A cell's planned reads (ADR 0144): the `perform`s its read plan binds,
//! answered inside the statement that makes them.
//!
//! A read is no admitted execution. It mints no call identity and no wait,
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
use std::time::{Duration, Instant};

use lash_kernel_doc::Datum;
use lash_kernel_doc::ErrorDatum;
use lash_protocol_types::sync::MutexExt;
use lash_vm_broker::ParentFault;
use lash_vm_broker::kernel::{EFFECT_CANCELLED, EFFECT_RESULT, EFFECT_TIMED_OUT, datum_from_json};
use lash_vm_runtime::{EffectReads, HostBoundary};
use lash_vocabulary::legacy_names::EffectName;

use super::host::{CellArgs, CellHostLedgers, TOOL_CALL_LIMIT, TOOL_FAILED, refused, tool_failed};

/// The reads one activation of a cell answers.
pub(super) struct CellReads {
    /// Each planned effect's route on this node.
    pub routes: BTreeMap<EffectName, lash_core::ReadRoute>,
    pub config: lash_core::plugin::OwnerConfigView,
    /// Cancelled when the cell's run is stopped.
    pub cancel: lash_core::CancellationToken,
    /// How long the worker waits on its parent's answer.
    pub parent_wait: Duration,
    pub ledgers: Arc<Mutex<CellHostLedgers>>,
    pub boundary: HostBoundary,
    pub limit: lash_core::MaxToolCalls,
    /// The cell, as its observations name it.
    pub cell: String,
}

impl CellReads {
    /// Counts one read against `max_tool_calls`: its ordinal among the
    /// cell's reads, or the refusal its `perform` raises.
    fn count(&self) -> Result<u64, ErrorDatum> {
        let mut ledgers = self.ledgers.lock_recover();
        let counted = ledgers.counted(&self.boundary);
        if counted.saturating_add(1) > self.limit.get() {
            let exceeded = lash_core::ToolCallLimitExceeded {
                scope: lash_core::ToolCallLimitScope::Cell,
                limit: self.limit,
                counted,
                requested: 1,
            };
            ledgers.tool_call_limit.get_or_insert(exceeded);
            return Err(refused(TOOL_CALL_LIMIT, exceeded.to_string()));
        }
        ledgers.reads += 1;
        Ok(ledgers.reads)
    }

    /// The read's arguments, validated against its tool's input schema.
    fn args(
        &self,
        effect: &EffectName,
        route: &lash_core::ReadRoute,
        args: &[Datum],
    ) -> Result<serde_json::Value, ErrorDatum> {
        let args = CellArgs
            .args(args)
            .map_err(|refusal| refusal.error(effect))?;
        // The protocol's argument projection, as its transform applies it
        // to a recorded call's.
        let args = crate::projection::normalize_tool_args_for_projection(
            args,
            &route.definition.manifest.argument_projection,
        )
        .map_err(|error| refused(super::host::TOOL_ARGUMENTS, error.to_string()))?;
        lash_protocol_types::validate_tool_input(&route.definition.contract, &args).map_err(
            |error| {
                tool_failed(
                    &lash_core::ToolFailure::runtime(
                        lash_core::ToolFailureClass::InvalidRequest,
                        "invalid_tool_args",
                        error.to_string(),
                    )
                    .with_cause(lash_core::ToolFailureCause::ValueMismatch { source: error }),
                )
            },
        )?;
        Ok(args)
    }

    async fn answer(
        &self,
        effect: &EffectName,
        route: &lash_core::ReadRoute,
        args: &serde_json::Value,
    ) -> Result<(Datum, usize), ErrorDatum> {
        let bound = route.definition.manifest.execution().min(self.parent_wait);
        let deadline = Instant::now() + bound;
        let read = route.read(args, &self.config, &self.cancel, deadline);
        let value = tokio::select! {
            biased;
            () = self.cancel.cancelled() => {
                return Err(refused(
                    EFFECT_CANCELLED,
                    format!("the read of `{effect}` was stopped with the cell"),
                ));
            }
            () = tokio::time::sleep_until(deadline.into()) => {
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
        let text = value.to_string();
        let value = datum_from_json(&text)
            .map_err(|invalid| refused(EFFECT_RESULT, invalid.to_string()))?;
        Ok((value, text.len()))
    }
}

#[async_trait::async_trait]
impl EffectReads for CellReads {
    async fn perform(
        &self,
        effect: &EffectName,
        args: &[Datum],
        site: &lash_vocabulary::sites::PositionLabel,
        origin: &lash_vocabulary::sites::Site,
    ) -> Result<Result<Datum, ErrorDatum>, ParentFault> {
        // The plan names every effect the machine binds as a read, and the
        // activation routed each before the run started.
        let route = self.routes.get(effect).ok_or_else(|| {
            ParentFault(format!(
                "the cell performs `{effect}` as a read it did not plan"
            ))
        })?;
        let started = Instant::now();
        let ordinal = match self.count() {
            Ok(ordinal) => ordinal,
            Err(refusal) => return Ok(Err(refusal)),
        };
        // A tool served under another contract than the run pinned raises
        // `operation_contract_changed` at the `perform`: nothing is read.
        if let Some(failure) = route.contract_changed() {
            return Ok(Err(tool_failed(failure)));
        }
        let answer = match self.args(effect, route, args) {
            Ok(args) => {
                let answer = self.answer(effect, route, &args).await;
                tracing::debug!(
                    target: "lash::cell::read",
                    cell = %self.cell,
                    provider = %route.provider,
                    tool = %route.definition.manifest.id,
                    site = %site,
                    origin = %origin,
                    seq = ordinal,
                    elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                    args_bytes = args.to_string().len(),
                    answer_bytes = answer.as_ref().map_or(0, |(_, bytes)| *bytes),
                    answered = answer.is_ok(),
                    "cell read"
                );
                answer.map(|(value, _)| value)
            }
            Err(refusal) => Err(refusal),
        };
        Ok(answer)
    }
}
