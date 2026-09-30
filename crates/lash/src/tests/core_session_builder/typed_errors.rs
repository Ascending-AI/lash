use super::*;
use lash_core::facade_support::{
    PluginRegistrar, PluginSessionContext, ReconfigureError, SessionPlugin,
};
use lash_core::plugin::SessionReadyContext;
use lash_core::testing::{EffectLayer, LayeredEffectHost, ProcessRegistryFaults};
use lash_core::{PluginError, PluginStateEdit, PluginStateError, RuntimeError, RuntimeErrorCode};
use std::error::Error;

#[path = "cleanup_fixture.rs"]
mod cleanup_fixture;

#[allow(
    clippy::disallowed_methods,
    reason = "test fixture reads the PostgreSQL service URL"
)]
async fn backend(postgres: bool) -> (lash_core::Backend, Option<Box<dyn std::any::Any>>) {
    if !postgres {
        return (double_backend_explicit_reconcile().await, None);
    }
    let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
        .expect("PostgreSQL law requires a database URL");
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::PostgresStorage::connect(database.url())
        .await
        .expect("connect PostgreSQL");
    let attachments = tempfile::tempdir().expect("attachments");
    let stores = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        &storage,
        Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            attachments.path(),
        )),
    )) as Arc<dyn lash_core::StoreSet>;
    let backend = double_backend_over_explicit_reconcile(
        lash_restate_test::ServerConfig::default(),
        move |_| stores,
    )
    .await;
    (backend, Some(Box::new((database, storage, attachments))))
}

fn core(backend: lash_core::Backend) -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
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

struct EmptyTools;
#[async_trait]
impl ToolProvider for EmptyTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        Vec::new()
    }
    fn resolve_contract(&self, _: &str) -> Option<Arc<lash_core::ToolContract>> {
        None
    }
    async fn execute(&self, _: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        unreachable!("no tools")
    }
}

async fn tool_law(postgres: bool) -> Result<()> {
    let (backend, _held) = backend(postgres).await;
    let core = core(backend);
    let session = core.session("typed-tools").created().await.open().await?;
    let tools = session.admin().tools();
    let source = tools.add_provider(Arc::new(EmptyTools)).await?;
    tools.remove_source(&source).await?;
    let before = tools.state().await?;
    let error = tools
        .remove_source(&source)
        .await
        .expect_err("repeat removal refuses");
    assert!(
        matches!(error.source().and_then(|e| e.downcast_ref::<ReconfigureError>()), Some(ReconfigureError::UnknownSource(id)) if id == source.id()),
        "unknown source is typed: {error:?}"
    );
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

#[tokio::test]
async fn tool_admin_preserves_reconfigure_variants() -> Result<()> {
    tool_law(false).await
}
#[tokio::test]
#[ignore = "requires PostgreSQL service"]
async fn tool_admin_preserves_reconfigure_variants_on_postgres() -> Result<()> {
    tool_law(true).await
}

#[derive(Clone)]
struct StateHook {
    mode: Arc<AtomicUsize>,
    handle: Arc<StdMutex<Option<lash_core::PluginStateStore>>>,
}
impl PluginFactory for StateHook {
    fn id(&self) -> &'static str {
        "typed-state"
    }
    fn build(
        &self,
        _: &PluginSessionContext,
    ) -> std::result::Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(self.clone()))
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
        let mode = self.mode.load(Ordering::SeqCst);
        if mode == 0 {
            return Ok(());
        }
        if mode == 3 {
            for key in ["a", "b", "c"] {
                ctx.state.set(key, serde_json::json!("x".repeat(32766)))?;
            }
        }
        let generation = ctx.state.generation();
        let before = ctx.state.get("k");
        let keys = ctx.state.keys();
        let result = match mode {
            1 => ctx.state.set("", serde_json::json!(true)),
            2 => ctx.state.set("k", serde_json::json!("x".repeat(32768))),
            3 => ctx.state.set("k", serde_json::json!("x".repeat(32766))),
            4 => ctx.state.apply_guarded(
                generation + 1,
                vec![PluginStateEdit::Set {
                    key: "k".into(),
                    value: serde_json::json!(true),
                }],
            ),
            _ => unreachable!(),
        };
        assert_eq!(ctx.state.generation(), generation);
        assert_eq!(ctx.state.get("k"), before);
        assert_eq!(ctx.state.keys(), keys);
        result?;
        panic!("hook edit should be refused")
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
        (
            1,
            PluginStateError::InvalidKey {
                key,
                reason: lash_core::KeyRejection::Empty,
            },
        ) => assert!(key.is_empty()),
        (2, PluginStateError::ValueTooLarge { key, bytes, limit }) => {
            assert_eq!(key, "k");
            assert_eq!((*bytes, *limit), (32770, 32768));
        }
        (3, PluginStateError::StoreTooLarge { bytes, limit }) => {
            assert_eq!((*bytes, *limit), (131093, 131072));
        }
        (4, PluginStateError::GenerationConflict { expected, actual }) => {
            assert_eq!((*expected, *actual), (1, 0))
        }
        other => panic!("wrong state fields: {other:?}"),
    }
    assert!(!error.is_retryable());
    assert_eq!(error.is_terminal(), mode != 4);
}

#[test]
fn plugin_state_hook_errors_keep_their_structured_cause_conversion() {
    for (mode, error) in [
        (
            1,
            PluginStateError::InvalidKey {
                key: String::new(),
                reason: lash_core::KeyRejection::Empty,
            },
        ),
        (
            2,
            PluginStateError::ValueTooLarge {
                key: "k".into(),
                bytes: 32770,
                limit: 32768,
            },
        ),
        (
            3,
            PluginStateError::StoreTooLarge {
                bytes: 131093,
                limit: 131072,
            },
        ),
        (
            4,
            PluginStateError::GenerationConflict {
                expected: 1,
                actual: 0,
            },
        ),
    ] {
        assert_state_error(&error.into(), mode);
    }
}

async fn state_law(postgres: bool) -> Result<()> {
    let (backend, _held) = backend(postgres).await;
    for mode in 1..=4 {
        for rematerialize in [false, true] {
            let id = format!("typed-state-{mode}-{rematerialize}");
            let mode_control = Arc::new(AtomicUsize::new(if rematerialize { 0 } else { mode }));
            let core = explicit_ephemeral_facets(LashCore::standard_builder(
                backend.clone(),
                crate::TurnBudget::Unbounded,
            ))
            .serve_test_model(mock_provider(), mock_model_spec())
            .plugin(Arc::new(StateHook {
                mode: Arc::clone(&mode_control),
                handle: Arc::default(),
            }))
            .build(crate::testing::runtime_lease_owner())?;
            let created = core
                .session(id.as_str())
                .create(crate::SessionCreation::default())
                .await;
            let error = if rematerialize {
                created?;
                let session = core.session(id.as_str()).open().await?;
                session.park().await?;
                mode_control.store(mode, Ordering::SeqCst);
                core.session(id.as_str())
                    .open()
                    .await
                    .err()
                    .expect("ready refuses on reopen")
            } else {
                match created {
                    Err(error) => error,
                    Ok(_) => core
                        .session(id.as_str())
                        .open()
                        .await
                        .err()
                        .expect("ready refuses on create"),
                }
            };
            let plugin = match &error {
                EmbedError::Plugin(plugin) | EmbedError::Session(SessionError::Plugin(plugin)) => {
                    plugin
                }
                other => panic!("facade retains plugin source: {other:?}"),
            };
            assert_state_error(plugin, mode);
            assert!(!error.is_retryable());
            assert_eq!(error.is_terminal(), mode != 4);
        }
    }
    Ok(())
}
#[tokio::test]
async fn plugin_state_hook_errors_keep_their_structured_cause() -> Result<()> {
    state_law(false).await
}
#[tokio::test]
#[ignore = "requires PostgreSQL service"]
async fn plugin_state_hook_errors_keep_their_structured_cause_on_postgres() -> Result<()> {
    state_law(true).await
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

async fn cleanup_law(postgres: bool) -> Result<()> {
    let (backend, _held) = backend(postgres).await;
    let core = core(backend);
    for recorded in [false, true] {
        for step in 0..=4 {
            for transient in [false, true] {
                let id = format!("typed-cleanup-{recorded}-{step}-{transient}");
                let id = id.as_str();
                if recorded {
                    core.session(id)
                        .create(crate::SessionCreation::default())
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
                    Err(error) if !recorded => {
                        if step == 4 {
                            let EmbedError::SessionDeleteStorage {
                                session_id,
                                failure,
                            } = error
                            else {
                                panic!("storage cleanup carrier: {error:?}");
                            };
                            assert_eq!(session_id, id);
                            assert_eq!(failure.partial, partial);
                        }
                        if step < 4 {
                            let EmbedError::SessionDeleteCleanup {
                                session_id,
                                failure,
                            } = error
                            else {
                                panic!("typed cleanup carrier: {error:?}");
                            };
                            assert_eq!(session_id, id);
                            assert!(matches!(
                                (step, failure.as_ref()),
                                (0, crate::SessionDeleteFailure::Process { .. })
                                    | (1, crate::SessionDeleteFailure::Triggers { .. })
                                    | (2, crate::SessionDeleteFailure::Waits { .. })
                                    | (3, crate::SessionDeleteFailure::Journal { .. })
                            ));
                            assert_eq!(error.is_retryable(), transient);
                            assert_eq!(error.is_terminal(), !transient);
                        }
                        error
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
    cleanup_law(false).await
}
#[tokio::test]
#[ignore = "requires PostgreSQL service"]
async fn session_cleanup_failures_preserve_step_and_source_on_postgres() -> Result<()> {
    cleanup_law(true).await
}

#[test]
fn tool_admin_preserves_reconfigure_variants_conversion() {
    let error = EmbedError::from(ReconfigureError::GenerationMismatch {
        expected: 7,
        actual: 9,
    });
    assert!(matches!(
        &error,
        EmbedError::Reconfigure(ReconfigureError::GenerationMismatch {
            expected: 7,
            actual: 9
        })
    ));
    assert!(!error.is_retryable());
    assert!(!error.is_terminal());
    assert!(matches!(
        error
            .source()
            .and_then(|e| e.downcast_ref::<ReconfigureError>()),
        Some(ReconfigureError::GenerationMismatch {
            expected: 7,
            actual: 9
        })
    ));
}

#[test]
fn plugin_state_codec_and_key_errors_survive_journaling() {
    for error in [
        PluginStateError::InvalidKey {
            key: "x".repeat(129),
            reason: lash_core::KeyRejection::TooLong,
        },
        PluginStateError::InvalidKey {
            key: "a/".into(),
            reason: lash_core::KeyRejection::IllegalCharacter { at: 1, byte: b'/' },
        },
        PluginStateError::Encode {
            key: "k".into(),
            message: "deliberate encoding error".into(),
        },
        PluginStateError::Decode {
            key: "k".into(),
            message: "expected integer, found string".into(),
        },
    ] {
        let expected = error.clone();
        let plugin = PluginError::from(error);
        let replayed: PluginError =
            serde_json::from_slice(&serde_json::to_vec(&plugin).expect("encode")).expect("decode");
        assert!(matches!(&replayed, PluginError::State(found) if *found == expected));
        assert!(replayed.is_terminal());
        assert!(!replayed.is_retryable());
        assert_eq!(plugin.to_string(), replayed.to_string());
    }
}
