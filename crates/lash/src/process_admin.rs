//! The global process facade surface.
//!
//! [`Processes`] (reached via [`LashCore::processes`](crate::LashCore::processes),
//! re-exported as [`lash::process::Processes`](crate::process::Processes)) is THE
//! host-level process surface (ADR 0014 grill): start, observe, cancel,
//! transfer, prune, and abandon-request every process, with the two distinct
//! scope filters — `observed_by` (what a session may address) and `originated_by`
//! (what a session created). The session-scoped
//! [`SessionProcessAdmin`](crate::process::SessionProcessAdmin) is thin sugar over
//! this surface pre-filtered by a session's observer edge; it lives in `admin` because it
//! owns its session scope and runtime access.

use crate::support::{Arc, EmbedError, LashCore, Result};
use lash_core::ActorContext;

use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::ProcessId;

async fn await_process_terminal(
    process_work: &dyn lash_core::ProcessWorkSubstrate,
    process_id: &ProcessId,
) -> std::result::Result<
    lash_execution_contracts::process::ProcessAwaitOutput,
    lash_execution_contracts::plugin_error::PluginError,
> {
    loop {
        match process_work.await_process_terminal(process_id).await? {
            lash_core::ProcessTerminalWait::Terminal(output) => return Ok(output),
            lash_core::ProcessTerminalWait::Reattach => continue,
        }
    }
}

#[derive(Clone)]
/// Manages durable processes through a configured Lash core.
pub struct Processes {
    pub(crate) core: LashCore,
}

/// A process's immutable recorded authority and provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessEnvView {
    pub process: ProcessId,
    pub authority: ProcessAuthority,
    pub provenance: lash_execution_contracts::process::ProcessProvenance,
}

/// Engine processes carry an explicit environment; session-turn processes
/// carry no environment and run their child session's own configuration.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessAuthority {
    Engine {
        env_ref: lash_core::ProcessEnvRef,
        env: Box<lash_core::ProcessEnv>,
        engine_settings: Option<lash_execution_contracts::process::EngineSettings>,
        /// The components `env` names that this core declares retired, in
        /// env order: the process builds without them (FIG-6296).
        retired: Vec<lash_kernel_value::providers::ProcessComponentId>,
    },
    SessionTurn {
        session: SessionId,
    },
}

fn decode_environment(
    env_ref: &lash_core::ProcessEnvRef,
    bytes: &[u8],
) -> Result<lash_core::ProcessEnv> {
    if lash_core_store::process_identity::process_env_ref_for_bytes(bytes) != *env_ref {
        return Err(crate::plugins::ProcessEnvLoadError::Mismatched(env_ref.clone()).into());
    }
    lash_core::ProcessEnv::from_store_bytes(bytes).map_err(|error| {
        crate::plugins::ProcessEnvLoadError::Undecodable {
            env_ref: env_ref.clone(),
            message: error.to_string(),
        }
        .into()
    })
}

impl Processes {
    /// The node holding this process's committed actor row, or `None` when
    /// no node holds it (including when the actor does not exist).
    ///
    /// This uncached read never claims or wakes the process. The answer may
    /// immediately become stale; routing by it is optional, and requests sent
    /// to any node still reach the owner through the store.
    pub async fn owner_node(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<crate::durable::NodeId>> {
        let Ok(actor) = crate::durable::ActorKey::process(process_id.as_str()) else {
            return Ok(None);
        };
        Ok(self
            .core
            .backend()
            .durable()
            .actor(&actor)
            .await?
            .and_then(|snapshot| snapshot.owner)
            .map(|owner| owner.node))
    }

    /// Read a retained tool call by the typed id on its process effect
    /// occurrence. Reads committed round outcomes, including after reopening
    /// the deployment; it runs no engine, tool, hook or current catalog lookup.
    /// Unknown calls, unfinished calls and outcomes without retained material
    /// return `None`. Process pruning retires these records with their owner.
    pub async fn tool_call(
        &self,
        process_id: &ProcessId,
        call_id: &crate::ToolCallId,
    ) -> Result<Option<crate::tools::ToolCallRecord>> {
        let host = self.core.actor_context();
        if let Some(record) =
            lash_core::facade_support::read_process_tool_call(&host, process_id, call_id).await?
        {
            return Ok(Some(record));
        }
        Ok(self
            .saved_reads(process_id)
            .await?
            .into_iter()
            .find(|record| record.call_id == *call_id))
    }

    /// Saved reads in caller ordinal order, independent of the bounded live
    /// replay store. Terminal processes retain these records until pruning.
    /// This offline query runs no engine, provider, hook or catalog lookup.
    pub async fn saved_reads(
        &self,
        process_id: &ProcessId,
    ) -> Result<Vec<crate::tools::ToolCallRecord>> {
        let host = self.core.actor_context();
        let kernel_run = format!(
            "{}/{}",
            lash_vm_runtime::LASH_VM_ENGINE_KIND,
            lash_vm_runtime::KERNEL_RUN_STEP
        );
        let mut reads = Vec::new();
        for step in lash_core::facade_support::read_process_steps(&host, process_id).await? {
            if step.tool != kernel_run {
                continue;
            }
            let Some(payload) = step
                .outcome
                .as_ref()
                .and_then(lash_core_store::tool_run::SettledOutput::payload)
            else {
                continue;
            };
            reads.extend(
                lash_vm_runtime::process::saved_read_facts(payload).map_err(|error| {
                    EmbedError::Durable(lash_core::durable_port::DurableError::Store(
                        lash_core::durable_port::StoreFailure {
                            code: None,
                            kind: lash_core::durable_port::StoreFailureKind::Corrupt,
                            message: format!("the kernel run's saved read facts: {error}"),
                        },
                    ))
                })?,
            );
        }
        reads.sort_by_key(|fact| fact.ordinal);
        Ok(reads.into_iter().map(|fact| fact.record()).collect())
    }

    /// Replay recorded run segments of a process observed at `counts`, on
    /// demand: each chosen segment re-runs from the start its journal
    /// records, its host reads answered from its recorded tape, and answers
    /// the statement path it verified, or why it has none
    /// ([`Unreplayable`](crate::workflow::Unreplayable)). It is a read: a
    /// replay performs nothing and records nothing, and a park ends it.
    /// An unknown process, or one of an engine that replays nothing,
    /// answers no segment; a pruned process answers each segment it was
    /// asked for `Pruned`.
    #[cfg(feature = "codemode")]
    pub async fn replay(
        &self,
        process_id: &ProcessId,
        segments: crate::workflow::ReplaySegments,
    ) -> Result<Vec<crate::workflow::SegmentReplay>> {
        use crate::workflow::{ReplaySegments, SegmentAnswer, SegmentReplay, Unreplayable};
        let pruned = |segments: &ReplaySegments| match segments {
            ReplaySegments::All => Vec::new(),
            ReplaySegments::Only(segments) => segments
                .iter()
                .map(|segment| SegmentReplay {
                    segment: *segment,
                    answer: SegmentAnswer::Unreplayable {
                        reason: Unreplayable::Pruned,
                    },
                })
                .collect(),
        };
        // The steps are read before the record: a record still retained
        // after them proves the steps read were retained too.
        let host = self.core.actor_context();
        let steps = lash_core::facade_support::read_process_steps(&host, process_id).await?;
        let process = match self.make_observer().process(process_id).await {
            Ok(Some(process)) => process,
            Ok(None)
            | Err(lash_execution_contracts::plugin_error::PluginError::ProcessUnknown { .. }) => {
                return Ok(Vec::new());
            }
            Err(lash_execution_contracts::plugin_error::PluginError::ProcessNoLongerRetained {
                ..
            }) => {
                return Ok(pruned(&segments));
            }
            Err(error) => return Err(error.into()),
        };
        let lash_execution_contracts::process::ProcessInput::Engine {
            kind,
            payload,
            launch,
            ..
        } = &process.input
        else {
            return Ok(Vec::new());
        };
        let Some(provider) = self.core.host_process_engines.document_provider(kind) else {
            return Ok(Vec::new());
        };
        let request = lash_vm_runtime::ProcessReplayRequest {
            pages: host.pages()?,
            process: process_id.clone(),
            settings: launch.settings.clone(),
            payload: payload.clone(),
            steps,
            segments,
        };
        provider
            .replay(lash_execution_contracts::process::ProcessDocument::new(
                request,
            ))
            .await?
            .downcast::<Vec<SegmentReplay>>()
            .map_err(|_| {
                lash_execution_contracts::plugin_error::PluginError::Invoke(format!(
                    "the `{kind}` engine answered a replay with another type"
                ))
                .into()
            })
    }

    /// One process, observed: its durable snapshot and the recovering feed
    /// that continues it from the core's process replay store.
    pub fn observe(&self, process_id: &ProcessId) -> crate::process_feed::ObservableProcess {
        crate::process_feed::ObservableProcess {
            source: crate::process_feed::ProcessFeedSource::new(
                process_id.clone(),
                self.registry(),
                self.make_observer(),
                self.core.host_process_engines.clone(),
                Arc::clone(&self.core.process_replay_store),
                self.core.env.core.observation_work_limits,
                crate::process_feed::FeedReconcile {
                    publisher: Arc::clone(&self.core.language_observation_publisher),
                    changes: self.core.process_emissions().clone(),
                },
            )
            .with_effect_retention(
                self.core
                    .env
                    .core
                    .tracing
                    .evidence_limits()
                    .process_effect_occurrences(),
            ),
        }
    }

    fn registry(&self) -> Arc<dyn lash_execution_contracts::process::ProcessRegistry> {
        self.core.process_registry.clone()
    }

    fn make_observer(&self) -> lash_execution_contracts::process::ProcessWorkObserver {
        lash_execution_contracts::process::ProcessWorkObserver::new(self.registry())
            .with_actor_parks(Arc::clone(self.core.backend.durable()))
            .with_read_attempts(self.core.observer_pacing.snapshot_read_attempts)
            .with_work_limits(self.core.env.core.observation_work_limits)
    }

    /// The listing filter [`prune`](Self::prune) surveys effect-journal
    /// retirement candidates with. It is the caller's retention filter verbatim
    /// so the journals retired are exactly the rows the prune can delete, and
    /// absence widens to every status rather than the `Running` default.
    fn prune_selection(
        filter: Option<&lash_execution_contracts::process::ProcessListFilter>,
    ) -> Result<lash_execution_contracts::process::ProcessListFilter> {
        let Some(filter) = filter else {
            return Ok(lash_execution_contracts::process::ProcessListFilter {
                status: lash_execution_contracts::process::ProcessStatusFilter::Any,
                ..lash_execution_contracts::process::ProcessListFilter::default()
            });
        };
        if matches!(&filter.status, lash_execution_contracts::process::ProcessStatusFilter::In(statuses) if !statuses.is_empty() && statuses.iter().all(|status| !status.is_retired()))
        {
            return Err(EmbedError::Plugin(
                lash_execution_contracts::plugin_error::PluginError::Session(format!(
                    "process retention filter selects the live status set `{:?}`, \
                     which no prunable row can hold; pass \
                     `ProcessStatusFilter::Any` or a set containing a retired status",
                    filter.status
                )),
            ));
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
        scoped_effect_controller: ActorContext,
    ) -> Result<lash_core::ProcessEffectOutcome> {
        self.execute_command(command, scoped_effect_controller, None, None)
            .await
            .map_err(|err| EmbedError::Plugin(err.into()))
    }

    async fn execute_command(
        &self,
        command: lash_core::ProcessCommand,
        scoped_effect_controller: ActorContext,
        session_turn_admission: Option<lash_core::runtime::SessionTurnAdmission>,
        phases: Option<Arc<lash_core::runtime::StartPhaseClock>>,
    ) -> std::result::Result<lash_core::ProcessEffectOutcome, lash_core::RuntimeEffectControllerError>
    {
        let registry = self.registry();
        let process_work = Arc::clone(self.core.substrate_slot.ports().await.process.port());
        let invocation =
            Self::process_invocation(&command, scoped_effect_controller.execution_scope());
        let outcome = scoped_effect_controller
            .process_effect(
                lash_core::RuntimeEffectEnvelope::new(
                    invocation,
                    lash_core::RuntimeEffectCommand::process(command),
                ),
                lash_core::RuntimeEffectLocalExecutor::processes(
                    registry,
                    process_work,
                    self.core.host_process_engines.clone(),
                    lash_core::runtime::HostStartAdmission {
                        tracing: Some(self.core.env.core.tracing.clone()),
                        session_catalog: Some(Arc::clone(&self.core.store_factory) as _),
                        session_turn_admission,
                        phases,
                    },
                )
                .with_process_actor_parks(Arc::clone(self.core.backend.durable()))
                .with_process_attachments(self.core.backend.attachment_referrers())
                .with_process_env_store(Arc::clone(
                    &self.core.env.core.durability.process_env_store,
                )),
            )
            .await?;
        match outcome {
            lash_core::RuntimeEffectOutcome::Process { result } => Ok(result),
            _ => Err(lash_core::RuntimeEffectControllerError::from(
                lash_execution_contracts::plugin_error::PluginError::Session(
                    "process effect returned non-process outcome".to_string(),
                ),
            )),
        }
    }

    /// The scope a host start may live until: `session_id`, looked up now.
    ///
    /// A host start is a root: it has no starter, so its lifetime is
    /// [`Lifetime::Detached`](lash_execution_contracts::process::Lifetime::Detached) or `Until` a
    /// session this lookup grants (FIG-3607 R3). A process started `Until` it
    /// is cancelled when the session is deleted, and its descendants inherit
    /// the session as their session capability.
    ///
    /// # Errors
    ///
    /// [`EmbedError::UnknownSession`] when the session does not exist or was
    /// deleted.
    pub async fn session_scope(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_execution_contracts::process::ScopeRef> {
        self.require_live_session(session_id).await?;
        Ok(lash_execution_contracts::process::ScopeRef::host_session_lookup(session_id.clone()))
    }

    async fn require_live_session(&self, session_id: &SessionId) -> Result<()> {
        let live = lash_execution_contracts::deployment::session_is_live(
            self.core.store_factory.as_ref(),
            session_id,
        )
        .await?;
        if live {
            Ok(())
        } else {
            Err(EmbedError::UnknownSession {
                session_id: session_id.clone(),
            })
        }
    }

    /// Check a host's session-turn start before anything is registered. A
    /// session-turn process carries no environment (FIG-6034 D5): its child
    /// session runs under the configuration its create request states. No
    /// session starts it, so nothing recorded stands beneath it and the core
    /// keeps no default (FIG-4594): the request states its policy and model,
    /// and the child's plugin config is admitted here, on the core's plugin
    /// set.
    ///
    /// # Errors
    ///
    /// [`EmbedError::SessionTurnStartUnspecified`] when the request leaves
    /// its policy or its model unstated.
    fn check_host_session_turn(
        &self,
        create_request: &lash_execution_contracts::session_types::SessionCreateRequest,
    ) -> Result<()> {
        if let Some(unstated) = create_request.unstated_root_config() {
            return Err(EmbedError::SessionTurnStartUnspecified { unstated });
        }
        if create_request.policy.is_none() {
            return Err(EmbedError::SessionTurnStartUnspecified {
                unstated: lash_execution_contracts::session_types::UnstatedSessionConfig::Policy,
            });
        }
        crate::support::build_plugin_host(
            self.core.protocol_factory.as_ref(),
            self.core.plugin_factories.as_ref(),
            &self.core.env.core,
        )?
        .resolve_creation_plugin_config(
            self.core
                .protocol_factory
                .as_ref()
                .map(|protocol_factory| protocol_factory.id()),
            &create_request.plugin_options,
            // The resolution only validates the request: nothing is written.
            &lash_core::store::plugin_writers::PluginAdmission::default(),
        )
        .map_err(lash_core::SessionError::from)?;
        Ok(())
    }

    fn host_session_turn_admission(
        &self,
        request: lash_execution_contracts::session_types::SessionCreateRequest,
    ) -> lash_core::runtime::SessionTurnAdmission {
        let core = self.core.clone();
        Arc::new(move |fresh, _claim| {
            let core = core.clone();
            let request = request.clone();
            Box::pin(async move {
                if fresh && let Some(key) = request.model.as_ref() {
                    // The reasoning the key's binding runs with: the
                    // request's own, else the default selection. Nothing
                    // beneath the request supplies one (ADR 0134).
                    let reasoning = request.reasoning.clone().unwrap_or_default();
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
                Ok(())
            })
        })
    }

    /// Start a process, refusing a session-turn child's model key whose
    /// reasoning is unsupported before any process is written. The refusal
    /// retains `RuntimeErrorCode::ReasoningRefused`.
    ///
    /// An engine or definition start states its launch: the environment the
    /// host published under a pin ([`HostArtifacts::publish_process_env`](crate::artifacts::HostArtifacts::publish_process_env))
    /// and its engine settings, which staging holds under the start's key
    /// and seals onto the row; lash fills in neither (FIG-6034).
    ///
    /// A definition id is a content address, not a permission: lash does not check
    /// that the caller ran or holds the definition; authorizing creation from an id
    /// is the host's decision.
    ///
    /// A session-turn start carries no environment: it is a root creation,
    /// and takes an explicit spec like every other (FIG-4594): build its
    /// create request with
    /// [`SessionCreateRequest::with_spec`](lash_execution_contracts::session_types::SessionCreateRequest::with_spec).
    /// One that leaves its policy or model unstated is refused with
    /// [`EmbedError::SessionTurnStartUnspecified`] and nothing is
    /// registered. The start is registered exactly as stated, so a retry
    /// under its host key is returned the retained process whatever the
    /// host passes to other starts by then.
    /// `operation` is the idempotency identity of this host ingress, retained
    /// by the caller for retries. Lash binds it to this core.
    pub async fn start(
        &self,
        operation: crate::OperationId,
        request: lash_execution_contracts::process::ProcessStartRequest,
    ) -> Result<lash_execution_contracts::process::ProcessStartReceipt> {
        let scoped_effect_controller = self.operation_context(&operation)?;
        let request = operation.key_start(request);
        // The start's `lash.process.start` producer opens here, before the
        // first await, under the caller's context the request chose (else
        // the ambient one): the process is admitted under it.
        let mut producer = self.core.env.core.tracing.open_producer(
            lash_core::ProducerKind::ProcessStart,
            request.trace_call(),
            lash_core::TraceIds::default(),
        );
        let request = request.with_trace_parent(producer.parent());
        // The phases the start's producer attributes its time to, from its
        // opening: timed only when something observes the start.
        let tracing = &self.core.env.core.tracing;
        let phases = tracing.is_observed().then(|| {
            Arc::new(lash_core::runtime::StartPhaseClock::new(Arc::clone(
                tracing.clock(),
            )))
        });
        let started = async {
            // The operation identity keys registration. The registrar mints
            // the process id and fences the request's stated content.
            // A root start's session grant is the host's lookup, whether it
            // came from `session_scope` or from a remote start's
            // `until_session` data (FIG-3607 R3). The start's recorded
            // admission checks the session is live, so a replay after the
            // session was deleted answers the start its first run made (ADR
            // 0105 §1).
            let observers = request.observers.clone();
            let registration = request.into_registration();
            // A host start is its own operation (ADR 0113 §3.3): its staging
            // holds the stated environment under `Start(key)` until
            // registration carries it onto the record. The core-bound
            // operation context holds no actor execution claim of its own,
            // so preparation is retained independently under the start key.
            let mut session_turn_admission = None;
            if let lash_execution_contracts::process::ProcessStartTarget::Input(
                lash_execution_contracts::process::ProcessInput::SessionTurn {
                    create_request, ..
                },
            ) = registration.input.as_ref()
            {
                self.check_host_session_turn(create_request)?;
                session_turn_admission =
                    Some(self.host_session_turn_admission(create_request.as_ref().clone()));
            }
            let start_key = registration.start_key.clone();
            let command = lash_core::ProcessCommand::Start {
                registration,
                observers,
                execution_context: Box::new(
                    lash_execution_contracts::process::ProcessExecutionContext::default(),
                ),
            };
            let outcome = self
                .execute_command(
                    command,
                    scoped_effect_controller.clone(),
                    session_turn_admission,
                    phases.clone(),
                )
                .await
                .map_err(|error| {
                    EmbedError::Plugin(host_start_refusal(start_key.as_ref(), error))
                })?;
            let lash_core::ProcessEffectOutcome::Start {
                record,
                disposition,
            } = outcome
            else {
                return Err(EmbedError::Plugin(
                    lash_execution_contracts::plugin_error::PluginError::Session(
                        "process start returned the wrong outcome".to_string(),
                    ),
                ));
            };
            Ok((record, disposition, start_key))
        }
        .await;
        if let Some(phases) = &phases {
            producer.timed(phases.phases());
        }
        let (record, disposition, start_key) = match started {
            Ok(started) => started,
            Err(error) => {
                producer.settle(crate::tracing::refused(&error));
                return Err(error);
            }
        };
        producer.ids_mut().process = Some(record.process_id.clone());
        producer.settle(match &disposition {
            lash_execution_contracts::process::ProcessRegistrationOutcome::Created => {
                lash_core::ProducerOutcome::Selected
            }
            lash_execution_contracts::process::ProcessRegistrationOutcome::Existing => {
                lash_core::ProducerOutcome::Reused
            }
        });
        // Registration created the process's actor ready, in the same
        // transaction: nothing is left to deliver.
        Ok(lash_execution_contracts::process::ProcessStartReceipt {
            process_id: record.process_id,
            start_key,
            disposition,
        })
    }

    /// Reads one bounded fleet page. Follow every continuation, including on
    /// empty filtered pages, then drain global changes after `change_cursor`.
    /// A pruned cursor requires discarding the roster and starting a new scan.
    /// The scan is convergent, rather than a point-in-time snapshot. The host
    /// authorizes this unscoped read and reapplies its selection to changes.
    pub async fn list(
        &self,
        filter: &lash_execution_contracts::process::ProcessListFilter,
        limit: std::num::NonZeroUsize,
        continuation: Option<lash_execution_contracts::process::ProcessRosterCursor>,
    ) -> Result<lash_execution_contracts::process::ProcessRosterPage> {
        self.make_observer()
            .list(filter, limit, continuation)
            .await
            .map_err(Into::into)
    }

    /// List processes a session may address — the **observer** filter.
    /// This is the visibility lens (what a session may see), distinct
    /// from [`list_originated_by`](Self::list_originated_by). `session.admin().processes()`
    /// is thin sugar over this method pre-scoped to the session's observer edge.
    pub async fn list_observed_by(
        &self,
        session_scope: &lash_execution_contracts::process::SessionScope,
        filter: &lash_execution_contracts::process::ProcessListFilter,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>> {
        self.make_observer()
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
        session_scope: &lash_execution_contracts::process::SessionScope,
        filter: &lash_execution_contracts::process::ProcessListFilter,
    ) -> Result<Vec<lash_execution_contracts::process::ObservedProcess>> {
        self.make_observer()
            .list_originated_by(session_scope, filter)
            .await
            .map_err(Into::into)
    }

    /// Read the roster changes after `after`, oldest first: each process
    /// whose row changed, as it now stands, and the tombstone of each pruned
    /// one. A page converges on the latest row of a process; a process's own
    /// facts are its event pages. Each entry carries its backend-issued opaque
    /// `cursor`. A host can commit that entry's projection effect and cursor
    /// atomically in its own database, then resume after it following a crash.
    /// Lash provides no cross-database transaction. Persist page `next` only
    /// after applying every entry; empty pages still carry `next` and the
    /// retention horizon. A pruned cursor requires a fresh roster scan.
    ///
    /// The read covers every process on the store, whatever session observes
    /// it, so the host must authorize access.
    pub async fn changed_since(
        &self,
        after: lash_execution_contracts::process::ProcessChangeCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<
        crate::ChangePage<
            lash_execution_contracts::process::ObservedProcessChange,
            lash_execution_contracts::process::ProcessChangeCursor,
        >,
    > {
        let bounds = self.registry().process_change_bounds().await?;
        let (changes, mut next) = self
            .make_observer()
            .changed_since(
                after,
                limit
                    .get()
                    .min(lash_execution_contracts::process::MAX_PROCESS_ROSTER_PAGE_SIZE),
            )
            .await?;
        if changes.is_empty() && next.store_sequence() < bounds.current.store_sequence() {
            next = bounds.current;
        }
        Ok(crate::ChangePage {
            changes,
            next,
            retained_after: Some(bounds.retained_after),
        })
    }

    /// This build's default engine settings for a start of `payload` on
    /// engine `kind`, for a host that chooses to state them. Lash never
    /// calls it to fill a start's settings: a start states its own, and
    /// every run reads what its row records (FIG-6034 D4).
    ///
    /// # Errors
    ///
    /// No engine of `kind` is registered, or the engine refuses `payload`.
    pub fn engine_defaults(
        &self,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<Option<lash_execution_contracts::process::EngineSettings>> {
        Ok(self
            .core
            .host_process_engines
            .default_settings(kind, payload)?)
    }

    /// This kernel engine's defaults, ready for a typed `observing` choice.
    /// Unknown settings fields are refused by the kernel's strict decoder.
    ///
    /// # Errors
    ///
    /// The kernel engine is absent or its settings do not decode.
    #[cfg(feature = "codemode")]
    pub fn kernel_engine_defaults(&self) -> Result<crate::process::KernelRecordedSettings> {
        let settings = self
            .engine_defaults(
                lash_vm_runtime::LASH_VM_ENGINE_KIND,
                &serde_json::Value::Null,
            )?
            .ok_or_else(|| {
                crate::error::EmbedError::Plugin(
                    lash_execution_contracts::plugin_error::PluginError::Registration(
                        "the kernel engine returned no settings".to_owned(),
                    ),
                )
            })?;
        crate::process::KernelRecordedSettings::from_engine_settings(&settings).map_err(|error| {
            lash_execution_contracts::plugin_error::PluginError::Registration(format!(
                "the kernel settings do not decode: {error}"
            ))
            .into()
        })
    }

    /// Read the process with its park state from this core's actor store.
    /// A retained terminal reports `NotParked`; this read never reports `NotRead`.
    pub async fn get(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<lash_execution_contracts::process::ObservedProcess>> {
        self.make_observer()
            .process(process_id)
            .await
            .map_err(Into::into)
    }

    /// The status the process's engine projected from its last committed
    /// transition: the engine's own typed data, which its plugin decodes.
    /// `None` for a process whose engine declares no status projection, one
    /// that has not transitioned yet, or one that is not retained. It is a
    /// read of committed state, not a lifecycle event.
    pub async fn engine_status(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<lash_execution_contracts::process::EngineStatus>> {
        lash_core::runtime::durable::engine_status(self.core.backend.durable().as_ref(), process_id)
            .await
            .map_err(|error| {
                EmbedError::Plugin(
                    lash_execution_contracts::plugin_error::PluginError::Session(format!(
                        "read process `{process_id}`'s engine status: {error}"
                    )),
                )
            })
    }

    /// Read recorded authority in one backend call and one SQL statement.
    /// Builds no component and opens no session. Narrowing the creator's
    /// session leaves this answer unchanged. An unknown id returns `None`;
    /// a pruned id returns `PluginError::ProcessNoLongerRetained`. Missing or
    /// corrupt recorded environment bytes return a typed `ProcessEnvLoad` error.
    pub async fn environment(&self, id: &ProcessId) -> Result<Option<ProcessEnvView>> {
        let row = self
            .core
            .backend
            .process_authority_reads()
            .process_authority(id)
            .await
            .map_err(|error| match error {
                lash_core::StoreError::ProcessAuthorityPruned {
                    terminal_label,
                    pruned_at_ms,
                } => {
                    match lash_execution_contracts::process::registry_transitions::ProcessTombstoneStamp::from_row(
                        id,
                        &terminal_label,
                        pruned_at_ms,
                    ) {
                        Ok(stamp) => EmbedError::Plugin(
                            lash_execution_contracts::process::registry_transitions::process_no_longer_retained(
                                stamp,
                            ),
                        ),
                        Err(error) => error.into(),
                    }
                }
                error => error.into(),
            })?;
        let Some((record, bytes)) = row else {
            return Ok(None);
        };
        let authority = match record.input.as_ref() {
            lash_execution_contracts::process::ProcessInput::Engine { launch, .. } => {
                let bytes = bytes.ok_or_else(|| {
                    crate::plugins::ProcessEnvLoadError::Missing(launch.env.clone())
                })?;
                let env = decode_environment(&launch.env, &bytes)?;
                ProcessAuthority::Engine {
                    env_ref: launch.env.clone(),
                    retired: self.core.host_process_engines.components().retired_in(&env),
                    env: Box::new(env),
                    engine_settings: launch.settings.clone(),
                }
            }
            lash_execution_contracts::process::ProcessInput::SessionTurn {
                create_request, ..
            } => ProcessAuthority::SessionTurn {
                session: lash_execution_contracts::process::process_session_id(id, create_request),
            },
        };
        Ok(Some(ProcessEnvView {
            process: record.id,
            authority,
            provenance: record.provenance,
        }))
    }

    /// Inspect the selected Link and validate its per-use environment through
    /// the same judgment every process start uses. No policy is chosen here.
    pub async fn check_link(
        &self,
        kind: &str,
        start: &lash_kernel_value::LinkedStart,
        env: &lash_core::ProcessEnv,
    ) -> Result<lash_vocabulary::claims::ClaimRequirements> {
        let engines = &self.core.host_process_engines;
        let inspection = engines.inspect_link(kind, start).await.map_err(|error| {
            EmbedError::Plugin(lash_execution_contracts::PluginError::Package(Box::new(
                error,
            )))
        })?;
        engines
            .check_links(&Arc::new(env.clone()), &inspection)
            .map_err(|error| {
                EmbedError::Plugin(lash_execution_contracts::PluginError::Package(Box::new(
                    error,
                )))
            })
    }

    /// Carry `id` onto the successor pair its host names (D2): the engine
    /// payload it goes on as (for a kernel process, its entry over another
    /// document) and the published env that payload links under. A process
    /// stays pinned to the pair it was started as; this is the one way it
    /// moves, and only when its host asks.
    ///
    /// The pair is link-checked as a start's is, under `successor.fits`, and
    /// held under the process's own referrer. Its engine plans the carry of
    /// the stored run: for a kernel process, a correspondence that replaces
    /// each library function its document lists by the successor's function
    /// of the same name, carrying the parked run and its admitted waits
    /// without re-matching any. Then the request is recorded, and the
    /// process's owner applies it at the run's next rest under its claim
    /// fence, rewriting the engine state and the process's recorded input
    /// in one commit. A request the owner refuses after all is recorded as
    /// the `process.successor_refused` event.
    ///
    /// # Errors
    ///
    /// [`PluginError::ProcessSuccessorRefused`](lash_execution_contracts::plugin_error::PluginError::ProcessSuccessorRefused)
    /// with the typed reason: nothing was recorded, and the process goes on
    /// as written.
    pub async fn succeed(
        &self,
        id: &ProcessId,
        successor: lash_execution_contracts::process::ProcessSuccessor,
    ) -> Result<lash_execution_contracts::process::ProcessSuccession> {
        Ok(lash_core::request_process_successor(
            &self.core.backend,
            &self.core.host_process_engines,
            id,
            successor,
        )
        .await?)
    }

    /// Recover a published environment from its content reference, including
    /// capability binding ids, contracts and arguments. One store read, without
    /// component builds or session reads. Absent bytes return `None`; corrupt
    /// bytes return a typed `ProcessEnvLoad` error.
    pub async fn env(
        &self,
        env_ref: &lash_core::ProcessEnvRef,
    ) -> Result<Option<lash_core::ProcessEnv>> {
        let bytes = self
            .core
            .backend
            .process_authority_reads()
            .environment_bytes(env_ref)
            .await?;
        bytes
            .map(|bytes| decode_environment(env_ref, &bytes))
            .transpose()
    }

    /// Read the process as its workflow: the identity and signature of the
    /// definition it runs and its document entered at its entry, or the
    /// typed reason there is none. The document is the one the process was
    /// admitted under, whatever was published after it started.
    ///
    /// The read names the definition through the process's own recorded
    /// input, so it answers for as long as the row is retained, on any core
    /// over the same stores.
    #[cfg(feature = "codemode")]
    pub async fn graph(&self, process_id: &ProcessId) -> Result<crate::workflow::WorkflowRead> {
        use crate::workflow::{WorkflowRead, WorkflowUnavailable};
        let Some(process) = self.get(process_id).await? else {
            return Ok(WorkflowRead::Unavailable(WorkflowUnavailable::Process {
                process_id: process_id.clone(),
            }));
        };
        let lash_execution_contracts::process::ProcessInput::Engine { kind, payload, .. } =
            &process.input
        else {
            return Ok(WorkflowRead::Unsupported {
                engine_kind: process.identity.kind.clone(),
            });
        };
        crate::workflow::read(
            &self.core.host_process_engines,
            &lash_execution_contracts::process::ProcessEngineKind::from(kind.as_str()),
            payload,
        )
        .await
    }

    /// Read one durable event page after `from`. Full/Lite is a request
    /// parameter; the returned continuation names where the next page
    /// starts. It pages the durable log only: following a process live is
    /// [`observe`](Self::observe).
    pub async fn events(
        &self,
        from: crate::process_history::ProcessHistoryContinuation,
        limit: std::num::NonZeroUsize,
        mode: lash_execution_contracts::process::ProcessEventQueryMode,
    ) -> Result<crate::process_history::ProcessEventsRead> {
        Ok(crate::process_history::read_events(&self.registry(), from, limit, mode).await?)
    }

    pub async fn await_output(
        &self,
        process_id: &ProcessId,
    ) -> Result<lash_execution_contracts::process::ProcessAwaitOutput> {
        let process_id = self
            .core
            .process_registry
            .clone()
            .require_process_id(process_id)
            .await?;
        let process_work = Arc::clone(self.core.substrate_slot.ports().await.process.port());
        Ok(await_process_terminal(process_work.as_ref(), &process_id).await?)
    }

    /// Requests cancellation of the identified process.
    pub async fn cancel(
        &self,
        operation: crate::OperationId,
        process_id: &ProcessId,
    ) -> Result<lash_execution_contracts::process::ProcessCancelReceipt> {
        let scoped_effect_controller = self.operation_context(&operation)?;
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
            return Err(EmbedError::Plugin(
                lash_execution_contracts::plugin_error::PluginError::Session(
                    "process cancel returned the wrong outcome".to_string(),
                ),
            ));
        };
        Ok(lash_execution_contracts::process::ProcessCancelReceipt::from_observed(*record)?)
    }

    /// Redrives a parked process (ADR 0132 §11): clears its park and its
    /// activation-loop count and makes it runnable, so its engine runs again
    /// from its committed state. Answers whether the process was parked; a
    /// process that is not parked is left as it is. Cancelling a parked
    /// process instead ends it without running its engine.
    pub async fn redrive(&self, process_id: &ProcessId, requester: &str) -> Result<bool> {
        self.core
            .backend
            .redrive_process(process_id, requester)
            .await
            .map_err(|error| {
                EmbedError::Plugin(lash_execution_contracts::plugin_error::PluginError::Invoke(
                    error.to_string(),
                ))
            })
    }

    /// Returns the current process-session snapshot.
    pub async fn session_snapshot(
        &self,
        session_id: SessionId,
    ) -> Result<lash_execution_contracts::process::ProcessWorkSnapshot> {
        self.make_observer()
            .snapshot_for_session(session_id)
            .await
            .map_err(Into::into)
    }

    pub fn observer(&self) -> Result<lash_execution_contracts::process::ProcessWorkObserver> {
        Ok(self.make_observer())
    }

    /// Cancel every live process: running, or waiting on a call, a key, a
    /// sleep or another process. A host-wide lever; for a
    /// session-scoped stop use [`SessionProcessAdmin::cancel_all`](crate::process::SessionProcessAdmin::cancel_all).
    pub async fn cancel_all(
        &self,
        operation: crate::OperationId,
    ) -> Result<Vec<lash_execution_contracts::process::ProcessCancelReceipt>> {
        let scoped_effect_controller = self.operation_context(&operation)?;
        let outcome = self
            .run_command(
                lash_core::ProcessCommand::List {
                    selection: lash_core::ProcessListSelection::HostRunning,
                },
                scoped_effect_controller.clone(),
            )
            .await?;
        let lash_core::ProcessEffectOutcome::List { entries: running } = outcome else {
            return Err(EmbedError::Plugin(
                lash_execution_contracts::plugin_error::PluginError::Session(
                    "process listing returned the wrong outcome".to_string(),
                ),
            ));
        };
        let mut summaries = Vec::with_capacity(running.len());
        for process in running {
            summaries.push(self.cancel(operation.clone(), &process.process_id).await?);
        }
        Ok(summaries)
    }

    fn operation_context(&self, operation: &crate::OperationId) -> Result<ActorContext> {
        operation.bind(&self.core.actor_context())
    }

    /// Processes are global; this re-homes only observer membership, never the process itself.
    pub async fn transfer(
        &self,
        from_scope: &lash_execution_contracts::process::SessionScope,
        to_scope: &lash_execution_contracts::process::SessionScope,
        process_ids: &[ProcessId],
    ) -> Result<()> {
        self.registry()
            .transfer_observers(
                &from_scope.session_id,
                &to_scope.session_id,
                process_ids,
                lash_execution_contracts::process::ProcessObserverBy::host("admin-transfer"),
            )
            .await
            .map_err(Into::into)
    }

    /// Host-scheduled history release (ADR 0023, FIG-3482): release the
    /// payloads of `process_id`'s events at or below `through` while the
    /// process stays retained, typically a long-running process whose events
    /// the host has already projected.
    ///
    /// The host must preserve any history its projections or event awaiters
    /// still need; reads below the selected horizon expire explicitly.
    ///
    /// The prefix is clamped to the process's last event and a horizon never
    /// moves back, so repeating the call releases nothing new. Released events
    /// keep their sequence, kind and replay identity, so the release
    /// is safe while the process runs. A read starting below the horizon
    /// answers [`ProcessEventHistoryRetention::Released`](lash_execution_contracts::process::ProcessEventHistoryRetention::Released)
    /// with a continuation after it, and an observation snapshot reports its
    /// effect evidence incomplete with
    /// [`ProcessEffectGapReason::HistoryReleased`](lash_core::ProcessEffectGapReason::HistoryReleased).
    /// Pruning a retired process remains [`Self::prune`].
    pub async fn release_events(
        &self,
        process_id: &ProcessId,
        through: u64,
    ) -> Result<lash_execution_contracts::process::ProcessEventRelease> {
        Ok(self
            .registry()
            .release_process_events(process_id, through)
            .await?)
    }

    /// Host-scheduled retention lever (ADR 0017): physically delete retired
    /// process rows (and their events, observer edges, leases) older than
    /// `cutoff_epoch_ms`, returning what was reclaimed. Retired is the terminal
    /// outcomes. Live process rows — running and waiting — are
    /// never touched. Lash exposes no finite maximum waiter
    /// lifetime: the host must retain rows beyond every still-replayable await,
    /// and a later await after pruning receives the typed
    /// `ProcessNoLongerRetained` outcome. Pass
    /// either the projector's acknowledged
    /// [`ProjectionWatermark::UpTo`](lash_execution_contracts::process::ProjectionWatermark::UpTo)
    /// cursor or an explicit
    /// [`ProjectionWatermark::NoProjector`](lash_execution_contracts::process::ProjectionWatermark::NoProjector).
    ///
    /// `filter` narrows *which* eligible retired rows this call reclaims (ADR
    /// 0023): retention is differentiated host policy, so a host expresses
    /// "reclaim the work this deleted session originated" and "reclaim terminal
    /// child-session debris after a day" as two scheduled calls over the same
    /// lever.
    /// `None` considers every retired row. Because retention only ever deletes
    /// retired rows, a nonempty set containing only running and waiting
    /// statuses cannot match and is refused. This includes the running
    /// default selected by an otherwise unspecified filter.
    pub async fn prune(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<&lash_execution_contracts::process::ProcessListFilter>,
        watermark: lash_execution_contracts::process::ProjectionWatermark,
    ) -> Result<lash_execution_contracts::process::ProcessPruneReport> {
        let registry = self.registry();
        Self::prune_selection(filter)?;
        Ok(registry
            .prune_terminal_processes(cutoff_epoch_ms, filter.cloned(), watermark)
            .await?)
    }

    /// Compact payload-free process tombstones through the host's retention
    /// cutoff and explicit projection watermark.
    pub async fn compact_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: lash_execution_contracts::process::ProjectionWatermark,
    ) -> Result<usize> {
        Ok(self
            .registry()
            .compact_process_tombstones(cutoff_epoch_ms, watermark)
            .await?)
    }
}
/// A conflicting host start names only its key (ADR 0107). Every other
/// refusal retains the controller error, including its cause and evidence.
pub(crate) fn host_start_refusal(
    start_key: Option<&lash_core::StartKey>,
    error: lash_core::RuntimeEffectControllerError,
) -> lash_execution_contracts::plugin_error::PluginError {
    if let Some(start_key) = start_key
        && (error.code == lash_core::RuntimeErrorCode::ProcessStartKeyConflict
            || (start_key.is_host_supplied() && error.code.is_replay_mismatch()))
    {
        return lash_execution_contracts::plugin_error::PluginError::StartKeyConflict {
            start_key: start_key.clone(),
        };
    }
    lash_execution_contracts::plugin_error::PluginError::RuntimeEffectController(error)
}

#[cfg(test)]
mod terminal_wait_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ReattachOnce {
        waits: AtomicUsize,
        terminal: lash_execution_contracts::process::ProcessAwaitOutput,
    }

    #[async_trait::async_trait]
    impl lash_core::ProcessWorkSubstrate for ReattachOnce {
        async fn await_process_terminal(
            &self,
            process_id: &lash_vocabulary::identity::ProcessId,
        ) -> std::result::Result<
            lash_core::ProcessTerminalWait,
            lash_execution_contracts::plugin_error::PluginError,
        > {
            assert_eq!(
                process_id,
                &lash_vocabulary::identity::ProcessId::fixture("admin-reattach-process")
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
            _process_id: &lash_vocabulary::identity::ProcessId,
            _request: &lash_core::CancelRequest,
            _key: &str,
        ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
            unreachable!("terminal-wait witness does not deliver cancels")
        }
    }

    #[tokio::test]
    async fn process_admin_reattaches_once_then_returns_terminal_output() {
        let terminal = lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
            lash_core::ToolCallOutput::success(serde_json::json!({"done": true})),
        );
        let port = ReattachOnce {
            waits: AtomicUsize::new(0),
            terminal: terminal.clone(),
        };

        let output = await_process_terminal(
            &port,
            &lash_vocabulary::identity::ProcessId::fixture("admin-reattach-process"),
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
    use lash_core::RuntimeEffectControllerError;
    use lash_core::RuntimeErrorCode;
    use lash_execution_contracts::plugin_error::PluginError;

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

#[cfg(feature = "codemode")]
impl Processes {
    /// Request an explicit helper upgrade. The owner rechecks every retained root at rest.
    /// `Requested` includes a run whose active continuation must first drain.
    pub async fn upgrade_run(
        &self,
        process: &ProcessId,
        pin: &crate::process::HostArtifactPin,
        request: &lash_kernel_doc::HelperRunUpgradeRequest,
    ) -> Result<
        std::result::Result<
            lash_kernel_doc::HelperRunUpgrade,
            lash_kernel_doc::HelperUpgradeRefusal,
        >,
    > {
        use lash_execution_contracts::process::ProcessInput;
        let record = self
            .core
            .backend
            .process_registry()
            .get_process(process)
            .await?
            .ok_or_else(|| {
                lash_execution_contracts::PluginError::Session("process is not retained".into())
            })?;
        let lookup = lash_execution_contracts::process::ProcessSuccessorRequest {
            unchanged: false,
            request_id: Some(request.request_id.clone()),
            payload: serde_json::Value::Null,
            env: lash_core::ProcessEnvRef::new("upgrade-lookup"),
            definition_id: None,
        };
        for prefix in ["process.succeeded", "process.successor_refused"] {
            let settled = self
                .core
                .backend
                .process_registry()
                .event_by_replay_key(
                    process,
                    &lookup.replay_key(process, prefix),
                    record.last_event_sequence,
                )
                .await?;
            if !matches!(
                settled,
                lash_execution_contracts::process::ProcessEventReadOutcome::Retained(None)
            ) {
                return Ok(Ok(lash_kernel_doc::HelperRunUpgrade {
                    request_id: request.request_id.clone(),
                    publication: None,
                    state: lash_kernel_doc::HelperRunUpgradeState::AlreadyResolved,
                }));
            }
        }
        let ProcessInput::Engine {
            kind,
            payload,
            launch,
        } = record.input.as_ref()
        else {
            return Err(lash_execution_contracts::PluginError::Session(
                "process has no kernel engine".into(),
            )
            .into());
        };
        if kind != lash_vm_runtime::LASH_VM_ENGINE_KIND {
            return Err(lash_execution_contracts::PluginError::Session(
                "process has another engine".into(),
            )
            .into());
        }
        let mut input = lash_vm_runtime::KernelProcessInput::from_payload(payload)
            .map_err(|e| lash_execution_contracts::PluginError::Session(e.to_string()))?;
        let artifacts = self.core.host_artifacts();
        let (code, publication) = match artifacts
            .documents()?
            .upgrade_run_code(&crate::artifacts::upgrade_claim(pin)?, &input)
            .await
            .map_err(lash_execution_contracts::PluginError::from)?
        {
            Ok(upgraded) => upgraded,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let mut env = self.env(&launch.env).await?.ok_or_else(|| {
            lash_execution_contracts::PluginError::Session("process environment is missing".into())
        })?;
        let mut execution = env.execution().clone();
        for (use_, binding) in env.execution().uses.iter() {
            if let Some(link) = publication.links.get(&use_.link) {
                execution.uses.insert(
                    lash_kernel_value::OperationUse {
                        link: *link,
                        slot: use_.slot,
                    },
                    binding.clone(),
                );
            }
        }
        let descriptor =
            lash_vm_runtime::definition_draft(&lash_vm_runtime::KernelProcessDefinition {
                code: code.clone(),
            })
            .map_err(|e| lash_execution_contracts::PluginError::Session(e.to_string()))?;
        let definition = artifacts.publish_definition(pin, &descriptor).await?;
        let inspection = artifacts
            .inspect_target(&crate::process::LinkTarget::Definition(definition.id))
            .await?;
        execution.requirements.machine.extend(&inspection.machine);
        execution.roots.insert(publication.to.link());
        execution.authored.insert(publication.to.link());
        for link in &env.execution().authored {
            if let Some(next) = publication.links.get(link) {
                execution.authored.insert(*next);
            }
        }
        env = env.with_execution(execution);
        let env = artifacts.publish_process_env(pin, &env).await?;
        input.code = code;
        let payload = serde_json::to_value(input)
            .map_err(|e| lash_execution_contracts::PluginError::Session(e.to_string()))?;
        let mut successor = lash_execution_contracts::process::ProcessSuccessor::new(payload, env);
        successor.request_id = Some(request.request_id.clone());
        let result = match self.succeed(process, successor).await {
            Ok(result) => result,
            Err(crate::EmbedError::Plugin(
                lash_execution_contracts::PluginError::ProcessSuccessorRefused { refusal, .. },
            )) => {
                if let lash_execution_contracts::process::ProcessSuccessorRefusal::Engine {
                    refusal,
                    ..
                } = refusal.as_ref()
                    && let Ok(refusal) = serde_json::from_value::<
                        lash_kernel_doc::HelperUpgradeRefusal,
                    >(refusal.clone())
                {
                    return Ok(Err(refusal));
                }
                return Err(
                    lash_execution_contracts::PluginError::ProcessSuccessorRefused {
                        process: process.clone(),
                        refusal,
                    }
                    .into(),
                );
            }
            Err(error) => return Err(error),
        };
        let state = match result {
            lash_execution_contracts::process::ProcessSuccession::Requested => {
                lash_kernel_doc::HelperRunUpgradeState::Requested
            }
            lash_execution_contracts::process::ProcessSuccession::AlreadyCurrent => {
                lash_kernel_doc::HelperRunUpgradeState::AlreadyCurrent
            }
            lash_execution_contracts::process::ProcessSuccession::AlreadyResolved => {
                lash_kernel_doc::HelperRunUpgradeState::AlreadyResolved
            }
        };
        Ok(Ok(lash_kernel_doc::HelperRunUpgrade {
            request_id: request.request_id.clone(),
            publication: Some(publication),
            state,
        }))
    }
}
