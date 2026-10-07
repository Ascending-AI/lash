/// version_surface = "coexist"
/// version_guard(items(LASH_DERIVED_TRIGGER_SUBSCRIPTION_DOMAIN_VERSION, derived_trigger_subscription_key))
const LASH_DERIVED_TRIGGER_SUBSCRIPTION_DOMAIN_VERSION: &str =
    "lash-derived-trigger-subscription/v2";

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
/// version_surface = "coexist"
pub(super) const TRIGGER_DEFINITION_FAMILY_VERSION: u8 = 3;
/// version_guard(
///     items(trigger_subscription_address_preimage, project_trigger_owner),
/// )
/// version_surface = "coexist"
const TRIGGER_LOOKUP_FAMILY_VERSION: u8 = 2;
/// version_guard(
///     items(
///         path = "crates/lash-core-execution/src/triggers/router.rs",
///         path = "crates/lash-core-execution/src/runtime/process/identity_projection.rs",
///         trigger_source_preimage, project_process_payload_leaf,
///     ),
/// )
/// version_surface = "coexist"
const TRIGGER_SOURCE_FAMILY_VERSION: u8 = 1;
/// version_guard(
///     items(derived_trigger_subscription_key),
/// )
/// version_surface = "coexist"
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
/// Process inputs: 1 burned, 2 engine, 3 session turn, 4 burned.
/// Tool output contracts: 1 static, 2 from-input-schema.
/// Arbitrary JSON and schemas are each one canonical opaque bytes leaf.
/// Value selectors: 1 payload, 2 pointer, 3 const, 4 template, 5 present.
/// Process statuses: 1 running, 2 waiting, 3 completed, 4 failed, 5 cancelled, 6 abandoned, 7
/// burned.
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
    project_process_schema_leaf(identity, payload_schema.as_value());
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
    project_process_schema_leaf(identity, config_schema.as_value());
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
    target: &crate::ProcessStartTarget,
) {
    let input = match target {
        crate::ProcessStartTarget::Input(input) => input,
        crate::ProcessStartTarget::Definition {
            definition_id,
            args,
            ..
        } => {
            identity.tag(5);
            identity.string(definition_id.as_str());
            project_process_payload_leaf(identity, &serde_json::Value::Object(args.clone()));
            return;
        }
    };
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
                    identity.optional(
                        schema.as_ref().map(crate::JsonSchema::as_value),
                        project_process_schema_leaf,
                    );
                }
            }
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
        crate::stable_hash::blake3_hex(
            LASH_DERIVED_TRIGGER_SUBSCRIPTION_DOMAIN_VERSION,
            &identity.finish()
        )
    )
}

/// The start key of the one process a trigger delivery starts (ADR 0107).
///
/// Derived from the delivery's identity — its occurrence and the exact
/// subscription revision it was reserved against — so every attempt at the
/// delivery, the first and every recovery, presents the same key.
pub fn trigger_delivery_start_key(reservation: &TriggerDeliveryReservation) -> crate::StartKey {
    delivery_start_key(&reservation.occurrence, &reservation.subscription)
}

pub(crate) fn delivery_start_key(
    occurrence: &TriggerOccurrenceRecord,
    subscription: &TriggerSubscriptionRecord,
) -> crate::StartKey {
    crate::StartKeyDerivation::LASH_START_PATHS.for_trigger_delivery(
        &occurrence.occurrence_id,
        &subscription.subscription_id,
        &subscription.incarnation,
        subscription.revision,
    )
}

/// The refusal a recorded emission raises when one of its deliveries did not
/// start. See [`TriggerRouter::emit_recorded`] for why this is an error rather
/// than a `Failed` entry in an otherwise successful report.
fn unstarted_delivery(
    subscription_id: &str,
    code: &crate::RuntimeErrorCode,
    reason: &str,
    value_mismatch: Option<&lash_sansio::ValueMismatch>,
) -> PluginError {
    let mut error = crate::RuntimeEffectControllerError::new(
        code.clone(),
        format!("trigger delivery for subscription `{subscription_id}` did not start: {reason}"),
    );
    if let Some(source) = value_mismatch {
        error.cause = Some(crate::RuntimeErrorCause::ValueMismatch {
            context: format!("trigger delivery for subscription `{subscription_id}`")
                .into_boxed_str(),
            source: Box::new(source.clone()),
        });
    }
    error.into()
}

/// How many times an emission plans its occurrence again when the
/// subscriptions it matches moved between its plan and its commit. Each round
/// prepares every delivery afresh; a subscription set that keeps moving faster
/// than an emission commits is a contention the caller retries.
const START_ATTEMPTS: usize = 8;

#[derive(Clone)]
pub struct TriggerRouter {
    tracing: Option<crate::trace::TraceRuntime>,
    store: Arc<dyn TriggerStore>,
    process_work: crate::ProcessWorkWiring,
    process_env_store: Option<Arc<dyn crate::ProcessExecutionEnvStore>>,
    process_engines: Option<crate::ProcessEngineRegistry>,
    route_restorer: Option<Arc<dyn TriggerRouteRestorer>>,
}

impl TriggerRouter {
    pub fn new(store: Arc<dyn TriggerStore>, process_work: crate::ProcessWorkWiring) -> Self {
        Self {
            store,
            process_work,
            process_env_store: None,
            process_engines: None,
            tracing: None,
            route_restorer: None,
        }
    }

    pub fn with_route_restorer(mut self, restorer: Arc<dyn TriggerRouteRestorer>) -> Self {
        self.route_restorer = Some(restorer);
        self
    }

    #[must_use]
    pub fn with_trace_runtime(mut self, tracing: crate::trace::TraceRuntime) -> Self {
        self.tracing = Some(tracing);
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
    /// settles the report, so redriving that one declaration returns the same
    /// bytes.
    ///
    /// A recorded declaration's report becomes the durable, wire-visible
    /// `ToolIntentExecutionOutcome::Executed` result. [`Self::emit`] reports
    /// `Started` for every delivery the occurrence holds, on the first shift
    /// and every redrive alike, since each delivery is bound to its process
    /// in the transaction that recorded it. A redrive after retention
    /// reclaimed the occurrence is refused by the store's tombstone, and the
    /// declaration fails with that typed refusal (FIG-4513).
    ///
    /// A delivery refused before its start committed carries no such
    /// statement: its reason is a live error string. Reporting that inside a
    /// successful outcome would both call a failure a success and put
    /// redrive-varying bytes on the wire, so a refused delivery fails the
    /// whole declaration instead — the caller turns the error into the
    /// intent's own refusal, which is where a command that did not happen
    /// belongs.
    pub async fn emit_recorded(
        &self,
        request: TriggerOccurrenceRequest,
        effect_controller: &crate::ActorContext,
    ) -> Result<TriggerEmitReport, PluginError> {
        self.emit_recorded_reporting_realization(request, effect_controller)
            .await
            .map(|(report, _)| report)
    }

    /// [`Self::emit_recorded`], also reporting whether this call recorded the
    /// occurrence or found one already held under the same idempotency key
    /// (FIG-3070).
    ///
    /// The occurrence idempotency key is the dedupe point for a re-submitted
    /// emission, so a caller that reports replay to a host -- the tool-intent
    /// ingress -- can only learn it from the store. The verdict rides beside
    /// the report rather than inside it: it describes this call, not the
    /// occurrence, and `TriggerEmitReport` crosses the remote peer wire where
    /// a per-call field would be a protocol change for a fact the wire never
    /// had to carry.
    pub async fn emit_recorded_reporting_realization(
        &self,
        request: TriggerOccurrenceRequest,
        effect_controller: &crate::ActorContext,
    ) -> Result<(TriggerEmitReport, crate::StoreRealization), PluginError> {
        let (report, realization) = self
            .emit_reporting_realization(request, effect_controller)
            .await?;
        for delivery in &report.deliveries {
            if let TriggerDeliveryEmitOutcome::Failed {
                code,
                reason,
                value_mismatch,
            } = &delivery.outcome
            {
                return Err(unstarted_delivery(
                    &delivery.subscription_id,
                    code,
                    reason,
                    value_mismatch.as_deref(),
                ));
            }
        }
        Ok((report, realization))
    }

    pub async fn emit(
        &self,
        request: TriggerOccurrenceRequest,
        effect_controller: &crate::ActorContext,
    ) -> Result<TriggerEmitReport, PluginError> {
        self.emit_reporting_realization(request, effect_controller)
            .await
            .map(|(report, _)| report)
    }

    /// [`Self::emit`], also reporting the trigger store's occurrence-key
    /// verdict for this call (FIG-3070).
    ///
    /// The occurrence is planned, every delivery's process is prepared, and
    /// one `trigger.start` commit records the occurrence with each delivery
    /// bound to its process, whose actor it creates ready (ADR 0132 §12),
    /// through `effect_controller`'s store. A crash before that commit leaves
    /// nothing, and the emission runs again
    /// from its plan; after it, the occurrence is held and every later
    /// emission of it answers its bound deliveries.
    pub async fn emit_reporting_realization(
        &self,
        request: TriggerOccurrenceRequest,
        effect_controller: &crate::ActorContext,
    ) -> Result<(TriggerEmitReport, crate::StoreRealization), PluginError> {
        validate_trigger_occurrence_request(&request)?;
        for _ in 0..START_ATTEMPTS {
            let (occurrence, subscriptions) = match self.store.plan_occurrence(&request).await? {
                TriggerOccurrencePlan::Held(receipt) => return Ok(held_emission(receipt)),
                TriggerOccurrencePlan::Fresh {
                    occurrence,
                    subscriptions,
                } => (occurrence, subscriptions),
            };
            if let Some(report) =
                Box::pin(self.start_occurrence(effect_controller, occurrence, subscriptions))
                    .await?
            {
                return Ok((report, crate::StoreRealization::Realized));
            }
        }
        Err(PluginError::StoreUnavailable {
            fault: crate::store::StoreFault::Contended,
        })
    }

    /// Prepare every delivery of a fresh `occurrence` and commit them with it
    /// in one `trigger.start` transaction. `None` when the plan no longer
    /// holds: the occurrence was recorded meanwhile, or the subscriptions it
    /// matches moved.
    async fn start_occurrence(
        &self,
        cx: &crate::ActorContext,
        occurrence: TriggerOccurrenceRecord,
        mut subscriptions: Vec<TriggerSubscriptionRecord>,
    ) -> Result<Option<TriggerEmitReport>, PluginError> {
        sort_trigger_subscriptions(&mut subscriptions);
        let planned = subscriptions
            .iter()
            .map(TriggerSubscriptionFence::of)
            .collect();
        let engines = self.process_engines.clone().unwrap_or_default();
        let registry = Arc::clone(self.process_work.registry());
        let starts = subscriptions
            .iter()
            .map(|subscription| DeliveryStart::of(self, &occurrence, subscription))
            .collect::<Result<Vec<_>, _>>()?;
        // Each delivery's outcome, in delivery order: its prepared start, or
        // the refusal that keeps it from ever starting as planned.
        let mut prepared = Vec::with_capacity(starts.len());
        for start in &starts {
            let stores = start.stores(self, &engines, registry.as_ref());
            let registration = match &start.registration {
                Ok(registration) => registration.clone(),
                Err(refusal) => {
                    prepared.push(Err(refusal.clone()));
                    continue;
                }
            };
            match Box::pin(crate::runtime::stage_process_start(
                &stores,
                registration,
                &start.observers,
            ))
            .await
            {
                // A process holds the delivery's key: the occurrence was
                // recorded since the plan read it.
                Ok(staged) if staged.registration.retained() => {
                    staged.staging.abandon(&stores).await?;
                    self.abandon(&starts, prepared, &engines, registry.as_ref())
                        .await?;
                    return Ok(None);
                }
                Ok(staged) => prepared.push(Ok(staged)),
                Err(error) if error.is_terminal() => prepared.push(Err(error.into())),
                Err(error) => {
                    self.abandon(&starts, prepared, &engines, registry.as_ref())
                        .await?;
                    return Err(error.into());
                }
            }
        }
        let mut deliveries = Vec::new();
        let mut staged = Vec::with_capacity(prepared.len());
        for (start, outcome) in starts.iter().zip(prepared) {
            match outcome {
                Ok(crate::runtime::PreparedProcessStart {
                    staging,
                    registration,
                }) => {
                    let (registration, observers, process_id, _, _) =
                        registration.into_commit(staging.anchor());
                    deliveries.push(TriggerDeliveryStartRows {
                        subscription: start.subscription.clone(),
                        registration,
                        observers,
                        process_id,
                    });
                    staged.push(Ok(staging));
                }
                Err(refusal) => staged.push(Err(refusal)),
            }
        }
        let rows = TriggerStartRows {
            occurrence: occurrence.clone(),
            planned,
            deliveries,
        };
        let committed = cx
            .commit_mail(rows.mail_tx()?, lash_durable::CommitLabel::TRIGGER_START)
            .await;
        let processes = match rows.answer(committed) {
            Ok(Some(processes)) => processes,
            // Nothing committed: what the starts staged is released before
            // the emission plans again or fails.
            unstarted => {
                for (start, staging) in starts.iter().zip(staged) {
                    if let Ok(staging) = staging {
                        staging
                            .abandon(&start.stores(self, &engines, registry.as_ref()))
                            .await?;
                    }
                }
                return unstarted.map(|_| None);
            }
        };
        let mut processes = processes.into_iter();
        let mut receipts = Vec::with_capacity(starts.len());
        for (start, staging) in starts.iter().zip(staged) {
            let outcome = match staging {
                Ok(staging) => {
                    let process_id = processes.next().ok_or_else(|| {
                        PluginError::Session(format!(
                            "trigger occurrence `{}` started fewer processes than deliveries",
                            occurrence.occurrence_id
                        ))
                    })?;
                    // The row and its binding committed: what the start
                    // staged is held under its record now.
                    let record = registry.get_process(&process_id).await?.ok_or_else(|| {
                        PluginError::Session(format!(
                            "trigger delivery process `{process_id}` is missing after its start committed"
                        ))
                    })?;
                    staging
                        .adopt(
                            &start.stores(self, &engines, registry.as_ref()),
                            Ok(crate::ProcessRegistrationReceipt::created(record)),
                        )
                        .await?;
                    TriggerDeliveryEmitOutcome::Started { process_id }
                }
                Err(refusal) => refused_delivery(refusal),
            };
            receipts.push(TriggerDeliveryEmitReceipt {
                occurrence_id: occurrence.occurrence_id.clone(),
                subscription_id: start.subscription.subscription_id.clone(),
                outcome,
            });
        }
        Ok(Some(TriggerEmitReport::new(
            occurrence.occurrence_id,
            receipts,
        )))
    }

    /// Give up the starts an emission prepared before it stopped short of
    /// its commit.
    async fn abandon(
        &self,
        starts: &[DeliveryStart],
        prepared: Vec<Result<crate::runtime::PreparedProcessStart<'_>, PluginError>>,
        engines: &crate::ProcessEngineRegistry,
        registry: &dyn crate::ProcessRegistry,
    ) -> Result<(), PluginError> {
        for (start, prepared) in starts.iter().zip(prepared) {
            if let Ok(prepared) = prepared {
                prepared
                    .staging
                    .abandon(&start.stores(self, engines, registry))
                    .await?;
            }
        }
        Ok(())
    }
}

/// The outcome of a delivery refused before its start committed.
fn refused_delivery(refusal: PluginError) -> TriggerDeliveryEmitOutcome {
    let error = crate::RuntimeEffectControllerError::from(refusal);
    let value_mismatch = match &error.cause {
        Some(crate::RuntimeErrorCause::ValueMismatch { source, .. }) => Some(source.clone()),
        _ => None,
    };
    TriggerDeliveryEmitOutcome::Failed {
        code: error.code,
        reason: error.message,
        value_mismatch,
    }
}

/// The report of an occurrence the store already holds: each delivery it
/// holds, bound to its process.
fn held_emission(receipt: TriggerIngressReceipt) -> (TriggerEmitReport, crate::StoreRealization) {
    let TriggerIngressReceipt {
        occurrence,
        reservations,
        realization,
    } = receipt;
    let deliveries = reservations
        .into_iter()
        .map(|reservation| TriggerDeliveryEmitReceipt {
            occurrence_id: reservation.occurrence.occurrence_id,
            subscription_id: reservation.subscription.subscription_id,
            outcome: TriggerDeliveryEmitOutcome::Started {
                process_id: reservation.process_id,
            },
        })
        .collect();
    (
        TriggerEmitReport::new(occurrence.occurrence_id, deliveries),
        realization,
    )
}

/// Everything one delivery's start needs, derived only from its occurrence
/// and the subscription snapshot it is planned against: the registration it
/// starts, or the reason it can never start as planned.
/// It consults nothing live: the captured route is handed on for the start's
/// admission to restore (FIG-4554).
struct DeliveryStart {
    subscription: TriggerSubscriptionRecord,
    registration: Result<crate::ProcessStartRegistration, PluginError>,
    observers: Vec<crate::SessionId>,
    route: Option<TriggerRouteRestore>,
    starter: lash_sansio::EffectJournalIdentity,
}

impl DeliveryStart {
    fn of(
        router: &TriggerRouter,
        occurrence: &TriggerOccurrenceRecord,
        subscription: &TriggerSubscriptionRecord,
    ) -> Result<Self, PluginError> {
        let start_key = delivery_start_key(occurrence, subscription);
        let starter = crate::runtime::start_operation_journal(&start_key)
            .map_err(|error| PluginError::Session(error.to_string()))?;
        Ok(Self {
            subscription: subscription.clone(),
            registration: delivery_registration(occurrence, subscription, start_key),
            observers: subscription
                .registrant_session_id()
                .cloned()
                .into_iter()
                .collect(),
            route: router.captured_route(&subscription.source_capture),
            starter,
        })
    }

    fn stores<'a>(
        &'a self,
        router: &'a TriggerRouter,
        engines: &'a crate::ProcessEngineRegistry,
        registry: &'a dyn crate::ProcessRegistry,
    ) -> crate::runtime::ProcessStartStores<'a> {
        crate::runtime::ProcessStartStores {
            tracing: router.tracing.as_ref(),
            registry,
            env_store: router.process_env_store.as_ref(),
            engines,
            // A trigger delivery's start is never a host-granted root.
            session_catalog: None,
            session_turn_admission: None,
            executor: "trigger delivery start",
            starter: &self.starter,
            trigger_route: self.route.as_ref(),
        }
    }
}

/// The registration one delivery starts.
///
/// # Errors
///
/// The reservation can never start as reserved: its payload or source leaves
/// the captured contract, or its target does not take its inputs.
fn delivery_registration(
    occurrence: &TriggerOccurrenceRecord,
    subscription: &TriggerSubscriptionRecord,
    start_key: crate::StartKey,
) -> Result<crate::ProcessStartRegistration, PluginError> {
    // Delivery validates against the contract this subscription captured at
    // registration, never against whatever the live catalog now says.
    subscription
        .payload_schema
        .validate(&occurrence.payload)
        .map_err(|err| PluginError::ValueMismatch {
            context: format!("payload for trigger `{}`", subscription.subscription_key),
            source: Box::new(err),
        })?;
    if let Some(source) = occurrence.source.as_ref() {
        subscription
            .source_capture
            .config_schema
            .validate(source)
            .map_err(|err| PluginError::ValueMismatch {
                context: format!("source for trigger `{}`", subscription.subscription_key),
                source: Box::new(err),
            })?;
    }
    let args = materialize_trigger_process_args(&subscription.input_template, &occurrence.payload)?;
    let target = apply_trigger_inputs(subscription.target.clone(), args)?;
    let causal_ref = delivery_causal_ref(occurrence, subscription);
    Ok(crate::ProcessStartRegistration::of_target(
        target,
        crate::ProcessProvenance::new(subscription.registrant.clone())
            .with_caused_by(Some(causal_ref)),
        crate::Lifetime::Detached,
    )
    .with_start_key(Some(start_key))
    .with_admitted_identity(crate::AdmittedProcessIdentity::pinned(
        subscription.target_identity.clone(),
    ))
    .with_extra_event_types(subscription.event_types.clone())
    .with_execution_env_ref(Some(subscription.env_ref.clone()))
    // Each delivery is independent work the fire produced: its process links
    // the occurrence's retained anchor.
    .with_trace(lash_trace::TraceScopeOffer::caused_by(
        occurrence
            .trace
            .as_ref()
            .map(lash_trace::DurableTraceScope::linked_cause)
            .unwrap_or_default(),
    ))
    .with_wake_session_id(
        subscription
            .wake_target
            .as_ref()
            .map(|scope| scope.session_id.clone()),
    ))
}

/// The cause a delivery's process records: its occurrence, through the
/// subscription it was reserved for.
fn delivery_causal_ref(
    occurrence: &TriggerOccurrenceRecord,
    subscription: &TriggerSubscriptionRecord,
) -> crate::CausalRef {
    crate::CausalRef::TriggerOccurrence {
        occurrence_id: occurrence.occurrence_id.clone(),
        subscription_id: Some(subscription.subscription_id.clone()),
        subscription_incarnation: Some(subscription.incarnation.clone()),
        subscription_revision: Some(subscription.revision),
    }
}

impl TriggerRouter {
    /// The route restore one delivery's start carries into its admission
    /// (FIG-4554).
    ///
    /// The restorer is a live host service handed only the captured route.
    /// Nothing asks it here: the start's admission does, before the start's
    /// commit and while no process holds the start's key.
    ///
    /// A resident source needs nothing. A provider route with no restorer wired
    /// is left as captured: the host that never installed a restorer has no
    /// revocation policy to consult, and inventing one here would be a fresh
    /// authorization decision. A restorer that answers `Unavailable` fails the
    /// emission before anything commits, so its retry starts the identical
    /// delivery; `Revoked` refuses the delivery visibly and nothing
    /// re-resolves the source.
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
    mut target: crate::ProcessStartTarget,
    args: serde_json::Map<String, serde_json::Value>,
) -> Result<crate::ProcessStartTarget, PluginError> {
    match &mut target {
        crate::ProcessStartTarget::Definition {
            args: target_args, ..
        } => {
            *target_args = args;
            Ok(target)
        }
        crate::ProcessStartTarget::Input(crate::ProcessInput::Engine { payload, .. }) => {
            let object = payload.as_object_mut().ok_or_else(|| {
                PluginError::Session(
                    "trigger engine target payload must be a JSON object".to_string(),
                )
            })?;
            object.insert("args".to_string(), serde_json::Value::Object(args));
            Ok(target)
        }
        crate::ProcessStartTarget::Input(other @ crate::ProcessInput::SessionTurn { .. }) => {
            Err(PluginError::Session(format!(
                "trigger target must be an engine process, got {}",
                other.engine_kind()
            )))
        }
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
        trace: _,
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
        trace: _,
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
