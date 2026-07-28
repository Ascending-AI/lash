use super::*;

pub struct InMemoryTriggerStore {
    clock: Arc<dyn crate::Clock>,
    state: Mutex<InMemoryTriggerEventState>,
}

impl InMemoryTriggerStore {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(crate::SystemClock))
    }

    pub fn with_clock(clock: Arc<dyn crate::Clock>) -> Self {
        Self {
            clock,
            state: Mutex::new(InMemoryTriggerEventState::default()),
        }
    }

    fn list_deliveries_matching(
        &self,
        matches: impl Fn(&InMemoryTriggerDeliveryRecord) -> bool,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        let state = self
            .state
            .lock()
            .map_err(|_| PluginError::Session("trigger store lock poisoned".to_string()))?;
        let mut deliveries = state
            .deliveries
            .values()
            .filter(|delivery| matches(delivery))
            .map(|delivery| {
                in_memory_delivery_reservation(
                    &state,
                    delivery,
                    TriggerDeliveryReservationStatus::AlreadyReserved,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        deliveries.sort_by(|left, right| {
            left.created_at_ms
                .cmp(&right.created_at_ms)
                .then_with(|| {
                    left.occurrence
                        .occurrence_id
                        .cmp(&right.occurrence.occurrence_id)
                })
                .then_with(|| {
                    left.subscription
                        .subscription_id
                        .cmp(&right.subscription.subscription_id)
                })
        });
        Ok(deliveries)
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn delete_deliveries_by_process_ids(
        &self,
        process_ids: &std::collections::HashSet<String>,
    ) -> Result<usize, PluginError> {
        if process_ids.is_empty() {
            return Ok(0);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| PluginError::Session("trigger store lock poisoned".to_string()))?;
        let before = state.deliveries.len();
        state
            .deliveries
            .retain(|_, delivery| !process_ids.contains(&delivery.process_id));
        Ok(before.saturating_sub(state.deliveries.len()))
    }
}

impl Default for InMemoryTriggerStore {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
pub(super) struct InMemoryTriggerEventState {
    pub(super) subscriptions: BTreeMap<String, TriggerSubscriptionRecord>,
    pub(super) mutation_receipts: BTreeMap<String, (String, TriggerEffectResult, u64)>,
    pub(super) occurrences: BTreeMap<String, TriggerOccurrenceRecord>,
    pub(super) occurrence_id_by_idempotency_key: BTreeMap<String, String>,
    pub(super) occurrence_hashes: BTreeMap<String, String>,
    pub(super) deliveries: BTreeMap<(String, String), InMemoryTriggerDeliveryRecord>,
}

#[derive(Clone)]
pub(super) struct InMemoryTriggerDeliveryRecord {
    pub(super) occurrence_id: String,
    pub(super) subscription_id: String,
    pub(super) process_id: String,
    pub(super) created_at_ms: u64,
    pub(super) subscription_snapshot: TriggerSubscriptionRecord,
}

fn in_memory_delivery_reservation(
    state: &InMemoryTriggerEventState,
    delivery: &InMemoryTriggerDeliveryRecord,
    reservation_status: TriggerDeliveryReservationStatus,
) -> Result<TriggerDeliveryReservation, PluginError> {
    let occurrence = state
        .occurrences
        .get(&delivery.occurrence_id)
        .cloned()
        .ok_or_else(|| {
            PluginError::Session(format!(
                "missing trigger occurrence `{}` for delivery",
                delivery.occurrence_id
            ))
        })?;
    let subscription = delivery.subscription_snapshot.clone();
    Ok(TriggerDeliveryReservation {
        occurrence,
        subscription,
        process_id: delivery.process_id.clone(),
        created_at_ms: delivery.created_at_ms,
        reservation_status,
    })
}

#[async_trait::async_trait]
impl TriggerStore for InMemoryTriggerStore {
    async fn execute_command(
        &self,
        operation_id: &str,
        command: TriggerCommand,
    ) -> Result<TriggerEffectResult, PluginError> {
        if operation_id.trim().is_empty() {
            return Ok(Err(TriggerOperationError::Invalid {
                message: "trigger operation id must be non-empty".to_string(),
            }));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| PluginError::Session("trigger store lock poisoned".to_string()))?;
        execute_in_memory_trigger_command(
            &mut state,
            operation_id,
            command,
            self.clock.timestamp_ms(),
        )
    }

    async fn list_subscriptions(
        &self,
        filter: TriggerSubscriptionFilter,
    ) -> Result<Vec<TriggerSubscriptionRecord>, PluginError> {
        let state = self
            .state
            .lock()
            .map_err(|_| PluginError::Session("trigger store lock poisoned".to_string()))?;
        let mut records = state
            .subscriptions
            .values()
            .filter(|record| filter.matches(record))
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| {
            left.registrant_scope_id()
                .cmp(&right.registrant_scope_id())
                .then_with(|| left.subscription_key.cmp(&right.subscription_key))
        });
        Ok(records)
    }

    async fn delete_session_subscriptions(&self, session_id: &str) -> Result<usize, PluginError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| PluginError::Session("trigger store lock poisoned".to_string()))?;
        let mut changed = 0usize;
        let now = self.clock.timestamp_ms();
        for record in state.subscriptions.values_mut().filter(|record| {
            record.registrant_session_id() == Some(session_id) && !record.tombstoned
        }) {
            record.enabled = false;
            record.tombstoned = true;
            record.deleted_at_ms = Some(now);
            record.revision = record.revision.saturating_add(1);
            record.updated_at_ms = now;
            changed = changed.saturating_add(1);
        }
        Ok(changed)
    }

    async fn ingest_occurrence(
        &self,
        request: TriggerOccurrenceRequest,
    ) -> Result<TriggerIngressResult, PluginError> {
        validate_trigger_occurrence_request(&request)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| PluginError::Session("trigger store lock poisoned".to_string()))?;
        let request_hash = trigger_occurrence_request_hash(&request)?;
        if let Some(existing_id) = state
            .occurrence_id_by_idempotency_key
            .get(&request.idempotency_key)
            .cloned()
        {
            let existing_hash = state
                .occurrence_hashes
                .get(&existing_id)
                .cloned()
                .unwrap_or_default();
            if existing_hash != request_hash {
                return Err(PluginError::Session(format!(
                    "trigger occurrence idempotency conflict for `{}`",
                    request.idempotency_key
                )));
            }
            let occurrence = state.occurrences.get(&existing_id).cloned().ok_or_else(|| {
                PluginError::Session(format!(
                    "missing trigger occurrence `{existing_id}` for idempotency key"
                ))
            });
            let occurrence = occurrence?;
            let reservations = state
                .deliveries
                .values()
                .filter(|delivery| delivery.occurrence_id == existing_id)
                .map(|delivery| {
                    in_memory_delivery_reservation(
                        &state,
                        delivery,
                        TriggerDeliveryReservationStatus::AlreadyReserved,
                    )
                })
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(TriggerIngressResult {
                occurrence,
                reservations,
            });
        }
        let occurrence_id = deterministic_occurrence_id(&request)?;
        let record = TriggerOccurrenceRecord {
            occurrence_id: occurrence_id.clone(),
            source_type: request.source_type,
            source_key: request.source_key,
            payload: request.payload,
            idempotency_key: request.idempotency_key.clone(),
            source: request.source,
            session_id: request.session_id,
            occurred_at_ms: self.clock.timestamp_ms(),
        };
        state
            .occurrence_id_by_idempotency_key
            .insert(request.idempotency_key, occurrence_id.clone());
        state
            .occurrence_hashes
            .insert(occurrence_id.clone(), request_hash);
        state.occurrences.insert(occurrence_id, record.clone());
        let reservations =
            reserve_in_memory_for_occurrence(&mut state, &record, self.clock.as_ref())?;
        Ok(TriggerIngressResult {
            occurrence: record,
            reservations,
        })
    }

    async fn list_occurrences(
        &self,
        filter: TriggerOccurrenceFilter,
    ) -> Result<Vec<TriggerOccurrenceRecord>, PluginError> {
        let state = self
            .state
            .lock()
            .map_err(|_| PluginError::Session("trigger store lock poisoned".to_string()))?;
        let mut records = state
            .occurrences
            .values()
            .filter(|record| filter.matches(record))
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| {
            left.occurred_at_ms
                .cmp(&right.occurred_at_ms)
                .then_with(|| left.occurrence_id.cmp(&right.occurrence_id))
        });
        Ok(records)
    }

    async fn list_deliveries_by_occurrence_id(
        &self,
        occurrence_id: &str,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        self.list_deliveries_matching(|delivery| delivery.occurrence_id == occurrence_id)
    }

    async fn list_deliveries_by_subscription_id(
        &self,
        subscription_id: &str,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        self.list_deliveries_matching(|delivery| delivery.subscription_id == subscription_id)
    }

    async fn list_deliveries_by_process_id(
        &self,
        process_id: &str,
    ) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        self.list_deliveries_matching(|delivery| delivery.process_id == process_id)
    }

    async fn list_deliveries(&self) -> Result<Vec<TriggerDeliveryReservation>, PluginError> {
        self.list_deliveries_matching(|_| true)
    }

    async fn prune_mutation_receipts(&self, cutoff_epoch_ms: u64) -> Result<usize, PluginError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| PluginError::Session("trigger store lock poisoned".to_string()))?;
        let before = state.mutation_receipts.len();
        state
            .mutation_receipts
            .retain(|_, (_, _, created_at_ms)| *created_at_ms >= cutoff_epoch_ms);
        Ok(before.saturating_sub(state.mutation_receipts.len()))
    }
}

fn execute_in_memory_trigger_command(
    state: &mut InMemoryTriggerEventState,
    operation_id: &str,
    command: TriggerCommand,
    now: u64,
) -> Result<TriggerEffectResult, PluginError> {
    let is_mutation = command.is_mutation();
    let request_hash = trigger_command_hash(&command)?;
    let receipt_id = trigger_operation_receipt_id(command.owner_scope(), operation_id)?;
    if is_mutation
        && let Some((existing_hash, existing_result, _)) = state.mutation_receipts.get(&receipt_id)
    {
        if existing_hash == &request_hash {
            return Ok(existing_result.clone());
        }
        return Ok(Err(TriggerOperationError::Conflict {
            subscription_key: command.subscription_key().unwrap_or_default().to_string(),
            existing_revision: None,
            existing_definition_hash: Some(existing_hash.clone()),
            requested_definition_hash: Some(request_hash),
            reason: format!("operation id `{operation_id}` was reused with different content"),
        }));
    }

    let result = apply_in_memory_trigger_command(state, command, now);
    if is_mutation {
        state
            .mutation_receipts
            .insert(receipt_id, (request_hash, result.clone(), now));
    }
    Ok(result)
}

pub(super) fn apply_in_memory_trigger_command(
    state: &mut InMemoryTriggerEventState,
    command: TriggerCommand,
    now: u64,
) -> TriggerEffectResult {
    match command {
        TriggerCommand::List {
            owner_scope,
            mut filter,
        } => {
            filter.registrant_scope_id = Some(owner_scope.namespace());
            let mut records = state
                .subscriptions
                .values()
                .filter(|record| filter.matches(record))
                .cloned()
                .collect::<Vec<_>>();
            records.sort_by(|left, right| left.subscription_key.cmp(&right.subscription_key));
            Ok(TriggerCommandOutcome::List { records })
        }
        TriggerCommand::Prune {
            owner_scope,
            actor,
            subscription_keys,
        } => {
            let records = state.subscriptions.values().cloned().collect::<Vec<_>>();
            let result =
                evaluate_trigger_prune(records, owner_scope, actor, subscription_keys, now)?;
            if let TriggerCommandOutcome::Prune { receipts } = &result {
                for receipt in receipts {
                    state.subscriptions.insert(
                        receipt.subscription_id.clone(),
                        receipt.record_snapshot.clone(),
                    );
                }
            }
            Ok(result)
        }
        TriggerCommand::Register {
            owner_scope,
            actor,
            draft,
        } => {
            draft.validate().map_err(TriggerOperationError::from)?;
            let subscription_id =
                deterministic_subscription_id(&owner_scope, &draft.subscription_key)
                    .map_err(TriggerOperationError::from)?;
            let definition_hash = trigger_subscription_definition_hash(&owner_scope, &draft)
                .map_err(TriggerOperationError::from)?;
            if let Some(existing) = state.subscriptions.get(&subscription_id).cloned() {
                if !existing.tombstoned && existing.definition_hash == definition_hash {
                    return Ok(TriggerCommandOutcome::Mutation {
                        receipt: Box::new(TriggerMutationReceipt::from_record(
                            existing,
                            TriggerMutationDisposition::Unchanged,
                        )),
                    });
                }
                return Err(subscription_conflict(
                    &draft.subscription_key,
                    Some(&existing),
                    Some(definition_hash),
                    if existing.tombstoned {
                        "subscription is tombstoned; use revive"
                    } else {
                        "register does not replace a different definition; use update"
                    },
                ));
            }
            let record = subscription_record_from_draft(
                owner_scope,
                actor,
                draft,
                subscription_id.clone(),
                uuid::Uuid::new_v4().to_string(),
                1,
                definition_hash,
                true,
                now,
                now,
            );
            state.subscriptions.insert(subscription_id, record.clone());
            Ok(TriggerCommandOutcome::Mutation {
                receipt: Box::new(TriggerMutationReceipt::from_record(
                    record,
                    TriggerMutationDisposition::Created,
                )),
            })
        }
        TriggerCommand::Update {
            owner_scope,
            actor,
            subscription_key,
            mut draft,
            expected_revision,
        } => {
            draft.subscription_key.clone_from(&subscription_key);
            draft.validate().map_err(TriggerOperationError::from)?;
            let subscription_id = deterministic_subscription_id(&owner_scope, &subscription_key)
                .map_err(TriggerOperationError::from)?;
            let requested_hash = trigger_subscription_definition_hash(&owner_scope, &draft)
                .map_err(TriggerOperationError::from)?;
            let Some(existing) = state.subscriptions.get(&subscription_id).cloned() else {
                return Err(subscription_conflict(
                    &subscription_key,
                    None,
                    Some(requested_hash),
                    "subscription does not exist",
                ));
            };
            ensure_live_revision(&existing, expected_revision, Some(requested_hash.clone()))?;
            let record = subscription_record_from_draft(
                owner_scope,
                actor,
                draft,
                subscription_id.clone(),
                existing.incarnation,
                existing.revision.saturating_add(1),
                requested_hash,
                existing.enabled,
                existing.created_at_ms,
                now,
            );
            state.subscriptions.insert(subscription_id, record.clone());
            Ok(TriggerCommandOutcome::Mutation {
                receipt: Box::new(TriggerMutationReceipt::from_record(
                    record,
                    TriggerMutationDisposition::Updated,
                )),
            })
        }
        TriggerCommand::Enable {
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
        } => mutate_enabled(
            state,
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
            true,
            now,
        ),
        TriggerCommand::Disable {
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
        } => mutate_enabled(
            state,
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
            false,
            now,
        ),
        TriggerCommand::Delete {
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
        } => {
            let subscription_id = deterministic_subscription_id(&owner_scope, &subscription_key)
                .map_err(TriggerOperationError::from)?;
            let Some(existing) = state.subscriptions.get_mut(&subscription_id) else {
                return Err(subscription_conflict(
                    &subscription_key,
                    None,
                    None,
                    "subscription does not exist",
                ));
            };
            ensure_live_revision(existing, expected_revision, None)?;
            existing.registrant = actor;
            existing.enabled = false;
            existing.tombstoned = true;
            existing.deleted_at_ms = Some(now);
            existing.revision = existing.revision.saturating_add(1);
            existing.updated_at_ms = now;
            Ok(TriggerCommandOutcome::Mutation {
                receipt: Box::new(TriggerMutationReceipt::from_record(
                    existing.clone(),
                    TriggerMutationDisposition::Deleted,
                )),
            })
        }
        TriggerCommand::Revive {
            owner_scope,
            actor,
            subscription_key,
            mut draft,
            expected_revision,
        } => {
            draft.subscription_key.clone_from(&subscription_key);
            draft.validate().map_err(TriggerOperationError::from)?;
            let subscription_id = deterministic_subscription_id(&owner_scope, &subscription_key)
                .map_err(TriggerOperationError::from)?;
            let requested_hash = trigger_subscription_definition_hash(&owner_scope, &draft)
                .map_err(TriggerOperationError::from)?;
            let Some(existing) = state.subscriptions.get(&subscription_id).cloned() else {
                return Err(subscription_conflict(
                    &subscription_key,
                    None,
                    Some(requested_hash),
                    "subscription does not exist; use register",
                ));
            };
            if !existing.tombstoned || existing.revision != expected_revision {
                return Err(subscription_conflict(
                    &subscription_key,
                    Some(&existing),
                    Some(requested_hash),
                    "revive requires the current tombstone revision",
                ));
            }
            let record = subscription_record_from_draft(
                owner_scope,
                actor,
                draft,
                subscription_id.clone(),
                uuid::Uuid::new_v4().to_string(),
                existing.revision.saturating_add(1),
                requested_hash,
                true,
                existing.created_at_ms,
                now,
            );
            state.subscriptions.insert(subscription_id, record.clone());
            Ok(TriggerCommandOutcome::Mutation {
                receipt: Box::new(TriggerMutationReceipt::from_record(
                    record,
                    TriggerMutationDisposition::Revived,
                )),
            })
        }
    }
}
