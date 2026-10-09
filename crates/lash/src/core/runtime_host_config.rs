use super::*;

impl LashCoreBuilder {
    /// Assemble the runtime host config over the backend, which supplies its
    /// effect host, attachment store, process-env store and clock, then apply
    /// every runtime setting this builder carries over it.
    pub(super) fn resolve_runtime_host_config(
        &mut self,
        data_retention: facade_support::DataRetentionConfig,
    ) -> Result<RuntimeHostConfig> {
        let commit_budget = self
            .commit_budget
            .take()
            .ok_or(EmbedError::MissingCommitBudget)?;
        let queued_work_batching = self
            .queued_work_batching
            .take()
            .ok_or(EmbedError::MissingQueuedWorkBatching)?;
        let tool_source_policy = self
            .tool_source_policy
            .take()
            .ok_or(EmbedError::MissingToolSourcePolicy)?;
        let execution_budgets = self
            .execution_budgets
            .take()
            .ok_or(EmbedError::MissingExecutionBudgets)?;
        let delta_coalescing = self
            .delta_coalescing
            .take()
            .ok_or(EmbedError::MissingDeltaCoalescing)?;
        let core = RuntimeHostConfig::new(
            self.backend.clone(),
            commit_budget,
            queued_work_batching,
            tool_source_policy,
            execution_budgets,
            delta_coalescing,
            data_retention,
        )
        .with_provider_file_cache(self.provider_file_cache);
        let core = self.apply_core_overrides(core);
        core.control.relay_policy().validate()?;
        core.control.commit_admission.validate()?;
        Ok(core)
    }

    fn apply_core_overrides(&mut self, mut core: RuntimeHostConfig) -> RuntimeHostConfig {
        core.durability.attachment_store = Arc::new(
            core.durability
                .attachment_store
                .reconfigured_reclamation_retry(self.attachment_reclamation_retry),
        );
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
        if let Some(sink) = self.trace_sink.take() {
            core.tracing = core.tracing.with_trace_sink(sink);
        }
        core.providers.delivery_fetch_horizon = self.delivery_fetch_horizon;
        core.observation_work_limits = self.observation_work_limits;
        if let Some(limits) = self.trace_limits.take() {
            core.tracing = core.tracing.clone().with_limits(limits);
        }
        if let Some(level) = self.trace_level.take() {
            core.tracing = core.tracing.clone().with_level(level);
        }
        if let Some(content) = self.telemetry_content.take() {
            core.tracing = core.tracing.clone().with_content(content);
        }
        if let Some(context) = self.trace_context.take() {
            core.tracing = core.tracing.clone().with_base_context(context);
        }
        if let Some(cuts) = self.output_cuts.take() {
            core.control.output_cuts = cuts;
        }
        core.control.prompt_render_pool = self.prompt_render_pool.take();
        // The host's delivery bound is the one relay-policy source: the
        // recovery pass's relays and every immediate `deliver_now` derive
        // theirs from it (FIG-4246).
        core.control.recovery_pass = self.recovery_pass;
        core.control.relay = self.relay_policy;
        core.control.commit_admission = self.commit_admission;
        core.control.pacing = self.runtime_pacing;
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
pub(super) mod tests {
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
    pub(in crate::core) struct Sink(pub(in crate::core) AtomicUsize);
    impl lash_trace::TraceSink for Sink {
        fn append(
            &self,
            _: &lash_trace::TraceRecord,
        ) -> std::result::Result<(), lash_trace::TraceSinkError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    /// FIG-5020: repeated sink and telemetry setters replace their own value.
    #[tokio::test]
    async fn trace_configuration_setters_replace_previous_values() {
        let backend = crate::tests::sqlite_memory_store_backend().await;
        let old_metrics = Arc::new(AtomicUsize::new(0));
        let metrics = Arc::new(AtomicUsize::new(0));
        let first = Arc::new(Sink(AtomicUsize::new(0)));
        let second = Arc::new(Sink(AtomicUsize::new(0)));
        let mut builder = crate::tests::explicit_ephemeral_facets(LashCore::builder(backend))
            .telemetry(telemetry(old_metrics.clone()))
            .trace_sink(first.clone())
            .telemetry(telemetry(metrics.clone()))
            .trace_sink(second.clone());
        let config = builder
            .resolve_runtime_host_config(facade_support::DataRetentionConfig::standard())
            .expect("trace config");
        config
            .tracing
            .metrics()
            .tool_intent
            .record_executed("start_process");
        assert_eq!(old_metrics.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.load(Ordering::Relaxed), 1);
        let scope = lash_trace::DurableTraceScope {
            scope: lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Turn {
                session_id: "s".into(),
                turn_id: "t".into(),
            }),
            cause: lash_trace::TraceCause::Root,
            anchor: lash_trace::TraceAnchor::Untraced,
            started_at_ms: 1,
        };
        config.tracing.emitter().emit(
            Some(&lash_trace::EmissionPermit::new_transition()),
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
        assert_eq!(first.0.load(Ordering::Relaxed), 0);
        assert_eq!(second.0.load(Ordering::Relaxed), 1);
    }
    /// FIG-5499: the facade's uploader installer respects cache capacity.
    #[tokio::test]
    async fn uploader_installation_honors_facade_cache_limits() {
        use crate::attachments::AttachmentCreateMeta;
        use crate::attachments::{
            DeliveryLimits, DeliverySecret, ProviderAccepts, ProviderFileScope,
        };
        use crate::persistence::{
            AttachmentStoreError, ProviderFileUploader, UploadedProviderFile,
        };
        struct Uploader {
            scope: ProviderFileScope,
            calls: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl ProviderFileUploader for Uploader {
            fn scope(&self) -> &ProviderFileScope {
                &self.scope
            }
            async fn upload(
                &self,
                _: &crate::attachments::AttachmentRef,
                _: &[u8],
            ) -> std::result::Result<UploadedProviderFile, AttachmentStoreError> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                Ok(UploadedProviderFile {
                    id: DeliverySecret::new("file".into()),
                    valid_until_ms: None,
                })
            }
        }
        let uploader = Arc::new(Uploader {
            scope: ProviderFileScope {
                provider: "test".into(),
                endpoint: "test".into(),
                credential_scope: "host".into(),
            },
            calls: AtomicUsize::new(0),
        });
        let backend = crate::tests::sqlite_memory_store_backend().await;
        let mut builder = crate::tests::explicit_ephemeral_facets(LashCore::builder(backend))
            .provider_file_cache(crate::persistence::ProviderFileCacheLimits {
                capacity: 0,
                ..Default::default()
            });
        let config = builder
            .resolve_runtime_host_config(facade_support::DataRetentionConfig::standard())
            .unwrap()
            .with_provider_file_uploaders(vec![uploader.clone()]);
        let store = &config.durability.attachment_store;
        let reference = store
            .put(
                b"original".to_vec(),
                AttachmentCreateMeta::new("text/plain".parse().unwrap(), None, None),
            )
            .await
            .unwrap();
        let accepts = ProviderAccepts {
            provider_file: Some(uploader.scope.clone()),
            ..Default::default()
        };
        for _ in 0..2 {
            store
                .backend()
                .deliver(
                    &reference,
                    &accepts,
                    &DeliveryLimits {
                        max_bytes: 1024,
                        max_upload_bytes: 1024,
                        valid_through_ms: 0,
                    },
                )
                .await
                .unwrap();
        }
        assert_eq!(
            uploader.calls.load(Ordering::Relaxed),
            2,
            "zero capacity uploads again rather than reusing a derivative"
        );
    }
}

#[cfg(test)]
mod pacing_laws {
    use super::*;
    use std::time::Duration;

    fn builder(backend: lash_core::Backend) -> LashCoreBuilder {
        crate::tests::explicit_ephemeral_facets(LashCore::standard_builder(backend))
            .serve_sessions(false)
    }

    /// D-DEFAULTS2: the facade's work cadence controls the actual external
    /// process waiter, including its exponential maximum.
    #[tokio::test]
    async fn facade_work_cadence_controls_terminal_polling() {
        let backend = crate::tests::sqlite_memory_store_backend().await;
        let raw = backend.process_registry();
        let registered = raw
            .register_process(crate::testing::held_engine_registration(
                serde_json::json!({}),
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            ))
            .await
            .expect("register held process");
        let faults = Arc::new(lash_core::testing::ProcessRegistryFaults::new(raw));
        faults.set_process_read_pinned(Some(registered.clone()));
        let backend = crate::testing::LayeredBackend::over(backend)
            .map_process_registry(|_| faults.clone())
            .into_backend();
        let core = builder(backend)
            .work_cadence(crate::WorkCadencePolicy {
                poll_initial: Duration::from_secs(2),
                poll_max: Duration::from_secs(3),
            })
            .build(crate::testing::runtime_lease_owner())
            .expect("configured core");
        core.shutdown().await.expect("stop background tasks");
        tokio::time::pause();
        let port = core.substrate_slot.setup.process.port();
        let mut wait = Box::pin(port.await_process_terminal(&registered.id));
        assert!(futures_util::poll!(&mut wait).is_pending());
        let reads = faults.process_point_reads();
        tokio::time::advance(Duration::from_millis(1999)).await;
        assert!(futures_util::poll!(&mut wait).is_pending());
        assert_eq!(
            faults.process_point_reads(),
            reads,
            "no read before the configured floor"
        );
        // Tokio rounds timer deadlines up to its millisecond boundary.
        tokio::time::advance(Duration::from_millis(2)).await;
        assert!(futures_util::poll!(&mut wait).is_pending());
        assert_eq!(faults.process_point_reads(), reads + 1);
        tokio::time::advance(Duration::from_millis(2999)).await;
        assert!(futures_util::poll!(&mut wait).is_pending());
        assert_eq!(faults.process_point_reads(), reads + 1);
        // Tokio rounds timer deadlines up to its millisecond boundary.
        tokio::time::advance(Duration::from_millis(2)).await;
        assert!(futures_util::poll!(&mut wait).is_pending());
        assert_eq!(faults.process_point_reads(), reads + 2);
    }
    /// D-DEFAULTS2: both kinds of recovery delivery use the configured retry
    /// shape and the most recently selected attempt budget.
    #[tokio::test]
    async fn facade_relay_policy_reaches_delivery_and_budget_resolution() {
        use lash_core::runtime::obligations::relay::ObligationRelay;
        let backend = crate::tests::sqlite_memory_store_backend().await;
        let policy = crate::RelayPolicy {
            base_backoff_ms: 17,
            max_backoff_ms: 50,
            attempt_ceiling: std::num::NonZeroU32::MIN.saturating_add(3),
            claim_ttl_ms: 100,
            attempt_budget_ms: 10,
        };
        let mut configured = builder(backend.clone())
            .relay_policy(policy)
            .recovery_pass_budget(crate::RecoveryPassBudget {
                attempt: Duration::from_millis(21),
            });
        let resolved = configured
            .resolve_runtime_host_config(facade_support::DataRetentionConfig::standard())
            .expect("valid policy");
        let relay = lash_core::runtime::artifact_cleanup::ArtifactCleanupRelay::over_backend(
            &backend,
            resolved.process_engines.clone(),
        )
        .with_policy(resolved.control.relay_policy());
        assert_eq!(relay.policy().backoff_ms(2), 34);
        assert_eq!(relay.policy().backoff_ms(3), 50);
        assert_eq!(relay.policy().claim_ttl_ms, 100);
        assert_eq!(relay.policy().attempt_ceiling.get(), 4);
        assert_eq!(relay.policy().attempt_budget_ms, 21);
        let mut reversed = builder(backend.clone())
            .recovery_pass_budget(crate::RecoveryPassBudget {
                attempt: Duration::from_millis(21),
            })
            .relay_policy(policy);
        assert_eq!(
            reversed
                .resolve_runtime_host_config(facade_support::DataRetentionConfig::standard())
                .expect("policy")
                .control
                .relay_policy()
                .attempt_budget_ms,
            10
        );
        let mut invalid = builder(backend).relay_policy(crate::RelayPolicy {
            claim_ttl_ms: 10,
            ..policy
        });
        assert!(matches!(
            invalid.resolve_runtime_host_config(facade_support::DataRetentionConfig::standard()),
            Err(EmbedError::RelayPolicy(_))
        ));
    }

    /// D-DEFAULTS2: each caller's facade admission limits govern the shared
    /// session FIFO; a queued attempt never executes before admission.
    #[tokio::test]
    async fn facade_commit_admission_enforces_capacity_and_ttl() {
        use lash_core::StoreError;
        use lash_core::runtime::run_head_advancing_commit_attempt;
        use tokio_util::sync::CancellationToken;
        let backend = crate::tests::sqlite_memory_store_backend().await;
        let selected = crate::CommitAdmissionPolicy {
            max_waiters: std::num::NonZeroUsize::MIN,
            wait_ttl: Duration::from_millis(7),
        };
        let mut configured = builder(backend.clone()).commit_admission(selected);
        let policy = configured
            .resolve_runtime_host_config(facade_support::DataRetentionConfig::standard())
            .expect("valid admission")
            .control
            .commit_admission;
        tokio::time::pause();
        let mut active = Box::pin(run_head_advancing_commit_attempt(
            "facade-admission",
            "active",
            CancellationToken::new(),
            policy,
            |_, _| std::future::pending::<std::result::Result<(), StoreError>>(),
        ));
        assert!(futures_util::poll!(&mut active).is_pending());
        let mut waiting = Box::pin(run_head_advancing_commit_attempt(
            "facade-admission",
            "waiting",
            CancellationToken::new(),
            policy,
            |_, _| async { panic!("expired work must not execute") },
        ));
        assert!(futures_util::poll!(&mut waiting).is_pending());
        let refused = run_head_advancing_commit_attempt(
            "facade-admission",
            "excess",
            CancellationToken::new(),
            policy,
            |_, _| async { Ok::<(), StoreError>(()) },
        )
        .await;
        assert!(matches!(refused, Err(StoreError::Contended)));
        tokio::time::advance(Duration::from_millis(6)).await;
        assert!(futures_util::poll!(&mut waiting).is_pending());
        tokio::time::advance(Duration::from_millis(1)).await;
        let expired: std::result::Result<(), StoreError> = waiting.await;
        assert!(matches!(expired, Err(StoreError::Contended)));
        drop(active);
        let mut invalid = builder(backend).commit_admission(crate::CommitAdmissionPolicy {
            wait_ttl: Duration::ZERO,
            ..selected
        });
        assert!(matches!(
            invalid.resolve_runtime_host_config(facade_support::DataRetentionConfig::standard()),
            Err(EmbedError::CommitAdmissionPolicy(_))
        ));
    }

    /// D-DEFAULTS2: durable and open session observers retain the configured
    /// schedules and buffers instead of reinstalling defaults when binding.
    #[tokio::test]
    async fn facade_observer_pacing_survives_durable_and_live_binding() {
        let backend = crate::tests::sqlite_memory_store_backend().await;
        let schedule = crate::PollPacing::new(Duration::from_millis(37), Duration::from_millis(80))
            .expect("pacing");
        let pacing = crate::ObserverPacing {
            follow: schedule,
            admin: schedule,
            deletion: schedule,
            terminal: schedule,
            follow_buffer: std::num::NonZeroUsize::MIN.saturating_add(2),
            send_channel: std::num::NonZeroUsize::MIN.saturating_add(1),
            snapshot_read_attempts: std::num::NonZeroUsize::MIN.saturating_add(3),
        };
        let core = builder(backend)
            .serve_test_llm_profile(
                crate::testing::TestProvider::default().into_handle(),
                crate::tests::mock_llm_profile_spec(),
            )
            .observer_pacing(pacing)
            .build(crate::testing::runtime_lease_owner())
            .expect("core");
        let id = lash_core::SessionId::from("pacing-binding");
        let durable = core
            .session(id.clone())
            .create(crate::SessionCreation::root(
                lash_core::SessionToolAccess::ambient(),
                crate::tests::mock_session_spec(),
            ))
            .await
            .expect("create");
        assert_eq!(
            *durable.send_parts().await.expect("parts").observer_pacing,
            pacing
        );
        let live = core.session(id).open().await.expect("open");
        assert_eq!(
            *live
                .durable()
                .send_parts()
                .await
                .expect("live parts")
                .observer_pacing,
            pacing
        );
        assert_eq!(live.admin().target.observer_pacing(), pacing);
        assert_eq!(
            pacing.follow.next(pacing.follow.initial()),
            Duration::from_millis(74)
        );
        assert_eq!(
            pacing.admin.next(Duration::from_millis(74)),
            Duration::from_millis(80)
        );
        assert_eq!(
            core.observer_pacing.deletion.initial(),
            Duration::from_millis(37)
        );
        core.shutdown().await.expect("shutdown");
    }

    /// D-DEFAULTS2: runtime chunks and tool-fault waits are resolved at the
    /// facade rather than selected anew by execution contexts.
    #[tokio::test]
    async fn facade_runtime_pacing_controls_chunks_and_fault_backoff() {
        let backend = crate::tests::sqlite_memory_store_backend().await;
        let retry = crate::PollPacing::new(Duration::from_millis(3), Duration::from_millis(11))
            .expect("retry");
        let mut configured = builder(backend).runtime_pacing(crate::RuntimePacingPolicy {
            tool_fault_retry: retry,
            checkpoint_inputs: std::num::NonZeroUsize::MIN.saturating_add(2),
        });
        let resolved = configured
            .resolve_runtime_host_config(facade_support::DataRetentionConfig::standard())
            .expect("runtime policy");
        assert_eq!(resolved.control.pacing.checkpoint_inputs.get(), 3);
        assert_eq!(
            resolved.control.pacing.tool_fault_retry.after_faults(1),
            Duration::from_millis(6)
        );
        assert_eq!(
            resolved.control.pacing.tool_fault_retry.after_faults(2),
            Duration::from_millis(11)
        );
    }

    /// D-DEFAULTS2: a non-default attachment write-fence bound reaches the
    /// facade put, including the binding and unbound-store copies it traverses.
    #[tokio::test]
    async fn facade_attachment_retry_bounds_a_reclamation_fence() {
        let backend = crate::tests::sqlite_memory_store_backend().await;
        let retry = crate::persistence::AttachmentReclamationRetryPolicy::new(
            std::num::NonZeroU32::MIN.saturating_add(1),
            0,
            Duration::from_millis(3),
            Duration::from_millis(5),
        )
        .expect("retry policy");
        let core = builder(backend.clone())
            .serve_test_llm_profile(
                crate::testing::TestProvider::default().into_handle(),
                crate::tests::mock_llm_profile_spec(),
            )
            .attachment_reclamation_retry(retry)
            .build(crate::testing::runtime_lease_owner())
            .expect("core");
        let id = lash_core::SessionId::from("retry-fence");
        core.session(id.clone())
            .create(crate::SessionCreation::root(
                lash_core::SessionToolAccess::ambient(),
                crate::tests::mock_session_spec(),
            ))
            .await
            .expect("create");
        let live = core.session(id).open().await.expect("open");
        let meta = lash_core::AttachmentCreateMeta::new(
            lash_core::MediaType::parse("text/plain").expect("media type"),
            None,
            None,
        );
        let bytes = b"held for physical deletion".to_vec();
        let blob = backend
            .attachment_store()
            .put(bytes.clone(), meta.clone())
            .await
            .expect("raw blob");
        let roots = backend.session_store_factory();
        let sweep = roots.begin_attachment_sweep().await.expect("sweep");
        roots
            .condemn_attachment(&blob.id, &sweep)
            .await
            .expect("condemn");
        roots
            .arm_attachment_delete(&blob.id, &sweep)
            .await
            .expect("arm deletion");
        assert!(matches!(
            live.put_attachment(bytes, meta).await,
            Err(lash_core::AttachmentStoreError::ReclamationInFlight { attempts: 2, .. })
        ));
        core.shutdown().await.expect("shutdown");
    }

    struct ClaimProbe {
        inner: Arc<dyn lash_core::store::ArtifactCleanupLedger>,
        claims: tokio::sync::mpsc::UnboundedSender<(u64, usize)>,
    }
    #[async_trait::async_trait]
    impl lash_core::store::ObligationLedger for ClaimProbe {
        fn kind(&self) -> lash_core::store::ObligationKind {
            self.inner.kind()
        }
        async fn claim_due(
            &self,
            now_ms: u64,
            ttl: u64,
            limit: std::num::NonZeroUsize,
        ) -> std::result::Result<Vec<lash_core::store::ClaimedObligation>, lash_core::StoreError>
        {
            let rows = self.inner.claim_due(now_ms, ttl, limit).await?;
            let _ = self.claims.send((ttl, limit.get()));
            Ok(rows)
        }
        async fn claim(
            &self,
            id: &lash_core::store::ObligationId,
            token: &lash_core::store::ClaimToken,
            now: u64,
            ttl: u64,
        ) -> std::result::Result<Option<lash_core::store::ClaimedObligation>, lash_core::StoreError>
        {
            self.inner.claim(id, token, now, ttl).await
        }
        async fn settle(
            &self,
            id: &lash_core::store::ObligationId,
            token: &lash_core::store::ClaimToken,
            settlement: lash_core::store::ObligationSettlement,
            now: u64,
        ) -> std::result::Result<lash_core::store::SettleOutcome, lash_core::StoreError> {
            self.inner.settle(id, token, settlement, now).await
        }
        async fn rearm(
            &self,
            id: &lash_core::store::ObligationId,
            now: u64,
        ) -> std::result::Result<bool, lash_core::StoreError> {
            self.inner.rearm(id, now).await
        }
        async fn list_stalled(
            &self,
            after: Option<&lash_core::store::ObligationId>,
            limit: std::num::NonZeroUsize,
        ) -> std::result::Result<Vec<lash_core::store::StalledObligation>, lash_core::StoreError>
        {
            self.inner.list_stalled(after, limit).await
        }
        async fn count_stalled(&self) -> std::result::Result<u64, lash_core::StoreError> {
            self.inner.count_stalled().await
        }
        async fn standing(
            &self,
            id: &lash_core::store::ObligationId,
        ) -> std::result::Result<Option<lash_core::store::ObligationStanding>, lash_core::StoreError>
        {
            self.inner.standing(id).await
        }
    }

    #[async_trait::async_trait]
    impl lash_core::store::ArtifactCleanupLedger for ClaimProbe {
        async fn arm_cleanup(
            &self,
            cleanup: &crate::persistence::ArtifactCleanup,
            now: u64,
        ) -> std::result::Result<lash_core::store::ObligationId, lash_core::StoreError> {
            self.inner.arm_cleanup(cleanup, now).await
        }
        async fn nudge(
            &self,
            referrer: &lash_core::ArtifactReferrer,
            now: u64,
        ) -> std::result::Result<bool, lash_core::StoreError> {
            self.inner.nudge(referrer, now).await
        }
        async fn load_cleanup(
            &self,
            id: &lash_core::store::ObligationId,
        ) -> std::result::Result<Option<crate::persistence::ArtifactCleanup>, lash_core::StoreError>
        {
            self.inner.load_cleanup(id).await
        }
    }

    /// D-DEFAULTS2: the background cleanup task consumes the facade's page,
    /// claim TTL and interval. The stock ten-second grid cannot pass this law.
    #[tokio::test]
    async fn facade_recovery_pacing_controls_the_background_due_pass() {
        let backend = crate::tests::sqlite_memory_store_backend().await;
        let (claims, mut received) = tokio::sync::mpsc::unbounded_channel();
        let backend = crate::testing::LayeredBackend::over(backend)
            .map_artifact_cleanup(move |inner| Arc::new(ClaimProbe { inner, claims }))
            .into_backend();
        let core = builder(backend)
            .recovery_pacing(
                crate::RecoveryPacing::new(Duration::from_millis(30), std::num::NonZeroUsize::MIN)
                    .expect("recovery pacing"),
            )
            .relay_policy(crate::RelayPolicy {
                claim_ttl_ms: 70_000,
                ..crate::RelayPolicy::standard()
            })
            .build(crate::testing::runtime_lease_owner())
            .expect("core");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), received.recv())
                .await
                .expect("initial due pass"),
            Some((70_000, 1))
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), received.recv())
                .await
                .expect("configured next pass"),
            Some((70_000, 1))
        );
        core.shutdown().await.expect("shutdown");
    }
    struct FailingDelete(Arc<dyn crate::persistence::AttachmentStore>);
    #[async_trait::async_trait]
    impl crate::persistence::AttachmentStore for FailingDelete {
        async fn put(
            &self,
            bytes: Vec<u8>,
            meta: lash_core::AttachmentCreateMeta,
        ) -> std::result::Result<lash_core::AttachmentRef, lash_core::AttachmentStoreError>
        {
            self.0.put(bytes, meta).await
        }
        async fn get(
            &self,
            id: &lash_core::AttachmentId,
            max: u64,
        ) -> std::result::Result<lash_core::StoredAttachment, lash_core::AttachmentStoreError>
        {
            self.0.get(id, max).await
        }
        async fn delete(
            &self,
            _: &lash_core::AttachmentId,
        ) -> std::result::Result<(), lash_core::AttachmentStoreError> {
            Err(lash_core::AttachmentStoreError::Backend {
                operation: "delete",
                class: lash_core::AttachmentStoreFailureClass::Transient,
                source: "retryable fixture delete".into(),
            })
        }
        async fn list(
            &self,
        ) -> std::result::Result<Vec<lash_core::StoredBlobRef>, lash_core::AttachmentStoreError>
        {
            // Keep freshness outside the grace window without a wall-clock sleep.
            let mut blobs = self.0.list().await?;
            for blob in &mut blobs {
                blob.last_modified_epoch_ms = Some(0);
            }
            Ok(blobs)
        }
        async fn head(
            &self,
            id: &lash_core::AttachmentId,
        ) -> std::result::Result<Option<lash_core::StoredBlobRef>, lash_core::AttachmentStoreError>
        {
            let mut blob = self.0.head(id).await?;
            if let Some(blob) = &mut blob {
                blob.last_modified_epoch_ms = Some(0);
            }
            Ok(blob)
        }
    }

    /// D-DEFAULTS2: the facade's optional physical-delete retry limit affects
    /// the retained stall fact; a failure before the stock five attempts can stall.
    #[tokio::test]
    async fn facade_attachment_delete_limit_changes_stall_admission() {
        use crate::persistence::AttachmentStore;
        let backend = crate::tests::sqlite_memory_store_backend().await;
        let blobs = FailingDelete(backend.attachment_store());
        let blob = blobs
            .put(
                b"unreferenced retry candidate".to_vec(),
                lash_core::AttachmentCreateMeta::new(
                    lash_core::MediaType::parse("text/plain").expect("media"),
                    None,
                    None,
                ),
            )
            .await
            .expect("raw unreferenced blob");
        let roots = backend.session_store_factory();
        let policy = crate::persistence::AttachmentReclamationPolicy::new(
            0,
            crate::persistence::EmptyRootSetPolicy::AuthorizeDeleteAll,
        )
        .with_delete_attempt_limit(std::num::NonZeroU32::MIN);
        let report =
            crate::persistence::reclaim_unreferenced_attachments(roots.as_ref(), &blobs, policy)
                .await
                .expect("completed failing sweep");
        assert_eq!(report.stalled_ids, vec![blob.id.clone()]);
        let rows = roots.list_condemnations().await.expect("retained stalls");
        assert_eq!(rows[0].delete_attempts, 1);
        assert_eq!(
            rows[0].stalled,
            Some(lash_core::AttachmentDeleteStallReason::AttemptsExhausted)
        );
    }
}
