use super::*;
use lash_core::facade_support::{
    PluginRegistrar, PluginSessionContext, ReconfigureError, SessionPlugin,
};
use lash_core::plugin::SessionReadyContext;
use lash_core::testing::{EffectLayer, LayeredEffectHost, ProcessRegistryFaults};
use lash_core::{PluginError, PluginStateError, RuntimeError, RuntimeErrorCode};
use std::error::Error;

#[path = "cleanup_fixture.rs"]
mod cleanup_fixture;

fn core(backend: lash_core::Backend) -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("core")
}

fn plugin_source<'a>(error: &'a (dyn Error + 'static)) -> Option<&'a PluginError> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(error) = error.downcast_ref::<PluginError>() {
            return Some(error);
        }
        if let Some(error) = error.downcast_ref::<Box<PluginError>>() {
            return Some(error.as_ref());
        }
        current = error.source();
    }
    None
}

#[tokio::test]
async fn a_native_tool_membership_refusal_preserves_its_cause() -> Result<()> {
    let backend = double_backend_explicit_reconcile().await;
    let core = core(backend);
    let session = core
        .session(crate::SessionId::parse("typed-tools").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    materialize_session(&session).await?;
    let tools = session.admin().tools();
    let before = tools.state().await?;
    let error = tools
        .set_membership_many(&[("tool:absent".into(), false)])
        .await
        .expect_err("unknown membership refuses");
    assert!(
        matches!(error.source().and_then(|e| e.downcast_ref::<ReconfigureError>()), Some(ReconfigureError::Validation(message)) if message == "unknown tool id `tool:absent`"),
        "validation is typed: {error:?}"
    );
    let after = tools.state().await?;
    assert_eq!(before.generation(), after.generation());
    assert_eq!(before.tool_manifests(), after.tool_manifests());
    assert!(error.is_terminal());
    assert!(!error.is_retryable());
    Ok(())
}

/// A plugin whose readiness decodes a stored value it cannot read (mode 1)
/// or encodes a command value that has no JSON form (mode 2). Mode 0
/// refuses nothing.
#[derive(Clone)]
struct StateHook {
    mode: Arc<AtomicUsize>,
    handle: Arc<StdMutex<Option<lash_core::PluginStateView>>>,
}
impl PluginFactory for StateHook {
    fn id(&self) -> &'static str {
        "typed-state"
    }

    fn initialize_state(
        &self,
        _: &lash_core::RuntimeOwner,
        _: &lash_core::PluginConfig,
    ) -> std::result::Result<std::collections::BTreeMap<String, serde_json::Value>, PluginError>
    {
        Ok([("k".into(), serde_json::json!("text"))].into())
    }
    fn build(
        &self,
        _: &PluginSessionContext,
    ) -> std::result::Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

impl lash_core::plugin::PluginDefinition for StateHook {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("typed-state")
    }
}
impl SessionPlugin for StateHook {
    fn id(&self) -> &'static str {
        "typed-state"
    }
    fn register(&self, registrar: &mut PluginRegistrar) -> std::result::Result<(), PluginError> {
        *self.handle.lock_recover() = Some(registrar.state());
        Ok(())
    }
    fn session_ready(&self, ctx: SessionReadyContext) -> std::result::Result<(), PluginError> {
        let generation = ctx.state.generation();
        match self.mode.load(Ordering::SeqCst) {
            0 => return Ok(()),
            1 => {
                ctx.state.get_as::<u64>("k")?;
            }
            2 => {
                let keyed = std::collections::BTreeMap::from([((1, 2), 3)]);
                lash_core::plugin::StateCommands::new().set_as("k", &keyed)?;
            }
            _ => unreachable!(),
        }
        assert_eq!(ctx.state.generation(), generation);
        panic!("the state codec should refuse")
    }
}

fn assert_state_error(error: &PluginError, mode: usize) {
    let value = serde_json::to_value(error).expect("serialize plugin error");
    assert_eq!(
        value["type"], "state",
        "state category survives conversion: {error:?}"
    );
    let replayed: PluginError = serde_json::from_value(value.clone()).expect("journal JSON replay");
    assert_eq!(
        serde_json::to_value(replayed.clone()).expect("clone transport"),
        value
    );
    // A recorded process admission journals its refusal as the step's
    // `Result<_, PluginError>` answer.
    let recorded: std::result::Result<(), PluginError> = Err(replayed.clone());
    let journal = serde_json::to_vec(&recorded).expect("recorded admission refusal");
    let Err(refusal) = serde_json::from_slice::<std::result::Result<(), PluginError>>(&journal)
        .expect("replay recorded admission")
    else {
        panic!("journal outcome kind");
    };
    assert_eq!(serde_json::to_value(refusal).expect("journal cause"), value);
    let state = replayed
        .source()
        .and_then(|e| e.downcast_ref::<PluginStateError>())
        .expect("typed state source");
    match (mode, state) {
        (1, PluginStateError::Decode { key, .. }) | (2, PluginStateError::Encode { key, .. }) => {
            assert_eq!(key, "k");
        }
        other => panic!("wrong state fields: {other:?}"),
    }
    assert!(!error.is_retryable());
    assert!(error.is_terminal());
}

#[tokio::test]
async fn a_native_cold_open_preserves_state_codec_refusals() -> Result<()> {
    let backend = double_backend_explicit_reconcile().await;
    for mode in 1..=2 {
        let id = SessionId::fixture(format!("native-state-codec-{mode}"));
        let mode_control = Arc::new(AtomicUsize::new(0));
        let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .plugin(Arc::new(StateHook {
                mode: Arc::clone(&mode_control),
                handle: Arc::default(),
            }))
            .build(crate::testing::runtime_lease_owner())?;
        let session = core.session(id.clone()).created().await.open().await?;
        materialize_session(&session).await?;
        session.park().await?;
        mode_control.store(mode, Ordering::SeqCst);
        let error = core
            .session(id)
            .open()
            .await
            .err()
            .expect("native readiness refuses");
        let plugin = match &error {
            EmbedError::Plugin(plugin) | EmbedError::Session(SessionError::Plugin(plugin)) => {
                plugin
            }
            other => panic!("facade retains plugin source: {other:?}"),
        };
        assert_state_error(plugin, mode);
        assert!(!error.is_retryable());
        assert!(error.is_terminal());
    }
    Ok(())
}
struct CleanupLayer {
    step: usize,
    error: RuntimeError,
}
#[async_trait]
impl EffectLayer for CleanupLayer {
    async fn revoke_await_events_for_session(
        &self,
        inner: &dyn lash_core::AwaitEventResolver,
        id: &SessionId,
    ) -> std::result::Result<(), RuntimeError> {
        if self.step == 2 {
            return Err(self.error.clone());
        }
        inner.revoke_await_events_for_session(id).await
    }
    async fn retire_effect_journal(
        &self,
        inner: &dyn lash_core::EffectHost,
        retirement: lash_core::EffectJournalRetirement,
    ) -> std::result::Result<usize, RuntimeError> {
        if self.step == 3 {
            return Err(self.error.clone());
        }
        inner.retire_effect_journal(retirement).await
    }
}
struct DeleteExecution<'a> {
    admin: lash_core::SessionAdministration,
    scoped: lash_core::ScopedEffectController<'a>,
}
impl lash_core::SessionDeleteExecution for DeleteExecution<'_> {
    fn administration(&self) -> &lash_core::SessionAdministration {
        &self.admin
    }
    fn scoped<'a>(
        &'a self,
        _: lash_core::AdmittedScope,
    ) -> std::result::Result<lash_core::ScopedEffectController<'a>, RuntimeError> {
        Ok(self.scoped.clone())
    }
}

async fn cleanup_law() -> Result<()> {
    let backend = double_backend_explicit_reconcile().await;
    let core = core(backend);
    for recorded in [false, true] {
        for step in 0..=4 {
            for transient in [false, true] {
                let id = format!("typed-cleanup-{recorded}-{step}-{transient}");
                let id = &lash_core::SessionId::fixture(id);
                if recorded {
                    core.session((id).clone())
                        .create(crate::SessionCreation::root(mock_session_spec()))
                        .await?;
                }
                let original = core.session_administration().await;
                let refusal = lash_core::store::StoreRefusal::WriterFenced {
                    recorded: 7,
                    writable: lash_core::compat::VersionRange::exactly(3),
                };
                let plugin = if transient {
                    PluginError::Runtime(RuntimeError::new(
                        RuntimeErrorCode::StoreCommitContended,
                        "transient cleanup",
                    ))
                } else {
                    PluginError::StoreRefusal(refusal.clone())
                };
                let runtime = RuntimeError::new(
                    if transient {
                        RuntimeErrorCode::StoreCommitContended
                    } else if step == 3 {
                        RuntimeErrorCode::EffectJournalRetirementUnsupported
                    } else {
                        RuntimeErrorCode::AwaitEventUnsupported
                    },
                    "cleanup cause",
                );
                let registry =
                    Arc::new(ProcessRegistryFaults::new(core.backend.process_registry()));
                if step == 0 {
                    registry.fail_next_session_delete(plugin.clone());
                }
                let triggers = Arc::new(cleanup_fixture::TriggerFault {
                    inner: core.backend.trigger_store(),
                    error: StdMutex::new((step == 1).then(|| plugin.clone())),
                });
                let storage = Arc::new(
                    lash_core::testing::runtime_helpers::RecordingDeploymentStore::over(
                        Arc::clone(original.store_factory()),
                    ),
                );
                let partial = lash_core::SessionBlobReclaimReport {
                    enumerated_blob_count: 4,
                    retained_blob_count: 1,
                    deleted_blob_count: 2,
                };
                if step == 4 {
                    storage.fail_next_delete(lash_core::MaintenanceFailure::failed(
                        lash_core::StoreError::Backend("partial cleanup".into()),
                        partial.clone(),
                    ));
                }
                let admin = lash_core::SessionAdministration::new(
                    storage,
                    Arc::new(LayeredEffectHost::new(
                        Arc::clone(original.effect_host()),
                        Arc::new(CleanupLayer {
                            step,
                            error: runtime.clone(),
                        }),
                    )),
                    Some(lash_core::ProcessWorkWiring::without_process_work(registry)),
                    Some(triggers),
                    Arc::clone(original.process_env_store()),
                    original.process_engines().clone(),
                    original.session_close().clone(),
                );
                let double = held_double(&core).expect("held double");
                let handler = double
                    .open_handler(lash_core::AdmittedScope::session_delete(SessionId::from(
                        id,
                    )))
                    .await
                    .expect("delete handler");
                let execution = DeleteExecution {
                    admin,
                    scoped: handler.scoped(),
                };
                let result = LashCore::delete_session(
                    lash_core::SessionDeleteContext::from_execution(&execution, id)?,
                )
                .await;
                let failure: &dyn Error = match &result {
                    Ok(crate::SessionDeletion::Closing(closing)) if recorded => {
                        assert_eq!(closing.session_id, id);
                        assert!(closing.obligation.is_some());
                        let crate::SessionDeleteWait::Failed(failure) = &closing.waiting else {
                            panic!("cleanup failed: {closing:?}");
                        };
                        let correct_step = matches!(
                            (step, failure),
                            (0, crate::SessionDeleteFailure::Process { .. })
                                | (1, crate::SessionDeleteFailure::Triggers { .. })
                                | (2, crate::SessionDeleteFailure::Waits { .. })
                                | (3, crate::SessionDeleteFailure::Journal { .. })
                                | (4, crate::SessionDeleteFailure::Storage(_))
                        );
                        assert!(correct_step, "step retained: {failure:?}");
                        if step < 4 {
                            assert_eq!(failure.is_retryable(), transient);
                            assert_eq!(failure.is_terminal(), !transient);
                        }
                        if let crate::SessionDeleteFailure::Storage(failure) = failure {
                            assert_eq!(failure.partial, partial);
                        }
                        failure
                    }
                    Ok(crate::SessionDeletion::Absent { session_id }) if !recorded => {
                        // No close, no cleanup (ADR 0049): the faulted step
                        // never ran, so nothing of it surfaces.
                        assert_eq!(session_id, id);
                        drop(execution);
                        handler.close().await.expect("close handler");
                        continue;
                    }
                    other => panic!(
                        "cleanup transport recorded={recorded} step={step} transient={transient}: {other:?}"
                    ),
                };
                if step < 2 {
                    let found = plugin_source(failure).expect("typed process/trigger source");
                    assert_eq!(
                        serde_json::to_value(found).expect("cause JSON"),
                        serde_json::to_value(&plugin).expect("expected JSON")
                    );
                } else if step < 4 {
                    let mut cursor = Some(failure);
                    let mut found = None;
                    while let Some(error) = cursor {
                        if let Some(error) = error.downcast_ref::<RuntimeError>() {
                            found = Some(error);
                            break;
                        }
                        if let Some(error) = error.downcast_ref::<Box<RuntimeError>>() {
                            found = Some(error.as_ref());
                            break;
                        }
                        cursor = error.source();
                    }
                    let found = found.expect("typed wait/journal runtime source");
                    assert_eq!(found.code, runtime.code);
                    assert_eq!(found.message, runtime.message);
                }
                drop(execution);
                handler.close().await.expect("close handler");
            }
        }
    }
    Ok(())
}
#[tokio::test]
async fn session_cleanup_failures_preserve_step_and_source() -> Result<()> {
    cleanup_law().await
}
