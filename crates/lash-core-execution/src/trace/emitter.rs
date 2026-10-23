//! Trace record preparation and delivery to observers.
use super::*;
impl TraceEmitter {
    /// Whether a record sink observes external records. A site tests it
    /// before it reads the clock or builds anything a record would hold.
    pub fn has_external_observers(&self) -> bool {
        !self.sinks.is_empty()
    }

    /// Whether a product observation consumer is installed.
    pub fn has_product_observers(&self) -> bool {
        !self.product.is_empty()
    }

    /// The record sinks, in installation order.
    pub fn sinks(&self) -> &[Arc<dyn TraceSink>] {
        &self.sinks
    }

    /// The product observation consumers, in installation order.
    pub fn product_observers(&self) -> &[Arc<dyn ProductObservationSink>] {
        &self.product
    }

    /// Emits one record at `position`.
    ///
    /// Returns before calling `identity` or `record` when nothing observes or
    /// `permit` is `None`: a replayed step and a retained read build nothing.
    /// `at_ms` is the retained time of the fact the record reports. The
    /// caller holds no engine lock: every sink is host code.
    pub fn emit(
        &self,
        permit: Option<&EmissionPermit>,
        position: Option<TracePosition>,
        identity: impl FnOnce() -> TraceRecordIdentity,
        at_ms: u64,
        record: impl FnOnce() -> (TraceContext, TraceEvent),
    ) {
        if let Some(record) = self.prepare(permit, position, identity, at_ms, record) {
            self.append(&record);
        }
    }

    /// The record [`emit`](Self::emit) would hand the sinks, built, selected
    /// and hooked, without appending it: for a record a publication hold
    /// keeps until its parent publishes. `None` when nothing is built.
    pub(crate) fn prepare(
        &self,
        permit: Option<&EmissionPermit>,
        position: Option<TracePosition>,
        identity: impl FnOnce() -> TraceRecordIdentity,
        at_ms: u64,
        record: impl FnOnce() -> (TraceContext, TraceEvent),
    ) -> Option<TraceRecord> {
        #[cfg(feature = "perf-witness")]
        let _history_scope = lash_core_ids::perf_witness::history::Scope::enter(
            lash_core_ids::perf_witness::history::Component::TraceObserver,
        );
        if permit.is_none() || !self.has_external_observers() {
            return None;
        }
        let (context, event) = record();
        match TraceRecord::identified(
            &identity(),
            context,
            event,
            TraceTimestamp::from_unix_ms(at_ms),
        ) {
            Ok(record) => {
                let mut record = record.at(position);
                let mut policy = self.policy.clone();
                let mut failed = self.selector_failed;
                if !self.selected
                    && let Some(selector) = &self.selector
                {
                    let ids = record.context.ids();
                    let mut narrow = lash_trace::core_internal::content_narrowing(&policy, None);
                    let kind = record
                        .position
                        .as_ref()
                        .map_or(TraceSpanKind::Point, |p| p.kind);
                    if let Err(payload) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            selector.select(
                                &lash_trace::SelectorView {
                                    kind,
                                    ids: &ids,
                                    policy: &policy,
                                },
                                &mut narrow,
                            )
                        }))
                    {
                        narrow = lash_trace::core_internal::content_narrowing(&policy, None);
                        narrow.omit_all();
                        failed = true;
                        if crate::panic_containment::is_loud() {
                            std::panic::resume_unwind(payload);
                        }
                    }
                    if !narrow.builds(kind) {
                        return None;
                    }
                    policy.content = narrow.content();
                }
                if failed {
                    record
                        .omissions
                        .add(lash_trace::TraceOmission::SelectorFailed);
                }
                let panic = record.apply_content(&policy, self.hook.as_deref());
                if record.content.hook_failed && !self.hook_failure.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        event = "trace.content_hook.failed",
                        "content hook panicked; content omitted"
                    );
                }
                if let Some(payload) = panic
                    && crate::panic_containment::is_loud()
                {
                    std::panic::resume_unwind(payload);
                }
                self.omissions.record(
                    &lash_trace::TraceOmissionSite::Record {
                        kind: record
                            .position
                            .as_ref()
                            .map_or(TraceSpanKind::Point, |p| p.kind),
                        record: record.id.clone(),
                        span: record.position.as_ref().map(|p| p.span.span_id()),
                    },
                    &record.omissions,
                );
                Some(record)
            }
            Err(error) => {
                tracing::warn!(%error, "failed to derive a trace record identity");
                None
            }
        }
    }

    /// Publishes one product observation (the process and language graph).
    /// It takes no permit and runs on replay, because the product folds it
    /// by its own identity; it never reaches a record sink.
    pub fn observe_product(&self, record: impl FnOnce() -> TraceRecord) {
        #[cfg(feature = "perf-witness")]
        let _history_scope = lash_core_ids::perf_witness::history::Scope::enter(
            lash_core_ids::perf_witness::history::Component::TraceObserver,
        );
        if self.product.is_empty() {
            return;
        }
        let record = record();
        for observer in self.product.iter() {
            observer.observe(&record);
        }
    }

    /// Hands `record` to every sink in order. Each append runs under panic
    /// containment and counts its own failures: a sink that fails or panics
    /// costs only its own delivery.
    pub(crate) fn append(&self, record: &TraceRecord) {
        #[cfg(feature = "perf-witness")]
        let _history_scope = lash_core_ids::perf_witness::history::Scope::enter(
            lash_core_ids::perf_witness::history::Component::TraceObserver,
        );
        for (sink_index, (sink, failures)) in
            self.sinks.iter().zip(self.sink_failures.iter()).enumerate()
        {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.append(record)));
            let failure = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(payload) => {
                    let message = crate::panic_containment::payload_message(payload.as_ref());
                    if crate::panic_containment::is_loud() {
                        std::panic::resume_unwind(payload);
                    }
                    Some(format!("the sink panicked: {message}"))
                }
            };
            let mut count = failures.lock_recover();
            match failure {
                Some(error) => {
                    *count = count.saturating_add(1);
                    if *count == 1 {
                        // A host subscriber can forward this diagnostic to a
                        // sink; release the counter before invoking its layer.
                        drop(count);
                        tracing::warn!(event = "trace_sink.degraded", sink_index, %error,
                            failure_count = 1_u64,
                            session_id = record.context.session_id.as_ref().map(|id| id.as_str()),
                            turn_id = record.context.turn_id.as_ref().map(|id| id.as_str()),
                            "trace sink append failed; records may be lost");
                    }
                }
                None if *count > 0 => {
                    let failure_count = std::mem::take(&mut *count);
                    drop(count);
                    tracing::info!(
                        event = "trace_sink.recovered",
                        sink_index,
                        failure_count,
                        "trace sink append recovered"
                    );
                }
                None => {}
            }
        }
    }

    /// The failures each sink has counted since its last success, in
    /// installation order.
    pub fn sink_failures(&self) -> Vec<u64> {
        self.sink_failures
            .iter()
            .map(|failures| *failures.lock_recover())
            .collect()
    }
}
