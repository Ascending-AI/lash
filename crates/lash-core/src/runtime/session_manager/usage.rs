use super::*;
use lash_sansio::sync::MutexExt;
use std::sync::atomic::Ordering;

#[derive(Clone, Debug)]
#[cfg_attr(any(test, feature = "testing"), derive(serde::Serialize))]
pub(crate) struct PendingTokenLedgerEntry {
    pub(crate) entry: TokenLedgerEntry,
    pub(crate) identity: Option<crate::store::RuntimeUsageDeltaIdentity>,
}

impl PendingTokenLedgerEntry {
    pub(crate) fn unstaged(entry: TokenLedgerEntry) -> Self {
        Self {
            entry,
            identity: None,
        }
    }
}

impl std::ops::Deref for PendingTokenLedgerEntry {
    type Target = TokenLedgerEntry;

    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

#[derive(Clone)]
pub(in crate::runtime::session_manager) struct ChannelEventSink {
    pub(in crate::runtime::session_manager) tx: mpsc::Sender<SessionStreamEvent>,
    pub(in crate::runtime::session_manager) live_usage: Option<LiveChildUsageForwarder>,
}

#[derive(Clone)]
pub(in crate::runtime::session_manager) struct LiveChildUsageForwarder {
    pub(in crate::runtime::session_manager) turn_id: String,
    pub(in crate::runtime::session_manager) session_id: String,
    pub(in crate::runtime::session_manager) source: String,
    pub(in crate::runtime::session_manager) model: String,
    pub(in crate::runtime::session_manager) token_ledger:
        Arc<std::sync::Mutex<Vec<PendingTokenLedgerEntry>>>,
    pub(in crate::runtime::session_manager) child_turn_live_usage:
        Arc<std::sync::Mutex<HashMap<String, TokenUsage>>>,
    pub(in crate::runtime::session_manager) relay: Option<ChildUsageEventRelay>,
    /// Set by the turn's `ManagedTurnLease` when it releases the live-usage
    /// entry. An emit still in flight at that moment must not report, because
    /// `entry(..).or_default()` would resurrect the released entry with a zero
    /// baseline and re-record the full cumulative usage.
    pub(in crate::runtime::session_manager) turn_released: Arc<AtomicBool>,
}

#[derive(Clone, Default)]
pub(in crate::runtime) struct ChildUsageEventRelay {
    tx: Arc<StdMutex<Option<mpsc::Sender<RuntimeStreamEvent>>>>,
}

impl ChildUsageEventRelay {
    pub(in crate::runtime) fn new(tx: mpsc::Sender<RuntimeStreamEvent>) -> Self {
        Self {
            tx: Arc::new(StdMutex::new(Some(tx))),
        }
    }

    pub(in crate::runtime) fn clear(&self) {
        self.tx.lock_recover().take();
    }

    async fn emit(&self, event: SessionStreamEvent) {
        let tx = self.tx.lock_recover().clone();
        let Some(tx) = tx else { return };
        if tx.is_closed() {
            return;
        }
        // Project ChildTokenUsage onto the embed-facing TurnActivity stream
        // before forwarding the SessionStreamEvent itself. Other variants reach the
        // turn-activity stream through `send_session_event` in `turn_driver`,
        // but child usage skips that path because it originates in the
        // session manager rather than the parent's turn driver.
        if let SessionStreamEvent::ChildTokenUsage {
            session_id,
            source,
            model,
            protocol_iteration,
            usage,
            cumulative,
        } = &event
        {
            let activity = TurnActivity::new(
                TurnActivityId::new(uuid::Uuid::new_v4().to_string()),
                TurnEvent::ChildUsage {
                    session_id: session_id.clone(),
                    source: source.clone(),
                    model: model.clone(),
                    protocol_iteration: *protocol_iteration,
                    usage: usage.clone(),
                    cumulative: cumulative.clone(),
                },
            );
            let _ = tx.send(RuntimeStreamEvent::Turn(activity)).await;
        }
        let _ = tx.send(RuntimeStreamEvent::Session(event)).await;
    }
}

impl UsageCapability {
    pub(in crate::runtime) fn record_token_usage(
        &self,
        source: &str,
        model: &str,
        usage: &TokenUsage,
    ) {
        record_token_usage_shared(&self.token_ledger, source, model, usage);
    }

    pub(in crate::runtime::session_manager) fn stage_token_ledger(
        &self,
        state: &mut RuntimeSessionState,
        operation: &crate::OperationId,
    ) -> Result<StagedTokenLedger, crate::StoreError> {
        let staged = stage_token_ledger_shared(&self.token_ledger, operation)?;
        let mut projected = state.token_ledger.clone();
        for delta in staged.deltas() {
            crate::store::merge_token_ledger_entry_checked(&mut projected, delta.entry.clone())?;
        }
        state.token_ledger = projected;
        Ok(staged)
    }

    pub(in crate::runtime) async fn persist_current_usage_ledger(
        &self,
        current: &CurrentSessionCapability,
        boundary_id: &str,
    ) -> Result<(), crate::PluginError> {
        if !self.persist_to_store {
            return Ok(());
        }
        let Some(store) = &current.store else {
            return Ok(());
        };
        let mut state = current.current_snapshot_for_store_write().await?;
        let operation =
            super::super::state::boundary_operation(&state.session_id, boundary_id, "usage-ledger");
        let staged = self
            .stage_token_ledger(&mut state, &operation)
            .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        if staged.deltas().is_empty() {
            return Ok(());
        }
        let (commit, persisted_node_ids) =
            crate::store::RuntimeCommit::persisted_state_with_operation_and_staged_usage_and_budget(
                &mut state,
                staged.deltas(),
                operation,
                current.host.core.durability.commit_budget,
            )
            .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        // Dual-context site: ordinary host services are lane-less, but a
        // TurnPersisted observer may finish managed-child usage while the
        // parent lane is still held. Select authority from the explicit
        // service context, never from scheduling or elapsed time.
        let result = if let Some(lease) = &current.held_session_execution_lease {
            let result = commit_runtime_state_with_borrowed_lease(
                lease,
                Arc::clone(store),
                commit,
                &current.runtime_lease_owner,
            )
            .await;
            if result.is_ok() {
                current
                    .resident_graph_head_stale
                    .store(true, Ordering::Release);
            }
            result
        } else {
            commit_runtime_state_with_fresh_session_execution_lease(
                Arc::clone(store),
                commit,
                &current.runtime_lease_owner,
                &current.runtime_lease_executor_id,
                current.host.core.control.lease_timings,
                Arc::clone(&current.host.core.clock),
            )
            .await
        }
        .map_err(|err| match err {
            crate::StoreError::SessionExecutionLeaseExpired { session_id } => {
                crate::PluginError::SessionExecutionLeaseLost { session_id }
            }
            err => crate::PluginError::Session(err.to_string()),
        })?;
        let confirmed_usage = result.committed_usage_delta_identities.clone();
        staged
            .confirm_identities(&confirmed_usage)
            .map_err(plugin_error_from_usage_confirmation)?;
        state.apply_persisted_commit_result(result);
        state.mark_node_ids_persisted(persisted_node_ids);
        Ok(())
    }
}

pub(crate) struct StagedTokenLedger {
    ledger: Arc<std::sync::Mutex<Vec<PendingTokenLedgerEntry>>>,
    deltas: Vec<crate::store::RuntimeUsageDelta>,
}

impl StagedTokenLedger {
    pub(crate) fn deltas(&self) -> &[crate::store::RuntimeUsageDelta] {
        &self.deltas
    }

    pub(crate) fn confirm_identities(
        self,
        confirmed: &[crate::store::RuntimeUsageDeltaIdentity],
    ) -> Result<(), crate::StoreError> {
        let confirmed_count = confirmed.len();
        let confirmed = confirmed
            .iter()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let mut ledger = self.ledger.lock_recover();
        let staged = ledger
            .iter()
            .filter_map(|pending| pending.identity.clone())
            .collect::<std::collections::HashSet<_>>();
        let staged_count = confirmed
            .iter()
            .filter(|identity| staged.contains(*identity))
            .count();
        let confirmations_are_unique = confirmed.len() == confirmed_count;
        let confirmations_are_staged = confirmed.iter().all(|identity| staged.contains(identity));
        if !confirmations_are_unique || !confirmations_are_staged || staged_count != confirmed_count
        {
            return Err(crate::StoreError::UnstagedUsageConfirmation {
                confirmed_count,
                staged_count,
            });
        }
        ledger.retain(|pending| {
            pending
                .identity
                .as_ref()
                .is_none_or(|identity| !confirmed.contains(identity))
        });
        Ok(())
    }
}

pub(super) fn plugin_error_from_usage_confirmation(error: crate::StoreError) -> crate::PluginError {
    match error {
        crate::StoreError::UnstagedUsageConfirmation {
            confirmed_count,
            staged_count,
        } => crate::PluginError::UnstagedUsageConfirmation {
            confirmed_count,
            staged_count,
        },
        error => crate::PluginError::Session(error.to_string()),
    }
}

pub(crate) fn stage_token_ledger_shared(
    token_ledger: &Arc<std::sync::Mutex<Vec<PendingTokenLedgerEntry>>>,
    operation: &crate::OperationId,
) -> Result<StagedTokenLedger, crate::StoreError> {
    let operation_storage_key = operation.storage_key()?;
    let mut ledger = token_ledger.lock_recover();
    let mut next_ordinal = ledger
        .iter()
        .filter_map(|pending| pending.identity.as_ref())
        .filter(|identity| identity.operation_storage_key == operation_storage_key)
        .map(|identity| identity.entry_ordinal)
        .max()
        .map_or(Ok(0), |ordinal| {
            ordinal.checked_add(1).ok_or_else(|| {
                crate::StoreError::Backend(
                    "usage delta ordinal overflowed durable u64 identity".to_string(),
                )
            })
        })?;
    for pending in ledger
        .iter_mut()
        .filter(|pending| pending.identity.is_none())
    {
        pending.identity = Some(crate::store::RuntimeUsageDeltaIdentity::for_entry(
            operation_storage_key.clone(),
            next_ordinal,
            &pending.entry,
        ));
        next_ordinal = next_ordinal.checked_add(1).ok_or_else(|| {
            crate::StoreError::Backend(
                "usage delta ordinal overflowed durable u64 identity".to_string(),
            )
        })?;
    }
    let deltas = ledger
        .iter()
        .map(|pending| {
            let identity = pending.identity.clone().ok_or_else(|| {
                crate::StoreError::Backend(
                    "staging left a pending usage row without durable identity".to_string(),
                )
            })?;
            Ok(crate::store::RuntimeUsageDelta {
                identity,
                entry: pending.entry.clone(),
            })
        })
        .collect::<Result<Vec<_>, crate::StoreError>>()?;
    drop(ledger);
    Ok(StagedTokenLedger {
        ledger: Arc::clone(token_ledger),
        deltas,
    })
}

pub(crate) fn record_token_usage_shared(
    token_ledger: &Arc<std::sync::Mutex<Vec<PendingTokenLedgerEntry>>>,
    source: &str,
    model: &str,
    usage: &TokenUsage,
) {
    if usage.is_zero() {
        return;
    }
    let mut ledger = token_ledger.lock_recover();
    if let Some(entry) = ledger
        .iter_mut()
        .find(|entry| entry.identity.is_none() && entry.source == source && entry.model == model)
    {
        // Pre-identity staging deliberately saturates so infallible provider
        // callbacks cannot wrap or discard a row. The checked merge permits a
        // clamped counter only when every counter and the canonical total still
        // fit in i64; otherwise projection/commit returns the typed error.
        entry.entry.usage.input_tokens = entry
            .entry
            .usage
            .input_tokens
            .saturating_add(usage.input_tokens);
        entry.entry.usage.output_tokens = entry
            .entry
            .usage
            .output_tokens
            .saturating_add(usage.output_tokens);
        entry.entry.usage.cache_read_input_tokens = entry
            .entry
            .usage
            .cache_read_input_tokens
            .saturating_add(usage.cache_read_input_tokens);
        entry.entry.usage.cache_write_input_tokens = entry
            .entry
            .usage
            .cache_write_input_tokens
            .saturating_add(usage.cache_write_input_tokens);
        entry.entry.usage.reasoning_output_tokens = entry
            .entry
            .usage
            .reasoning_output_tokens
            .saturating_add(usage.reasoning_output_tokens);
    } else {
        ledger.push(PendingTokenLedgerEntry::unstaged(TokenLedgerEntry {
            source: source.to_string(),
            model: model.to_string(),
            usage: usage.clone(),
        }));
    }
}

pub(in crate::runtime::session_manager) fn subtract_usage(
    reported_total: &TokenUsage,
    final_total: &TokenUsage,
) -> Option<TokenUsage> {
    let delta = TokenUsage {
        input_tokens: final_total
            .input_tokens
            .saturating_sub(reported_total.input_tokens),
        output_tokens: final_total
            .output_tokens
            .saturating_sub(reported_total.output_tokens),
        cache_read_input_tokens: final_total
            .cache_read_input_tokens
            .saturating_sub(reported_total.cache_read_input_tokens),
        cache_write_input_tokens: final_total
            .cache_write_input_tokens
            .saturating_sub(reported_total.cache_write_input_tokens),
        reasoning_output_tokens: final_total
            .reasoning_output_tokens
            .saturating_sub(reported_total.reasoning_output_tokens),
    };
    (!delta.is_zero()).then_some(delta)
}

impl LiveChildUsageForwarder {
    #[cfg(test)]
    pub(in crate::runtime::session_manager) async fn relay_token_usage_for_test(
        &self,
        protocol_iteration: usize,
        usage: &TokenUsage,
        cumulative_usage: &TokenUsage,
    ) {
        self.relay_token_usage(protocol_iteration, usage, cumulative_usage)
            .await;
    }

    #[cfg(test)]
    pub(in crate::runtime::session_manager) async fn relay_token_usage_gated_for_test(
        &self,
        protocol_iteration: usize,
        usage: &TokenUsage,
        cumulative_usage: &TokenUsage,
        after_live_accounting: impl std::future::Future<Output = ()>,
    ) {
        self.relay_token_usage_with_after_live_accounting(
            protocol_iteration,
            usage,
            cumulative_usage,
            after_live_accounting,
        )
        .await;
    }

    async fn relay_token_usage(
        &self,
        protocol_iteration: usize,
        usage: &TokenUsage,
        cumulative_usage: &TokenUsage,
    ) {
        self.relay_token_usage_with_after_live_accounting(
            protocol_iteration,
            usage,
            cumulative_usage,
            std::future::ready(()),
        )
        .await;
    }

    async fn relay_token_usage_with_after_live_accounting(
        &self,
        protocol_iteration: usize,
        _usage: &TokenUsage,
        cumulative_usage: &TokenUsage,
        after_live_accounting: impl std::future::Future<Output = ()>,
    ) {
        let (delta, cumulative) = {
            let mut live_usage = self.child_turn_live_usage.lock_recover();
            // Ordering: the lease sets `turn_released` before it removes the
            // entry, and this check happens under the same lock the removal
            // takes, so an emit either observes a live entry (and is counted) or
            // observes the release (and is dropped). Reporting after release
            // would both leak a resurrected entry and double-count.
            if self.turn_released.load(Ordering::Acquire) {
                return;
            }
            let reported = live_usage.entry(self.turn_id.clone()).or_default();
            let Some(delta) = subtract_usage(reported, cumulative_usage) else {
                return;
            };
            *reported = cumulative_usage.clone();
            (delta, reported.clone())
        };
        after_live_accounting.await;
        record_token_usage_shared(&self.token_ledger, &self.source, &self.model, &delta);
        if let Some(relay) = &self.relay {
            relay
                .emit(SessionStreamEvent::ChildTokenUsage {
                    session_id: self.session_id.clone(),
                    source: self.source.clone(),
                    model: self.model.clone(),
                    protocol_iteration,
                    usage: delta,
                    cumulative,
                })
                .await;
        }
    }
}

#[async_trait::async_trait]
impl EventSink for ChannelEventSink {
    async fn emit(&self, event: SessionStreamEvent) {
        if let SessionStreamEvent::TokenUsage {
            protocol_iteration,
            usage,
            cumulative,
        } = &event
            && let Some(live_usage) = &self.live_usage
        {
            live_usage
                .relay_token_usage(*protocol_iteration, usage, cumulative)
                .await;
        }
        if !self.tx.is_closed() {
            let _ = self.tx.send(event).await;
        }
    }
}

#[cfg(test)]
mod staging_tests {
    use super::*;

    fn operation(id: &str) -> crate::OperationId {
        super::super::state::boundary_operation("root", id, "usage-ledger")
    }

    fn entry(input_tokens: i64) -> TokenLedgerEntry {
        TokenLedgerEntry {
            source: "source".to_string(),
            model: "model".to_string(),
            usage: TokenUsage {
                input_tokens,
                ..TokenUsage::default()
            },
        }
    }

    #[test]
    fn staging_overflow_is_typed_and_keeps_every_pending_identity() {
        let mut overflowing = entry(i64::MAX);
        overflowing.usage.output_tokens = 1;
        let ledger = Arc::new(std::sync::Mutex::new(vec![
            PendingTokenLedgerEntry::unstaged(overflowing),
        ]));
        let staged = stage_token_ledger_shared(&ledger, &operation("overflow"))
            .expect("identity staging does not perform usage arithmetic");
        let mut projected = Vec::new();
        let error = crate::store::merge_token_ledger_entry_checked(
            &mut projected,
            staged.deltas()[0].entry.clone(),
        )
        .expect_err("overflow must be reported");
        assert!(matches!(
            error,
            crate::StoreError::TokenUsageAccountingOverflow {
                counter: "total_tokens",
                ..
            }
        ));
        let pending = ledger.lock_recover();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].usage.input_tokens, i64::MAX);
        assert!(pending[0].identity.is_some());
    }

    #[test]
    fn confirmation_ignores_mutex_poison_and_preserves_concurrent_usage() {
        let ledger = Arc::new(std::sync::Mutex::new(vec![
            PendingTokenLedgerEntry::unstaged(entry(5)),
        ]));
        let staged = stage_token_ledger_shared(&ledger, &operation("poison")).expect("stage usage");
        record_token_usage_shared(
            &ledger,
            "source",
            "model",
            &TokenUsage {
                input_tokens: 7,
                ..TokenUsage::default()
            },
        );
        let poison_target = Arc::clone(&ledger);
        let _ = std::panic::catch_unwind(move || {
            let _guard = poison_target.lock_recover();
            panic!("poison token ledger");
        });
        let identities = staged
            .deltas()
            .iter()
            .map(|delta| delta.identity.clone())
            .collect::<Vec<_>>();
        staged
            .confirm_identities(&identities)
            .expect("confirm staged identities");
        let pending = ledger.lock_recover();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].identity.is_none());
        assert_eq!(pending[0].usage.input_tokens, 7);
    }

    #[test]
    fn confirmation_refuses_unstaged_identity_without_removing_staged_usage() {
        let ledger = Arc::new(std::sync::Mutex::new(vec![
            PendingTokenLedgerEntry::unstaged(entry(5)),
        ]));
        let staged =
            stage_token_ledger_shared(&ledger, &operation("unstaged")).expect("stage usage");
        let staged_identity = staged.deltas()[0].identity.clone();
        let mut foreign_identity = staged_identity.clone();
        foreign_identity.entry_ordinal += 1;

        let error = staged
            .confirm_identities(&[staged_identity, foreign_identity])
            .expect_err("foreign confirmation must be refused");
        assert!(matches!(
            error,
            crate::StoreError::UnstagedUsageConfirmation {
                confirmed_count: 2,
                staged_count: 1,
            }
        ));
        let pending = ledger.lock_recover();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].identity.is_some());
    }
}
