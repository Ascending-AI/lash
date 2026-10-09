//! The fleet record's per-plugin writer ranges on SQLite (FIG-4746): every
//! plugin state and config publication is admitted against them inside its
//! write transaction.
#![expect(
    clippy::expect_used,
    reason = "test module: clippy's allow-expect-in-tests only exempts #[test] functions, and the fixture helpers here are test code too"
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use lash_core_execution::compat::{CompatRefusal, VersionRange};
use lash_core_execution::store::RuntimeCommit;
use lash_core_execution::store::plugin_writers::PluginWriterRegistration;
use lash_core_execution::testing::store_fixtures::{
    commit_runtime_state_for_test, root_session_request,
};
use lash_core_execution::{
    FleetFormat, FormatNamespace, FormatVersion, PluginNamespaceState, PluginState,
    ProcessExecutionEnvStore as _, RuntimeSessionState, RuntimeStore, SessionId, StoreError,
};
use rusqlite::Connection;

use crate::{SqliteLocation, SqliteStoreSet};

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

fn raw(location: &SqliteLocation) -> Connection {
    let connection = Connection::open_with_flags(
        location.target().uri(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("open a second connection, as another process would");
    connection
        .busy_timeout(Duration::from_secs(5))
        .expect("busy timeout");
    connection
}

async fn file_set() -> (tempfile::TempDir, SqliteStoreSet) {
    let root = tempfile::tempdir().expect("store root");
    let set = SqliteStoreSet::open(
        root.path().join("lash.db"),
        crate::SqliteSynchronous::Normal,
    )
    .await
    .expect("open the store set");
    (root, set)
}

fn store(set: &SqliteStoreSet) -> Arc<dyn RuntimeStore> {
    set.process_env_store()
}

/// The recorded range of [`PLUGIN`], read as another process would.
fn recorded_range(location: &SqliteLocation) -> Option<(i64, i64)> {
    raw(location)
        .query_row(
            "SELECT min_format, max_format FROM lash_plugin_writers WHERE plugin_id = ?1",
            [PLUGIN],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            error => Err(error),
        })
        .expect("read the writer range")
}

/// Everything a refused publication must leave alone in the durable core.
fn published(location: &SqliteLocation) -> (i64, i64, i64, Vec<(String, String)>) {
    let connection = raw(location);
    let count = |table: &str| -> i64 {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count rows")
    };
    let mut heads = connection
        .prepare("SELECT session_id, head_json FROM session_head JOIN session_revisions USING (session_id, head_revision) ORDER BY session_id")
        .expect("prepare the head read");
    let heads = heads
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("read heads")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("decode heads");
    (
        count("session_meta"),
        count("blobs"),
        count("artifact_refs"),
        heads,
    )
}

fn state(session_id: &str) -> RuntimeSessionState {
    RuntimeSessionState {
        session_id: lash_core_execution::SessionId::fixture(session_id),
        ..RuntimeSessionState::ambient_fixture(lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
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

fn env_bytes(format: u32) -> Vec<u8> {
    let mut config = lash_core_execution::PluginConfig::default();
    config.insert_versioned(PLUGIN, version(format), serde_json::json!({"count": 1}));
    lash_core_execution::ProcessExecutionEnvSpec::new(
        lash_core_execution::AdmittedPluginConfig::new(config, 0),
        lash_core_execution::SessionPolicy::new(
            lash_core_execution::TurnBudget::Unbounded,
            lash_core_execution::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ),
        lash_core_execution::SessionToolAccess::ambient(),
    )
    .to_store_bytes()
    .expect("encode the environment")
}

async fn publish_env(set: &SqliteStoreSet, format: u32) -> Result<(), StoreError> {
    let bytes = env_bytes(format);
    let env_ref = lash_core_execution::process_execution_env_ref_for_bytes(&bytes);
    let claim =
        lash_core_execution::ReferrerClaim::unguarded(
            lash_core_execution::ArtifactReferrer::HostPin(
                lash_core_execution::HostArtifactPin::mint(),
            ),
        )
        .expect("host pin claim");
    set.process_env_store()
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_out_of_range_plugin_write_is_refused_with_zero_publication_on_every_path() {
    let (_run, set) = file_set().await;
    let location = set.location().clone();
    let store = store(&set);
    let permitted = VersionRange::exactly(1);

    // A plugin the record does not name publishes only its first format.
    let mut request = root_session_request(&SessionId::from("unprovisioned"));
    request.config.plugin_config.insert_versioned(
        PLUGIN,
        version(2),
        serde_json::json!({"count": 1}),
    );
    let before = published(&location);
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
    assert_eq!(published(&location), before);
    assert_eq!(recorded_range(&location), None);

    // Its first format records `[1, 1]`.
    let session = SessionId::from("writer");
    store
        .admit_session(&root_session_request(&session))
        .await
        .expect("admit the session");
    let mut state = state("writer");
    state.set_plugin_state(Some(plugin_state(1, 1)));
    state.authority.plugin_config.insert_versioned(
        PLUGIN,
        version(2),
        serde_json::json!({"count": 1}),
    );
    let before = published(&location);
    let error = commit(&store, &mut state)
        .await
        .expect_err("a first-format state cannot provision a later config in the same publication");
    outside(&error, FormatNamespace::Config, 2, permitted);
    assert_eq!(published(&location), before);
    assert_eq!(recorded_range(&location), None);

    state.authority.plugin_config.insert_versioned(
        PLUGIN,
        version(1),
        serde_json::json!({"count": 1}),
    );
    commit(&store, &mut state)
        .await
        .expect("the first format is admitted");
    assert_eq!(recorded_range(&location), Some((1, 1)));

    // Session creation.
    let mut request = root_session_request(&SessionId::from("created-at-2"));
    request.config.plugin_config.insert_versioned(
        PLUGIN,
        version(2),
        serde_json::json!({"count": 1}),
    );
    let before = published(&location);
    let error = store
        .admit_session(&request)
        .await
        .expect_err("creation refuses a config outside the range");
    outside(&error, FormatNamespace::Config, 2, permitted);
    assert_eq!(published(&location), before);

    // A commit's plugin state.
    let mut next = state.clone();
    next.set_plugin_state(Some(plugin_state(2, 2)));
    let error = commit(&store, &mut next)
        .await
        .expect_err("a commit refuses plugin state outside the range");
    outside(&error, FormatNamespace::State, 2, permitted);
    assert_eq!(published(&location), before);

    // A commit's plugin config.
    let mut next = state.clone();
    next.authority.plugin_config.insert_versioned(
        PLUGIN,
        version(2),
        serde_json::json!({"count": 2}),
    );
    let error = commit(&store, &mut next)
        .await
        .expect_err("a commit refuses plugin config outside the range");
    outside(&error, FormatNamespace::Config, 2, permitted);
    assert_eq!(published(&location), before);

    // A process execution environment.
    let error = publish_env(&set, 2)
        .await
        .expect_err("an environment refuses plugin config outside the range");
    outside(&error, FormatNamespace::Config, 2, permitted);
    assert_eq!(published(&location), before);
    publish_env(&set, 1)
        .await
        .expect("an environment inside the range is published");

    assert_eq!(recorded_range(&location), Some((1, 1)));
    // The permitted format still commits after every refusal.
    state.set_plugin_state(Some(plugin_state(1, 3)));
    commit(&store, &mut state)
        .await
        .expect("the permitted format still commits");
}

/// A recorded range that is not a range refuses every plugin publication
/// typed, and the store publishes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_writer_range_refuses_every_plugin_publication() {
    let (_run, set) = file_set().await;
    let location = set.location().clone();
    let store = store(&set);
    raw(&location)
        .execute(
            "INSERT INTO lash_plugin_writers (plugin_id, min_format, max_format) VALUES (?1, 2, 1)",
            [PLUGIN],
        )
        .expect("record a malformed range");
    let mut request = root_session_request(&SessionId::from("malformed"));
    request
        .config
        .plugin_config
        .insert_versioned(PLUGIN, version(1), serde_json::json!({}));
    let before = published(&location);
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
    assert_eq!(published(&location), before);
    assert!(matches!(
        store.plugin_writers().await,
        Err(StoreError::Incompatible {
            refusal: CompatRefusal::PluginWriterRangeMalformed { .. }
        })
    ));
}

/// Provisioning comes from registrations: it records a range only for a
/// plugin the record does not name, and never moves a recorded one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provisioning_records_a_registered_plugin_once() {
    let (_run, set) = file_set().await;
    let location = set.location().clone();
    let store = store(&set);
    assert!(
        store
            .plugin_writers()
            .await
            .expect("read the ranges")
            .is_empty()
    );
    // The fleet epoch is this build's own, so the plugin writes up to its
    // native format.
    let finalized = FleetFormat::writable().max() == store.fleet_format().version();
    let expected = if finalized { (1, 2) } else { (1, 1) };
    let ranges = store
        .provision_plugin_writers(&[registration(2, &[1, 2])])
        .await
        .expect("provision the plugin");
    assert_eq!(
        ranges
            .permitted_writer(PLUGIN)
            .map(|range| (i64::from(range.min()), i64::from(range.max()))),
        Ok(expected)
    );
    assert_eq!(recorded_range(&location), Some(expected));
    let again = store
        .provision_plugin_writers(&[registration(3, &[3])])
        .await
        .expect("a recorded range is left alone");
    assert_eq!(again, ranges);
    assert_eq!(recorded_range(&location), Some(expected));
    assert_eq!(
        store.plugin_writers().await.expect("read the ranges"),
        ranges
    );
}

/// Deleting a plugin's writer entry un-provisions it (FIG-4859/L12): a write
/// at a stamp the deleted range had permitted is refused typed and publishes
/// nothing, on every write path. The record's bootstrap rule still applies:
/// a first-format write by a plugin the record does not name provisions
/// `[1, 1]` — a deletion followed by one is a fresh provision, visibly
/// narrower than the deleted range.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deleted_writer_entry_refuses_writes_and_publishes_nothing() {
    let (_run, set) = file_set().await;
    let location = set.location().clone();
    let store = store(&set);

    // The plugin's entry recorded [1, 2], as a finalize that admitted its
    // native format would leave it.
    raw(&location)
        .execute(
            "INSERT INTO lash_plugin_writers (plugin_id, min_format, max_format) VALUES (?1, 1, 2)",
            [PLUGIN],
        )
        .expect("record the plugin's range");
    raw(&location)
        .execute(
            "DELETE FROM lash_plugin_writers WHERE plugin_id = ?1",
            [PLUGIN],
        )
        .expect("delete the entry, as an out-of-band mutation would");
    assert_eq!(recorded_range(&location), None);
    assert!(
        store
            .plugin_writers()
            .await
            .expect("read the ranges")
            .is_empty()
    );

    let unprovisioned = |error: &StoreError| {
        assert!(
            matches!(
                error,
                StoreError::Incompatible {
                    refusal: CompatRefusal::PluginWriterUnprovisioned { plugin }
                } if plugin == PLUGIN
            ),
            "a write against the deleted entry refuses typed: {error:?}"
        );
    };

    // Session creation carrying the deleted range's headroom.
    let mut request = root_session_request(&SessionId::from("deleted-entry"));
    request.config.plugin_config.insert_versioned(
        PLUGIN,
        version(2),
        serde_json::json!({"count": 1}),
    );
    let before = published(&location);
    let error = store
        .admit_session(&request)
        .await
        .expect_err("creation at the deleted range's stamp must refuse");
    unprovisioned(&error);
    assert_eq!(published(&location), before);

    // A commit's plugin state at the same stamp.
    store
        .admit_session(&root_session_request(&SessionId::from("deleted-commit")))
        .await
        .expect("admit a session without plugin stamps");
    let admitted = published(&location);
    let mut state = state("deleted-commit");
    state.set_plugin_state(Some(plugin_state(2, 2)));
    let error = commit(&store, &mut state)
        .await
        .expect_err("plugin state at the deleted range's stamp must refuse");
    unprovisioned(&error);
    assert_eq!(published(&location), admitted);

    // A process execution environment at the same stamp.
    let error = publish_env(&set, 2)
        .await
        .expect_err("an environment at the deleted range's stamp must refuse");
    unprovisioned(&error);
    assert_eq!(published(&location), admitted);
    assert_eq!(recorded_range(&location), None);

    // A first-format write after the deletion provisions [1, 1] — the
    // bootstrap any unprovisioned plugin gets — and never the deleted
    // range's headroom.
    state.set_plugin_state(Some(plugin_state(1, 3)));
    commit(&store, &mut state)
        .await
        .expect("the first format still bootstraps");
    assert_eq!(recorded_range(&location), Some((1, 1)));
    let error = publish_env(&set, 2)
        .await
        .expect_err("the deleted headroom stays refused");
    assert!(
        matches!(
            error,
            StoreError::Incompatible {
                refusal: CompatRefusal::PluginWriterOutsideRange { .. }
            }
        ),
        "{error:?}"
    );
}
