//! What a readability probe found, in the shape an operator and a deploy gate
//! both read.
//!
//! The report is deliberately three answers rather than one verdict. A boolean
//! "is this store compatible?" cannot carry the thing that makes a version
//! boundary survivable: *which* format refuses, *how many* items carry the
//! version it refuses, and *which* of them an operator has to drain. Nor can it
//! say what nobody looked at, which is the difference between a store that is
//! clean and a store that was not read.
//!
//! Everything here serializes, because the report's second reader is a gate.
//! The version-bump runbook asserts on these fields directly, which is what
//! makes "the probe is right" a checked claim rather than a described one.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::formats::{DurableFormat, FormatProbe, FormatVersion};

/// How much of the store a probe was asked to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PreflightMode {
    /// Schema versions plus the process-registry surfaces.
    ///
    /// Bounded by the number of parked processes and pending wakes rather than
    /// by the number of sessions, which is what makes it safe to run on every
    /// boot. It leaves the per-session blob walk unread, and the report says so
    /// rather than letting the silence read as a clean bill of health.
    Summary,
    /// Everything summary reads, plus the per-session checkpoint walk, paged.
    Deep,
}

impl PreflightMode {
    /// The operator-facing word used in reports.
    pub fn name(self) -> &'static str {
        match self {
            PreflightMode::Summary => "summary",
            PreflightMode::Deep => "deep",
        }
    }
}

/// What the whole report means for a host deciding to boot.
///
/// The same three-way shape as
/// [`StoreSchemaOutcome`](lash_core::StoreSchemaOutcome), and for the same
/// reason: an item nobody could decode is neither a refusal nor a pass, and a
/// boolean would have to call it one of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PreflightOutcome {
    /// Nothing read refuses, and everything read decoded.
    Ready,
    /// At least one schema stamp or durable payload carries a version this
    /// build refuses. Booting produces exactly that refusal, later and less
    /// legibly.
    Refused,
    /// Nothing refuses, but something could not be read far enough to decide.
    /// The evidence for a pass is missing, not present.
    Undecided,
}

impl PreflightOutcome {
    /// The operator-facing word used in reports.
    pub fn name(self) -> &'static str {
        match self {
            PreflightOutcome::Ready => "ready",
            PreflightOutcome::Refused => "refused",
            PreflightOutcome::Undecided => "undecided",
        }
    }
}

/// One database's schema answer, in serializable form.
///
/// A projection of [`StoreSchemaDatabase`](lash_core::StoreSchemaDatabase)
/// rather than the type itself: the store contract deliberately carries no
/// serde derives, and a report that a gate reads has to serialize. The
/// projection is total — every verdict maps onto a name and, where there is
/// one, a found version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SchemaDatabaseReport {
    /// Operator-facing name of the database.
    pub name: String,
    /// Where the bytes live.
    pub location: String,
    /// The version this build requires.
    pub expected: i64,
    /// `matches`, `mismatch`, `absent` or `unreadable`.
    pub verdict: &'static str,
    /// The version stamped in the store, when one was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub found: Option<i64>,
    /// The backend's diagnostic, when the read could not decide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Every schema-carrying database, and what they mean together.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SchemaReport {
    /// `ready`, `refused` or `undecided`.
    pub outcome: &'static str,
    /// The databases, in the order the backend would open them.
    pub databases: Vec<SchemaDatabaseReport>,
}

/// One found version and how many items carry it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct FoundVersion {
    /// The version read out of the stored bytes.
    pub version: u32,
    /// How many items carried it.
    pub count: u64,
}

/// Why a format's row says what it says.
///
/// The distinction exists because three of the enumerated formats are not
/// independently stored, and a report that presented them as if they were would
/// be claiming evidence it does not have. A carried format's boundary is real —
/// it is simply enforced by the envelope that embeds it, whose version is
/// bumped whenever the carried format changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "carrier")]
#[non_exhaustive]
pub enum FormatEvidence {
    /// The version was read out of the stored bytes for this format.
    Direct,
    /// This format is never stored on its own: it rides inside another
    /// format's envelope, and that envelope's version moves whenever this one
    /// does. The verdict is the carrier's verdict, and the payload is the
    /// carrier's operator-facing name.
    CarriedBy(&'static str),
    /// No stored counterpart exists at all, so nothing is compared.
    NotPersisted,
}

/// What one durable format means for this deployment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ComponentVerdict {
    /// Every item found carried a version this build reads.
    AllReadable,
    /// At least one item carries a version this build refuses.
    Refused,
    /// The walk found no items carrying this format. Nothing refuses because
    /// nothing is there.
    Empty,
    /// Items were found and at least one could not be decoded far enough to
    /// read a version. Not a refusal; not a pass.
    Undecodable,
    /// The surfaces carrying this format were not walked in this run. The
    /// report's `not_scanned` list says why.
    NotScanned,
}

impl ComponentVerdict {
    /// The operator-facing word used in reports.
    pub fn name(self) -> &'static str {
        match self {
            ComponentVerdict::AllReadable => "all readable",
            ComponentVerdict::Refused => "refused",
            ComponentVerdict::Empty => "empty",
            ComponentVerdict::Undecodable => "undecodable",
            ComponentVerdict::NotScanned => "not scanned",
        }
    }
}

/// One durable format's readability across everything the walk read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ComponentReadability {
    /// The operator-facing format name, matching the manifest.
    pub format: String,
    /// The version this build writes, rendered the way it is compared.
    pub expected: String,
    /// What a probe can conclude about this format at all.
    pub probe: &'static str,
    /// Why this row says what it says.
    pub evidence: FormatEvidence,
    /// What this format means for the deployment.
    pub verdict: ComponentVerdict,
    /// How many items carrying this format the walk read.
    pub scanned: u64,
    /// Every version found, with counts, in ascending version order.
    pub found: Vec<FoundVersion>,
    /// How many items could not be decoded far enough to read a version.
    ///
    /// The bucket exists so the probe never has to panic on the data it is
    /// warning about: bytes that do not parse are counted here and described,
    /// not unwrapped.
    pub undecodable: u64,
    /// The first few undecodable reasons, verbatim, so the count is
    /// investigable without a second run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub undecodable_reasons: Vec<String>,
    /// How many items this build refuses without a version integer to name.
    ///
    /// The identity-only formats need this: a program identity that is not this
    /// build's is a refusal, and there is no "found version" to report for it
    /// because no version was ever stored. Counting these as undecodable would
    /// call a decided refusal undecided.
    #[serde(skip_serializing_if = "is_zero")]
    pub refused_without_version: u64,
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

impl ComponentReadability {
    /// Whether this format alone would refuse an open.
    pub fn refuses_open(&self) -> bool {
        self.verdict == ComponentVerdict::Refused
    }
}

/// One durable item that will not survive the version boundary.
///
/// This is the drain preflight: the same walk, keeping identities, so
/// "deployments drain first" becomes a list an operator can work through rather
/// than an instruction they have to trust.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DrainBlocker {
    /// The process holding the state, when the state belongs to one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_id: Option<String>,
    /// The session holding the state, when the state belongs to one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The store's own status word for the owner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// The format that refuses.
    pub format: String,
    /// The version this build writes, rendered the way it is compared.
    pub expected: String,
    /// The version found in the stored bytes, when one was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub found: Option<u32>,
    /// A single sentence naming the refusal, ready to print.
    pub detail: String,
}

/// Something the probe did not read, and why.
///
/// Every report carries this list, including a clean one. A preflight's worst
/// failure mode is a silent cap: a walk that stopped early and reported what it
/// managed to see as if it were everything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NotScanned {
    /// What was not read, in operator vocabulary.
    pub what: String,
    /// Why it was not read.
    pub reason: String,
}

/// The complete answer to "will this durable data open under this build?".
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PreflightReport {
    /// Which deployment was inspected, verbatim from the handle.
    pub backend: String,
    /// How much of it was read.
    pub mode: PreflightMode,
    /// What the whole report means for a host deciding to boot.
    pub outcome: PreflightOutcome,
    /// The schema answer, which is the boundary a host hits first.
    pub schema: SchemaReport,
    /// One row per durable format this build writes.
    pub components: Vec<ComponentReadability>,
    /// The items an operator has to drain, in walk order.
    pub drain: Vec<DrainBlocker>,
    /// Everything the probe did not read.
    pub not_scanned: Vec<NotScanned>,
}

impl PreflightReport {
    /// The formats that would refuse an open, in report order.
    pub fn refusals(&self) -> impl Iterator<Item = &ComponentReadability> {
        self.components
            .iter()
            .filter(|component| component.refuses_open())
    }

    /// One actionable sentence for a host that has decided not to start, or
    /// `None` when nothing refuses.
    ///
    /// A supervisor reads one line before it decides whether to restart, and a
    /// restart cannot fix a version boundary. The message therefore names the
    /// boundary, the count, and the remedy in a single string a host can log
    /// and exit on — which is the whole difference between a clean permanent
    /// failure and a crash loop.
    pub fn refusal_message(&self) -> Option<String> {
        if self.outcome != PreflightOutcome::Refused {
            return None;
        }
        let mut message = format!("{} refuses this build: ", self.backend);
        let mut reasons: Vec<String> = self
            .schema
            .databases
            .iter()
            .filter(|database| database.verdict == "mismatch")
            .map(|database| {
                format!(
                    "schema `{}` is at version {} and this build requires {}",
                    database.name,
                    database
                        .found
                        .map(|found| found.to_string())
                        .unwrap_or_else(|| "an unread version".to_string()),
                    database.expected
                )
            })
            .collect();
        for component in self.refusals() {
            // The identity-only formats refuse without a version to print:
            // bytecode names the build that produced it, so there is no found
            // integer, and rendering an empty list would leave the operator
            // reading "`bytecode` is at  and this build writes ...".
            if component.found.is_empty() {
                let remedy = if component.format == DurableFormat::ModuleArtifact.name() {
                    "; recompile and republish the module"
                } else {
                    ""
                };
                reasons.push(format!(
                    "`{}` holds {} item(s) written by another build, and this build writes {}{}",
                    component.format, component.refused_without_version, component.expected, remedy
                ));
                continue;
            }
            let found = component
                .found
                .iter()
                .map(|found| format!("{} ({} item(s))", found.version, found.count))
                .collect::<Vec<_>>()
                .join(", ");
            reasons.push(format!(
                "`{}` is at {found} and this build writes {}",
                component.format, component.expected
            ));
        }
        message.push_str(&reasons.join("; "));
        write!(
            message,
            ". Drain {} affected item(s) on the previous build, or recreate the store; \
             there is no migration decoder at these boundaries.",
            self.drain.len()
        )
        .expect("a String write cannot fail");
        Some(message)
    }
}

impl std::fmt::Display for PreflightReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "backend: {}", self.backend)?;
        writeln!(f, "mode: {}", self.mode.name())?;
        writeln!(f, "outcome: {}", self.outcome.name())?;
        writeln!(f, "schema: {}", self.schema.outcome)?;
        for database in &self.schema.databases {
            writeln!(
                f,
                "  {}: {} (expected {})",
                database.name,
                match (database.verdict, database.found) {
                    ("mismatch", Some(found)) => format!("found {found}"),
                    (verdict, _) => verdict.to_string(),
                },
                database.expected
            )?;
        }
        for component in &self.components {
            write!(
                f,
                "  {}: {} (expected {})",
                component.format,
                component.verdict.name(),
                component.expected
            )?;
            for found in &component.found {
                write!(f, " [found {} x{}]", found.version, found.count)?;
            }
            if component.undecodable > 0 {
                write!(f, " [undecodable x{}]", component.undecodable)?;
            }
            writeln!(f)?;
        }
        for blocker in &self.drain {
            writeln!(f, "  drain: {}", blocker.detail)?;
        }
        for skipped in &self.not_scanned {
            writeln!(f, "  not scanned: {} ({})", skipped.what, skipped.reason)?;
        }
        Ok(())
    }
}

/// Accumulates one format's evidence across a walk.
///
/// Kept separate from [`ComponentReadability`] because a tally is a growing
/// thing and a report row is a settled one: folding counts into the reported
/// shape would mean a half-built report is representable, and half-built
/// reports are how a walk that stopped early gets published as a complete
/// answer.
#[derive(Clone, Debug, Default)]
pub(super) struct FormatTally {
    pub(super) scanned: u64,
    pub(super) found: BTreeMap<u32, u64>,
    pub(super) undecodable: u64,
    pub(super) undecodable_reasons: Vec<String>,
    pub(super) refused_without_version: u64,
}

/// How many undecodable reasons a report keeps verbatim.
///
/// A store with a systematic decode failure has one reason repeated a million
/// times; keeping a handful makes the count investigable without making the
/// report the size of the corruption.
const MAX_UNDECODABLE_REASONS: usize = 3;

impl FormatTally {
    pub(super) fn record(&mut self, version: u32) {
        self.scanned += 1;
        *self.found.entry(version).or_default() += 1;
    }

    pub(super) fn identity_match(&mut self) {
        self.scanned += 1;
    }

    /// Record an item this build refuses for a reason no version integer
    /// expresses — the identity-only case.
    ///
    pub(super) fn refuse_without_version(&mut self) {
        self.scanned += 1;
        self.refused_without_version += 1;
    }

    pub(super) fn undecodable(&mut self, reason: impl Into<String>) {
        self.scanned += 1;
        self.undecodable += 1;
        if self.undecodable_reasons.len() < MAX_UNDECODABLE_REASONS {
            self.undecodable_reasons.push(reason.into());
        }
    }

    pub(super) fn into_row(
        self,
        format: DurableFormat,
        expected: FormatVersion,
        probe: FormatProbe,
        evidence: FormatEvidence,
    ) -> ComponentReadability {
        let refused = self.refused_without_version > 0
            || self
                .found
                .keys()
                .any(|found| !reads_version(expected, *found));
        let verdict = if self.scanned == 0 {
            ComponentVerdict::Empty
        } else if refused {
            // A refusal outranks an undecodable item for the same reason a
            // schema refusal outranks an undecided database: a known refusal is
            // the decisive fact, and the undecodable count stays reportable
            // behind it.
            ComponentVerdict::Refused
        } else if self.undecodable > 0 {
            ComponentVerdict::Undecodable
        } else {
            ComponentVerdict::AllReadable
        };
        ComponentReadability {
            format: format.name().to_string(),
            expected: expected.to_string(),
            probe: probe_name(probe),
            evidence,
            verdict,
            scanned: self.scanned,
            found: self
                .found
                .into_iter()
                .map(|(version, count)| FoundVersion { version, count })
                .collect(),
            undecodable: self.undecodable,
            undecodable_reasons: self.undecodable_reasons,
            refused_without_version: self.refused_without_version,
        }
    }
}

/// Whether this build reads a found version, given the boundary contract the
/// manifest states for the format.
///
/// Shared with the walk rather than kept private to the tally, because the
/// drain list and the readability row must agree by construction: two copies of
/// this comparison is exactly how a report comes to name a refusal it did not
/// list, or list one it did not name.
pub(super) fn reads_version(expected: FormatVersion, found: u32) -> bool {
    match expected {
        FormatVersion::Counter(version) => found == version,
        // One-directional by contract: older generations still mean what they
        // meant, a newer one cannot be given a shape.
        FormatVersion::ForwardOnly(generation) => found <= generation,
        // An identity is compared as a string elsewhere; a counter found against
        // an identity format is not a comparison this build makes.
        FormatVersion::Identity(_) => false,
    }
}

/// The manifest's probe classification, as the word a report prints.
pub(super) fn probe_name(probe: FormatProbe) -> &'static str {
    match probe {
        FormatProbe::Comparable => "comparable",
        FormatProbe::IdentityOnly => "identity only",
        FormatProbe::NotPersisted => "not persisted",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tally_row(tally: FormatTally, expected: FormatVersion) -> ComponentReadability {
        tally.into_row(
            DurableFormat::VmContinuation,
            expected,
            FormatProbe::Comparable,
            FormatEvidence::Direct,
        )
    }

    #[test]
    fn an_empty_walk_is_empty_rather_than_readable() {
        // The distinction a boolean loses: "nothing is parked" and "everything
        // parked opens" are different operational facts, and only one of them
        // means a drain is unnecessary because a drain already happened.
        let row = tally_row(FormatTally::default(), FormatVersion::Counter(8));
        assert_eq!(row.verdict, ComponentVerdict::Empty);
        assert_eq!(row.scanned, 0);
        assert!(!row.refuses_open());
    }

    #[test]
    fn a_counter_boundary_refuses_in_both_directions() {
        for found in [7, 9] {
            let mut tally = FormatTally::default();
            tally.record(found);
            let row = tally_row(tally, FormatVersion::Counter(8));
            assert_eq!(row.verdict, ComponentVerdict::Refused, "found {found}");
            assert!(row.refuses_open());
        }
    }

    #[test]
    fn a_forward_only_fence_reads_older_generations_and_refuses_newer() {
        let mut older = FormatTally::default();
        older.record(1);
        assert_eq!(
            tally_row(older, FormatVersion::ForwardOnly(2)).verdict,
            ComponentVerdict::AllReadable,
            "rolling back is the supported direction for an immutable body"
        );

        let mut newer = FormatTally::default();
        newer.record(3);
        assert_eq!(
            tally_row(newer, FormatVersion::ForwardOnly(2)).verdict,
            ComponentVerdict::Refused
        );
    }

    #[test]
    fn a_refusal_outranks_an_undecodable_item_without_hiding_it() {
        let mut tally = FormatTally::default();
        tally.record(7);
        tally.undecodable("trailing garbage");
        let row = tally_row(tally, FormatVersion::Counter(8));
        assert_eq!(row.verdict, ComponentVerdict::Refused);
        assert_eq!(row.undecodable, 1);
        assert_eq!(
            row.undecodable_reasons,
            vec!["trailing garbage".to_string()]
        );
        assert_eq!(row.scanned, 2);
    }

    #[test]
    fn undecodable_items_alone_are_undecided_rather_than_readable() {
        let mut tally = FormatTally::default();
        tally.undecodable("not valid JSON");
        let row = tally_row(tally, FormatVersion::Counter(8));
        assert_eq!(row.verdict, ComponentVerdict::Undecodable);
        assert!(!row.refuses_open(), "nobody read a version to refuse");
    }

    #[test]
    fn undecodable_reasons_are_capped_but_counted_in_full() {
        let mut tally = FormatTally::default();
        for index in 0..50 {
            tally.undecodable(format!("failure {index}"));
        }
        let row = tally_row(tally, FormatVersion::Counter(8));
        assert_eq!(row.undecodable, 50);
        assert_eq!(row.undecodable_reasons.len(), MAX_UNDECODABLE_REASONS);
    }

    #[test]
    fn found_versions_are_counted_and_ordered() {
        let mut tally = FormatTally::default();
        tally.record(9);
        tally.record(8);
        tally.record(9);
        let row = tally_row(tally, FormatVersion::Counter(8));
        assert_eq!(
            row.found,
            vec![
                FoundVersion {
                    version: 8,
                    count: 1
                },
                FoundVersion {
                    version: 9,
                    count: 2
                }
            ]
        );
    }

    fn refused_report() -> PreflightReport {
        let mut tally = FormatTally::default();
        tally.record(2);
        PreflightReport {
            backend: "sqlite (/srv/lash/durable-core.db)".to_string(),
            mode: PreflightMode::Summary,
            outcome: PreflightOutcome::Refused,
            schema: SchemaReport {
                outcome: "ready",
                databases: Vec::new(),
            },
            components: vec![tally.into_row(
                DurableFormat::LashlangSegmentHandover,
                FormatVersion::Counter(3),
                FormatProbe::Comparable,
                FormatEvidence::Direct,
            )],
            drain: vec![DrainBlocker {
                process_id: Some("p-1".to_string()),
                session_id: Some("s-1".to_string()),
                status: Some("waiting".to_string()),
                format: "Lashlang segment handover".to_string(),
                expected: "3".to_string(),
                found: Some(2),
                detail: "process `p-1` parked a segment at version 2".to_string(),
            }],
            not_scanned: Vec::new(),
        }
    }

    #[test]
    fn a_refusal_message_names_the_boundary_the_count_and_the_remedy() {
        let message = refused_report()
            .refusal_message()
            .expect("a refused report has a message");
        assert!(message.contains("Lashlang segment handover"), "{message}");
        assert!(message.contains("2 (1 item(s))"), "{message}");
        assert!(message.contains("this build writes 3"), "{message}");
        assert!(message.contains("Drain 1 affected item(s)"), "{message}");
        assert!(message.contains("no migration decoder"), "{message}");
    }

    #[test]
    fn a_ready_report_has_no_refusal_message() {
        // A host that printed a refusal over a healthy store would teach its
        // operators to ignore the message.
        let mut report = refused_report();
        report.outcome = PreflightOutcome::Ready;
        assert_eq!(report.refusal_message(), None);
    }

    #[test]
    fn the_report_serializes_the_fields_a_gate_asserts_on() {
        let json = serde_json::to_value(refused_report()).expect("the report serializes");
        assert_eq!(json["outcome"], "refused");
        assert_eq!(json["mode"], "summary");
        assert_eq!(json["components"][0]["verdict"], "refused");
        assert_eq!(json["components"][0]["found"][0]["version"], 2);
        assert_eq!(json["components"][0]["found"][0]["count"], 1);
        assert_eq!(json["components"][0]["evidence"]["kind"], "direct");
        assert_eq!(json["drain"][0]["process_id"], "p-1");
        assert_eq!(json["drain"][0]["found"], 2);
    }

    #[test]
    fn a_carried_format_names_its_carrier_in_the_serialized_report() {
        // The honest limit made machine-readable: a gate can tell a directly
        // probed row from one whose boundary is enforced by an envelope.
        let row = FormatTally::default().into_row(
            DurableFormat::HeapSizeSchedule,
            FormatVersion::Counter(2),
            FormatProbe::Comparable,
            FormatEvidence::CarriedBy(DurableFormat::VmContinuation.name()),
        );
        let json = serde_json::to_value(row).expect("the row serializes");
        assert_eq!(json["evidence"]["kind"], "carried_by");
        assert_eq!(json["evidence"]["carrier"], "VM continuation");
    }

    #[test]
    fn rendering_names_what_was_not_scanned() {
        let mut report = refused_report();
        report.not_scanned.push(NotScanned {
            what: "session checkpoints".to_string(),
            reason: "summary mode skips the per-session blob walk".to_string(),
        });
        let rendered = report.to_string();
        assert!(rendered.contains("mode: summary"), "{rendered}");
        assert!(
            rendered.contains("not scanned: session checkpoints"),
            "{rendered}"
        );
        assert!(rendered.contains("[found 2 x1]"), "{rendered}");
    }
}
