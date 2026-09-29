mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::{ProcessEffectOutcome, RuntimeEffectController};

    /// The start laws' server-double seed.
    const SEED: u64 = 0x90_ca1;

    fn runtime_controller(backend: &crate::Backend) -> Arc<dyn RuntimeEffectController> {
        crate::support::scoped_controller(
            backend,
            crate::AdmittedScope::runtime_operation("runtime"),
        )
    }

    /// Runs `envelope` once inside a `runtime_operation` handler on `double`:
    /// Each handler is a fresh invocation, so its journal holds
    /// no record of an earlier attempt — the shape a start's retry runs under
    /// after a crash before the journal commit.
    async fn execute_in_handler(
        double: &lash_restate_test::RestateTestBackend,
        envelope: crate::RuntimeEffectEnvelope,
        executor: crate::RuntimeEffectLocalExecutor<'static>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let handler = double
            .open_handler(crate::AdmittedScope::runtime_operation("runtime"))
            .await
            .expect("open the runtime-operation handler");
        let outcome = handler.scoped().execute_effect(envelope, executor).await;
        handler
            .close()
            .await
            .expect("the runtime-operation handler completes");
        outcome
    }

    /// A start keyed by `key`: a journaled start is addressed by its key.
    fn tool_registration(key: &str, marker: &str) -> crate::ProcessRegistration {
        crate::ProcessRegistration::new(
            crate::ProcessInput::ToolCall {
                call: crate::PreparedToolCall::from_parts(
                    key,
                    crate::ToolId::new("test-tool"),
                    "test_tool",
                    serde_json::json!({"marker": marker}),
                    None,
                    serde_json::Value::Null,
                ),
            },
            crate::ProcessProvenance::host(),
            crate::Lifetime::Detached,
        )
        .with_start_key(Some(crate::StartKey::for_host(key)))
    }

    fn started_record(outcome: crate::RuntimeEffectOutcome) -> crate::ProcessRecord {
        let crate::RuntimeEffectOutcome::Process {
            result: ProcessEffectOutcome::Start { record, .. },
        } = outcome
        else {
            panic!("wrong start outcome: {outcome:?}")
        };
        *record
    }

    fn start_claim(key: &crate::StartKey) -> crate::ReferrerClaim {
        crate::ReferrerClaim::guarded(
            crate::ArtifactReferrer::Start(key.clone()),
            crate::ArtifactCleanupPlan::AwaitStart {
                starter: crate::ExecutionScope::runtime_operation("runtime")
                    .journal_identity()
                    .expect("runtime journal"),
            },
        )
        .expect("start claim")
    }

    fn end(referrer: crate::ArtifactReferrer) -> crate::ResolvedArtifactCleanup {
        crate::ResolvedArtifactCleanup {
            referrer,
            carries: Vec::new(),
        }
    }

    fn carry_env(
        from: crate::ArtifactReferrer,
        to: crate::ArtifactReferrer,
        env_ref: &crate::ProcessExecutionEnvRef,
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

    fn start_envelope(
        effect_id: &str,
        registration: crate::ProcessRegistration,
        env_spec: crate::ProcessExecutionEnvSpec,
    ) -> crate::RuntimeEffectEnvelope {
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
                registration,
                observers: Vec::new(),
                env_spec: Some(env_spec),
                execution_context: Box::new(crate::ProcessExecutionContext::default()),
            }),
        )
    }

    #[tokio::test]
    async fn process_start_replays_and_carries_environment_to_process() {
        let key = "owned-env-start";
        let double =
            crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let registry: Arc<dyn crate::ProcessRegistry> = double.lash_backend().process_registry();
        let env_store: Arc<dyn crate::ProcessExecutionEnvStore> =
            double.lash_backend().process_env_store();
        let env_spec = crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        );
        let env_ref = env_spec.stable_ref().expect("stable environment reference");
        let command = start_envelope(
            "owned-env-start",
            tool_registration(key, "original"),
            env_spec,
        );
        let executor = || {
            crate::RuntimeEffectLocalExecutor::processes(
                Arc::clone(&registry),
                Arc::new(crate::NoProcessWork::for_registry(Arc::clone(&registry))),
            )
            .with_process_env_store(Arc::clone(&env_store))
        };
        let first = started_record(
            execute_in_handler(&double, command.clone(), executor())
                .await
                .expect("initial process start"),
        );
        // The second attempt runs the local start again, as the crash after
        // the local start and before its journal commit leaves it: in a
        // second handler whose journal holds no record of the first run (a
        // redrive over the same journal would only replay it), against the
        // same registry and environment store.
        let rerun = started_record(
            execute_in_handler(&double, command, executor())
                .await
                .expect("re-run process start before guard cleanup"),
        );
        assert_eq!(
            rerun.id, first.id,
            "the start key returns the retained process, never a second one"
        );
        let record = registry
            .get_process(&first.id)
            .await
            .expect("read registered process")
            .expect("registered process remains live");

        let start = crate::ArtifactReferrer::Start(crate::StartKey::for_host(key));
        let process = crate::ArtifactReferrer::ProcessRecord(record.id.clone());
        env_store
            .end_process_env_referrer(&carry_env(start, process.clone(), &env_ref))
            .await
            .expect("carry start environment to process");
        env_store
            .end_process_env_referrer(&end(process))
            .await
            .expect("end process environment referrer");
        assert_eq!(
            env_store
                .get_process_execution_env(&env_ref)
                .await
                .expect("read reclaimed environment"),
            None,
            "the process record was the only surviving referrer after the carry"
        );
    }

    /// A store that fences the start just before its first acquire, as a
    /// concurrent attempt can settle the same key before this one stages.
    struct FenceStartBeforeFirstAcquire {
        inner: Arc<dyn crate::ProcessExecutionEnvStore>,
        start: crate::ArtifactReferrer,
        interleavings: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ProcessExecutionEnvStore for FenceStartBeforeFirstAcquire {
        async fn publish_process_execution_env(
            &self,
            claim: &crate::ReferrerClaim,
            env_ref: &crate::ProcessExecutionEnvRef,
            bytes: &[u8],
        ) -> Result<(), crate::ArtifactStoreError> {
            self.inner
                .publish_process_execution_env(claim, env_ref, bytes)
                .await
        }

        async fn acquire_process_execution_env(
            &self,
            claim: &crate::ReferrerClaim,
            env_ref: &crate::ProcessExecutionEnvRef,
        ) -> Result<(), crate::ArtifactStoreError> {
            if self.interleavings.fetch_add(1, Ordering::SeqCst) == 0 {
                self.inner
                    .end_process_env_referrer(&end(self.start.clone()))
                    .await?;
            }
            self.inner
                .acquire_process_execution_env(claim, env_ref)
                .await
        }

        async fn end_process_env_referrer(
            &self,
            cleanup: &crate::ResolvedArtifactCleanup,
        ) -> Result<(), crate::ArtifactStoreError> {
            self.inner.end_process_env_referrer(cleanup).await
        }

        async fn get_process_execution_env(
            &self,
            env_ref: &crate::ProcessExecutionEnvRef,
        ) -> Result<Option<Vec<u8>>, crate::ArtifactStoreError> {
            self.inner.get_process_execution_env(env_ref).await
        }
    }

    /// FIG-3090: a start whose referrer is fenced before acquisition still
    /// leaves the registered process holding its environment.
    ///
    /// This is the trigger-delivery shape: the registration names an environment
    /// a subscription already published. A concurrent attempt fences the
    /// shared start referrer before this attempt acquires; the runtime then
    /// acquires the same bytes directly for the registered process.
    #[tokio::test]
    async fn a_start_acquires_its_environment_after_a_concurrent_attempt_fences_its_referrer() {
        let key = "raced-staging-owner-start";
        let double =
            crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let registry: Arc<dyn crate::ProcessRegistry> = double.lash_backend().process_registry();
        let env_spec = crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        );
        let env_ref = env_spec.stable_ref().expect("stable environment reference");
        let bytes = env_spec.to_store_bytes().expect("encode environment");
        let inner: Arc<dyn crate::ProcessExecutionEnvStore> =
            double.lash_backend().process_env_store();
        // The subscription's own edge, exactly as a registered trigger holds it.
        let subscription = crate::ArtifactReferrer::HostPin(crate::HostArtifactPin::mint());
        let subscription_claim =
            crate::ReferrerClaim::unguarded(subscription.clone()).expect("subscription pin claim");
        inner
            .publish_process_execution_env(&subscription_claim, &env_ref, &bytes)
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
                registration: tool_registration(key, "delivery")
                    .with_execution_env_ref(Some(env_ref.clone())),
                observers: Vec::new(),
                env_spec: None,
                execution_context: Box::new(crate::ProcessExecutionContext::default()),
            }),
        );
        let executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&registry),
            Arc::new(crate::NoProcessWork::for_registry(Arc::clone(&registry))),
        )
        .with_process_env_store(Arc::clone(&env_store) as Arc<dyn crate::ProcessExecutionEnvStore>);

        let started = started_record(
            execute_in_handler(&double, envelope, executor)
                .await
                .expect("a severed staging edge must not fail the start"),
        );

        assert_eq!(
            env_store.interleavings.load(Ordering::SeqCst),
            2,
            "the start first tries its fenced claim, then acquires for the process"
        );
        let record = registry
            .get_process(&started.id)
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
                .get_process_execution_env(&env_ref)
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
                .get_process_execution_env(&env_ref)
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
        let double =
            crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let registry: Arc<dyn crate::ProcessRegistry> = double.lash_backend().process_registry();
        let env_store: Arc<dyn crate::ProcessExecutionEnvStore> =
            double.lash_backend().process_env_store();
        let env = |budget: crate::TurnBudget| {
            crate::ProcessExecutionEnvSpec::new(
                crate::PluginOptions::default(),
                crate::SessionPolicy::new(budget),
            )
        };
        let first_env = env(crate::TurnBudget::Unbounded);
        let retry_env = env(crate::TurnBudget::Bounded(
            std::num::NonZeroUsize::new(3).expect("non-zero budget"),
        ));
        let first_ref = first_env.stable_ref().expect("first environment reference");
        let retry_ref = retry_env.stable_ref().expect("retry environment reference");
        assert_ne!(first_ref, retry_ref, "the retry submits different content");
        let key = crate::StartKey::for_tool_intent(
            crate::StartKeyDerivation::LASH_START_PATHS,
            &crate::derive_tool_intent_identity(
                &crate::SessionId::from("session"),
                "runtime",
                Some("crashed-start"),
                0,
            )
            .expect("the crashed start's intent identity derives"),
        );
        let registration = |marker: &str| {
            let mut registration = tool_registration("crashed-start", marker);
            registration.start_key = Some(key.clone());
            registration
        };
        let staging = start_claim(&key);

        // The first attempt: staged, registered, crashed before settling.
        crate::publish_process_execution_env(env_store.as_ref(), &staging, &first_env)
            .await
            .expect("stage the first attempt's environment");
        let retained = registry
            .register_process(registration("first").with_execution_env_ref(Some(first_ref.clone())))
            .await
            .expect("the first attempt registered its process");

        // The retry, in a second handler whose journal holds no record of the
        // first attempt, with changed content.
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
                registration: registration("retry"),
                observers: Vec::new(),
                env_spec: Some(retry_env),
                execution_context: Box::new(crate::ProcessExecutionContext::default()),
            }),
        );
        let executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&registry),
            Arc::new(crate::NoProcessWork::for_registry(Arc::clone(&registry))),
        )
        .with_process_env_store(Arc::clone(&env_store));
        let returned = started_record(
            execute_in_handler(&double, envelope, executor)
                .await
                .expect("the retry is returned the retained process"),
        );
        assert_eq!(returned.id, retained.id);
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
                .get_process_execution_env(&first_ref)
                .await
                .expect("read the retained environment")
                .is_some(),
            "the retained process's environment survives the retry"
        );
        assert_eq!(
            env_store
                .get_process_execution_env(&retry_ref)
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
                .get_process_execution_env(&first_ref)
                .await
                .expect("read the reclaimed environment"),
            None,
            "the process record held the environment after start cleanup"
        );
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
            Err(crate::PluginError::Invoke(
                "start delivery unavailable".into(),
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
        let backend = crate::support::memory_backend().await;
        let registry: Arc<dyn crate::ProcessRegistry> = backend.process_registry();
        let env_spec = crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        );
        let process_work = Arc::new(PokeAlwaysFails {
            pokes: AtomicUsize::new(0),
        });
        let executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&registry),
            Arc::clone(&process_work) as Arc<dyn crate::ProcessWorkSubstrate>,
        )
        .with_process_starts(
            backend.obligation_ledger(crate::store::ObligationKind::ProcessStart),
            backend.clock(),
        )
        .with_process_env_store(backend.process_env_store());

        let outcome = runtime_controller(&backend)
            .execute_effect(
                start_envelope(
                    "advisory-poke-start",
                    tool_registration(key, "advisory"),
                    env_spec,
                ),
                executor,
            )
            .await
            .expect("an advisory poke failure must not fail the start");
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
            .standing(&crate::store::process_start_obligation_id(&record.id))
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

    /// ADR 0107: a lash-derived start key is trusted. A retry under a
    /// retained key whose content changed returns the retained process
    /// untouched, and start cleanup reclaims the environment it staged rather
    /// than attaching it to a process that never submitted it. (A host key fences its
    /// content instead; the registry conformance laws cover that.)
    #[tokio::test]
    async fn a_changed_content_retry_reclaims_its_unadopted_start_artifact() {
        let key = "changed-content-start";
        let start_key = crate::StartKey::for_trigger_delivery(
            crate::StartKeyDerivation::LASH_START_PATHS,
            key,
            "subscription",
            "incarnation",
            1,
        );
        let keyed =
            |marker: &str| tool_registration(key, marker).with_start_key(Some(start_key.clone()));
        let double =
            crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let registry: Arc<dyn crate::ProcessRegistry> = double.lash_backend().process_registry();
        let env_store: Arc<dyn crate::ProcessExecutionEnvStore> =
            double.lash_backend().process_env_store();
        let retained_env_ref = crate::ProcessExecutionEnvRef::new("process-env:retained");
        let retained = registry
            .register_process(
                keyed("retained").with_execution_env_ref(Some(retained_env_ref.clone())),
            )
            .await
            .expect("register the key's process");
        let env_spec = crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        );
        let env_ref = env_spec.stable_ref().expect("stable environment reference");
        assert_ne!(
            env_ref, retained_env_ref,
            "the retry must submit other content"
        );
        let bytes = env_spec.to_store_bytes().expect("encode environment");
        let executor = crate::RuntimeEffectLocalExecutor::processes(
            Arc::clone(&registry),
            Arc::new(crate::NoProcessWork::for_registry(Arc::clone(&registry))),
        )
        .with_process_env_store(Arc::clone(&env_store));

        let returned = started_record(
            execute_in_handler(
                &double,
                start_envelope("changed-content-start", keyed("changed"), env_spec),
                executor,
            )
            .await
            .expect("a retry under a retained key is not a failure"),
        );

        assert_eq!(
            returned.id, retained.id,
            "the key names the retained process"
        );
        assert_eq!(
            returned.input, retained.input,
            "the retry's content is not adopted"
        );
        assert_eq!(returned.env_ref, Some(retained_env_ref));
        env_store
            .end_process_env_referrer(&end(crate::ArtifactReferrer::Start(start_key.clone())))
            .await
            .expect("reclaim the retry's unadopted staging");
        assert_eq!(
            env_store
                .get_process_execution_env(&env_ref)
                .await
                .expect("read reclaimed environment"),
            None,
            "the retry's staged environment must not be attached to the retained process"
        );
        assert!(
            env_store
                .publish_process_execution_env(&start_claim(&start_key), &env_ref, &bytes,)
                .await
                .is_err(),
            "the ended start referrer must fence a late publication"
        );
    }
}
