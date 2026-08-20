//! Ask a store whether its durable data opens under this build — before wiring
//! anything.
//!
//! Lash's durable formats fail closed (ADR 0055/0061/0064). There is no
//! migration decoder at any of these boundaries, so state parked by another
//! build is refused rather than read. That policy is right, and the refusals
//! already exist and already fire correctly. What did not exist was a way to
//! ask the question first: the only path to the answer was to boot, wire the
//! runtime, and let the first store access produce the refusal — which under a
//! supervisor or `entr` is a crash loop rather than a diagnosis, and which bit
//! a real deployment on a prior schema bump.
//!
//! [`probe_store`] answers first, and answers in three parts, because a version
//! boundary needs three different things said about it:
//!
//! 1. **The schema stamps**, from
//!    [`StorePreflight::schema_status`]
//!    — the boundary a host hits first, before a single durable payload is read.
//! 2. **Readability per component**: for each durable format this build writes,
//!    the version it expects, every version actually found with counts, and an
//!    undecodable bucket. The bucket is what lets the probe promise never to
//!    panic on the data it is warning about.
//! 3. **A drain list**: the same walk, keeping identities, so "drain before you
//!    deploy" becomes a list of processes and sessions an operator can work
//!    through rather than an instruction they have to take on faith.
//!
//! # One walk, two granularities
//!
//! The readability answer and the drain list are not two passes. Every item the
//! walk reads contributes to both — the counts and the identities are the same
//! evidence summarised two ways — because two passes over a live store can
//! disagree, and a report whose halves disagree is worse than either half.
//!
//! # What it will not do
//!
//! It reads. It never marks, migrates, condemns or deletes; disposal stays with
//! the operator and the published drain/recreate procedures, and reclaiming
//! space belongs to garbage collection rather than here. It is built on the
//! read-only handles a backend constructs from raw connection configuration,
//! never on a wired store, because constructing a store is itself the
//! side-effectful act this exists to precede.
//!
//! # What it did not read
//!
//! Every report carries [`PreflightReport::not_scanned`], including a clean one.
//! [`PreflightMode::Summary`] deliberately skips the per-session blob walk, and
//! two of the durable formats have no surface a bounded walk can enumerate at
//! all. A preflight's worst failure mode is a silent cap — a walk that stopped
//! early and published what it managed to see as if it were everything — so the
//! list is part of the answer rather than a footnote to it.

mod extract;
mod msgpack;
mod report;

use std::collections::BTreeMap;

use lash_core::{
    DurableItem, DurableScan, DurableSurface, ScanCoverage, StoreError, StorePreflight,
    StoreSchemaOutcome, StoreSchemaStatus, StoreSchemaVerdict,
};

use crate::formats::{DurableFormat, FormatProbe, durable_formats};
use extract::{Extraction, extract, primary_format};
use report::{FormatTally, reads_version};

pub use report::{
    ComponentReadability, ComponentVerdict, DrainBlocker, FormatEvidence, FoundVersion, NotScanned,
    PreflightMode, PreflightOutcome, PreflightReport, SchemaDatabaseReport, SchemaReport,
};

/// How many items one page of a surface walk asks for.
///
/// A page bounds memory and query cost without bounding the walk: the probe
/// keeps paging until a surface is exhausted. The number is a round trip
/// trade-off rather than a limit on what gets reported, which is the difference
/// between paging and capping.
pub const DEFAULT_PAGE_SIZE: usize = 256;

/// How a probe is asked to read a store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PreflightOptions {
    /// How much of the store to read.
    pub mode: PreflightMode,
    /// How many items to request per page.
    pub page_size: usize,
}

impl PreflightOptions {
    /// The mode a host runs on every boot: schema stamps plus the
    /// process-registry surfaces, bounded by the process count rather than by
    /// the session count.
    pub fn summary() -> Self {
        Self {
            mode: PreflightMode::Summary,
            page_size: DEFAULT_PAGE_SIZE,
        }
    }

    /// The mode an operator runs before a version bump: everything, including
    /// the per-session checkpoint walk, paged.
    pub fn deep() -> Self {
        Self {
            mode: PreflightMode::Deep,
            page_size: DEFAULT_PAGE_SIZE,
        }
    }

    /// Request a different page size.
    pub fn with_page_size(mut self, page_size: usize) -> Self {
        // A zero page would ask for nothing forever; one item per round trip is
        // the smallest request that still makes progress.
        self.page_size = page_size.max(1);
        self
    }
}

impl Default for PreflightOptions {
    fn default() -> Self {
        Self::summary()
    }
}

/// Read a store's schema stamps and durable payloads, and report whether they
/// open under this build.
///
/// Strictly read-only. The `Result` is for failures that leave nothing to
/// report at all — an unreachable server — because everything a probe can
/// describe belongs in the report rather than in an error: a refusal, an
/// unreadable database, a payload nobody can decode and a surface nobody walked
/// are four different findings, and an error type collapses them into one.
pub async fn probe_store(
    handle: &dyn StorePreflight,
    options: PreflightOptions,
) -> Result<PreflightReport, StoreError> {
    let schema = handle.schema_status().await?;
    let mut walk = Walk::default();
    for surface in [
        DurableSurface::ModuleArtifact,
        DurableSurface::ParkedSegment,
        DurableSurface::PendingWake,
        DurableSurface::SessionCheckpoint,
        DurableSurface::SessionExecutionState,
    ] {
        if surface.is_deep() && options.mode == PreflightMode::Summary {
            walk.not_scanned.push(NotScanned {
                what: surface.name().to_string(),
                reason: "summary mode skips the per-session blob walk; run a deep probe to read it"
                    .to_string(),
            });
            continue;
        }
        walk.surface(handle, surface, options.page_size).await?;
    }
    Ok(walk.finish(handle, &schema, options.mode))
}

/// The accumulated evidence of one walk.
#[derive(Default)]
struct Walk {
    tallies: BTreeMap<DurableFormat, FormatTally>,
    drain: Vec<DrainBlocker>,
    not_scanned: Vec<NotScanned>,
}

impl Walk {
    /// Page through one surface to exhaustion, folding every item into both the
    /// per-format tallies and the drain list.
    async fn surface(
        &mut self,
        handle: &dyn StorePreflight,
        surface: DurableSurface,
        page_size: usize,
    ) -> Result<(), StoreError> {
        let mut cursor: Option<String> = None;
        loop {
            let scan = DurableScan {
                surface,
                after: cursor.clone(),
                limit: page_size,
            };
            let page = handle.scan_durable(&scan).await?;
            if let ScanCoverage::NotScanned { reason } = &page.coverage {
                self.not_scanned.push(NotScanned {
                    what: surface.name().to_string(),
                    reason: reason.clone(),
                });
                return Ok(());
            }
            for item in &page.items {
                self.item(item);
            }
            match page.next {
                // A backend that returned the cursor it was handed would page
                // forever; refusing to follow a cursor that did not advance
                // turns a backend bug into a truncated report rather than a
                // hung boot.
                //
                // The test is cursor equality, deliberately not an empty page.
                // Backends mint this cursor from the last row they *read*, not
                // the last item they emitted, so a page of sessions that happen
                // to hold no execution state contributes nothing and still
                // advances. Treating that ordinary shape as a backend bug would
                // abandon the surface and report the formats behind it as
                // unscanned.
                Some(next) if cursor.as_deref() == Some(next.as_str()) => {
                    self.not_scanned.push(NotScanned {
                        what: surface.name().to_string(),
                        reason: format!(
                            "the backend offered `{next}` again as the next page cursor; the walk \
                             stopped rather than paging forever"
                        ),
                    });
                    return Ok(());
                }
                Some(next) => cursor = Some(next),
                None => return Ok(()),
            }
        }
    }

    fn item(&mut self, item: &DurableItem) {
        for extraction in extract(item) {
            match extraction {
                Extraction::Found { format, version } => {
                    let expected =
                        crate::formats::durable_format(format).map(|entry| entry.version);
                    self.tallies.entry(format).or_default().record(version);
                    if let Some(expected) = expected
                        && !reads_version(expected, version)
                    {
                        self.drain.push(DrainBlocker {
                            process_id: item.process_id.clone(),
                            session_id: item.session_id.clone(),
                            status: item.status.clone(),
                            format: format.name().to_string(),
                            expected: expected.to_string(),
                            found: Some(version),
                            detail: format!(
                                "{} carries {} version {version}, and this build writes {expected}",
                                owner(item),
                                format.name()
                            ),
                        });
                    }
                }
                Extraction::Undecodable { format, reason } => {
                    self.tallies.entry(format).or_default().undecodable(reason);
                }
                #[cfg(feature = "rlm")]
                Extraction::IdentityMismatch { format, detail } => {
                    self.tallies
                        .entry(format)
                        .or_default()
                        .refuse_without_version();
                    self.drain.push(DrainBlocker {
                        process_id: item.process_id.clone(),
                        session_id: item.session_id.clone(),
                        status: item.status.clone(),
                        format: format.name().to_string(),
                        expected: crate::formats::durable_format(format)
                            .map(|entry| entry.version.to_string())
                            .unwrap_or_else(|| "not in this build".to_string()),
                        found: None,
                        detail: format!("{}: {detail}", owner(item)),
                    });
                }
                #[cfg(feature = "rlm")]
                Extraction::IdentityMatch { format } => {
                    self.tallies.entry(format).or_default().identity_match();
                }
            }
        }
    }

    fn finish(
        mut self,
        handle: &dyn StorePreflight,
        schema: &StoreSchemaStatus,
        mode: PreflightMode,
    ) -> PreflightReport {
        let components = self.components();
        let schema_report = schema_report(schema);
        let refused = schema.outcome() == StoreSchemaOutcome::Refused
            || components.iter().any(ComponentReadability::refuses_open);
        let undecided = schema.outcome() == StoreSchemaOutcome::Undecided
            || components
                .iter()
                .any(|component| component.verdict == ComponentVerdict::Undecodable);
        let outcome = if refused {
            PreflightOutcome::Refused
        } else if undecided {
            PreflightOutcome::Undecided
        } else {
            PreflightOutcome::Ready
        };
        self.not_scanned
            .sort_by(|left, right| (&left.what, &left.reason).cmp(&(&right.what, &right.reason)));
        self.not_scanned.dedup();
        PreflightReport {
            backend: handle.backend().to_string(),
            mode,
            outcome,
            schema: schema_report,
            components,
            drain: self.drain,
            not_scanned: self.not_scanned,
        }
    }

    /// One row per manifest entry, in manifest order.
    ///
    /// Driven by the manifest rather than by what the walk happened to find, so
    /// a format this build writes but this deployment has none of appears as
    /// `empty` rather than vanishing from the report. A row that silently
    /// disappears is indistinguishable from a format nobody thought to check.
    fn components(&mut self) -> Vec<ComponentReadability> {
        let mut rows: Vec<ComponentReadability> = Vec::new();
        for entry in durable_formats() {
            let evidence = evidence_for(entry.format, entry.probe);
            let tally = self.tallies.remove(&entry.format).unwrap_or_default();
            let mut row = tally.into_row(entry.format, entry.version, entry.probe, evidence);
            if matches!(evidence, FormatEvidence::NotPersisted) {
                // Nothing durable to find, so "empty" is the whole truth: there
                // is no store this could have been read from.
                row.verdict = ComponentVerdict::Empty;
            } else if row.verdict == ComponentVerdict::Empty && !self.walked(entry.format) {
                row.verdict = ComponentVerdict::NotScanned;
            }
            rows.push(row);
        }
        // Carried formats resolve in a second pass, because a carrier can
        // appear after the format it carries in manifest order and a
        // single-pass lookup would silently report the earlier row as unscanned.
        let verdicts: BTreeMap<String, ComponentVerdict> = rows
            .iter()
            .map(|row| (row.format.clone(), row.verdict))
            .collect();
        for (row, entry) in rows.iter_mut().zip(durable_formats()) {
            if let Some(carrier) = carrier_of(entry.format) {
                // A carried format has no evidence of its own by construction:
                // its verdict is the carrier's, because the carrier's version
                // moves whenever the carried format changes.
                row.verdict = verdicts
                    .get(carrier.name())
                    .copied()
                    .unwrap_or(ComponentVerdict::NotScanned);
            }
        }
        for (what, reason) in unwalkable_formats() {
            self.not_scanned.push(NotScanned {
                what: what.to_string(),
                reason: reason.to_string(),
            });
        }
        rows
    }

    /// Whether any surface carrying this format was actually walked.
    ///
    /// The distinction the report exists to preserve: a format with zero found
    /// items is `empty` when somebody looked and `not scanned` when nobody did,
    /// and only the first of those means a drain is unnecessary.
    fn walked(&self, format: DurableFormat) -> bool {
        let unwalked = |surface: DurableSurface| {
            self.not_scanned
                .iter()
                .any(|skipped| skipped.what == surface.name())
        };
        match format {
            DurableFormat::ModuleArtifact => !unwalked(DurableSurface::ModuleArtifact),
            DurableFormat::LashlangSegmentHandover
            | DurableFormat::VmContinuation
            | DurableFormat::Bytecode => !unwalked(DurableSurface::ParkedSegment),
            DurableFormat::ProcessWakeDelivery => !unwalked(DurableSurface::PendingWake),
            DurableFormat::SessionCheckpointManifest
            | DurableFormat::CheckpointComponentEncoding => {
                !unwalked(DurableSurface::SessionCheckpoint)
            }
            DurableFormat::RlmSnapshotEnvelope => !unwalked(DurableSurface::SessionExecutionState),
            _ => false,
        }
    }
}

/// Which format's envelope enforces a carried format's boundary.
///
/// Both relationships are stated by the constants themselves: the segment
/// handover documents that it embeds the VM continuation and Lashlang snapshot
/// formats its build carries, and the RLM snapshot envelope's history records
/// which Lashlang snapshot version each of its versions carries. A probe that
/// invented an independent check for them would be reporting a boundary that
/// does not exist.
fn carrier_of(format: DurableFormat) -> Option<DurableFormat> {
    match format {
        DurableFormat::LashlangSnapshot => Some(DurableFormat::RlmSnapshotEnvelope),
        DurableFormat::HeapSizeSchedule => Some(DurableFormat::VmContinuation),
        _ => None,
    }
}

fn evidence_for(format: DurableFormat, probe: FormatProbe) -> FormatEvidence {
    if probe == FormatProbe::NotPersisted {
        return FormatEvidence::NotPersisted;
    }
    match carrier_of(format) {
        Some(carrier) => FormatEvidence::CarriedBy(carrier.name()),
        None => FormatEvidence::Direct,
    }
}

/// The formats no bounded walk enumerates, and why.
///
/// Both are readable in principle — their versions sit in plain JSON columns —
/// and neither has a surface a preflight can walk without reading the two
/// highest-cardinality tables in the store. Naming them is the honest answer;
/// walking them on every boot would make the preflight the outage it exists to
/// prevent.
fn unwalkable_formats() -> [(&'static str, &'static str); 2] {
    [
        (
            DurableFormat::SessionHeadMeta.name(),
            "no bounded surface: one row per session, refused at open rather than at rest",
        ),
        (
            DurableFormat::SessionNodeBody.name(),
            "no bounded surface: one row per graph node, and the boundary is forward-only",
        ),
    ]
}

/// How an item is named on a drain list.
fn owner(item: &DurableItem) -> String {
    match (&item.process_id, &item.session_id) {
        (Some(process), Some(session)) => format!("process `{process}` in session `{session}`"),
        (Some(process), None) => format!("process `{process}`"),
        (None, Some(session)) => format!("session `{session}`"),
        (None, None) => format!(
            "an unidentified {} item",
            primary_format(item.surface).name()
        ),
    }
}

fn schema_report(status: &StoreSchemaStatus) -> SchemaReport {
    SchemaReport {
        outcome: match status.outcome() {
            StoreSchemaOutcome::Ready => "ready",
            StoreSchemaOutcome::Refused => "refused",
            StoreSchemaOutcome::Undecided => "undecided",
            _ => "unclassified",
        },
        databases: status
            .databases
            .iter()
            .map(|database| {
                let (verdict, found, reason) = match &database.verdict {
                    StoreSchemaVerdict::Matches => ("matches", None, None),
                    StoreSchemaVerdict::Mismatch { found } => ("mismatch", Some(*found), None),
                    StoreSchemaVerdict::Absent => ("absent", None, None),
                    StoreSchemaVerdict::Unreadable { reason } => {
                        ("unreadable", None, Some(reason.clone()))
                    }
                    _ => ("unclassified", None, None),
                };
                SchemaDatabaseReport {
                    name: database.name.clone(),
                    location: database.location.clone(),
                    expected: database.expected,
                    verdict,
                    found,
                    reason,
                }
            })
            .collect(),
    }
}

// Gated with the language, not merely with `test`: every fixture in there
// describes a store holding Lashlang durable data, and the version constants
// it compares against are themselves `rlm`-only.
#[cfg(all(test, feature = "rlm"))]
mod tests;
