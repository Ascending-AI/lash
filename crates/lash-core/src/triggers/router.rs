use super::*;

const TRIGGER_DEFINITION_FAMILY_VERSION: u8 = 2;
const TRIGGER_LOOKUP_FAMILY_VERSION: u8 = 2;

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
/// 5 cancelled, 6 abandoned. Retired tags remain burned.
fn trigger_subscription_definition_preimage(
    owner_scope: &TriggerOwnerScope,
    draft: &TriggerSubscriptionDraft,
) -> Vec<u8> {
    let mut fingerprint = crate::stable_identity::IdentityEncoder::new(
        "lash.trigger-subscription-definition",
        TRIGGER_DEFINITION_FAMILY_VERSION,
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
    crate::stable_identity::rendered_hash(
        "trigger-definition",
        TRIGGER_DEFINITION_FAMILY_VERSION,
        &preimage,
    )
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
        crate::ProcessOriginator::Session { session_id } => {
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
    project_trigger_process_input(identity, target);
    let crate::ProcessIdentity {
        kind,
        label,
        definition,
    } = target_identity;
    identity.string(kind);
    identity.optional(label.as_deref(), |identity, label| identity.string(label));
    identity.optional(definition.as_ref(), project_trigger_payload_leaf);
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
                let lash_sansio::llm::types::ProviderReplayMeta { item_id, opaque } = replay;
                identity.optional(item_id.as_deref(), |identity, value| identity.string(value));
                identity.optional(opaque.as_deref(), |identity, value| identity.string(value));
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
            record.enabled
                && !record.tombstoned
                && record.source_type == occurrence.source_type
                && record.source_key == occurrence.source_key
                && occurrence
                    .session_id
                    .as_deref()
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
            reservation_status: TriggerDeliveryReservationStatus::Reserved,
        });
    }
    sort_trigger_delivery_reservations(&mut reservations);
    Ok(reservations)
}

pub(super) fn default_enabled() -> bool {
    true
}

pub fn default_trigger_source_key(
    source_type: &str,
    source: &serde_json::Value,
) -> Result<String, PluginError> {
    let digest = crate::stable_hash::stable_json_sha256_hex(&(source_type, source))
        .map_err(|err| PluginError::Session(format!("failed to hash trigger source key: {err}")))?;
    Ok(format!("source:{source_type}:sha256:{digest}"))
}

pub fn empty_trigger_source_key(source_type: &str) -> Result<String, PluginError> {
    default_trigger_source_key(source_type, &serde_json::json!({}))
}

pub fn deterministic_occurrence_id(
    request: &TriggerOccurrenceRequest,
) -> Result<String, PluginError> {
    let digest = crate::stable_hash::stable_json_sha256_hex(&(
        request.source_type.as_str(),
        request.source_key.as_str(),
        request.idempotency_key.as_str(),
    ))
    .map_err(|err| PluginError::Session(format!("failed to hash trigger occurrence: {err}")))?;
    Ok(format!("trigger:{digest}"))
}

pub fn deterministic_delivery_process_id(
    occurrence_id: &str,
    subscription_id: &str,
    incarnation: &str,
    revision: u64,
) -> Result<String, PluginError> {
    let digest = crate::stable_hash::stable_json_sha256_hex(&(
        "lash.trigger-delivery",
        1_u8,
        occurrence_id,
        subscription_id,
        incarnation,
        revision,
    ))
    .map_err(|err| PluginError::Session(format!("failed to hash trigger delivery: {err}")))?;
    Ok(format!("process:trigger:{digest}"))
}

#[derive(Clone)]
pub struct TriggerRouter {
    store: Arc<dyn TriggerStore>,
    process_registry: Option<Arc<dyn crate::ProcessRegistry>>,
    process_work_driver: Option<crate::ProcessWorkDriver>,
}

impl TriggerRouter {
    pub fn new(
        store: Arc<dyn TriggerStore>,
        process_registry: Option<Arc<dyn crate::ProcessRegistry>>,
        process_work_driver: Option<crate::ProcessWorkDriver>,
    ) -> Self {
        Self {
            store,
            process_registry,
            process_work_driver,
        }
    }

    pub(crate) fn store(&self) -> Arc<dyn TriggerStore> {
        Arc::clone(&self.store)
    }

    pub async fn emit(
        &self,
        request: TriggerOccurrenceRequest,
        effect_controller: &dyn crate::RuntimeEffectController,
    ) -> Result<TriggerEmitReport, PluginError> {
        let TriggerIngressResult {
            occurrence,
            reservations,
        } = self.store.ingest_occurrence(request).await?;
        let Some(process_registry) = self.process_registry.as_ref() else {
            let deliveries = reservations
                .iter()
                .map(|reservation| {
                    let outcome = match reservation.reservation_status {
                        TriggerDeliveryReservationStatus::Reserved => {
                            TriggerDeliveryEmitOutcome::Failed {
                                reason: "trigger delivery requires a process registry".to_string(),
                            }
                        }
                        TriggerDeliveryReservationStatus::AlreadyReserved => {
                            TriggerDeliveryEmitOutcome::AlreadyReserved
                        }
                    };
                    reservation.emit_report(outcome)
                })
                .collect();
            return Ok(TriggerEmitReport::new(occurrence.occurrence_id, deliveries));
        };
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
                    Arc::clone(process_registry),
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
                TriggerDeliveryReservationStatus::Reserved => TriggerDeliveryEmitOutcome::Started,
                TriggerDeliveryReservationStatus::AlreadyReserved => {
                    TriggerDeliveryEmitOutcome::AlreadyReserved
                }
            };
            deliveries.push(reservation.emit_report(outcome));
        }
        if started_any && let Some(driver) = self.process_work_driver.as_ref() {
            driver.claim_and_run_pending("trigger_delivery").await?;
        }
        Ok(TriggerEmitReport::new(occurrence.occurrence_id, deliveries))
    }

    pub(crate) async fn start_delivery(
        &self,
        reservation: &TriggerDeliveryReservation,
        process_registry: Arc<dyn crate::ProcessRegistry>,
        effect_controller: &dyn crate::RuntimeEffectController,
    ) -> Result<(), PluginError> {
        let subscription = &reservation.subscription;
        let occurrence = &reservation.occurrence;
        subscription
            .payload_schema
            .validate(&occurrence.payload)
            .map_err(|err| {
                PluginError::Session(format!(
                    "invalid payload for trigger `{}`: {err}",
                    subscription.subscription_key
                ))
            })?;
        let args =
            materialize_trigger_process_args(&subscription.input_template, &occurrence.payload)?;
        let target = apply_trigger_inputs(subscription.target.clone(), args)?;
        let originator_scope_id = subscription.registrant_scope_id();
        let trigger_causal_ref = crate::CausalRef::TriggerOccurrence {
            occurrence_id: occurrence.occurrence_id.clone(),
            subscription_id: Some(subscription.subscription_id.clone()),
            subscription_incarnation: Some(subscription.incarnation.clone()),
            subscription_revision: Some(subscription.revision),
        };
        let trigger_occurrence_invocation = crate::runtime::causal::trigger_occurrence_invocation(
            &originator_scope_id,
            &occurrence.occurrence_id,
        );
        let registration = crate::ProcessRegistration::new(
            reservation.process_id.clone(),
            target.clone(),
            // Trigger targets are journaled engine/tool rows, idempotent by
            // process id, so recovery may re-execute them (ADR 0019).
            crate::RecoveryDisposition::Rerunnable,
            crate::ProcessProvenance::new(subscription.registrant.clone())
                .with_caused_by(Some(trigger_causal_ref.clone())),
        )
        .with_identity(subscription.target_identity.clone())
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
                .map(str::to_owned)
                .into_iter()
                .collect(),
            execution_context: Box::new(execution_context),
        };
        let effect_id = command.effect_id();
        let invocation = crate::RuntimeInvocation::effect(
            crate::RuntimeScope::new(originator_scope_id),
            effect_id.clone(),
            crate::RuntimeEffectKind::Process,
            format!(
                "trigger:{}:{}:{}:{}",
                occurrence.occurrence_id,
                subscription.subscription_id,
                subscription.incarnation,
                subscription.revision
            ),
        )
        .with_caused_by(Some(trigger_causal_ref));
        let outcome = effect_controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::process(command),
                ),
                crate::RuntimeEffectLocalExecutor::processes(
                    process_registry,
                    self.process_work_driver.clone(),
                ),
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
    Ok(())
}

pub fn trigger_occurrence_request_hash(
    request: &TriggerOccurrenceRequest,
) -> Result<String, PluginError> {
    crate::stable_hash::stable_json_sha256_hex(&(
        request.source_type.as_str(),
        request.source_key.as_str(),
        &request.payload,
        &request.source,
    ))
    .map_err(|err| PluginError::Session(format!("failed to hash trigger occurrence: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn minimal_identity_corpus_draft(input: crate::ProcessInput) -> TriggerSubscriptionDraft {
        TriggerSubscriptionDraft::for_process(
            "sub",
            crate::ProcessExecutionEnvRef::new("env"),
            "source",
            "key",
            input,
            crate::ProcessIdentity::new("kind"),
        )
    }

    fn enriched_identity_corpus_draft(input: crate::ProcessInput) -> TriggerSubscriptionDraft {
        let mut bindings = BTreeMap::new();
        bindings.insert("event".to_string(), TriggerInputBinding::Event);
        bindings.insert(
            "fixed".to_string(),
            TriggerInputBinding::Fixed {
                value: serde_json::json!([null, false, true, -1, 0, u64::MAX, 1.5, "a:b", [], {"x": 0}]),
            },
        );
        let mut selector_fields = BTreeMap::new();
        selector_fields.insert(
            "const".to_string(),
            crate::ProcessValueSelector::Const(serde_json::json!(0)),
        );
        selector_fields.insert("payload".to_string(), crate::ProcessValueSelector::Payload);
        selector_fields.insert(
            "pointer".to_string(),
            crate::ProcessValueSelector::Pointer("/x".to_string()),
        );
        selector_fields.insert(
            "present".to_string(),
            crate::ProcessValueSelector::Present("/y".to_string()),
        );
        let mut draft = minimal_identity_corpus_draft(input)
            .with_source(serde_json::json!({"source": [0, "0"]}))
            .with_payload_schema(crate::LashSchema::new(
                serde_json::json!({"type": "object"}),
            ))
            .with_wake_target(crate::SessionScope::for_agent_frame("session", "frame"))
            .with_event_types([crate::ProcessEventType {
                name: "app.event".to_string(),
                payload_schema: crate::LashSchema::new(serde_json::json!({"type": "object"})),
                semantics: crate::ProcessEventSemanticsSpec {
                    terminal: Some(crate::ProcessTerminalSpec {
                        status: crate::ProcessStatus::Completed,
                        await_output: Some(crate::ProcessValueSelector::Template {
                            template: "{payload}:{pointer}:{const}:{present}".to_string(),
                            fields: selector_fields,
                        }),
                    }),
                    wake: Some(crate::ProcessWakeSpec {
                        when: None,
                        input: crate::ProcessValueSelector::Payload,
                    }),
                },
            }])
            .with_input_template(bindings)
            .with_name("name")
            .with_target_label("label");
        draft.target_identity = crate::ProcessIdentity::new("kind")
            .with_label(Some("label"))
            .with_definition(Some(serde_json::json!({"definition": 0})));
        draft
    }

    #[test]
    fn trigger_definition_identity_golden_corpus() {
        let tool = crate::ProcessInput::ToolCall {
            call: crate::PreparedToolCall::from_parts(
                "call",
                crate::ToolId::new("tool-id"),
                "tool",
                serde_json::json!({"arg": 0}),
                Some(lash_sansio::llm::types::ProviderReplayMeta {
                    item_id: Some("item".to_string()),
                    opaque: None,
                }),
                serde_json::json!({"prepared": true}),
            ),
        };
        let inputs = [
            tool,
            crate::ProcessInput::Engine {
                kind: "engine".to_string(),
                payload: serde_json::json!({"payload": 0}),
            },
            crate::ProcessInput::SessionTurn {
                definition_key: "golden-session-turn:v1".to_string(),
                create_request: Box::new(crate::SessionCreateRequest::root(
                    crate::SessionStartPoint::Empty,
                    crate::PluginOptions::default(),
                )),
                turn_input: Box::new(crate::TurnInput::empty()),
                output_contract: crate::ToolOutputContract::FromInputSchema {
                    input_field: "field".to_string(),
                    default_schema: Some(serde_json::json!({})),
                },
            },
            crate::ProcessInput::External {
                metadata: serde_json::json!({"metadata": 0}),
            },
            crate::ProcessInput::SessionTurn {
                definition_key: "golden-session-turn-static:v1".to_string(),
                create_request: Box::new(crate::SessionCreateRequest::root(
                    crate::SessionStartPoint::Empty,
                    crate::PluginOptions::default(),
                )),
                turn_input: Box::new(crate::TurnInput::empty()),
                output_contract: crate::ToolOutputContract::Static,
            },
        ];
        let owners = [
            TriggerOwnerScope::session("owner"),
            TriggerOwnerScope::host("owner").expect("host owner"),
            TriggerOwnerScope::Platform,
            TriggerOwnerScope::session("owner"),
            TriggerOwnerScope::host("static-owner").expect("host owner"),
        ];
        let actual = owners
            .iter()
            .zip(inputs)
            .enumerate()
            .map(|(index, (owner, input))| {
                let draft = if index == 0 {
                    enriched_identity_corpus_draft(input)
                } else {
                    minimal_identity_corpus_draft(input)
                };
                (
                    hex(&trigger_subscription_definition_preimage(owner, &draft)),
                    trigger_subscription_definition_fingerprint(owner, &draft),
                )
            })
            .collect::<Vec<_>>();
        let expected = [
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e0100000000000000056f776e657200000000000000037375620000000000000003656e7601000000000000000773657373696f6e0100000000000000056672616d650100000000000000046e616d650000000000000006736f7572636500000000000000036b657900000000000000127b22736f75726365223a5b302c2230225d7d00000000000000117b2274797065223a226f626a656374227d01000000000000000463616c6c0000000000000007746f6f6c2d69640000000000000004746f6f6c00000000000000097b22617267223a307d010100000000000000046974656d0000000000000000117b227072657061726564223a747275657d00000000000000046b696e640100000000000000056c6162656c0100000000000000107b22646566696e6974696f6e223a307d000000000000000100000000000000096170702e6576656e7400000000000000117b2274797065223a226f626a656374227d0103010400000000000000257b7061796c6f61647d3a7b706f696e7465727d3a7b636f6e73747d3a7b70726573656e747d00000000000000040000000000000005636f6e73740300000000000000013000000000000000077061796c6f6164010000000000000007706f696e7465720200000000000000022f78000000000000000770726573656e740500000000000000022f79010001000000000000000200000000000000056576656e7401000000000000000566697865640200000000000000405b6e756c6c2c66616c73652c747275652c2d312c302c31383434363734343037333730393535313631352c312e352c22613a62222c5b5d2c7b2278223a307d5d0100000000000000056c6162656c",
                "trigger-definition:v2:sha256:02e4f66d4c1b12f2db41fafa15475f58f5af4956f84b69b1d9ddcb45095b9591",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e0200000000000000056f776e657200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d020000000000000006656e67696e65000000000000000d7b227061796c6f6164223a307d00000000000000046b696e6400000000000000000000000000000000000000",
                "trigger-definition:v2:sha256:1e5f12da808227cecaf6afa3a5ddd915412cb7e0011f2485ccd60a5497655362",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e0300000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d030000000000000016676f6c64656e2d73657373696f6e2d7475726e3a76310200000000000000056669656c640100000000000000027b7d00000000000000046b696e6400000000000000000000000000000000000000",
                "trigger-definition:v2:sha256:2ac7034de852f7fcc09af21ca0ef6b5a40c6ff95abafd278315faba6b08f9707",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e0100000000000000056f776e657200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d04000000000000000e7b226d65746164617461223a307d00000000000000046b696e6400000000000000000000000000000000000000",
                "trigger-definition:v2:sha256:fc3f3a5cf8157dfd624284348b32bfea32ad51e4afcb639e59d48f1abbce70fe",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e02000000000000000c7374617469632d6f776e657200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d03000000000000001d676f6c64656e2d73657373696f6e2d7475726e2d7374617469633a76310100000000000000046b696e6400000000000000000000000000000000000000",
                "trigger-definition:v2:sha256:637563247f23f440b1d9e114f5f2fff307ed72a6b9b06160737791e2319d12ea",
            ),
        ];
        assert_eq!(actual.len(), expected.len());
        for ((preimage, key), (expected_preimage, expected_key)) in actual.iter().zip(expected) {
            assert_eq!(preimage, expected_preimage);
            assert_eq!(key, expected_key);
        }
    }

    #[test]
    fn executable_trigger_definition_changes_rotate_the_fingerprint() {
        let mut first = minimal_identity_corpus_draft(crate::ProcessInput::External {
            metadata: serde_json::json!({"revision": 1}),
        });
        first.event_types = vec![crate::ProcessEventType {
            name: "app.event".to_string(),
            payload_schema: crate::LashSchema::new(serde_json::json!({"type": "string"})),
            semantics: crate::ProcessEventSemanticsSpec::default(),
        }];
        let mut second = first.clone();
        second.event_types[0].payload_schema =
            crate::LashSchema::new(serde_json::json!({"type": "number"}));
        assert_ne!(
            trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &first),
            trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &second)
        );

        let mut annotated = first.clone();
        annotated.event_types[0].payload_schema = crate::LashSchema::new(
            serde_json::json!({"type": "string", "description": "display only"}),
        );
        assert_eq!(
            trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &first),
            trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &annotated),
            "schema annotations are not executable trigger definition"
        );

        let mut ordered = first.clone();
        ordered.event_types.push(crate::ProcessEventType {
            name: "app.another".to_string(),
            payload_schema: crate::LashSchema::any(),
            semantics: crate::ProcessEventSemanticsSpec::default(),
        });
        let mut reversed = ordered.clone();
        reversed.event_types.reverse();
        assert_eq!(
            trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &ordered),
            trigger_subscription_definition_fingerprint(&TriggerOwnerScope::Platform, &reversed),
            "event declaration source order is not executable trigger definition"
        );
    }

    #[test]
    fn trigger_operation_identity_golden_corpus() {
        let owner = TriggerOwnerScope::session("owner");
        let actor = crate::ProcessOriginator::host_scoped("actor");
        let session_actor = crate::ProcessOriginator::session(crate::SessionScope::new("actor"));
        let draft = minimal_identity_corpus_draft(crate::ProcessInput::External {
            metadata: serde_json::json!({"metadata": 0}),
        });
        let commands = [
            TriggerCommand::Register {
                owner_scope: owner.clone(),
                actor: actor.clone(),
                draft: draft.clone(),
            },
            TriggerCommand::List {
                owner_scope: owner.clone(),
                filter: TriggerSubscriptionFilter {
                    registrant_scope_id: Some("r".to_string()),
                    session_id: None,
                    subscription_key: Some("s".to_string()),
                    name: None,
                    source_type: Some("t".to_string()),
                    source_key: None,
                    target: Some(serde_json::json!({"target": 0})),
                    enabled: Some(false),
                },
            },
            TriggerCommand::List {
                owner_scope: TriggerOwnerScope::Platform,
                filter: TriggerSubscriptionFilter {
                    registrant_scope_id: None,
                    session_id: None,
                    subscription_key: None,
                    name: None,
                    source_type: None,
                    source_key: None,
                    target: None,
                    enabled: Some(true),
                },
            },
            TriggerCommand::Update {
                owner_scope: owner.clone(),
                actor: actor.clone(),
                subscription_key: "sub".to_string(),
                draft: draft.clone(),
                expected_revision: 0,
            },
            TriggerCommand::Enable {
                owner_scope: owner.clone(),
                actor: actor.clone(),
                subscription_key: "sub".to_string(),
                expected_revision: 0,
            },
            TriggerCommand::Disable {
                owner_scope: owner.clone(),
                actor: actor.clone(),
                subscription_key: "sub".to_string(),
                expected_revision: 0,
            },
            TriggerCommand::Delete {
                owner_scope: owner.clone(),
                actor: crate::ProcessOriginator::host(),
                subscription_key: "sub".to_string(),
                expected_revision: 0,
            },
            TriggerCommand::Revive {
                owner_scope: owner.clone(),
                actor: actor.clone(),
                subscription_key: "sub".to_string(),
                draft,
                expected_revision: 0,
            },
            TriggerCommand::Prune {
                owner_scope: owner.clone(),
                actor: session_actor,
                subscription_keys: vec!["ab".to_string(), "a".to_string()],
            },
        ];
        let actual = commands
            .iter()
            .map(|command| {
                (
                    hex(&super::super::trigger_command_preimage(command)),
                    super::super::trigger_command_fingerprint(command),
                )
            })
            .collect::<Vec<_>>();
        let expected = [
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000146c6173682e747269676765722d636f6d6d616e64010100000000000000056f776e6572010100000000000000056163746f7200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d04000000000000000e7b226d65746164617461223a307d00000000000000046b696e6400000000000000000000000000000000000000",
                "trigger-command:v2:sha256:530b0bde6ef304964755e34831e103759c9056ea29bf1bfb18ad161679e7bf08",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000146c6173682e747269676765722d636f6d6d616e64020100000000000000056f776e657201000000000000000172000100000000000000017300010000000000000001740001000000000000000c7b22746172676574223a307d0100",
                "trigger-command:v2:sha256:3f6c42e5e1d8f29b256e7eca61b0863ae4d1ff3576f328157963ddebfa969f18",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000146c6173682e747269676765722d636f6d6d616e640203000000000000000101",
                "trigger-command:v2:sha256:9e4d97853cd2011ae58f4d42843dc9b08b9f5461f8ae4b256a4650980046eadb",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000146c6173682e747269676765722d636f6d6d616e64030100000000000000056f776e6572010100000000000000056163746f72000000000000000373756200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d04000000000000000e7b226d65746164617461223a307d00000000000000046b696e64000000000000000000000000000000000000000000000000000000",
                "trigger-command:v2:sha256:9362776dec79c4eec7d18b8e52bc8d2cf09b04bb381d12c1045e24652ff85419",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000146c6173682e747269676765722d636f6d6d616e64040100000000000000056f776e6572010100000000000000056163746f7200000000000000037375620000000000000000",
                "trigger-command:v2:sha256:cd75a81bb26d131341d356d9ae82bcda0a94267396b795293316742d9ed3962f",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000146c6173682e747269676765722d636f6d6d616e64050100000000000000056f776e6572010100000000000000056163746f7200000000000000037375620000000000000000",
                "trigger-command:v2:sha256:eaceddc6103bb67443aeeee06e3486ad41608b8e937fbbec84c50337644bd825",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000146c6173682e747269676765722d636f6d6d616e64060100000000000000056f776e6572010000000000000000037375620000000000000000",
                "trigger-command:v2:sha256:b7b356e7cea58e3deb066a6d14f6d5d72b45d223b94e55c7b9809fa26c326e2f",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000146c6173682e747269676765722d636f6d6d616e64070100000000000000056f776e6572010100000000000000056163746f72000000000000000373756200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b657900000000000000027b7d00000000000000027b7d04000000000000000e7b226d65746164617461223a307d00000000000000046b696e64000000000000000000000000000000000000000000000000000000",
                "trigger-command:v2:sha256:e99cb59f853265e1c40cd76842f02046c4bcbf499eade76b7a32cf80111cf59c",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000146c6173682e747269676765722d636f6d6d616e64080100000000000000056f776e65720200000000000000056163746f72000000000000000200000000000000026162000000000000000161",
                "trigger-command:v2:sha256:246362f8bafcc567a03a089c6742263560a6a25215720386da307b1e9094e496",
            ),
        ];
        assert_eq!(actual.len(), expected.len());
        for ((preimage, key), (expected_preimage, expected_key)) in actual.iter().zip(expected) {
            assert_eq!(preimage, expected_preimage);
            assert_eq!(key, expected_key);
        }

        assert_eq!(
            (
                hex(&trigger_subscription_address_preimage(
                    &TriggerOwnerScope::session("ab"),
                    "c",
                )),
                deterministic_subscription_id(&TriggerOwnerScope::session("ab"), "c"),
            ),
            ("6c6173682d737461626c652d6964656e74697479010200000000000000216c6173682e747269676765722d737562736372697074696f6e2d616464726573730100000000000000026162000000000000000163".to_string(), "trigger-subscription:v2:sha256:0e952384463c79d6322f29034298ccb07742ffbbdc7d3dd7c5a48f25bcb55dd2".to_string())
        );
        assert_eq!(
            deterministic_subscription_id(&TriggerOwnerScope::session("a"), "bc"),
            "trigger-subscription:v2:sha256:eb31d127dbf96c2bf1c00488182547b0a4fdeb6b7ce37dc7defdfcd1461a96a6"
        );
        assert_eq!(
            (
                hex(&super::super::trigger_operation_receipt_preimage(
                    &TriggerOwnerScope::Platform,
                    "op:0",
                )),
                super::super::trigger_operation_receipt_id(&TriggerOwnerScope::Platform, "op:0"),
            ),
            ("6c6173682d737461626c652d6964656e746974790102000000000000001e6c6173682e747269676765722d6f7065726174696f6e2d616464726573730300000000000000046f703a30".to_string(), "trigger-operation:v2:sha256:eab1201cfa848a57178dbe7bd3438ffc51cf22980b4884ea2cf0e7a30ddb79fc".to_string())
        );
    }

    fn button_payload_schema() -> crate::LashSchema {
        crate::LashSchema::any()
    }

    fn trigger_process_draft(source_key: &str, process_name: &str) -> TriggerSubscriptionDraft {
        TriggerSubscriptionDraft::for_process(
            format!("test/{process_name}"),
            crate::ProcessExecutionEnvRef::new(format!("process-env:{process_name}")),
            "ui.button.pressed",
            source_key,
            crate::ProcessInput::Engine {
                kind: "test-engine".to_string(),
                payload: serde_json::json!({ "process": process_name }),
            },
            crate::ProcessIdentity::new("test-engine").with_label(Some(process_name)),
        )
        .with_payload_schema(crate::LashSchema::any())
    }

    async fn register(
        store: &InMemoryTriggerStore,
        operation_id: &str,
        draft: TriggerSubscriptionDraft,
    ) -> TriggerSubscriptionRecord {
        let outcome = store
            .execute_command(
                operation_id,
                TriggerCommand::Register {
                    owner_scope: TriggerOwnerScope::host("test").unwrap(),
                    actor: crate::ProcessOriginator::host_scoped("test"),
                    draft,
                },
            )
            .await
            .expect("execute registration")
            .expect("register subscription");
        let TriggerCommandOutcome::Mutation { receipt } = outcome else {
            panic!("expected mutation receipt")
        };
        receipt.record_snapshot
    }

    async fn register_for_session(
        store: &InMemoryTriggerStore,
        operation_id: &str,
        session_id: &str,
        draft: TriggerSubscriptionDraft,
    ) -> TriggerSubscriptionRecord {
        let outcome = store
            .execute_command(
                operation_id,
                TriggerCommand::Register {
                    owner_scope: TriggerOwnerScope::session(session_id),
                    actor: crate::ProcessOriginator::session(crate::SessionScope::new(session_id)),
                    draft,
                },
            )
            .await
            .expect("execute session registration")
            .expect("register session subscription");
        let TriggerCommandOutcome::Mutation { receipt } = outcome else {
            panic!("expected mutation receipt")
        };
        receipt.record_snapshot
    }

    fn button_occurrence(
        source_key: impl Into<String>,
        idempotency_key: impl Into<String>,
    ) -> TriggerOccurrenceRequest {
        TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            source_key,
            serde_json::json!({ "button": "Blue" }),
            idempotency_key,
        )
    }

    #[test]
    fn trigger_catalog_rejects_duplicate_trigger_source_identity() {
        let mut catalog = TriggerEventCatalog::new();
        catalog
            .declare(TriggerEvent::new(
                "Button",
                "ui.button",
                "pressed",
                button_payload_schema(),
            ))
            .expect("first trigger occurrence");

        let err = catalog
            .declare(TriggerEvent::new(
                "AlternateButton",
                "ui.button",
                "pressed",
                button_payload_schema(),
            ))
            .expect_err("duplicate public source identity should be rejected");

        assert!(err.contains("duplicate trigger source `ui.button.pressed`"));
    }

    #[tokio::test]
    async fn trigger_store_rejects_mismatched_target_label() {
        let store = InMemoryTriggerStore::default();
        let draft = TriggerSubscriptionDraft::for_process(
            "mismatched-label",
            crate::ProcessExecutionEnvRef::new("process-env:test"),
            "ui.button.pressed",
            "source-key",
            crate::ProcessInput::External {
                metadata: serde_json::json!({}),
            },
            crate::ProcessIdentity::new("external").with_label(Some("expected")),
        )
        .with_target_label("other");

        let err = store
            .execute_command(
                "mismatched-label",
                TriggerCommand::Register {
                    owner_scope: TriggerOwnerScope::host("test").unwrap(),
                    actor: crate::ProcessOriginator::host_scoped("test"),
                    draft,
                },
            )
            .await
            .expect("store execution")
            .expect_err("mismatched target labels should be rejected");
        assert!(err.to_string().contains("target_label must match"));
    }

    #[tokio::test]
    async fn trigger_emit_report_records_started_and_already_reserved_deliveries() {
        let store = Arc::new(InMemoryTriggerStore::default());
        let registry: Arc<dyn crate::ProcessRegistry> =
            Arc::new(crate::TestLocalProcessRegistry::default());
        let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
        let subscription = register(
            store.as_ref(),
            "started-register",
            trigger_process_draft(&source_key, "started"),
        )
        .await;
        let router = TriggerRouter::new(store, Some(Arc::clone(&registry)), None);
        let controller = crate::InlineRuntimeEffectController::default();

        let report = router
            .emit(
                button_occurrence(source_key.clone(), "button-blue-report"),
                &controller,
            )
            .await
            .expect("emit trigger");
        assert_eq!(report.deliveries.len(), 1);
        let delivery = &report.deliveries[0];
        assert_eq!(delivery.occurrence_id, report.occurrence_id);
        assert_eq!(delivery.subscription_id, subscription.subscription_id);
        assert_eq!(delivery.outcome, TriggerDeliveryEmitOutcome::Started);
        let record = registry
            .get_process(&delivery.process_id)
            .await
            .expect("read process")
            .expect("started process record");
        assert!(matches!(
            record.provenance.caused_by,
            Some(crate::CausalRef::TriggerOccurrence {
                occurrence_id,
                subscription_id: Some(subscription_id),
                ..
            }) if occurrence_id == report.occurrence_id
                && subscription_id == subscription.subscription_id
        ));

        let replay = router
            .emit(
                button_occurrence(source_key, "button-blue-report"),
                &controller,
            )
            .await
            .expect("replay trigger");
        assert_eq!(replay.deliveries.len(), 1);
        assert_eq!(
            replay.deliveries[0].outcome,
            TriggerDeliveryEmitOutcome::AlreadyReserved
        );
        assert_eq!(replay.deliveries[0].process_id, delivery.process_id);
    }

    #[tokio::test]
    async fn session_trigger_process_is_observed_by_its_registrant() {
        let store = Arc::new(InMemoryTriggerStore::default());
        let registry = Arc::new(crate::TestLocalProcessRegistry::default());
        let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
        register_for_session(
            store.as_ref(),
            "session-register",
            "session-owner",
            trigger_process_draft(&source_key, "session-owned"),
        )
        .await;
        let router = TriggerRouter::new(
            store,
            Some(Arc::clone(&registry) as Arc<dyn crate::ProcessRegistry>),
            None,
        );

        let report = router
            .emit(
                button_occurrence(source_key, "session-button-blue"),
                &crate::InlineRuntimeEffectController::default(),
            )
            .await
            .expect("emit session trigger");
        let process_id = &report.deliveries[0].process_id;
        assert!(
            crate::ProcessRegistry::is_observer(registry.as_ref(), "session-owner", process_id)
                .await
                .expect("read initial observer"),
            "the session that explicitly registered the trigger must observe its process"
        );
    }

    #[tokio::test]
    async fn trigger_emit_report_records_failed_delivery_outcome() {
        let store = Arc::new(InMemoryTriggerStore::default());
        let source_key = empty_trigger_source_key("ui.button.pressed").expect("source key");
        let subscription = register(
            store.as_ref(),
            "failed-register",
            trigger_process_draft(&source_key, "failed"),
        )
        .await;
        let router = TriggerRouter::new(store, None, None);
        let controller = crate::InlineRuntimeEffectController::default();

        let report = router
            .emit(
                button_occurrence(source_key, "button-blue-failed"),
                &controller,
            )
            .await
            .expect("emit trigger");
        assert_eq!(report.deliveries.len(), 1);
        let delivery = &report.deliveries[0];
        assert_eq!(delivery.subscription_id, subscription.subscription_id);
        assert!(matches!(
            &delivery.outcome,
            TriggerDeliveryEmitOutcome::Failed { reason }
                if reason.contains("process registry")
        ));
    }
}
