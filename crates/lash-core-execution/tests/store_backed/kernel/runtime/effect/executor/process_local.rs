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
        .with_start_key(Some(crate::StartKey::for_host(
            crate::StartKeyOwner::HOST,
            key,
        )))
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

    fn staging_owner(key: &str) -> crate::ArtifactOwner {
        crate::ArtifactOwner::process_start(&crate::ProcessCommand::start_effect_id(Some(
            &crate::StartKey::for_host(crate::StartKeyOwner::HOST, key),
        )))
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
    async fn process_start_transfers_environment_and_replays_after_staging_retirement() {
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
                .expect("re-run process start after staging retirement"),
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

        env_store
            .release_process_execution_env(
                &crate::ArtifactOwner::process(record.id.clone()),
                &env_ref,
            )
            .await
            .expect("release process environment owner");
        assert_eq!(
            env_store
                .get_process_execution_env(&env_ref)
                .await
                .expect("read reclaimed environment"),
            None,
            "the process owner must be the only surviving edge after transfer"
        );
    }

    /// A `ProcessExecutionEnvStore` that retires one staging owner at the exact
    /// moment a start settles its staged environment.
    ///
    /// The injected call is the one a concurrent attempt at the same start makes
    /// on its own: `process-start:<id>` is stable per process id, so a second
    /// attempt — a trigger delivery reconciled by the worker while the emitting
    /// turn is still starting it, or an attempt whose registration failed —
    /// retires the shared staging owner. The interleaving point is the reachable
    /// one: after this attempt's publication landed, before its transfer.
    struct RetireStagingOwnerBeforeFirstTransfer {
        inner: Arc<dyn crate::ProcessExecutionEnvStore>,
        staging_owner: crate::ArtifactOwner,
        interleavings: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ProcessExecutionEnvStore for RetireStagingOwnerBeforeFirstTransfer {
        async fn publish_process_execution_env(
            &self,
            owner: &crate::ArtifactOwner,
            env_ref: &crate::ProcessExecutionEnvRef,
            bytes: &[u8],
        ) -> Result<(), crate::PluginError> {
            self.inner
                .publish_process_execution_env(owner, env_ref, bytes)
                .await
        }

        async fn transfer_process_execution_env(
            &self,
            from: &crate::ArtifactOwner,
            to: &crate::ArtifactOwner,
            env_ref: &crate::ProcessExecutionEnvRef,
        ) -> Result<(), crate::PluginError> {
            if self.interleavings.fetch_add(1, Ordering::SeqCst) == 0 {
                self.inner
                    .retire_process_execution_env_owner(&self.staging_owner)
                    .await?;
            }
            self.inner
                .transfer_process_execution_env(from, to, env_ref)
                .await
        }

        async fn release_process_execution_env(
            &self,
            owner: &crate::ArtifactOwner,
            env_ref: &crate::ProcessExecutionEnvRef,
        ) -> Result<(), crate::PluginError> {
            self.inner
                .release_process_execution_env(owner, env_ref)
                .await
        }

        async fn retire_process_execution_env_owner(
            &self,
            owner: &crate::ArtifactOwner,
        ) -> Result<(), crate::PluginError> {
            self.inner.retire_process_execution_env_owner(owner).await
        }

        async fn get_process_execution_env(
            &self,
            env_ref: &crate::ProcessExecutionEnvRef,
        ) -> Result<Option<Vec<u8>>, crate::PluginError> {
            self.inner.get_process_execution_env(env_ref).await
        }
    }

    /// FIG-3090: a start whose staging edge is severed mid-flight still leaves
    /// the registered process owning its environment.
    ///
    /// This is the trigger-delivery shape: the registration names an environment
    /// a subscription already published, the start re-publishes it under the
    /// stable staging owner, and a concurrent attempt at the same start retires
    /// that owner before this attempt transfers. The store is right to refuse a
    /// transfer whose source edge is gone; this attempt still holds the exact
    /// content-addressed bytes, so it settles the destination edge itself
    /// instead of failing the delivery.
    #[tokio::test]
    async fn a_start_settles_its_environment_after_a_concurrent_attempt_retires_the_staging_owner()
    {
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
        let subscription_owner = crate::ArtifactOwner::host("trigger-subscription");
        inner
            .publish_process_execution_env(&subscription_owner, &env_ref, &bytes)
            .await
            .expect("publish the subscription environment");
        let env_store = Arc::new(RetireStagingOwnerBeforeFirstTransfer {
            inner: Arc::clone(&inner),
            staging_owner: staging_owner(key),
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
            1,
            "the start must still attempt the transfer first"
        );
        let record = registry
            .get_process(&started.id)
            .await
            .expect("read registered process")
            .expect("registered process remains live");
        let process_owner = crate::ArtifactOwner::process(record.id.clone());
        inner
            .release_process_execution_env(&subscription_owner, &env_ref)
            .await
            .expect("release the subscription edge");
        assert_eq!(
            inner
                .get_process_execution_env(&env_ref)
                .await
                .expect("read the retained environment"),
            Some(bytes.clone()),
            "the registered process must own its environment after the race"
        );
        inner
            .release_process_execution_env(&process_owner, &env_ref)
            .await
            .expect("release the process edge");
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
    /// The first attempt staged E1 under the key's staging owner, registered
    /// the process naming E1, and crashed before settling it: E1 is held by
    /// the staging owner alone. The retry submits E2, is returned the retained
    /// process, and must release only its own staging. Retiring the shared
    /// staging owner outright severed E1's only edge, and the retained process
    /// later failed with a missing environment.
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
        let key = crate::StartKey::for_orchestration_call(
            &crate::ExecutionScope::runtime_operation("runtime"),
            "crashed-start",
            0,
        );
        let registration = |marker: &str| {
            let mut registration = tool_registration("crashed-start", marker);
            registration.start_key = Some(key.clone());
            registration
        };
        let staging = crate::ArtifactOwner::process_start(&crate::ProcessCommand::start_effect_id(
            Some(&key),
        ));

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
            "the retry's own unadopted staging is released"
        );
        env_store
            .release_process_execution_env(
                &crate::ArtifactOwner::process(retained.id.clone()),
                &first_ref,
            )
            .await
            .expect("release the process edge");
        assert_eq!(
            env_store
                .get_process_execution_env(&first_ref)
                .await
                .expect("read the reclaimed environment"),
            None,
            "the process owner holds the environment, and the staging owner is retired"
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
            _process_id: &crate::ProcessId,
            _delivery_key: &str,
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
    }

    /// ADR 0107: a lash-derived start key is trusted. A retry under a
    /// retained key whose content changed returns the retained process
    /// untouched, and the environment it staged is released rather than
    /// attached to a process that never submitted it. (A host key fences its
    /// content instead; the registry conformance laws cover that.)
    #[tokio::test]
    async fn a_changed_content_retry_returns_the_retained_process_and_releases_its_staging() {
        let key = "changed-content-start";
        let start_key =
            crate::StartKey::for_trigger_delivery(key, "subscription", "incarnation", 1);
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
                .publish_process_execution_env(
                    &crate::ArtifactOwner::process_start(&crate::ProcessCommand::start_effect_id(
                        Some(&start_key),
                    )),
                    &env_ref,
                    &bytes,
                )
                .await
                .is_err(),
            "the released staging owner must fence a late staging publication"
        );
    }
}
