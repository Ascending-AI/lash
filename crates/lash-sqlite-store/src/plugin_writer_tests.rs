//! The fleet record's per-plugin writer ranges on SQLite (FIG-4746): every
//! plugin state and config publication is admitted against them inside its
//! write transaction, and finalize moves them with `F`.
#![expect(
    clippy::expect_used,
    reason = "test module: clippy's allow-expect-in-tests only exempts #[test] functions, and the fixture helpers here are test code too"
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use lash_core_execution::compat::{CompatRefusal, VersionRange};
use lash_core_execution::engine::BuildGeneration;
use lash_core_execution::store::RuntimeCommit;
use lash_core_execution::store::fleet_finalize::{
    FinalizeError, FinalizeRefusal, FleetEpochFlip, NoDeployments,
};
use lash_core_execution::store::plugin_writers::PluginWriterRegistration;
use lash_core_execution::testing::store_fixtures::{
    commit_runtime_state_for_test, root_session_request,
};
use lash_core_execution::{
    FleetFormat, FormatNamespace, FormatVersion, PluginNamespaceState, PluginState,
    ProcessExecutionEnvStore as _, RuntimeSessionState, RuntimeStore, SessionId, StoreError,
    StoreSet,
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
    let set = SqliteStoreSet::open(root.path().join("lash.db"))
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

fn fleet(location: &SqliteLocation) -> i64 {
    raw(location)
        .query_row("SELECT fleet_format FROM lash_compat", [], |row| row.get(0))
        .expect("read F")
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
                values: BTreeMap::from([("count".to_owned(), serde_json::json!(value))]),
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
        ),
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

/// Plugin-only finalize: the release's one format change is a plugin's. The
/// finalize moves `F` and the plugin's range together, the successor's
/// format is refused before it and admitted after it, and the retired build
/// is fenced from then on. A finalize that would change a range without
/// moving `F` is refused and changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plugin_only_finalize_moves_f_and_the_range_together_and_fences_the_old_build() {
    let (_run, set) = file_set().await;
    let location = set.location().clone();
    let store = store(&set);
    let writable = FleetFormat::writable();
    let seeded = i64::from(FleetFormat::seed(writable).version());
    let next = writable.max() + 1;
    let successor = VersionRange::new(writable.min(), next).expect("writable range");
    let retired = BuildGeneration::for_test("plugin-finalize-old");
    let successor_plugins = [registration(2, &[1, 2])];

    // The retired build's plugin writes format 1.
    store
        .admit_session(&root_session_request(&SessionId::from("window")))
        .await
        .expect("admit the session");
    let mut state = state("window");
    state.set_plugin_state(Some(plugin_state(1, 1)));
    commit(&store, &mut state)
        .await
        .expect("the retired build's format commits");
    assert_eq!(recorded_range(&location), Some((1, 1)));

    // Inside the window the successor's native format is refused.
    let mut ahead = state.clone();
    ahead.set_plugin_state(Some(plugin_state(2, 2)));
    let error = commit(&store, &mut ahead)
        .await
        .expect_err("format 2 is refused before finalize");
    outside(&error, FormatNamespace::State, 2, VersionRange::exactly(1));

    // A range changes only with `F`: with `F` already this build's epoch the
    // finalize is refused, and nothing changes.
    set.generation_drain()
        .mark_draining(&retired, 1)
        .await
        .expect("mark the retired generation draining");
    if seeded == i64::from(writable.max()) {
        match set
            .finalize_as(&retired, &NoDeployments, &successor_plugins, 5, writable)
            .await
        {
            Err(FinalizeError::Refused(FinalizeRefusal::PluginRangesNeedEpochMove {
                fleet,
                plugins,
            })) => {
                assert_eq!(fleet, writable.max());
                assert_eq!(plugins, vec![PLUGIN.to_owned()]);
            }
            other => panic!("a range change without an epoch move must refuse: {other:?}"),
        }
        assert_eq!(recorded_range(&location), Some((1, 1)));
        assert_eq!(fleet(&location), seeded);
    }

    // The successor finalizes: `F` and the range move in one step.
    let flip = set
        .finalize_as(&retired, &NoDeployments, &successor_plugins, 5, successor)
        .await
        .expect("finalize");
    assert_eq!(
        flip,
        FleetEpochFlip::Finalized {
            from: writable.min(),
            to: next
        }
    );
    assert_eq!(fleet(&location), i64::from(next));
    assert_eq!(recorded_range(&location), Some((1, 2)));
    assert_eq!(
        raw(&location)
            .query_row("SELECT COUNT(*) FROM lash_plugin_writers", [], |row| row
                .get::<_, i64>(0))
            .expect("count ranges"),
        1
    );

    // The retired build is fenced: its permitted-format write is refused and
    // writes nothing.
    let before = published(&location);
    state.set_plugin_state(Some(plugin_state(1, 4)));
    let error = commit(&store, &mut state)
        .await
        .expect_err("the retired build is fenced after finalize");
    assert!(
        matches!(
            error,
            StoreError::WriterFenced { recorded, writable: range }
                if recorded == next && range == writable
        ),
        "{error:?}"
    );
    assert_eq!(published(&location), before);

    // A rerun finds both finalized.
    let rerun = set
        .finalize_as(&retired, &NoDeployments, &successor_plugins, 6, successor)
        .await
        .expect("rerun");
    assert_eq!(rerun, FleetEpochFlip::AlreadyFinalized { fleet: next });
    assert_eq!(recorded_range(&location), Some((1, 2)));
}

fn registration_for(plugin: &str, native: u32, writable: &[u32]) -> PluginWriterRegistration {
    PluginWriterRegistration {
        plugin: plugin.to_owned(),
        native: version(native),
        writable: writable.iter().copied().map(version).collect(),
    }
}

/// A plugin the finalizing build no longer registers keeps its recorded
/// writer range across the finalize (FIG-4859/L12): the fleet record is the
/// only durable memory of what an inactive plugin may write, so the move of
/// `F` must not drop it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deregistered_plugins_writer_entry_survives_finalize() {
    let (_run, set) = file_set().await;
    let location = set.location().clone();
    let store = store(&set);
    let writable = FleetFormat::writable();
    let next = writable.max() + 1;
    let successor = VersionRange::new(writable.min(), next).expect("writable range");
    let retired = BuildGeneration::for_test("deregistered-entry-old");

    // The retiring build's plugin wrote its first format; the successor's
    // plugin joins it.
    store
        .admit_session(&root_session_request(&SessionId::from("inactive")))
        .await
        .expect("admit the session");
    let mut state = state("inactive");
    state.set_plugin_state(Some(plugin_state(1, 1)));
    commit(&store, &mut state)
        .await
        .expect("the first format commits");
    store
        .provision_plugin_writers(&[registration_for("other-plugin", 1, &[1, 2])])
        .await
        .expect("provision the surviving plugin");
    assert_eq!(recorded_range(&location), Some((1, 1)));

    // The successor finalizes naming only its own plugin: nothing
    // deregisters the retiring plugin's entry.
    set.generation_drain()
        .mark_draining(&retired, 1)
        .await
        .expect("mark the retired generation draining");
    set.finalize_as(
        &retired,
        &NoDeployments,
        &[registration_for("other-plugin", 2, &[1, 2])],
        5,
        successor,
    )
    .await
    .expect("finalize");
    let ranges = store.plugin_writers().await.expect("read the ranges");
    assert_eq!(
        ranges
            .permitted_writer(PLUGIN)
            .ok()
            .map(|range| (i64::from(range.min()), i64::from(range.max()))),
        Some((1, 1)),
        "the deregistered plugin's entry survives the finalize"
    );
    assert_eq!(
        ranges
            .permitted_writer("other-plugin")
            .ok()
            .map(|range| (i64::from(range.min()), i64::from(range.max()))),
        Some((1, 2)),
        "the registered plugin's range moved with `F`"
    );
    assert_eq!(fleet(&location), i64::from(next));
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

/// A plugin that reads format 2 natively and still writes format 1.
#[cfg(feature = "synthetic-next")]
struct WindowPlugin;

#[cfg(feature = "synthetic-next")]
impl lash_core_execution::facade_support::PluginFactory for WindowPlugin {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn migrate_format(
        &self,
        _from: FormatVersion,
        _namespace: FormatNamespace,
        value: serde_json::Value,
    ) -> Result<serde_json::Value, lash_core_execution::FormatRefusal> {
        Ok(value)
    }

    fn encode_format(
        &self,
        _to: FormatVersion,
        _namespace: FormatNamespace,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, lash_core_execution::FormatRefusal> {
        Ok(value.clone())
    }

    fn build(
        &self,
        ctx: &lash_core_execution::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core_execution::plugin::SessionPlugin>, lash_core_execution::PluginError>
    {
        lash_core_execution::plugin::StaticPluginFactory::new(
            lash_core_execution::plugin::PluginMetadata::plugin_declaration(self),
            lash_core_execution::plugin::PluginSpec::new(),
        )
        .build(ctx)
    }
}

#[cfg(feature = "synthetic-next")]
impl lash_core_execution::plugin::PluginDefinition for WindowPlugin {
    fn declaration() -> lash_core_execution::plugin::PluginDeclaration {
        let mut declaration = lash_core_execution::plugin::PluginDeclaration::initial(PLUGIN);
        declaration.format_version = version(2);
        declaration.writable_formats = vec![version(1), version(2)];
        declaration
    }
}

/// A Run of `session` as its shift admits it: the session, a sealed shift
/// fence, a head input and the run's admission recording `plugins`.
#[cfg(feature = "synthetic-next")]
async fn admit_run(
    store: &Arc<dyn RuntimeStore>,
    session: &str,
    plugins: &lash_core_execution::store::plugin_writers::PluginAdmission,
) -> (
    lash_core_execution::store::AdmitRunRequest,
    lash_core_execution::store::RunAdmission,
) {
    use lash_core_execution::testing::store_fixtures::{
        admit_run_request_for_test, seal_shift_fence_for_test,
    };
    let session_id =
        SessionId::try_from(session.to_owned()).expect("a law session id is never blank");
    let fence = seal_shift_fence_for_test(store, &session_id, "plugin-admission-law").await;
    let head = store
        .enqueue_pending_turn_input(lash_core_execution::PendingTurnInputDraft::new(
            session_id,
            lash_core_execution::TurnInputIngress::NextTurn,
            lash_core_execution::TurnInput::text("question"),
        ))
        .await
        .expect("enqueue the Run's head");
    let mut request = admit_run_request_for_test(
        &fence,
        &lash_core_execution::TurnId::try_from(format!("{session}-run"))
            .expect("a formatted id is never blank"),
        lash_core_execution::store::AdmittedHead::Input(head.input_id),
    );
    request.plugins = plugins.clone();
    let admission = store
        .admit_run(&request)
        .await
        .expect("admit the Run")
        .expect("the admission reaches its head");
    (request, admission)
}

/// FIG-4747: the writer a Run's admission chose is the Run's for good. A
/// Run admitted inside the window records format 1; the successor
/// finalizes, which widens the range to format 2; a retry of the Run
/// re-admits, is answered its recorded admission and commits format 1,
/// while a Run admitted after the finalize records and commits format 2.
#[cfg(feature = "synthetic-next")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_admitted_before_finalize_and_retried_after_it_keeps_its_recorded_writer() {
    use lash_core_execution::plugin::PluginSessionRequest;

    let (_run, set) = file_set().await;
    let location = set.location().clone();
    let store = store(&set);
    let mut factories = lash_core_execution::testing::test_standard_protocol_factories();
    factories.push(Arc::new(WindowPlugin));
    let host = lash_core_execution::facade_support::PluginHost::new(factories);
    let retired = BuildGeneration::for_test("plugin-admission-old");
    let writer = |admission: &lash_core_execution::store::plugin_writers::PluginAdmission| {
        admission
            .writer(PLUGIN)
            .expect("the admission names the plugin")
            .get()
    };
    // What a session admitted under `admission` commits, and that the store
    // takes it.
    let commit_under =
        |session: &'static str,
         admission: lash_core_execution::store::plugin_writers::PluginAdmission,
         store: Arc<dyn RuntimeStore>,
         host: lash_core_execution::facade_support::PluginHost| async move {
            let plugins = host
                .isolated_registry()
                .build_session(PluginSessionRequest::creation(session, Default::default()))
                .expect("build the plugin session");
            plugins.adopt_plugin_admission(admission);
            let mut state = state(session);
            state
                .capture_plugin_states(&plugins, lash_core_store::store::FleetFormat::current())
                .expect("capture the plugin state");
            let committed = state
                .plugin_state()
                .expect("the capture is resident")
                .plugins[PLUGIN]
                .format_version
                .get();
            commit(&store, &mut state).await.map(|()| committed)
        };

    // Inside the window the fleet permits the plugin's oldest format only,
    // and the Run's admission records it with the composition.
    let chosen = host
        .admit_plugins(store.as_ref())
        .await
        .expect("admit the plugins inside the window");
    assert_eq!(writer(&chosen), 1);
    assert_eq!(recorded_range(&location), Some((1, 1)));
    let (request, admission) = admit_run(&store, "before", &chosen).await;
    assert_eq!(admission.plugins, chosen);
    assert_eq!(
        admission
            .plugins
            .plugins()
            .iter()
            .map(|plugin| plugin.plugin.as_str())
            .collect::<Vec<_>>(),
        host.factories()
            .iter()
            .map(|factory| factory.id())
            .collect::<Vec<_>>(),
        "the record is the composition in hook order"
    );
    assert_eq!(
        commit_under(
            "before",
            admission.plugins.clone(),
            store.clone(),
            host.clone()
        )
        .await
        .expect("the Run's format 1 commits inside the window"),
        1
    );

    // The successor finalizes: the range reaches the plugin's native format.
    set.generation_drain()
        .mark_draining(&retired, 1)
        .await
        .expect("mark the retired generation draining");
    let registrations = host
        .composition()
        .expect("a valid composition")
        .writer_registrations();
    let flip = set
        .finalize(&retired, &NoDeployments, &registrations, 5)
        .await
        .expect("finalize");
    assert!(matches!(flip, FleetEpochFlip::Finalized { .. }), "{flip:?}");
    assert_eq!(recorded_range(&location), Some((1, 2)));

    // A retry of the Run chooses again, as its first execution did, and is
    // answered the admission the store recorded: format 1, which it commits.
    let fresh = host
        .admit_plugins(store.as_ref())
        .await
        .expect("admit the plugins after the finalize");
    assert_eq!(writer(&fresh), 2);
    let mut retry = request.clone();
    retry.plugins = fresh.clone();
    let replayed = store
        .admit_run(&retry)
        .await
        .expect("re-admit the Run")
        .expect("the recorded admission");
    assert_eq!(replayed.plugins, chosen, "the record, never the live range");
    assert_eq!(
        commit_under("before", replayed.plugins, store.clone(), host.clone())
            .await
            .expect("the retried Run's format 1 commits after the finalize"),
        1
    );

    // A Run admitted after the finalize records and commits format 2.
    let (_, admitted_after) = admit_run(&store, "after", &fresh).await;
    assert_eq!(writer(&admitted_after.plugins), 2);
    assert_eq!(
        commit_under("after", admitted_after.plugins, store.clone(), host.clone())
            .await
            .expect("the new Run's format 2 commits"),
        2
    );
}
