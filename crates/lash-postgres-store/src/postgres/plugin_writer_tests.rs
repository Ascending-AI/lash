//! The fleet record's per-plugin writer ranges on PostgreSQL (FIG-4746):
//! every plugin state and config publication is admitted against them under
//! the guarded transaction. N is a storage whose writable range is `[1,1]`,
//! N+1 one whose range is `[1,2]`, over one isolated database.

// This file is test code; ambient env access is sanctioned here (the
// workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core_execution::compat::{CompatRefusal, VersionRange};
use lash_core_execution::store::RuntimeCommit;
use lash_core_execution::store::plugin_writers::PluginWriterRegistration;
use lash_core_execution::testing::store_fixtures::{
    commit_runtime_state_for_test, root_session_request,
};
use lash_core_execution::{
    FormatNamespace, FormatVersion, PluginNamespaceState, PluginState,
    ProcessExecutionEnvStore as _, RuntimeSessionState, RuntimeStore, SessionId, StoreError,
};

use crate::PostgresStorage;
use crate::testing::IsolatedDatabase;

const N: VersionRange = VersionRange::exactly(1);
const NEXT: VersionRange = VersionRange::between(1, 2);
const PLUGIN: &str = "format-probe";

fn version(value: u32) -> FormatVersion {
    FormatVersion::new(value).expect("a format version")
}

fn registration(native: u32, writable: &[u32]) -> PluginWriterRegistration {
    PluginWriterRegistration {
        plugin: PLUGIN.to_owned(),
        native: version(native),
        writable: writable.iter().copied().map(version).collect(),
    }
}

async fn isolated() -> Option<IsolatedDatabase> {
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping plugin writer law: database URL is not set");
        return None;
    };
    Some(IsolatedDatabase::create(&database_url).await)
}

async fn open_as(url: &str, writable: VersionRange) -> PostgresStorage {
    let pool = sqlx::PgPool::connect(url).await.expect("connect");
    crate::testing::from_pool_as(pool, &crate::PostgresHostConfig::default(), writable)
        .await
        .expect("open the store")
}

/// N and N+1 over one freshly provisioned store. In the synthetic build N+1
/// has run its expand first: the synthetic shape check admits only an
/// expanded catalog.
async fn fleet(database: &IsolatedDatabase) -> (PostgresStorage, PostgresStorage) {
    #[cfg(feature = "synthetic-next")]
    {
        let pool = sqlx::PgPool::connect(database.url())
            .await
            .expect("connect for the expand");
        crate::migrate::expand_for_testing(
            &pool,
            &crate::guarded_tx::WriterFence::new(NEXT, lash_core_execution::FleetFormat::current()),
        )
        .await
        .expect("N+1 expands the store");
        pool.close().await;
    }
    (
        open_as(database.url(), N).await,
        open_as(database.url(), NEXT).await,
    )
}

fn store(storage: &PostgresStorage) -> Arc<dyn RuntimeStore> {
    Arc::new(storage.store())
}

async fn recorded_range(storage: &PostgresStorage) -> Option<(i32, i32)> {
    sqlx::query_as(
        "SELECT min_format, max_format FROM lash_fleet_plugin_writers WHERE plugin_id = $1",
    )
    .bind(PLUGIN)
    .fetch_optional(storage.pool())
    .await
    .expect("read the writer range")
}

/// Everything a refused publication must leave alone.
async fn published(storage: &PostgresStorage) -> (i64, i64, i64, Vec<(String, String)>) {
    let count = |table: &'static str| async move {
        sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(storage.pool())
            .await
            .expect("count rows")
    };
    let heads = sqlx::query_as(
        "SELECT session_id, head_json::TEXT FROM lash_session_head JOIN lash_session_revisions USING (session_id, head_revision) ORDER BY session_id",
    )
    .fetch_all(storage.pool())
    .await
    .expect("read heads");
    (
        count("lash_session_meta").await,
        count("lash_blobs").await,
        count("lash_lashlang_artifacts").await,
        heads,
    )
}

fn state(session_id: &str) -> RuntimeSessionState {
    RuntimeSessionState {
        session_id: lash_core_execution::SessionId::fixture(session_id),
        ..RuntimeSessionState::new(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
        ))
    }
}

fn plugin_state(format: u32, value: u64) -> PluginState {
    PluginState {
        plugins: BTreeMap::from([(
            PLUGIN.to_owned(),
            PluginNamespaceState {
                format_version: version(format),
                generation: value,
                publication: Default::default(),
                fork: Default::default(),
                values: BTreeMap::from([("count".to_owned(), serde_json::json!(value))]).into(),
            },
        )]),
    }
}

async fn commit(
    store: &Arc<dyn RuntimeStore>,
    state: &mut RuntimeSessionState,
) -> Result<(), StoreError> {
    let receipt = commit_runtime_state_for_test(
        store,
        RuntimeCommit::persisted_state_for_test(state),
        "plugin-writer-law",
    )
    .await?;
    state.apply_persisted_commit_result(receipt);
    Ok(())
}

fn outside(error: &StoreError, namespace: FormatNamespace, writer: u32, permitted: VersionRange) {
    assert!(
        matches!(
            error,
            StoreError::Incompatible {
                refusal: CompatRefusal::PluginWriterOutsideRange {
                    plugin,
                    namespace: found,
                    writer: stamped,
                    permitted: range,
                }
            } if plugin == PLUGIN && *found == namespace && *stamped == writer && *range == permitted
        ),
        "expected {namespace:?} format {writer} refused outside {permitted}: {error:?}"
    );
}

async fn publish_env(storage: &PostgresStorage, format: u32) -> Result<(), StoreError> {
    let mut config = lash_core_execution::PluginConfig::default();
    config.insert_versioned(PLUGIN, version(format), serde_json::json!({"count": 1}));
    let bytes = lash_core_execution::ProcessExecutionEnvSpec::new(
        lash_core_execution::AdmittedPluginConfig::new(config, 0),
        lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
        ),
    )
    .to_store_bytes()
    .expect("encode the environment");
    let env_ref = lash_core_execution::process_execution_env_ref_for_bytes(&bytes);
    let claim =
        lash_core_execution::ReferrerClaim::unguarded(
            lash_core_execution::ArtifactReferrer::HostPin(
                lash_core_execution::HostArtifactPin::mint(),
            ),
        )
        .expect("host pin claim");
    storage
        .process_env_store()
        .publish_process_execution_env(&claim, &env_ref, &bytes)
        .await
        .map_err(|error| match error {
            lash_core_execution::ArtifactStoreError::Incompatible { refusal } => {
                StoreError::Incompatible { refusal }
            }
            other => StoreError::Backend(other.to_string()),
        })
}

/// Write-path coverage: a session creation, a commit's plugin state, a
/// commit's plugin config and a process execution environment each refuse a
/// namespace stamped outside its plugin's range, and publish nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_out_of_range_plugin_write_is_refused_with_zero_publication_on_every_path() {
    let Some(database) = isolated().await else {
        return;
    };
    let (n, _next) = fleet(&database).await;
    let store = store(&n);
    let permitted = VersionRange::exactly(1);

    // A plugin the record does not name publishes only its first format.
    let mut request = root_session_request(&SessionId::from("unprovisioned"));
    request.config.plugin_config.insert_versioned(
        PLUGIN,
        version(2),
        serde_json::json!({"count": 1}),
    );
    let before = published(&n).await;
    let error = store
        .admit_session(&request)
        .await
        .expect_err("an unprovisioned plugin cannot publish format 2");
    assert!(
        matches!(
            &error,
            StoreError::Incompatible {
                refusal: CompatRefusal::PluginWriterUnprovisioned { plugin }
            } if plugin == PLUGIN
        ),
        "{error:?}"
    );
    assert_eq!(published(&n).await, before);
    assert_eq!(recorded_range(&n).await, None);

    // Its first format records `[1, 1]`.
    store
        .admit_session(&root_session_request(&SessionId::from("writer")))
        .await
        .expect("admit the session");
    let mut state = state("writer");
    state.set_plugin_state(Some(plugin_state(1, 1)));
    state.authority.plugin_config.insert_versioned(
        PLUGIN,
        version(1),
        serde_json::json!({"count": 1}),
    );
    commit(&store, &mut state)
        .await
        .expect("the first format is admitted");
    assert_eq!(recorded_range(&n).await, Some((1, 1)));

    // Session creation.
    let mut request = root_session_request(&SessionId::from("created-at-2"));
    request.config.plugin_config.insert_versioned(
        PLUGIN,
        version(2),
        serde_json::json!({"count": 1}),
    );
    let before = published(&n).await;
    let error = store
        .admit_session(&request)
        .await
        .expect_err("creation refuses a config outside the range");
    outside(&error, FormatNamespace::Config, 2, permitted);
    assert_eq!(published(&n).await, before);

    // A commit's plugin state.
    let mut next_state = state.clone();
    next_state.set_plugin_state(Some(plugin_state(2, 2)));
    let error = commit(&store, &mut next_state)
        .await
        .expect_err("a commit refuses plugin state outside the range");
    outside(&error, FormatNamespace::State, 2, permitted);
    assert_eq!(published(&n).await, before);

    // A commit's plugin config.
    let mut next_state = state.clone();
    next_state.authority.plugin_config.insert_versioned(
        PLUGIN,
        version(2),
        serde_json::json!({"count": 2}),
    );
    let error = commit(&store, &mut next_state)
        .await
        .expect_err("a commit refuses plugin config outside the range");
    outside(&error, FormatNamespace::Config, 2, permitted);
    assert_eq!(published(&n).await, before);

    // A process execution environment.
    let error = publish_env(&n, 2)
        .await
        .expect_err("an environment refuses plugin config outside the range");
    outside(&error, FormatNamespace::Config, 2, permitted);
    assert_eq!(published(&n).await, before);
    publish_env(&n, 1)
        .await
        .expect("an environment inside the range is published");

    assert_eq!(recorded_range(&n).await, Some((1, 1)));
    state.set_plugin_state(Some(plugin_state(1, 3)));
    commit(&store, &mut state)
        .await
        .expect("the permitted format still commits");
}

/// A recorded range that is not a range refuses every plugin publication
/// typed, and the store publishes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_writer_range_refuses_every_plugin_publication() {
    let Some(database) = isolated().await else {
        return;
    };
    let (n, _next) = fleet(&database).await;
    let store = store(&n);
    sqlx::query(
        "INSERT INTO lash_fleet_plugin_writers (plugin_id, min_format, max_format)
         VALUES ($1, 2, 1)",
    )
    .bind(PLUGIN)
    .execute(n.pool())
    .await
    .expect("record a malformed range");
    let mut request = root_session_request(&SessionId::from("malformed"));
    request
        .config
        .plugin_config
        .insert_versioned(PLUGIN, version(1), serde_json::json!({}));
    let before = published(&n).await;
    let error = store
        .admit_session(&request)
        .await
        .expect_err("a malformed range admits nothing");
    assert!(
        matches!(
            &error,
            StoreError::Incompatible {
                refusal: CompatRefusal::PluginWriterRangeMalformed { plugin, .. }
            } if plugin == PLUGIN
        ),
        "{error:?}"
    );
    assert_eq!(published(&n).await, before);
    assert!(matches!(
        store.plugin_writers().await,
        Err(StoreError::Incompatible {
            refusal: CompatRefusal::PluginWriterRangeMalformed { .. }
        })
    ));
}

/// Provisioning comes from registrations: inside the rollback window a
/// plugin the record does not name is permitted its oldest writable format,
/// and a recorded range is never moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provisioning_records_a_registered_plugin_once() {
    let Some(database) = isolated().await else {
        return;
    };
    let (n, next) = fleet(&database).await;
    // N+1 provisions inside the window: `F` is 1, below its own epoch.
    let ranges = store(&next)
        .provision_plugin_writers(&[registration(2, &[1, 2])])
        .await
        .expect("provision the plugin");
    assert_eq!(
        ranges.permitted_writer(PLUGIN),
        Ok(VersionRange::exactly(1))
    );
    assert_eq!(recorded_range(&n).await, Some((1, 1)));
    let again = store(&n)
        .provision_plugin_writers(&[registration(3, &[3])])
        .await
        .expect("a recorded range is left alone");
    assert_eq!(again, ranges);
    assert_eq!(recorded_range(&n).await, Some((1, 1)));
    assert_eq!(
        store(&n).plugin_writers().await.expect("read the ranges"),
        ranges
    );
}
