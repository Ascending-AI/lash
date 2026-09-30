//! Config transactions at the command lane (FIG-4379).
//!
//! A session's config changes only through a config transaction: an ordered
//! list of typed commands, each owned by the core owner or by an installed
//! plugin's [`ConfigOwner`](crate::plugin::ConfigOwner). Ingress admits the
//! transaction against the host's config registry (every owner and command
//! registered, every argument decoding), records the reducer implementation
//! of every owner it names, and enqueues it as
//! [`SessionCommand::ApplyConfigTransaction`](crate::SessionCommand) under
//! the caller's stable id. Nothing is judged against the session's config
//! at ingress: submission completes while a root owns the head, and the
//! transaction waits.
//!
//! The drive's command lane applies it alone once no root owns the head
//! (ADR 0101 §4), so a root that is running or parked when the transaction
//! arrives finishes under the config it was admitted with, and the next root
//! runs under the transaction's. Application is two steps:
//!
//! 1. One recorded step, `config-transaction` on the command's own queue
//!    drain scope, resolves the transaction over the boundary's config: a
//!    stale base, an owner's typed refusal, or every touched owner's
//!    complete replacement with each command's output. Its first execution
//!    runs the reducers only when this build runs the implementations the
//!    transaction was admitted under; otherwise the attempt ends unrecorded,
//!    typed [`RuntimeErrorCode::RetiredGeneration`], and the command root
//!    parks for a build that does. A replay reads the resolution back and
//!    never runs a reducer again, whatever code the build now runs.
//! 2. One fenced commit publishes the recorded resolution, advances
//!    `config_revision` exactly once when it applied, and settles the
//!    command with its [`ConfigTransactionOutcome`]. A stale or refused
//!    transaction publishes nothing and settles all the same.
//!
//! A storeless runtime has no lane and no drive: its `&mut self` serializes
//! the transaction with every turn it runs, so it resolves and publishes
//! directly ([`LashRuntime::apply_storeless_config_transaction`]).

use super::*;
use crate::runtime::effect::executor::RuntimeEffectLocalRunner;

/// The replay key of a config transaction's resolution on its command's
/// scope. The command applies alone under its own scope, so one key names
/// its one resolution.
const CONFIG_TRANSACTION_REPLAY_KEY: &str = "config-transaction";

/// Why a config transaction submission was not accepted.
#[derive(Debug, thiserror::Error)]
pub enum ConfigTransactionSubmitError {
    /// The transaction was not admitted: nothing was enqueued.
    #[error(transparent)]
    Refused(#[from] crate::ConfigSubmitError),
    /// The runtime could not take the submission.
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
}

impl LashRuntime {
    /// The session's config revision: what a transaction written now is
    /// written against (ADR 0101 §12).
    pub fn config_revision(&self) -> u64 {
        self.state.config_revision
    }

    /// Every config registration of this session's installed plugins, and
    /// the core owner's.
    pub fn config_registry(&self) -> Result<Arc<crate::ConfigRegistry>, RuntimeError> {
        let session = self.session.as_ref().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::SessionCommandRun,
                "config commands need the runtime's plugin session",
            )
        })?;
        session.plugins().host().config_registry().map_err(|error| {
            RuntimeError::new(
                RuntimeErrorCode::SessionCommandRun,
                format!("the installed plugins' config registration is invalid: {error}"),
            )
        })
    }

    /// The catalog of every config command this session admits, generated
    /// from the installed registrations and describing the config at the
    /// session's current revision. Discovery only: an argument the catalog's
    /// schema admits can still be refused by its owner.
    pub fn config_command_catalog(&self) -> Result<crate::ConfigCommandCatalog, RuntimeError> {
        Ok(self.config_registry()?.catalog(self.state.config_revision))
    }

    /// Admit `transaction` under the caller's stable `id`, written against
    /// `expected_revision`: every command's owner and name registered and
    /// every argument decoding, with the reducer implementation of every
    /// named owner.
    fn admit_config_transaction(
        &self,
        id: String,
        expected_revision: u64,
        transaction: &crate::ConfigTransaction,
    ) -> Result<crate::ConfigTransactionRecord, ConfigTransactionSubmitError> {
        if id.trim().is_empty() {
            return Err(RuntimeError::new(
                RuntimeErrorCode::SessionCommandIdempotencyKey,
                "a config transaction id cannot be empty",
            )
            .into());
        }
        let registry = self.config_registry()?;
        let entries = registry.entries(transaction)?;
        Ok(registry.admit(id, expected_revision, entries)?)
    }

    /// Submit `transaction` to the session's command lane under the caller's
    /// stable `id`, written against `expected_revision`, and return as soon
    /// as it is durable: before it applies. The drive applies it once no
    /// root owns the head; its outcome is read with
    /// [`Self::settle_session_command`].
    ///
    /// A resubmission under `id` while the first is retained answers the
    /// first's receipt when its content is the same, and is refused
    /// [`ConfigSubmitError::ChangedContent`](crate::ConfigSubmitError::ChangedContent)
    /// when it differs.
    pub async fn submit_config_transaction(
        &mut self,
        id: impl Into<String>,
        expected_revision: u64,
        transaction: &crate::ConfigTransaction,
    ) -> Result<crate::SessionCommandReceipt, ConfigTransactionSubmitError> {
        self.reload_invalidated_resident_session_state().await?;
        let id = id.into();
        let record = self.admit_config_transaction(id.clone(), expected_revision, transaction)?;
        // The store decides a resubmission by its submission digest (ADR
        // 0101 §8): the same record answers the retained command, and any
        // other one under the id is refused.
        let accepted = match self
            .enqueue_session_command(
                crate::SessionCommand::ApplyConfigTransaction {
                    transaction: Box::new(record),
                },
                id.clone(),
            )
            .await
        {
            Ok(accepted) => accepted,
            Err(super::session_api::SessionCommandEnqueueError::ChangedContent(_)) => {
                return Err(crate::ConfigSubmitError::ChangedContent { id }.into());
            }
            Err(super::session_api::SessionCommandEnqueueError::Runtime(error)) => {
                return Err(error.into());
            }
        };
        let receipt = match accepted {
            super::session_api::AcceptedSessionCommand::Inline(receipt) => return Ok(receipt),
            super::session_api::AcceptedSessionCommand::Queued(handle) => handle.receipt,
        };
        self.invalidate_resident_session_state();
        Ok(receipt)
    }

    /// Resolve and publish `transaction` on a storeless runtime, written
    /// against `expected_revision`.
    ///
    /// A storeless runtime has no drive and no durable head: its `&mut self`
    /// serializes the transaction with every turn it runs. A store-backed
    /// runtime is refused: the bound turn owns its head, so its transactions
    /// go through [`Self::submit_config_transaction`].
    pub async fn apply_storeless_config_transaction(
        &mut self,
        id: impl Into<String>,
        expected_revision: u64,
        transaction: &crate::ConfigTransaction,
    ) -> Result<crate::ConfigTransactionOutcome, ConfigTransactionSubmitError> {
        if self.is_store_backed() {
            return Err(RuntimeError::new(
                RuntimeErrorCode::SessionCommandRequired,
                "a store-backed session changes its config through its command lane: submit \
                 the transaction, which applies at the next turn boundary",
            )
            .into());
        }
        let record = self.admit_config_transaction(id.into(), expected_revision, transaction)?;
        let previous = self.session_policy();
        let mut next = self.state.clone();
        // A transaction changes the sticky config, never a root's recorded
        // execution view.
        next.authority.committed_config = None;
        next.authority.root_snapshot = None;
        let base = crate::store::persisted_session_config_from_state(&next);
        let registry = self.config_registry()?;
        let resolution = registry.resolve(&base, &record, &self.core_route_validator());
        let outcome = publish_config_resolution(&resolution, &mut next);
        if matches!(outcome, crate::ConfigTransactionOutcome::Applied { .. }) {
            self.install_resident_state(next);
            self.notify_session_config_changed(previous)
                .await
                .map_err(|error| {
                    RuntimeError::new(RuntimeErrorCode::SessionCommandRun, error.to_string())
                })?;
        }
        Ok(outcome)
    }

    /// The core owner's candidate check: a candidate that changes the route
    /// is refused when no provider of this host serves it (D3 §3.3).
    fn core_route_validator(
        &self,
    ) -> impl Fn(&crate::CoreConfig, &crate::CoreConfig) -> Result<(), crate::ConfigRefusal> + '_
    {
        let resolver = Arc::clone(&self.host.core.providers.provider_resolver);
        move |base, candidate| core_route_check(resolver.as_ref(), base, candidate)
    }

    /// Apply the config transaction the command run `completion` names,
    /// under the command root's `drive_fence` (FIG-4379). `false` when the
    /// command was withdrawn since the lane was read: nothing was applied.
    ///
    /// The resolution is one recorded step on the command's own scope, the
    /// queue drain its batch names, rescoped from the root's controller: a
    /// redrive of the unsettled command publishes the resolution its first
    /// execution recorded, and a replay of a settled one adopts the head its
    /// commit published without committing again.
    pub(super) async fn apply_config_transaction_command(
        &mut self,
        transaction: crate::ConfigTransactionRecord,
        completion: crate::QueuedWorkCompletion,
        drive_fence: &crate::store::DriveFence,
        root_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<bool, RuntimeError> {
        let [batch_id] = completion.batch_ids.as_slice() else {
            return Err(RuntimeError::new(
                RuntimeErrorCode::SessionCommandRun,
                format!(
                    "a config transaction applies alone, but its run names {:?}",
                    completion.batch_ids
                ),
            ));
        };
        let store = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .ok_or_else(|| {
                RuntimeError::new(
                    RuntimeErrorCode::StoreCommitFailed,
                    "a commanded config transaction commits through the session's store",
                )
            })?;
        drop(RuntimeNamedPhase::begin(
            self.turn_phase_probe.clone(),
            super::host_commands::SESSION_COMMAND_APPLYING_PHASE,
        ));
        // The transaction resolves over the boundary's committed head,
        // whichever runtime committed it last.
        self.reload_invalidated_resident_session_state().await?;
        self.adopt_committed_head().await?;
        let host = Arc::clone(&self.host.core.control.effect_host);
        let controller = super::drive::step_controller(
            root_controller,
            host.as_ref(),
            crate::AdmittedScope::queue_drain(self.state.session_id.clone(), batch_id.as_str()),
        )?;
        let resolution =
            Box::pin(self.resolve_config_transaction(&controller, &transaction)).await?;
        if self
            .session_command_run_settled(&store, &completion)
            .await?
        {
            // A replay of a settled transaction: its commit published the
            // head, which the resident session adopts.
            self.invalidate_resident_session_state();
            self.reload_invalidated_resident_session_state().await?;
            return Ok(true);
        }
        let previous = self.session_policy();
        self.state.authority.committed_config = None;
        self.state.authority.root_snapshot = None;
        let outcome = publish_config_resolution(&resolution, &mut self.state);
        let applied = matches!(outcome, crate::ConfigTransactionOutcome::Applied { .. });
        if applied {
            self.publish_resident_authority();
        }
        let committed =
            Box::pin(
                self.commit_host_command(&completion, drive_fence, None, None, |_, _| {
                    crate::runtime::SessionCommandOutcome::ConfigTransaction { outcome }
                }),
            )
            .await?;
        if applied && matches!(committed, super::host_commands::CommandCommit::Landed) {
            self.notify_session_config_changed(previous)
                .await
                .map_err(|error| {
                    RuntimeError::new(RuntimeErrorCode::SessionCommandRun, error.to_string())
                })?;
        }
        Ok(!matches!(
            committed,
            super::host_commands::CommandCommit::Withdrawn
        ))
    }

    /// Resolve `transaction` over the resident config as one recorded step
    /// on `controller`, and return the recorded resolution.
    async fn resolve_config_transaction(
        &mut self,
        controller: &crate::ScopedEffectController<'_>,
        transaction: &crate::ConfigTransactionRecord,
    ) -> Result<crate::ConfigResolution, RuntimeError> {
        let session_id = self.state.session_id.clone();
        let registry = self.config_registry()?;
        let mismatch = registry.check_implementations(transaction).err();
        let invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                controller.execution_scope().clone(),
                CONFIG_TRANSACTION_REPLAY_KEY.to_string(),
            )?,
            crate::RuntimeAttribution::for_session(session_id.clone()),
            format!("config-transaction:{}", transaction.id),
        );
        let mut base_state = self.state.clone();
        base_state.authority.committed_config = None;
        base_state.authority.root_snapshot = None;
        let runner = ResolveConfigTransactionRunner {
            registry,
            base: crate::store::persisted_session_config_from_state(&base_state),
            transaction: transaction.clone(),
            mismatch: mismatch.clone(),
            resolver: Arc::clone(&self.host.core.providers.provider_resolver),
        };
        controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::ResolveConfigTransaction {
                        session: session_id,
                        transaction: transaction.id.clone(),
                    },
                ),
                crate::RuntimeEffectLocalExecutor::owned_runner(Box::new(runner), None),
            )
            .await
            .and_then(crate::RuntimeEffectOutcome::into_config_resolution)
            .map_err(
                |error| match (&mismatch, error.code == RuntimeErrorCode::RetiredGeneration) {
                    // The typed refusal the command root parks on, with the
                    // reducer identities it names.
                    (Some(mismatch), true) => RuntimeError::retired_config_reducer(
                        &mismatch.owner,
                        &mismatch.recorded,
                        mismatch.current.as_deref(),
                    ),
                    _ => error.into_runtime_error(),
                },
            )
    }
}

/// Publish `resolution` onto resident `state`: the recorded replacements
/// and one revision step when it applied, nothing otherwise. A resolution
/// recorded over another revision than the state's settles stale: only the
/// lane changes config, so the state can have moved only past it.
fn publish_config_resolution(
    resolution: &crate::ConfigResolution,
    state: &mut crate::RuntimeSessionState,
) -> crate::ConfigTransactionOutcome {
    if resolution.base_revision != state.config_revision
        && matches!(
            resolution.result,
            crate::ConfigResolutionDecision::Applied { .. }
        )
    {
        return crate::ConfigTransactionOutcome::Stale {
            expected: resolution.base_revision,
            actual: state.config_revision,
        };
    }
    let mut config = crate::store::persisted_session_config_from_state(state);
    let outcome = resolution.publish(&mut config);
    if matches!(outcome, crate::ConfigTransactionOutcome::Applied { .. }) {
        crate::runtime::state::adopt_session_config(state, &config);
    }
    outcome
}

/// The core owner's route check: a candidate that changes the provider or
/// the model must name a route `resolver` serves.
fn core_route_check(
    resolver: &dyn crate::provider::RuntimeProviderResolver,
    base: &crate::CoreConfig,
    candidate: &crate::CoreConfig,
) -> Result<(), crate::ConfigRefusal> {
    if base.provider_id == candidate.provider_id && base.model == candidate.model {
        return Ok(());
    }
    let Err(code) =
        super::drive::validate_route(resolver, &candidate.provider_id, &candidate.model)
    else {
        return Ok(());
    };
    let refusal = crate::CoreConfigRefusal::UnservableRoute {
        code,
        provider_id: candidate.provider_id.clone(),
        model: candidate.model.id.clone(),
    };
    Err(crate::ConfigRefusal {
        index: None,
        owner: crate::CORE_CONFIG_OWNER.to_string(),
        command: None,
        message: refusal.to_string(),
        refusal: serde_json::to_value(&refusal).unwrap_or(serde_json::Value::Null),
    })
}

/// The first execution of one `ResolveConfigTransaction` step: it resolves
/// the transaction over the base config captured at the boundary and
/// records the result. None of it enters the envelope, which names only the
/// session and the transaction.
struct ResolveConfigTransactionRunner {
    registry: Arc<crate::ConfigRegistry>,
    base: crate::PersistedSessionConfig,
    transaction: crate::ConfigTransactionRecord,
    /// The owner whose installed reducer is not the one the transaction was
    /// admitted under, when one is not.
    mismatch: Option<crate::ConfigImplementationMismatch>,
    resolver: Arc<dyn crate::provider::RuntimeProviderResolver>,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for ResolveConfigTransactionRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _usage_run: Option<crate::UsageRun>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::ResolveConfigTransaction { transaction, .. } =
            &envelope.command
        else {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "config-transaction executor cannot execute {} command",
                    envelope.command.kind().as_str()
                ),
            ));
        };
        if *transaction != self.transaction.id {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                format!(
                    "config-transaction executor was bound to `{}` but asked to resolve \
                     `{transaction}`",
                    self.transaction.id
                ),
            ));
        }
        // Resolving with other reducers than the transaction was admitted
        // under would decide it with code it was not admitted to run. The
        // attempt ends unrecorded, and the command root parks until a build
        // that runs the admitted reducers resolves it.
        if let Some(mismatch) = self.mismatch {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RetiredGeneration,
                mismatch.to_string(),
            )
            .retryable_uncommitted_derivation());
        }
        let resolver = Arc::clone(&self.resolver);
        let resolution = self.registry.resolve(
            &self.base,
            &self.transaction,
            &move |base: &crate::CoreConfig, candidate: &crate::CoreConfig| {
                core_route_check(resolver.as_ref(), base, candidate)
            },
        );
        Ok(crate::RuntimeEffectOutcome::ResolveConfigTransaction {
            resolution: Box::new(resolution),
        })
    }
}
