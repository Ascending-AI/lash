mod tests {
    use std::sync::Arc;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::ProcessEffectOutcome;

    #[tokio::test]
    async fn a_journaled_environment_load_keeps_its_bytes_after_the_source_pin_ends() {
        let backend = crate::support::sqlite_memory_store_backend().await;
        let store = backend.process_env_store();
        let pin = crate::testing::host_pin_claim_for_testing();
        let spec = crate::ProcessExecutionEnvSpec::new(
            crate::AdmittedPluginConfig::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
        );
        let env_ref = crate::publish_process_execution_env(store.as_ref(), &pin, &spec)
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
            crate::load_process_execution_env(store.as_ref(), &env_ref)
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
                .get_process_execution_env(&env_ref)
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
                crate::ProcessEngineAdmission::new("testing-fixture", |kind, payload, env| {
                    assert!(env.is_some(), "admission receives the recorded environment");
                    if payload["marker"] == "invalid" {
                        return Err(crate::PluginError::Session(
                            "invalid fixture payload".to_owned(),
                        ));
                    }
                    if payload["marker"] == "missing-config" {
                        return Err(crate::PluginError::MissingRecordedProcessConfig {
                            engine_kind: kind.to_owned(),
                        });
                    }
                    Ok(crate::ProcessIdentity::labelled(kind, Some("engine-stamp")))
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
                crate::ProcessExecutionEnvSpec::new(
                    crate::AdmittedPluginConfig::default(),
                    crate::SessionPolicy::new(
                        crate::TurnBudget::Unbounded,
                        crate::MaxToolCalls::new(1024),
                    ),
                ),
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
                    crate::ProcessIdentity::labelled("testing-fixture", Some("engine-stamp"))
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
            crate::ProcessExecutionEnvSpec::new(
                crate::AdmittedPluginConfig::default(),
                crate::SessionPolicy::new(
                    crate::TurnBudget::Unbounded,
                    crate::MaxToolCalls::new(1024),
                ),
            ),
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
    fn engine_registration(key: &str, marker: &str) -> crate::ProcessRegistration {
        crate::ProcessRegistration::new(
            crate::ProcessInput::Engine {
                kind: "testing-fixture".to_string(),
                payload: serde_json::json!({"marker": marker}),
            },
            crate::ProcessProvenance::host(),
            crate::Lifetime::Detached,
        )
        .with_start_key(Some(crate::StartKey::for_host(key)))
    }

    fn end(referrer: crate::ArtifactReferrer) -> crate::ResolvedArtifactCleanup {
        crate::ResolvedArtifactCleanup {
            referrer,
            carries: Vec::new(),
        }
    }

    async fn start_envelope(
        env_store: &dyn crate::ProcessExecutionEnvStore,
        effect_id: &str,
        registration: crate::ProcessRegistration,
        env_spec: crate::ProcessExecutionEnvSpec,
    ) -> crate::RuntimeEffectEnvelope {
        let starter = crate::ExecutionScope::runtime_operation("runtime")
            .journal_identity()
            .expect("starter journal");
        let claim = crate::ReferrerClaim::guarded(crate::ReferrerGuard::Start {
            start_key: registration.start_key.clone().expect("start key"),
            starter,
        });
        let env_ref = crate::publish_process_execution_env(env_store, &claim, &env_spec)
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
                registration: registration.with_execution_env_ref(Some(env_ref)).into(),
                observers: Vec::new(),
                execution_context: Box::new(crate::ProcessExecutionContext::default()),
            }),
        )
    }

    /// A `ProcessWorkSubstrate` whose advisory poke always fails.
    struct PokeAlwaysFails {
        pokes: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ProcessWorkSubstrate for PokeAlwaysFails {
        async fn deliver_process_start(
            &self,
            _record: &crate::ProcessRecord,
        ) -> Result<(), crate::PluginError> {
            self.pokes.fetch_add(1, Ordering::SeqCst);
            Err(crate::PluginError::attempt_fault(
                "start delivery unavailable",
            ))
        }

        async fn await_process_terminal(
            &self,
            _process_ref: &crate::ProcessId,
        ) -> Result<crate::ProcessTerminalWait, crate::PluginError> {
            unreachable!("poke witness does not await terminals")
        }

        async fn deliver_cancel(
            &self,
            _process_id: &crate::ProcessId,
            _request: &crate::CancelRequest,
            _key: &str,
        ) -> Result<(), crate::PluginError> {
            unreachable!("poke witness does not deliver cancels")
        }

        async fn publish_process_terminal(
            &self,
            process_id: &crate::ProcessId,
            output: &crate::ProcessAwaitOutput,
            key: &str,
        ) -> Result<(), crate::PluginError> {
            let _ = (process_id, output, key);
            Ok(())
        }
    }

    /// The immediate delivery after registration is advisory.
    ///
    /// Registration already committed the durable row, and the row is the work
    /// queue — the obligation relay retries it whether or not delivery lands. A
    /// failed nudge surfaced as a start error would tell the caller the child
    /// does not exist while it is queued to run, and the caller's retry would
    /// then do the work twice.
    #[tokio::test]
    async fn a_failed_worker_poke_still_returns_the_started_record() {
        let key = "advisory-poke-start";
        let backend = crate::support::sqlite_recording_backend().await;
        let env_store = backend.process_env_store();
        let registry: Arc<dyn crate::ProcessRegistry> = backend.process_registry();
        let env_spec = crate::ProcessExecutionEnvSpec::new(
            crate::AdmittedPluginConfig::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
        );
        let process_work = Arc::new(PokeAlwaysFails {
            pokes: AtomicUsize::new(0),
        });
        let executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&registry),
            Arc::clone(&process_work) as Arc<dyn crate::ProcessWorkSubstrate>,
            crate::testing::process_engine_fixture(),
            crate::runtime::HostStartAdmission::default(),
        )
        .with_process_starts(
            backend.obligation_ledger(crate::store::ObligationKind::ProcessStart),
            backend.clock(),
            crate::runtime::obligations::relay::RelayPolicy::default(),
            Default::default(),
        )
        .with_process_env_store(backend.process_env_store());

        let envelope = start_envelope(
            env_store.as_ref(),
            "advisory-poke-start",
            engine_registration(key, "advisory"),
            env_spec,
        )
        .await;
        let crate::RuntimeEffectCommand::Process { command } = envelope.command else {
            panic!("a start envelope carries a process command");
        };
        let outcome = crate::RuntimeEffectOutcome::Process {
            result: executor
                .into_process()
                .expect("a process executor")
                .execute(envelope.invocation.execution_scope(), *command)
                .await
                .expect("an advisory poke failure must not fail the start"),
        };
        let crate::RuntimeEffectOutcome::Process {
            result: ProcessEffectOutcome::Start { record, .. },
        } = outcome
        else {
            panic!("wrong start outcome")
        };
        assert_eq!(
            process_work.pokes.load(Ordering::SeqCst),
            1,
            "the start must still attempt the nudge"
        );

        let stored = registry
            .get_process(&record.id)
            .await
            .expect("read registered process")
            .expect("the registered row stands");
        assert!(
            !stored.is_terminal(),
            "a failed nudge must not terminalise the row the rescan will run"
        );
        assert!(
            stored.cancel_request.is_none(),
            "a failed nudge is not a start failure and must not request cancel"
        );

        // The failed poke is durable, not dropped: the armed obligation keeps
        // the attempt count and stays due for the reconcile's retry.
        let standing = backend
            .obligation_ledger(crate::store::ObligationKind::ProcessStart)
            .standing(
                &crate::store::ObligationKey::ProcessStart {
                    process_id: record.id.clone(),
                }
                .id(),
            )
            .await
            .expect("read the start obligation")
            .expect("the start obligation stands");
        assert_eq!(
            standing.state,
            crate::store::ObligationState::Due,
            "a retryable poke failure leaves the start due, never lost"
        );
        assert_eq!(standing.attempts, 1, "the failed attempt is counted");
    }
}
