//! Typed causes survive the facade on the durable substrate (FIG-5307).

use super::*;

use lash_core::PluginStateError;
use lash_core::facade_support::SessionPlugin;
use lash_core::plugin::SessionReadyContext;
use lash_execution_contracts::plugin_error::PluginError;

/// FIG-5500: cold open requires recorded identity even when the row is root.
#[tokio::test]
async fn a_cold_open_refuses_a_head_without_identity_even_for_root() {
    use lash_core_store::checkpoint_graph::{EncodeBudget, ExplicitLimit, GraphStaging, config};

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
            .build(crate::tests::host::runtime_lease_owner())
            .expect("core")
    };
    let creating = build();
    for id in ["root", "host-chosen-session"] {
        creating
            .session(crate::SessionId::from(id))
            .create(crate::SessionCreation::root(mock_session_spec()))
            .await
            .expect("create with explicit identity");
    }
    creating.shutdown().await.expect("shutdown creator");
    let reopened = build();
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let mut roots_without_identity = Vec::new();
    for id in ["root", "host-chosen-session"] {
        let session = reopened
            .session(crate::SessionId::from(id))
            .open()
            .await
            .expect("recorded identity permits cold open");
        assert_eq!(session.session_id().as_str(), id);
        drop(session);
        let head = lash_core::SessionCommitStore::load_session_head_meta(
            reopened.store_factory.as_ref(),
            &crate::SessionId::from(id),
        )
        .await
        .expect("read recorded head")
        .expect("head");
        let mut value = serde_json::to_value(head.payload()).expect("head value");
        value
            .as_object_mut()
            .expect("head object")
            .remove("session_id");
        let root = config::stage(&mut staging, &value).expect("valid graph without identity");
        roots_without_identity.push((id, root));
    }
    reopened.shutdown().await.expect("shutdown reader");
    let mut raw = rusqlite::Connection::open(&path).expect("raw SQLite connection");
    let tx = raw.transaction().expect("fixture transaction");
    // Keep canonical envelopes, hashes and edges intact: only the recorded
    // head's identity is absent, so cold open must reach its typed refusal.
    for node in staging.nodes().values() {
        tx.execute(
            "INSERT OR IGNORE INTO blobs (hash, content, encoding) VALUES (?1, ?2, 1)",
            rusqlite::params![node.id.to_hex(), node.bytes.as_ref()],
        )
        .expect("install config node");
    }
    for node in staging.nodes().values() {
        for child in node.children().expect("canonical children") {
            tx.execute(
                "INSERT OR IGNORE INTO blob_edges (parent_hash, child_hash) VALUES (?1, ?2)",
                rusqlite::params![node.id.to_hex(), child.to_hex()],
            )
            .expect("install config edge");
        }
    }
    for (id, root) in roots_without_identity {
        assert_eq!(
            tx.execute(
                "UPDATE session_revisions SET config_ref = ?1 WHERE session_id = ?2",
                rusqlite::params![root.to_hex(), id],
            )
            .expect("replace only the stored identity"),
            1,
        );
    }
    tx.commit().expect("publish identity-free graphs");
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

/// A plugin whose readiness records the value its namespace holds at `k`
/// (mode 0), decodes that value as a type it cannot read (mode 1), or
/// encodes a command value that has no JSON form (mode 2).
#[derive(Clone)]
struct StateHook {
    mode: Arc<AtomicUsize>,
    seen: Arc<std::sync::Mutex<Vec<Option<serde_json::Value>>>>,
}

impl StateHook {
    fn new() -> Self {
        Self {
            mode: Arc::new(AtomicUsize::new(0)),
            seen: Arc::default(),
        }
    }

    fn core(&self, backend: crate::Backend) -> LashCore {
        explicit_ephemeral_facets(LashCore::standard_builder(backend))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .plugin(Arc::new(self.clone()))
            .build(crate::tests::host::runtime_lease_owner())
            .expect("core")
    }
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

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<crate::plugins::PluginRecipe, crate::plugins::PluginError> {
        let mut reg = crate::plugins::PluginRegistrar::<StateHook>::new(
            crate::plugins::PluginFactory::id(self.as_ref()),
        );
        StateHook::declare(&mut reg)?;
        let id = crate::plugins::PluginFactory::id(self.as_ref());
        Ok(crate::plugins::PluginRecipe::new(
            id,
            reg,
            move |_ctx, _state| Ok(Arc::new(self.as_ref().clone())),
        ))
    }
}

impl StateHook {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("typed-state")
    }
}
impl lash_core::plugin::PluginMetadata for StateHook {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
    }
}

impl SessionPlugin for StateHook {
    fn id(&self) -> &'static str {
        "typed-state"
    }

    fn session_ready(&self, ctx: SessionReadyContext) -> std::result::Result<(), PluginError> {
        match self.mode.load(Ordering::SeqCst) {
            1 => {
                ctx.state.get_as::<u64>("k")?;
            }
            2 => {
                let keyed = std::collections::BTreeMap::from([((1, 2), 3)]);
                lash_execution_contracts::state_commands::StateCommands::new()
                    .set_as("k", &keyed)?;
            }
            _ => {
                self.seen.lock().expect("seen").push(ctx.state.get("k"));
                return Ok(());
            }
        }
        Err(PluginError::Registration(format!(
            "the state codec did not refuse; the namespace held {:?}",
            ctx.state.keys()
        )))
    }
}
impl StateHook {
    fn declare(
        _reg: &mut crate::plugins::PluginRegistrar<Self>,
    ) -> std::result::Result<(), crate::plugins::PluginError> {
        Ok(())
    }
}

/// Create session `id` on `backend` and run its first turn on a core that
/// installs `hook`, then shut that core down.
async fn run_first_turn(backend: &crate::Backend, hook: &StateHook, id: &str) {
    let core = hook.core(backend.clone());
    core.session(crate::SessionId::parse(id).expect("id"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created")
        .send(crate::TurnInput::text("ready"))
        .output()
        .await
        .expect("the session's first turn answers");
    core.shutdown().await.expect("shutdown");
}

/// A cold session's readiness reads the namespace its first turn recorded,
/// which that turn's readiness already read from the factory's initial
/// values (FIG-5324).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_sessions_readiness_reads_its_recorded_namespace() {
    let backend = sqlite_memory_store_backend().await;
    let hook = StateHook::new();
    run_first_turn(&backend, &hook, "cold-namespace").await;
    let core = hook.core(backend);
    core.session(crate::SessionId::parse("cold-namespace").expect("id"))
        .durable()
        .await
        .expect("handle")
        .send(crate::TurnInput::text("cold"))
        .output()
        .await
        .expect("the cold turn answers");
    let seen = hook.seen.lock().expect("seen").clone();
    assert!(seen.len() >= 2, "both turns' readiness ran: {seen:?}");
    assert!(
        seen.iter()
            .all(|value| *value == Some(serde_json::json!("text"))),
        "every readiness reads the initialized value: {seen:?}"
    );
    core.shutdown().await.expect("shutdown");
}

/// A plugin's state-codec refusal at a cold session's readiness reaches the
/// host as a typed, terminal plugin-state error naming its key, both for a
/// command value it cannot encode and for a recorded value it cannot decode
/// (FIG-5324).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cold_sessions_readiness_state_refusal_reaches_the_host_typed() {
    let backend = sqlite_memory_store_backend().await;
    for mode in [2, 1] {
        let id = format!("cold-state-refusal-{mode}");
        let hook = StateHook::new();
        run_first_turn(&backend, &hook, &id).await;
        hook.mode.store(mode, Ordering::SeqCst);
        let core = hook.core(backend.clone());
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
        let state = error
            .plugin_state_refusal()
            .unwrap_or_else(|| panic!("the facade keeps the typed state refusal: {error:?}"));
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

fn stated_commit_and_retention(backend: crate::Backend) -> crate::LashCoreBuilder {
    LashCore::standard_builder(backend)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(crate::DataRetention::standard())
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
}

/// FIG-5491: execution budgets and delta coalescing are host decisions with
/// no default, so a core that lacks either is refused.
#[tokio::test]
async fn a_core_without_its_budgets_or_coalescing_choice_is_refused() {
    assert!(matches!(
        stated_commit_and_retention(sqlite_memory_store_backend().await)
            .delta_coalescing(crate::DeltaCoalescing::recommended())
            .build(crate::tests::host::runtime_lease_owner()),
        Err(EmbedError::MissingExecutionBudgets)
    ));
    assert!(matches!(
        stated_commit_and_retention(sqlite_memory_store_backend().await)
            .execution_budgets(crate::ExecutionBudgets::recommended())
            .build(crate::tests::host::runtime_lease_owner()),
        Err(EmbedError::MissingDeltaCoalescing)
    ));
}

/// FIG-5491: the budgets and coalescing a host states are the ones its core
/// runs under; nothing of the recommended preset stands in for them.
#[tokio::test]
async fn a_core_runs_under_the_budgets_and_coalescing_its_host_states() {
    let budgets = crate::ExecutionBudgets::new(crate::ExecutionBudgetsConfig {
        model_total: std::time::Duration::from_secs(90),
        tool_check_phase: std::time::Duration::from_secs(7),
        turn_hook_phase: std::time::Duration::from_secs(9),
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
    let core = stated_commit_and_retention(sqlite_memory_store_backend().await)
        .execution_budgets(budgets.clone())
        .delta_coalescing(crate::DeltaCoalescing::off())
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::tests::host::runtime_lease_owner())
        .expect("core");
    assert_eq!(core.env.core.control.execution_budgets, budgets);
    assert_eq!(
        core.env.core.control.delta_coalescing,
        crate::DeltaCoalescing::off()
    );
    core.shutdown().await.expect("shutdown");
}

/// D-DEFAULTS2: a host must state what it keeps; lash has no data retention
/// of its own to fall back on.
#[tokio::test]
async fn a_core_without_a_data_retention_statement_is_refused() {
    let builder = LashCore::standard_builder(sqlite_memory_store_backend().await)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(crate::ExecutionBudgets::recommended())
        .delta_coalescing(crate::DeltaCoalescing::recommended());
    assert!(matches!(
        builder.build(crate::tests::host::runtime_lease_owner()),
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
        .build(crate::tests::host::runtime_lease_owner())
        .expect("a core under a stated retention");
        let id = crate::SessionId::from("stated-retention");
        core.session(id.clone())
            .create(crate::SessionCreation::root(mock_session_spec()))
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
            .create(crate::SessionCreation::root(spec))
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
        .create(crate::SessionCreation::root(mock_session_spec()))
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
    let committed = session
        .durable()
        .read()
        .await
        .expect("read committed append")
        .expect("the session has a head");
    assert_eq!(
        committed.messages().len(),
        1,
        "the retry retains exactly one durable append"
    );
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
