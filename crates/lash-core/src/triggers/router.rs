use super::*;
#[cfg(test)]
use crate::facade_support::ProcessOriginatorFacadeOps;

pub fn deterministic_subscription_id(
    owner_scope: &TriggerOwnerScope,
    subscription_key: &str,
) -> Result<String, PluginError> {
    let digest = crate::stable_hash::stable_json_sha256_hex(&(
        "lash.trigger-subscription",
        1_u8,
        owner_scope,
        subscription_key,
    ))
    .map_err(|err| PluginError::Session(format!("failed to hash trigger identity: {err}")))?;
    Ok(format!("trigger-subscription:v1:sha256:{digest}"))
}

pub fn trigger_subscription_definition_hash(
    owner_scope: &TriggerOwnerScope,
    draft: &TriggerSubscriptionDraft,
) -> Result<String, PluginError> {
    crate::stable_hash::stable_json_sha256_hex(&(
        "lash.trigger-subscription-definition",
        1_u8,
        owner_scope,
        draft,
    ))
    .map_err(|err| PluginError::Session(format!("failed to hash trigger definition: {err}")))
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
