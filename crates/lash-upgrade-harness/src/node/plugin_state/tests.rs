use super::*;
#[cfg(feature = "synthetic-next")]
use lash::StoreSet as _;
use lash_core::compat::VersionRange;

fn probe(native: u32) -> ProbePlugin {
    ProbePlugin {
        native: FormatVersion::new(native).unwrap(),
        calls: Arc::default(),
    }
}

async fn snapshot(
    stores: &Arc<dyn lash::StoreSet>,
    session: &SessionId,
) -> Result<lash_core::store::SessionWindowRead> {
    stores
        .session_store_factory()
        .load_session_window(session, lash_core::store::WindowSelector::Current)
        .await?
        .context("callback snapshot")
}

async fn rollback(stores: Arc<dyn lash::StoreSet>) -> Result<()> {
    let session = SessionId::fixture("callback-rollback");
    let old = probe(1);
    let next = probe(2);
    let old_calls = Arc::clone(&old.calls);
    let next_calls = Arc::clone(&next.calls);
    let n_generation = callback_on(Arc::clone(&stores), &session, old.clone()).await?;
    let n = snapshot(&stores, &session).await?;
    let store = stores.session_store_factory();
    let state = lash_core::store::window_state(n.clone(), store.fleet_format())?.state;
    assert_eq!(counter(&old, state.plugin_state().unwrap()), Some(7));
    assert_eq!(old_calls.load(Ordering::SeqCst), 1);
    assert_eq!(n.config.plugin_config.get(PLUGIN).unwrap()["step"], 1);
    assert!(n.config.model.is_some());

    let next_generation = callback_on(Arc::clone(&stores), &session, next.clone()).await?;
    assert_ne!(
        n_generation, next_generation,
        "the plugin revision changes the executable route"
    );
    let rolled = snapshot(&stores, &session).await?;
    assert_eq!(
        rolled.config, n.config,
        "all namespaces and the model route survive changed defaults"
    );
    let state = lash_core::store::window_state(rolled, store.fleet_format())?.state;
    assert_eq!(
        state.plugin_state().unwrap().plugins[PLUGIN].format_version,
        FormatVersion::ONE
    );
    assert_eq!(counter(&old, state.plugin_state().unwrap()), Some(8));
    assert_eq!(next_calls.load(Ordering::SeqCst), 1);

    let rollback_generation = callback_on(Arc::clone(&stores), &session, old).await?;
    assert_eq!(
        rollback_generation, n_generation,
        "rollback uses N's own lane"
    );
    let onward = snapshot(&stores, &session).await?;
    assert_eq!(onward.config, n.config);
    let state = lash_core::store::window_state(onward, store.fleet_format())?.state;
    assert_eq!(
        state.plugin_state().unwrap().plugins[PLUGIN]
            .values
            .get("count"),
        Some(&serde_json::json!(9))
    );
    assert_eq!(old_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        store.plugin_writers().await?.permitted_writer(PLUGIN)?,
        VersionRange::exactly(1)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollback_callbacks_preserve_config_and_routes_sqlite_memory() -> Result<()> {
    rollback(Arc::new(lash::sqlite::SqliteStoreSet::memory().await?)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rollback_callbacks_preserve_config_and_routes_sqlite_file() -> Result<()> {
    let directory = tempfile::tempdir()?;
    rollback(Arc::new(
        lash::sqlite::SqliteStoreSet::open(directory.path().join("lash.db")).await?,
    ))
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run inside a private pg16 gate"]
async fn rollback_callbacks_preserve_config_and_routes_postgres_overlap() -> Result<()> {
    let database = lash_postgres_store::testing::IsolatedDatabase::create(
        &lash_postgres_store::testing::required_database_url(),
    )
    .await;
    let directory = tempfile::tempdir()?;
    let stores = super::super::open_stores(&super::super::StoreArgs {
        store: super::super::StoreSpec::Postgres(database.url().to_owned()),
        data_dir: directory.path().to_owned(),
    })
    .await?;
    rollback(stores).await
}

#[cfg(feature = "synthetic-next")]
async fn retained_history(stores: lash::sqlite::SqliteStoreSet) -> Result<()> {
    let stores = Arc::new(stores);
    let ports = Arc::clone(&stores) as Arc<dyn lash::StoreSet>;
    let session = SessionId::fixture("plugin-history");
    let old = probe(1);
    callback_on(Arc::clone(&ports), &session, old.clone()).await?;
    let before = snapshot(&ports, &session).await?;
    ports
        .session_store_factory()
        .pin(&session, &lash_core::Target::Revision(before.head_revision))
        .await?;
    let bytes = before
        .checkpoint
        .as_ref()
        .unwrap()
        .component_body(lash_core::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
        .unwrap()
        .to_vec();
    let head = lash_core::store::SessionHeadRef {
        generation: ports
            .session_store_factory()
            .read_session_state_version(&session)
            .await?,
        revision: before.head_revision,
        leaf: before.window.nodes.last().map(|node| node.node_id.clone()),
        checkpoint: before.checkpoint_ref,
    };
    callback_on(Arc::clone(&ports), &session, probe(2)).await?;
    let retired = lash_core::engine::BuildGeneration::for_test("history-finalize");
    stores.generation_drain().mark_draining(&retired, 1).await?;
    let declaration = probe(2).plugin_declaration();
    stores
        .finalize(
            &retired,
            &lash_core::store::fleet_finalize::NoDeployments,
            &[PluginWriterRegistration {
                plugin: PLUGIN.into(),
                native: declaration.format_version,
                writable: declaration.writable_formats,
            }],
            2,
        )
        .await?;
    callback_on(Arc::clone(&ports), &session, probe(2)).await?;
    let current = snapshot(&ports, &session).await?;
    let state = lash_core::store::window_state(
        current.clone(),
        ports.session_store_factory().fleet_format(),
    )?
    .state;
    assert_eq!(
        state.plugin_state().unwrap().plugins[PLUGIN]
            .format_version
            .get(),
        2
    );
    assert_eq!(counter(&probe(2), state.plugin_state().unwrap()), Some(9));
    assert_eq!(current.config.model, before.config.model);
    assert_eq!(
        current.config.plugin_config.get(PLUGIN),
        before.config.plugin_config.get(PLUGIN)
    );
    ports.session_store_factory().gc_unreachable().await?;
    let retained = ports
        .session_store_factory()
        .load_session_window(&session, lash_core::store::WindowSelector::Admitted(head))
        .await?
        .context("retained v1 history")?;
    assert_eq!(
        retained
            .checkpoint
            .as_ref()
            .unwrap()
            .component_body(lash_core::store::PLUGIN_STATE_CHECKPOINT_COMPONENT),
        Some(bytes.as_slice())
    );
    let historical =
        lash_core::store::window_state(retained, ports.session_store_factory().fleet_format())?
            .state;
    assert_eq!(counter(&old, historical.plugin_state().unwrap()), Some(7));
    assert_eq!(
        historical.plugin_state().unwrap().plugins[PLUGIN].format_version,
        FormatVersion::ONE
    );
    let decoded = PluginHost::new(vec![Arc::new(probe(2))])
        .decode_state(historical.plugin_state().unwrap())?;
    assert_eq!(counter(&probe(2), &decoded), Some(7));
    Ok(())
}

#[cfg(feature = "synthetic-next")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retained_v1_plugin_history_survives_finalize_sqlite_memory() -> Result<()> {
    retained_history(lash::sqlite::SqliteStoreSet::memory().await?).await
}

#[cfg(feature = "synthetic-next")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retained_v1_plugin_history_survives_finalize_sqlite_file() -> Result<()> {
    let directory = tempfile::tempdir()?;
    retained_history(lash::sqlite::SqliteStoreSet::open(directory.path().join("lash.db")).await?)
        .await
}
