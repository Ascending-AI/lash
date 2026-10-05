use super::*;

struct ConstructionProbe {
    inner: StateDeriver,
    owners: Arc<StdMutex<Vec<lash_core::RuntimeOwner>>>,
}

impl lash_core::plugin::PluginFactory for ConstructionProbe {
    fn id(&self) -> &'static str {
        lash_core::plugin::PluginFactory::id(&self.inner)
    }

    fn build(
        &self,
        context: &lash_core::plugin::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError>
    {
        self.owners.lock_recover().push(context.owner.clone());
        lash_core::plugin::PluginFactory::build(&self.inner, context)
    }
}

impl lash_core::plugin::PluginDefinition for ConstructionProbe {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        <StateDeriver as lash_core::plugin::PluginDefinition>::declaration()
    }
}

async fn loaded(core: &LashCore, id: &SessionId) -> Result<lash_core::RuntimeSessionState> {
    let store = crate::session::resolve_existing_session(&core.store_factory, id).await?;
    crate::session::load_state_from_store(id, &store).await
}

async fn catalog_fork_records_child_admission(storage: Storage) -> Result<()> {
    let world = world(storage, false).await.expect("store fixture");
    let engine = Engine::Double(world.double);
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let owners = Arc::new(StdMutex::new(Vec::new()));
    let build_core = || {
        let calls = provider_calls.clone();
        let provider = crate::testing::TestProvider::builder()
            .kind("catalog-fork")
            .complete(move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(text_response(RAW)) }
            })
            .build()
            .into_handle();
        LashCore::standard_builder(engine.lash_backend())
            .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
            .serve_test_llm_profile(provider, mock_llm_profile_spec())
            .plugin(Arc::new(ConstructionProbe {
                inner: StateDeriver(hook_calls.clone()),
                owners: owners.clone(),
            }))
            .build(crate::testing::runtime_lease_owner())
            .expect("build the core")
    };
    let core = build_core();
    let parent_id = SessionId::from("catalog-fork-parent");
    let child_id = SessionId::from("catalog-fork-child");
    core.session(parent_id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?
        .send(TurnInput::text("record the parent's plugin state"))
        .output()
        .await?;
    let parent = loaded(&core, &parent_id).await?;
    let parent_bytes = parent
        .plugin_admission_snapshot()
        .expect("parent admission");
    let parent_view = lash_core::plugin::PluginNativeView::decode(
        &parent_bytes,
        lash_core::FleetFormat::current(),
    )?;
    assert_eq!(
        parent_view.request.owner,
        lash_core::RuntimeOwner::Session(parent_id.clone())
    );
    // The shared deriver runs accept, refuse and response once per turn.
    assert_eq!(hook_calls.load(Ordering::SeqCst), 3);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);

    core.fork_at(
        &parent_id,
        lash_core::Target::Revision(parent.head_revision),
        crate::ForkRequest {
            session_id: child_id.clone(),
            relation: lash_core::SessionRelation::Fork {
                source_session_id: parent_id.clone(),
                source_node_id: None,
            },
            observed_processes: Vec::new(),
        },
    )
    .await?;
    drop(core);

    // A fresh core must open the catalog fork without adopting its parent's
    // native admission or constructing capabilities before its own transition.
    let core = build_core();
    let child = core
        .session(child_id.clone())
        .open()
        .await
        .expect("a cold catalog fork opens under its own owner");
    drop(child);
    let seed = loaded(&core, &child_id).await?;
    assert!(seed.plugin_admission_snapshot().is_none());
    assert_eq!(seed.plugin_state(), parent.plugin_state());
    assert_eq!(seed.authority.plugin_config, parent.authority.plugin_config);
    assert_eq!(hook_calls.load(Ordering::SeqCst), 3);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 1);
    assert!(
        !owners
            .lock_recover()
            .contains(&lash_core::RuntimeOwner::Session(child_id.clone()))
    );

    let plugins = lash_core::facade_support::PluginHost::new(Vec::new()).defer_session(
        lash_core::plugin::PluginSessionRequest::creation(child_id.clone(), Default::default()),
    )?;
    assert!(matches!(
        plugins.adopt_native_view(&parent_bytes, lash_core::FleetFormat::current()),
        Err(lash_core::PluginError::State(
            lash_core::PluginStateError::EffectOwnerMismatch
        ))
    ));

    core.session(child_id.clone())
        .durable()
        .await?
        .send(TurnInput::text("record the child's plugin transition"))
        .output()
        .await?;
    let child = loaded(&core, &child_id).await?;
    let child_view = lash_core::plugin::PluginNativeView::decode(
        &child.plugin_admission_snapshot().expect("child admission"),
        lash_core::FleetFormat::current(),
    )?;
    assert_eq!(
        child_view.request.owner,
        lash_core::RuntimeOwner::Session(child_id.clone())
    );
    assert_ne!(child_view.request.id, parent_view.request.id);
    assert!(
        owners
            .lock_recover()
            .contains(&lash_core::RuntimeOwner::Session(child_id.clone()))
    );
    let inherited = &parent.plugin_state().expect("parent state").plugins["state-replay-deriver"];
    let child_state = &child.plugin_state().expect("child state").plugins["state-replay-deriver"];
    assert_eq!(inherited.generation, 2);
    assert_eq!(inherited.values["accepted"], serde_json::json!(17));
    assert_eq!(inherited.values["second"], serde_json::json!(23));
    let mut expected_child = inherited.values.clone();
    // The child's accepting hook applies another recorded add(23).
    expected_child.insert("second".into(), serde_json::json!(46));
    assert_eq!(child_state.values, expected_child);
    assert_eq!(child_state.generation, 4);
    assert_eq!(
        loaded(&core, &parent_id).await?.plugin_state(),
        parent.plugin_state()
    );
    assert_eq!(hook_calls.load(Ordering::SeqCst), 6);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    drop(core);

    let core = build_core();
    drop(core.session(child_id).open().await?);
    assert_eq!(hook_calls.load(Ordering::SeqCst), 6);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_fork_records_child_admission_sqlite_memory() -> Result<()> {
    catalog_fork_records_child_admission(Storage::SqliteMemory).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn catalog_fork_records_child_admission_sqlite_file() -> Result<()> {
    catalog_fork_records_child_admission(Storage::SqliteFile).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn catalog_fork_records_child_admission_postgres() -> Result<()> {
    catalog_fork_records_child_admission(Storage::Postgres).await
}
