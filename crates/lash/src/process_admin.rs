//! The global process facade surface.
//!
//! [`Processes`] (reached via [`LashCore::processes`](crate::LashCore::processes),
//! re-exported as [`lash::process::Processes`](crate::process::Processes)) is THE
//! host-level process surface (ADR 0014 grill): start, observe, signal, cancel,
//! transfer, prune, and abandon-request every process, with the two distinct
//! scope filters — `observed_by` (what a session may address) and `originated_by`
//! (what a session created). The session-scoped
//! [`SessionProcessAdmin`](crate::admin::SessionProcessAdmin) is thin sugar over
//! this surface pre-filtered by a session's observer edge; it lives in `admin` because it
//! wraps a [`SessionAdmin`](crate::admin::SessionAdmin).

use crate::support::{Arc, EmbedError, LashCore, Result, ScopedEffectController};
use lash_core::facade_support::ScopedEffectControllerFacadeOps;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;

async fn await_process_terminal(
    process_work: &dyn lash_core::ProcessWorkSubstrate,
    process_id: &ProcessId,
) -> std::result::Result<lash_core::ProcessAwaitOutput, lash_core::PluginError> {
    loop {
        match process_work.await_process_terminal(process_id).await? {
            lash_core::ProcessTerminalWait::Terminal(output) => return Ok(output),
            lash_core::ProcessTerminalWait::Reattach => continue,
        }
    }
}

struct SurveyedTriggerStore<'a> {
    inner: &'a dyn lash_core::TriggerStore,
    retention_candidates: std::sync::Mutex<Vec<lash_core::TriggerDeliveryRetentionCandidate>>,
}

impl<'a> SurveyedTriggerStore<'a> {
    fn new(
        inner: &'a dyn lash_core::TriggerStore,
        retention_candidates: Vec<lash_core::TriggerDeliveryRetentionCandidate>,
    ) -> Self {
        Self {
            inner,
            retention_candidates: std::sync::Mutex::new(retention_candidates),
        }
    }

    fn delivery_process_ids(&self) -> Vec<ProcessId> {
        self.retention_candidates
            .lock_recover()
            .iter()
            .map(|candidate| candidate.process_id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn protected_process_count(&self) -> usize {
        self.delivery_process_ids().len()
    }
}

#[async_trait::async_trait]
impl lash_core::TriggerStore for SurveyedTriggerStore<'_> {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: lash_core::TriggerCommand,
    ) -> std::result::Result<lash_core::TriggerEffectResult, lash_core::PluginError> {
        self.inner.execute_command(operation_id, command).await
    }

    async fn list_subscriptions(
        &self,
        filter: lash_core::TriggerSubscriptionFilter,
    ) -> std::result::Result<Vec<lash_core::TriggerSubscriptionRecord>, lash_core::PluginError>
    {
        self.inner.list_subscriptions(filter).await
    }

    async fn subscriptions_changed_since(
        &self,
        cursor: lash_core::TriggerSubscriptionChangeCursor,
        limit: usize,
    ) -> std::result::Result<
        (
            Vec<lash_core::TriggerSubscriptionChange>,
            lash_core::TriggerSubscriptionChangeCursor,
        ),
        lash_core::PluginError,
    > {
        self.inner.subscriptions_changed_since(cursor, limit).await
    }
    async fn list_subscriptions_with_cursor(
        &self,
    ) -> std::result::Result<
        (
            Vec<lash_core::TriggerSubscriptionRecord>,
            lash_core::TriggerSubscriptionChangeCursor,
        ),
        lash_core::PluginError,
    > {
        self.inner.list_subscriptions_with_cursor().await
    }
    async fn compact_subscription_tombstones(
        &self,
        cutoff_epoch_ms: u64,
    ) -> std::result::Result<usize, lash_core::PluginError> {
        self.inner
            .compact_subscription_tombstones(cutoff_epoch_ms)
            .await
    }

    async fn delete_session_subscriptions(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<usize, lash_core::PluginError> {
        self.inner.delete_session_subscriptions(session_id).await
    }

    async fn ingest_occurrence(
        &self,
        request: lash_core::TriggerOccurrenceRequest,
    ) -> std::result::Result<lash_core::TriggerIngressReceipt, lash_core::PluginError> {
        self.inner.ingest_occurrence(request).await
    }

    async fn list_occurrences(
        &self,
        filter: lash_core::TriggerOccurrenceFilter,
    ) -> std::result::Result<Vec<lash_core::TriggerOccurrenceRecord>, lash_core::PluginError> {
        self.inner.list_occurrences(filter).await
    }

    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> std::result::Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError>
    {
        self.inner
            .list_deliveries_by_occurrence_id(occurrence_id)
            .await
    }

    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> std::result::Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError>
    {
        self.inner
            .list_deliveries_by_subscription_id(subscription_id)
            .await
    }

    async fn list_deliveries_by_process_id(
        &self,
        process_id: &ProcessId,
    ) -> std::result::Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError>
    {
        self.inner.list_deliveries_by_process_id(process_id).await
    }

    async fn list_deliveries(
        &self,
    ) -> std::result::Result<Vec<lash_core::TriggerDeliveryReservation>, lash_core::PluginError>
    {
        self.inner.list_deliveries().await
    }

    async fn bind_delivery_process(
        &self,
        occurrence_id: &str,
        subscription_id: &str,
        process_id: &ProcessId,
    ) -> std::result::Result<(), lash_core::PluginError> {
        self.inner
            .bind_delivery_process(occurrence_id, subscription_id, process_id)
            .await
    }

    async fn list_delivery_process_ids(
        &self,
    ) -> std::result::Result<Vec<ProcessId>, lash_core::PluginError> {
        Ok(self.delivery_process_ids())
    }

    async fn list_delivery_retention_candidates(
        &self,
    ) -> std::result::Result<
        Vec<lash_core::TriggerDeliveryRetentionCandidate>,
        lash_core::PluginError,
    > {
        Ok(self.retention_candidates.lock_recover().clone())
    }

    async fn list_session_owner_ids_for_retention(
        &self,
    ) -> std::result::Result<Vec<SessionId>, lash_core::PluginError> {
        self.inner.list_session_owner_ids_for_retention().await
    }

    async fn reconcile_trigger_retention(
        &self,
        candidates: &[lash_core::TriggerDeliveryRetentionCandidate],
        deleted_session_ids: &[SessionId],
    ) -> std::result::Result<lash_core::TriggerRetentionReconciliationReport, lash_core::PluginError>
    {
        let report = self
            .inner
            .reconcile_trigger_retention(candidates, deleted_session_ids)
            .await?;
        if report.reclaimed_delivery_count == candidates.len() {
            let deleted_candidates = candidates
                .iter()
                .cloned()
                .collect::<std::collections::HashSet<_>>();
            self.retention_candidates
                .lock_recover()
                .retain(|candidate| !deleted_candidates.contains(candidate));
        }
        Ok(report)
    }

    async fn delete_delivery_retention_candidates(
        &self,
        candidates: &[lash_core::TriggerDeliveryRetentionCandidate],
    ) -> std::result::Result<usize, lash_core::PluginError> {
        let deleted = self
            .inner
            .delete_delivery_retention_candidates(candidates)
            .await?;
        if deleted == candidates.len() {
            let deleted_candidates = candidates
                .iter()
                .cloned()
                .collect::<std::collections::HashSet<_>>();
            self.retention_candidates
                .lock_recover()
                .retain(|candidate| !deleted_candidates.contains(candidate));
        }
        Ok(deleted)
    }

    async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> lash_core::TriggerOccurrenceReclamationResult {
        self.inner
            .reclaim_trigger_occurrences(cutoff_epoch_ms)
            .await
    }

    async fn forget_trigger_tombstones(
        &self,
        written_before_epoch_ms: u64,
    ) -> std::result::Result<usize, lash_core::StoreError> {
        self.inner
            .forget_trigger_tombstones(written_before_epoch_ms)
            .await
    }

    async fn prune_mutation_receipts(
        &self,
        cutoff_epoch_ms: u64,
    ) -> std::result::Result<usize, lash_core::PluginError> {
        self.inner.prune_mutation_receipts(cutoff_epoch_ms).await
    }

    async fn prune_non_fired_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> std::result::Result<usize, lash_core::PluginError> {
        self.inner
            .prune_non_fired_occurrences(cutoff_epoch_ms)
            .await
    }
}

#[derive(Clone)]
/// Manages durable processes through a configured Lash core.
pub struct Processes {
    pub(crate) core: LashCore,
}

impl Processes {
    /// Observe one exact process lifetime through its process cursor.
    ///
    /// Without a cursor the first item is a snapshot. With one, the
    /// subscription resumes after it when this core's live route still bridges
    /// it; otherwise the first item is a gap with a snapshot and a new cursor.
    pub async fn subscribe_observation(
        &self,
        process_id: &ProcessId,
        cursor: Option<&crate::process_observation::ProcessCursor>,
    ) -> Result<crate::process_observation::ProcessObservationSubscription> {
        Ok(self
            .core
            .process_observation_hub
            .subscribe(self.registry(), process_id, cursor)
            .await?)
    }

    /// Decode an exact-version remote request into this core's local route.
    pub async fn subscribe_observation_remote(
        &self,
        request: &lash_remote_protocol::RemoteProcessObservationRequest,
    ) -> Result<crate::process_observation::ProcessObservationSubscription> {
        request.validate()?;
        self.subscribe_observation(&request.process_id, request.cursor.as_ref())
            .await
    }

    fn registry(&self) -> Arc<dyn lash_core::ProcessRegistry> {
        self.core.process_registry()
    }

    fn make_observer(&self) -> Result<lash_core::facade_support::ProcessWorkObserver> {
        Ok(lash_core::facade_support::ProcessWorkObserver::new(
            self.registry(),
        ))
    }

    /// The listing filter [`prune`](Self::prune) surveys effect-journal
    /// retirement candidates with. It is the caller's retention filter verbatim
    /// so the journals retired are exactly the rows the prune can delete, and
    /// absence widens to every status rather than the `Running` default.
    fn prune_selection(
        filter: Option<&lash_core::ProcessListFilter>,
    ) -> Result<lash_core::ProcessListFilter> {
        let Some(filter) = filter else {
            return Ok(lash_core::ProcessListFilter {
                status: lash_core::ProcessStatusFilter::Any,
                ..lash_core::ProcessListFilter::default()
            });
        };
        if matches!(&filter.status, lash_core::ProcessStatusFilter::In(statuses) if !statuses.is_empty() && statuses.iter().all(|status| !status.is_retired()))
        {
            return Err(EmbedError::Plugin(lash_core::PluginError::Session(
                format!(
                    "process retention filter selects the live status set `{:?}`, \
                     which no prunable row can hold; pass \
                     `ProcessStatusFilter::Any` or a set containing a retired status",
                    filter.status
                ),
            )));
        }
        Ok(filter.clone())
    }

    #[expect(
        clippy::expect_used,
        reason = "the scope is the caller's own live execution scope, which is admitted \
                  by construction"
    )]
    fn process_invocation(
        command: &lash_core::ProcessCommand,
        scope: &lash_core::ExecutionScope,
    ) -> lash_core::RuntimeEffectInvocation {
        let effect_id = command.effect_id();
        lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(scope.clone(), effect_id.clone())
                .expect("process command carries an admitted effect scope"),
            lash_core::RuntimeAttribution::none(),
            effect_id,
        )
    }

    async fn run_command(
        &self,
        command: lash_core::ProcessCommand,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessEffectOutcome> {
        self.execute_command(command, scoped_effect_controller, None)
            .await
            .map_err(|err| EmbedError::Plugin(err.into()))
    }

    async fn execute_command(
        &self,
        command: lash_core::ProcessCommand,
        scoped_effect_controller: ScopedEffectController<'_>,
        session_turn_admission: Option<lash_core::runtime::SessionTurnAdmission>,
    ) -> std::result::Result<lash_core::ProcessEffectOutcome, lash_core::RuntimeEffectControllerError>
    {
        let registry = self.registry();
        let process_work = Arc::clone(self.core.substrate_slot.ports().await.process.port());
        let invocation =
            Self::process_invocation(&command, scoped_effect_controller.execution_scope());
        let outcome = scoped_effect_controller
            .execute_process_effect(
                lash_core::RuntimeEffectEnvelope::new(
                    invocation,
                    lash_core::RuntimeEffectCommand::process(command),
                ),
                lash_core::RuntimeEffectLocalExecutor::processes(registry, process_work)
                    .with_process_attachments(self.core.backend.attachment_referrers())
                    .with_process_starts(
                        self.core
                            .backend
                            .obligation_ledger(lash_core::store::ObligationKind::ProcessStart),
                        Arc::clone(&self.core.env.core.clock),
                        self.core.env.core.control.relay_policy(),
                    )
                    .with_process_env_store(Arc::clone(
                        &self.core.env.core.durability.process_env_store,
                    ))
                    .with_process_session_catalog(Arc::clone(&self.core.store_factory) as _)
                    .with_session_turn_admission(session_turn_admission)
                    .with_process_engines(self.core.host_process_engines.clone()),
            )
            .await?;
        match outcome {
            lash_core::RuntimeEffectOutcome::Process { result } => Ok(result),
            _ => Err(lash_core::RuntimeEffectControllerError::from(
                lash_core::PluginError::Session(
                    "process effect returned non-process outcome".to_string(),
                ),
            )),
        }
    }

    /// Engine-admission ruling (FIG-1488): this route deliberately stays outside
    /// the gate. It is an operator seam — the host names the registration
    /// itself, on its own authority, exactly as a host calling the process
    /// registry directly does. The gate exists to stop a *model or leaf* payload
    /// from becoming a committed start; it is not a guard against the operator's
    /// own request. `ProcessEngine::run` still refuses an unrunnable row.
    /// The scope a host start may live until: `session_id`, looked up now.
    ///
    /// A host start is a root: it has no starter, so its lifetime is
    /// [`Lifetime::Detached`](lash_core::Lifetime::Detached) or `Until` a
    /// session this lookup grants (FIG-3607 R3). A process started `Until` it
    /// is cancelled when the session is deleted, and its descendants inherit
    /// the session as their session capability.
    ///
    /// # Errors
    ///
    /// [`EmbedError::UnknownSession`] when the session does not exist or was
    /// deleted.
    pub async fn session_scope(&self, session_id: &SessionId) -> Result<lash_core::ScopeRef> {
        self.require_live_session(session_id).await?;
        Ok(lash_core::ScopeRef::host_session_lookup(session_id.clone()))
    }

    async fn require_live_session(&self, session_id: &SessionId) -> Result<()> {
        let live =
            lash_core::runtime::session_is_live(self.core.store_factory.as_ref(), session_id)
                .await?;
        if live {
            Ok(())
        } else {
            Err(EmbedError::UnknownSession {
                session_id: session_id.clone(),
            })
        }
    }

    /// The environment a host's session-turn start captures when its start
    /// names none (FIG-4396). No session starts it, so nothing recorded
    /// stands beneath it and the core keeps no default (FIG-4594): the
    /// environment is the policy the request itself states, with no model
    /// minted, and no plugin configuration. It is a function of what the
    /// host stated, so the start-key fence that compares it compares only
    /// that, and a retry on a deployment whose host changed what it passes
    /// elsewhere presents the same start. The child's plugin config is
    /// admitted here, on the core's plugin set, before anything is
    /// registered.
    ///
    /// # Errors
    ///
    /// [`EmbedError::SessionTurnStartUnspecified`] when the request leaves
    /// its policy or its model unstated.
    fn host_session_turn_environment(
        &self,
        create_request: &lash_core::SessionCreateRequest,
    ) -> Result<lash_core::ProcessExecutionEnvSpec> {
        if let Some(unstated) = create_request.unstated_root_config() {
            return Err(EmbedError::SessionTurnStartUnspecified { unstated });
        }
        let Some(policy) = create_request.policy.clone() else {
            return Err(EmbedError::SessionTurnStartUnspecified {
                unstated: lash_core::UnstatedSessionConfig::Policy,
            });
        };
        let environment = lash_core::ProcessExecutionEnvSpec::new(
            lash_core::AdmittedPluginConfig::default(),
            policy,
        );
        let is_child = create_request.relation.parent_session_id().is_some();
        crate::support::build_plugin_host(
            self.core.protocol_factory.as_ref(),
            self.core.plugin_factories.as_ref(),
        )?
        .resolve_creation_plugin_config(
            self.core
                .protocol_factory
                .as_ref()
                .map(|protocol_factory| protocol_factory.id()),
            &create_request.plugin_options,
            is_child.then_some(environment.plugin_config.config.as_ref()),
            !is_child,
            // The resolution only validates the request: nothing is written.
            &lash_core::store::plugin_writers::PluginAdmission::default(),
        )
        .map_err(lash_core::SessionError::from)?;
        Ok(environment)
    }

    fn host_session_turn_admission(
        &self,
        request: lash_core::SessionCreateRequest,
        environment: Option<lash_core::ProcessExecutionEnvSpec>,
        env_ref: Option<lash_core::ProcessExecutionEnvRef>,
        claim: lash_core::ReferrerClaim,
    ) -> lash_core::runtime::SessionTurnAdmission {
        let core = self.core.clone();
        Arc::new(move |fresh| {
            let core = core.clone();
            let request = request.clone();
            let environment = environment.clone();
            let env_ref = env_ref.clone();
            let claim = claim.clone();
            Box::pin(async move {
                let env_store = core.env.core.durability.process_env_store.as_ref();
                if fresh && let Some(key) = request.model.as_ref() {
                    // The reasoning the key's binding runs with: the
                    // request's own, else the one the policy beneath it
                    // records: the request's, the start's environment, or
                    // the captured environment the start names. A start
                    // carries one of them (`Processes::start`).
                    let reasoning = match request.reasoning {
                        Some(reasoning) => reasoning,
                        None => {
                            let policy = match (request.policy, environment.as_ref()) {
                                (Some(policy), _) => Some(policy),
                                (None, Some(environment)) => Some(environment.policy.clone()),
                                (None, None) => match env_ref.as_ref() {
                                    Some(env_ref) => Some(
                                        lash_core::runtime::load_process_execution_env(
                                            env_store, env_ref,
                                        )
                                        .await
                                        .map_err(lash_core::PluginError::from)?
                                        .policy,
                                    ),
                                    None => None,
                                },
                            };
                            policy
                                .and_then(|policy| policy.model)
                                .map(|model| model.reasoning)
                                .unwrap_or_default()
                        }
                    };
                    let model =
                        lash_core::LlmProfileConfig {
                            model: core.env.core.providers.models.snapshot(key).map_err(
                                |error| {
                                    lash_core::RuntimeEffectControllerError::new(
                                        lash_core::RuntimeErrorCode::LlmProfileUnknown,
                                        error.to_string(),
                                    )
                                },
                            )?,
                            reasoning,
                        };
                    model.validate_reasoning().map_err(|refused| {
                        lash_core::RuntimeEffectControllerError::new(
                            lash_core::RuntimeErrorCode::ReasoningRefused,
                            format!("process start refused: {refused}"),
                        )
                    })?;
                }
                match environment {
                    Some(environment) => {
                        lash_core::publish_process_execution_env(env_store, &claim, &environment)
                            .await?;
                    }
                    None => {
                        if let Some(env_ref) = env_ref {
                            env_store
                                .acquire_process_execution_env(&claim, &env_ref)
                                .await
                                .map_err(lash_core::PluginError::from)?;
                        }
                    }
                }
                Ok(())
            })
        })
    }

    /// Start a process, refusing a session-turn child's model key whose
    /// reasoning is unsupported before any environment or process is
    /// written. The refusal retains `RuntimeErrorCode::ReasoningRefused`.
    ///
    /// A session-turn start a host issues with no captured environment is a
    /// root creation, and takes an explicit spec like every other
    /// (FIG-4594): build its create request with
    /// [`SessionCreateRequest::with_spec`](lash_core::SessionCreateRequest::with_spec).
    /// One that leaves its policy or model unstated is refused with
    /// [`EmbedError::SessionTurnStartUnspecified`] and nothing is
    /// registered. The start is registered exactly as stated, so a retry
    /// under its host key is returned the retained process whatever the
    /// host passes to other starts by then.
    pub async fn start(
        &self,
        request: lash_core::ProcessStartRequest,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessStartReceipt> {
        // The registrar mints the id; the key only makes the start idempotent.
        // A host mints only host keys: a key of a family lash derives for its
        // own start paths is refused, never adopted (ADR 0107).
        // The request is registered as the host stated it: lash lays
        // nothing of its own under a session-turn start (FIG-4594), so the
        // start-key fence compares only what the host stated.
        let request = request
            .keyed_in(&scoped_effect_controller)
            .map_err(EmbedError::Plugin)?;
        // A root start's session grant is the host's lookup, whether it came
        // from `session_scope` or from a remote start's `until_session` data
        // (FIG-3607 R3). The start's recorded admission checks the session is
        // live, so a replay after the session was deleted answers the start
        // its first run made (ADR 0105 §1).
        let observers = request.observers.clone();
        let mut registration = request.into_registration();
        let host_session_turn_environment = match registration.input.as_ref() {
            lash_core::ProcessStartTarget::Input(lash_core::ProcessInput::SessionTurn {
                create_request,
                ..
            }) if registration.env_ref.is_none() => {
                Some(self.host_session_turn_environment(create_request)?)
            }
            _ => None,
        };
        let mut session_turn_admission = None;
        if registration.env_ref.is_some() || host_session_turn_environment.is_some() {
            let claim = lash_core::ReferrerClaim::guarded(lash_core::ReferrerGuard::Journal(
                scoped_effect_controller
                    .execution_scope()
                    .journal_identity()
                    .map_err(|error| lash_core::PluginError::Session(error.to_string()))?,
            ));
            if let lash_core::ProcessStartTarget::Input(lash_core::ProcessInput::SessionTurn {
                create_request,
                ..
            }) = registration.input.as_ref()
            {
                if let Some(environment) = host_session_turn_environment.as_ref() {
                    let env_ref = environment.stable_ref().map_err(|error| {
                        lash_core::PluginError::Session(format!(
                            "failed to encode process execution env: {error}"
                        ))
                    })?;
                    registration.env_ref = Some(env_ref);
                }
                session_turn_admission = Some(self.host_session_turn_admission(
                    create_request.as_ref().clone(),
                    host_session_turn_environment,
                    registration.env_ref.clone(),
                    claim,
                ));
            } else if let Some(env_ref) = registration.env_ref.as_ref() {
                self.core
                    .env
                    .core
                    .durability
                    .process_env_store
                    .acquire_process_execution_env(&claim, env_ref)
                    .await
                    .map_err(lash_core::PluginError::from)?;
            }
        }
        let start_key = registration.start_key.clone();
        let command = lash_core::ProcessCommand::Start {
            registration,
            observers,
            execution_context: Box::new(lash_core::ProcessExecutionContext::default()),
        };
        let outcome = self
            .execute_command(
                command,
                scoped_effect_controller.clone(),
                session_turn_admission,
            )
            .await
            .map_err(|error| EmbedError::Plugin(host_start_refusal(start_key.as_ref(), error)))?;
        let lash_core::ProcessEffectOutcome::Start {
            record,
            disposition,
        } = outcome
        else {
            return Err(EmbedError::Plugin(lash_core::PluginError::Session(
                "process start returned the wrong outcome".to_string(),
            )));
        };
        // The start's one delivery path ran inside the effect: registration
        // armed the obligation and the executor's `with_process_starts`
        // relay claimed and delivered it (ADR 0109 §1.5). A row it could not
        // deliver is settled on the ledger for the reconcile tick.
        Ok(lash_core::ProcessStartReceipt::of(&record, disposition))
    }

    /// Lists processes matching the supplied filter.
    pub async fn list(
        &self,
        filter: &lash_core::ProcessListFilter,
    ) -> Result<Vec<lash_core::facade_support::ObservedProcess>> {
        self.make_observer()?.list(filter).await.map_err(Into::into)
    }

    /// List processes a session may address — the **observer** filter.
    /// This is the visibility lens (what a session may see), distinct
    /// from [`list_originated_by`](Self::list_originated_by). `session.admin().processes()`
    /// is thin sugar over this method pre-scoped to the session's observer edge.
    pub async fn list_observed_by(
        &self,
        session_scope: &lash_core::SessionScope,
        filter: &lash_core::ProcessListFilter,
    ) -> Result<Vec<lash_core::facade_support::ObservedProcess>> {
        self.make_observer()?
            .list_observed_by(session_scope, filter)
            .await
            .map_err(Into::into)
    }

    /// List processes a session originated — the **provenance** filter (ADR
    /// 0011). This is the lineage lens (what a session created), distinct from
    /// [`list_observed_by`](Self::list_observed_by): a process a session started
    /// then transferred away still matches here, and one merely observed by it
    /// does not.
    pub async fn list_originated_by(
        &self,
        session_scope: &lash_core::SessionScope,
        filter: &lash_core::ProcessListFilter,
    ) -> Result<Vec<lash_core::facade_support::ObservedProcess>> {
        self.make_observer()?
            .list_originated_by(session_scope, filter)
            .await
            .map_err(Into::into)
    }

    pub async fn get(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<lash_core::facade_support::ObservedProcess>> {
        self.make_observer()?
            .process(process_id)
            .await
            .map_err(Into::into)
    }

    /// Read one durable event page from a process cursor, or from the start of
    /// the lifetime a process id currently names. Full/Lite is a request
    /// parameter; the returned cursor continues the history and resumes live
    /// observation.
    pub async fn events(
        &self,
        from: crate::process_observation::ProcessEventsFrom,
        limit: std::num::NonZeroUsize,
        mode: lash_core::ProcessEventQueryMode,
    ) -> Result<crate::process_observation::ProcessEventsRead> {
        Ok(crate::process_observation::read_events(
            &self.registry(),
            Some(self.core.process_observation_hub.as_ref()),
            from,
            limit,
            mode,
        )
        .await?)
    }

    /// Read a page for the exact lifetime named by a remote request.
    pub async fn events_remote(
        &self,
        request: &lash_remote_protocol::RemoteProcessEventsRequest,
    ) -> Result<lash_remote_protocol::RemoteProcessEventsResponse> {
        request.validate()?;
        let process_id = &request.process_id;
        let registry = self.registry();
        let cursor = match request.cursor.clone() {
            Some(cursor) => cursor,
            None => {
                let (epoch, position) = self.core.process_observation_hub.route(process_id);
                let version = registry
                    .fleet_format()
                    .writer_version(lash_core::surface_format!(
                        lash_sansio::PROCESS_CURSOR_VERSION
                    ));
                crate::process_observation::ProcessCursor::at_version(
                    version,
                    epoch,
                    lash_sansio::ProcessCursorReference::for_process(process_id),
                    position,
                    0,
                )
                .map_err(|error| {
                    EmbedError::Plugin(lash_core::PluginError::Session(error.to_string()))
                })?
            }
        };
        let outcome = registry
            .event_page_after(process_id, cursor.sequence(), request.limit, request.mode)
            .await?;
        let cursor = match &outcome {
            lash_core::ProcessEventReadOutcome::Retained(page) => page
                .last_sequence(|event| event.sequence, |event| event.sequence)
                .map_or_else(|| cursor.clone(), |sequence| cursor.with_sequence(sequence)),
            lash_core::ProcessEventReadOutcome::NoLongerRetained(_) => cursor,
        };
        Ok((process_id.clone(), outcome, cursor).try_into()?)
    }

    pub async fn await_output(
        &self,
        process_id: &ProcessId,
    ) -> Result<lash_core::ProcessAwaitOutput> {
        let process_id = self
            .core
            .process_registry()
            .require_process_id(process_id)
            .await?;
        let process_work = Arc::clone(self.core.substrate_slot.ports().await.process.port());
        Ok(await_process_terminal(process_work.as_ref(), &process_id).await?)
    }

    /// Requests cancellation of the identified process.
    pub async fn cancel(
        &self,
        process_id: &ProcessId,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessCancelReceipt> {
        #[expect(
            clippy::expect_used,
            reason = "an execution scope is a struct of opaque string identities, whose \
                      serialization has no failing case"
        )]
        let command = lash_core::ProcessCommand::Cancel {
            process_id: process_id.clone(),
            origin: lash_core::CancelOrigin::OperatorRequested,
            requester: serde_json::to_string(scoped_effect_controller.execution_scope()).expect(
                "an execution scope is a struct of opaque string identities, whose \
                     serialization has no failing case",
            ),
            attribution: None,
        };
        let outcome = self
            .run_command(command, scoped_effect_controller.clone())
            .await?;
        let lash_core::ProcessEffectOutcome::Cancel { record } = outcome else {
            return Err(EmbedError::Plugin(lash_core::PluginError::Session(
                "process cancel returned the wrong outcome".to_string(),
            )));
        };
        Ok(lash_core::ProcessCancelReceipt::from_record(*record)?)
    }

    /// Delivers one signal to the process its identity names.
    ///
    /// The signal's identity is its append key (FIG-4299): delivering the
    /// same signal again, from a retry, a redrive or another host, is served
    /// the event its first delivery admitted, and the same identity under a
    /// changed payload is refused as a durable-identity conflict.
    pub async fn signal(
        &self,
        signal: lash_core::ProcessSignal,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<lash_core::ProcessEvent> {
        let command = lash_core::ProcessCommand::Signal { signal };
        let outcome = self
            .run_command(command, scoped_effect_controller.clone())
            .await?;
        let lash_core::ProcessEffectOutcome::Signal { event } = outcome else {
            return Err(EmbedError::Plugin(lash_core::PluginError::Session(
                "process signal returned the wrong outcome".to_string(),
            )));
        };
        Ok(*event)
    }

    /// Returns the current process-session snapshot.
    pub async fn session_snapshot(
        &self,
        session_id: impl Into<SessionId>,
    ) -> Result<lash_core::facade_support::ProcessWorkSnapshot> {
        self.make_observer()?
            .snapshot_for_session(session_id)
            .await
            .map_err(Into::into)
    }

    pub fn observer(&self) -> Result<lash_core::facade_support::ProcessWorkObserver> {
        self.make_observer()
    }

    /// Cancel every currently-running process. A host-wide lever; for a
    /// session-scoped stop use [`SessionProcessAdmin::cancel_all`](crate::admin::SessionProcessAdmin::cancel_all).
    pub async fn cancel_all(
        &self,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<Vec<lash_core::ProcessCancelReceipt>> {
        let outcome = self
            .run_command(
                lash_core::ProcessCommand::List {
                    selection: lash_core::ProcessListSelection::HostRunning,
                },
                scoped_effect_controller.clone(),
            )
            .await?;
        let lash_core::ProcessEffectOutcome::List { entries: running } = outcome else {
            return Err(EmbedError::Plugin(lash_core::PluginError::Session(
                "process listing returned the wrong outcome".to_string(),
            )));
        };
        let mut summaries = Vec::with_capacity(running.len());
        for process in running {
            summaries.push(
                self.cancel(&process.id, scoped_effect_controller.clone())
                    .await?,
            );
        }
        Ok(summaries)
    }

    /// Processes are global; this re-homes only observer membership, never the process itself.
    pub async fn transfer(
        &self,
        from_scope: &lash_core::SessionScope,
        to_scope: &lash_core::SessionScope,
        process_ids: &[ProcessId],
    ) -> Result<()> {
        self.registry()
            .transfer_observers(
                &from_scope.session_id,
                &to_scope.session_id,
                process_ids,
                lash_core::ProcessObserverBy::host("admin-transfer"),
            )
            .await
            .map_err(Into::into)
    }

    /// Host-scheduled retention lever (ADR 0017): physically delete retired
    /// process rows (and their events, observer edges, leases) older than
    /// `cutoff_epoch_ms`, returning what was reclaimed. Retired is the terminal
    /// outcomes plus
    /// [`ProcessStatus::CallerDeparted`](lash_core::ProcessStatus::CallerDeparted),
    /// which nothing may ever honestly terminalize. The configured trigger
    /// store then removes exact delivery reservations for processes now
    /// represented by tombstones. In the same trigger-store transaction it
    /// reclaims empty-fan-out occurrences and trigger rows whose session owner
    /// has crossed the ADR 0049 deletion frontier. Host and platform name
    /// fences remain permanent. Live process rows — running and waiting — are
    /// never touched. Lash exposes no finite maximum waiter
    /// lifetime: the host must retain rows beyond every still-replayable await,
    /// and a later await after pruning receives the typed
    /// `ProcessNoLongerRetained` outcome. Pass
    /// either the projector's acknowledged
    /// [`ProjectionWatermark::UpTo`](lash_core::ProjectionWatermark::UpTo)
    /// cursor or an explicit
    /// [`ProjectionWatermark::NoProjector`](lash_core::ProjectionWatermark::NoProjector).
    ///
    /// `filter` narrows *which* eligible retired rows this call reclaims (ADR
    /// 0023): retention is differentiated host policy, so a host expresses
    /// "reclaim the work this deleted session originated" and "reclaim terminal
    /// subagent debris after a day" as two scheduled calls over the same lever.
    /// `None` considers every retired row. Because retention only ever deletes
    /// retired rows, a nonempty set containing only running and waiting
    /// statuses cannot match and is refused. This includes the running
    /// default selected by an otherwise unspecified filter.
    ///
    /// A process a trigger delivery registered stays pinned until the
    /// delivery's bind commits, so a completed child whose bind was lost is
    /// never pruned and started again (ADR 0021, FIG-4203). The pass first
    /// releases the pins of deliveries that are bound or no longer reserved,
    /// recovering any release lost after its bind, then prunes.
    pub async fn prune(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<&lash_core::ProcessListFilter>,
        watermark: lash_core::ProjectionWatermark,
    ) -> Result<lash_core::ProcessPruneReport> {
        let registry = self.registry();
        Self::prune_selection(filter)?;
        let trigger_store = self.core.env.core.trigger_store();
        if let Err(err) = lash_core::facade_support::release_bound_trigger_delivery_pins(
            registry.as_ref(),
            trigger_store.as_ref(),
        )
        .await
        {
            tracing::warn!(
                failure_stage = "release_bound_trigger_delivery_pins",
                cutoff_epoch_ms,
                error = %err,
                "process retention failed"
            );
            return Err(err.into());
        }
        // Survey exactly the rows the registry's prune will delete, with the
        // registry's own eligibility predicate (retired status, cutoff,
        // projection watermark, no pending wake delivery, no parent-end
        // plan, filter): a process the registry keeps keeps its journal and
        // its promises too. The fence lands before the row goes, so the
        // interval between prune and any re-registration of the same id is
        // covered (ADR 0049).
        let prunable = registry
            .prunable_terminal_processes(cutoff_epoch_ms, filter.cloned(), watermark)
            .await?;
        for process_id in prunable {
            let process_scope = lash_core::ExecutionScope::process(process_id.clone());
            // Usage accounting drains before the journal goes (ADR 0125): the
            // process's settlements are delivered and its owner retired, so
            // nothing admitted after this spends under a pruned process. A
            // process runtime spends under its own owner only.
            let owner = lash_core::RuntimeOwner::Process(process_id.clone());
            if let Err(err) = self
                .core
                .env
                .core
                .control
                .effect_host
                .drain_usage_accounting(&owner)
                .await
            {
                tracing::warn!(
                    failure_stage = "drain_process_usage_accounting",
                    cutoff_epoch_ms,
                    process_id = %process_id,
                    %owner,
                    error = %err,
                    "process retention failed"
                );
                return Err(err.into());
            }
            // This is the cancellation-admission serialization point. The
            // factory checks every persisted closure and writes the scope
            // tombstone under the same backend fence later authorization
            // uses. Either an existing/new pin makes this call fail, or
            // every later authorization is refused before it is written.
            self.core
                .store_factory
                .retire_turn_cancel_closure_scope(&process_scope)
                .await?;
            // The process journal: nothing can replay it once the row is
            // gone (FIG-2500). The retirement also retires the scope's
            // await-event promises and leaves the scope fence (FIG-2499). The
            // registry's verdict is the unreachability proof, so the
            // owner-terminal gate applies and in-flight rows go with the rest.
            let retirements = [lash_core::EffectJournalRetirement::for_scope(
                &process_scope,
            )];
            for retirement in retirements.into_iter().flatten() {
                if let Err(err) = self
                    .core
                    .env
                    .core
                    .control
                    .effect_host
                    .retire_effect_journal(retirement)
                    .await
                {
                    tracing::warn!(
                        failure_stage = "retire_process_effect_journal",
                        cutoff_epoch_ms,
                        process_id = %process_id,
                        error = %err,
                        "process retention failed"
                    );
                    return Err(err.into());
                }
            }
        }
        let mut report = match registry
            .prune_terminal_processes(cutoff_epoch_ms, filter.cloned(), watermark)
            .await
        {
            Ok(report) => report,
            Err(err) => {
                tracing::warn!(
                    failure_stage = "prune_process_registry",
                    cutoff_epoch_ms,
                    error = %err,
                    "process retention failed"
                );
                return Err(err.into());
            }
        };
        let retention = match lash_core::facade_support::reconcile_pruned_trigger_deliveries(
            registry.as_ref(),
            trigger_store.as_ref(),
            Some(self.core.store_factory.as_ref()),
        )
        .await
        {
            Ok(retention) => retention,
            Err(err) => {
                tracing::warn!(
                    failure_stage = "reconcile_trigger_deliveries_after_process_prune",
                    cutoff_epoch_ms,
                    pruned_processes = report.pruned_processes,
                    pruned_events = report.pruned_events,
                    error = %err,
                    "process retention partially completed"
                );
                return Err(err.into());
            }
        };
        report.pruned_trigger_deliveries = retention.reclaimed_delivery_count;
        tracing::info!(
            reclaimed_trigger_deliveries = retention.reclaimed_delivery_count,
            reclaimed_trigger_occurrences = retention.reclaimed_occurrence_count,
            reclaimed_trigger_subscriptions = retention.reclaimed_subscription_count,
            reclaimed_trigger_mutation_receipts = retention.reclaimed_mutation_receipt_count,
            "completed trigger retention after process prune"
        );
        Ok(report)
    }

    /// Reclaim terminal trigger occurrences armed no later than the cutoff.
    /// Each delete writes a tombstone on the store's clock. Reclaim never
    /// deletes a tombstone, including when the cutoff is `u64::MAX`.
    pub async fn reclaim_trigger_occurrences(
        &self,
        cutoff_epoch_ms: u64,
    ) -> lash_core::TriggerOccurrenceReclamationResult {
        self.core
            .env
            .core
            .trigger_store()
            .reclaim_trigger_occurrences(cutoff_epoch_ms)
            .await
    }

    /// Delete exactly the trigger-occurrence tombstones written strictly
    /// before `written_before_epoch_ms` on the configured store's clock.
    /// Returns the number removed. Tombstones are never deleted automatically.
    ///
    /// The host vouches that its trigger source will no longer redeliver those
    /// occurrences. A later redelivery of a forgotten occurrence runs as a
    /// new occurrence; identities whose tombstones remain are still refused.
    ///
    /// Store failures, including writer-fence and contention causes, remain
    /// typed as [`EmbedError::Store`](crate::EmbedError::Store). A failed
    /// transaction removes nothing.
    pub async fn forget_trigger_tombstones(&self, written_before_epoch_ms: u64) -> Result<usize> {
        self.core
            .env
            .core
            .trigger_store()
            .forget_trigger_tombstones(written_before_epoch_ms)
            .await
            .map_err(Into::into)
    }

    /// Compact payload-free process tombstones while structurally excluding
    /// every process id referenced by an outstanding trigger delivery.
    /// Reconciliation runs first, then the raw registry compaction lever surveys
    /// the configured trigger store itself and refuses every matching tombstone.
    /// A configured trigger store that cannot be surveyed or reconciled blocks
    /// compaction, so tombstones accumulate until it recovers rather than
    /// allowing recovery evidence to become orphaned. The caller supplies the
    /// same explicit projection watermark required by the registry retention
    /// contract.
    pub async fn compact_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: lash_core::ProjectionWatermark,
    ) -> Result<usize> {
        let registry = self.registry();
        let trigger_store = self.core.env.core.trigger_store();
        let retention_candidates = match trigger_store.list_delivery_retention_candidates().await {
            Ok(candidates) => candidates,
            Err(err) => {
                tracing::warn!(
                    failure_stage = "survey_outstanding_trigger_deliveries",
                    cutoff_epoch_ms,
                    error = %err,
                    "process tombstone compaction blocked"
                );
                return Err(err.into());
            }
        };
        let surveyed_trigger_store =
            SurveyedTriggerStore::new(trigger_store.as_ref(), retention_candidates);
        let reconciled_trigger_deliveries =
            match lash_core::facade_support::reconcile_pruned_trigger_deliveries(
                registry.as_ref(),
                &surveyed_trigger_store,
                Some(self.core.store_factory.as_ref()),
            )
            .await
            {
                Ok(retention) => retention.reclaimed_delivery_count,
                Err(err) => {
                    tracing::warn!(
                        failure_stage = "reconcile_trigger_deliveries_before_compaction",
                        cutoff_epoch_ms,
                        protected_process_count = surveyed_trigger_store.protected_process_count(),
                        error = %err,
                        "process tombstone compaction blocked"
                    );
                    return Err(err.into());
                }
            };
        tracing::debug!(
            protected_process_count = surveyed_trigger_store.protected_process_count(),
            reconciled_trigger_deliveries,
            "prepared delivery-aware process tombstone compaction"
        );

        match registry
            .compact_process_tombstones(
                cutoff_epoch_ms,
                watermark,
                Some(&surveyed_trigger_store as &dyn lash_core::TriggerStore),
            )
            .await
        {
            Ok(compacted) => Ok(compacted),
            Err(err) => {
                tracing::warn!(
                    failure_stage = "compact_process_tombstones",
                    cutoff_epoch_ms,
                    protected_process_count = surveyed_trigger_store.protected_process_count(),
                    error = %err,
                    "process tombstone compaction failed"
                );
                Err(err.into())
            }
        }
    }

    /// List durable process-wake delivery rows, optionally filtered by state.
    pub async fn wake_deliveries(
        &self,
        state: Option<lash_core::WakeDeliveryState>,
    ) -> Result<Vec<lash_core::WakeDelivery>> {
        self.registry()
            .list_wake_deliveries(state)
            .await
            .map_err(Into::into)
    }

    /// Summarize delivery states and name blocked groups with their redrive ids.
    pub async fn wake_delivery_report(&self) -> Result<lash_core::WakeDeliveryReport> {
        self.registry()
            .wake_delivery_report()
            .await
            .map_err(Into::into)
    }

    /// Explicitly return a discarded delivery to the pending lane.
    pub async fn redrive_wake_delivery(&self, delivery_id: &str) -> Result<()> {
        self.registry()
            .redrive_wake_delivery(delivery_id)
            .await
            .map_err(Into::into)
    }

    pub async fn drive_wake_deliveries(
        &self,
    ) -> Result<lash_core::facade_support::WakeDeliveryDriveReport> {
        let ports = self.core.substrate_slot.ports().await;
        ports.queued.drive_wake().await.map_err(Into::into)
    }
}
/// A conflicting host start names only its key (ADR 0107). Every other
/// refusal retains the controller error, including its cause and evidence.
pub(crate) fn host_start_refusal(
    start_key: Option<&lash_core::StartKey>,
    error: lash_core::RuntimeEffectControllerError,
) -> lash_core::PluginError {
    if let Some(start_key) = start_key
        && (error.code == lash_core::RuntimeErrorCode::ProcessStartKeyConflict
            || (start_key.is_host_supplied() && error.code.is_replay_mismatch()))
    {
        return lash_core::PluginError::StartKeyConflict {
            start_key: start_key.clone(),
        };
    }
    lash_core::PluginError::RuntimeEffectController(error)
}

#[cfg(test)]
mod terminal_wait_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ReattachOnce {
        waits: AtomicUsize,
        terminal: lash_core::ProcessAwaitOutput,
    }

    #[async_trait::async_trait]
    impl lash_core::ProcessWorkSubstrate for ReattachOnce {
        async fn deliver_process_start(
            &self,
            _record: &lash_core::ProcessRecord,
        ) -> std::result::Result<(), lash_core::PluginError> {
            unreachable!("terminal-wait witness does not deliver starts")
        }

        async fn await_process_terminal(
            &self,
            process_id: &lash_core::ProcessId,
        ) -> std::result::Result<lash_core::ProcessTerminalWait, lash_core::PluginError> {
            assert_eq!(
                process_id,
                &lash_core::ProcessId::fixture("admin-reattach-process")
            );
            if self.waits.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(lash_core::ProcessTerminalWait::Reattach)
            } else {
                Ok(lash_core::ProcessTerminalWait::Terminal(
                    self.terminal.clone(),
                ))
            }
        }

        async fn deliver_cancel(
            &self,
            _process_id: &lash_core::ProcessId,
            _request: &lash_core::CancelRequest,
            _key: &str,
        ) -> std::result::Result<(), lash_core::PluginError> {
            unreachable!("terminal-wait witness does not deliver cancels")
        }

        async fn publish_process_terminal(
            &self,
            process_id: &lash_core::ProcessId,
            output: &lash_core::ProcessAwaitOutput,
            key: &str,
        ) -> std::result::Result<(), lash_core::PluginError> {
            let _ = (process_id, output, key);
            Ok(())
        }
    }

    #[tokio::test]
    async fn process_admin_reattaches_once_then_returns_terminal_output() {
        let terminal = lash_core::ProcessAwaitOutput::from_tool_output(
            lash_core::ToolCallOutput::success(serde_json::json!({"done": true})),
        );
        let port = ReattachOnce {
            waits: AtomicUsize::new(0),
            terminal: terminal.clone(),
        };

        let output = await_process_terminal(
            &port,
            &lash_core::ProcessId::fixture("admin-reattach-process"),
        )
        .await
        .expect("reattachment reaches terminal output");

        assert_eq!(output, terminal);
        assert_eq!(port.waits.load(Ordering::SeqCst), 2);
    }
}

#[cfg(test)]
mod host_start_refusal_tests {
    use super::host_start_refusal;
    use lash_core::{PluginError, RuntimeEffectControllerError, RuntimeErrorCode};

    #[test]
    fn a_host_start_preserves_the_controller_refusal_and_its_class() {
        let mut errors = vec![
            lash_core::StoreError::WriterFenced {
                recorded: 101,
                writable: lash_core::compat::VersionRange::exactly(100),
            }
            .into(),
        ];
        for code in [
            RuntimeErrorCode::ProcessParentEnded,
            RuntimeErrorCode::TriggerDeliveryBound,
            RuntimeErrorCode::TriggerDeliveryRetired,
            RuntimeErrorCode::SessionHeadOwned,
            RuntimeErrorCode::RuntimeStore,
            RuntimeErrorCode::ReasoningRefused,
            RuntimeErrorCode::LlmProfileUnknown,
        ] {
            errors.push(RuntimeEffectControllerError::new(code, "start refused"));
        }
        for mut error in errors {
            error.summary = Some(Box::new(lash_core::RuntimeEffectReplayMismatchReport {
                divergent_path_count: 1,
                first_divergent_paths: vec!["command.registration".into()],
                effect_kind: Some("process".into()),
            }));
            error.journaled = true;
            let expected = serde_json::to_value(&error).expect("controller error serializes");
            let start_key = lash_core::StartKey::for_host("refused-host-start");
            for key in [None, Some(&start_key)] {
                let mapped = host_start_refusal(key, error.clone());
                let PluginError::RuntimeEffectController(actual) = mapped else {
                    panic!("the host must retain the controller refusal: {mapped:?}");
                };
                assert_eq!(
                    serde_json::to_value(&actual).expect("mapped error serializes"),
                    expected
                );
                assert!(actual.journaled);
                let host = crate::EmbedError::Plugin(PluginError::RuntimeEffectController(actual));
                assert_eq!(
                    host.is_retryable(),
                    PluginError::RuntimeEffectController(error.clone()).is_retryable()
                );
                assert_eq!(host.is_terminal(), error.is_terminal());
            }
        }
    }
}
