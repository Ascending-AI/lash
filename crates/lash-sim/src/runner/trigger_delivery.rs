use super::*;

/// Trigger boundaries whose external processes the scheduler owns. A
/// successful boundary registers and binds its process in the world's stores;
/// the bind delivers the reservation's obligation (ADR 0109).
pub(super) struct SimTriggerHarness {
    store: Arc<dyn lash_core::TriggerStore>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    registered_source_keys: BTreeSet<String>,
}

impl SimTriggerHarness {
    pub(super) fn over(stores: Arc<dyn lash_core::StoreSet>) -> Self {
        Self {
            store: stores.trigger_store(),
            registry: stores.process_registry(),
            registered_source_keys: BTreeSet::new(),
        }
    }
}

impl SimTriggerHarness {
    pub(super) async fn deliver(
        &mut self,
        event: &BoundaryEvent,
    ) -> Result<Value, FixedScriptRunnerError> {
        let session = event
            .payload
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or(&event.actor_alias)
            .to_string();
        let source_key = event
            .payload
            .get("source_key")
            .and_then(Value::as_str)
            .unwrap_or(&event.boundary_id)
            .to_string();
        let source_type = "sim.trigger";
        if self.registered_source_keys.insert(source_key.clone()) {
            let draft = lash_core::TriggerSubscriptionDraft::for_process(
                format!("sim/{}", event.boundary_id),
                lash_core::ProcessExecutionEnvRef::new("process-env:sim-trigger"),
                source_type,
                source_key.clone(),
                lash_core::ProcessInput::External {
                    metadata: json!({
                        "trigger_boundary": event.boundary_id,
                    }),
                },
                lash_core::ProcessIdentity::labelled("sim-trigger", Some("sim trigger")),
            )
            .with_wake_target(lash_core::SessionScope::new(session.clone()));
            self.store
                .execute_command(
                    &format!("sim-trigger-register:{}", event.boundary_id),
                    lash_core::TriggerCommand::Register {
                        owner_scope: lash_core::TriggerOwnerScope::session(session.clone()),
                        actor: lash_core::ProcessOriginator::session(lash_core::SessionScope::new(
                            session.clone(),
                        )),
                        draft,
                    },
                )
                .await
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        }
        let ingress = self
            .store
            .ingest_occurrence(
                lash_core::TriggerOccurrenceRequest::new(
                    source_type,
                    source_key.clone(),
                    json!({
                        "boundary_id": event.boundary_id,
                        "session": session,
                    }),
                    format!("sim-trigger:{}", event.boundary_id),
                )
                .with_source(json!({"sim": true})),
            )
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        for reservation in &ingress.reservations {
            let subscription = &reservation.subscription;
            // These are externally owned simulated processes, so their
            // registration captures no engine environment and owes no
            // ProcessStart. Replaying the boundary finds the same start key,
            // including when registration landed but the bind did not.
            let registration = lash_core::ProcessRegistration::new(
                subscription.target.clone(),
                lash_core::ProcessProvenance::new(subscription.registrant.clone()).with_caused_by(
                    Some(lash_core::CausalRef::TriggerOccurrence {
                        occurrence_id: reservation.occurrence.occurrence_id.clone(),
                        subscription_id: Some(subscription.subscription_id.clone()),
                        subscription_incarnation: Some(subscription.incarnation.clone()),
                        subscription_revision: Some(subscription.revision),
                    }),
                ),
                lash_core::Lifetime::Detached,
            )
            .with_start_key(Some(lash_core::facade_support::trigger_delivery_start_key(
                reservation,
            )))
            .with_admitted_identity(lash_core::AdmittedProcessIdentity::pinned(
                subscription.target_identity.clone(),
            ))
            .with_extra_event_types(subscription.event_types.clone())
            .with_wake_session_id(
                subscription
                    .wake_target
                    .as_ref()
                    .map(|scope| scope.session_id.clone()),
            );
            let observers: Vec<_> = subscription
                .registrant_session_id()
                .cloned()
                .into_iter()
                .collect();
            let process = self
                .registry
                .register_process_reporting_disposition(registration, &observers)
                .await
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
            self.store
                .bind_delivery_process(
                    &reservation.occurrence.occurrence_id,
                    &subscription.subscription_id,
                    &process.record.id,
                )
                .await
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        }
        Ok(json!({
            "session": session,
            "trigger_delivered": true,
            "source_key": source_key,
            "occurrence_id": ingress.occurrence.occurrence_id,
            "reservation_count": ingress.reservations.len(),
            "started_process": !ingress.reservations.is_empty(),
        }))
    }
}

#[cfg(test)]
mod tests;
