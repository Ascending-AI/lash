//! What the durable walk must say about a deployment it is handed.
//!
//! The walk exists so a host can list what is stranded behind a refusal without
//! performing the open that refuses. Three of its promises can only be checked
//! against a real server, and each one is a way the surface fails silently
//! rather than loudly if it breaks: an unprovisioned deployment must produce a
//! *report* rather than an error, a page must carry the identity fields a drain
//! list is made of, and paging must be exact — a walk that duplicated or skipped
//! items would hand an operator a drain list that is wrong in the direction
//! nobody checks. The dangling-reference case is here for the same reason: a
//! walk that errored on one missing blob would lose every finding behind it.

use lash_core::store::SessionCheckpoint;
use lash_core::{
    BlobRef, CheckpointComponentDescriptor, DurablePayload, DurableScan, DurableSurface,
    ScanCoverage, StorePreflight,
};
use lash_postgres_store::{
    PostgresStorage, PostgresStoreConfig, PostgresStorePreflight, SchemaProvisioning,
};

#[allow(dead_code)]
mod support;

use support::database_url;

#[allow(dead_code)]
#[path = "schema_drift/harness.rs"]
mod harness;

use harness::ScratchSchema;

/// The most valuable deployment to describe is often the one nobody has
/// provisioned. A walk that failed on the missing table would take the whole
/// preflight report down with it, so every surface has to answer `NotScanned`
/// with a reason instead — and specifically not an empty `Scanned` page, which a
/// host would read as "nothing here is stranded".
#[tokio::test]
async fn an_unprovisioned_database_reports_not_scanned_rather_than_erroring() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping preflight durable walk: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    scratch
        .apply("DROP SCHEMA IF EXISTS lash_preflight_walk_empty CASCADE")
        .await;
    scratch
        .apply("CREATE SCHEMA lash_preflight_walk_empty")
        .await;
    let empty_pool =
        harness::pool_with_search_path(&database_url, "lash_preflight_walk_empty").await;
    let preflight = PostgresStorePreflight::from_pool(empty_pool.clone());

    for surface in [
        DurableSurface::ParkedSegment,
        DurableSurface::PendingWake,
        DurableSurface::SessionCheckpoint,
        DurableSurface::SessionExecutionState,
    ] {
        let page = preflight
            .scan_durable(&DurableScan::first(surface, 10))
            .await
            .unwrap_or_else(|error| {
                panic!("an unprovisioned deployment is reportable, not an error: {error}")
            });
        match page.coverage {
            ScanCoverage::NotScanned { reason } => assert!(
                !reason.is_empty(),
                "an unwalked surface names why it was not walked"
            ),
            coverage => panic!(
                "{} must not report as walked here: {coverage:?}",
                surface.name()
            ),
        }
        assert!(page.items.is_empty());
        assert!(page.next.is_none());
    }

    empty_pool.close().await;
    scratch
        .apply("DROP SCHEMA IF EXISTS lash_preflight_walk_empty CASCADE")
        .await;
    scratch.cleanup().await;
}

#[tokio::test]
async fn module_artifact_surface_reads_the_persisted_json() {
    use lashlang::LashlangArtifactStore;

    let Some(database_url) = database_url() else {
        eprintln!("skipping module artifact preflight walk: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let storage = PostgresStorage::from_pool_with(
        scratch.pool.clone(),
        PostgresStoreConfig {
            schema_provisioning: SchemaProvisioning::HostProvisioned,
            ..PostgresStoreConfig::default()
        },
    )
    .await
    .expect("open provisioned Postgres storage");
    let artifact = lashlang::ModuleArtifact::from_store_bytes(include_bytes!(
        "../../lashlang/tests/fixtures/module-artifact-old.json"
    ))
    .expect("decode frozen artifact");
    storage
        .lashlang_artifact_store()
        .put_module_artifact(&artifact)
        .await
        .expect("persist module artifact");
    drop(storage);

    let page = PostgresStorePreflight::from_pool(scratch.pool.clone())
        .scan_durable(&DurableScan::first(DurableSurface::ModuleArtifact, 10))
        .await
        .expect("walk module artifacts");
    assert_eq!(page.coverage, ScanCoverage::Scanned);
    assert_eq!(page.items.len(), 1, "{page:?}");
    assert_eq!(page.items[0].cursor, artifact.module_ref.as_str());
    match &page.items[0].payload {
        DurablePayload::Json(json) => assert!(json.contains("compilation_dialect"), "{json}"),
        other => panic!("expected module artifact JSON, got {other:?}"),
    }

    scratch.cleanup().await;
}

/// A parked segment has to arrive carrying the identity an operator acts on —
/// the process, the session it wakes into, the store's own status word, and the
/// registry record a stored program identity can be recomputed from. It also has
/// to arrive *only* for a live process: a terminal process's handover row is
/// residue, and putting it on a drain list sends an operator after a
/// continuation nothing will resume.
#[tokio::test]
async fn a_parked_segment_is_enumerated_with_its_identity_and_terminal_ones_are_not() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping preflight durable walk: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    seed_process(&scratch, "proc-live", "waiting", Some("session-1")).await;
    seed_process(&scratch, "proc-done", "completed", Some("session-2")).await;
    seed_segment(&scratch, "proc-live", 0, r#"{"segment":"live"}"#).await;
    seed_segment(&scratch, "proc-done", 0, r#"{"segment":"residue"}"#).await;

    let preflight = PostgresStorePreflight::from_pool(scratch.pool.clone());
    let page = preflight
        .scan_durable(&DurableScan::first(DurableSurface::ParkedSegment, 10))
        .await
        .expect("a provisioned deployment walks");

    assert_eq!(page.coverage, ScanCoverage::Scanned);
    assert_eq!(
        page.items.len(),
        1,
        "only the live process contributes a parked segment: {:?}",
        page.items
    );
    let item = &page.items[0];
    assert_eq!(item.process_id.as_deref(), Some("proc-live"));
    assert_eq!(item.session_id.as_deref(), Some("session-1"));
    assert_eq!(item.status.as_deref(), Some("waiting"));
    assert_eq!(
        item.owner_record.as_deref(),
        Some(r#"{"process":"proc-live"}"#),
        "the registry record travels with the item"
    );
    assert_eq!(
        item.payload,
        DurablePayload::Json(r#"{"segment":"live"}"#.to_string())
    );
    assert!(
        page.next.is_none(),
        "a page shorter than its limit is the end of the surface"
    );

    scratch.cleanup().await;
}

/// Undelivered wakes only. `enqueued` has already left the queue, and reporting
/// it would put finished work on a drain list.
#[tokio::test]
async fn only_undelivered_wakes_are_enumerated() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping preflight durable walk: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    seed_process(&scratch, "proc-1", "running", Some("session-1")).await;
    seed_wake(&scratch, "delivery-a", "proc-1", "pending").await;
    seed_wake(&scratch, "delivery-b", "proc-1", "enqueuing").await;
    seed_wake(&scratch, "delivery-c", "proc-1", "enqueued").await;

    let page = PostgresStorePreflight::from_pool(scratch.pool.clone())
        .scan_durable(&DurableScan::first(DurableSurface::PendingWake, 10))
        .await
        .expect("a provisioned deployment walks");

    let delivered: Vec<&str> = page.items.iter().map(|item| item.cursor.as_str()).collect();
    assert_eq!(delivered, vec!["delivery-a", "delivery-b"]);
    assert_eq!(page.items[0].session_id.as_deref(), Some("session-target"));
    assert_eq!(page.items[1].status.as_deref(), Some("enqueuing"));

    scratch.cleanup().await;
}

/// Paging with the smallest possible page is the harshest test of a keyset
/// walk: every boundary is exercised, and a cursor that disagreed with the
/// ordering by even one row would show up as a duplicate or a hole.
#[tokio::test]
async fn paging_a_surface_one_item_at_a_time_is_exact() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping preflight durable walk: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    seed_process(&scratch, "proc-a", "waiting", Some("session-a")).await;
    seed_process(&scratch, "proc-b", "running", None).await;
    seed_segment(&scratch, "proc-a", 0, r#"{"n":0}"#).await;
    seed_segment(&scratch, "proc-a", 1, r#"{"n":1}"#).await;
    seed_segment(&scratch, "proc-b", 0, r#"{"n":2}"#).await;

    let preflight = PostgresStorePreflight::from_pool(scratch.pool.clone());
    let mut walked: Vec<String> = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..8 {
        let scan = DurableScan {
            surface: DurableSurface::ParkedSegment,
            after: after.clone(),
            limit: 1,
        };
        let page = preflight.scan_durable(&scan).await.expect("walk one item");
        assert!(page.items.len() <= 1, "a page never exceeds its limit");
        for item in &page.items {
            walked.push(match &item.payload {
                DurablePayload::Json(json) => json.clone(),
                other => panic!("a parked segment is JSON, got {other:?}"),
            });
        }
        match page.next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }

    assert_eq!(
        walked,
        vec![
            r#"{"n":0}"#.to_string(),
            r#"{"n":1}"#.to_string(),
            r#"{"n":2}"#.to_string(),
        ],
        "every segment appears exactly once, in key order"
    );

    scratch.cleanup().await;
}

/// A checkpoint root whose blob is gone is a finding the report must carry, not
/// an error that discards it. The session is still named, and the reason names
/// the reference an operator would chase.
#[tokio::test]
async fn a_dangling_checkpoint_ref_is_reported_missing_rather_than_skipped() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping preflight durable walk: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    // One healthy session whose manifest and execution-state component are both
    // present, so the dangling one below is a contrast rather than the only
    // shape the walk has ever seen.
    let component = b"execution state bytes".to_vec();
    let component_ref = "hash-execution-state";
    let manifest = encoded_manifest(component_ref);
    let manifest_ref = "hash-manifest";
    seed_blob(&scratch, component_ref, &component).await;
    seed_blob(&scratch, manifest_ref, &manifest).await;
    seed_session(&scratch, "session-healthy", manifest_ref).await;
    seed_session(&scratch, "session-dangling", "hash-that-was-collected").await;

    let preflight = PostgresStorePreflight::from_pool(scratch.pool.clone());
    let page = preflight
        .scan_durable(&DurableScan::first(DurableSurface::SessionCheckpoint, 10))
        .await
        .expect("a dangling reference is a finding, not a failure");
    assert_eq!(page.items.len(), 2, "neither session is skipped");
    assert_eq!(
        page.items[0].session_id.as_deref(),
        Some("session-dangling")
    );
    match &page.items[0].payload {
        DurablePayload::Missing { reason } => assert!(
            reason.contains("hash-that-was-collected"),
            "the reason names the dangling reference: {reason}"
        ),
        other => panic!("a dangling checkpoint root is Missing, got {other:?}"),
    }
    assert_eq!(
        page.items[1].payload,
        DurablePayload::MessagePack(manifest.clone()),
        "the healthy session yields the manifest's logical bytes unchanged"
    );

    // One level deeper: only the session whose manifest names an execution-state
    // component contributes, and it yields that component's bytes rather than
    // the manifest's.
    let deeper = preflight
        .scan_durable(&DurableScan::first(
            DurableSurface::SessionExecutionState,
            10,
        ))
        .await
        .expect("the deep surface walks");
    assert_eq!(deeper.items.len(), 1, "{:?}", deeper.items);
    assert_eq!(
        deeper.items[0].session_id.as_deref(),
        Some("session-healthy")
    );
    assert_eq!(
        deeper.items[0].payload,
        DurablePayload::MessagePack(component)
    );

    scratch.cleanup().await;
}

/// A deep page can emit fewer items than the sessions it scanned, so its `next`
/// has to come from the last session *scanned*. Taking it from the last item
/// emitted would resume before sessions the walk already passed and loop over
/// them forever.
#[tokio::test]
async fn a_deep_page_resumes_after_the_last_session_scanned_not_the_last_item() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping preflight durable walk: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let manifest_with = encoded_manifest("hash-execution-state");
    seed_blob(&scratch, "hash-execution-state", b"execution state bytes").await;
    seed_blob(&scratch, "hash-with", &manifest_with).await;
    seed_blob(
        &scratch,
        "hash-without",
        &encoded_manifest_without_components(),
    )
    .await;
    seed_session(&scratch, "session-1", "hash-with").await;
    // Scanned second, emits nothing: it has no execution-state component at all.
    seed_session(&scratch, "session-2", "hash-without").await;

    let page = PostgresStorePreflight::from_pool(scratch.pool.clone())
        .scan_durable(&DurableScan::first(
            DurableSurface::SessionExecutionState,
            2,
        ))
        .await
        .expect("the deep surface walks");

    assert_eq!(page.items.len(), 1);
    assert_eq!(
        page.next.as_deref(),
        Some("session-2"),
        "the page resumes after the last session scanned, not the last item emitted"
    );

    scratch.cleanup().await;
}

/// Encode a checkpoint manifest exactly as the write path does — the real
/// `SessionCheckpoint`, through the same named-field MessagePack encoding — so
/// the walk's navigation is proved against the shape production writes rather
/// than against a fixture that agrees with the reader by construction.
fn encoded_manifest(execution_state_ref: &str) -> Vec<u8> {
    let mut components = std::collections::BTreeMap::new();
    components.insert(
        lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
        CheckpointComponentDescriptor {
            blob_ref: BlobRef(execution_state_ref.to_string()),
            encoding_version: lash_core::store::CHECKPOINT_COMPONENT_ENCODING_VERSION,
        },
    );
    encode_manifest(components)
}

fn encoded_manifest_without_components() -> Vec<u8> {
    encode_manifest(std::collections::BTreeMap::new())
}

fn encode_manifest(
    components: std::collections::BTreeMap<String, CheckpointComponentDescriptor>,
) -> Vec<u8> {
    let manifest = SessionCheckpoint {
        schema_version: lash_core::store::SESSION_CHECKPOINT_SCHEMA_VERSION,
        turn_state: lash_core::PersistedTurnState::default(),
        components,
        plugin_snapshot_revision: None,
    };
    let mut bytes = Vec::new();
    rmp_serde::encode::write_named(&mut bytes, &manifest).expect("encode checkpoint manifest");
    bytes
}

async fn seed_process(scratch: &ScratchSchema, process_id: &str, status: &str, wake: Option<&str>) {
    let wake = match wake {
        Some(session_id) => format!("'{session_id}'"),
        None => "NULL".to_string(),
    };
    scratch
        .apply(&format!(
            "INSERT INTO lash_processes (
                 process_id, registration_fingerprint, originator_id, wake_session_id,
                 identity_kind, identity_label, is_waiting, created_at_ms, updated_at_ms,
                 change_seq, status, record_json
             ) VALUES (
                 '{process_id}', 'fingerprint', 'originator', {wake},
                 'program', NULL, {waiting}, 0, 0, 1, '{status}',
                 '{{\"process\":\"{process_id}\"}}'
             )",
            waiting = status == "waiting",
        ))
        .await;
}

async fn seed_segment(scratch: &ScratchSchema, process_id: &str, ordinal: i64, handover: &str) {
    scratch
        .apply(&format!(
            "INSERT INTO lash_process_segment_handovers
                 (process_id, segment_ordinal, handover_json)
             VALUES ('{process_id}', {ordinal}, '{handover}')"
        ))
        .await;
}

async fn seed_wake(scratch: &ScratchSchema, delivery_id: &str, process_id: &str, state: &str) {
    scratch
        .apply(&format!(
            "INSERT INTO lash_process_wake_deliveries (
                 delivery_id, process_id, target_session_id, sequence, state,
                 next_attempt_at_ms, expires_at_ms, delivery_json
             ) VALUES (
                 '{delivery_id}', '{process_id}', 'session-target', 1, '{state}',
                 0, 0, '{{\"delivery\":\"{delivery_id}\"}}'
             )"
        ))
        .await;
}

async fn seed_session(scratch: &ScratchSchema, session_id: &str, checkpoint_ref: &str) {
    scratch
        .apply(&format!(
            "INSERT INTO lash_sessions (session_id, head_revision, head_json, checkpoint_ref)
             VALUES ('{session_id}', 1, '{{}}', '{checkpoint_ref}')"
        ))
        .await;
}

async fn seed_blob(scratch: &ScratchSchema, hash: &str, content: &[u8]) {
    let hex: String = content.iter().map(|byte| format!("{byte:02x}")).collect();
    scratch
        .apply(&format!(
            "INSERT INTO lash_blobs (hash, content) VALUES ('{hash}', '\\x{hex}')"
        ))
        .await;
}
