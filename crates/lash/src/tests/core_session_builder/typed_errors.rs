//! Typed causes survive the facade on the durable substrate (FIG-5307).

use super::*;
use lash_core::facade_support::{
    PluginRegistrar, PluginSessionContext, ReconfigureError, SessionPlugin,
};
use lash_core::plugin::SessionReadyContext;
use lash_core::{PluginError, PluginStateError};
use std::error::Error;

/// FIG-5500: cold open requires recorded identity even when the row is root.
#[tokio::test]
async fn a_cold_open_refuses_a_head_without_identity_even_for_root() {
    let files = tempfile::tempdir().expect("SQLite directory");
    let path = files.path().join("lash.db");
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(
            &path,
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("SQLite stores"),
    );
    let backend = lash_conformance::backend_over(stores);
    let build = || {
        explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .serve_sessions(false)
            .build(crate::testing::runtime_lease_owner())
            .expect("core")
    };
    let creating = build();
    for id in ["root", "host-chosen-session"] {
        creating
            .session(crate::SessionId::from(id))
            .create(crate::SessionCreation::root(
                crate::plugins::SessionToolAccess::ambient(),
                mock_session_spec(),
            ))
            .await
            .expect("create with explicit identity");
    }
    creating.shutdown().await.expect("shutdown creator");
    let reopened = build();
    for id in ["root", "host-chosen-session"] {
        let session = reopened
            .session(crate::SessionId::from(id))
            .open()
            .await
            .expect("recorded identity permits cold open");
        assert_eq!(session.session_id().as_str(), id);
        drop(session);
    }
    reopened.shutdown().await.expect("shutdown reader");
    let raw = rusqlite::Connection::open(&path).expect("raw SQLite connection");
    assert_eq!(
        raw.execute(
            "UPDATE session_revisions SET head_json = json_remove(head_json, '$.session_id')",
            [],
        )
        .expect("remove only the stored identities"),
        2,
    );
    drop(raw);
    let refusing = build();
    for id in ["root", "host-chosen-session"] {
        let error = match refusing.session(crate::SessionId::from(id)).open().await {
            Ok(_) => panic!("a head missing identity must not reopen as {id}"),
            Err(error) => error,
        };
        assert!(
            matches!(
                &error,
                EmbedError::Session(lash_core::SessionError::Store {
                    source: StoreError::StoredDataCorrupt {
                        record_kind: "SessionHeadMeta",
                        message,
                    },
                    ..
                }) if message.contains("missing field `session_id`")
            ),
            "cold open keeps the truthful typed cause: {error:?}"
        );
        assert!(error.is_terminal(), "{error:?}");
        assert!(!error.is_retryable(), "{error:?}");
    }
    refusing.shutdown().await.expect("shutdown refusing core");
}

/// A refused tool-membership change answers its typed validation cause,
/// changes nothing and submits nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_native_tool_membership_refusal_preserves_its_cause() {
    use lash_core::facade_support::ToolStateFacadeOps as _;
    let core = standard_core_over(sqlite_memory_store_backend().await);
    core.session(crate::SessionId::parse("typed-tools").expect("id"))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await
        .expect("created")
        .send(crate::TurnInput::text("materialize the session"))
        .output()
        .await
        .expect("the first turn answers");
    let session = core
        .session(crate::SessionId::parse("typed-tools").expect("id"))
        .open()
        .await
        .expect("open");
    let tools = session.admin().tools();
    let before = tools.state().await.expect("tool state");
    let error = tools
        .set_membership_many(
            &[("tool:absent".into(), false)],
            "host:typed_errors:set_membership_many:33",
        )
        .await
        .expect_err("unknown membership refuses");
    assert!(
        matches!(error.source().and_then(|e| e.downcast_ref::<ReconfigureError>()), Some(ReconfigureError::Validation(message)) if message == "unknown tool id `tool:absent`"),
        "validation is typed: {error:?}"
    );
    let after = tools.state().await.expect("tool state");
    assert_eq!(
        before.recorded().map(|state| state.generation()),
        after.recorded().map(|state| state.generation())
    );
    assert_eq!(
        before.recorded().map(|state| state.tool_manifests()),
        after.recorded().map(|state| state.tool_manifests())
    );
    assert!(
        after.pending().is_empty(),
        "a refused change is never submitted"
    );
    assert!(error.is_terminal());
    assert!(!error.is_retryable());
    core.shutdown().await.expect("shutdown");
}

/// A plugin whose readiness decodes a stored value it cannot read (mode 1)
/// or encodes a command value that has no JSON form (mode 2). Mode 0
/// refuses nothing.
#[derive(Clone)]
struct StateHook {
    mode: Arc<AtomicUsize>,
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

    fn register(&self, _registrar: &mut PluginRegistrar) -> std::result::Result<(), PluginError> {
        Ok(())
    }

    fn session_ready(&self, ctx: SessionReadyContext) -> std::result::Result<(), PluginError> {
        match self.mode.load(Ordering::SeqCst) {
            1 => {
                ctx.state.get_as::<u64>("k")?;
            }
            2 => {
                let keyed = std::collections::BTreeMap::from([((1, 2), 3)]);
                lash_core::plugin::StateCommands::new().set_as("k", &keyed)?;
            }
            _ => return Ok(()),
        }
        Err(PluginError::Registration(format!(
            "the state codec did not refuse; the namespace held {:?}",
            ctx.state.keys()
        )))
    }
}

/// The typed plugin-state error somewhere in `error`'s source chain.
fn state_source<'a>(error: &'a (dyn Error + 'static)) -> Option<&'a PluginStateError> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(state) = error.downcast_ref::<PluginStateError>() {
            return Some(state);
        }
        current = error.source();
    }
    None
}

/// A plugin's state-codec refusal at a cold session's readiness reaches the
/// host as a typed, terminal plugin-state error naming its key, both for a
/// value it cannot decode and for one it cannot encode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "FIG-5324: a cold session's readiness sees an empty plugin namespace and a readiness refusal loses its typed cause"]
async fn a_native_cold_open_preserves_state_codec_refusals() {
    let backend = sqlite_memory_store_backend().await;
    for mode in 1..=2 {
        let id = format!("native-state-codec-{mode}");
        let mode_control = Arc::new(AtomicUsize::new(0));
        let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .plugin(Arc::new(StateHook {
                mode: Arc::clone(&mode_control),
            }))
            .build(crate::testing::runtime_lease_owner())
            .expect("core");
        let session = core
            .session(crate::SessionId::parse(&id).expect("id"))
            .create(crate::SessionCreation::root(
                crate::plugins::SessionToolAccess::ambient(),
                mock_session_spec(),
            ))
            .await
            .expect("created");
        session
            .send(crate::TurnInput::text("ready"))
            .output()
            .await
            .expect("the session's first turn answers");
        core.shutdown().await.expect("shutdown");

        mode_control.store(mode, Ordering::SeqCst);
        let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .plugin(Arc::new(StateHook {
                mode: Arc::clone(&mode_control),
            }))
            .build(crate::testing::runtime_lease_owner())
            .expect("core");
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            core.session(crate::SessionId::parse(&id).expect("id"))
                .durable()
                .await
                .expect("handle")
                .send(crate::TurnInput::text("cold"))
                .output(),
        )
        .await
        .expect("the cold turn settles")
        .expect_err("readiness refuses");
        let state = state_source(&error)
            .unwrap_or_else(|| panic!("the facade keeps the typed state source: {error:?}"));
        match (mode, state) {
            (1, PluginStateError::Decode { key, .. })
            | (2, PluginStateError::Encode { key, .. }) => assert_eq!(key, "k"),
            other => panic!("wrong state fields: {other:?}"),
        }
        assert!(!error.is_retryable(), "{error:?}");
        assert!(error.is_terminal(), "{error:?}");
        core.shutdown().await.expect("shutdown");
    }
}

/// FIG-5431: a host must choose how a deployment handles missing tool sources.
#[tokio::test]
async fn a_core_without_a_tool_loss_choice_is_refused() {
    let builder = LashCore::standard_builder(sqlite_memory_store_backend().await)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(crate::DataRetention::standard())
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1));
    assert!(matches!(
        builder.build(crate::testing::runtime_lease_owner()),
        Err(EmbedError::MissingToolSourcePolicy)
    ));
}

/// FIG-5491: execution budgets and delta coalescing are host decisions with
/// no default, so a core that lacks either is refused.
#[tokio::test]
async fn a_core_without_its_budgets_or_coalescing_choice_is_refused() {
    let stated = |backend| {
        LashCore::standard_builder(backend)
            .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
            .tool_source_policy(crate::tools::ToolSourcePolicy::Tolerate)
    };
    assert!(matches!(
        stated(sqlite_memory_store_backend().await)
            .delta_coalescing(crate::DeltaCoalescing::recommended())
            .build(crate::testing::runtime_lease_owner()),
        Err(EmbedError::MissingExecutionBudgets)
    ));
    assert!(matches!(
        stated(sqlite_memory_store_backend().await)
            .execution_budgets(crate::ExecutionBudgets::recommended())
            .build(crate::testing::runtime_lease_owner()),
        Err(EmbedError::MissingDeltaCoalescing)
    ));
}

/// FIG-5491: the budgets and coalescing a host states are the ones its core
/// runs under; nothing of the recommended preset stands in for them.
#[tokio::test]
async fn a_core_runs_under_the_budgets_and_coalescing_its_host_states() {
    let budgets = crate::ExecutionBudgets::new(crate::ExecutionBudgetsConfig {
        model_total: std::time::Duration::from_secs(90),
        control_phase: std::time::Duration::from_secs(7),
        stop_grace: std::time::Duration::from_secs(1),
        provider: crate::ProviderAttemptLimits::new(
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(10),
            std::time::Duration::from_secs(10),
            2,
        )
        .expect("valid provider limits"),
        agent_frame_switch_limit: std::num::NonZeroU32::new(3).expect("nonzero"),
    })
    .expect("valid budgets");
    assert_ne!(budgets, crate::ExecutionBudgets::recommended());
    let core = LashCore::standard_builder(sqlite_memory_store_backend().await)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(crate::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(budgets.clone())
        .delta_coalescing(crate::DeltaCoalescing::off())
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("core");
    assert_eq!(core.env.core.control.execution_budgets, budgets);
    assert_eq!(
        core.env.core.control.delta_coalescing,
        crate::DeltaCoalescing::off()
    );
    core.shutdown().await.expect("shutdown");
}

/// FIG-5491: tool authority is the creator's decision, and the session
/// records the one it states.
#[tokio::test]
async fn a_session_records_the_tool_authority_its_creator_states() {
    let core = standard_core_over(sqlite_memory_store_backend().await);
    let restricted = crate::plugins::SessionToolAccess::restricted([]).expect("no tools");
    assert_ne!(restricted, crate::plugins::SessionToolAccess::ambient());
    let id = crate::SessionId::from("restricted-at-creation");
    core.session(id.clone())
        .create(crate::SessionCreation::root(
            restricted.clone(),
            mock_session_spec(),
        ))
        .await
        .expect("created");
    let recorded =
        lash_core::SessionCommitStore::load_session_head_meta(core.store_factory.as_ref(), &id)
            .await
            .expect("head read")
            .expect("persisted head")
            .config;
    assert_eq!(recorded.tool_access, restricted);
    core.shutdown().await.expect("shutdown");
}

/// D-DEFAULTS2: a host must state what it keeps; lash has no data retention
/// of its own to fall back on.
#[tokio::test]
async fn a_core_without_a_data_retention_statement_is_refused() {
    let builder = LashCore::standard_builder(sqlite_memory_store_backend().await)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(crate::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(crate::ExecutionBudgets::recommended())
        .delta_coalescing(crate::DeltaCoalescing::recommended());
    assert!(matches!(
        builder.build(crate::testing::runtime_lease_owner()),
        Err(EmbedError::MissingDataRetention)
    ));
}

/// D-DEFAULTS2: the revision retention a host states is what each session
/// it creates records, in the store that releases revisions by it.
#[tokio::test]
async fn a_created_session_records_the_revision_retention_its_host_states() {
    let window = std::num::NonZeroU32::new(3).expect("three is nonzero");
    for stated in [
        crate::Retention::HeadOnly,
        crate::Retention::LastTurns(window),
        crate::Retention::UntilGc,
    ] {
        let core = explicit_ephemeral_facets(LashCore::standard_builder(
            sqlite_memory_store_backend().await,
        ))
        .data_retention(crate::DataRetention {
            session_revisions: stated,
            ..crate::DataRetention::standard()
        })
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("a core under a stated retention");
        let id = crate::SessionId::from("stated-retention");
        core.session(id.clone())
            .create(crate::SessionCreation::root(
                crate::plugins::SessionToolAccess::ambient(),
                mock_session_spec(),
            ))
            .await
            .expect("create");
        let session = core.session(id).open().await.expect("open");
        assert_eq!(session.retention().await.expect("retention"), stated);
        core.shutdown().await.expect("shutdown");
    }
}

/// FIG-5431: a creator must choose the session's stall bound.
#[tokio::test]
async fn a_session_without_a_stall_bound_is_refused() {
    let core = standard_core_over(sqlite_memory_store_backend().await);
    let spec = crate::SessionSpec::new(
        mock_llm_profile_spec().wire_model,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    );
    assert!(matches!(
        core.session(crate::SessionId::from("no-stall-choice"))
            .create(crate::SessionCreation::root(
                crate::plugins::SessionToolAccess::ambient(),
                spec
            ))
            .await,
        Err(EmbedError::MissingNoProgressBudget)
    ));
    core.shutdown().await.expect("shutdown");
}

/// FIG-5431: a retry after a lost acknowledgement reattaches to one mutation.
#[tokio::test]
async fn a_retried_admin_mutation_with_the_same_host_key_applies_once() {
    let core = standard_core_over(sqlite_memory_store_backend().await);
    core.session(crate::SessionId::from("retry-keyed-append"))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await
        .expect("create");
    let session = core
        .session(crate::SessionId::from("retry-keyed-append"))
        .open()
        .await
        .expect("open");
    let state = session.admin().state();
    let messages = || {
        vec![lash_core::PluginMessage::text(
            lash_core::MessageRole::User,
            "append once",
        )]
    };
    let first = state
        .append_messages(messages(), "host-stable-append".to_string())
        .await
        .expect("first submission");
    let retried = state
        .append_messages(messages(), "host-stable-append".to_string())
        .await
        .expect("retry submission");
    if let (crate::AdminMutation::Pending(first), crate::AdminMutation::Pending(retried)) =
        (&first, &retried)
    {
        assert_eq!(first, retried, "a retry carries the original receipt");
    }
    first
        .settle_with(
            &session.admin().commands(),
            crate::testing::admin_fixture_outcome,
        )
        .await
        .expect("first settles");
    retried
        .settle_with(
            &session.admin().commands(),
            crate::testing::admin_fixture_outcome,
        )
        .await
        .expect("retry settles");
    let snapshot = state.export().await;
    assert_eq!(
        snapshot
            .session_graph
            .nodes
            .iter()
            .filter(|node| matches!(
                &node.payload,
                lash_core::SessionNodePayload::Event {
                    event: lash_core::SessionHistoryRecord::Conversation(_)
                }
            ))
            .count(),
        1,
        "one append was committed"
    );
    core.shutdown().await.expect("shutdown");
}
