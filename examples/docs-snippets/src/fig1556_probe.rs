//! Store readability probe and drain preflight (FIG-1556, pieces 3 and 4):
//! asking *which* durable data will not open under this build, and *whose* it
//! is.
//!
//! Piece 1 answered the schema question — the boundary a host hits first. This
//! is the rest of the answer. Lash's durable formats fail closed with no
//! migration decoders, so a build that meets state written by another build
//! refuses it. A host that discovers that by booting is in a crash loop; a host
//! that asks first gets three things it can act on: which format refuses, how
//! many items carry the refusing version, and which processes and sessions have
//! to be drained before the deploy.
//!
//! Everything below reads. Nothing here marks, migrates or deletes: disposal
//! stays with the operator and the published drain/recreate procedures.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use async_trait::async_trait;
use lash::persistence::{
    DurableItem, DurablePayload, DurableScan, DurableScanPage, DurableSurface, ScanCoverage,
    StoreBackend, StoreError, StorePreflight, StoreSchemaDatabase, StoreSchemaStatus,
    StoreSchemaVerdict,
};
use lash::preflight::{
    ComponentReadability, ComponentVerdict, DEFAULT_PAGE_SIZE, DrainBlocker, FormatEvidence,
    FoundVersion, NotScanned, PreflightMode, PreflightOptions, PreflightOutcome, PreflightReport,
    SchemaDatabaseReport, SchemaReport, probe_store,
};

/// A host boots against the probe, not against the store.
///
/// The gate is one probe, one decision, one exit. A supervisor that reads a
/// single sentence naming the version boundary and the remedy can be configured
/// not to restart; a supervisor that reads a panic from somewhere inside turn
/// execution restarts forever, because a restart is the one remedy a permanent
/// refusal cannot accept.
pub async fn boot_gate(handle: &dyn StorePreflight) -> Result<String, String> {
    // docs:start:boot-gate
    let report = probe_store(handle, PreflightOptions::summary())
        .await
        .map_err(|error| format!("could not read the store: {error}"))?;
    if let Some(refusal) = report.refusal_message() {
        // One line, then exit. Everything an operator needs to act is in it.
        return Err(refusal);
    }
    let ready = format!(
        "store preflight: {} ({} mode)",
        report.outcome.name(),
        report.mode.name()
    );
    // docs:end:boot-gate
    Ok(ready)
}

/// The audit an operator runs *before* a version bump, rather than the gate a
/// host runs on every boot.
///
/// Deep mode adds the per-session checkpoint walk, which summary mode skips
/// because it costs at least one blob read per session. The page size is the
/// round-trip knob, not a cap: the walk pages until each surface is exhausted,
/// so a smaller page changes how many queries the audit costs and never how
/// much of the store it covers.
pub async fn pre_bump_audit(handle: &dyn StorePreflight) -> Result<PreflightReport, StoreError> {
    // docs:start:deep-probe
    let options = PreflightOptions::deep().with_page_size(DEFAULT_PAGE_SIZE);
    eprintln!("store audit: {}", options_note(&options));
    let report = probe_store(handle, options).await?;
    // docs:end:deep-probe
    Ok(report)
}

/// What an audit is about to cost, read back off the options it was given.
///
/// The two knobs are not the same kind of thing: the mode decides which
/// surfaces are read at all, and the page size decides only how many round
/// trips reading them takes.
pub fn options_note(options: &PreflightOptions) -> String {
    format!(
        "{} walk, {} items per page",
        options.mode.name(),
        options.page_size
    )
}

/// The report as lash itself renders it.
///
/// Worth using rather than reformatting when a host has nowhere better to put a
/// refusal than a log line: the built-in rendering moves with the report's
/// fields, and a host's own formatter silently stops mentioning a field the day
/// one is added.
pub fn rendered_by_lash(report: &PreflightReport) -> String {
    report.to_string()
}

/// Whether this deployment is safe to deploy onto, as three answers rather than
/// two.
///
/// The third answer is the one a boolean cannot carry: an item nobody could
/// decode is neither a refusal nor a pass, and a gate that treated it as either
/// would be acting on evidence it does not have.
pub fn outcome_label(outcome: PreflightOutcome) -> &'static str {
    match outcome {
        PreflightOutcome::Ready => "safe to start",
        PreflightOutcome::Refused => "will not open; drain or recreate",
        PreflightOutcome::Undecided => "investigate before starting",
        _ => "unclassified",
    }
}

/// The word a deploy runbook prints for how much of the store was read.
pub fn mode_label(mode: PreflightMode) -> &'static str {
    match mode {
        PreflightMode::Summary => "schema and process registry",
        PreflightMode::Deep => "schema, process registry and every session",
        _ => "unclassified",
    }
}

/// The row-level verdict, spelled out for an operator rather than abbreviated.
pub fn verdict_label(verdict: ComponentVerdict) -> &'static str {
    match verdict {
        ComponentVerdict::AllReadable => "every stored item opens",
        ComponentVerdict::Refused => "at least one stored item refuses",
        ComponentVerdict::Empty => "nothing stored",
        ComponentVerdict::Undecodable => "stored items could not be read",
        ComponentVerdict::NotScanned => "nobody looked",
        _ => "unclassified",
    }
}

/// Why a row says what it says.
///
/// The distinction a deploy gate has to keep: a directly probed row rests on
/// versions read out of stored bytes, a carried row rests on the envelope that
/// embeds it, and a not-persisted row rests on nothing at all because nothing
/// is stored.
pub fn evidence_label(evidence: FormatEvidence) -> String {
    match evidence {
        FormatEvidence::Direct => "read from stored bytes".to_string(),
        FormatEvidence::CarriedBy(carrier) => format!("carried by {carrier}"),
        FormatEvidence::NotPersisted => "never persisted".to_string(),
        _ => "unclassified".to_string(),
    }
}

/// One rendered line per durable format, in manifest order.
///
/// Driven by the report's rows rather than by the formats that happened to be
/// found, because a format that vanishes from a report is indistinguishable
/// from one nobody thought to check.
pub fn component_lines(report: &PreflightReport) -> Vec<String> {
    // docs:start:component-rows
    let mut lines = Vec::new();
    for component in &report.components {
        let mut line = format!(
            "{}: {} (expected {}, {} probe, {})",
            component.format,
            component.verdict.name(),
            component.expected,
            component.probe,
            evidence_label(component.evidence)
        );
        write!(line, " scanned={}", component.scanned).expect("a String write cannot fail");
        for FoundVersion { version, count } in &component.found {
            write!(line, " found {version} x{count}").expect("a String write cannot fail");
        }
        if component.undecodable > 0 {
            write!(line, " undecodable={}", component.undecodable)
                .expect("a String write cannot fail");
        }
        if component.refused_without_version > 0 {
            write!(
                line,
                " identity-refused={}",
                component.refused_without_version
            )
            .expect("a String write cannot fail");
        }
        lines.push(line);
    }
    // docs:end:component-rows
    lines
}

/// The formats that would refuse an open, named rather than counted.
pub fn refusing_formats(report: &PreflightReport) -> Vec<&str> {
    report
        .refusals()
        .map(|component| component.format.as_str())
        .collect()
}

/// Whether one row alone would refuse an open.
pub fn row_refuses(component: &ComponentReadability) -> bool {
    component.refuses_open()
}

/// The first undecodable reason a row kept, when it kept any.
///
/// A count alone sends an operator back for a second run; the reason makes the
/// count investigable from the report they already have.
pub fn first_undecodable_reason(component: &ComponentReadability) -> Option<&str> {
    component.undecodable_reasons.first().map(String::as_str)
}

/// The drain preflight, as the worklist it exists to be.
///
/// "Deployments drain first" is an instruction an operator has to trust. This
/// is the same claim as a list they can work through and check off, which is
/// the entire difference the drain preflight makes.
pub fn drain_worklist(report: &PreflightReport) -> Vec<String> {
    // docs:start:drain-list
    let mut worklist = Vec::new();
    for DrainBlocker {
        process_id,
        session_id,
        status,
        format,
        expected,
        found,
        detail,
    } in &report.drain
    {
        let owner = match (process_id, session_id) {
            (Some(process), Some(session)) => format!("process {process} (session {session})"),
            (Some(process), None) => format!("process {process}"),
            (None, Some(session)) => format!("session {session}"),
            (None, None) => "an unattributed item".to_string(),
        };
        // Bytecode is the one format with no found version to print: its stored
        // identity says which build wrote it, never which generation, so a
        // mismatch is reportable and a distance is not.
        let found = found
            .as_ref()
            .map(|version| version.to_string())
            .unwrap_or_else(|| "a build-specific identity".to_string());
        worklist.push(format!(
            "{owner} [{}] holds {format} {found}, expected {expected}: {detail}",
            status.as_deref().unwrap_or("unknown status")
        ));
    }
    // docs:end:drain-list
    worklist
}

/// What the probe did not read, which is part of the answer rather than a
/// footnote to it.
pub fn coverage_gaps(report: &PreflightReport) -> Vec<String> {
    // docs:start:not-scanned
    report
        .not_scanned
        .iter()
        .map(|NotScanned { what, reason }| format!("{what}: {reason}"))
        .collect()
    // docs:end:not-scanned
}

/// The schema section, rendered from the serializable projection the report
/// carries rather than from the store contract's own types.
pub fn schema_lines(schema: &SchemaReport) -> Vec<String> {
    let mut lines = vec![format!("schema: {}", schema.outcome)];
    for database in &schema.databases {
        lines.push(schema_database_line(database));
    }
    lines
}

/// One database's line, naming both numbers whenever both are known.
pub fn schema_database_line(database: &SchemaDatabaseReport) -> String {
    let found = database
        .found
        .map(|found| format!(", found {found}"))
        .unwrap_or_default();
    let reason = database
        .reason
        .as_deref()
        .map(|reason| format!(" ({reason})"))
        .unwrap_or_default();
    format!(
        "{} at {}: {} (expected {}{found}){reason}",
        database.name, database.location, database.verdict, database.expected
    )
}

/// The whole report as one operator-facing block.
pub fn render(report: &PreflightReport) -> String {
    let mut rendered = format!(
        "{}: {} — {}\n",
        report.backend,
        outcome_label(report.outcome),
        mode_label(report.mode)
    );
    for line in schema_lines(&report.schema)
        .into_iter()
        .chain(component_lines(report))
        .chain(drain_worklist(report))
        .chain(coverage_gaps(report))
    {
        rendered.push_str(&line);
        rendered.push('\n');
    }
    rendered
}

/// A read-only handle over durable payloads a host holds itself.
///
/// A backend implements [`StorePreflight`] on a handle built from raw
/// connection configuration, never on a wired store — constructing a store is
/// the side-effectful act a preflight precedes. This one is built from payloads
/// held in memory, which is the same shape with the I/O removed: it declares
/// surfaces, pages them with a keyset cursor, and says plainly when it cannot
/// read one.
pub struct DeclaredPreflight {
    location: String,
    databases: Vec<StoreSchemaDatabase>,
    surfaces: BTreeMap<DurableSurface, Vec<DurableItem>>,
    unreadable: BTreeMap<DurableSurface, String>,
}

impl DeclaredPreflight {
    /// A handle over a deployment at `location` with nothing declared yet.
    pub fn at(location: impl Into<String>) -> Self {
        Self {
            location: location.into(),
            databases: Vec::new(),
            surfaces: BTreeMap::new(),
            unreadable: BTreeMap::new(),
        }
    }

    /// Declare one schema-carrying database and what it said.
    pub fn with_database(mut self, name: &str, expected: i64, verdict: StoreSchemaVerdict) -> Self {
        self.databases.push(StoreSchemaDatabase {
            name: name.to_string(),
            location: format!("{}/{name}.db", self.location),
            expected,
            verdict,
        });
        self
    }

    /// Declare the items one surface holds.
    pub fn with_items(mut self, surface: DurableSurface, items: Vec<DurableItem>) -> Self {
        self.surfaces.insert(surface, items);
        self
    }

    /// Declare a surface this deployment cannot read, and why.
    ///
    /// Distinct from a surface with no items: an empty surface says nothing
    /// refuses, an unreadable one says nobody looked, and a handle that
    /// returned an empty page for the second would let a host boot on evidence
    /// that does not exist.
    pub fn unreadable(mut self, surface: DurableSurface, reason: &str) -> Self {
        self.unreadable.insert(surface, reason.to_string());
        self
    }
}

#[async_trait]
impl StorePreflight for DeclaredPreflight {
    fn backend(&self) -> StoreBackend {
        StoreBackend::Sqlite {
            location: self.location.clone(),
        }
    }

    async fn schema_status(&self) -> Result<StoreSchemaStatus, StoreError> {
        Ok(StoreSchemaStatus {
            databases: self.databases.clone(),
        })
    }

    async fn scan_durable(&self, scan: &DurableScan) -> Result<DurableScanPage, StoreError> {
        // docs:start:scan-surface
        if let Some(reason) = self.unreadable.get(&scan.surface) {
            return Ok(DurableScanPage {
                items: Vec::new(),
                next: None,
                coverage: ScanCoverage::NotScanned {
                    reason: reason.clone(),
                },
            });
        }
        let all = self
            .surfaces
            .get(&scan.surface)
            .cloned()
            .unwrap_or_default();
        // Keyset paging, not offset paging: a cursor names the last item read,
        // so concurrent writes cannot make a page skip or repeat rows.
        let start = match &scan.after {
            None => 0,
            Some(cursor) => all
                .iter()
                .position(|item| &item.cursor == cursor)
                .map(|index| index + 1)
                .unwrap_or(all.len()),
        };
        let items: Vec<DurableItem> = all.iter().skip(start).take(scan.limit).cloned().collect();
        let next = if start + items.len() >= all.len() {
            None
        } else {
            items.last().map(|item| item.cursor.clone())
        };
        Ok(DurableScanPage {
            items,
            next,
            coverage: ScanCoverage::Scanned,
        })
        // docs:end:scan-surface
    }
}

/// Whether reading a surface costs a per-session blob walk, which is what makes
/// summary mode's split principled rather than arbitrary.
pub fn deep_surfaces() -> Vec<&'static str> {
    [
        DurableSurface::ModuleArtifact,
        DurableSurface::ParkedSegment,
        DurableSurface::PendingWake,
        DurableSurface::SessionCheckpoint,
        DurableSurface::SessionExecutionState,
    ]
    .into_iter()
    .filter(|surface| surface.is_deep())
    .map(DurableSurface::name)
    .collect()
}

/// A parked Lashlang segment, as the walk hands it to the probe.
///
/// The continuation is a byte sequence inside the envelope rather than nested
/// JSON, which keeps the outer handover engine-agnostic. A probe un-nests it
/// once; it never decodes it.
pub fn parked_segment_item(process: &str, session: &str, segment_version: u32) -> DurableItem {
    let engine_state = serde_json::to_vec(&serde_json::json!({
        "version": segment_version,
        "vm": {"format_version": lash::formats::VM_CONTINUATION_FORMAT_VERSION},
    }))
    .expect("the continuation encodes");
    DurableItem {
        surface: DurableSurface::ParkedSegment,
        cursor: format!("{process}:00000000000000000000"),
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

/// A pending wake delivery, whose version sits in plain JSON.
pub fn pending_wake_item(delivery: &str, process: &str, version: u32) -> DurableItem {
    DurableItem {
        surface: DurableSurface::PendingWake,
        cursor: delivery.to_string(),
        process_id: Some(process.to_string()),
        session_id: Some("chat-1".to_string()),
        status: None,
        owner_record: None,
        payload: DurablePayload::Json(
            serde_json::json!({"wake_id": delivery, "version": version}).to_string(),
        ),
    }
}

/// A session checkpoint whose root blob has gone missing.
///
/// Reported rather than skipped and rather than fatal: a walk that aborted on
/// the first dangling reference could not describe the thousand items behind
/// it, and a walk that skipped it would report a store as clean because part of
/// it was unreadable.
pub fn missing_checkpoint_item(session: &str, blob_ref: &str) -> DurableItem {
    DurableItem {
        surface: DurableSurface::SessionCheckpoint,
        cursor: session.to_string(),
        process_id: None,
        session_id: Some(session.to_string()),
        status: None,
        owner_record: None,
        payload: DurablePayload::Missing {
            reason: format!("blob {blob_ref} is absent"),
        },
    }
}

/// A session checkpoint root, stored as MessagePack.
pub fn checkpoint_item(session: &str, schema_version: u32, encoding_version: u32) -> DurableItem {
    let root = rmp_serde::to_vec_named(&serde_json::json!({
        "schema_version": schema_version,
        "components": {
            "execution_state": {"blob_ref": "sha256:a", "encoding_version": encoding_version},
        },
    }))
    .expect("the checkpoint root encodes");
    DurableItem {
        surface: DurableSurface::SessionCheckpoint,
        cursor: session.to_string(),
        process_id: None,
        session_id: Some(session.to_string()),
        status: None,
        owner_record: None,
        payload: DurablePayload::MessagePack(root),
    }
}

/// The first page of a surface, and the page after a cursor — the two requests
/// a paged walk ever makes.
pub fn scan_requests(surface: DurableSurface, cursor: &str) -> (DurableScan, DurableScan) {
    (
        DurableScan::first(surface, DEFAULT_PAGE_SIZE),
        DurableScan::after(surface, cursor, DEFAULT_PAGE_SIZE),
    )
}

/// How a page describes itself: what it holds, where to resume, and whether it
/// was read at all.
pub fn page_summary(page: &DurableScanPage) -> String {
    let coverage = match &page.coverage {
        ScanCoverage::Scanned => "scanned".to_string(),
        ScanCoverage::NotScanned { reason } => format!("not scanned ({reason})"),
        _ => "unclassified".to_string(),
    };
    format!(
        "{} item(s), {} — {coverage}",
        page.items.len(),
        page.next
            .as_deref()
            .map(|next| format!("resume after {next}"))
            .unwrap_or_else(|| "exhausted".to_string())
    )
}

/// What one walked item says about itself before any format logic runs.
pub fn item_summary(item: &DurableItem) -> String {
    let payload = match &item.payload {
        DurablePayload::Json(text) => format!("{} bytes of JSON", text.len()),
        DurablePayload::MessagePack(bytes) => format!("{} bytes of MessagePack", bytes.len()),
        DurablePayload::Missing { reason } => format!("unreadable: {reason}"),
        _ => "an unrecognised framing".to_string(),
    };
    format!(
        "{} {}/{} [{}] {} (record: {})",
        item.surface.name(),
        item.process_id.as_deref().unwrap_or("-"),
        item.session_id.as_deref().unwrap_or("-"),
        item.status.as_deref().unwrap_or("-"),
        payload,
        if item.owner_record.is_some() {
            "present"
        } else {
            "absent"
        }
    )
}

#[cfg(test)]
mod tests {
    use lash::formats::{
        CHECKPOINT_COMPONENT_ENCODING_VERSION, LASHLANG_SEGMENT_STATE_VERSION,
        PROCESS_WAKE_DELIVERY_FORMAT_VERSION, SESSION_CHECKPOINT_SCHEMA_VERSION,
    };

    use super::*;

    fn healthy() -> DeclaredPreflight {
        DeclaredPreflight::at("/srv/lash")
            .with_database("durable core", 37, StoreSchemaVerdict::Matches)
            .with_items(
                DurableSurface::ParkedSegment,
                vec![parked_segment_item(
                    "proc-1",
                    "chat-1",
                    LASHLANG_SEGMENT_STATE_VERSION,
                )],
            )
            .with_items(
                DurableSurface::PendingWake,
                vec![pending_wake_item(
                    "wake-1",
                    "proc-1",
                    PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
                )],
            )
            .with_items(
                DurableSurface::SessionCheckpoint,
                vec![checkpoint_item(
                    "chat-1",
                    SESSION_CHECKPOINT_SCHEMA_VERSION,
                    CHECKPOINT_COMPONENT_ENCODING_VERSION,
                )],
            )
    }

    fn stale() -> DeclaredPreflight {
        DeclaredPreflight::at("/srv/lash")
            .with_database("durable core", 37, StoreSchemaVerdict::Matches)
            .with_items(
                DurableSurface::ParkedSegment,
                vec![
                    parked_segment_item("proc-1", "chat-1", LASHLANG_SEGMENT_STATE_VERSION - 1),
                    parked_segment_item("proc-2", "chat-2", LASHLANG_SEGMENT_STATE_VERSION),
                ],
            )
    }

    fn row<'a>(report: &'a PreflightReport, format: &str) -> &'a ComponentReadability {
        report
            .components
            .iter()
            .find(|component| component.format == format)
            .unwrap_or_else(|| panic!("the report has a row for {format}"))
    }

    #[tokio::test]
    async fn a_store_this_build_wrote_lets_the_host_start() {
        let ready = boot_gate(&healthy()).await.expect("nothing refuses");
        assert_eq!(ready, "store preflight: ready (summary mode)");
    }

    #[tokio::test]
    async fn a_stale_segment_stops_the_boot_with_one_actionable_sentence() {
        // The crash loop replaced: a permanent refusal, named once, with the
        // remedy in the same string a supervisor logs.
        let refusal = boot_gate(&stale())
            .await
            .expect_err("a stale segment refuses");
        assert!(refusal.contains("Lashlang segment handover"), "{refusal}");
        assert!(refusal.contains("Drain 1 affected item(s)"), "{refusal}");
        assert!(refusal.contains("no migration decoder"), "{refusal}");
    }

    #[tokio::test]
    async fn the_refusing_format_is_named_counted_and_attributed() {
        let report = probe_store(&stale(), PreflightOptions::summary())
            .await
            .expect("the probe reads the store");

        assert_eq!(report.outcome, PreflightOutcome::Refused);
        assert_eq!(
            outcome_label(report.outcome),
            "will not open; drain or recreate"
        );
        assert_eq!(report.mode, PreflightMode::Summary);
        assert_eq!(mode_label(report.mode), "schema and process registry");
        assert_eq!(refusing_formats(&report), vec!["Lashlang segment handover"]);

        let handover = row(&report, "Lashlang segment handover");
        assert!(row_refuses(handover));
        assert_eq!(handover.verdict, ComponentVerdict::Refused);
        assert_eq!(
            verdict_label(handover.verdict),
            "at least one stored item refuses"
        );
        assert_eq!(handover.verdict.name(), "refused");
        assert_eq!(handover.probe, "comparable");
        assert_eq!(handover.evidence, FormatEvidence::Direct);
        assert_eq!(evidence_label(handover.evidence), "read from stored bytes");
        let writes = LASHLANG_SEGMENT_STATE_VERSION.to_string();
        assert_eq!(handover.expected, writes);
        assert_eq!(handover.scanned, 2);
        assert_eq!(
            handover.found,
            vec![
                FoundVersion {
                    version: LASHLANG_SEGMENT_STATE_VERSION - 1,
                    count: 1,
                },
                FoundVersion {
                    version: LASHLANG_SEGMENT_STATE_VERSION,
                    count: 1,
                },
            ],
            "the readable items are counted too, not only the refusing one"
        );
        assert_eq!(handover.undecodable, 0);
        assert_eq!(handover.refused_without_version, 0);
        assert_eq!(first_undecodable_reason(handover), None);
    }

    #[tokio::test]
    async fn the_drain_list_names_the_process_a_deploy_has_to_drain() {
        let report = probe_store(&stale(), PreflightOptions::summary())
            .await
            .expect("the probe reads the store");
        let worklist = drain_worklist(&report);
        assert_eq!(worklist.len(), 1, "{worklist:?}");
        assert!(
            worklist[0].starts_with("process proc-1 (session chat-1)"),
            "{worklist:?}"
        );
        assert!(worklist[0].contains("[waiting]"), "{worklist:?}");

        let DrainBlocker {
            process_id,
            session_id,
            status,
            format,
            expected,
            found,
            detail,
        } = &report.drain[0];
        assert_eq!(process_id.as_deref(), Some("proc-1"));
        assert_eq!(session_id.as_deref(), Some("chat-1"));
        assert_eq!(status.as_deref(), Some("waiting"));
        assert_eq!(format, "Lashlang segment handover");
        assert_eq!(expected, &LASHLANG_SEGMENT_STATE_VERSION.to_string());
        assert_eq!(found, &Some(LASHLANG_SEGMENT_STATE_VERSION - 1));
        assert!(detail.contains("proc-1"), "{detail}");
    }

    #[tokio::test]
    async fn a_schema_refusal_names_both_versions_in_the_report() {
        let store = DeclaredPreflight::at("/srv/lash").with_database(
            "durable core",
            37,
            StoreSchemaVerdict::Mismatch { found: 36 },
        );
        let report = probe_store(&store, PreflightOptions::summary())
            .await
            .expect("the probe reads the store");
        assert_eq!(report.outcome, PreflightOutcome::Refused);

        let SchemaReport { outcome, databases } = &report.schema;
        assert_eq!(*outcome, "refused");
        let SchemaDatabaseReport {
            name,
            location,
            expected,
            verdict,
            found,
            reason,
        } = &databases[0];
        assert_eq!(name, "durable core");
        assert_eq!(location, "/srv/lash/durable core.db");
        assert_eq!(*expected, 37);
        assert_eq!(*verdict, "mismatch");
        assert_eq!(*found, Some(36));
        assert_eq!(*reason, None);

        let lines = schema_lines(&report.schema);
        assert_eq!(lines[0], "schema: refused");
        assert!(lines[1].contains("found 36"), "{lines:?}");
        assert_eq!(schema_database_line(&databases[0]), lines[1]);
    }

    #[tokio::test]
    async fn an_unreadable_database_is_undecided_rather_than_a_refusal() {
        let store = DeclaredPreflight::at("/srv/lash").with_database(
            "effect replay",
            11,
            StoreSchemaVerdict::Unreadable {
                reason: "file is not a database".to_string(),
            },
        );
        let report = probe_store(&store, PreflightOptions::summary())
            .await
            .expect("the probe reads the store");
        assert_eq!(report.outcome, PreflightOutcome::Undecided);
        assert_eq!(outcome_label(report.outcome), "investigate before starting");
        assert_eq!(
            report.schema.databases[0].reason.as_deref(),
            Some("file is not a database")
        );
        assert_eq!(
            report.refusal_message(),
            None,
            "an undecided store is not a permanent failure to exit on"
        );
    }

    #[tokio::test]
    async fn a_payload_that_cannot_be_fetched_is_reported_rather_than_skipped() {
        let store = DeclaredPreflight::at("/srv/lash").with_items(
            DurableSurface::SessionCheckpoint,
            vec![missing_checkpoint_item("chat-9", "sha256:deadbeef")],
        );
        let report = pre_bump_audit(&store)
            .await
            .expect("the probe reads the store");
        let manifest = row(&report, "session checkpoint manifest");
        assert_eq!(manifest.verdict, ComponentVerdict::Undecodable);
        assert_eq!(
            verdict_label(manifest.verdict),
            "stored items could not be read"
        );
        assert_eq!(manifest.undecodable, 1);
        assert!(
            first_undecodable_reason(manifest)
                .is_some_and(|reason| reason.contains("sha256:deadbeef")),
            "{:?}",
            manifest.undecodable_reasons
        );
        assert_eq!(report.outcome, PreflightOutcome::Undecided);
    }

    #[tokio::test]
    async fn summary_mode_names_the_walk_it_skipped_and_deep_mode_does_it() {
        let summary = probe_store(&healthy(), PreflightOptions::summary())
            .await
            .expect("the probe reads the store");
        let gaps = coverage_gaps(&summary);
        assert!(
            gaps.iter()
                .any(|gap| gap.starts_with("session checkpoints:")),
            "{gaps:?}"
        );
        assert_eq!(
            row(&summary, "session checkpoint manifest").verdict,
            ComponentVerdict::NotScanned,
            "a skipped surface must never read as an empty one"
        );
        assert_eq!(verdict_label(ComponentVerdict::NotScanned), "nobody looked");

        let deep = pre_bump_audit(&healthy())
            .await
            .expect("the probe reads the store");
        assert_eq!(deep.mode, PreflightMode::Deep);
        assert_eq!(
            mode_label(deep.mode),
            "schema, process registry and every session"
        );
        assert_eq!(
            row(&deep, "session checkpoint manifest").verdict,
            ComponentVerdict::AllReadable
        );
        assert_eq!(
            row(&deep, "checkpoint component encoding").scanned,
            1,
            "one blob read decides both formats"
        );
    }

    #[tokio::test]
    async fn every_report_names_the_formats_no_bounded_walk_enumerates() {
        let report = pre_bump_audit(&healthy())
            .await
            .expect("the probe reads the store");
        let gaps = coverage_gaps(&report);
        assert!(
            gaps.iter().any(|gap| gap.starts_with("session head meta:")),
            "{gaps:?}"
        );
        assert!(
            gaps.iter().any(|gap| gap.starts_with("session node body:")),
            "{gaps:?}"
        );
    }

    #[tokio::test]
    async fn a_surface_the_deployment_cannot_read_is_reported_verbatim() {
        let store = healthy().unreadable(
            DurableSurface::ParkedSegment,
            "this deployment declared no process registry",
        );
        let report = probe_store(&store, PreflightOptions::summary())
            .await
            .expect("the probe reads the store");
        assert!(
            coverage_gaps(&report)
                .iter()
                .any(|gap| gap.ends_with("this deployment declared no process registry")),
            "{:?}",
            report.not_scanned
        );
        assert_eq!(
            row(&report, "Lashlang segment handover").verdict,
            ComponentVerdict::NotScanned
        );
    }

    #[tokio::test]
    async fn a_carried_format_reports_the_envelope_that_decides_it() {
        let report = pre_bump_audit(&healthy())
            .await
            .expect("the probe reads the store");
        let schedule = row(&report, "heap size schedule");
        assert_eq!(
            schedule.evidence,
            FormatEvidence::CarriedBy("VM continuation")
        );
        assert_eq!(
            evidence_label(schedule.evidence),
            "carried by VM continuation"
        );

        let abi = row(&report, "Lashlang VM ABI");
        assert_eq!(abi.evidence, FormatEvidence::NotPersisted);
        assert_eq!(evidence_label(abi.evidence), "never persisted");
        assert_eq!(abi.probe, "not persisted");
        assert_eq!(verdict_label(abi.verdict), "nothing stored");
        assert_eq!(abi.verdict, ComponentVerdict::Empty);
    }

    #[tokio::test]
    async fn paging_reads_every_item_exactly_once() {
        let items: Vec<DurableItem> = (0..5)
            .map(|index| {
                parked_segment_item(
                    &format!("proc-{index}"),
                    "chat-1",
                    LASHLANG_SEGMENT_STATE_VERSION - 1,
                )
            })
            .collect();
        let store =
            DeclaredPreflight::at("/srv/lash").with_items(DurableSurface::ParkedSegment, items);
        let report = probe_store(&store, PreflightOptions::summary().with_page_size(2))
            .await
            .expect("the probe reads the store");
        assert_eq!(row(&report, "Lashlang segment handover").scanned, 5);
        assert_eq!(report.drain.len(), 5, "a paged walk drops nothing");
    }

    #[tokio::test]
    async fn a_page_describes_what_it_holds_and_where_to_resume() {
        let store = healthy();
        let (first, _) = scan_requests(DurableSurface::ParkedSegment, "proc-1:0");
        assert_eq!(first.surface, DurableSurface::ParkedSegment);
        assert_eq!(first.after, None);
        assert_eq!(first.limit, DEFAULT_PAGE_SIZE);

        let page = store.scan_durable(&first).await.expect("the surface reads");
        assert_eq!(page_summary(&page), "1 item(s), exhausted — scanned");
        assert!(matches!(page.coverage, ScanCoverage::Scanned));
        assert_eq!(page.next, None);
        assert_eq!(page.items.len(), 1);
        assert!(
            item_summary(&page.items[0]).starts_with("parked segments proc-1/chat-1 [waiting]"),
            "{}",
            item_summary(&page.items[0])
        );
        assert!(item_summary(&page.items[0]).contains("JSON"));

        let (_, resumed) = scan_requests(DurableSurface::ParkedSegment, "proc-1:0");
        assert_eq!(resumed.after.as_deref(), Some("proc-1:0"));
    }

    #[tokio::test]
    async fn an_unreadable_surface_reports_itself_rather_than_returning_an_empty_page() {
        let store = healthy().unreadable(DurableSurface::PendingWake, "the registry is offline");
        let page = store
            .scan_durable(&DurableScan::first(DurableSurface::PendingWake, 10))
            .await
            .expect("the surface answers");
        assert_eq!(
            page_summary(&page),
            "0 item(s), exhausted — not scanned (the registry is offline)"
        );
    }

    #[test]
    fn only_the_per_session_surfaces_cost_a_blob_walk() {
        // What makes the summary/deep split principled: the deep surfaces are
        // bounded by session count and cost at least one blob read each.
        assert_eq!(
            deep_surfaces(),
            vec![
                DurableSurface::SessionCheckpoint.name(),
                DurableSurface::SessionExecutionState.name(),
            ]
        );
        assert_eq!(DurableSurface::ModuleArtifact.name(), "module artifacts");
        assert!(!DurableSurface::ModuleArtifact.is_deep());
        assert!(!DurableSurface::ParkedSegment.is_deep());
        assert_eq!(DurableSurface::PendingWake.name(), "pending wakes");
    }

    #[tokio::test]
    async fn the_rendered_report_carries_the_schema_rows_drain_and_gaps_together() {
        let report = probe_store(&stale(), PreflightOptions::summary())
            .await
            .expect("the probe reads the store");
        let rendered = render(&report);
        assert!(rendered.starts_with("sqlite (/srv/lash):"), "{rendered}");
        assert!(
            rendered.contains("will not open; drain or recreate"),
            "{rendered}"
        );
        assert_eq!(
            options_note(&PreflightOptions::deep().with_page_size(64)),
            "deep walk, 64 items per page"
        );
        assert_eq!(
            options_note(&PreflightOptions::summary()),
            format!("summary walk, {DEFAULT_PAGE_SIZE} items per page")
        );
        let lash_rendering = rendered_by_lash(&report);
        assert!(lash_rendering.contains("refused"), "{lash_rendering}");
        assert!(
            lash_rendering.contains("Lashlang segment handover"),
            "{lash_rendering}"
        );
        assert!(
            rendered.contains("Lashlang segment handover:"),
            "{rendered}"
        );
        assert!(rendered.contains("scanned=2"), "{rendered}");
        assert!(
            rendered.contains("process proc-1 (session chat-1)"),
            "{rendered}"
        );
        assert!(rendered.contains("session checkpoints:"), "{rendered}");
        assert_eq!(
            component_lines(&report).len(),
            report.components.len(),
            "one line per format, including the ones with nothing stored"
        );
    }

    #[tokio::test]
    async fn the_report_serializes_the_fields_a_deploy_gate_asserts_on() {
        let report = probe_store(&stale(), PreflightOptions::summary())
            .await
            .expect("the probe reads the store");
        let json = serde_json::to_value(&report).expect("the report serializes");
        assert_eq!(json["outcome"], "refused");
        assert_eq!(json["mode"], "summary");
        assert_eq!(json["backend"], "sqlite (/srv/lash)");
        assert_eq!(json["drain"][0]["process_id"], "proc-1");
        assert_eq!(json["schema"]["outcome"], "ready");
    }
}
