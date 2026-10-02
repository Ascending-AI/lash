#![allow(
    deprecated,
    reason = "Restate SDK 0.11 retains the trait service API while its replacement is staged"
)]

//! The accounting continuation (ADR 0125): the engine half of model-usage
//! delivery.
//!
//! One responsibility: carry a spending effect's journaled usage settlement
//! into storage once, independently of the execution that journaled it. The
//! model-call execution journals a one-way `settle` send to this object right
//! after the spending effect's entry and before the drive sees the outcome.
//! A send is not a child of its caller, so no cancel, kill, park, fork,
//! refusal or deletion of the caller recalls it; Restate retains its payload
//! until the handler completes, and the handler's completion is the
//! acknowledgement.
//!
//! The object is keyed by the usage owner's display (`session:<id>` or
//! `process:<id>`). It holds no state: SQL is the state, and the object exists
//! for per-owner exclusivity and FIFO order. That order is what makes the
//! drain exact: a `drain` or `retire_execution` reaches the owner's object
//! after every `settle` the owner's executions issued before it.

use std::sync::{Arc, OnceLock};

use lash_core::{
    RuntimeOwner, UsageAccountingStore, UsageOwnerRetired, UsageSettlement,
    project_usage_settlement,
};
use restate_sdk::context::ObjectContext;
use restate_sdk::errors::{HandlerError, HandlerResult, TerminalError};
use serde::{Deserialize, Serialize};

use crate::compat::{Call, Reply};

/// The version of the continuation's request bodies. A settle is retained in
/// Restate until a handler reads it, so a body version this build does not
/// read is a terminal refusal (upgrade by drain).
///
/// version_guard(
///     shapes(
///         cover(
///             UsageAccountingSettle, UsageExecutionRetirement, UsageOwnerDrain,
///             UsageOwnerRetiredWire,
///         ),
///     ),
///     shapes(
///         path = "crates/lash-core-store/src/usage_accounting.rs",
///         cover(UsageSettlement, UsageAttemptFact, AttemptFactOutcome, RunAccounting),
///     ),
///     file(
///         path = "crates/lash-sansio/src/identity.rs",
///         cover("string_identity!", RuntimeOwner, SessionId, ProcessId),
///     ),
/// )
/// version_surface = "drain"
/// format_manifest = "engine:restate.usage_accounting"
pub const USAGE_ACCOUNTING_WIRE_VERSION: u32 = 1;

/// One spending effect's settlement, sent by the execution that journaled it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageAccountingSettle {
    pub usage_accounting_version: u32,
    pub settlement: UsageSettlement,
}

/// Resolve the open runs of one killed or lost execution of the owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageExecutionRetirement {
    pub usage_accounting_version: u32,
    pub owner: RuntimeOwner,
    /// `EffectJournalIdentity::key()` of the execution's scope.
    pub execution_scope_key: String,
}

/// Retire the owner after every settlement its executions issued.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageOwnerDrain {
    pub usage_accounting_version: u32,
    pub owner: RuntimeOwner,
}

/// [`UsageOwnerRetired`] on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageOwnerRetiredWire {
    pub retired_at_ms: u64,
    pub resolved_open_runs: u64,
    pub already_retired: bool,
}

impl From<UsageOwnerRetired> for UsageOwnerRetiredWire {
    fn from(retired: UsageOwnerRetired) -> Self {
        let UsageOwnerRetired {
            retired_at_ms,
            resolved_open_runs,
            already_retired,
        } = retired;
        Self {
            retired_at_ms,
            resolved_open_runs,
            already_retired,
        }
    }
}

impl From<UsageOwnerRetiredWire> for UsageOwnerRetired {
    fn from(retired: UsageOwnerRetiredWire) -> Self {
        let UsageOwnerRetiredWire {
            retired_at_ms,
            resolved_open_runs,
            already_retired,
        } = retired;
        Self {
            retired_at_ms,
            resolved_open_runs,
            already_retired,
        }
    }
}

/// The object key of `owner`'s continuation.
pub(crate) fn usage_accounting_object_key(owner: &RuntimeOwner) -> String {
    owner.to_string()
}

/// The accounting continuation. Every lash deployment serves it
/// (`crate::services::bind_lash_services`): a deployment that journaled
/// settle sends without it would retain them forever and project nothing.
#[restate_sdk::object]
pub trait LashUsageAccounting {
    /// Project one settlement: one SQL transaction.
    async fn settle(call: Call<UsageAccountingSettle>) -> HandlerResult<Reply<()>>;
    /// Resolve a killed or lost execution's open runs
    /// `unknown(execution_ended)`, after every settle it issued.
    async fn retire_execution(call: Call<UsageExecutionRetirement>) -> HandlerResult<Reply<u64>>;
    /// Retire the owner, after every settle its executions issued.
    async fn drain(call: Call<UsageOwnerDrain>) -> HandlerResult<Reply<UsageOwnerRetiredWire>>;
}

/// [`LashUsageAccounting`] over the storage the deployment's engine bound.
#[derive(Clone)]
pub(crate) struct LashUsageAccountingImpl {
    store: Arc<OnceLock<Arc<dyn UsageAccountingStore>>>,
}

impl LashUsageAccountingImpl {
    pub(crate) fn new(store: Arc<OnceLock<Arc<dyn UsageAccountingStore>>>) -> Self {
        Self { store }
    }

    /// The bound store. An engine binds it before it serves anything; a
    /// handler that runs first fails retryably and runs again once it is.
    fn store(&self) -> Result<Arc<dyn UsageAccountingStore>, HandlerError> {
        self.store.get().cloned().ok_or_else(|| {
            HandlerError::from(std::io::Error::other(
                "no usage accounting store is bound on this deployment's effect host",
            ))
        })
    }
}

fn check_version(version: u32) -> Result<(), HandlerError> {
    if version == USAGE_ACCOUNTING_WIRE_VERSION {
        return Ok(());
    }
    Err(TerminalError::new(format!(
        "usage accounting request version {version} is not read by this build (reads \
         {USAGE_ACCOUNTING_WIRE_VERSION})"
    ))
    .into())
}

fn now_ms() -> u64 {
    lash_core::ClockWallTime::timestamp_ms(&lash_core::facade_support::SystemClock)
}

fn retryable(operation: &str, error: impl std::fmt::Display) -> HandlerError {
    HandlerError::from(std::io::Error::other(format!(
        "usage accounting {operation} failed: {error}"
    )))
}

impl LashUsageAccounting for LashUsageAccountingImpl {
    async fn settle(
        &self,
        _ctx: ObjectContext<'_>,
        call: Call<UsageAccountingSettle>,
    ) -> HandlerResult<Reply<()>> {
        let (wire, request) = call.open()?;
        let UsageAccountingSettle {
            usage_accounting_version,
            settlement,
        } = request;
        check_version(usage_accounting_version)?;
        let store = self.store()?;
        // A store fault retries without end: a projection waits out a
        // database outage and is never given up. A conflict is answered, not
        // retried: it is marked on the runs in SQL.
        project_usage_settlement(store.as_ref(), &settlement, now_ms())
            .await
            .map_err(|error| retryable("settle", error))?;
        Ok(Reply::at(wire, ()))
    }

    async fn retire_execution(
        &self,
        _ctx: ObjectContext<'_>,
        call: Call<UsageExecutionRetirement>,
    ) -> HandlerResult<Reply<u64>> {
        let (wire, request) = call.open()?;
        let UsageExecutionRetirement {
            usage_accounting_version,
            owner,
            execution_scope_key,
        } = request;
        check_version(usage_accounting_version)?;
        let store = self.store()?;
        let resolved = store
            .retire_usage_execution(&owner, &execution_scope_key, now_ms())
            .await
            .map_err(|error| retryable("execution retirement", error))?;
        Ok(Reply::at(wire, resolved))
    }

    async fn drain(
        &self,
        _ctx: ObjectContext<'_>,
        call: Call<UsageOwnerDrain>,
    ) -> HandlerResult<Reply<UsageOwnerRetiredWire>> {
        let (wire, request) = call.open()?;
        let UsageOwnerDrain {
            usage_accounting_version,
            owner,
        } = request;
        check_version(usage_accounting_version)?;
        let store = self.store()?;
        let retired = store
            .retire_usage_owner(&owner, now_ms())
            .await
            .map_err(|error| retryable("owner drain", error))?;
        Ok(Reply::at(wire, retired.into()))
    }
}

/// The continuation's handler options: a projection waits out any outage.
/// It is never killed by policy, because a settle that stopped retrying
/// would leave its run open forever. The attempt budget is the largest the
/// discovery document carries (a `u32`), about eight thousand years at the
/// minute-long cap, and reaching it pauses the invocation with its request
/// kept, never drops it.
pub(crate) fn usage_accounting_handler_options() -> restate_sdk::endpoint::HandlerOptions {
    restate_sdk::endpoint::HandlerOptions::new()
        .retry_policy_initial_interval(std::time::Duration::from_secs(1))
        .retry_policy_exponentiation_factor(2.0)
        .retry_policy_max_interval(std::time::Duration::from_secs(60))
        .retry_policy_max_attempts(u64::from(u32::MAX))
        .retry_policy_pause_on_max_attempts()
}

/// Drain `owner` through ingress: request-response, after every settle its
/// executions sent (per-key FIFO), under an idempotency key so a retry
/// attaches to the first drain.
#[allow(
    clippy::result_large_err,
    reason = "RestateHttpError travels unboxed across the crate's ingress API"
)]
pub(crate) async fn drain_usage_owner(
    ingress: &crate::RestateIngressClient,
    namespace: &crate::RestateNamespace,
    owner: &RuntimeOwner,
) -> Result<UsageOwnerRetired, crate::RestateHttpError> {
    let retired: UsageOwnerRetiredWire = ingress
        .call_lash_object_idempotent(
            &namespace.stable(crate::LashService::UsageAccounting).name(),
            &usage_accounting_object_key(owner),
            "drain",
            &UsageOwnerDrain {
                usage_accounting_version: USAGE_ACCOUNTING_WIRE_VERSION,
                owner: owner.clone(),
            },
            &format!("usage:drain:{owner}"),
        )
        .await?;
    Ok(retired.into())
}

/// Retire one killed or lost execution of `owner` through ingress, after
/// every settle it sent. `execution_scope_key` is the execution scope's
/// `EffectJournalIdentity::key()`.
#[allow(
    clippy::result_large_err,
    reason = "RestateHttpError travels unboxed across the crate's ingress API"
)]
pub(crate) async fn retire_usage_execution(
    ingress: &crate::RestateIngressClient,
    namespace: &crate::RestateNamespace,
    owner: &RuntimeOwner,
    execution_scope_key: &str,
) -> Result<u64, crate::RestateHttpError> {
    ingress
        .call_lash_object_idempotent(
            &namespace.stable(crate::LashService::UsageAccounting).name(),
            &usage_accounting_object_key(owner),
            "retire_execution",
            &UsageExecutionRetirement {
                usage_accounting_version: USAGE_ACCOUNTING_WIRE_VERSION,
                owner: owner.clone(),
                execution_scope_key: execution_scope_key.to_string(),
            },
            &format!("usage:retire_execution:{owner}:{execution_scope_key}"),
        )
        .await
}

/// The journal key of `scope`, as a usage run records it.
pub(crate) fn execution_scope_key(
    scope: &lash_core::ExecutionScope,
) -> Result<String, lash_core::RuntimeError> {
    scope
        .journal_identity()
        .map(|identity| identity.key().to_string())
        .map_err(|error| {
            lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::MissingExecutionScopeId,
                format!("a usage execution retirement names an invalid scope: {error}"),
            )
        })
}

/// Resolve the open runs of a root's killed or lost turn execution: its
/// session owns every run the execution admitted under the root's turn
/// scope. A follow-on turn of the root that ran under a scope of its own
/// resolves when the session is drained.
pub(crate) async fn retire_root_usage(
    ingress: &crate::RestateIngressClient,
    namespace: &crate::RestateNamespace,
    session: &lash_core::SessionId,
    root: &lash_core::TurnId,
) -> Result<u64, String> {
    let scope = lash_core::ExecutionScope::turn(session.clone(), root.clone());
    let key = execution_scope_key(&scope).map_err(|error| error.to_string())?;
    retire_usage_execution(
        ingress,
        namespace,
        &RuntimeOwner::Session(session.clone()),
        &key,
    )
    .await
    .map_err(|error| error.to_string())
}

/// Resolve the open runs of a process's lost execution under its process
/// scope. A process runtime spends under its own owner only: it runs in no
/// session of its own.
pub(crate) async fn retire_process_usage(
    ingress: &crate::RestateIngressClient,
    namespace: &crate::RestateNamespace,
    process_id: &lash_core::ProcessId,
) -> Result<u64, String> {
    let scope = lash_core::ExecutionScope::process(process_id.clone());
    let key = execution_scope_key(&scope).map_err(|error| error.to_string())?;
    retire_usage_execution(
        ingress,
        namespace,
        &RuntimeOwner::Process(process_id.clone()),
        &key,
    )
    .await
    .map_err(|error| error.to_string())
}
