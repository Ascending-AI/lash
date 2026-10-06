#[cfg(test)]
use super::*;

#[cfg(test)]
impl SimTriggerHarness {
    pub(super) fn over(stores: Arc<dyn lash_core::StoreSet>) -> Self {
        Self {
            store: stores.trigger_store(),
            registry: stores.process_registry(),
            starts: stores.obligation_ledger(lash_core::store::ObligationKind::ProcessStart),
            clock: stores.clock(),
            registered_source_keys: BTreeSet::new(),
        }
    }

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
                lash_core::ProcessInput::Engine {
                    kind: "sim-trigger".to_string(),
                    payload: json!({
                        "trigger_boundary": event.boundary_id,
                    }),
                },
                lash_core::ProcessIdentity::labelled("sim-trigger", Some("sim trigger")),
            )
            .with_wake_target(lash_core::SessionScope::new(
                lash_core::SessionId::fixture(session.clone()),
            ));
            self.store
                .execute_command(
                    &format!("sim-trigger-register:{}", event.boundary_id),
                    lash_core::TriggerCommand::Register {
                        owner_scope: lash_core::TriggerOwnerScope::session(
                            lash_core::SessionId::fixture(session.clone()),
                        ),
                        actor: lash_core::ProcessOriginator::session(lash_core::SessionScope::new(
                            lash_core::SessionId::fixture(session.clone()),
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
            // The subscription's own engine target, under its captured
            // environment: the process owes its engine start, which this
            // boundary delivers once the process is bound.
            // Replaying the boundary finds the same start key, including when
            // registration landed but the bind did not.
            let input = match &subscription.target {
                lash_core::ProcessStartTarget::Input(
                    input @ lash_core::ProcessInput::Engine { .. },
                ) => input.clone(),
                target => {
                    return Err(FixedScriptRunnerError::Runtime(format!(
                        "simulation trigger requires an Engine target, received {target:?}"
                    )));
                }
            };
            let registration = lash_core::ProcessRegistration::new(
                input,
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
            .with_execution_env_ref(Some(subscription.env_ref.clone()))
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
                .register_process_reporting_outcome(registration, &observers)
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
            self.deliver_start(&process.record.id).await?;
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

    /// Claims and delivers the process's `ProcessStart`. A replay finds it
    /// already delivered and leaves it.
    async fn deliver_start(
        &self,
        process_id: &lash_core::ProcessId,
    ) -> Result<(), FixedScriptRunnerError> {
        let id = lash_core::store::ObligationKey::ProcessStart {
            process_id: process_id.clone(),
        }
        .id();
        let token = lash_core::store::ClaimToken::mint();
        let now_ms = self.clock.timestamp_ms();
        let claimed = self
            .starts
            .claim(&id, &token, now_ms, 60_000)
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        if claimed.is_some() {
            self.starts
                .settle(
                    &id,
                    &token,
                    lash_core::store::ObligationSettlement::Delivered,
                    now_ms,
                )
                .await
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
impl SimTriggerHarness {}

#[cfg(test)]
mod tests;
/// Trigger boundaries that register their subscription's engine process. A
/// successful boundary registers and binds its process in the world's stores;
/// the bind delivers the reservation's obligation (ADR 0109). The scripted
/// world has no engine relay, so the boundary also delivers the process's
/// start, standing in for the deployment that runs the trigger's engine.
#[cfg(test)]
pub(super) struct SimTriggerHarness {
    store: Arc<dyn lash_core::TriggerStore>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    starts: Arc<dyn lash_core::store::ObligationLedger>,
    clock: Arc<dyn lash_core::Clock>,
    registered_source_keys: BTreeSet<String>,
}
