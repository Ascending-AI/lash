//! Config transactions at the command lane (FIG-4379).
//!
//! A session's config changes only through a config transaction: an ordered
//! list of typed commands, each owned by the core owner or by an installed
//! plugin's [`ConfigOwner`](crate::plugin::ConfigOwner). Ingress admits the
//! transaction against the host's config registry (every owner and command
//! registered, every argument decoding) and enqueues it as
//! [`SessionCommand::ApplyConfigTransaction`](crate::SessionCommand) under
//! the caller's stable id. Nothing is judged against the session's config
//! at ingress: submission completes while a run owns the head, and the
//! transaction waits.
//!
//! The shift's command lane applies it alone once no run owns the head
//! (ADR 0101 §4), so a run that is running or parked when the transaction
//! arrives finishes under the config it was admitted with, and the next run
//! runs under the transaction's. Application is two steps:
//!
//! 1. One recorded step, `config-transaction` on the command's own queue
//!    drain scope, resolves the transaction over the boundary's config: a
//!    stale base, an owner's typed refusal, or every touched owner's
//!    complete replacement with each command's output. Its first execution
//!    runs this build's reducers: an owner's reducers are its plugin's
//!    behaviour, which the build generation hashes (FIG-4791), so the lane
//!    that admitted the command run is the one whose reducers decide it,
//!    and a redrive of the run stays on that lane. A replay reads the
//!    resolution back and never runs a reducer again.
//! 2. One fenced commit publishes the recorded resolution, advances
//!    `config_revision` exactly once when it applied, and settles the
//!    command with its [`ConfigTransactionOutcome`]. A stale or refused
//!    transaction publishes nothing and settles all the same.
//!
//! A storeless runtime has no lane and no shift: its `&mut self` serializes
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
        // Registrations belong to the installed composition. Submission must
        // work before a command Run publishes the session's native view and
        // constructs its capabilities.
        self.services
            .plugins
            .host()
            .config_registry()
            .map_err(|error| {
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
    /// every argument decoding.
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
    /// as it is durable: before it applies. The shift applies it once no
    /// run owns the head; its outcome is read with
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
    /// A storeless runtime has no shift and no durable head: its `&mut self`
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
        // A transaction resolves over the sticky config and changes it,
        // never the recorded view of the run this runtime ran last.
        next.take_run_view();
        let base = crate::store::persisted_session_config_from_state(&next);
        let registry = self.config_registry()?;
        // A storeless runtime has no fleet record: each plugin writes its
        // native format.
        let resolution = registry
            .resolve(
                &base,
                &record,
                self.host.core.providers.models.as_ref(),
                &crate::store::plugin_writers::PluginAdmission::default(),
            )
            .map_err(|corrupt| {
                crate::RuntimeEffectControllerError::from(corrupt.into_store_error())
                    .into_runtime_error()
            })?;
        let outcome = publish_config_resolution(&resolution, &mut next);
        if matches!(outcome, crate::ConfigTransactionOutcome::Applied { .. }) {
            self.install_resident_state(next)
                .map_err(RuntimeError::from)?;
            self.notify_session_config_changed(previous).await;
        }
        Ok(outcome)
    }

    /// Apply the config transaction the command run `completion` names,
    /// (FIG-4379). `false` when the
    /// command was withdrawn since the lane was read: nothing was applied.
    ///
    /// The resolution is one recorded step on the command's own scope, the
    /// session operation its batch names, rescoped from the run's controller: a
    /// redrive of the unsettled command publishes the resolution its first
    /// execution recorded, and a replay of a settled one adopts the head its
    /// commit published without committing again.
    pub(super) async fn apply_config_transaction_command(
        &mut self,
        transaction: crate::ConfigTransactionRecord,
        completion: crate::QueuedWorkCompletion,
        run_controller: &crate::ActorContext,
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
        let store = self.services.store.clone().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::StoreCommitFailed,
                "a commanded config transaction settles in the session's store",
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
        // The head this runtime committed itself is not reloaded, so the
        // recorded view of the run it ran last is still installed: the
        // transaction resolves over the sticky config under it and
        // publishes onto it, never the run's overrides.
        self.uninstall_run_view()?;
        let controller = super::step_controller(
            run_controller,
            crate::AdmittedScope::session_operation(
                self.state.session_id.clone(),
                batch_id.as_str(),
            ),
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
        let outcome = publish_config_resolution(&resolution, &mut self.state);
        let applied = matches!(outcome, crate::ConfigTransactionOutcome::Applied { .. });
        if applied {
            self.publish_resident_authority()?;
        }
        let committed =
            Box::pin(
                self.commit_host_command(run_controller, &completion, None, None, |_, _| {
                    crate::runtime::SessionCommandOutcome::ConfigTransaction { outcome }
                }),
            )
            .await?;
        if applied && matches!(committed, super::host_commands::CommandCommit::Landed) {
            self.notify_session_config_changed(previous).await;
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
        controller: &crate::ActorContext,
        transaction: &crate::ConfigTransactionRecord,
    ) -> Result<crate::ConfigResolution, RuntimeError> {
        let session_id = self.state.session_id.clone();
        let registry = self.config_registry()?;
        let invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                controller.execution_scope().clone(),
                CONFIG_TRANSACTION_REPLAY_KEY.to_string(),
            )?,
            crate::RuntimeAttribution::for_session(session_id.clone()),
            format!("config-transaction:{}", transaction.id),
        );
        let runner = ResolveConfigTransactionRunner {
            registry,
            plugin_host: self
                .session
                .as_ref()
                .map(|session| session.plugins().host().clone()),
            store: self.services.store.clone(),
            base: crate::store::persisted_session_config_from_state(&self.state),
            transaction: transaction.clone(),
            models: Arc::clone(&self.host.core.providers.models),
        };
        controller
            .session_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::ResolveConfigTransaction {
                        session: session_id,
                        transaction: transaction.id.clone(),
                    },
                ),
                lash_core_execution::core_internal::owned_runner_executor(Box::new(runner), None),
            )
            .await
            .and_then(crate::RuntimeEffectOutcome::into_config_resolution)
            .map_err(crate::RuntimeEffectControllerError::into_runtime_error)
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

/// The first execution of one `ResolveConfigTransaction` step: it resolves
/// the transaction over the base config captured at the boundary and
/// records the result. None of it enters the envelope, which names only the
/// session and the transaction.
struct ResolveConfigTransactionRunner {
    registry: Arc<crate::ConfigRegistry>,
    /// The plugins whose writer formats the step chooses from the fleet
    /// record (FIG-4747). The choice is part of the recorded resolution, so
    /// a replay publishes the formats the first execution chose.
    plugin_host: Option<crate::plugin::PluginHost>,
    store: Option<crate::store::SessionStore>,
    base: crate::PersistedSessionConfig,
    transaction: crate::ConfigTransactionRecord,
    /// The host's models a model command mints its key's binding through.
    models: Arc<dyn crate::LlmProfiles>,
}

#[async_trait::async_trait]
impl RuntimeEffectLocalRunner for ResolveConfigTransactionRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _effect_attempt: Option<crate::EffectAttempt>,
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
        // A recorded namespace its owner cannot read is corruption of the
        // session's config, never this transaction's refusal.
        let writers = match (&self.plugin_host, &self.store) {
            (Some(host), Some(store)) => host
                .admit_plugins(store.store().as_ref())
                .await
                .map_err(crate::runtime::runtime_error_from_store_commit)?,
            _ => crate::store::plugin_writers::PluginAdmission::default(),
        };
        let mut resolution = self
            .registry
            .resolve(
                &self.base,
                &self.transaction,
                self.models.as_ref(),
                &writers,
            )
            .map_err(crate::RecordedNamespaceCorrupt::into_store_error)?;
        if let (Some(host), crate::ConfigResolutionDecision::Applied { namespaces, .. }) =
            (&self.plugin_host, &mut resolution.result)
        {
            let mut candidate = crate::PluginConfig::default();
            candidate.apply_namespace_updates(namespaces);
            let native = host.decode_config(&candidate)?;
            *namespaces = native.namespaces().clone();
        }
        Ok(crate::RuntimeEffectOutcome::ResolveConfigTransaction {
            resolution: Box::new(resolution),
        })
    }
}
