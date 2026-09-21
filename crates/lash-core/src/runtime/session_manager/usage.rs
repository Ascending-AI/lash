use super::*;
use lash_sansio::sync::MutexExt;

#[derive(Clone, Debug)]
#[cfg_attr(any(test, feature = "testing"), derive(serde::Serialize))]
pub struct PendingTokenLedgerEntry {
    pub entry: TokenLedgerEntry,
    pub identity: Option<crate::store::RuntimeUsageDeltaIdentity>,
}

impl PendingTokenLedgerEntry {
    pub fn unstaged(entry: TokenLedgerEntry) -> Self {
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
        state.capture_plugin_states(&current.plugins);
        let (mut commit, persisted_node_ids) =
            crate::store::RuntimeCommit::persisted_state_with_operation_and_staged_usage_and_budget(
                &mut state,
                staged.deltas(),
                operation,
                current.host.core.durability.commit_budget,
            )
            .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        // Stamp last: the semantic-boundary identity hashes the commit's
        // canonical request content, so every content edit must precede it.
        commit
            .stamp_semantic_boundary()
            .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        let result = super::super::state::commit_in_lane_context(
            current.held_session_execution_lease.as_ref(),
            Arc::clone(store),
            commit,
            &current.runtime_lease_owner,
            &current.runtime_lease_executor_id,
            current.host.core.control.lease_timings,
            Arc::clone(&current.host.core.clock),
            &current.resident_graph_head_stale,
        )
        .await
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

pub struct StagedTokenLedger {
    ledger: Arc<std::sync::Mutex<Vec<PendingTokenLedgerEntry>>>,
    deltas: Vec<crate::store::RuntimeUsageDelta>,
    minted: Vec<crate::store::RuntimeUsageDeltaIdentity>,
}

impl StagedTokenLedger {
    pub fn deltas(&self) -> &[crate::store::RuntimeUsageDelta] {
        &self.deltas
    }

    /// Rolls back this staging after the durable commit carrying `deltas`
    /// failed: pending rows this staging claimed are removed from the shared
    /// ledger, so the journaled effect that recorded them re-records the
    /// billed usage on replay rather than double-merging a retained row.
    /// Rows staged under earlier operations keep their identity and stay
    /// pending for the next boundary.
    pub fn discard_staged(self) {
        let minted = self
            .minted
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        let mut ledger = self.ledger.lock_recover();
        ledger.retain(|pending| {
            pending
                .identity
                .as_ref()
                .is_none_or(|identity| !minted.contains(identity))
        });
    }

    pub fn confirm_identities(
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

pub fn stage_token_ledger_shared(
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
    let mut minted = Vec::new();
    for pending in ledger
        .iter_mut()
        .filter(|pending| pending.identity.is_none())
    {
        let identity = crate::store::RuntimeUsageDeltaIdentity::for_entry(
            operation_storage_key.clone(),
            next_ordinal,
            &pending.entry,
        );
        minted.push(identity.clone());
        pending.identity = Some(identity);
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
        minted,
    })
}

pub fn record_token_usage_shared(
    token_ledger: &Arc<std::sync::Mutex<Vec<PendingTokenLedgerEntry>>>,
    source: &str,
    model: &str,
    usage: &TokenUsage,
) {
    if usage.is_zero() {
        return;
    }
    let mut ledger = token_ledger.lock_recover();
    if let Some(entry) = ledger.iter_mut().find(|entry| {
        entry.identity.is_none()
            && entry.source == source
            && entry.model == model
            && entry.usage_disposition.is_reported()
    }) {
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
        ledger.push(PendingTokenLedgerEntry::unstaged(
            TokenLedgerEntry::reported(source, model, usage.clone()),
        ));
    }
}

/// Record interrupted attempts whose provider usage never arrived: a
/// zero-usage row marked unreported, so the ledger shows the hole instead of
/// writing nothing (ADR 0031). Accumulates into the pending unreported row for
/// the same `(source, model)`.
pub fn record_unreported_attempts_shared(
    token_ledger: &Arc<std::sync::Mutex<Vec<PendingTokenLedgerEntry>>>,
    source: &str,
    model: &str,
    attempts: &[crate::UnreportedLedgerAttempt],
) {
    if attempts.is_empty() {
        return;
    }
    let incoming = crate::LedgerUsageDisposition::unreported(attempts.iter().cloned());
    let mut ledger = token_ledger.lock_recover();
    if let Some(entry) = ledger.iter_mut().find(|entry| {
        entry.identity.is_none()
            && entry.source == source
            && entry.model == model
            && matches!(
                entry.usage_disposition,
                crate::LedgerUsageDisposition::Unreported { .. }
            )
    }) {
        entry.entry.usage_disposition.absorb_saturating(&incoming);
    } else {
        ledger.push(PendingTokenLedgerEntry::unstaged(TokenLedgerEntry {
            source: source.to_string(),
            model: model.to_string(),
            usage: TokenUsage::default(),
            usage_disposition: incoming,
        }));
    }
}

/// Append-only: never merged into the reported row and never rewrites the unreported row it
/// fills.
pub fn record_reconciled_usage_shared(
    token_ledger: &Arc<std::sync::Mutex<Vec<PendingTokenLedgerEntry>>>,
    source: &str,
    model: &str,
    usage: &TokenUsage,
    call_id: &str,
    attempt_ordinal: u32,
) {
    let mut ledger = token_ledger.lock_recover();
    ledger.push(PendingTokenLedgerEntry::unstaged(TokenLedgerEntry {
        source: source.to_string(),
        model: model.to_string(),
        usage: usage.clone(),
        usage_disposition: crate::LedgerUsageDisposition::Reconciled {
            call_id: call_id.to_string(),
            attempt_ordinal,
        },
    }));
}

#[async_trait::async_trait]
impl EventSink for ChannelEventSink {
    async fn emit(&self, event: SessionStreamEvent) {
        if !self.tx.is_closed() {
            let _ = self.tx.send(event).await;
        }
    }
}

#[cfg(test)]
mod staging_tests {
    use super::*;

    fn operation(id: &str) -> crate::OperationId {
        super::super::state::boundary_operation(&SessionId::from("root"), id, "usage-ledger")
    }

    fn entry(input_tokens: i64) -> TokenLedgerEntry {
        TokenLedgerEntry {
            source: "source".to_string(),
            model: "model".to_string(),
            usage: TokenUsage {
                input_tokens,
                ..TokenUsage::default()
            },
            usage_disposition: Default::default(),
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
