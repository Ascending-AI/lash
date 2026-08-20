//! The probe's behaviour against a handle whose durable data is controlled
//! exactly.
//!
//! The fake backend here is not a shortcut around a real store: it is how the
//! interesting cases get tested at all. A store cannot be made to hold a
//! payload from a *future* build — the build that writes it is the one running
//! the test — and the version boundary that matters most is exactly that one.

use std::collections::BTreeMap;

use async_trait::async_trait;
use lash_core::{
    DurableItem, DurablePayload, DurableScan, DurableScanPage, DurableSurface, ScanCoverage,
    StoreBackend, StoreError, StorePreflight, StoreSchemaDatabase, StoreSchemaStatus,
    StoreSchemaVerdict,
};

use super::*;
use crate::formats::{
    LASHLANG_SEGMENT_STATE_VERSION, PROCESS_WAKE_DELIVERY_FORMAT_VERSION, RLM_SNAPSHOT_VERSION,
    SESSION_CHECKPOINT_SCHEMA_VERSION, VM_CONTINUATION_FORMAT_VERSION,
};

/// A handle whose surfaces are exactly what a test declares.
#[derive(Default)]
struct FakeStore {
    databases: Vec<StoreSchemaDatabase>,
    surfaces: BTreeMap<DurableSurface, Vec<DurableItem>>,
    /// Surfaces described as the rows a backend *reads* rather than the items it
    /// emits.
    ///
    /// Both real backends page the execution-state surface by session and mint
    /// the next cursor from the last session read, so a page whose sessions all
    /// hold no execution state returns no items and still advances. `surfaces`
    /// cannot express that shape, because it derives the cursor from the items.
    rows: BTreeMap<DurableSurface, Vec<(String, Option<DurableItem>)>>,
    unscanned: BTreeMap<DurableSurface, String>,
    /// Surfaces whose backend hands back the cursor it was given.
    stuck: BTreeMap<DurableSurface, String>,
}

impl FakeStore {
    fn with_database(mut self, name: &str, expected: i64, verdict: StoreSchemaVerdict) -> Self {
        self.databases.push(StoreSchemaDatabase {
            name: name.to_string(),
            location: format!("/srv/lash/{name}.db"),
            expected,
            verdict,
        });
        self
    }

    fn with_items(mut self, surface: DurableSurface, items: Vec<DurableItem>) -> Self {
        self.surfaces.insert(surface, items);
        self
    }

    /// Describe a surface as the rows the backend reads, each of which may or
    /// may not carry an item, with the page cursor minted from the last row.
    fn with_rows(
        mut self,
        surface: DurableSurface,
        rows: Vec<(&str, Option<DurableItem>)>,
    ) -> Self {
        self.rows.insert(
            surface,
            rows.into_iter()
                .map(|(cursor, item)| (cursor.to_string(), item))
                .collect(),
        );
        self
    }

    fn unscannable(mut self, surface: DurableSurface, reason: &str) -> Self {
        self.unscanned.insert(surface, reason.to_string());
        self
    }

    /// Make a surface hand back the cursor it was given, forever.
    fn stuck_cursor(mut self, surface: DurableSurface, cursor: &str) -> Self {
        self.stuck.insert(surface, cursor.to_string());
        self
    }
}

#[async_trait]
impl StorePreflight for FakeStore {
    fn backend(&self) -> StoreBackend {
        StoreBackend::Sqlite {
            location: "/srv/lash/durable-core.db".to_string(),
        }
    }

    async fn schema_status(&self) -> Result<StoreSchemaStatus, StoreError> {
        Ok(StoreSchemaStatus {
            databases: self.databases.clone(),
        })
    }

    async fn scan_durable(&self, scan: &DurableScan) -> Result<DurableScanPage, StoreError> {
        if let Some(reason) = self.unscanned.get(&scan.surface) {
            return Ok(DurableScanPage {
                items: Vec::new(),
                next: None,
                coverage: ScanCoverage::NotScanned {
                    reason: reason.clone(),
                },
            });
        }
        if let Some(cursor) = self.stuck.get(&scan.surface) {
            return Ok(DurableScanPage {
                items: Vec::new(),
                next: Some(cursor.clone()),
                coverage: ScanCoverage::Scanned,
            });
        }
        if let Some(rows) = self.rows.get(&scan.surface) {
            let start = match &scan.after {
                None => 0,
                Some(cursor) => rows
                    .iter()
                    .position(|(row, _)| row == cursor)
                    .map(|index| index + 1)
                    .unwrap_or(rows.len()),
            };
            let page: Vec<&(String, Option<DurableItem>)> =
                rows.iter().skip(start).take(scan.limit).collect();
            let exhausted = start + page.len() >= rows.len();
            // Deliberately the last row read, not the last item emitted: that is
            // what both real backends do.
            let next = if exhausted {
                None
            } else {
                page.last().map(|(cursor, _)| cursor.clone())
            };
            return Ok(DurableScanPage {
                items: page
                    .into_iter()
                    .filter_map(|(_, item)| item.clone())
                    .collect(),
                next,
                coverage: ScanCoverage::Scanned,
            });
        }
        let all = self
            .surfaces
            .get(&scan.surface)
            .cloned()
            .unwrap_or_default();
        let start = match &scan.after {
            None => 0,
            Some(cursor) => all
                .iter()
                .position(|item| &item.cursor == cursor)
                .map(|index| index + 1)
                .unwrap_or(all.len()),
        };
        let items: Vec<DurableItem> = all.iter().skip(start).take(scan.limit).cloned().collect();
        let exhausted = start + items.len() >= all.len();
        let next = if exhausted {
            None
        } else {
            items.last().map(|item| item.cursor.clone())
        };
        Ok(DurableScanPage {
            items,
            next,
            coverage: ScanCoverage::Scanned,
        })
    }
}

fn segment_item(process: &str, session: &str, segment: u32, continuation: u32) -> DurableItem {
    let engine_state = serde_json::to_vec(&serde_json::json!({
        "version": segment,
        "vm": {"format_version": continuation},
    }))
    .expect("the fixture encodes");
    DurableItem {
        surface: DurableSurface::ParkedSegment,
        cursor: format!("{process}:0"),
        process_id: Some(process.to_string()),
        session_id: Some(session.to_string()),
        status: Some("waiting".to_string()),
        owner_record: None,
        payload: DurablePayload::Json(
            serde_json::json!({
                "segment_ordinal": 0,
                "handover": {"engine_state": engine_state},
            })
            .to_string(),
        ),
    }
}

fn wake_item(delivery: &str, process: &str, version: u32) -> DurableItem {
    DurableItem {
        surface: DurableSurface::PendingWake,
        cursor: delivery.to_string(),
        process_id: Some(process.to_string()),
        session_id: Some("s-1".to_string()),
        status: None,
        owner_record: None,
        payload: DurablePayload::Json(
            serde_json::json!({"wake_id": delivery, "version": version}).to_string(),
        ),
    }
}

fn checkpoint_item(session: &str, schema_version: u32, encoding: u32) -> DurableItem {
    let bytes = rmp_serde::to_vec_named(&serde_json::json!({
        "schema_version": schema_version,
        "components": {"execution_state": {"blob_ref": "sha256:a", "encoding_version": encoding}},
    }))
    .expect("the fixture encodes");
    DurableItem {
        surface: DurableSurface::SessionCheckpoint,
        cursor: session.to_string(),
        process_id: None,
        session_id: Some(session.to_string()),
        status: None,
        owner_record: None,
        payload: DurablePayload::MessagePack(bytes),
    }
}

fn execution_state_item(session: &str, version: u32) -> DurableItem {
    let bytes = rmp_serde::to_vec_named(&serde_json::json!({"version": version, "engine": "rlm"}))
        .expect("the fixture encodes");
    DurableItem {
        surface: DurableSurface::SessionExecutionState,
        cursor: session.to_string(),
        process_id: None,
        session_id: Some(session.to_string()),
        status: None,
        owner_record: None,
        payload: DurablePayload::MessagePack(bytes),
    }
}

fn component(report: &PreflightReport, format: DurableFormat) -> &ComponentReadability {
    report
        .components
        .iter()
        .find(|row| row.format == format.name())
        .unwrap_or_else(|| panic!("the report has a row for {}", format.name()))
}

fn healthy_store() -> FakeStore {
    FakeStore::default()
        .with_database("durable core", 37, StoreSchemaVerdict::Matches)
        .with_items(
            DurableSurface::ParkedSegment,
            vec![segment_item(
                "p-1",
                "s-1",
                LASHLANG_SEGMENT_STATE_VERSION,
                VM_CONTINUATION_FORMAT_VERSION,
            )],
        )
        .with_items(
            DurableSurface::PendingWake,
            vec![wake_item(
                "w-1",
                "p-1",
                PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
            )],
        )
        .with_items(
            DurableSurface::SessionCheckpoint,
            vec![checkpoint_item("s-1", SESSION_CHECKPOINT_SCHEMA_VERSION, 2)],
        )
        .with_items(
            DurableSurface::SessionExecutionState,
            vec![execution_state_item("s-1", RLM_SNAPSHOT_VERSION)],
        )
}

#[tokio::test]
async fn a_store_this_build_wrote_is_ready_with_an_empty_drain_list() {
    let report = probe_store(&healthy_store(), PreflightOptions::deep())
        .await
        .expect("the probe reads the store");
    assert_eq!(report.outcome, PreflightOutcome::Ready);
    assert!(report.drain.is_empty(), "{:?}", report.drain);
    assert_eq!(report.refusal_message(), None);
    assert_eq!(
        component(&report, DurableFormat::LashlangSegmentHandover).verdict,
        ComponentVerdict::AllReadable
    );
    assert_eq!(
        component(&report, DurableFormat::SessionCheckpointManifest).verdict,
        ComponentVerdict::AllReadable
    );
    assert_eq!(
        component(&report, DurableFormat::ModuleArtifact).verdict,
        ComponentVerdict::Empty
    );
}

#[tokio::test]
async fn a_future_module_artifact_refusal_names_recompile_and_republish() {
    let mut raw: serde_json::Value = serde_json::from_str(include_str!(
        "../../../lashlang/tests/fixtures/module-artifact-old.json"
    ))
    .expect("frozen fixture should be JSON");
    raw["compilation_dialect"] = serde_json::json!("future_dialect");
    raw["canonical_ir"]["main"] = serde_json::json!({"FutureExpr": null});
    let item = DurableItem {
        surface: DurableSurface::ModuleArtifact,
        cursor: "lashlang:v1:sha256:future".to_string(),
        process_id: None,
        session_id: None,
        status: None,
        owner_record: None,
        payload: DurablePayload::Json(
            serde_json::to_string(&raw).expect("future fixture should encode"),
        ),
    };
    let report = probe_store(
        &FakeStore::default().with_items(DurableSurface::ModuleArtifact, vec![item]),
        PreflightOptions::summary(),
    )
    .await
    .expect("the probe reads the store");
    assert_eq!(report.outcome, PreflightOutcome::Refused);
    assert_eq!(
        component(&report, DurableFormat::ModuleArtifact).verdict,
        ComponentVerdict::Refused
    );
    let message = report.refusal_message().expect("a refusal has a message");
    assert!(message.contains("recompile and republish"), "{message}");
}

#[tokio::test]
async fn a_segment_from_another_build_refuses_and_lands_on_the_drain_list() {
    // The whole point of the surface: the refusal is named, counted and
    // attributed to a process an operator can drain, before anything is wired.
    let store = FakeStore::default()
        .with_database("durable core", 37, StoreSchemaVerdict::Matches)
        .with_items(
            DurableSurface::ParkedSegment,
            vec![
                segment_item(
                    "p-1",
                    "s-1",
                    LASHLANG_SEGMENT_STATE_VERSION - 1,
                    VM_CONTINUATION_FORMAT_VERSION,
                ),
                segment_item(
                    "p-2",
                    "s-2",
                    LASHLANG_SEGMENT_STATE_VERSION,
                    VM_CONTINUATION_FORMAT_VERSION,
                ),
            ],
        );
    let report = probe_store(&store, PreflightOptions::summary())
        .await
        .expect("the probe reads the store");

    assert_eq!(report.outcome, PreflightOutcome::Refused);
    let handover = component(&report, DurableFormat::LashlangSegmentHandover);
    assert_eq!(handover.verdict, ComponentVerdict::Refused);
    assert_eq!(handover.scanned, 2);
    assert_eq!(
        handover.found,
        vec![
            FoundVersion {
                version: LASHLANG_SEGMENT_STATE_VERSION - 1,
                count: 1
            },
            FoundVersion {
                version: LASHLANG_SEGMENT_STATE_VERSION,
                count: 1
            },
        ],
        "both versions are counted, not just the refusing one"
    );

    assert_eq!(report.drain.len(), 1, "{:?}", report.drain);
    let blocker = &report.drain[0];
    assert_eq!(blocker.process_id.as_deref(), Some("p-1"));
    assert_eq!(blocker.session_id.as_deref(), Some("s-1"));
    assert_eq!(blocker.status.as_deref(), Some("waiting"));
    assert_eq!(blocker.found, Some(LASHLANG_SEGMENT_STATE_VERSION - 1));

    let message = report.refusal_message().expect("a refusal has a message");
    assert!(message.contains("Drain 1 affected item(s)"), "{message}");
}

#[tokio::test]
async fn a_schema_refusal_alone_is_still_a_refusal() {
    // The boundary a host hits first, and the one that produced the crash loop
    // this surface exists to replace.
    let store = FakeStore::default().with_database(
        "durable core",
        37,
        StoreSchemaVerdict::Mismatch { found: 36 },
    );
    let report = probe_store(&store, PreflightOptions::summary())
        .await
        .expect("the probe reads the store");
    assert_eq!(report.outcome, PreflightOutcome::Refused);
    assert_eq!(report.schema.outcome, "refused");
    assert_eq!(report.schema.databases[0].found, Some(36));
    let message = report.refusal_message().expect("a refusal has a message");
    assert!(
        message.contains("schema `durable core` is at version 36"),
        "{message}"
    );
}

#[tokio::test]
async fn an_unreadable_database_is_undecided_rather_than_ready() {
    let store = FakeStore::default().with_database(
        "durable core",
        37,
        StoreSchemaVerdict::Unreadable {
            reason: "file is not a database".to_string(),
        },
    );
    let report = probe_store(&store, PreflightOptions::summary())
        .await
        .expect("the probe reads the store");
    assert_eq!(report.outcome, PreflightOutcome::Undecided);
    assert_eq!(report.schema.databases[0].verdict, "unreadable");
    assert_eq!(
        report.schema.databases[0].reason.as_deref(),
        Some("file is not a database")
    );
    assert_eq!(
        report.refusal_message(),
        None,
        "an undecided store must not be reported as a permanent refusal"
    );
}

#[tokio::test]
async fn a_payload_nobody_can_decode_is_undecided_and_never_panics() {
    let mut item = wake_item("w-1", "p-1", PROCESS_WAKE_DELIVERY_FORMAT_VERSION);
    item.payload = DurablePayload::Json("not json at all".to_string());
    let store = FakeStore::default().with_items(DurableSurface::PendingWake, vec![item]);
    let report = probe_store(&store, PreflightOptions::summary())
        .await
        .expect("the probe reads the store");
    let wake = component(&report, DurableFormat::ProcessWakeDelivery);
    assert_eq!(wake.verdict, ComponentVerdict::Undecodable);
    assert_eq!(wake.undecodable, 1);
    assert_eq!(wake.undecodable_reasons.len(), 1);
    assert_eq!(report.outcome, PreflightOutcome::Undecided);
    assert!(report.drain.is_empty(), "nobody read a version to refuse");
}

#[tokio::test]
async fn summary_mode_names_the_per_session_walk_it_skipped() {
    // The silence that would otherwise read as a clean bill of health.
    let report = probe_store(&healthy_store(), PreflightOptions::summary())
        .await
        .expect("the probe reads the store");
    let skipped: Vec<&str> = report
        .not_scanned
        .iter()
        .map(|entry| entry.what.as_str())
        .collect();
    assert!(
        skipped.contains(&DurableSurface::SessionCheckpoint.name()),
        "{skipped:?}"
    );
    assert!(
        skipped.contains(&DurableSurface::SessionExecutionState.name()),
        "{skipped:?}"
    );
    assert_eq!(
        component(&report, DurableFormat::SessionCheckpointManifest).verdict,
        ComponentVerdict::NotScanned,
        "a format whose surface was skipped must not read as empty"
    );
    assert_eq!(
        component(&report, DurableFormat::LashlangSegmentHandover).verdict,
        ComponentVerdict::AllReadable,
        "the cheap surfaces are still walked"
    );
}

#[tokio::test]
async fn a_deep_probe_reads_the_surfaces_summary_skipped() {
    let report = probe_store(&healthy_store(), PreflightOptions::deep())
        .await
        .expect("the probe reads the store");
    let skipped: Vec<&str> = report
        .not_scanned
        .iter()
        .map(|entry| entry.what.as_str())
        .collect();
    assert!(
        !skipped.contains(&DurableSurface::SessionCheckpoint.name()),
        "{skipped:?}"
    );
    assert_eq!(
        component(&report, DurableFormat::RlmSnapshotEnvelope).verdict,
        ComponentVerdict::AllReadable
    );
    assert_eq!(
        component(&report, DurableFormat::CheckpointComponentEncoding).scanned,
        1
    );
}

#[tokio::test]
async fn a_report_always_names_the_formats_no_walk_enumerates() {
    // Two durable formats have no bounded surface. Leaving them out of the
    // report entirely would make the manifest and the probe disagree about what
    // this build writes.
    let report = probe_store(&healthy_store(), PreflightOptions::deep())
        .await
        .expect("the probe reads the store");
    let named: Vec<&str> = report
        .not_scanned
        .iter()
        .map(|entry| entry.what.as_str())
        .collect();
    assert!(
        named.contains(&DurableFormat::SessionHeadMeta.name()),
        "{named:?}"
    );
    assert!(
        named.contains(&DurableFormat::SessionNodeBody.name()),
        "{named:?}"
    );
    assert_eq!(
        component(&report, DurableFormat::SessionNodeBody).verdict,
        ComponentVerdict::NotScanned
    );
}

#[tokio::test]
async fn a_backend_that_cannot_walk_a_surface_says_so_verbatim() {
    let store = healthy_store().unscannable(
        DurableSurface::ParkedSegment,
        "the deployment declared no process registry",
    );
    let report = probe_store(&store, PreflightOptions::summary())
        .await
        .expect("the probe reads the store");
    assert!(
        report.not_scanned.iter().any(|entry| {
            entry.what == DurableSurface::ParkedSegment.name()
                && entry.reason == "the deployment declared no process registry"
        }),
        "{:?}",
        report.not_scanned
    );
    assert_eq!(
        component(&report, DurableFormat::LashlangSegmentHandover).verdict,
        ComponentVerdict::NotScanned,
        "an unwalked surface must never read as an empty one"
    );
}

#[tokio::test]
async fn paging_reads_every_item_exactly_once() {
    // A walk that dropped or double-counted items would report drain lists an
    // operator cannot reconcile with the store.
    let items: Vec<DurableItem> = (0..7)
        .map(|index| {
            segment_item(
                &format!("p-{index}"),
                "s-1",
                LASHLANG_SEGMENT_STATE_VERSION - 1,
                VM_CONTINUATION_FORMAT_VERSION,
            )
        })
        .collect();
    let store = FakeStore::default().with_items(DurableSurface::ParkedSegment, items);
    let report = probe_store(&store, PreflightOptions::summary().with_page_size(2))
        .await
        .expect("the probe reads the store");
    assert_eq!(
        component(&report, DurableFormat::LashlangSegmentHandover).scanned,
        7
    );
    assert_eq!(report.drain.len(), 7);
    let mut named: Vec<&str> = report
        .drain
        .iter()
        .filter_map(|blocker| blocker.process_id.as_deref())
        .collect();
    named.sort();
    named.dedup();
    assert_eq!(named.len(), 7, "no process appears twice");
}

#[cfg(feature = "rlm")]
#[test]
fn an_identity_only_refusal_says_how_many_items_another_build_wrote() {
    // Bytecode refuses without a version integer to print. Rendering its empty
    // found-list would hand a supervisor "`bytecode` is at  and this build
    // writes 9", which reads as a bug in the probe rather than a boundary in
    // the store.
    let report = PreflightReport {
        backend: "sqlite (/srv/lash)".to_string(),
        mode: PreflightMode::Summary,
        outcome: PreflightOutcome::Refused,
        schema: SchemaReport {
            outcome: "ready",
            databases: Vec::new(),
        },
        components: vec![ComponentReadability {
            format: DurableFormat::Bytecode.name().to_string(),
            expected: crate::formats::BYTECODE_FORMAT_VERSION.to_string(),
            probe: "identity only",
            evidence: FormatEvidence::Direct,
            verdict: ComponentVerdict::Refused,
            scanned: 3,
            found: Vec::new(),
            undecodable: 0,
            undecodable_reasons: Vec::new(),
            refused_without_version: 3,
        }],
        drain: Vec::new(),
        not_scanned: Vec::new(),
    };
    let message = report.refusal_message().expect("a refusal has a message");
    assert!(
        message.contains("3 item(s) written by another build"),
        "the count stands in for the version that was never stored: {message}"
    );
    assert!(
        !message.contains("is at  and"),
        "no blank where a version would go: {message}"
    );
}

#[tokio::test]
async fn a_page_that_contributes_nothing_does_not_abandon_the_surface() {
    // The ordinary shape, not a backend bug: both backends page the
    // execution-state surface by session and mint the cursor from the last
    // session *read*, so the sessions that hold no execution state produce a
    // full page with no items and a cursor that still advances. Stopping there
    // would report every format behind this surface as unscanned, on a store
    // that is merely paged.
    let store = healthy_store().with_rows(
        DurableSurface::SessionExecutionState,
        vec![
            ("session-1", None),
            ("session-2", None),
            (
                "session-3",
                Some(execution_state_item("session-3", RLM_SNAPSHOT_VERSION)),
            ),
        ],
    );
    let report = probe_store(&store, PreflightOptions::deep().with_page_size(2))
        .await
        .expect("the probe reads the store");
    let envelope = component(&report, DurableFormat::RlmSnapshotEnvelope);
    assert_eq!(
        envelope.scanned, 1,
        "the item behind the empty page is read"
    );
    assert_ne!(
        envelope.verdict,
        ComponentVerdict::NotScanned,
        "a paged surface is not an unscanned one"
    );
    assert!(
        !report
            .not_scanned
            .iter()
            .any(|entry| entry.what == DurableSurface::SessionExecutionState.name()),
        "no surface is abandoned: {:?}",
        report.not_scanned
    );
}

#[tokio::test]
async fn a_cursor_that_never_advances_stops_the_walk() {
    // The bug the guard is actually for. A backend that hands back the cursor
    // it was given would page forever, so the walk stops and says so rather
    // than hanging a boot.
    let store = healthy_store().stuck_cursor(DurableSurface::SessionExecutionState, "session-1");
    let report = probe_store(&store, PreflightOptions::deep())
        .await
        .expect("the probe reads the store");
    let stopped = report
        .not_scanned
        .iter()
        .find(|entry| entry.what == DurableSurface::SessionExecutionState.name())
        .expect("the stalled surface is named");
    assert!(
        stopped.reason.contains("session-1") && stopped.reason.contains("again"),
        "the reason names the repeated cursor: {}",
        stopped.reason
    );
}

#[tokio::test]
async fn a_zero_page_still_makes_progress() {
    let store = healthy_store();
    let report = probe_store(&store, PreflightOptions::summary().with_page_size(0))
        .await
        .expect("the probe reads the store");
    assert_eq!(
        component(&report, DurableFormat::LashlangSegmentHandover).scanned,
        1
    );
}

#[tokio::test]
async fn a_carried_format_inherits_its_carriers_verdict_in_both_directions() {
    // The honest limit: neither format is stored on its own, so the boundary
    // that decides them is the envelope's. The report says which envelope.
    let healthy = probe_store(&healthy_store(), PreflightOptions::deep())
        .await
        .expect("the probe reads the store");
    let snapshot = component(&healthy, DurableFormat::LashlangSnapshot);
    assert_eq!(
        snapshot.evidence,
        FormatEvidence::CarriedBy(DurableFormat::RlmSnapshotEnvelope.name())
    );
    assert_eq!(snapshot.verdict, ComponentVerdict::AllReadable);

    let store = healthy_store().with_items(
        DurableSurface::SessionExecutionState,
        vec![execution_state_item("s-1", RLM_SNAPSHOT_VERSION - 1)],
    );
    let refused = probe_store(&store, PreflightOptions::deep())
        .await
        .expect("the probe reads the store");
    assert_eq!(
        component(&refused, DurableFormat::LashlangSnapshot).verdict,
        ComponentVerdict::Refused,
        "a refused carrier refuses everything it carries"
    );
    assert_eq!(
        component(&refused, DurableFormat::HeapSizeSchedule).evidence,
        FormatEvidence::CarriedBy(DurableFormat::VmContinuation.name())
    );
}

#[tokio::test]
async fn the_vm_abi_is_reported_without_a_verdict_it_cannot_have() {
    let report = probe_store(&healthy_store(), PreflightOptions::deep())
        .await
        .expect("the probe reads the store");
    let abi = component(&report, DurableFormat::VmAbi);
    assert_eq!(abi.evidence, FormatEvidence::NotPersisted);
    assert_eq!(abi.scanned, 0);
    assert_eq!(
        abi.verdict,
        ComponentVerdict::Empty,
        "nothing durable exists to refuse, so it is empty rather than unscanned"
    );
}

#[tokio::test]
async fn the_serialized_report_carries_every_field_a_gate_asserts_on() {
    let store = FakeStore::default()
        .with_database("durable core", 37, StoreSchemaVerdict::Matches)
        .with_items(
            DurableSurface::ParkedSegment,
            vec![segment_item(
                "p-1",
                "s-1",
                LASHLANG_SEGMENT_STATE_VERSION - 1,
                VM_CONTINUATION_FORMAT_VERSION,
            )],
        );
    let report = probe_store(&store, PreflightOptions::summary())
        .await
        .expect("the probe reads the store");
    let json = serde_json::to_value(&report).expect("the report serializes");

    assert_eq!(json["outcome"], "refused");
    assert_eq!(json["mode"], "summary");
    assert_eq!(json["backend"], "sqlite (/srv/lash/durable-core.db)");
    assert_eq!(json["schema"]["outcome"], "ready");
    assert_eq!(json["drain"][0]["process_id"], "p-1");
    assert_eq!(
        json["drain"][0]["found"],
        LASHLANG_SEGMENT_STATE_VERSION - 1
    );
    assert!(
        json["not_scanned"]
            .as_array()
            .is_some_and(|list| !list.is_empty())
    );
    let handover = json["components"]
        .as_array()
        .expect("components is a list")
        .iter()
        .find(|row| row["format"] == DurableFormat::LashlangSegmentHandover.name())
        .expect("the handover row is present");
    assert_eq!(handover["verdict"], "refused");
    assert_eq!(handover["probe"], "comparable");
    assert_eq!(handover["evidence"]["kind"], "direct");
    assert_eq!(handover["scanned"], 1);
}
