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

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

use lash_core_execution::ProcessIdMint;
use lash_core_execution::store::SessionCheckpoint;
use lash_core_execution::{
    ArtifactReferrer, BlobRef, CheckpointComponentDescriptor, DurablePayload, DurableScan,
    DurableSurface, HostArtifactPin, ModuleArtifactStore, ReferrerClaim, ScanCoverage,
    StorePreflight,
};
use lash_postgres_store::PostgresStorePreflight;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;

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
        DurableSurface::StartedProcess,
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
    let Some(database_url) = database_url() else {
        eprintln!("skipping module artifact preflight walk: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let storage = lash_postgres_store::testing::from_pool(
        scratch.pool.clone(),
        &lash_postgres_store::PostgresHostConfig::default(),
    )
    .await
    .expect("open provisioned Postgres storage");
    let artifact = lashlang::ModuleArtifact::from_program(lashlang::Program::block(vec![
        lashlang::Expr::Finish(Box::new(lashlang::Expr::String("done".into()))),
    ]))
    .expect("a one-statement module forms an artifact");
    let claim = ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint()))
        .expect("host pin is unguarded");
    let bytes = artifact.to_store_bytes().expect("encode module artifact");
    storage
        .lashlang_artifact_store()
        .publish_module_artifact(&claim, artifact.module_ref().as_str(), &bytes)
        .await
        .expect("persist module artifact");
    drop(storage);

    let page = PostgresStorePreflight::from_pool(scratch.pool.clone())
        .scan_durable(&DurableScan::first(DurableSurface::ModuleArtifact, 10))
        .await
        .expect("walk module artifacts");
    assert_eq!(page.coverage, ScanCoverage::Scanned);
    assert_eq!(page.items.len(), 1, "{page:?}");
    assert_eq!(page.items[0].cursor, artifact.module_ref().as_str());
    match &page.items[0].payload {
        DurablePayload::Json(json) => assert!(
            json.contains("host_requirements_ref") && json.contains("\"ir\""),
            "{json}"
        ),
        other => panic!("expected module artifact JSON, got {other:?}"),
    }

    scratch.cleanup().await;
}

/// C8 (FIG-3571): every live process is walked with its record, which carries
/// the start stamp the probe judges; a terminal one is not.
#[tokio::test]
async fn a_live_process_is_walked_with_its_record_and_terminal_ones_are_not() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping preflight durable walk: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let live = ProcessId::fixture("proc-live");
    seed_process(&scratch, &live, "running").await;
    seed_process(&scratch, &ProcessId::fixture("proc-done"), "completed").await;

    let preflight = PostgresStorePreflight::from_pool(scratch.pool.clone());
    let page = preflight
        .scan_durable(&DurableScan::first(DurableSurface::StartedProcess, 10))
        .await
        .expect("a provisioned deployment walks");

    assert_eq!(page.coverage, ScanCoverage::Scanned);
    assert_eq!(page.items.len(), 1, "{:?}", page.items);
    let item = &page.items[0];
    assert_eq!(item.surface, DurableSurface::StartedProcess);
    assert_eq!(item.cursor, live.as_str());
    assert_eq!(item.process_id.as_ref(), Some(&live));
    assert_eq!(item.session_id, None);
    assert_eq!(item.status.as_deref(), Some("running"));
    assert_eq!(
        item.payload,
        DurablePayload::Json(format!(r#"{{"process":"{live}"}}"#))
    );
    assert!(page.next.is_none());

    scratch.cleanup().await;
}

#[tokio::test]
async fn paging_a_surface_one_item_at_a_time_is_exact() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping preflight durable walk: database URL is not set");
        return;
    };
    let scratch = ScratchSchema::provision(&database_url).await;
    let ids: Vec<ProcessId> = (1..=3)
        .map(ProcessIdMint::sequential_id_for_testing)
        .collect();
    for (id, status) in ids.iter().zip(["waiting", "running", "waiting"]) {
        seed_process(&scratch, id, status).await;
    }

    let preflight = PostgresStorePreflight::from_pool(scratch.pool.clone());
    let mut walked: Vec<ProcessId> = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..8 {
        let scan = DurableScan {
            surface: DurableSurface::StartedProcess,
            after: after.clone(),
            limit: 1,
        };
        let page = preflight.scan_durable(&scan).await.expect("walk one item");
        assert!(page.items.len() <= 1, "a page never exceeds its limit");
        walked.extend(page.items.iter().filter_map(|item| item.process_id.clone()));
        match page.next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }

    assert_eq!(
        walked, ids,
        "every process appears exactly once, in key order"
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
    seed_session(&scratch, &SessionId::from("session-healthy"), manifest_ref).await;
    seed_session(
        &scratch,
        &SessionId::from("session-dangling"),
        "hash-that-was-collected",
    )
    .await;

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
    seed_session(&scratch, &SessionId::from("session-1"), "hash-with").await;
    // Scanned second, emits nothing: it has no execution-state component at all.
    seed_session(&scratch, &SessionId::from("session-2"), "hash-without").await;

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
        lash_core_execution::store::EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
        CheckpointComponentDescriptor {
            blob_ref: BlobRef(execution_state_ref.to_string()),
            encoding_version: lash_core_execution::store::CHECKPOINT_COMPONENT_ENCODING_VERSION,
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
        schema_version: lash_core_execution::store::SESSION_CHECKPOINT_SCHEMA_VERSION,
        turn_state: lash_core_execution::PersistedTurnState::default(),
        components,
    };
    let mut bytes = Vec::new();
    rmp_serde::encode::write_named(&mut bytes, &manifest).expect("encode checkpoint manifest");
    bytes
}

async fn seed_process(scratch: &ScratchSchema, process_id: &ProcessId, status: &str) {
    scratch
        .apply(&format!(
            "INSERT INTO lash_processes (
                 process_id, start_key, originator_id,
                 identity_kind, identity_label, created_at_ms, updated_at_ms,
                 last_event_sequence, change_seq, status, lifetime_scope_kind, lifetime_scope_id,
                 lifetime,
                 record_json
             ) VALUES (
                 '{process_id}', NULL, 'originator',
                 'program', NULL, 0, 0, 0, 1, '{status}', NULL, NULL, 'detached',
                 '{{\"process\":\"{process_id}\"}}'
             )",
        ))
        .await;
}

async fn seed_session(scratch: &ScratchSchema, session_id: &SessionId, checkpoint_ref: &str) {
    scratch
        .apply(&format!(
            "WITH recorded AS (
                 INSERT INTO lash_session_revisions (session_id, head_revision, head_json, checkpoint_ref)
                 VALUES ('{session_id}', 1, '{{}}', '{checkpoint_ref}') RETURNING session_id, head_revision
             ) INSERT INTO lash_session_head (session_id, head_revision)
                 SELECT session_id, head_revision FROM recorded"
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
