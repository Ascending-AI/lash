use super::*;

const TRIGGER_DEFINITION_FAMILY_VERSION: u8 = 2;

pub(super) fn append_owner_address(output: &mut String, owner_scope: &TriggerOwnerScope) {
    match owner_scope {
        TriggerOwnerScope::Session { session_id } => {
            output.push_str("1:");
            output.push_str(&session_id.len().to_string());
            output.push(':');
            output.push_str(session_id);
        }
        TriggerOwnerScope::Host { binding_id } => {
            output.push_str("2:");
            output.push_str(&binding_id.len().to_string());
            output.push(':');
            output.push_str(binding_id);
        }
        TriggerOwnerScope::Platform => output.push_str("3:0:"),
    }
}

pub fn deterministic_subscription_id(
    owner_scope: &TriggerOwnerScope,
    subscription_key: &str,
) -> Result<String, PluginError> {
    // Version 2 is a stored, unhashed lookup address. Lash's trigger schemas
    // reject version-1 stores; external stores must recreate before cutover.
    let mut address = String::from("trigger-subscription:v2:");
    append_owner_address(&mut address, owner_scope);
    address.push(':');
    address.push_str(&subscription_key.len().to_string());
    address.push(':');
    address.push_str(subscription_key);
    Ok(address)
}

/// Permanent trigger-definition tag registry.
///
/// Owners: 1 session, 2 host, 3 platform. Process inputs: 1 tool call,
/// 2 engine, 3 session turn, 4 external. Input bindings: 1 event, 2 fixed.
/// JSON: 1 null, 2 false, 3 true, 4 i64, 5 u64, 6 f64, 7 string, 8 array,
/// 9 object. Retired tags remain burned.
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
    project_trigger_json_value(identity, source);
    project_trigger_json_value(identity, &payload_schema.schema);
    project_trigger_process_input(identity, target);
    let crate::ProcessIdentity {
        kind,
        label,
        definition,
    } = target_identity;
    identity.string(kind);
    identity.optional(label.as_deref(), |identity, label| identity.string(label));
    identity.optional(definition.as_ref(), project_trigger_json_value);
    identity.sequence(
        event_types.iter(),
        event_types.len(),
        |identity, event_type| {
            let crate::ProcessEventType {
                name,
                payload_schema: _,
                semantics: _,
            } = event_type;
            identity.string(name);
        },
    );
    identity.sequence(
        input_template.iter(),
        input_template.len(),
        |identity, (name, binding)| {
            identity.string(name);
            match binding {
                TriggerInputBinding::Event => identity.tag(1),
                TriggerInputBinding::Fixed { value } => {
                    identity.tag(2);
                    project_trigger_json_value(identity, value);
                }
            }
        },
    );
    identity.optional(target_label.as_deref(), |identity, label| {
        identity.string(label)
    });
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
            project_trigger_json_value(identity, args);
            identity.optional(replay.as_ref(), |identity, replay| {
                let lash_sansio::llm::types::ProviderReplayMeta { item_id, opaque } = replay;
                identity.optional(item_id.as_deref(), |identity, value| identity.string(value));
                identity.optional(opaque.as_deref(), |identity, value| identity.string(value));
            });
            project_trigger_json_value(identity, prepared_payload);
        }
        crate::ProcessInput::Engine { kind, payload } => {
            identity.tag(2);
            identity.string(kind);
            project_trigger_json_value(identity, payload);
        }
        crate::ProcessInput::SessionTurn {
            create_request: _,
            turn_input: _,
            output_contract,
        } => {
            identity.tag(3);
            match output_contract {
                crate::ToolOutputContract::Static => identity.tag(1),
                crate::ToolOutputContract::FromInputSchema {
                    input_field,
                    default_schema,
                } => {
                    identity.tag(2);
                    identity.string(input_field);
                    identity.optional(default_schema.as_ref(), project_trigger_json_value);
                }
            }
        }
        crate::ProcessInput::External { metadata } => {
            identity.tag(4);
            project_trigger_json_value(identity, metadata);
        }
    }
}

pub(super) fn project_trigger_json_value(
    identity: &mut crate::stable_identity::IdentityEncoder,
    value: &serde_json::Value,
) {
    match value {
        serde_json::Value::Null => identity.tag(1),
        serde_json::Value::Bool(false) => identity.tag(2),
        serde_json::Value::Bool(true) => identity.tag(3),
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                identity.tag(4);
                identity.u64(value as u64);
            } else if let Some(value) = number.as_u64() {
                identity.tag(5);
                identity.u64(value);
            } else {
                identity.tag(6);
                identity.u64(
                    number
                        .as_f64()
                        .expect("serde_json numbers are i64, u64, or finite f64")
                        .to_bits(),
                );
            }
        }
        serde_json::Value::String(value) => {
            identity.tag(7);
            identity.string(value);
        }
        serde_json::Value::Array(values) => {
            identity.tag(8);
            identity.sequence(values, values.len(), project_trigger_json_value);
        }
        serde_json::Value::Object(values) => {
            identity.tag(9);
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            identity.sequence(entries, values.len(), |identity, (key, value)| {
                identity.string(key);
                project_trigger_json_value(identity, value);
            });
        }
    }
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
        let mut draft = minimal_identity_corpus_draft(input)
            .with_source(serde_json::json!({"source": [0, "0"]}))
            .with_payload_schema(crate::LashSchema::new(
                serde_json::json!({"type": "object"}),
            ))
            .with_wake_target(crate::SessionScope::for_agent_frame("session", "frame"))
            .with_event_types([crate::ProcessEventType {
                name: "app.event".to_string(),
                payload_schema: crate::LashSchema::any(),
                semantics: crate::ProcessEventSemanticsSpec::default(),
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
        ];
        let owners = [
            TriggerOwnerScope::session("owner"),
            TriggerOwnerScope::host("owner").expect("host owner"),
            TriggerOwnerScope::Platform,
            TriggerOwnerScope::session("owner"),
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
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e0100000000000000056f776e657200000000000000037375620000000000000003656e7601000000000000000773657373696f6e0100000000000000056672616d650100000000000000046e616d650000000000000006736f7572636500000000000000036b65790900000000000000010000000000000006736f75726365080000000000000002040000000000000000070000000000000001300900000000000000010000000000000004747970650700000000000000066f626a65637401000000000000000463616c6c0000000000000007746f6f6c2d69640000000000000004746f6f6c0900000000000000010000000000000003617267040000000000000000010100000000000000046974656d00090000000000000001000000000000000870726570617265640300000000000000046b696e640100000000000000056c6162656c01090000000000000001000000000000000a646566696e6974696f6e040000000000000000000000000000000100000000000000096170702e6576656e74000000000000000200000000000000056576656e7401000000000000000566697865640208000000000000000a01020304ffffffffffffffff04000000000000000005ffffffffffffffff063ff8000000000000070000000000000003613a620800000000000000000900000000000000010000000000000001780400000000000000000100000000000000056c6162656c",
                "trigger-definition:v2:sha256:024ed49289a7259516ea4f5ed83ba59f36b9ac71c310d348e59c2756f2725864",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e0200000000000000056f776e657200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b6579090000000000000000090000000000000000020000000000000006656e67696e6509000000000000000100000000000000077061796c6f616404000000000000000000000000000000046b696e6400000000000000000000000000000000000000",
                "trigger-definition:v2:sha256:0898034809dab747c286c75bbdf9f894dc73a364163e842585c6e5966209b0b9",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e0300000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b6579090000000000000000090000000000000000030200000000000000056669656c640109000000000000000000000000000000046b696e6400000000000000000000000000000000000000",
                "trigger-definition:v2:sha256:cf9432f2a2c69f1e97177b83d28015031528436caf2677d4a4bc248e1bd3fee6",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000246c6173682e747269676765722d737562736372697074696f6e2d646566696e6974696f6e0100000000000000056f776e657200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b65790900000000000000000900000000000000000409000000000000000100000000000000086d6574616461746104000000000000000000000000000000046b696e6400000000000000000000000000000000000000",
                "trigger-definition:v2:sha256:32bec49772d95340b2704dafde69ba79463ddcc7efbdb67cec8284943b92c03c",
            ),
        ];
        assert_eq!(actual.len(), expected.len());
        for ((preimage, key), (expected_preimage, expected_key)) in actual.iter().zip(expected) {
            assert_eq!(preimage, expected_preimage);
            assert_eq!(key, expected_key);
        }
    }

    #[test]
    fn trigger_operation_identity_golden_corpus() {
        let owner = TriggerOwnerScope::session("owner");
        let actor = crate::ProcessOriginator::host_scoped("actor");
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
                actor: actor.clone(),
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
                actor,
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
                "6c6173682d737461626c652d6964656e74697479010200000000000000146c6173682e747269676765722d636f6d6d616e64010100000000000000056f776e6572010100000000000000056163746f7200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b65790900000000000000000900000000000000000409000000000000000100000000000000086d6574616461746104000000000000000000000000000000046b696e6400000000000000000000000000000000000000",
                "trigger-command:v2:sha256:7fcc5830c2dadefbf47847eec34276fa7dfe7c07f41314d806e3ecdd793e31e4",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000146c6173682e747269676765722d636f6d6d616e64020100000000000000056f776e65720100000000000000017200010000000000000001730001000000000000000174000109000000000000000100000000000000067461726765740400000000000000000100",
                "trigger-command:v2:sha256:520b5a79a196529017bba0701679f4fbd282defce94093d49919d6a40d6a02fc",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000146c6173682e747269676765722d636f6d6d616e64030100000000000000056f776e6572010100000000000000056163746f72000000000000000373756200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b65790900000000000000000900000000000000000409000000000000000100000000000000086d6574616461746104000000000000000000000000000000046b696e64000000000000000000000000000000000000000000000000000000",
                "trigger-command:v2:sha256:38198669a782c5a838c5f54d1f612d60dce38898ecab7577cd4b86427f8631f6",
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
                "6c6173682d737461626c652d6964656e74697479010200000000000000146c6173682e747269676765722d636f6d6d616e64060100000000000000056f776e6572010100000000000000056163746f7200000000000000037375620000000000000000",
                "trigger-command:v2:sha256:f766211db04eeb3f1362683531c1b4e608a0f7c319c5a3a4d0944c1ac9ac6fdc",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000146c6173682e747269676765722d636f6d6d616e64070100000000000000056f776e6572010100000000000000056163746f72000000000000000373756200000000000000037375620000000000000003656e7600000000000000000006736f7572636500000000000000036b65790900000000000000000900000000000000000409000000000000000100000000000000086d6574616461746104000000000000000000000000000000046b696e64000000000000000000000000000000000000000000000000000000",
                "trigger-command:v2:sha256:f7aa7c0b8c7fd0d2f4bd688aa77ce8b59805213b20c7915d16968d9f2be6a62a",
            ),
            (
                "6c6173682d737461626c652d6964656e74697479010200000000000000146c6173682e747269676765722d636f6d6d616e64080100000000000000056f776e6572010100000000000000056163746f72000000000000000200000000000000026162000000000000000161",
                "trigger-command:v2:sha256:781f147a927c2a8aaeec2f487fff7a4bf017983efef488f491a587783579cde7",
            ),
        ];
        assert_eq!(actual.len(), expected.len());
        for ((preimage, key), (expected_preimage, expected_key)) in actual.iter().zip(expected) {
            assert_eq!(preimage, expected_preimage);
            assert_eq!(key, expected_key);
        }

        assert_eq!(
            deterministic_subscription_id(&TriggerOwnerScope::session("ab"), "c")
                .expect("subscription address"),
            "trigger-subscription:v2:1:2:ab:1:c"
        );
        assert_eq!(
            deterministic_subscription_id(&TriggerOwnerScope::session("a"), "bc")
                .expect("subscription address"),
            "trigger-subscription:v2:1:1:a:2:bc"
        );
        assert_eq!(
            super::super::trigger_operation_receipt_id(&TriggerOwnerScope::Platform, "op:0")
                .expect("operation address"),
            "trigger-operation:v2:3:0::4:op:0"
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
