mod tests {
    use std::sync::Arc;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::ProcessEffectOutcome;

    /// FIG-5615: effect reads are the same canonical observation as the roster,
    /// including typed outcomes, cancellation evidence and actor parks.
    #[tokio::test]
    async fn process_effect_reads_carry_the_canonical_observation() {
        let backend = crate::support::sqlite_memory_store_backend().await;
        let registry = backend.process_registry();
        let env_store = backend.process_env_store();
        let executor = || {
            crate::RuntimeEffectLocalExecutor::processes(
                Arc::clone(&registry),
                Arc::new(crate::NoProcessWork::for_registry(Arc::clone(&registry))),
                crate::testing::process_engine_fixture(),
                crate::runtime::HostStartAdmission::default(),
            )
            .with_process_env_store(Arc::clone(&env_store))
            .with_process_actor_parks(Arc::clone(backend.durable()))
        };
        let envelope = start_envelope(
            env_store.as_ref(),
            "observed-effects",
            engine_registration("observed-effects", "valid"),
            lash_execution_contracts::testing::process_env_with_limit(1024),
        )
        .await;
        let started: lash_execution_contracts::process::ObservedProcess =
            started_record(execute_start(envelope, executor()).await.expect("start"));
        let observer =
            lash_execution_contracts::process::ProcessWorkObserver::new(Arc::clone(&registry))
                .with_actor_parks(Arc::clone(backend.durable()));
        assert_eq!(
            Some(started.clone()),
            observer.process(&started.process_id).await.expect("read")
        );
        let receiver = crate::ExecutionScope::runtime_operation("observed-effects");
        let crate::ProcessEffectOutcome::List { entries } = executor()
            .into_process()
            .expect("executor")
            .execute(
                &receiver,
                crate::ProcessCommand::List {
                    selection: crate::ProcessListSelection::HostRunning,
                },
            )
            .await
            .expect("list")
        else {
            panic!("list outcome")
        };
        assert_eq!(entries, vec![started.clone()]);
        let crate::ProcessEffectOutcome::Cancel { record: cancelled } = executor()
            .into_process()
            .expect("executor")
            .execute(
                &receiver,
                crate::ProcessCommand::Cancel {
                    process_id: started.process_id.clone(),
                    origin: crate::CancelOrigin::OperatorRequested,
                    requester: "observed-effects".into(),
                    attribution: None,
                },
            )
            .await
            .expect("cancel")
        else {
            panic!("cancel outcome")
        };
        assert_eq!(
            Some(*cancelled.clone()),
            observer.process(&started.process_id).await.expect("read")
        );
        assert_eq!(
            cancelled.cancel_request.expect("cancel evidence").origin,
            crate::CancelOrigin::OperatorRequested
        );
        // A terminal list preserves the typed output, rather than flattening status.
        let output = crate::ToolCallOutput::success(serde_json::json!({"answer": 42}));
        registry
            .complete_process(
                &started.process_id,
                lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
                    output.clone(),
                ),
                lash_execution_contracts::process::ProcessCompletionAuthority::workflow_key(
                    &started.process_id,
                ),
            )
            .await
            .expect("terminal");
        let session_id = lash_kernel_value::identity::SessionId::from("observed-effects");
        registry
            .add_observer(
                &session_id,
                &started.process_id,
                lash_execution_contracts::process::ProcessObserverBy::host("law"),
            )
            .await
            .expect("observer");
        let crate::ProcessEffectOutcome::List { entries } = executor()
            .into_process()
            .expect("executor")
            .execute(
                &receiver,
                crate::ProcessCommand::List {
                    selection: crate::ProcessListSelection::Observed {
                        session_scope: lash_execution_contracts::process::SessionScope::new(
                            session_id,
                        ),
                        mode: lash_execution_contracts::process::ProcessListMode::All,
                    },
                },
            )
            .await
            .expect("terminal list")
        else {
            panic!("list outcome")
        };
        assert_eq!(
            entries[0]
                .terminal()
                .expect("typed terminal")
                .clone()
                .into_await_output()
                .into_tool_output(),
            output
        );
    }

    #[tokio::test]
    async fn a_journaled_environment_load_keeps_its_bytes_after_the_source_pin_ends() {
        let backend = crate::support::sqlite_memory_store_backend().await;
        let store = backend.process_env_store();
        let pin = lash_execution_contracts::testing::host_pin_claim_for_testing();
        let spec = lash_execution_contracts::testing::process_env_with_limit(1024);
        let env_ref = lash_core_execution::testing::publish_process_env_for_testing(
            store.as_ref(),
            &pin,
            &spec,
        )
        .await
        .expect("source publication");
        let scope = crate::ExecutionScope::runtime_operation("load-retention-law");
        let outcome = crate::RuntimeEffectLocalExecutor::execution_env_load(
            Arc::clone(&store),
            "retained load",
        )
        .execute(crate::RuntimeEffectEnvelope::new(
            crate::RuntimeEffectInvocation::new(
                crate::EffectAddress::new(scope.clone(), "load").expect("address"),
                crate::RuntimeAttribution::none(),
                "load",
            ),
            crate::RuntimeEffectCommand::LoadExecutionEnv {
                env: env_ref.clone(),
            },
        ))
        .await
        .expect("recorded load");
        assert_eq!(
            outcome.into_execution_env_ref().expect("recorded digest"),
            env_ref
        );
        store
            .end_process_env_referrer(&end(pin.referrer()))
            .await
            .expect("source pin ends");
        assert_eq!(
            lash_execution_contracts::process::load_process_env(store.as_ref(), &env_ref)
                .await
                .expect("replay resolves bytes"),
            spec
        );
        store
            .end_process_env_referrer(&end(crate::ArtifactReferrer::Execution(
                scope.journal_identity().expect("journal"),
            )))
            .await
            .expect("load journal ends");
        assert_eq!(
            store
                .get_process_env(&env_ref)
                .await
                .expect("read reclaimed bytes"),
            None
        );
    }

    #[tokio::test]
    async fn local_start_admits_payload_and_stamps_engine_identity() {
        let backend = crate::support::sqlite_memory_store_backend().await;
        let registry = backend.process_registry();
        let env_store = backend.process_env_store();
        let engines = crate::ProcessEngineRegistry::new().with_registration(
            crate::ProcessEngineRegistration::new(
                Arc::new(crate::testing::FixtureProcessEngine),
                lash_execution_contracts::process::ProcessEngineAdmission::new("testing-fixture", |kind, payload| {
                    if payload["marker"] == "invalid" {
                        return Err(lash_execution_contracts::plugin_error::PluginError::Session(
                            "invalid fixture payload".to_owned(),
                        ));
                    }
                    if payload["marker"] == "missing-config" {
                        return Err(lash_execution_contracts::plugin_error::PluginError::MissingRecordedProcessConfig {
                            engine_kind: kind.to_owned(),
                        });
                    }
                    Ok(lash_execution_contracts::process::ProcessIdentity::labelled(kind, Some("engine-stamp")))
                }),
            )
            .expect("matching engine kind"),
        );
        for marker in ["invalid", "missing-config", "valid"] {
            let registration = engine_registration(marker, marker);
            let key = registration.start_key.clone().expect("key");
            let envelope = start_envelope(
                env_store.as_ref(),
                marker,
                registration,
                lash_execution_contracts::testing::process_env_with_limit(1024),
            )
            .await;
            let crate::RuntimeEffectCommand::Process { command } = envelope.command else {
                panic!("expected a process command");
            };
            let outcome = crate::RuntimeEffectLocalExecutor::processes(
                registry.clone(),
                Arc::new(crate::NoProcessWork::for_registry(registry.clone())),
                engines.clone(),
                crate::runtime::HostStartAdmission::default(),
            )
            .with_process_env_store(env_store.clone())
            .into_process()
            .expect("process executor")
            .execute(
                &crate::ExecutionScope::runtime_operation("runtime"),
                *command,
            )
            .await;
            if marker == "missing-config" {
                let error = outcome.expect_err("typed admission refusal");
                assert_eq!(
                    error.code,
                    crate::RuntimeErrorCode::MissingRecordedProcessConfig
                );
                assert_eq!(
                    error.cause,
                    Some(crate::RuntimeErrorCause::MissingRecordedProcessConfig {
                        engine_kind: "testing-fixture".to_owned(),
                    })
                );
                assert!(
                    registry
                        .get_process_by_start_key(&key)
                        .await
                        .expect("read key")
                        .is_none()
                );
            } else if marker == "invalid" {
                assert!(
                    outcome
                        .expect_err("invalid payload is refused")
                        .to_string()
                        .contains("invalid fixture payload")
                );
                assert!(
                    registry
                        .get_process_by_start_key(&key)
                        .await
                        .expect("read key")
                        .is_none()
                );
            } else {
                let crate::ProcessEffectOutcome::Start { record, .. } =
                    outcome.expect("valid start")
                else {
                    panic!("expected a start");
                };
                assert_eq!(
                    record.identity,
                    lash_execution_contracts::process::ProcessIdentity::labelled(
                        "testing-fixture",
                        Some("engine-stamp")
                    )
                );
            }
        }
    }

    #[tokio::test]
    async fn every_local_start_requires_engine_admission() {
        let backend = crate::support::sqlite_memory_store_backend().await;
        let registry = backend.process_registry();
        let env_store = backend.process_env_store();
        let registration = engine_registration("seam-admission", "unchecked");
        let key = registration.start_key.clone().expect("start key");
        let envelope = start_envelope(
            env_store.as_ref(),
            "seam-admission",
            registration,
            lash_execution_contracts::testing::process_env_with_limit(1024),
        )
        .await;
        let outcome = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&registry),
            Arc::new(crate::NoProcessWork::for_registry(Arc::clone(&registry))),
            crate::ProcessEngineRegistry::new(),
            crate::runtime::HostStartAdmission::default(),
        )
        .with_process_env_store(env_store)
        .into_process()
        .expect("process executor")
        .execute(
            &crate::ExecutionScope::runtime_operation("runtime"),
            match envelope.command {
                crate::RuntimeEffectCommand::Process { command } => *command,
                other => panic!("expected process command: {other:?}"),
            },
        )
        .await;
        let error = outcome.expect_err("an unconfigured engine must be refused");
        assert!(
            error.to_string().contains("is not configured"),
            "engine admission must supply the refusal: {error:?}"
        );
        assert!(
            registry
                .get_process_by_start_key(&key)
                .await
                .expect("read key")
                .is_none()
        );
    }

    /// A start keyed by `key`: a journaled start is addressed by its key.
    fn engine_registration(
        key: &str,
        marker: &str,
    ) -> lash_execution_contracts::process::ProcessRegistration {
        lash_execution_contracts::process::ProcessRegistration::new(
            lash_execution_contracts::process::ProcessInput::Engine {
                kind: "testing-fixture".to_string(),
                payload: serde_json::json!({"marker": marker}),
                launch: lash_execution_contracts::testing::fixture_engine_launch(),
            },
            lash_execution_contracts::process::ProcessProvenance::host(),
            lash_execution_contracts::process::Lifetime::Detached,
        )
        .with_start_key(Some(crate::StartKey::for_host(key)))
    }

    /// `registration` launched under `env`.
    fn with_env(
        mut registration: lash_execution_contracts::process::ProcessRegistration,
        env: crate::ProcessEnvRef,
    ) -> lash_execution_contracts::process::ProcessRegistration {
        if let lash_execution_contracts::process::ProcessInput::Engine { kind, payload, .. } =
            registration.input.as_ref()
        {
            registration.input =
                Arc::new(lash_execution_contracts::process::ProcessInput::Engine {
                    kind: kind.clone(),
                    payload: payload.clone(),
                    launch: lash_execution_contracts::process::EngineLaunch::new(env, None),
                });
        }
        registration
    }

    fn end(referrer: crate::ArtifactReferrer) -> crate::ResolvedArtifactCleanup {
        crate::ResolvedArtifactCleanup {
            referrer,
            carries: Vec::new(),
        }
    }

    async fn start_envelope(
        env_store: &dyn lash_execution_contracts::process::ProcessEnvStore,
        effect_id: &str,
        registration: lash_execution_contracts::process::ProcessRegistration,
        env_spec: crate::ProcessEnv,
    ) -> crate::RuntimeEffectEnvelope {
        let starter = crate::ExecutionScope::runtime_operation("runtime")
            .journal_identity()
            .expect("starter journal");
        let claim = crate::ReferrerClaim::guarded(crate::ReferrerGuard::Start {
            start_key: registration.start_key.clone().expect("start key"),
            starter,
        });
        let env_ref = lash_core_execution::testing::publish_process_env_for_testing(
            env_store, &claim, &env_spec,
        )
        .await
        .expect("publish referenced environment");
        crate::RuntimeEffectEnvelope::new(
            crate::RuntimeEffectInvocation::new(
                crate::EffectAddress::new(
                    crate::ExecutionScope::runtime_operation("runtime"),
                    effect_id,
                )
                .expect("valid process-start test address"),
                crate::RuntimeAttribution::none(),
                effect_id,
            ),
            crate::RuntimeEffectCommand::process(crate::ProcessCommand::Start {
                registration: with_env(registration, env_ref).into(),
                observers: Vec::new(),
                execution_context: Box::new(
                    lash_execution_contracts::process::ProcessExecutionContext::default(),
                ),
            }),
        )
    }

    /// The start `envelope` carries, run by the process executor as a
    /// store-local effect: its registration is its own record, and nothing
    /// replays it.
    async fn execute_start(
        envelope: crate::RuntimeEffectEnvelope,
        executor: crate::RuntimeEffectLocalExecutor<'static>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::Process { command } = envelope.command else {
            panic!("a start envelope carries a process command");
        };
        let result = executor
            .into_process()
            .expect("a process executor")
            .execute(envelope.invocation.execution_scope(), *command)
            .await?;
        Ok(crate::RuntimeEffectOutcome::Process { result })
    }

    fn started_record(
        outcome: crate::RuntimeEffectOutcome,
    ) -> lash_execution_contracts::process::ObservedProcess {
        let crate::RuntimeEffectOutcome::Process {
            result: ProcessEffectOutcome::Start { record, .. },
        } = outcome
        else {
            panic!("wrong start outcome: {outcome:?}")
        };
        *record
    }

    fn start_claim(key: &crate::StartKey) -> crate::ReferrerClaim {
        crate::ReferrerClaim::guarded(crate::ReferrerGuard::Start {
            start_key: key.clone(),
            starter: crate::ExecutionScope::runtime_operation("runtime")
                .journal_identity()
                .expect("runtime journal"),
        })
    }

    fn carry_env(
        from: crate::ArtifactReferrer,
        to: crate::ArtifactReferrer,
        env_ref: &crate::ProcessEnvRef,
    ) -> crate::ResolvedArtifactCleanup {
        crate::ResolvedArtifactCleanup {
            referrer: from,
            carries: vec![crate::ArtifactCarry {
                artifact: crate::ArtifactName {
                    store: crate::ArtifactStoreId::ProcessEnv,
                    artifact_ref: env_ref.as_str().to_owned(),
                },
                to,
            }],
        }
    }

    /// A store that fences the start just before its first acquire, as a
    /// concurrent attempt can settle the same key before this one stages.
    struct FenceStartBeforeFirstAcquire {
        inner: Arc<dyn lash_execution_contracts::process::ProcessEnvStore>,
        start: crate::ArtifactReferrer,
        interleavings: AtomicUsize,
    }

    impl crate::store::FleetFormatStore for FenceStartBeforeFirstAcquire {
        fn fleet_format(&self) -> crate::FleetFormat {
            self.inner.fleet_format()
        }

        fn plugin_writers(&self) -> crate::store::PluginWriterRangesFuture<'_> {
            self.inner.plugin_writers()
        }

        fn provision_plugin_writers<'a>(
            &'a self,
            registrations: &'a [crate::store::plugin_writers::PluginWriterRegistration],
        ) -> crate::store::PluginWriterRangesFuture<'a> {
            self.inner.provision_plugin_writers(registrations)
        }
    }

    #[async_trait::async_trait]
    impl lash_execution_contracts::process::ProcessEnvStore for FenceStartBeforeFirstAcquire {
        async fn publish_process_env(
            &self,
            claim: &crate::ReferrerClaim,
            env_ref: &crate::ProcessEnvRef,
            bytes: &[u8],
        ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
            self.inner.publish_process_env(claim, env_ref, bytes).await
        }

        async fn acquire_process_env(
            &self,
            claim: &crate::ReferrerClaim,
            env_ref: &crate::ProcessEnvRef,
        ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
            if self.interleavings.fetch_add(1, Ordering::SeqCst) == 0 {
                self.inner
                    .end_process_env_referrer(&end(self.start.clone()))
                    .await?;
            }
            self.inner.acquire_process_env(claim, env_ref).await
        }

        async fn end_process_env_referrer(
            &self,
            cleanup: &crate::ResolvedArtifactCleanup,
        ) -> Result<(), lash_execution_contracts::module_artifacts::ArtifactStoreError> {
            self.inner.end_process_env_referrer(cleanup).await
        }

        async fn get_process_env(
            &self,
            env_ref: &crate::ProcessEnvRef,
        ) -> Result<Option<Vec<u8>>, lash_execution_contracts::module_artifacts::ArtifactStoreError>
        {
            self.inner.get_process_env(env_ref).await
        }
    }

    /// FIG-3090: a start whose referrer is fenced before acquisition still
    /// leaves the registered process holding its environment.
    ///
    /// This is the host-published shape: the registration names an environment
    /// a host pin already published. A concurrent attempt fences the
    /// shared start referrer before this attempt acquires; the runtime then
    /// acquires the same bytes directly for the registered process.
    #[tokio::test]
    async fn a_start_acquires_its_environment_after_a_concurrent_attempt_fences_its_referrer() {
        let key = "raced-staging-owner-start";
        let backend = crate::support::sqlite_memory_store_backend().await;
        let registry: Arc<dyn lash_execution_contracts::process::ProcessRegistry> =
            backend.process_registry();
        let env_spec = lash_execution_contracts::testing::process_env_with_limit(1024);
        let env_ref = env_spec.stable_ref().expect("stable environment reference");
        let bytes = env_spec.to_store_bytes().expect("encode environment");
        let inner: Arc<dyn lash_execution_contracts::process::ProcessEnvStore> =
            backend.process_env_store();
        // The host pin's own edge, exactly as a host holds it.
        let subscription = crate::ArtifactReferrer::HostPin(crate::HostArtifactPin::mint());
        let subscription_claim =
            crate::ReferrerClaim::unguarded(subscription.clone()).expect("subscription pin claim");
        inner
            .publish_process_env(&subscription_claim, &env_ref, &bytes)
            .await
            .expect("publish the subscription environment");
        let env_store = Arc::new(FenceStartBeforeFirstAcquire {
            inner: Arc::clone(&inner),
            start: crate::ArtifactReferrer::Start(crate::StartKey::for_host(key)),
            interleavings: AtomicUsize::new(0),
        });
        let envelope = crate::RuntimeEffectEnvelope::new(
            crate::RuntimeEffectInvocation::new(
                crate::EffectAddress::new(
                    crate::ExecutionScope::runtime_operation("runtime"),
                    "raced-staging-owner-start",
                )
                .expect("valid process-start test address"),
                crate::RuntimeAttribution::none(),
                "raced-staging-owner-start",
            ),
            crate::RuntimeEffectCommand::process(crate::ProcessCommand::Start {
                registration: with_env(engine_registration(key, "delivery"), env_ref.clone())
                    .into(),
                observers: Vec::new(),
                execution_context: Box::new(
                    lash_execution_contracts::process::ProcessExecutionContext::default(),
                ),
            }),
        );
        let executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&registry),
            Arc::new(crate::NoProcessWork::for_registry(Arc::clone(&registry))),
            crate::testing::process_engine_fixture(),
            crate::runtime::HostStartAdmission::default(),
        )
        .with_process_env_store(
            Arc::clone(&env_store) as Arc<dyn lash_execution_contracts::process::ProcessEnvStore>
        );

        let started = started_record(
            execute_start(envelope, executor)
                .await
                .expect("a severed staging edge must not fail the start"),
        );

        assert_eq!(
            env_store.interleavings.load(Ordering::SeqCst),
            2,
            "the start first tries its fenced claim, then acquires for the process"
        );
        let record = registry
            .get_process(&started.process_id)
            .await
            .expect("read registered process")
            .expect("registered process remains live");
        let process = crate::ArtifactReferrer::ProcessRecord(record.id.clone());
        inner
            .end_process_env_referrer(&end(subscription))
            .await
            .expect("end the subscription pin");
        assert_eq!(
            inner
                .get_process_env(&env_ref)
                .await
                .expect("read the retained environment"),
            Some(bytes.clone()),
            "the registered process must own its environment after the race"
        );
        inner
            .end_process_env_referrer(&end(process))
            .await
            .expect("end the process referrer");
        assert_eq!(
            inner
                .get_process_env(&env_ref)
                .await
                .expect("read the reclaimed environment"),
            None,
            "the process owner must be the only surviving edge"
        );
    }

    /// ADR 0107: a changed-content retry under a trusted key keeps the
    /// retained process's environment.
    ///
    /// The first attempt acquired E1 under the start referrer, registered
    /// the process naming E1, and crashed before its cleanup. The retry
    /// publishes E2 under the same start referrer, then returns the retained
    /// process. The cleanup carries E1 to that process and reclaims E2.
    #[tokio::test]
    async fn a_changed_content_retry_after_a_crash_keeps_the_retained_environment() {
        let backend = crate::support::sqlite_memory_store_backend().await;
        let registry: Arc<dyn lash_execution_contracts::process::ProcessRegistry> =
            backend.process_registry();
        let env_store: Arc<dyn lash_execution_contracts::process::ProcessEnvStore> =
            backend.process_env_store();
        let first_env = lash_execution_contracts::testing::process_env_with_limit(1024);
        let retry_env = lash_execution_contracts::testing::process_env_with_limit(3);
        let first_ref = first_env.stable_ref().expect("first environment reference");
        let retry_ref = retry_env.stable_ref().expect("retry environment reference");
        assert_ne!(first_ref, retry_ref, "the retry submits different content");
        let key = crate::StartKeyDerivation::LASH_START_PATHS.for_tool_intent(
            &lash_execution_contracts::tool_intent::derive_tool_intent_identity(
                &lash_vocabulary::identity::RuntimeOwner::Session(
                    lash_kernel_value::identity::SessionId::from("session"),
                ),
                "runtime",
                &lash_core_execution::ToolCallId::fixture("crashed-start"),
                0,
            ),
        );
        let registration = |marker: &str| {
            let mut registration = engine_registration("crashed-start", marker);
            registration.start_key = Some(key.clone());
            registration
        };
        let staging = start_claim(&key);

        // The first attempt: staged, registered, crashed before settling.
        lash_core_execution::testing::publish_process_env_for_testing(
            env_store.as_ref(),
            &staging,
            &first_env,
        )
        .await
        .expect("stage the first attempt's environment");
        let retained = registry
            .register_process(with_env(registration("first"), first_ref.clone()))
            .await
            .expect("the first attempt registered its process");

        // The retry, which no record of the first attempt answers, with
        // changed content.
        let retry_ref = lash_core_execution::testing::publish_process_env_for_testing(
            env_store.as_ref(),
            &staging,
            &retry_env,
        )
        .await
        .expect("publish retry environment");
        let envelope = crate::RuntimeEffectEnvelope::new(
            crate::RuntimeEffectInvocation::new(
                crate::EffectAddress::new(
                    crate::ExecutionScope::runtime_operation("runtime"),
                    "crashed-start",
                )
                .expect("valid process-start test address"),
                crate::RuntimeAttribution::none(),
                "crashed-start",
            ),
            crate::RuntimeEffectCommand::process(crate::ProcessCommand::Start {
                registration: with_env(registration("retry"), retry_ref.clone()).into(),
                observers: Vec::new(),
                execution_context: Box::new(
                    lash_execution_contracts::process::ProcessExecutionContext::default(),
                ),
            }),
        );
        let executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&registry),
            Arc::new(crate::NoProcessWork::for_registry(Arc::clone(&registry))),
            crate::testing::process_engine_fixture(),
            crate::runtime::HostStartAdmission::default(),
        )
        .with_process_env_store(Arc::clone(&env_store));
        let returned = started_record(
            execute_start(envelope, executor)
                .await
                .expect("the retry is returned the retained process"),
        );
        assert_eq!(returned.process_id, retained.id);
        assert_eq!(returned.env_ref.as_ref(), Some(&first_ref));

        let process = crate::ArtifactReferrer::ProcessRecord(retained.id.clone());
        env_store
            .end_process_env_referrer(&carry_env(
                crate::ArtifactReferrer::Start(key.clone()),
                process.clone(),
                &first_ref,
            ))
            .await
            .expect("carry the retained environment and reclaim the retry's bytes");

        assert!(
            env_store
                .get_process_env(&first_ref)
                .await
                .expect("read the retained environment")
                .is_some(),
            "the retained process's environment survives the retry"
        );
        assert_eq!(
            env_store
                .get_process_env(&retry_ref)
                .await
                .expect("read the retry's environment"),
            None,
            "the retry's unadopted bytes are reclaimed by start cleanup"
        );
        env_store
            .end_process_env_referrer(&end(process))
            .await
            .expect("end the process edge");
        assert_eq!(
            env_store
                .get_process_env(&first_ref)
                .await
                .expect("read the reclaimed environment"),
            None,
            "the process record held the environment after start cleanup"
        );
    }

    /// A process command an execution context issues runs as its store-local
    /// effect through `ActorContext::process_effect`, the one path a start, a
    /// cancel or a session's process delete takes.
    #[tokio::test]
    async fn a_process_start_issued_through_the_actor_context_registers_its_process() {
        let key = "actor-context-start";
        let backend = crate::support::sqlite_memory_store_backend().await;
        let registry: Arc<dyn lash_execution_contracts::process::ProcessRegistry> =
            backend.process_registry();
        let env_store = backend.process_env_store();
        let envelope = start_envelope(
            env_store.as_ref(),
            key,
            engine_registration(key, "context"),
            lash_execution_contracts::testing::process_env_with_limit(1024),
        )
        .await;
        let executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&registry),
            Arc::new(crate::NoProcessWork::for_registry(Arc::clone(&registry))),
            crate::testing::process_engine_fixture(),
            crate::runtime::HostStartAdmission::default(),
        )
        .with_process_env_store(env_store);
        let outcome = crate::ActorContext::detached(backend.clone())
            .process_effect(envelope, executor)
            .await
            .expect("the actor context runs the start");
        let record = started_record(outcome);
        assert!(
            registry
                .get_process(&record.process_id)
                .await
                .expect("read the started process")
                .is_some(),
            "the start registered its process"
        );
    }
}
