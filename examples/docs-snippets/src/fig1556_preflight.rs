//! Store-compatibility preflight (FIG-1556): asking "will this durable data
//! open under this build?" *before* wiring anything.
//!
//! Lash's durable formats fail closed with no migration decoders, so a version
//! boundary is a refusal rather than a migration. Discovering that refusal by
//! booting, wiring the runtime, and letting the first store access answer is a
//! crash loop under a supervisor. The surface below answers first, and reads
//! only: it is built from raw connection configuration rather than from a
//! wired store, because constructing a store is itself the side-effectful act
//! this precedes.

use std::fmt::Write as _;
use std::path::Path;

use lash::formats::{
    DurableFormat, DurableFormatEntry, FormatProbe, FormatVersion, durable_format, durable_formats,
};
use lash::persistence::{
    StoreBackend, StoreError, StorePreflight, StoreSchemaDatabase, StoreSchemaOutcome,
    StoreSchemaStatus, StoreSchemaVerdict,
};
use lash_postgres_store::PostgresStorePreflight;
use lash_sqlite_store::{SqliteDatabase, SqliteStorePreflight, verify_schema_at};

/// The formats this host has durable state behind, in the order a report reads
/// them. A deploy gate walks a fixed list rather than whatever the manifest
/// happens to hold, so a format appearing or vanishing between builds is a diff
/// somebody approved rather than a silent change of coverage.
const PINNED_FORMATS: [DurableFormat; 13] = [
    DurableFormat::ModuleArtifact,
    DurableFormat::SessionCheckpointManifest,
    DurableFormat::CheckpointComponentEncoding,
    DurableFormat::SessionHeadMeta,
    DurableFormat::ProcessWakeDelivery,
    DurableFormat::SessionNodeBody,
    DurableFormat::Bytecode,
    DurableFormat::VmContinuation,
    DurableFormat::LashlangSnapshot,
    DurableFormat::HeapSizeSchedule,
    DurableFormat::LashlangSegmentHandover,
    DurableFormat::RlmSnapshotEnvelope,
    DurableFormat::VmAbi,
];

/// The manifest is what an operator compares before a deploy: one typed table
/// of every durable format this build writes, re-exported from the crates that
/// own the constants so the reported version and the written version cannot
/// disagree.
fn format_manifest_report() -> String {
    // docs:start:format-manifest
    let mut report = String::new();
    for entry in durable_formats() {
        let row = format!(
            "{}: {} ({}::{}) [{:?}]",
            entry.format.name(),
            entry.version,
            entry.owning_crate,
            entry.constant,
            entry.probe
        );
        report.push_str(&row);
        report.push('\n');
    }
    // docs:end:format-manifest
    report
}

/// The version this build writes for one format, or `None` when the format is
/// not part of this build at all — which is a different answer from "version
/// zero" and has to stay distinguishable.
fn format_version_of(format: DurableFormat) -> Option<FormatVersion> {
    durable_format(format).map(|entry| entry.version)
}

/// The versions this build writes for the formats the host pinned, so the
/// deploy gate compares a list it controls rather than a list that moves.
fn pinned_format_versions() -> Vec<(DurableFormat, Option<FormatVersion>)> {
    PINNED_FORMATS
        .iter()
        .map(|format| (*format, format_version_of(*format)))
        .collect()
}

/// The manifest versions an operator compares between two builds. Recording
/// them beside the artifact is what lets a refusal after a rollout be traced to
/// the identity or bump that caused it, rather than reconstructed from logs
/// afterwards.
fn recorded_format_versions() -> Vec<(&'static str, String)> {
    vec![
        (
            "module artifact",
            lash::formats::LASHLANG_SEMANTIC_HASH_VERSION.to_string(),
        ),
        (
            "session checkpoint manifest",
            lash::formats::SESSION_CHECKPOINT_SCHEMA_VERSION.to_string(),
        ),
        (
            "checkpoint component encoding",
            lash::formats::CHECKPOINT_COMPONENT_ENCODING_VERSION.to_string(),
        ),
        (
            "session head meta",
            lash::formats::SESSION_HEAD_META_SCHEMA_VERSION.to_string(),
        ),
        (
            "process wake delivery",
            lash::formats::PROCESS_WAKE_DELIVERY_FORMAT_VERSION.to_string(),
        ),
        (
            "session node body",
            // The one forward-only fence, recorded in the shape it is compared
            // in: "N or older" rather than a bare integer that would read as an
            // exact match.
            FormatVersion::ForwardOnly(lash::formats::SESSION_NODE_BODY_SCHEMA_VERSION).to_string(),
        ),
        (
            "bytecode",
            lash::formats::BYTECODE_FORMAT_VERSION.to_string(),
        ),
        (
            "VM continuation",
            lash::formats::VM_CONTINUATION_FORMAT_VERSION.to_string(),
        ),
        (
            "Lashlang snapshot",
            lash::formats::LASHLANG_SNAPSHOT_VERSION.to_string(),
        ),
        (
            "heap size schedule",
            lash::formats::HEAP_SIZE_SCHEDULE_VERSION.to_string(),
        ),
        (
            "Lashlang segment handover",
            lash::formats::LASHLANG_SEGMENT_STATE_VERSION.to_string(),
        ),
        (
            "RLM snapshot envelope",
            lash::formats::RLM_SNAPSHOT_VERSION.to_string(),
        ),
        (
            "Lashlang VM ABI",
            lash::formats::LASHLANG_VM_ABI_VERSION.to_string(),
        ),
    ]
}

/// What a probe can conclude about each format — the honest limit of what
/// stored bytes can be asked, rendered as the column an operator reads.
fn probe_limits_report() -> String {
    let mut report = String::new();
    // docs:start:format-probe-limits
    for entry in durable_formats() {
        let note = match entry.probe {
            // Stored bytes carry their own version, so a probe reads it and
            // compares. What the comparison *means* is the version's own shape:
            // a counter is exact-match in both directions, a forward-only fence
            // refuses only a strictly newer generation.
            FormatProbe::Comparable => format!("version {}", entry.version),
            // Bytecode stores an identity, not a version: nothing recoverable
            // says "written by version N", so a probe can only recompute the
            // identity this build would produce and check whether it matches.
            FormatProbe::IdentityOnly => "identity only, recomputed per item".to_string(),
            // The VM ABI is never persisted, so there is nothing durable to
            // compare and the manifest reports it informationally.
            FormatProbe::NotPersisted => format!("not persisted ({})", entry.version),
            _ => "unclassified".to_string(),
        };
        writeln!(report, "{}: {note}", entry.format.name()).expect("a String write cannot fail");
    }
    // docs:end:format-probe-limits
    report
}

/// A counter is compared as a number, so a host rendering a comparison has to
/// know it is holding one before it subtracts anything.
fn version_counter(version: FormatVersion) -> Option<u32> {
    match version {
        FormatVersion::Counter(counter) => Some(counter),
        _ => None,
    }
}

/// A build identity is compared for equality and nothing else: there is no
/// ordering between two of them, so "newer" is not a question it can answer.
fn build_identity(version: FormatVersion) -> Option<&'static str> {
    match version {
        FormatVersion::Identity(identity) => Some(identity),
        _ => None,
    }
}

/// A forward-only fence's generation, when that is the shape this format's
/// boundary has. A host comparing two builds has to know which shape it holds:
/// subtracting fences tells it nothing about which direction is refused.
fn forward_only_generation(version: FormatVersion) -> Option<u32> {
    match version {
        FormatVersion::ForwardOnly(generation) => Some(generation),
        _ => None,
    }
}

/// Whether a deep walk is worth its cost for this format. An identity probe
/// recomputes per stored item, so a host decides up front rather than
/// discovering the bill mid-walk.
fn needs_recompute(entry: &DurableFormatEntry) -> bool {
    matches!(entry.probe, FormatProbe::IdentityOnly)
}

/// Read a SQLite deployment's recorded schema versions without opening it: no
/// write lock, no schema batch, and no database brought into existence by the
/// act of asking about it.
async fn sqlite_deployment_status(root: &Path) -> Result<StoreSchemaStatus, StoreError> {
    // docs:start:sqlite-preflight
    let preflight = SqliteStorePreflight::for_session_store_root(root)
        .with_process_registry(root.join("processes.db"))
        .with_trigger_store(root.join("triggers.db"))
        .with_effect_journal(root.join("effects.db"));

    let status = preflight.schema_status().await?;
    match status.outcome() {
        // Every database answered, and every answer matched.
        StoreSchemaOutcome::Ready => {}
        // The refusal the open would have produced, named before the wiring.
        StoreSchemaOutcome::Refused => {
            for refusal in status.refusals() {
                eprintln!(
                    "{} would refuse: expected {}, found {} at {}",
                    refusal.name,
                    refusal.expected,
                    match &refusal.verdict {
                        StoreSchemaVerdict::Mismatch { found } => found.to_string(),
                        _ => "n/a".to_string(),
                    },
                    refusal.location
                );
            }
        }
        // Nothing refused on a version, but a database could not be read far
        // enough to decide. Do not boot blind: the evidence for a pass is
        // missing, not present, and PostgreSQL's structural gate refuses drift
        // that arrives here.
        StoreSchemaOutcome::Undecided => {
            for undecided in status.undecided() {
                eprintln!(
                    "{} is undecided at {}: {} — investigate before starting",
                    undecided.name,
                    undecided.location,
                    match &undecided.verdict {
                        StoreSchemaVerdict::Unreadable { reason } => reason.as_str(),
                        _ => "no reason reported",
                    }
                );
            }
        }
        _ => eprintln!("unclassified schema outcome — investigate before starting"),
    }
    // docs:end:sqlite-preflight
    Ok(status)
}

/// Read one database's verdict, for a host that gates on a single file rather
/// than on the whole deployment.
async fn verify_one_database(path: &Path) -> StoreSchemaDatabase {
    // docs:start:sqlite-verify-one
    let database = verify_schema_at(path, SqliteDatabase::DurableCore).await;
    match &database.verdict {
        StoreSchemaVerdict::Matches => eprintln!("ready at version {}", database.expected),
        StoreSchemaVerdict::Mismatch { found } => {
            eprintln!("refused: found {found}, expected {}", database.expected)
        }
        StoreSchemaVerdict::Unreadable { reason } => eprintln!("undecided: {reason}"),
        StoreSchemaVerdict::Absent => eprintln!("nothing provisioned yet"),
        _ => {}
    }
    // docs:end:sqlite-verify-one
    database
}

/// A PostgreSQL preflight is built from raw connection configuration, and its
/// report header never carries the credentials that configuration held.
async fn postgres_preflight_target(database_url: &str) -> String {
    // docs:start:postgres-preflight
    let preflight =
        PostgresStorePreflight::for_database_url(database_url).expect("a valid database URL");
    let StoreBackend::Postgres { location } = preflight.backend() else {
        panic!("a postgres handle identifies as postgres")
    };
    preflight.close().await;
    // docs:end:postgres-preflight
    location
}

/// The report header: which deployment answered, in one operator-facing line.
fn backend_label(backend: &StoreBackend) -> String {
    backend.to_string()
}

/// Where the bytes live, when the backend has somewhere to point at. A
/// process-lifetime store has nothing durable behind it and says so rather than
/// inventing a path.
fn backend_location(backend: &StoreBackend) -> Option<&str> {
    match backend {
        StoreBackend::Sqlite { location } => Some(location),
        StoreBackend::Postgres { location } => Some(location),
        StoreBackend::InMemory => None,
        _ => None,
    }
}

/// Whether this database alone would refuse the open. An unreadable database is
/// undecided, not refused, and a host that conflates the two refuses to boot on
/// evidence it does not have.
fn would_refuse(database: &StoreSchemaDatabase) -> bool {
    database.verdict.refuses_open()
}

/// Whether this database went unanswered — the other half of the pair above,
/// kept separate so a host can log "could not decide" without dressing it up as
/// either a pass or a refusal.
fn is_undecided(database: &StoreSchemaDatabase) -> bool {
    database.verdict.is_undecided()
}

/// One word for the whole deployment, with no fall-through to green: an outcome
/// this build does not recognise reads as unclassified rather than as ready.
fn outcome_label(outcome: StoreSchemaOutcome) -> &'static str {
    match outcome {
        StoreSchemaOutcome::Ready => "ready",
        StoreSchemaOutcome::Refused => "refused",
        StoreSchemaOutcome::Undecided => "undecided",
        _ => "unclassified",
    }
}

/// How many schema-carrying databases answered, and how many of them would
/// refuse — the summary line a supervisor logs before deciding to start.
fn deployment_summary(status: &StoreSchemaStatus) -> String {
    let count = status.databases.len();
    format!(
        "{count} database{}, {} refusing, {} undecided\n",
        if count == 1 { "" } else { "s" },
        status.refusals().count(),
        status.undecided().count()
    )
}

/// One probe per boot. The handle is asked once and the answer is passed around,
/// because a second probe is a second round trip that can disagree with the
/// first — and a gate that read two different answers would report whichever it
/// happened to look at last.
async fn probe(handle: &dyn StorePreflight) -> Result<StoreSchemaStatus, StoreError> {
    handle.schema_status().await
}

/// A host boots against the preflight surface behind `&dyn StorePreflight`, so
/// the backend it happens to run is not baked into the boot path.
async fn boot_report(handle: &dyn StorePreflight) -> Result<String, StoreError> {
    let mut report = String::new();
    writeln!(report, "backend: {}", handle.backend()).expect("a String write cannot fail");
    let status = probe(handle).await?;
    // The outcome leads, because it is the line a supervisor acts on, and it
    // cannot say "ready" over a database nobody could read.
    let outcome = status.outcome();
    writeln!(report, "outcome: {}", outcome_label(outcome)).expect("a String write cannot fail");
    report.push_str(&deployment_summary(&status));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// The manifest is one typed table of every durable format this build
    /// writes, and each row reports the version its owning crate defines rather
    /// than a copy that can drift away from it.
    #[test]
    fn the_manifest_publishes_every_durable_format_this_build_writes() {
        let report = format_manifest_report();
        assert_eq!(report.lines().count(), durable_formats().len());
        assert!(report.contains("VM continuation: "), "{report}");
        assert!(report.contains("(lashlang::VM_CONTINUATION"), "{report}");

        // Three of these were unreachable from a host before this manifest
        // existed.
        let continuation =
            durable_format(DurableFormat::VmContinuation).expect("the language is in this build");
        assert_eq!(
            continuation.version,
            FormatVersion::Counter(lash::formats::VM_CONTINUATION_FORMAT_VERSION)
        );
        assert_eq!(continuation.owning_crate, "lashlang");

        let envelope = durable_format(DurableFormat::RlmSnapshotEnvelope)
            .expect("the RLM protocol is present");
        assert_eq!(
            envelope.version,
            FormatVersion::Counter(lash::formats::RLM_SNAPSHOT_VERSION)
        );
        let handover = durable_format(DurableFormat::LashlangSegmentHandover)
            .expect("segment handover is present");
        assert_eq!(
            handover.version,
            FormatVersion::Counter(lash::formats::LASHLANG_SEGMENT_STATE_VERSION)
        );
        assert_eq!(
            format_version_of(DurableFormat::SessionCheckpointManifest),
            Some(FormatVersion::Counter(
                lash::formats::SESSION_CHECKPOINT_SCHEMA_VERSION
            ))
        );
        assert_eq!(
            format_version_of(DurableFormat::CheckpointComponentEncoding),
            Some(FormatVersion::Counter(
                lash::formats::CHECKPOINT_COMPONENT_ENCODING_VERSION
            ))
        );
        assert_eq!(
            format_version_of(DurableFormat::SessionHeadMeta),
            Some(FormatVersion::Counter(
                lash::formats::SESSION_HEAD_META_SCHEMA_VERSION
            ))
        );
        assert_eq!(
            format_version_of(DurableFormat::ProcessWakeDelivery),
            Some(FormatVersion::Counter(
                lash::formats::PROCESS_WAKE_DELIVERY_FORMAT_VERSION
            ))
        );
        assert_eq!(
            format_version_of(DurableFormat::LashlangSnapshot),
            Some(FormatVersion::Counter(
                lash::formats::LASHLANG_SNAPSHOT_VERSION
            ))
        );
        assert_eq!(
            format_version_of(DurableFormat::HeapSizeSchedule),
            Some(FormatVersion::Counter(
                lash::formats::HEAP_SIZE_SCHEDULE_VERSION
            ))
        );
    }

    /// The pinned list is the host's own coverage claim, so it has to stay in
    /// step with what the build can actually answer for.
    #[test]
    fn every_pinned_format_is_answered_by_this_build() {
        let pinned = pinned_format_versions();
        assert_eq!(pinned.len(), durable_formats().len());
        assert!(pinned.iter().all(|(_, version)| version.is_some()));
        let recorded = recorded_format_versions();
        assert_eq!(recorded.len(), durable_formats().len());
        for (name, version) in &recorded {
            let entry = durable_formats()
                .iter()
                .find(|entry| entry.format.name() == *name)
                .expect("every recorded name is a manifest row");
            assert_eq!(&entry.version.to_string(), version, "{name}");
        }
    }

    /// A manifest row is traceable rather than merely readable: it names the
    /// crate and the constant its version came from, and it reports the version
    /// in the shape that version is compared in.
    #[test]
    fn a_manifest_row_traces_its_version_back_to_the_constant_that_wrote_it() {
        let head = durable_format(DurableFormat::SessionHeadMeta).expect("a store format");
        assert_eq!(head.format, DurableFormat::SessionHeadMeta);
        assert_eq!(head.format.name(), "session head meta");
        assert_eq!(head.owning_crate, "lash-core");
        assert_eq!(head.constant, "SESSION_HEAD_META_SCHEMA_VERSION");
        assert_eq!(head.probe, FormatProbe::Comparable);
        assert_eq!(
            head.version,
            FormatVersion::Counter(lash::formats::SESSION_HEAD_META_SCHEMA_VERSION)
        );
        assert_eq!(
            version_counter(head.version),
            Some(lash::formats::SESSION_HEAD_META_SCHEMA_VERSION)
        );
        assert_eq!(build_identity(head.version), None);
        assert!(!needs_recompute(head));

        let module = durable_format(DurableFormat::ModuleArtifact).expect("a module format");
        assert_eq!(module.owning_crate, "lashlang");
        assert_eq!(module.constant, "LASHLANG_SEMANTIC_HASH_VERSION");
        assert_eq!(module.probe, FormatProbe::IdentityOnly);
        assert_eq!(
            module.version,
            FormatVersion::Identity(lash::formats::LASHLANG_SEMANTIC_HASH_VERSION)
        );
    }

    /// The two exclusions are marked, not omitted. Dropping them would make the
    /// manifest quietly narrower than the boundary it describes.
    #[test]
    fn a_format_a_probe_cannot_compare_says_so_rather_than_disappearing() {
        let report = probe_limits_report();
        assert!(report.contains("bytecode: identity only"), "{report}");
        assert!(
            report.contains("Lashlang VM ABI: not persisted"),
            "{report}"
        );
        assert!(report.contains("session head meta: version"), "{report}");

        let bytecode = durable_format(DurableFormat::Bytecode).expect("bytecode is in this build");
        assert_eq!(bytecode.probe, FormatProbe::IdentityOnly);
        assert!(needs_recompute(bytecode));
        assert_eq!(
            bytecode.version,
            FormatVersion::Counter(lash::formats::BYTECODE_FORMAT_VERSION)
        );

        let abi = durable_format(DurableFormat::VmAbi).expect("the VM ABI is a build fact");
        assert_eq!(abi.probe, FormatProbe::NotPersisted);
        assert_eq!(
            abi.version,
            FormatVersion::Identity(lash::formats::LASHLANG_VM_ABI_VERSION)
        );
        assert_eq!(
            build_identity(abi.version),
            Some(lash::formats::LASHLANG_VM_ABI_VERSION)
        );
        assert_eq!(version_counter(abi.version), None);
        assert_eq!(
            abi.version.to_string(),
            lash::formats::LASHLANG_VM_ABI_VERSION
        );
        assert_eq!(DurableFormat::VmAbi.name(), "Lashlang VM ABI");
    }

    /// A refusal has to name the format an operator can act on, so every format
    /// carries an operator-facing name rather than a discriminant.
    #[test]
    fn every_durable_format_names_itself_the_way_a_report_reads_it() {
        let manifest = "session checkpoint manifest";
        let encoding = "checkpoint component encoding";
        let wake = "process wake delivery";
        let handover = "Lashlang segment handover";
        let envelope = "RLM snapshot envelope";

        assert_eq!(DurableFormat::SessionCheckpointManifest.name(), manifest);
        assert_eq!(DurableFormat::CheckpointComponentEncoding.name(), encoding);
        assert_eq!(DurableFormat::SessionHeadMeta.name(), "session head meta");
        assert_eq!(DurableFormat::ProcessWakeDelivery.name(), wake);
        assert_eq!(DurableFormat::SessionNodeBody.name(), "session node body");
        assert_eq!(DurableFormat::Bytecode.name(), "bytecode");
        assert_eq!(DurableFormat::VmContinuation.name(), "VM continuation");
        assert_eq!(DurableFormat::LashlangSnapshot.name(), "Lashlang snapshot");
        assert_eq!(DurableFormat::HeapSizeSchedule.name(), "heap size schedule");
        assert_eq!(DurableFormat::LashlangSegmentHandover.name(), handover);
        assert_eq!(DurableFormat::RlmSnapshotEnvelope.name(), envelope);
        assert_eq!(DurableFormat::VmAbi.name(), "Lashlang VM ABI");
    }

    /// A SQLite deployment answers the schema question without being opened,
    /// and asking does not provision the thing it was asked about.
    #[tokio::test]
    async fn a_sqlite_deployment_reports_its_schema_without_being_opened() {
        let root = tempfile::tempdir().expect("temp dir");
        let status = sqlite_deployment_status(root.path())
            .await
            .expect("read the deployment");

        // Nothing is provisioned yet, so nothing refuses — and asking did not
        // provision it.
        assert_eq!(status.outcome(), StoreSchemaOutcome::Ready);
        assert_eq!(outcome_label(status.outcome()), "ready");
        assert_eq!(status.databases.len(), 4);
        assert!(
            status
                .databases
                .iter()
                .all(|database| !is_undecided(database))
        );
        assert!(
            status
                .databases
                .iter()
                .all(|database| !would_refuse(database))
        );
        assert!(
            status
                .databases
                .iter()
                .all(|database| database.expected > 0)
        );
        assert_eq!(status.databases[0].verdict, StoreSchemaVerdict::Absent);
        assert!(!root.path().join("durable-core.db").exists());
        assert_eq!(status.refusals().count(), 0);
        assert_eq!(status.undecided().count(), 0);
        assert_eq!(
            deployment_summary(&status),
            "4 databases, 0 refusing, 0 undecided\n"
        );
    }

    /// The verdict a preflight returns is the refusal the open would have
    /// produced, reached before the wiring rather than by it — and a file it
    /// cannot read is reported as undecided rather than dressed up as a refusal.
    #[tokio::test]
    async fn an_unreadable_database_is_undecided_and_a_mismatch_is_the_refusal() {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("durable-core.db");
        std::fs::write(&path, b"not a SQLite database").expect("write junk");

        let database = verify_one_database(&path).await;
        assert!(matches!(
            database.verdict,
            StoreSchemaVerdict::Unreadable { .. }
        ));
        let StoreSchemaVerdict::Unreadable { reason } = &database.verdict else {
            panic!("junk bytes are undecidable, not refused")
        };
        assert!(
            !reason.is_empty(),
            "the probe reports the backend's own words"
        );
        assert!(
            !would_refuse(&database),
            "a refusal needs a version to name"
        );
        assert_eq!(
            database.expected,
            SqliteDatabase::DurableCore.expected_version()
        );
        assert_eq!(database.name, SqliteDatabase::DurableCore.name());
        assert_eq!(database.location, path.display().to_string());
        assert_eq!(SqliteDatabase::ProcessRegistry.name(), "process registry");
        assert_eq!(SqliteDatabase::Triggers.name(), "trigger store");
        assert_eq!(SqliteDatabase::EffectReplay.name(), "effect replay");

        // A mismatch is what a refusal looks like, and it names both numbers.
        let refused = StoreSchemaDatabase {
            name: SqliteDatabase::DurableCore.name().to_string(),
            location: path.display().to_string(),
            expected: SqliteDatabase::DurableCore.expected_version(),
            verdict: StoreSchemaVerdict::Mismatch {
                found: SqliteDatabase::DurableCore.expected_version() - 1,
            },
        };
        assert!(would_refuse(&refused));
        assert_eq!(
            refused.verdict,
            StoreSchemaVerdict::Mismatch {
                found: SqliteDatabase::DurableCore.expected_version() - 1
            }
        );
        let status = StoreSchemaStatus {
            databases: vec![refused],
        };
        assert_eq!(status.outcome(), StoreSchemaOutcome::Refused);
        assert_eq!(status.refusals().count(), 1);
        assert_eq!(status.databases.len(), 1);
        let rendered = status.to_string();
        assert!(rendered.contains("found version"), "{rendered}");
        // One database reads as one, not as "1 databases".
        assert_eq!(
            deployment_summary(&status),
            "1 database, 1 refusing, 0 undecided\n"
        );
    }

    /// The defect a boolean had: an undecided database is not a pass, and a host
    /// gating on "nothing refuses" booted against a store it could not read.
    #[tokio::test]
    async fn an_undecided_deployment_never_reports_itself_ready() {
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("durable-core.db");
        std::fs::write(&path, b"not a SQLite database").expect("write junk");

        let status = sqlite_deployment_status(root.path())
            .await
            .expect("read the deployment");

        assert_eq!(status.outcome(), StoreSchemaOutcome::Undecided);
        assert_eq!(status.refusals().count(), 0);
        assert_eq!(status.undecided().count(), 1);
        let undecided = status.undecided().next().expect("one undecided database");
        assert!(is_undecided(undecided));
        assert!(!would_refuse(undecided));
        assert_eq!(outcome_label(status.outcome()), "undecided");
        assert_eq!(undecided.name, SqliteDatabase::DurableCore.name());
        assert!(
            deployment_summary(&status).contains("1 undecided"),
            "{}",
            deployment_summary(&status)
        );

        // And a refusal alongside it outranks it without hiding it.
        let mut databases = status.databases.clone();
        databases.push(StoreSchemaDatabase {
            name: SqliteDatabase::ProcessRegistry.name().to_string(),
            location: "processes.db".to_string(),
            expected: SqliteDatabase::ProcessRegistry.expected_version(),
            verdict: StoreSchemaVerdict::Mismatch { found: 1 },
        });
        let mixed = StoreSchemaStatus { databases };
        assert_eq!(mixed.outcome(), StoreSchemaOutcome::Refused);
        assert_eq!(mixed.undecided().count(), 1);
        assert_eq!(outcome_label(mixed.outcome()), "refused");
    }

    /// The manifest is exhaustive over the durable formats this build writes,
    /// which is why a forward-only fence is listed with its own comparison shape
    /// rather than dropped for having one.
    #[test]
    fn a_forward_only_fence_is_reported_as_the_boundary_it_is() {
        let entry = durable_format(DurableFormat::SessionNodeBody)
            .expect("graph node bodies exist in every build");
        assert_eq!(entry.owning_crate, "lash-core");
        assert_eq!(entry.constant, "SESSION_NODE_BODY_SCHEMA_VERSION");
        assert_eq!(entry.probe, FormatProbe::Comparable);
        assert_eq!(
            entry.version,
            FormatVersion::ForwardOnly(lash::formats::SESSION_NODE_BODY_SCHEMA_VERSION)
        );
        assert_eq!(
            forward_only_generation(entry.version),
            Some(lash::formats::SESSION_NODE_BODY_SCHEMA_VERSION)
        );
        // A fence is not a counter, and an operator must not read it as one:
        // rolling back a generation is the supported direction here.
        assert_eq!(version_counter(entry.version), None);
        assert_eq!(forward_only_generation(FormatVersion::Counter(3)), None);
        assert_eq!(
            entry.version.to_string(),
            format!(
                "{} or older",
                lash::formats::SESSION_NODE_BODY_SCHEMA_VERSION
            )
        );
    }

    /// Only a version mismatch refuses an open. "Nothing provisioned yet" and
    /// "could not read it" are answers rather than failures, and a host that
    /// reads them as refusals refuses to boot against the empty deployment it
    /// was about to create.
    #[test]
    fn only_a_mismatch_refuses_and_the_other_verdicts_say_what_they_are() {
        assert!(StoreSchemaVerdict::Mismatch { found: 36 }.refuses_open());
        assert!(!StoreSchemaVerdict::Matches.refuses_open());
        assert!(!StoreSchemaVerdict::Absent.refuses_open());
        let reason = "disk I/O error".to_string();
        let undecided = StoreSchemaVerdict::Unreadable { reason };
        assert!(!undecided.refuses_open());
        // Not refused is not the same as fine.
        assert!(undecided.is_undecided());
        assert!(!StoreSchemaVerdict::Matches.is_undecided());
        assert!(!StoreSchemaVerdict::Absent.is_undecided());
        assert!(!StoreSchemaVerdict::Mismatch { found: 36 }.is_undecided());
        assert_eq!(
            StoreSchemaStatus {
                databases: Vec::new()
            }
            .outcome(),
            StoreSchemaOutcome::Ready
        );
    }

    /// A PostgreSQL report header identifies the deployment without carrying
    /// the credentials the configuration held.
    #[tokio::test]
    async fn a_postgres_preflight_reports_a_redacted_target() {
        let location =
            postgres_preflight_target("postgres://lash:hunter2@db.internal:5432/lash").await;
        assert!(!location.contains("hunter2"), "{location}");
        assert!(!location.contains("lash:"), "{location}");
        assert!(location.contains("db.internal"), "{location}");

        let backend = StoreBackend::Postgres {
            location: location.clone(),
        };
        assert_eq!(backend_location(&backend), Some(location.as_str()));
        assert_eq!(backend_label(&backend), format!("postgres ({location})"));
    }

    /// A deployment with nothing durable behind it still answers, and says
    /// which backend answered.
    #[test]
    fn an_in_memory_deployment_identifies_itself() {
        assert_eq!(backend_label(&StoreBackend::InMemory), "in-memory");
        assert_eq!(backend_location(&StoreBackend::InMemory), None);
        let sqlite = StoreBackend::Sqlite {
            location: "/srv/lash/durable-core.db".to_string(),
        };
        assert_eq!(backend_location(&sqlite), Some("/srv/lash/durable-core.db"));
        assert_eq!(backend_label(&sqlite), "sqlite (/srv/lash/durable-core.db)");
    }

    /// A host boots against `&dyn StorePreflight`, so the backend it happens to
    /// run is not baked into the boot path.
    #[tokio::test]
    async fn a_host_probes_through_the_trait_object() {
        let root = tempfile::tempdir().expect("temp dir");
        let handle: Arc<dyn StorePreflight> = Arc::new(SqliteStorePreflight::for_durable_core(
            root.path().join("durable-core.db"),
        ));
        let report = boot_report(handle.as_ref())
            .await
            .expect("read the deployment");
        assert!(report.starts_with("backend: sqlite ("), "{report}");
        assert!(report.contains("outcome: ready"), "{report}");
        assert!(
            report.contains("1 database, 0 refusing, 0 undecided"),
            "{report}"
        );
        assert!(matches!(handle.backend(), StoreBackend::Sqlite { .. }));
        assert_eq!(
            backend_location(&handle.backend()),
            Some(
                root.path()
                    .join("durable-core.db")
                    .display()
                    .to_string()
                    .as_str()
            )
        );
    }
}
