use super::*;

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
        Arc<std::sync::Mutex<Vec<TokenLedgerEntry>>>,
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
        self.tx.lock().expect("child usage relay lock").take();
    }

    async fn emit(&self, event: SessionStreamEvent) {
        let tx = self.tx.lock().expect("child usage relay lock").clone();
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

    pub(in crate::runtime::session_manager) fn drain_token_ledger(&self) -> Vec<TokenLedgerEntry> {
        let mut ledger = self.token_ledger.lock().expect("token ledger lock");
        std::mem::take(&mut *ledger)
    }

    pub(in crate::runtime::session_manager) fn merge_drained_token_ledger(
        &self,
        state: &mut RuntimeSessionState,
    ) -> Vec<TokenLedgerEntry> {
        let drained = self.drain_token_ledger();
        for entry in drained.iter().cloned() {
            merge_ledger_entry(&mut state.token_ledger, entry);
        }
        drained
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
        let drained = self.drain_token_ledger();
        if drained.is_empty() {
            return Ok(());
        }
        for entry in drained.iter().cloned() {
            merge_ledger_entry(&mut state.token_ledger, entry);
        }
        let operation =
            super::super::state::boundary_operation(&state.session_id, boundary_id, "usage-ledger");
        let (commit, persisted_node_ids) =
            crate::store::RuntimeCommit::persisted_state_with_operation(
                &mut state, &drained, operation,
            )
            .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        let result = commit_runtime_state_with_fresh_session_execution_lease(
            Arc::clone(store),
            commit,
            &current.runtime_lease_owner,
            current.host.core.control.lease_timings,
            Arc::clone(&current.host.core.clock),
        )
        .await
        .map_err(|err| crate::PluginError::Session(err.to_string()))?;
        state.apply_persisted_commit_result(result);
        state.mark_node_ids_persisted(persisted_node_ids);
        Ok(())
    }
}

fn usage_has_any_tokens(usage: &TokenUsage) -> bool {
    usage.input_tokens != 0
        || usage.output_tokens != 0
        || usage.cache_read_input_tokens != 0
        || usage.cache_write_input_tokens != 0
        || usage.reasoning_output_tokens != 0
}

pub(in crate::runtime::session_manager) fn record_token_usage_shared(
    token_ledger: &Arc<std::sync::Mutex<Vec<TokenLedgerEntry>>>,
    source: &str,
    model: &str,
    usage: &TokenUsage,
) {
    if !usage_has_any_tokens(usage) {
        return;
    }
    let mut ledger = token_ledger.lock().expect("token ledger lock");
    if let Some(entry) = ledger
        .iter_mut()
        .find(|entry| entry.source == source && entry.model == model)
    {
        entry.usage.input_tokens += usage.input_tokens;
        entry.usage.output_tokens += usage.output_tokens;
        entry.usage.cache_read_input_tokens += usage.cache_read_input_tokens;
        entry.usage.cache_write_input_tokens += usage.cache_write_input_tokens;
        entry.usage.reasoning_output_tokens += usage.reasoning_output_tokens;
    } else {
        ledger.push(TokenLedgerEntry {
            source: source.to_string(),
            model: model.to_string(),
            usage: usage.clone(),
        });
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
    usage_has_any_tokens(&delta).then_some(delta)
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

    async fn relay_token_usage(
        &self,
        protocol_iteration: usize,
        _usage: &TokenUsage,
        cumulative_usage: &TokenUsage,
    ) {
        let (delta, cumulative) = {
            let mut live_usage = self
                .child_turn_live_usage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
