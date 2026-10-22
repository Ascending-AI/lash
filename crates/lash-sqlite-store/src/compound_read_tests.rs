//! COMPOUND-READ-SNAPSHOT: a public read that hydrates what its first
//! statement selected with further statements reads all of them in one
//! snapshot. Each law pauses the read right after its selection, commits a
//! competing release and reclamation on a connection that shares none of the
//! store's gates (as another process's would), and then lets the read finish:
//! it answers the complete value it selected, and only the next read sees the
//! committed change.

use super::*;
use crate::artifact_store::MODULE_ARTIFACT_NAMESPACE;
use crate::testing::{SqliteCompoundRead, SqlitePauses};
use lash_core_store::artifact_referrer::ArtifactReferrer;
use lash_core_store::store::ToolMaterialStore as _;
use lash_execution_contracts::module_artifacts::ModuleArtifactStore as _;

/// A WAL file store whose connections carry `pauses`.
pub(crate) async fn paused_file_store(
    path: &std::path::Path,
    pauses: &SqlitePauses,
) -> Arc<SqliteStore> {
    Arc::new(
        SqliteStore::open_at(
            &crate::location::DatabaseLocation::standalone_file(path),
            StoreOptions::standard(crate::SqliteSynchronous::Normal),
            Arc::new(lash_core_ids::clock::SystemClock),
            lash_core_store::store::FleetFormat::writable(),
            crate::testing::ConnectionHooks {
                pauses: Some(pauses.clone()),
                inline_calls: false,
            },
        )
        .await
        .expect("open a store with read seams"),
    )
}

/// Commit `competing` on a second connection to `path`, with the store's
/// foreign keys enforced, while a read is paused inside the store.
pub(crate) fn commit_competing(
    path: &std::path::Path,
    competing: impl FnOnce(&rusqlite::Transaction<'_>),
) {
    let mut raw = rusqlite::Connection::open(path).expect("open the competing connection");
    raw.busy_timeout(std::time::Duration::from_millis(15_000))
        .expect("competing busy timeout");
    raw.execute_batch("PRAGMA foreign_keys=ON;")
        .expect("competing foreign keys");
    let tx = raw.transaction().expect("competing transaction");
    competing(&tx);
    tx.commit()
        .expect("the competing commit lands in the window");
}

pub(crate) fn blob_rows(tx: &rusqlite::Connection, hash: &str) -> i64 {
    tx.query_row(
        "SELECT COUNT(*) FROM blobs WHERE hash = unhex(?1)",
        params![hash],
        |row| row.get(0),
    )
    .expect("count blob rows")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_checkpoint_read_keeps_its_snapshot_while_the_checkpoint_is_peeled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("checkpoint-snapshot.db");
    let pauses = SqlitePauses::default();
    let store = paused_file_store(&path, &pauses).await;
    let mut checkpoint = HydratedSessionCheckpoint::default();
    checkpoint
        .components
        .insert(
            "snapshot/component".to_owned(),
            lash_core_store::store::HydratedCheckpointComponent::changed(vec![7; 129]),
        )
        .expect("fixture allocation admitted");
    let stored = store
        .put_checkpoint(&checkpoint)
        .await
        .expect("store an unreferenced checkpoint");
    let component_ref = stored.manifest.components["snapshot/component"]
        .blob_ref
        .as_str()
        .to_owned();
    let selected = store
        .get_checkpoint(&stored.checkpoint_ref)
        .await
        .expect("read the checkpoint")
        .expect("the checkpoint is stored");

    let pause = pauses.pause_compound_read(SqliteCompoundRead::Checkpoint);
    let reader = tokio::spawn({
        let store = Arc::clone(&store);
        let checkpoint_ref = stored.checkpoint_ref.clone();
        async move { store.get_checkpoint(&checkpoint_ref).await }
    });
    pause.wait_until_reached().await;
    commit_competing(&path, |tx| {
        crate::durable::peel(
            tx,
            std::collections::BTreeSet::from([stored.checkpoint_ref.as_str().to_owned()]),
        )
        .expect("peel the unreferenced checkpoint");
        assert_eq!(
            blob_rows(tx, &component_ref),
            0,
            "the peel reclaims the component the paused read selected"
        );
    });
    pause.release();

    let read = reader
        .await
        .expect("reader task")
        .expect("a peel between manifest and components is no storage failure")
        .expect("the read keeps the checkpoint it selected");
    assert_eq!(
        read.components, selected.components,
        "the manifest and its components come from one snapshot"
    );
    assert!(
        store
            .get_checkpoint(&stored.checkpoint_ref)
            .await
            .expect("read after the peel")
            .is_none(),
        "the next read sees the reclamation"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_artifact_read_keeps_its_snapshot_while_its_referrer_ends() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("artifact-snapshot.db");
    let pauses = SqlitePauses::default();
    let store = paused_file_store(&path, &pauses).await;
    let referrer =
        ArtifactReferrer::HostPin(lash_core_store::artifact_referrer::HostArtifactPin::mint());
    store
        .publish_module_artifact(
            &lash_core_store::artifact_referrer::ReferrerClaim::unguarded(referrer.clone())
                .expect("an unguarded host pin"),
            "snapshot-module",
            b"module body",
        )
        .await
        .expect("publish the module");

    let pause = pauses.pause_compound_read(SqliteCompoundRead::Artifact);
    let reader = tokio::spawn({
        let store = Arc::clone(&store);
        async move { store.get_module_artifact("snapshot-module").await }
    });
    pause.wait_until_reached().await;
    commit_competing(&path, |tx| {
        SqliteStore::end_namespaced_tx(
            tx,
            MODULE_ARTIFACT_NAMESPACE,
            &lash_core_store::artifact_referrer::ResolvedArtifactCleanup {
                referrer,
                carries: Vec::new(),
            },
            1,
        )
        .expect("end the module's only referrer");
        let pointers: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM artifact_refs WHERE artifact_ref = 'snapshot-module'",
                [],
                |row| row.get(0),
            )
            .expect("count pointers");
        assert_eq!(pointers, 0, "ending the referrer reclaims the pointer");
    });
    pause.release();

    assert_eq!(
        reader
            .await
            .expect("reader task")
            .expect("a reclamation between pointer and body is no corruption"),
        Some(b"module body".to_vec()),
        "the pointer and its body come from one snapshot"
    );
    assert_eq!(
        store
            .get_module_artifact("snapshot-module")
            .await
            .expect("read after the reclamation"),
        None,
        "the next read sees the reclamation"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_material_read_keeps_its_snapshot_while_its_holder_releases() {
    use lash_core_store::tool_run::{
        MaterialBundle, MaterialHolder, MaterialOwner, MaterialPayload, MaterialRefusal,
        MaterialRetentionError, MaterialRole,
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("material-snapshot.db");
    let pauses = SqlitePauses::default();
    let store = paused_file_store(&path, &pauses).await;
    let source = lash_core::AwaitEventKey {
        scope: lash_core::ExecutionScope::turn(
            SessionId::fixture("material-snapshot"),
            "material-turn",
        ),
        wait: lash_core::AwaitEventWaitIdentity::tool_call(lash_core::ToolCallId::fixture(
            "material-call",
        )),
        key_id: "material-key".into(),
        signature: "material-signature".into(),
    };
    let owner = MaterialOwner::Source {
        source: source.clone(),
    };
    let holder = MaterialHolder::Source { source };
    let text = "retained result ".repeat(512);
    let bundle = MaterialBundle::of(vec![MaterialPayload::new(
        owner.clone(),
        MaterialRole::AttemptOutput,
        text.clone(),
    )])
    .expect("encode the bundle")
    .expect("a non-empty bundle");
    let retained = store
        .retain_material(&holder, &bundle)
        .await
        .expect("retain the material");
    let reference = retained.references[0].clone();

    let pause = pauses.pause_compound_read(SqliteCompoundRead::ToolMaterial);
    let reader = tokio::spawn({
        let store = Arc::clone(&store);
        let holder = holder.clone();
        let reference = reference.clone();
        let owner = owner.clone();
        async move { store.read_material(&holder, &reference, &owner).await }
    });
    pause.wait_until_reached().await;
    commit_competing(&path, |tx| {
        crate::artifact_store::tool_material::release_material_tx(tx, &holder.referrer(), 1)
            .expect("release the holder");
        let pointers: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM artifact_refs WHERE namespace = 'tool_material'",
                [],
                |row| row.get(0),
            )
            .expect("count pointers");
        assert_eq!(pointers, 0, "the release reclaims the bundle");
    });
    pause.release();

    let read = reader
        .await
        .expect("reader task")
        .expect("a release after the lease check is no storage failure");
    assert_eq!(
        read.text, text,
        "the lease, pointer and body come from one snapshot"
    );
    assert!(
        matches!(
            store.read_material(&holder, &reference, &owner).await,
            Err(MaterialRetentionError::Refused(
                MaterialRefusal::Retired { .. }
            ))
        ),
        "the next read sees the released holder"
    );
}
