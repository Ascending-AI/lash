use super::*;

impl LashCoreBuilder {
    /// Assemble the runtime host config over the backend, which supplies its
    /// effect host, attachment store, process-env store and clock, then apply
    /// every runtime setting this builder carries over it.
    pub(super) fn resolve_runtime_host_config(&mut self) -> Result<RuntimeHostConfig> {
        let commit_budget = self
            .commit_budget
            .take()
            .ok_or(EmbedError::MissingCommitBudget)?;
        let queued_work_batching = self
            .queued_work_batching
            .take()
            .ok_or(EmbedError::MissingQueuedWorkBatching)?;
        let mut core =
            RuntimeHostConfig::new(self.backend.clone(), commit_budget, queued_work_batching);
        // The backend's process registry owns the lifetime scopes the drive
        // closes: a root's end and a session's close write its scope-close
        // rows (FIG-3607 item 7) and, over the engine's process port, apply
        // the plan each row records (FIG-3822).
        let process_work = self.backend.process_work();
        core.control.scope_close = Arc::new(
            (if process_work.runs_processes() {
                lash_core::RegistryScopeClose::with_delivery(
                    self.backend.process_registry(),
                    std::sync::Arc::clone(process_work.port()),
                    self.backend.clock(),
                )
            } else {
                lash_core::RegistryScopeClose::new(
                    self.backend.process_registry(),
                    self.backend.clock(),
                )
            })
            .with_effect_host(self.backend.effect_host())
            .with_session_store_factory(self.backend.session_store_factory()),
        );
        Ok(self.apply_core_overrides(core))
    }

    fn apply_core_overrides(&mut self, mut core: RuntimeHostConfig) -> RuntimeHostConfig {
        if let Some(max) = self.max_attachment_bytes.take() {
            core = core.with_max_attachment_bytes(max);
        }
        if let Some(policy) = self.attachment_read_policy.take() {
            core = core.with_attachment_read_policy(policy);
        }
        if let Some(expiry) = self.attachment_upload_expiry.take() {
            core = core.with_attachment_upload_expiry_ms(
                u64::try_from(expiry.as_millis()).unwrap_or(u64::MAX),
            );
        }
        if let Some(policy) = self.output_retention.take() {
            core = core.with_output_retention(policy);
        }
        if let Some(policy) = self.process_wake_delivery_policy.take() {
            core.control.process_wake_delivery_policy = policy;
        }
        if let Some(runtime) = self.trace_runtime.take() {
            core.tracing = runtime;
        }
        #[cfg(feature = "otel-trace")]
        if let Some(telemetry) = self.telemetry.take() {
            let metrics = telemetry.metrics().clone();
            let adapter = Arc::new(telemetry);
            core.tracing = core
                .tracing
                .with_scopes(adapter.clone())
                .with_projector(adapter)
                .with_metrics(metrics);
        }
        for sink in self.trace_sinks.drain(..) {
            core.tracing = core.tracing.with_trace_sink(sink);
        }
        if let Some(level) = self.trace_level.take() {
            core.tracing = core.tracing.clone().with_level(level);
        }
        if let Some(context) = self.trace_context.take() {
            core.tracing = core.tracing.clone().with_base_context(context);
        }
        if let Some(termination) = self.termination.take() {
            core.control.termination = termination;
        }
        if let Some(policy) = self.tool_source_policy.take() {
            core.control.tool_source_policy = policy;
        }
        if let Some(grace) = self.abort_drain_grace.take() {
            core.control.abort_drain_grace = grace;
        }
        // The host's delivery bound is the one relay-policy source: the
        // recovery pass's relays and every immediate `deliver_now` derive
        // theirs from it (FIG-4246).
        core.control.recovery_pass = self.recovery_pass;
        core.control.trigger_route_restorer = self.trigger_route_restorer.take();
        if let Some(models) = self.models.clone() {
            core.providers.models = models;
        }
        core.providers.run_definitions = self.run_definitions.clone();
        if let Some(filter) = self.process_tool_visibility_filter.take() {
            core.control.process_tool_visibility_filter = Some(filter);
        }
        core
    }
}

#[cfg(all(test, feature = "otel-trace"))]
mod tests {
    use super::*;
    use lash_trace::otel::{OtelOptions, OtelTelemetry, api};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MeterProvider(Arc<AtomicUsize>);
    struct CounterSpy(Arc<AtomicUsize>);
    impl api::metrics::SyncInstrument<u64> for CounterSpy {
        fn measure(&self, _: u64, _: &[api::KeyValue]) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    impl api::metrics::MeterProvider for MeterProvider {
        fn meter_with_scope(&self, _: api::InstrumentationScope) -> api::metrics::Meter {
            struct Instruments(Arc<AtomicUsize>);
            impl api::metrics::InstrumentProvider for Instruments {
                fn u64_counter(
                    &self,
                    _: api::metrics::InstrumentBuilder<'_, api::metrics::Counter<u64>>,
                ) -> api::metrics::Counter<u64> {
                    api::metrics::Counter::new(Arc::new(CounterSpy(self.0.clone())))
                }
            }
            api::metrics::Meter::new(Arc::new(Instruments(self.0.clone())))
        }
    }
    fn telemetry(measurements: Arc<AtomicUsize>) -> OtelTelemetry {
        OtelTelemetry::new(
            &api::trace::noop::NoopTracerProvider::new(),
            &MeterProvider(measurements),
            OtelOptions::default(),
        )
    }
    struct Sink(AtomicUsize);
    impl lash_trace::TraceSink for Sink {
        fn append(
            &self,
            _: &lash_trace::TraceRecord,
        ) -> std::result::Result<(), lash_trace::TraceSinkError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[tokio::test]
    async fn telemetry_installation_preserves_passive_sinks_and_refuses_a_second_adapter() {
        let backend = crate::tests::double_backend().await;
        let measurements = Arc::new(AtomicUsize::new(0));
        let duplicate = LashCore::builder(backend.clone())
            .telemetry(telemetry(measurements.clone()))
            .telemetry(telemetry(measurements.clone()))
            .build(crate::testing::runtime_lease_owner());
        let error = match duplicate {
            Err(error) => error,
            Ok(_) => panic!("a second telemetry adapter was accepted"),
        };
        assert!(matches!(error, EmbedError::DuplicateTelemetry));
        assert!(error.is_terminal());
        assert!(!error.is_retryable());
        let first = Arc::new(Sink(AtomicUsize::new(0)));
        let second = Arc::new(Sink(AtomicUsize::new(0)));
        let retained = Arc::new(Sink(AtomicUsize::new(0)));
        let mut builder = crate::tests::explicit_ephemeral_facets(LashCore::builder(backend))
            .trace_runtime(
                lash_core::trace::TraceRuntime::default().with_trace_sink(retained.clone()),
            )
            .trace_sink(first.clone())
            .telemetry(telemetry(measurements.clone()))
            .trace_sink(second.clone());
        let config = builder
            .resolve_runtime_host_config()
            .expect("telemetry config");
        config
            .tracing
            .metrics()
            .tool_intent
            .record_executed("start_process");
        assert_eq!(measurements.load(Ordering::Relaxed), 1);
        let scope = lash_trace::DurableTraceScope {
            scope: lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Turn {
                session_id: "s".into(),
                turn_id: "t".into(),
            }),
            cause: lash_trace::TraceCause::Root,
            anchor: lash_trace::TraceAnchor::Untraced,
            started_at_ms: 1,
        };
        for runtime in [&config.tracing, &lash_core::trace::TraceRuntime::default()] {
            runtime.emitter().emit(
                None,
                &scope,
                None,
                || panic!("denied emission built an identity"),
                2,
                || panic!("denied emission built a record"),
            );
        }
        lash_core::trace::TraceRuntime::default().emitter().emit(
            Some(&lash_trace::EmissionPermit::new_transition()),
            &scope,
            None,
            || panic!("unobserved emission built an identity"),
            2,
            || panic!("unobserved emission built a record"),
        );
        let permit = lash_trace::EmissionPermit::new_transition();
        config.tracing.emitter().emit(
            Some(&permit),
            &scope,
            None,
            || lash_trace::TraceRecordIdentity::Transition {
                scope: scope.scope.clone(),
                transition: lash_trace::TraceTransitionKind::Terminal,
                ordinal: 0,
            },
            2,
            || {
                (
                    Default::default(),
                    lash_trace::TraceEvent::TurnStarted {
                        metadata: Default::default(),
                    },
                )
            },
        );
        assert_eq!(retained.0.load(Ordering::Relaxed), 1);
        assert_eq!(first.0.load(Ordering::Relaxed), 1);
        assert_eq!(second.0.load(Ordering::Relaxed), 1);
    }
}
