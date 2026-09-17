use super::*;

// Bumped to 3 (FIG-2913): the preimage projects the admitted source
// contract and provider route the subscription now captures.
const LEGACY_TRIGGER_DEFINITION_FAMILY_VERSION: u8 = 3;
// Bumped to 5 (FIG-2913): the subscription-definition preimage projects the
// admitted source contract and provider route captured at registration. It was
// 4 (FIG-1383) when the process-status tag registry gained `caller_departed`;
// see the process-registration family note.
pub(super) const TRIGGER_DEFINITION_FAMILY_VERSION: u8 = 5;
const TRIGGER_LOOKUP_FAMILY_VERSION: u8 = 2;
const TRIGGER_SOURCE_FAMILY_VERSION: u8 = 1;
const TRIGGER_DELIVERY_PROCESS_FAMILY_VERSION: u8 = 1;
const DERIVED_TRIGGER_SUBSCRIPTION_FAMILY_VERSION: u8 = 3;

pub(super) fn trigger_definition_family_version(draft: &TriggerSubscriptionDraft) -> u8 {
    match &draft.target {
        crate::ProcessInput::ToolCall { call }
            if call
                .replay
                .as_ref()
                .and_then(|replay| replay.origin.as_ref())
                .is_some() =>
        {
            TRIGGER_DEFINITION_FAMILY_VERSION
        }
        _ => LEGACY_TRIGGER_DEFINITION_FAMILY_VERSION,
    }
}

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
/// Owners: 1 session, 2 host, 3 platform. Actors: 1 host, 2 session. Process
/// inputs: 1 tool call, 2 engine, 3 session turn, 4 external. Tool output
/// contracts: 1 static, 2 from-input-schema. Input bindings: 1 event, 2 fixed.
/// Arbitrary JSON and schemas are each one canonical opaque bytes leaf. Value
/// selectors: 1 payload, 2 pointer, 3 const, 4 template,
/// 5 present. Process statuses: 1 running, 2 waiting, 3 completed, 4 failed,
/// 5 cancelled, 6 abandoned, 7 caller departed. Retired tags remain burned.
fn trigger_subscription_definition_preimage(
    owner_scope: &TriggerOwnerScope,
    draft: &TriggerSubscriptionDraft,
) -> Vec<u8> {
    let family_version = trigger_definition_family_version(draft);
    let mut fingerprint = crate::stable_identity::IdentityEncoder::new(
        "lash.trigger-subscription-definition",
        family_version,
    );
    project_trigger_owner(&mut fingerprint, owner_scope);
    project_trigger_draft(&mut fingerprint, draft, family_version);
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
    let family_version = trigger_definition_family_version(draft);
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
    family_version: u8,
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
    project_trigger_payload_leaf(identity, source);
    project_trigger_schema_leaf(identity, &payload_schema.schema);
    project_trigger_source_capture(identity, source_capture);
    project_trigger_process_input(identity, target, family_version);
    let crate::ProcessIdentity {
        kind,
        label,
        definition,
    } = target_identity;
    identity.string(kind.as_str());
    identity.optional(label.as_deref(), |identity, label| identity.string(label));
    // A target identity's definition reference projects as the engine-owned
    // definition value alone: the engine kind is already fixed by `kind`, and the
    // signature is a claim the engine resolves, never part of what the
    // subscription names. The bytes therefore stay identical to the pre-FIG-2992
    // untyped blob and no family rotation is needed.
    identity.optional(definition.as_ref(), |identity, definition| {
        project_trigger_payload_leaf(identity, definition.definition.as_json());
    });
    let mut event_types = event_types.iter().collect::<Vec<_>>();
    event_types.sort_by(|left, right| left.name.cmp(&right.name));
    identity.sequence(event_types, |identity, event_type| {
        project_trigger_event_type(identity, event_type);
    });
    identity.sequence(input_template.iter(), |identity, (name, binding)| {
        identity.string(name);
        match binding {
            TriggerInputBinding::Event => identity.tag(1),
            TriggerInputBinding::Fixed { value } => {
                identity.tag(2);
                project_trigger_payload_leaf(identity, value);
            }
        }
    });
    identity.optional(target_label.as_deref(), |identity, label| {
        identity.string(label)
    });
}

/// Projects the admitted source contract and provider route.
///
/// Route tags: 1 resident, 2 provider. The opaque route is one canonical
/// payload leaf; the configuration contract is one canonical schema leaf.
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
    project_trigger_schema_leaf(identity, &config_schema.schema);
    match route {
        TriggerProviderRoute::Resident => identity.tag(1),
        TriggerProviderRoute::Provider { provider_id, route } => {
            identity.tag(2);
            identity.string(provider_id);
            project_trigger_payload_leaf(identity, route);
        }
    }
}

fn project_trigger_event_type(
    identity: &mut crate::stable_identity::IdentityEncoder,
    event_type: &crate::ProcessEventType,
) {
    let crate::ProcessEventType {
        name,
        payload_schema,
        semantics,
    } = event_type;
    identity.string(name);
    let crate::LashSchema { schema } = payload_schema;
    project_trigger_schema_leaf(identity, schema);
    let crate::ProcessEventSemanticsSpec { terminal, wake } = semantics;
    identity.optional(terminal.as_ref(), |identity, terminal| {
        let crate::ProcessTerminalSpec {
            status,
            await_output,
        } = terminal;
        identity.tag(match status {
            crate::ProcessStatus::Running => 1,
            crate::ProcessStatus::Waiting => 2,
            crate::ProcessStatus::Completed => 3,
            crate::ProcessStatus::Failed => 4,
            crate::ProcessStatus::Cancelled => 5,
            crate::ProcessStatus::Abandoned => 6,
            crate::ProcessStatus::CallerDeparted => 7,
        });
        identity.optional(await_output.as_ref(), project_trigger_value_selector);
    });
    identity.optional(wake.as_ref(), |identity, wake| {
        let crate::ProcessWakeSpec { when, input } = wake;
        identity.optional(when.as_ref(), project_trigger_value_selector);
        project_trigger_value_selector(identity, input);
    });
}

fn project_trigger_value_selector(
    identity: &mut crate::stable_identity::IdentityEncoder,
    selector: &crate::ProcessValueSelector,
) {
    match selector {
        crate::ProcessValueSelector::Payload => identity.tag(1),
        crate::ProcessValueSelector::Pointer(pointer) => {
            identity.tag(2);
            identity.string(pointer);
        }
        crate::ProcessValueSelector::Const(value) => {
            identity.tag(3);
            project_trigger_payload_leaf(identity, value);
        }
        crate::ProcessValueSelector::Template { template, fields } => {
            identity.tag(4);
            identity.string(template);
            identity.sequence(fields.iter(), |identity, (name, selector)| {
                identity.string(name);
                project_trigger_value_selector(identity, selector);
            });
        }
        crate::ProcessValueSelector::Present(pointer) => {
            identity.tag(5);
            identity.string(pointer);
        }
    }
}

fn project_trigger_process_input(
    identity: &mut crate::stable_identity::IdentityEncoder,
    input: &crate::ProcessInput,
    family_version: u8,
) {
    match input {
        crate::ProcessInput::ToolCall { call } => {
            let crate::PreparedToolCall {
                call_id,
                tool_id,
                tool_name,
                args,
                replay,
                prepared_payload,
            } = call;
            identity.tag(1);
            identity.string(call_id);
            identity.string(tool_id.as_str());
            identity.string(tool_name);
            project_trigger_payload_leaf(identity, args);
            identity.optional(replay.as_ref(), |identity, replay| {
                let lash_sansio::llm::types::ProviderReplayMeta {
                    item_id,
                    opaque,
                    origin,
                } = replay;
                identity.optional(item_id.as_deref(), |identity, value| identity.string(value));
                identity.optional(opaque.as_deref(), |identity, value| identity.string(value));
                if family_version == TRIGGER_DEFINITION_FAMILY_VERSION {
                    identity.optional(origin.as_ref(), crate::stable_identity::provider_route);
                }
            });
            project_trigger_payload_leaf(identity, prepared_payload);
        }
        crate::ProcessInput::Engine { kind, payload } => {
            identity.tag(2);
            identity.string(kind);
            project_trigger_payload_leaf(identity, payload);
        }
        crate::ProcessInput::SessionTurn {
            definition_key,
            create_request: _,
            turn_input: _,
            output_contract,
        } => {
            identity.tag(3);
            identity.string(definition_key);
            match output_contract {
                crate::ToolOutputContract::Static => identity.tag(1),
                crate::ToolOutputContract::FromInputSchema {
                    input_field,
                    default_schema,
                } => {
                    identity.tag(2);
                    identity.string(input_field);
                    identity.optional(default_schema.as_ref(), project_trigger_schema_leaf);
                }
            }
        }
        crate::ProcessInput::External { metadata } => {
            identity.tag(4);
            project_trigger_payload_leaf(identity, metadata);
        }
    }
}

pub(super) fn project_trigger_payload_leaf(
    identity: &mut crate::stable_identity::IdentityEncoder,
    value: &serde_json::Value,
) {
    identity.bytes(&crate::identity_json::payload_leaf(value));
}

pub(super) fn project_trigger_schema_leaf(
    identity: &mut crate::stable_identity::IdentityEncoder,
    value: &serde_json::Value,
) {
    identity.bytes(&crate::identity_json::schema_leaf(value));
}

pub(super) fn reserve_in_memory_for_occurrence(
    state: &mut InMemoryTriggerEventState,
    occurrence: &TriggerOccurrenceRecord,
    clock: &dyn crate::Clock,
) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
    let subscriptions = state
        .subscriptions
        .values()
        .filter(|record| {
            record.routable()
                && record.source_type == occurrence.source_type
                && record.source_key == occurrence.source_key
                && occurrence
                    .session_id
                    .as_ref()
                    .is_none_or(|session_id| record.registrant_session_id() == Some(session_id))
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut reservations = Vec::new();
    for subscription in subscriptions {
        let process_id = deterministic_delivery_process_id(
            &occurrence.occurrence_id,
            &subscription.subscription_id,
            &subscription.incarnation,
            subscription.revision,
        )?;
        let key = (
            occurrence.occurrence_id.clone(),
            subscription.subscription_id.clone(),
        );
        let delivery = InMemoryTriggerDeliveryRecord {
            occurrence_id: occurrence.occurrence_id.clone(),
            subscription_id: subscription.subscription_id.clone(),
            process_id,
            created_at_ms: clock.timestamp_ms(),
            subscription_snapshot: subscription.clone(),
        };
        state.deliveries.insert(key, delivery.clone());
        reservations.push(TriggerDeliveryReservation {
            occurrence: occurrence.clone(),
            subscription,
            process_id: delivery.process_id,
            created_at_ms: delivery.created_at_ms,
            reservation_status: TriggerDeliveryReservationOutcome::Reserved,
        });
    }
    sort_trigger_delivery_reservations(&mut reservations);
    Ok(reservations)
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
    project_trigger_payload_leaf(&mut identity, source);
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

pub fn deterministic_delivery_process_id(
    occurrence_id: &str,
    subscription_id: &str,
    incarnation: &str,
    revision: u64,
) -> Result<ProcessId, PluginError> {
    let preimage =
        trigger_delivery_process_preimage(occurrence_id, subscription_id, incarnation, revision);
    Ok(ProcessId::from(crate::stable_identity::rendered_hash(
        "process:trigger-delivery",
        TRIGGER_DELIVERY_PROCESS_FAMILY_VERSION,
        &preimage,
    )))
}

fn trigger_delivery_process_preimage(
    occurrence_id: &str,
    subscription_id: &str,
    incarnation: &str,
    revision: u64,
) -> Vec<u8> {
    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.trigger-delivery-process",
        TRIGGER_DELIVERY_PROCESS_FAMILY_VERSION,
    );
    identity.string(occurrence_id);
    identity.string(subscription_id);
    identity.string(incarnation);
    identity.u64(revision);
    identity.finish()
}

/// The refusal a recorded emission raises when one of its deliveries did not
/// start. See [`TriggerRouter::emit_recorded`] for why this is an error rather
/// than a `Failed` entry in an otherwise successful report.
fn unstarted_delivery(subscription_id: &str, reason: &str) -> PluginError {
    PluginError::Session(format!(
        "trigger delivery for subscription `{subscription_id}` did not start: {reason}"
    ))
}

#[derive(Clone)]
pub struct TriggerRouter {
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
            route_restorer: None,
        }
    }

    /// Bind the host component that reinstalls a captured provider route before
    /// an unrecorded delivery executes.
    pub fn with_route_restorer(mut self, restorer: Arc<dyn TriggerRouteRestorer>) -> Self {
        self.route_restorer = Some(restorer);
        self
    }

    /// The engine registry this router admits trigger targets against, when the
    /// deployment wired one.
    pub fn process_engines(&self) -> Option<&crate::ProcessEngineRegistry> {
        self.process_engines.as_ref()
    }

    /// Bind the exact artifact stores used by the runtime that will execute
    /// trigger-started processes.
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
    /// [`Self::emit`] reports each delivery's live reservation status, which is
    /// committed outside the effect journal and flips `Reserved` to
    /// `AlreadyReserved` once the first drive has run (FIG-806). A recorded
    /// declaration cannot carry that read: its report becomes the durable,
    /// wire-visible `ToolIntentExecutionOutcome::Executed` result, and on a
    /// runtime-owned host there is no journal to replay it from, so the drain
    /// must recompute the identical value. Every drive starts each reserved
    /// delivery under the same deterministic journal key, so `Started` is the
    /// statement that holds on the first drive and every redrive.
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
    /// effect is what [`crate::ToolContext::triggers`] refuses inside an
    /// atomic tool attempt.
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
        let mut deliveries = Vec::with_capacity(report.deliveries.len());
        for mut delivery in report.deliveries {
            delivery.outcome = match delivery.outcome {
                TriggerDeliveryEmitOutcome::AlreadyReserved => TriggerDeliveryEmitOutcome::Started,
                TriggerDeliveryEmitOutcome::Failed { reason } => {
                    return Err(unstarted_delivery(&delivery.subscription_id, &reason));
                }
                outcome => outcome,
            };
            deliveries.push(delivery);
        }
        Ok((
            TriggerEmitReport::new(report.occurrence_id, deliveries),
            realization,
        ))
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
        } = self.store.ingest_occurrence(request).await?;
        let process_work = &self.process_work;
        let mut deliveries = Vec::new();
        let mut started_any = false;
        for reservation in reservations {
            // FIG-806: reservation status is committed outside the effect
            // journal and changes from Reserved to AlreadyReserved on replay.
            // Emit the deterministic process start before consulting it. The
            // journal and deterministic process id provide the dedupe point;
            // status may shape only the post-emission report.
            if let Err(err) = self
                .start_delivery(
                    &reservation,
                    Arc::clone(process_work.registry()),
                    effect_controller,
                )
                .await
            {
                deliveries.push(reservation.emit_report(TriggerDeliveryEmitOutcome::Failed {
                    reason: err.to_string(),
                }));
                continue;
            }
            started_any = true;
            let outcome = match reservation.reservation_status {
                TriggerDeliveryReservationOutcome::Reserved => TriggerDeliveryEmitOutcome::Started,
                TriggerDeliveryReservationOutcome::AlreadyReserved => {
                    TriggerDeliveryEmitOutcome::AlreadyReserved
                }
            };
            deliveries.push(reservation.emit_report(outcome));
        }
        if started_any {
            let _ = process_work
                .port()
                .admit_pending_processes("trigger_delivery")
                .await?;
        }
        Ok((
            TriggerEmitReport::new(occurrence.occurrence_id, deliveries),
            realization,
        ))
    }

    pub async fn start_delivery(
        &self,
        reservation: &TriggerDeliveryReservation,
        process_registry: Arc<dyn crate::ProcessRegistry>,
        effect_controller: &crate::ScopedEffectController<'_>,
    ) -> Result<(), PluginError> {
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
        self.restore_captured_route(&subscription.source_capture)
            .await?;
        let args =
            materialize_trigger_process_args(&subscription.input_template, &occurrence.payload)?;
        let target = apply_trigger_inputs(subscription.target.clone(), args)?;
        let trigger_causal_ref = crate::CausalRef::TriggerOccurrence {
            occurrence_id: occurrence.occurrence_id.clone(),
            subscription_id: Some(subscription.subscription_id.clone()),
            subscription_incarnation: Some(subscription.incarnation.clone()),
            subscription_revision: Some(subscription.revision),
        };
        let trigger_occurrence_invocation = crate::runtime::causal::trigger_occurrence_invocation(
            subscription
                .registrant_session_id()
                .cloned()
                .map(crate::RuntimeAttribution::for_session)
                .unwrap_or_else(crate::RuntimeAttribution::none),
            &trigger_causal_ref,
        );
        // Engine-admission ruling (FIG-1488): this route deliberately stays
        // outside the gate. A delivery does not carry a caller-supplied engine
        // payload — it replays the subscription's own durable target and the
        // `target_identity` recorded when the subscription was registered, so
        // the admission decision was made once at registration. Re-gating per
        // occurrence would make a delivery fail on catalog drift the
        // subscription already survived, and every delivery for one reservation
        // must stay deterministic.
        let registration = crate::ProcessRegistration::new(
            reservation.process_id.clone(),
            target.clone(),
            // Trigger targets are journaled engine/tool rows, idempotent by
            // process id, so recovery may re-execute them (ADR 0019).
            crate::RecoveryContract::Rerunnable,
            crate::ProcessProvenance::new(subscription.registrant.clone())
                .with_caused_by(Some(trigger_causal_ref.clone())),
            crate::ProcessLifecyclePolicy::new(
                crate::ParentScope::Host,
                crate::OnParentEnd::Abandon,
            ),
        )
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
        let command = crate::ProcessCommand::Start {
            registration,
            observers: subscription
                .registrant_session_id()
                .cloned()
                .into_iter()
                .collect(),
            env_spec: None,
            execution_context: Box::new(execution_context),
        };
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
            subscription
                .registrant_session_id()
                .cloned()
                .map(crate::RuntimeAttribution::for_session)
                .unwrap_or_else(crate::RuntimeAttribution::none),
            effect_id.clone(),
        )
        .with_caused_by(Some(trigger_causal_ref));
        let outcome = effect_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::process(command),
                ),
                {
                    let mut executor = crate::RuntimeEffectLocalExecutor::processes(
                        process_registry,
                        Arc::clone(self.process_work.port()),
                    );
                    if let Some(store) = self.process_env_store.as_ref() {
                        executor = executor.with_process_env_store(Arc::clone(store));
                    }
                    if let Some(engines) = self.process_engines.as_ref() {
                        executor = executor.with_process_engines(engines.clone());
                    }
                    executor
                },
            )
            .await?;
        match outcome {
            crate::RuntimeEffectOutcome::Process {
                result: crate::ProcessEffectOutcome::Start { .. },
            } => Ok(()),
            other => Err(PluginError::Session(format!(
                "trigger process start returned the wrong outcome: {}",
                other.kind().as_str()
            ))),
        }
    }
}

impl TriggerRouter {
    /// Reinstalls the captured provider route for one unrecorded delivery.
    ///
    /// A resident source needs nothing. A provider route with no restorer wired
    /// is left as captured: the host that never installed a restorer has no
    /// revocation policy to consult, and inventing one here would be a fresh
    /// authorization decision. A restorer that answers `Unavailable` leaves the
    /// reservation durable so the next attempt retries the identical delivery
    /// identity; `Revoked` refuses visibly and nothing re-resolves the source.
    async fn restore_captured_route(
        &self,
        capture: &TriggerSourceCapture,
    ) -> Result<(), PluginError> {
        if matches!(capture.route, TriggerProviderRoute::Resident) {
            return Ok(());
        }
        let Some(restorer) = self.route_restorer.as_ref() else {
            return Ok(());
        };
        restorer.restore(capture).await.map_err(PluginError::from)
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
