use super::*;
use crate::runtime::process::identity_projection::{
    project_process_event_type, project_process_payload_leaf, project_process_schema_leaf,
};

/// version_guard(
///     items(
///         path = "crates/lash-core-execution/src/triggers/router.rs",
///         path = "crates/lash-core-execution/src/runtime/process/identity_projection.rs",
///         trigger_subscription_definition_preimage, project_trigger_owner, project_trigger_draft,
///         project_process_event_type, project_process_value_selector,
///         project_trigger_process_input, project_process_payload_leaf, project_process_schema_leaf,
///     ),
/// )
pub(super) const TRIGGER_DEFINITION_FAMILY_VERSION: u8 = 3;
/// version_guard(
///     items(trigger_subscription_address_preimage, project_trigger_owner),
/// )
const TRIGGER_LOOKUP_FAMILY_VERSION: u8 = 2;
/// version_guard(
///     items(
///         path = "crates/lash-core-execution/src/triggers/router.rs",
///         path = "crates/lash-core-execution/src/runtime/process/identity_projection.rs",
///         trigger_source_preimage, project_process_payload_leaf,
///     ),
/// )
const TRIGGER_SOURCE_FAMILY_VERSION: u8 = 1;
/// version_guard(
///     items(derived_trigger_subscription_key),
/// )
const DERIVED_TRIGGER_SUBSCRIPTION_FAMILY_VERSION: u8 = 3;

pub fn deterministic_subscription_id(
    owner_scope: &TriggerOwnerScope,
    subscription_key: &str,
) -> String {
    // This fixed-size address projects only the independent lookup tuple, not
    // the growable definition. Trigger schemas reject v1 stores on cutover.
    let preimage = trigger_subscription_address_preimage(owner_scope, subscription_key);
    crate::stable_identity::rendered_hash(
        "trigger-subscription",
        TRIGGER_LOOKUP_FAMILY_VERSION,
        &preimage,
    )
}

fn trigger_subscription_address_preimage(
    owner_scope: &TriggerOwnerScope,
    subscription_key: &str,
) -> Vec<u8> {
    let mut address = crate::stable_identity::IdentityEncoder::new(
        "lash.trigger-subscription-address",
        TRIGGER_LOOKUP_FAMILY_VERSION,
    );
    project_trigger_owner(&mut address, owner_scope);
    address.string(subscription_key);
    address.finish()
}

/// Permanent trigger-definition tag registry.
///
/// Owners: 1 session, 2 host, 3 platform.
/// Actors: 1 host, 2 session.
/// Process inputs: 1 burned, 2 engine, 3 session turn, 4 external.
/// Tool output contracts: 1 static, 2 from-input-schema.
/// Arbitrary JSON and schemas are each one canonical opaque bytes leaf.
/// Value selectors: 1 payload, 2 pointer, 3 const, 4 template, 5 present.
/// Process statuses: 1 running, 2 waiting, 3 completed, 4 failed, 5 cancelled, 6 abandoned, 7
/// caller departed.
/// Retired tags remain burned.
fn trigger_subscription_definition_preimage(
    owner_scope: &TriggerOwnerScope,
    draft: &TriggerSubscriptionDraft,
) -> Vec<u8> {
    let family_version = TRIGGER_DEFINITION_FAMILY_VERSION;
    let mut fingerprint = crate::stable_identity::IdentityEncoder::new(
        "lash.trigger-subscription-definition",
        family_version,
    );
    project_trigger_owner(&mut fingerprint, owner_scope);
    project_trigger_draft(&mut fingerprint, draft);
    fingerprint.finish()
}

pub fn trigger_subscription_definition_fingerprint(
    owner_scope: &TriggerOwnerScope,
    draft: &TriggerSubscriptionDraft,
) -> String {
    // The fingerprint is compared only after the independent subscription-id
    // lookup. Its v2 grammar shares the trigger store's reject-and-recreate
    // lifecycle; projection corrections require a new family version.
    let preimage = trigger_subscription_definition_preimage(owner_scope, draft);
    let family_version = TRIGGER_DEFINITION_FAMILY_VERSION;
    crate::stable_identity::rendered_hash("trigger-definition", family_version, &preimage)
}

pub(super) fn project_trigger_owner(
    identity: &mut crate::stable_identity::IdentityEncoder,
    owner_scope: &TriggerOwnerScope,
) {
    match owner_scope {
        TriggerOwnerScope::Session { session_id } => {
            identity.tag(1);
            identity.string(session_id);
        }
        TriggerOwnerScope::Host { binding_id } => {
            identity.tag(2);
            identity.string(binding_id);
        }
        TriggerOwnerScope::Platform => identity.tag(3),
    }
}

pub(super) fn project_trigger_actor(
    identity: &mut crate::stable_identity::IdentityEncoder,
    actor: &crate::ProcessOriginator,
) {
    match actor {
        crate::ProcessOriginator::Host { scope } => {
            identity.tag(1);
            identity.optional(scope.as_deref(), |identity, scope| identity.string(scope));
        }
        crate::ProcessOriginator::Session { session_id, .. } => {
            identity.tag(2);
            identity.string(session_id);
        }
    }
}

pub(super) fn project_trigger_draft(
    identity: &mut crate::stable_identity::IdentityEncoder,
    draft: &TriggerSubscriptionDraft,
) {
    let TriggerSubscriptionDraft {
        subscription_key,
        env_ref,
        wake_target,
        name,
        source_type,
        source_key,
        source,
        payload_schema,
        source_capture,
        target,
        target_identity,
        event_types,
        input_template,
        target_label,
    } = draft;
    identity.string(subscription_key);
    identity.string(env_ref.as_str());
    identity.optional(wake_target.as_ref(), |identity, wake_target| {
        let crate::SessionScope {
            session_id,
            agent_frame_id,
        } = wake_target;
        identity.string(session_id);
        identity.optional(agent_frame_id.as_deref(), |identity, frame_id| {
            identity.string(frame_id);
        });
    });
    identity.optional(name.as_deref(), |identity, name| identity.string(name));
    identity.string(source_type);
    identity.string(source_key);
    project_process_payload_leaf(identity, source);
    project_process_schema_leaf(identity, &payload_schema.schema);
    project_trigger_source_capture(identity, source_capture);
    project_trigger_process_input(identity, target);
    // A definition ID is projected when present. Existing engine targets
    // without one keep their separately-owned identity preimages.
    let crate::ProcessIdentity {
        kind,
        label,
        definition,
        definition_id,
    } = target_identity;
    if let Some(id) = definition_id {
        identity.string(id.as_str());
    }
    identity.string(kind.as_str());
    identity.optional(label.as_deref(), |identity, label| identity.string(label));
    // A target identity's definition reference projects as the engine-owned
    // definition value alone: the engine kind is already fixed by `kind`, and the
    // signature is a claim the engine resolves, never part of what the
    // subscription names. The bytes therefore stay identical to the pre-FIG-2992
    // untyped blob and no family rotation is needed.
    identity.optional(definition.as_ref(), |identity, definition| {
        project_process_payload_leaf(identity, definition.definition.as_json());
    });
    let mut event_types = event_types.iter().collect::<Vec<_>>();
    event_types.sort_by(|left, right| left.name.cmp(&right.name));
    identity.sequence(event_types, |identity, event_type| {
        project_process_event_type(identity, event_type);
    });
    identity.sequence(input_template.iter(), |identity, (name, binding)| {
        identity.string(name);
        match binding {
            TriggerInputBinding::Event => identity.tag(1),
            TriggerInputBinding::Fixed { value } => {
                identity.tag(2);
                project_process_payload_leaf(identity, value);
            }
        }
    });
    identity.optional(target_label.as_deref(), |identity, label| {
        identity.string(label)
    });
}

/// Projects the admitted source contract and provider route.
///
/// The opaque route is one canonical payload leaf; the configuration contract is one canonical
/// schema leaf.
/// Retired tags remain burned.
pub(super) fn project_trigger_source_capture(
    identity: &mut crate::stable_identity::IdentityEncoder,
    capture: &TriggerSourceCapture,
) {
    let TriggerSourceCapture {
        constructor_path,
        config_schema,
        route,
    } = capture;
    identity.sequence(constructor_path.iter(), |identity, segment| {
        identity.string(segment);
    });
    project_process_schema_leaf(identity, &config_schema.schema);
    match route {
        TriggerProviderRoute::Resident => identity.tag(1),
        TriggerProviderRoute::Provider { provider_id, route } => {
            identity.tag(2);
            identity.string(provider_id);
            project_process_payload_leaf(identity, route);
        }
    }
}

fn project_trigger_process_input(
    identity: &mut crate::stable_identity::IdentityEncoder,
    input: &crate::ProcessInput,
) {
    match input {
        crate::ProcessInput::Engine { kind, payload } => {
            identity.tag(2);
            identity.string(kind);
            project_process_payload_leaf(identity, payload);
        }
        crate::ProcessInput::SessionTurn {
            definition_key,
            create_request: _,
            turn_input: _,
            result,
        } => {
            identity.tag(3);
            identity.string(definition_key);
            match result {
                crate::SessionTurnOutcome::Turn => identity.tag(1),
                crate::SessionTurnOutcome::FinalValue { schema } => {
                    identity.tag(2);
                    identity.optional(schema.as_ref(), project_process_schema_leaf);
                }
            }
        }
        crate::ProcessInput::External { metadata } => {
            identity.tag(4);
            project_process_payload_leaf(identity, metadata);
        }
        crate::ProcessInput::Definition {
            definition_id,
            args,
            ..
        } => {
            identity.tag(5);
            identity.string(definition_id.as_str());
            project_process_payload_leaf(identity, &serde_json::Value::Object(args.clone()));
        }
    }
}

pub(super) fn default_enabled() -> bool {
    true
}

pub fn default_trigger_source_key(source_type: &str, source: &serde_json::Value) -> String {
    let preimage = trigger_source_preimage(source_type, source);
    crate::stable_identity::rendered_hash(
        "trigger-source",
        TRIGGER_SOURCE_FAMILY_VERSION,
        &preimage,
    )
}

/// Permanent tag registry for residual trigger identity families.
///
/// Source and delivery-process v1 have no sum variants. Their complete field
/// sequences are encoded below; retired tags remain burned when variants are
/// introduced in later family versions.
fn trigger_source_preimage(source_type: &str, source: &serde_json::Value) -> Vec<u8> {
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.trigger-source",
        TRIGGER_SOURCE_FAMILY_VERSION,
    );
    identity.string(source_type);
    project_process_payload_leaf(&mut identity, source);
    identity.finish()
}

pub fn empty_trigger_source_key(source_type: &str) -> Result<String, PluginError> {
    Ok(default_trigger_source_key(
        source_type,
        &serde_json::json!({}),
    ))
}

pub fn deterministic_occurrence_id(request: &TriggerOccurrenceRequest) -> String {
    format!("trigger:{}", request.idempotency_key)
}

/// Derives the compiler-owned subscription key through the shared identity
/// framing used by every other durable trigger projection.
pub fn derived_trigger_subscription_key(
    process_name: &str,
    source_type: &str,
    source_key: &str,
) -> String {
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.trigger-subscription-key",
        DERIVED_TRIGGER_SUBSCRIPTION_FAMILY_VERSION,
    );
    identity.string(process_name);
    identity.string(source_type);
    identity.string(source_key);
    format!(
        "derived/v{DERIVED_TRIGGER_SUBSCRIPTION_FAMILY_VERSION}/{}",
        crate::stable_hash::blake3_hex("lash-derived-trigger-subscription/v2", &identity.finish())
    )
}

/// The start key of the one process a trigger delivery starts (ADR 0107).
///
/// Derived from the delivery's identity — its occurrence and the exact
/// subscription revision it was reserved against — so every attempt at the
/// delivery, the first and every recovery, presents the same key.
pub fn trigger_delivery_start_key(reservation: &TriggerDeliveryReservation) -> crate::StartKey {
    crate::StartKeyDerivation::LASH_START_PATHS.for_trigger_delivery(
        &reservation.occurrence.occurrence_id,
        &reservation.subscription.subscription_id,
        &reservation.subscription.incarnation,
        reservation.subscription.revision,
    )
}

/// The refusal a recorded emission raises when one of its deliveries did not
/// start. See [`TriggerRouter::emit_recorded`] for why this is an error rather
/// than a `Failed` entry in an otherwise successful report.
fn unstarted_delivery(subscription_id: &str, reason: &str) -> PluginError {
    PluginError::Session(format!(
        "trigger delivery for subscription `{subscription_id}` did not start: {reason}"
    ))
}

/// What a router wired for immediate `ProcessStart` attempts carries: the
/// kind's ledger they claim through, the clock they settle against, and the
/// host's relay policy they run under.
#[derive(Clone)]
struct ProcessStartWiring {
    ledger: Arc<dyn crate::store::ObligationLedger>,
    clock: Arc<dyn crate::Clock>,
    policy: crate::runtime::drive::relay::RelayPolicy,
}

#[derive(Clone)]
pub struct TriggerRouter {
    store: Arc<dyn TriggerStore>,
    process_work: crate::ProcessWorkWiring,
    process_env_store: Option<Arc<dyn crate::ProcessExecutionEnvStore>>,
    process_engines: Option<crate::ProcessEngineRegistry>,
    process_starts: Option<ProcessStartWiring>,
    route_restorer: Option<Arc<dyn TriggerRouteRestorer>>,
}

impl TriggerRouter {
    pub fn new(store: Arc<dyn TriggerStore>, process_work: crate::ProcessWorkWiring) -> Self {
        Self {
            store,
            process_work,
            process_env_store: None,
            process_engines: None,
            process_starts: None,
            route_restorer: None,
        }
    }

    pub fn with_route_restorer(mut self, restorer: Arc<dyn TriggerRouteRestorer>) -> Self {
        self.route_restorer = Some(restorer);
        self
    }

    /// The `ProcessStart` ledger the trigger's own start attempts claim
    /// through: a router wired with one tries the armed obligation at once
    /// (ADR 0109 §1.5); one without it leaves the row to the reconcile tick.
    /// `policy` is the host's relay policy, so the immediate delivery runs
    /// under the configured attempt budget.
    pub fn with_process_starts(
        mut self,
        ledger: Arc<dyn crate::store::ObligationLedger>,
        clock: Arc<dyn crate::Clock>,
        policy: crate::runtime::drive::relay::RelayPolicy,
    ) -> Self {
        self.process_starts = Some(ProcessStartWiring {
            ledger,
            clock,
            policy,
        });
        self
    }

    /// The engine registry this router admits trigger targets against, when the
    /// deployment wired one.
    pub fn process_engines(&self) -> Option<&crate::ProcessEngineRegistry> {
        self.process_engines.as_ref()
    }

    pub fn with_process_artifacts(
        mut self,
        process_env_store: Arc<dyn crate::ProcessExecutionEnvStore>,
        process_engines: crate::ProcessEngineRegistry,
    ) -> Self {
        self.process_env_store = Some(process_env_store);
        self.process_engines = Some(process_engines);
        self
    }

    pub(crate) fn store(&self) -> Arc<dyn TriggerStore> {
        Arc::clone(&self.store)
    }

    /// Emits a recorded [`crate::ToolIntent::EmitTrigger`] declaration and
    /// settles the report so redriving that one declaration returns the same
    /// bytes.
    ///
    /// A recorded declaration's report becomes the durable, wire-visible
    /// `ToolIntentExecutionOutcome::Executed` result, and on a runtime-owned
    /// host there is no journal to replay it from, so the drain must recompute
    /// the identical value. [`Self::emit`] reports `Started` for every
    /// delivery it started, on the first drive and every redrive alike.
    /// A redrive after retention reclaimed the occurrence has nothing to
    /// recompute it from: the store refuses the reclaimed identity, and the
    /// declaration fails with that typed refusal (FIG-4513).
    ///
    /// A delivery that did not start carries no such statement: its reason is a
    /// live error string, and the next drive may well start it. Reporting that
    /// inside a successful outcome would both call a failure a success and put
    /// replay-varying bytes on the wire, so a failed start fails the whole
    /// declaration instead — the caller turns the error into the intent's own
    /// refusal, which is where a command that did not happen belongs. Missing a
    /// process registry is the same case: nothing starts, so nothing is
    /// reported as started.
    ///
    /// A host whose registry is present on one drive and absent on the next
    /// changes from executing to refusing. That is a host configuration change
    /// between drives, not a redrive divergence; the same host answers the same
    /// way every time.
    ///
    /// This is deliberately not a journaled wrapper around [`Self::emit`]:
    /// delivery starts are themselves effects, and nesting them inside an outer
    /// effect is what an atomic tool attempt cannot do: its body declares the
    /// trigger as an intent instead.
    pub async fn emit_recorded(
        &self,
        request: TriggerOccurrenceRequest,
        effect_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<TriggerEmitReport, PluginError> {
        self.emit_recorded_reporting_realization(request, effect_controller)
            .await
            .map(|(report, _)| report)
    }

    /// [`Self::emit_recorded`], also reporting whether the trigger store
    /// recorded this occurrence or coalesced it onto one it already held under
    /// the same idempotency key (FIG-3070).
    ///
    /// The occurrence idempotency key, not an effect-journal key, is the dedupe
    /// point for a re-submitted emission, so a caller that reports replay to a
    /// host -- the tool-intent ingress -- can only learn it from the store. The
    /// verdict rides beside the report rather than inside it: it describes this
    /// call, not the occurrence, and `TriggerEmitReport` crosses the remote
    /// peer wire where a per-call field would be a protocol change for a fact
    /// the wire never had to carry.
    pub async fn emit_recorded_reporting_realization(
        &self,
        request: TriggerOccurrenceRequest,
        effect_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<(TriggerEmitReport, crate::StoreRealization), PluginError> {
        let (report, realization) = self
            .emit_reporting_realization(request, effect_controller)
            .await?;
        for delivery in &report.deliveries {
            if let TriggerDeliveryEmitOutcome::Failed { reason } = &delivery.outcome {
                return Err(unstarted_delivery(&delivery.subscription_id, reason));
            }
        }
        Ok((report, realization))
    }

    pub async fn emit(
        &self,
        request: TriggerOccurrenceRequest,
        effect_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<TriggerEmitReport, PluginError> {
        self.emit_reporting_realization(request, effect_controller)
            .await
            .map(|(report, _)| report)
    }

    /// [`Self::emit`], also reporting the trigger store's occurrence-key
    /// verdict for this call (FIG-3070).
    pub async fn emit_reporting_realization(
        &self,
        request: TriggerOccurrenceRequest,
        effect_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<(TriggerEmitReport, crate::StoreRealization), PluginError> {
        let TriggerIngressReceipt {
            occurrence,
            reservations,
            realization,
        } = self.ingest_occurrence(request, effect_controller).await?;
        let process_work = &self.process_work;
        let mut deliveries = Vec::new();
        for reservation in reservations {
            // The emission acts on the receipt its ingest recorded, never on
            // the store's answer now (FIG-4297, FIG-4503). A delivery the
            // receipt holds bound answers the bound process and starts
            // nothing, since after that process is pruned its start key would
            // mint another. One it holds unbound starts, and a replay
            // consumes the start and the bind it journaled, whose journal
            // answers the process the first attempt started (FIG-806). A
            // delivery bound, and its process pruned, after this emission's
            // ingest is refused at its start's registration, which
            // [`Self::start_delivery`] answers with the bound process
            // (FIG-4369). Either way the delivery reports `Started` with its
            // process on every attempt: the settled outcome, never the live
            // store read (FIG-4272).
            let started = match reservation.process_id.clone() {
                Some(process_id) => Ok(process_id),
                None => {
                    self.start_delivery_steps(
                        &reservation,
                        Arc::clone(process_work.registry()),
                        effect_controller,
                    )
                    .await
                }
            };
            let process_id = match started {
                Ok(process_id) => process_id,
                // The bind's store did not answer: the fault is this
                // attempt's, and the engine runs the emission again. Reported
                // as a failed delivery, the outage would settle the
                // emission's answer while the bind's step, unrecorded, binds
                // on the next replay (FIG-4513).
                Err(DeliveryStartFault::Attempt(fault)) => return Err(fault.into()),
                Err(DeliveryStartFault::Delivery(err)) => {
                    deliveries.push(reservation.emit_report(
                        None,
                        TriggerDeliveryEmitOutcome::Failed {
                            reason: err.to_string(),
                        },
                    ));
                    continue;
                }
            };
            deliveries.push(
                reservation.emit_report(Some(process_id), TriggerDeliveryEmitOutcome::Started),
            );
        }
        Ok((
            TriggerEmitReport::new(occurrence.occurrence_id, deliveries),
            realization,
        ))
    }

    /// Ingest `request` as one recorded `IngestTriggerOccurrence` step
    /// (FIG-4503), whose outcome is the store's receipt.
    ///
    /// The first execution writes the occurrence and reserves its
    /// deliveries. Every replay serves the recorded receipt and never
    /// reaches the store, so it reports what the first attempt reported
    /// after retention has reclaimed the occurrence. A host with no journal
    /// runs the step's body again on a redelivery: the store then refuses a
    /// reclaimed identity as
    /// [`TriggerOccurrenceReclaimed`](crate::RuntimeErrorCode::TriggerOccurrenceReclaimed)
    /// and writes nothing (FIG-4513), and the emission fails with that
    /// refusal. A request the store would refuse unread is refused here,
    /// before any step.
    async fn ingest_occurrence(
        &self,
        request: TriggerOccurrenceRequest,
        effect_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<TriggerIngressReceipt, PluginError> {
        validate_trigger_occurrence_request(&request)?;
        let effect_id = format!("trigger-ingest:{}", deterministic_occurrence_id(&request));
        let attribution = request
            .session_id
            .clone()
            .map(crate::RuntimeAttribution::for_session)
            .unwrap_or_else(crate::RuntimeAttribution::none);
        let invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                effect_controller.execution_scope().clone(),
                effect_id.clone(),
            )
            .map_err(crate::RuntimeEffectControllerError::from)?,
            attribution,
            effect_id,
        );
        Ok(effect_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::IngestTriggerOccurrence {
                        request: Box::new(request),
                    },
                ),
                crate::runtime::effect::executor::owned_runner_executor(
                    Box::new(OccurrenceIngestRunner {
                        store: Arc::clone(&self.store),
                    }),
                    None,
                ),
            )
            .await?
            .into_trigger_ingress_receipt()?)
    }

    /// Bind `reservation` to the process its start registered, as one
    /// recorded step (FIG-4503).
    ///
    /// The first execution binds the delivery and releases the process's
    /// pin. Every replay serves the recorded process: once retention has
    /// reclaimed the delivery there is no row to bind.
    async fn bind_started_delivery(
        &self,
        reservation: &TriggerDeliveryReservation,
        process_id: ProcessId,
        process_registry: Arc<dyn crate::ProcessRegistry>,
        effect_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<ProcessId, crate::RuntimeEffectControllerError> {
        self.record_binding(
            reservation,
            "trigger-bind",
            Box::new(DeliveryBindRunner {
                store: Arc::clone(&self.store),
                process_registry,
                occurrence_id: reservation.occurrence.occurrence_id.clone(),
                subscription_id: reservation.subscription.subscription_id.clone(),
                process_id,
            }),
            effect_controller,
        )
        .await
    }

    /// Record the process `reservation`'s delivery is bound to, once its
    /// start's registration refused as
    /// [`TriggerDeliveryBound`](crate::RuntimeErrorCode::TriggerDeliveryBound)
    /// (FIG-4369).
    ///
    /// The emission's ingest answered the delivery unbound. Another emission
    /// then bound it, and retention pruned the bound process, before this
    /// start registered. The start's key found nothing, and the registrar,
    /// reading the binding in the same transaction, registered nothing. The
    /// first execution reads the binding, which a bind writes once; every
    /// replay serves the recorded binding after the start's recorded refusal.
    async fn admit_bound_delivery(
        &self,
        reservation: &TriggerDeliveryReservation,
        effect_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<ProcessId, crate::RuntimeEffectControllerError> {
        self.record_binding(
            reservation,
            "trigger-bound-admission",
            Box::new(BoundDeliveryAdmissionRunner {
                store: Arc::clone(&self.store),
                occurrence_id: reservation.occurrence.occurrence_id.clone(),
                subscription_id: reservation.subscription.subscription_id.clone(),
            }),
            effect_controller,
        )
        .await
    }

    /// Journal one `AdmitTriggerDelivery` step for `reservation` under
    /// `replay_prefix`, whose first execution is `runner`.
    async fn record_binding(
        &self,
        reservation: &TriggerDeliveryReservation,
        replay_prefix: &str,
        runner: Box<dyn crate::runtime::effect::executor::RuntimeEffectLocalRunner>,
        effect_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<ProcessId, crate::RuntimeEffectControllerError> {
        let subscription = &reservation.subscription;
        let occurrence = &reservation.occurrence;
        let replay_key = format!(
            "{replay_prefix}:{}:{}:{}:{}",
            occurrence.occurrence_id,
            subscription.subscription_id,
            subscription.incarnation,
            subscription.revision
        );
        let invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(effect_controller.execution_scope().clone(), replay_key)?,
            delivery_attribution(subscription),
            format!(
                "{replay_prefix}:{}:{}",
                occurrence.occurrence_id, subscription.subscription_id
            ),
        )
        .with_caused_by(Some(delivery_causal_ref(reservation)));
        effect_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::AdmitTriggerDelivery {
                        occurrence_id: occurrence.occurrence_id.clone(),
                        subscription_id: subscription.subscription_id.clone(),
                    },
                ),
                crate::runtime::effect::executor::owned_runner_executor(runner, None),
            )
            .await?
            .into_trigger_delivery_admission()
            .map(|TriggerDeliveryAdmission::Bound { process_id }| process_id)
    }

    pub async fn start_delivery(
        &self,
        reservation: &TriggerDeliveryReservation,
        process_registry: Arc<dyn crate::ProcessRegistry>,
        effect_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<ProcessId, PluginError> {
        self.start_delivery_steps(reservation, process_registry, effect_controller)
            .await
            .map_err(DeliveryStartFault::into_error)
    }

    /// [`Self::start_delivery`], telling a delivery that did not start from
    /// an attempt whose bind or binding read the store did not answer.
    async fn start_delivery_steps(
        &self,
        reservation: &TriggerDeliveryReservation,
        process_registry: Arc<dyn crate::ProcessRegistry>,
        effect_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<ProcessId, DeliveryStartFault> {
        let DeliveryStart {
            command,
            attribution,
            causal_ref,
            route,
        } = self
            .prepare_delivery_start(reservation)
            .map_err(DeliveryStartFault::Delivery)?;
        let subscription = &reservation.subscription;
        let occurrence = &reservation.occurrence;
        let effect_id = command.effect_id();
        #[expect(
            clippy::expect_used,
            reason = "the scope comes from the caller's own live effect controller, which is admitted by construction"
        )]
        let invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(
                effect_controller.execution_scope().clone(),
                format!(
                    "trigger:{}:{}:{}:{}",
                    occurrence.occurrence_id,
                    subscription.subscription_id,
                    subscription.incarnation,
                    subscription.revision
                ),
            )
            .expect("trigger delivery uses the already admitted controller scope"),
            attribution,
            effect_id.clone(),
        )
        .with_caused_by(Some(causal_ref));
        let outcome = match effect_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::process(command),
                ),
                {
                    let mut executor = crate::RuntimeEffectLocalExecutor::processes(
                        Arc::clone(&process_registry),
                        Arc::clone(self.process_work.port()),
                    );
                    if let Some(starts) = self.process_starts.as_ref() {
                        executor = executor.with_process_starts(
                            Arc::clone(&starts.ledger),
                            Arc::clone(&starts.clock),
                            starts.policy,
                        );
                    }
                    if let Some(store) = self.process_env_store.as_ref() {
                        executor = executor.with_process_env_store(Arc::clone(store));
                    }
                    if let Some(engines) = self.process_engines.as_ref() {
                        executor = executor.with_process_engines(engines.clone());
                    }
                    // The start's recorded admission asks the host's
                    // restorer: a replay reads the step's record, a refusal
                    // included, and never asks again (FIG-4554).
                    if let Some(route) = route {
                        executor = executor.with_trigger_route(route);
                    }
                    executor
                },
            )
            .await
        {
            Ok(outcome) => outcome,
            // The delivery was bound, and its process pruned, since this
            // emission's ingest answered it unbound: the registrar registered
            // nothing, and the delivery's process is the bound one. Nothing
            // was pinned, and the binding needs no bind (FIG-4369).
            Err(refusal) if refusal.code == crate::RuntimeErrorCode::TriggerDeliveryBound => {
                return self
                    .admit_bound_delivery(reservation, effect_controller)
                    .await
                    .map_err(DeliveryStartFault::of_binding_step);
            }
            Err(error) => return Err(DeliveryStartFault::Delivery(error.into())),
        };
        match outcome {
            crate::RuntimeEffectOutcome::Process {
                result: crate::ProcessEffectOutcome::Start { record, .. },
            } => {
                // The delivery owns exactly the process its key minted: bind
                // it before the delivery is reported, so recovery resumes an
                // unbound reservation and never starts a second process for a
                // bound one (ADR 0107). The bind delivers the reservation's
                // `TriggerDelivery` obligation in the same write; only then
                // is the process's pin released. Both are one recorded step,
                // which a replay never runs again (FIG-4503).
                self.bind_started_delivery(
                    reservation,
                    record.id,
                    process_registry,
                    effect_controller,
                )
                .await
                .map_err(DeliveryStartFault::of_binding_step)
            }
            other => Err(DeliveryStartFault::Delivery(PluginError::Session(format!(
                "trigger process start returned the wrong outcome: {}",
                other.kind().as_str()
            )))),
        }
    }

    /// Recover the reserved delivery of `occurrence_id` to `subscription_id`
    /// into its one bound process: the `TriggerDelivery` obligation's delivery
    /// (ADR 0109, ADR 0021's FIG-4090 amendment).
    ///
    /// A crash between the reservation and its start leaves the row reserved
    /// and unbound, and nothing re-emits the occurrence: a replayed emit finds
    /// the reservation already held. Recovery therefore starts from the
    /// reservation the store holds, never from the occurrence. It registers
    /// the same process a first attempt registers — the start key is the
    /// delivery's identity, so a registration that already landed is found
    /// again rather than doubled — and binds it, which delivers the
    /// obligation. A reservation already bound is bound again to the same
    /// process, which answers at once.
    ///
    /// The registration pins its process until the bind commits (FIG-4203),
    /// so the start key still finds it after a lost bind even once it
    /// completed and a retention pass ran: a completed child is never started
    /// a second time. The pin is released once the bind commits.
    ///
    /// # Errors
    ///
    /// [`TriggerDeliveryRecoveryError::Refused`] when the reservation can never
    /// start as reserved: it is gone, its payload or source fails the captured
    /// contract, its route was revoked, its target is no engine process, or
    /// its registration was refused terminally. Anything else is
    /// [`TriggerDeliveryRecoveryError::Retryable`] under the same identity.
    pub async fn recover_delivery(
        &self,
        occurrence_id: &str,
        subscription_id: &str,
    ) -> Result<ProcessId, TriggerDeliveryRecoveryError> {
        let reservation = self
            .store
            .list_deliveries_by_occurrence_id(occurrence_id)
            .await
            .map_err(TriggerDeliveryRecoveryError::classified)?
            .into_iter()
            .find(|reservation| reservation.subscription.subscription_id == subscription_id)
            .ok_or_else(|| {
                TriggerDeliveryRecoveryError::Refused(PluginError::Session(format!(
                    "trigger delivery `{occurrence_id}`/`{subscription_id}` is no longer reserved"
                )))
            })?;
        let process_id = match reservation.process_id.clone() {
            Some(process_id) => process_id,
            None => match Box::pin(self.register_recovered_delivery(&reservation)).await? {
                RecoveredDeliveryStart::Registered(process_id) => process_id,
                RecoveredDeliveryStart::AlreadyBound => {
                    return self.recovered_binding(occurrence_id, subscription_id).await;
                }
            },
        };
        self.store
            .bind_delivery_process(occurrence_id, subscription_id, &process_id)
            .await
            .map_err(TriggerDeliveryRecoveryError::classified)?;
        release_trigger_delivery_pin(self.process_work.registry().as_ref(), &process_id).await;
        Ok(process_id)
    }

    /// The process a delivery whose recovered start was refused as bound is
    /// bound to (FIG-4369). The bind that refused the start was written once,
    /// so a reservation that answers unbound is gone or unreadable, and the
    /// relay tries again.
    async fn recovered_binding(
        &self,
        occurrence_id: &str,
        subscription_id: &str,
    ) -> Result<ProcessId, TriggerDeliveryRecoveryError> {
        self.store
            .list_deliveries_by_occurrence_id(occurrence_id)
            .await
            .map_err(TriggerDeliveryRecoveryError::classified)?
            .into_iter()
            .find(|reservation| reservation.subscription.subscription_id == subscription_id)
            .and_then(|reservation| reservation.process_id)
            .ok_or_else(|| {
                TriggerDeliveryRecoveryError::Retryable(PluginError::Session(format!(
                    "trigger delivery `{occurrence_id}`/`{subscription_id}` refused its start \
                     as bound, and holds no binding now"
                )))
            })
    }

    /// Register the process `reservation` starts, outside any journal: the
    /// relay has no caller whose journal could record it, and the start key
    /// makes the registration idempotent on its own. A delivery bound since
    /// the relay read it, whose process was pruned, registers nothing
    /// (FIG-4369).
    async fn register_recovered_delivery(
        &self,
        reservation: &TriggerDeliveryReservation,
    ) -> Result<RecoveredDeliveryStart, TriggerDeliveryRecoveryError> {
        let DeliveryStart { command, route, .. } = self
            .prepare_delivery_start(reservation)
            .map_err(TriggerDeliveryRecoveryError::Refused)?;
        let registry = Arc::clone(self.process_work.registry());
        let port = Arc::clone(self.process_work.port());
        let execution = crate::runtime::effect::executor::ProcessLocalExecution {
            process_starts: self.process_starts.as_ref().map(|starts| {
                Arc::new(
                    crate::runtime::process_start::ProcessStartRelay::new(
                        Arc::clone(&starts.ledger),
                        Arc::clone(&registry),
                        Arc::clone(&port),
                        Arc::clone(&starts.clock),
                    )
                    .with_policy(starts.policy),
                )
            }),
            registry,
            process_work: port,
            process_env_store: self.process_env_store.clone(),
            process_engines: self.process_engines.clone(),
            // A trigger delivery's start is never a host-granted root.
            host_start: None,
            turn_cancellation: None,
            effect_controller: None,
            attachments: None,
            // The registration asks the host's restorer only while no
            // process holds the start's key: a delivery whose start already
            // registered is recovered unasked (FIG-4554). An unavailable
            // route is retried under the same identity, and a revoked one
            // refuses for good.
            trigger_route: route,
            outcome_observer: None,
        };
        // A start records nothing into its caller: the relay has no journal,
        // and the start derives its starter from its own key.
        let receiver = crate::ExecutionScope::runtime_operation(format!(
            "trigger-delivery-recovery:{}:{}",
            reservation.occurrence.occurrence_id, reservation.subscription.subscription_id
        ));
        match Box::pin(execution.execute(&receiver, command)).await {
            Ok(crate::ProcessEffectOutcome::Start { record, .. }) => {
                Ok(RecoveredDeliveryStart::Registered(record.id))
            }
            Ok(_) => Err(TriggerDeliveryRecoveryError::Refused(PluginError::Session(
                "trigger process start returned an outcome other than a start".to_string(),
            ))),
            Err(refusal) if refusal.code == crate::RuntimeErrorCode::TriggerDeliveryBound => {
                Ok(RecoveredDeliveryStart::AlreadyBound)
            }
            Err(error) => Err(TriggerDeliveryRecoveryError::classified(PluginError::from(
                error,
            ))),
        }
    }

    /// Everything one delivery's start needs, derived only from the
    /// reservation, so the first attempt and every recovery register the
    /// identical process. It consults nothing live: the captured route is
    /// handed on for the start's recorded admission to restore (FIG-4554).
    ///
    /// # Errors
    ///
    /// The reservation can never start as reserved.
    fn prepare_delivery_start(
        &self,
        reservation: &TriggerDeliveryReservation,
    ) -> Result<DeliveryStart, PluginError> {
        let subscription = &reservation.subscription;
        let occurrence = &reservation.occurrence;
        // Delivery validates against the contract this subscription captured at
        // registration, never against whatever the live catalog now says. The
        // reservation carries the subscription snapshot the store pinned when
        // it reserved, so a catalog edit or a later explicit update cannot
        // rewrite an already-reserved delivery's contract or route.
        subscription
            .payload_schema
            .validate(&occurrence.payload)
            .map_err(|err| {
                PluginError::Session(format!(
                    "invalid payload for trigger `{}`: {err}",
                    subscription.subscription_key
                ))
            })?;
        if let Some(source) = occurrence.source.as_ref() {
            subscription
                .source_capture
                .config_schema
                .validate(source)
                .map_err(|err| {
                    PluginError::Session(format!(
                        "trigger `{}` occurrence source does not match the captured source contract: {err}",
                        subscription.subscription_key
                    ))
                })?;
        }
        let args =
            materialize_trigger_process_args(&subscription.input_template, &occurrence.payload)?;
        let target = apply_trigger_inputs(subscription.target.clone(), args)?;
        let causal_ref = delivery_causal_ref(reservation);
        let attribution = delivery_attribution(subscription);
        let trigger_occurrence_invocation =
            crate::runtime::causal::trigger_occurrence_invocation(attribution.clone(), &causal_ref);
        // Engine-admission ruling (FIG-1488): this route deliberately stays
        // outside the gate. A delivery does not carry a caller-supplied engine
        // payload — it replays the subscription's own durable target and the
        // `target_identity` recorded when the subscription was registered, so
        // the admission decision was made once at registration. Re-gating per
        // occurrence would make a delivery fail on catalog drift the
        // subscription already survived, and every delivery for one reservation
        // must stay deterministic.
        let registration = crate::ProcessRegistration::new(
            target,
            crate::ProcessProvenance::new(subscription.registrant.clone())
                .with_caused_by(Some(causal_ref.clone())),
            crate::Lifetime::Detached,
        )
        .with_start_key(Some(trigger_delivery_start_key(reservation)))
        .with_trigger_delivery_pin(Some(crate::TriggerDeliveryPin {
            occurrence_id: occurrence.occurrence_id.clone(),
            subscription_id: subscription.subscription_id.clone(),
        }))
        .with_admitted_identity(crate::AdmittedProcessIdentity::pinned(
            subscription.target_identity.clone(),
        ))
        .with_extra_event_types(subscription.event_types.clone())
        .with_execution_env_ref(Some(subscription.env_ref.clone()))
        .with_wake_session_id(
            subscription
                .wake_target
                .as_ref()
                .map(|scope| scope.session_id.clone()),
        );
        let execution_context = crate::ProcessExecutionContext::default()
            .with_causal_invocation(Some(trigger_occurrence_invocation));
        Ok(DeliveryStart {
            command: crate::ProcessCommand::Start {
                registration,
                observers: subscription
                    .registrant_session_id()
                    .cloned()
                    .into_iter()
                    .collect(),
                execution_context: Box::new(execution_context),
            },
            attribution,
            causal_ref,
            route: self.captured_route(&subscription.source_capture),
        })
    }
}

/// The cause a delivery's journaled steps record: its occurrence, through
/// the subscription it was reserved for.
fn delivery_causal_ref(reservation: &TriggerDeliveryReservation) -> crate::CausalRef {
    let subscription = &reservation.subscription;
    crate::CausalRef::TriggerOccurrence {
        occurrence_id: reservation.occurrence.occurrence_id.clone(),
        subscription_id: Some(subscription.subscription_id.clone()),
        subscription_incarnation: Some(subscription.incarnation.clone()),
        subscription_revision: Some(subscription.revision),
    }
}

/// The attribution a delivery's journaled steps record: the session that
/// registered its subscription, when one did.
fn delivery_attribution(subscription: &TriggerSubscriptionRecord) -> crate::RuntimeAttribution {
    subscription
        .registrant_session_id()
        .cloned()
        .map(crate::RuntimeAttribution::for_session)
        .unwrap_or_else(crate::RuntimeAttribution::none)
}

/// A store fault inside a recorded trigger step: one a retry may answer
/// differently is the attempt's, and the step runs again; any other is the
/// step's recorded outcome.
fn recorded_store_fault(error: PluginError) -> crate::RuntimeEffectControllerError {
    let retryable = error.is_retryable();
    let fault = crate::RuntimeEffectControllerError::from(error);
    if retryable {
        fault.retryable_uncommitted_derivation()
    } else {
        fault
    }
}

/// A store fault inside an emission's recorded ingest or bind (FIG-4519,
/// FIG-4513).
///
/// A store that did not answer reports an opaque session error, which names
/// no cause. It is the attempt's, under the live code a plugin hook's opaque
/// failure settles as. Recorded, every replay would serve the outage as the
/// step's refusal: nothing would ever write the occurrence, and a delivery
/// whose process started would answer as failed for good. A typed refusal
/// is still the step's recorded outcome.
fn attempt_store_fault(error: PluginError) -> crate::RuntimeEffectControllerError {
    match error {
        error @ PluginError::Session(_) => crate::RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::PluginSessionManager,
            error.to_string(),
        )
        .retryable_uncommitted_derivation(),
        error => recorded_store_fault(error),
    }
}

/// Why a delivery's start steps did not answer its process.
enum DeliveryStartFault {
    /// The delivery did not start: its emission reports it failed, and its
    /// obligation recovers it.
    Delivery(PluginError),
    /// The store did not answer the delivery's bind, or the read of its
    /// binding: the fault is the attempt's, and the step runs again.
    Attempt(crate::RuntimeEffectControllerError),
}

impl DeliveryStartFault {
    /// The fault of a delivery's `AdmitTriggerDelivery` step.
    fn of_binding_step(fault: crate::RuntimeEffectControllerError) -> Self {
        if fault
            .journal_disposition(crate::RuntimeEffectKind::AdmitTriggerDelivery)
            .is_retryable_derivation()
        {
            Self::Attempt(fault)
        } else {
            Self::Delivery(fault.into())
        }
    }

    fn into_error(self) -> PluginError {
        match self {
            Self::Delivery(error) => error,
            Self::Attempt(fault) => fault.into(),
        }
    }
}

fn wrong_command(
    runner: &str,
    envelope: &crate::RuntimeEffectEnvelope,
) -> crate::RuntimeEffectControllerError {
    crate::RuntimeEffectControllerError::new(
        crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
        format!(
            "{runner} executor cannot execute {} command",
            envelope.command.kind().as_str()
        ),
    )
}

/// The first execution of one `IngestTriggerOccurrence` step: it ingests the
/// request the envelope names and records the store's receipt (FIG-4503).
struct OccurrenceIngestRunner {
    store: Arc<dyn TriggerStore>,
}

#[async_trait::async_trait]
impl crate::runtime::effect::executor::RuntimeEffectLocalRunner for OccurrenceIngestRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _usage_run: Option<crate::UsageRun>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::IngestTriggerOccurrence { request } = envelope.command
        else {
            return Err(wrong_command("trigger occurrence ingest", &envelope));
        };
        let receipt = self
            .store
            .ingest_occurrence(*request)
            .await
            .map_err(attempt_store_fault)?;
        Ok(crate::RuntimeEffectOutcome::IngestTriggerOccurrence {
            receipt: Box::new(receipt),
        })
    }
}

/// The first execution of the `AdmitTriggerDelivery` step an emission
/// records once its delivery's start registered a process (FIG-4503): it
/// binds the delivery to that process, releases the process's pin, and
/// records the binding. None of it enters the envelope, which names only the
/// delivery.
struct DeliveryBindRunner {
    store: Arc<dyn TriggerStore>,
    process_registry: Arc<dyn crate::ProcessRegistry>,
    occurrence_id: String,
    subscription_id: String,
    process_id: ProcessId,
}

#[async_trait::async_trait]
impl crate::runtime::effect::executor::RuntimeEffectLocalRunner for DeliveryBindRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _usage_run: Option<crate::UsageRun>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::AdmitTriggerDelivery { .. } = &envelope.command else {
            return Err(wrong_command("trigger delivery bind", &envelope));
        };
        self.store
            .bind_delivery_process(&self.occurrence_id, &self.subscription_id, &self.process_id)
            .await
            .map_err(attempt_store_fault)?;
        release_trigger_delivery_pin(self.process_registry.as_ref(), &self.process_id).await;
        Ok(crate::RuntimeEffectOutcome::AdmitTriggerDelivery {
            admission: Box::new(TriggerDeliveryAdmission::Bound {
                process_id: self.process_id,
            }),
        })
    }
}

/// The first execution of the `AdmitTriggerDelivery` step an emission
/// records once its start was refused as bound (FIG-4369): it reads the
/// process the store holds the delivery bound to. A store that did not answer
/// is the attempt's fault, and the step runs again.
struct BoundDeliveryAdmissionRunner {
    store: Arc<dyn TriggerStore>,
    occurrence_id: String,
    subscription_id: String,
}

#[async_trait::async_trait]
impl crate::runtime::effect::executor::RuntimeEffectLocalRunner for BoundDeliveryAdmissionRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _usage_run: Option<crate::UsageRun>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::AdmitTriggerDelivery { .. } = &envelope.command else {
            return Err(wrong_command("trigger delivery admission", &envelope));
        };
        let bound = self
            .store
            .list_deliveries_by_occurrence_id(&self.occurrence_id)
            .await
            .map_err(|error| {
                crate::RuntimeEffectControllerError::from(error).retryable_uncommitted_derivation()
            })?
            .into_iter()
            .find(|reservation| reservation.subscription.subscription_id == self.subscription_id)
            .and_then(|reservation| reservation.process_id);
        // A bind is written once, so a delivery the registrar found bound is
        // bound here, unless retention has since removed it.
        let Some(process_id) = bound else {
            return Err(crate::RuntimeEffectControllerError::foreign(
                "trigger_delivery_unbound",
                crate::TurnFailureCause::Outcome,
                format!(
                    "trigger delivery `{}`/`{}` refused its start as bound, and holds no \
                     binding now",
                    self.occurrence_id, self.subscription_id
                ),
            ));
        };
        Ok(crate::RuntimeEffectOutcome::AdmitTriggerDelivery {
            admission: Box::new(TriggerDeliveryAdmission::Bound { process_id }),
        })
    }
}

/// One delivery's prepared start: the process command, the attribution and
/// cause a journaled attempt records it under, and the captured route the
/// start's admission restores.
struct DeliveryStart {
    command: crate::ProcessCommand,
    attribution: crate::RuntimeAttribution,
    causal_ref: crate::CausalRef,
    route: Option<TriggerRouteRestore>,
}

/// What a relay's registration of an unbound delivery's start answered.
enum RecoveredDeliveryStart {
    /// The start's key holds this process, newly registered or retained.
    Registered(ProcessId),
    /// The delivery was bound, and its process pruned, since the relay read
    /// it: the registrar registered nothing (FIG-4369).
    AlreadyBound,
}

/// Release the pin a delivery's registration wrote on `process_id`, once the
/// delivery's bind committed (ADR 0021, FIG-4203).
///
/// The delivery is bound whatever the release answers, so a failed release
/// fails nothing: it only keeps the process from being pruned until the
/// retention pass's
/// [`release_bound_trigger_delivery_pins`](crate::runtime::release_bound_trigger_delivery_pins)
/// finds the delivery bound and releases the pin itself.
async fn release_trigger_delivery_pin(
    registry: &dyn crate::ProcessRegistry,
    process_id: &ProcessId,
) {
    if let Err(error) = registry.release_trigger_delivery_pin(process_id).await {
        tracing::warn!(
            process_id = %process_id,
            %error,
            "trigger delivery bound; its pin release failed, and the retention pass releases it"
        );
    }
}

/// Why [`TriggerRouter::recover_delivery`] did not bind a process.
#[derive(Debug, thiserror::Error)]
pub enum TriggerDeliveryRecoveryError {
    /// The reservation can never start as reserved; the obligation stalls.
    #[error("{0}")]
    Refused(PluginError),
    /// A later attempt may start it under the same identity.
    #[error("{0}")]
    Retryable(PluginError),
}

impl TriggerDeliveryRecoveryError {
    /// A store or registration failure: refused when terminal, else retried.
    fn classified(error: PluginError) -> Self {
        if error.is_terminal() {
            Self::Refused(error)
        } else {
            Self::Retryable(error)
        }
    }
}

impl TriggerRouter {
    /// The route restore one unbound delivery's start carries into its
    /// recorded admission (FIG-4554).
    ///
    /// The restorer is a live host service handed only the recorded capture.
    /// Nothing asks it here: the start's admission does, on the step's first
    /// execution and while no process holds the start's key, so a replay and
    /// a redrive answer from what was recorded.
    ///
    /// A resident source needs nothing. A provider route with no restorer wired
    /// is left as captured: the host that never installed a restorer has no
    /// revocation policy to consult, and inventing one here would be a fresh
    /// authorization decision. A restorer that answers `Unavailable` leaves the
    /// reservation durable so its recovery retries the identical delivery
    /// identity; `Revoked` refuses visibly and nothing re-resolves the source.
    fn captured_route(&self, capture: &TriggerSourceCapture) -> Option<TriggerRouteRestore> {
        if matches!(capture.route, TriggerProviderRoute::Resident) {
            return None;
        }
        let restorer = self.route_restorer.as_ref()?;
        Some(TriggerRouteRestore::new(
            Arc::clone(restorer),
            capture.clone(),
        ))
    }
}

fn materialize_trigger_process_args(
    input_template: &BTreeMap<String, TriggerInputBinding>,
    event_payload: &serde_json::Value,
) -> Result<serde_json::Map<String, serde_json::Value>, PluginError> {
    let mut args = serde_json::Map::new();
    for (input_name, input) in input_template {
        let value = match input {
            TriggerInputBinding::Event => event_payload.clone(),
            TriggerInputBinding::Fixed { value } => value.clone(),
        };
        args.insert(input_name.to_string(), value);
    }
    Ok(args)
}

fn apply_trigger_inputs(
    mut target: crate::ProcessInput,
    args: serde_json::Map<String, serde_json::Value>,
) -> Result<crate::ProcessInput, PluginError> {
    match &mut target {
        crate::ProcessInput::Definition {
            args: target_args, ..
        } => {
            *target_args = args;
            Ok(target)
        }
        crate::ProcessInput::Engine { payload, .. } => {
            let object = payload.as_object_mut().ok_or_else(|| {
                PluginError::Session(
                    "trigger engine target payload must be a JSON object".to_string(),
                )
            })?;
            object.insert("args".to_string(), serde_json::Value::Object(args));
            Ok(target)
        }
        other => Err(PluginError::Session(format!(
            "trigger target must be an engine process, got {}",
            other.engine_kind()
        ))),
    }
}

pub fn validate_trigger_occurrence_request(
    request: &TriggerOccurrenceRequest,
) -> Result<(), PluginError> {
    if request.source_type.trim().is_empty() {
        return Err(PluginError::Session(
            "trigger occurrence requires source_type".to_string(),
        ));
    }
    if request.source_key.trim().is_empty() {
        return Err(PluginError::Session(
            "trigger occurrence requires source_key".to_string(),
        ));
    }
    if request.idempotency_key.trim().is_empty() {
        return Err(PluginError::Session(
            "trigger occurrence requires idempotency_key".to_string(),
        ));
    }
    match &request.outcome {
        TriggerOccurrenceOutcome::Fired => {}
        TriggerOccurrenceOutcome::Dropped { reason } => {
            if reason.trim().is_empty() {
                return Err(PluginError::Session(
                    "dropped trigger occurrence requires reason".to_string(),
                ));
            }
        }
    }
    Ok(())
}

pub fn trigger_occurrence_request_matches_record(
    request: &TriggerOccurrenceRequest,
    record: &TriggerOccurrenceRecord,
) -> bool {
    let TriggerOccurrenceRequest {
        source_type,
        source_key,
        payload,
        idempotency_key: _,
        source,
        session_id: _,
        outcome,
    } = request;
    let TriggerOccurrenceRecord {
        occurrence_id: _,
        source_type: stored_source_type,
        source_key: stored_source_key,
        payload: stored_payload,
        idempotency_key: _,
        source: stored_source,
        session_id: _,
        outcome: stored_outcome,
        occurred_at_ms: _,
    } = record;
    source_type == stored_source_type
        && source_key == stored_source_key
        && crate::identity_json::payloads_equal(payload, stored_payload)
        && crate::identity_json::optional_payloads_equal(source.as_ref(), stored_source.as_ref())
        && outcome == stored_outcome
}

#[cfg(test)]
#[path = "router/tests.rs"]
mod tests;
