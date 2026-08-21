//! The read-only store surface a host may inspect *before* it wires anything.
//!
//! Every durable format lash writes fails closed at a version boundary (ADR
//! 0055/0061/0064): there are no migration decoders, so a store written by
//! another build is refused rather than read. That policy is right, but the
//! only way to ask the question used to be to boot, wire the runtime, and let
//! the first store access answer it — which under a supervisor is a crash
//! loop rather than a diagnosis.
//!
//! [`StorePreflight`] is the surface that answers it first. It is deliberately
//! *not* implemented on a wired store, because constructing one is itself the
//! side-effectful act this exists to precede: PostgreSQL takes an exclusive
//! advisory lock and may run creation DDL, an explicit migration, a
//! signing-secret precondition and schema-gate telemetry; SQLite takes the
//! write lock and applies its schema batch. A probe reachable only through a
//! successful open could not describe the deployments worth describing.
//!
//! Backends therefore implement this on a dedicated read-only handle built
//! from raw connection configuration — a database URL, a filesystem path — and
//! the trait carries no mutating method by construction. Nothing here marks,
//! migrates, condemns, or deletes; disposal stays with the operator and the
//! published drain/recreate procedures.
//!
//! The trait answers *what the store says about itself*. It deliberately does
//! not decode durable payloads: which version a component expects, and whether
//! a found version is readable, is one build-wide question that belongs with
//! the format manifest rather than duplicated per backend.

use async_trait::async_trait;

use super::error::StoreError;

/// Which durable backend a preflight handle reads.
///
/// Reported verbatim in the preflight report header so an operator can tell,
/// from the report alone, which deployment was inspected.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreBackend {
    /// A SQLite deployment, identified by the directory or file it was built
    /// from.
    Sqlite {
        /// The path the handle was constructed from, as the host wrote it.
        location: String,
    },
    /// A PostgreSQL deployment, identified by a redacted connection target.
    ///
    /// Backends must not place credentials here: a preflight report is
    /// operator-facing output that ends up in logs and tickets.
    Postgres {
        /// Host and database name, never the password.
        location: String,
    },
    /// A process-lifetime store with nothing durable behind it.
    InMemory,
}

impl std::fmt::Display for StoreBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreBackend::Sqlite { location } => write!(f, "sqlite ({location})"),
            StoreBackend::Postgres { location } => write!(f, "postgres ({location})"),
            StoreBackend::InMemory => write!(f, "in-memory"),
        }
    }
}

/// What one schema-carrying database said when it was read.
///
/// `Absent` and `Unreadable` are first-class rather than errors: a preflight
/// that aborted on the first unopenable database could not report the ones
/// behind it, and "there is nothing here yet" is a perfectly good answer to
/// "will this open?".
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreSchemaVerdict {
    /// The found version equals the version this build expects.
    Matches,
    /// A version was read and it is not the expected one. This is the refusal
    /// a host would otherwise have discovered at open.
    Mismatch {
        /// The version stamped in the store.
        found: i64,
    },
    /// Nothing is provisioned yet, so the next open would create it. Not a
    /// refusal.
    Absent,
    /// The database exists but could not be read far enough to decide.
    ///
    /// Carries the backend's own words; the probe classifies it as undecided
    /// rather than guessing a verdict from a failure it does not model.
    Unreadable {
        /// The backend's diagnostic, verbatim.
        reason: String,
    },
}

impl StoreSchemaVerdict {
    /// Whether this verdict alone would refuse an open.
    ///
    /// [`StoreSchemaVerdict::Unreadable`] is *not* a refusal: an unreadable
    /// database is an undecided one, and reporting it as refused would put a
    /// verdict behind evidence the probe does not have. It is not a pass
    /// either — see [`StoreSchemaVerdict::is_undecided`].
    pub fn refuses_open(&self) -> bool {
        matches!(self, StoreSchemaVerdict::Mismatch { .. })
    }

    /// Whether the probe could not decide this database either way.
    ///
    /// The counterpart to [`StoreSchemaVerdict::refuses_open`], and the reason
    /// there is no single "is it fine?" boolean: a caller that only asked about
    /// refusals would read an undecided database as a pass and boot on evidence
    /// nobody has. An open may still fail here — PostgreSQL's structural gate
    /// refuses drift this verdict carries verbatim — so the honest instruction
    /// is to investigate, not to start.
    pub fn is_undecided(&self) -> bool {
        matches!(self, StoreSchemaVerdict::Unreadable { .. })
    }
}

/// What a whole deployment's schema status means for a host deciding to boot.
///
/// Replaces the conformant/non-conformant boolean this surface used to carry,
/// because two outcomes cannot describe three answers: an undecided database is
/// neither a refusal nor a pass, and a boolean has to call it one of them.
/// [`StoreSchemaOutcome::Refused`] wins over
/// [`StoreSchemaOutcome::Undecided`] when both are present — a known refusal is
/// the decisive fact, and a caller that wants both reads
/// [`StoreSchemaStatus::refusals`] and [`StoreSchemaStatus::undecided`]. What no
/// combination produces is [`StoreSchemaOutcome::Ready`] over an undecided
/// database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreSchemaOutcome {
    /// Every database answered, and every answer matched. Nothing here refuses
    /// an open on its recorded version.
    Ready,
    /// At least one database's recorded version is one this build refuses.
    /// Booting produces exactly that refusal, later and less legibly.
    Refused,
    /// Nothing refuses on a version, but at least one database could not be
    /// read far enough to decide. Investigate before starting: the evidence for
    /// a pass is missing, not present.
    Undecided,
}

/// One schema-carrying database inside a deployment, and its verdict.
///
/// A SQLite deployment has several — durable core, process registry, triggers,
/// effect replay — that version independently and can disagree; PostgreSQL has
/// one component stamp. Reporting them individually is what lets a refusal
/// name the database that refused rather than the deployment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreSchemaDatabase {
    /// Operator-facing name of the database, e.g. `durable core`.
    pub name: String,
    /// Where the bytes live: a file path, or a table namespace.
    pub location: String,
    /// The version this build requires.
    pub expected: i64,
    /// What was read, and what that means.
    pub verdict: StoreSchemaVerdict,
}

/// Every schema-carrying database in one deployment, read without opening it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreSchemaStatus {
    /// The databases, in the order the backend would open them.
    pub databases: Vec<StoreSchemaDatabase>,
}

impl StoreSchemaStatus {
    /// What this deployment means for a host deciding to boot.
    ///
    /// An empty status is [`StoreSchemaOutcome::Ready`]: a deployment with
    /// nothing provisioned has nothing that refuses and nothing undecided.
    pub fn outcome(&self) -> StoreSchemaOutcome {
        if self.refusals().next().is_some() {
            StoreSchemaOutcome::Refused
        } else if self.undecided().next().is_some() {
            StoreSchemaOutcome::Undecided
        } else {
            StoreSchemaOutcome::Ready
        }
    }

    /// The databases whose recorded version would refuse an open, in order.
    pub fn refusals(&self) -> impl Iterator<Item = &StoreSchemaDatabase> {
        self.databases
            .iter()
            .filter(|database| database.verdict.refuses_open())
    }

    /// The databases the probe could not decide, in order.
    ///
    /// Reported separately from [`StoreSchemaStatus::refusals`] so a host can
    /// say which it is holding: a refusal names a version, an undecided
    /// database names a reason the read stopped.
    pub fn undecided(&self) -> impl Iterator<Item = &StoreSchemaDatabase> {
        self.databases
            .iter()
            .filter(|database| database.verdict.is_undecided())
    }
}

impl std::fmt::Display for StoreSchemaStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for database in &self.databases {
            match &database.verdict {
                StoreSchemaVerdict::Matches => {
                    writeln!(f, "{}: version {} (ok)", database.name, database.expected)?
                }
                StoreSchemaVerdict::Mismatch { found } => writeln!(
                    f,
                    "{}: found version {found}, expected {} at {}",
                    database.name, database.expected, database.location
                )?,
                StoreSchemaVerdict::Absent => writeln!(
                    f,
                    "{}: not provisioned at {} (would be created)",
                    database.name, database.location
                )?,
                StoreSchemaVerdict::Unreadable { reason } => writeln!(
                    f,
                    "{}: unreadable at {}: {reason}",
                    database.name, database.location
                )?,
            }
        }
        Ok(())
    }
}

/// A place durable payloads live, enumerable one page at a time.
///
/// The surfaces are chosen so that one walk answers both questions the
/// preflight exists for: which durable *formats* would refuse to open, and
/// which durable *identities* carry them. They are not a table listing — a
/// backend is free to assemble a surface from however many of its own tables it
/// takes, and two backends that store the same payload differently still answer
/// the same surface.
///
/// Every surface yields the payload's **logical** bytes. A backend that wraps
/// stored bytes in a compression or envelope frame of its own unwraps it here,
/// because that frame is storage bookkeeping rather than a durable format any
/// version manifest describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum DurableSurface {
    /// One persisted JSON module artifact per content-addressed module ref.
    ModuleArtifact,
    /// One parked segment-handover envelope per non-terminal process that has
    /// one: the durable continuation a process resumes from.
    ParkedSegment,
    /// One undelivered wake payload per pending delivery.
    PendingWake,
    /// One checkpoint manifest per session that has published a checkpoint
    /// root.
    SessionCheckpoint,
    /// One protocol execution-state component body per session that stores one.
    SessionExecutionState,
}

impl DurableSurface {
    /// The operator-facing name used in reports.
    pub fn name(self) -> &'static str {
        match self {
            DurableSurface::ModuleArtifact => "module artifacts",
            DurableSurface::ParkedSegment => "parked segments",
            DurableSurface::PendingWake => "pending wakes",
            DurableSurface::SessionCheckpoint => "session checkpoints",
            DurableSurface::SessionExecutionState => "session execution state",
        }
    }

    /// Whether reading this surface costs a per-session blob walk.
    ///
    /// The split is what makes a summary mode honest rather than arbitrary: the
    /// cheap surfaces are bounded by the process registry, the deep ones are
    /// bounded by the session count and each costs at least one blob read per
    /// session.
    pub fn is_deep(self) -> bool {
        matches!(
            self,
            DurableSurface::SessionCheckpoint | DurableSurface::SessionExecutionState
        )
    }
}

/// How one item's logical bytes are framed, so a reader knows how to look at
/// them without guessing.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DurablePayload {
    /// UTF-8 JSON text.
    Json(String),
    /// MessagePack bytes.
    MessagePack(Vec<u8>),
    /// The item exists and is named, but its bytes could not be fetched.
    ///
    /// First-class rather than an error, for the same reason
    /// [`StoreSchemaVerdict::Unreadable`] is: a walk that aborted on the first
    /// dangling reference could not report the thousand items behind it, and
    /// "this one is unreadable" is a finding worth reporting.
    Missing {
        /// The backend's diagnostic, verbatim.
        reason: String,
    },
}

/// One durable payload, with enough identity to appear on a drain list.
///
/// The identity fields are what turn a readability answer into an actionable
/// one: "three segments refuse" is a fact an operator cannot act on, and
/// "process `p-7` in session `s-2` refuses" is a row in a drain list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurableItem {
    /// Which surface produced this item.
    pub surface: DurableSurface,
    /// The keyset key for resuming after this item. Opaque to the caller and
    /// meaningful only to the backend that minted it.
    pub cursor: String,
    /// The process this payload belongs to, when it belongs to one.
    pub process_id: Option<String>,
    /// The session this payload belongs to, when it belongs to one.
    pub session_id: Option<String>,
    /// The store's own status word for the owner, e.g. `waiting`. Reported
    /// verbatim so an operator reads the store's vocabulary rather than a
    /// translation of it.
    pub status: Option<String>,
    /// The store's record for the item's owner, when the surface has one.
    ///
    /// Carried because an identity-only format cannot be read out of the
    /// payload at all: the only way to decide whether a stored program identity
    /// is this build's is to recompute it from the inputs the owner records,
    /// and only the store holds those.
    pub owner_record: Option<String>,
    /// The payload's logical bytes.
    pub payload: DurablePayload,
}

/// One page of a surface walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurableScanPage {
    /// The items, in cursor order.
    pub items: Vec<DurableItem>,
    /// The cursor to resume after, or `None` when the surface is exhausted.
    pub next: Option<String>,
    /// Whether the surface was walked at all.
    pub coverage: ScanCoverage,
}

/// Whether a surface was walked, and if not, why not.
///
/// A backend that cannot enumerate a surface says so instead of returning an
/// empty page, because an empty page and an unwalked surface are the two
/// answers a preflight must never confuse: one says "nothing here refuses", the
/// other says "nobody looked".
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScanCoverage {
    /// The surface was read, and the page is what it holds.
    Scanned,
    /// The surface was not read. The reason is operator-facing and appears in
    /// the report's "not scanned" list verbatim.
    NotScanned {
        /// Why this backend did not walk the surface.
        reason: String,
    },
}

/// A request for one page of one surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurableScan {
    /// Which surface to walk.
    pub surface: DurableSurface,
    /// Resume strictly after this cursor, or start at the beginning.
    pub after: Option<String>,
    /// The most items to return. A backend may return fewer; it must not
    /// return more, because an unbounded page is how a preflight over a large
    /// deployment becomes the outage it was meant to prevent.
    pub limit: usize,
}

impl DurableScan {
    /// The first page of a surface.
    pub fn first(surface: DurableSurface, limit: usize) -> Self {
        Self {
            surface,
            after: None,
            limit,
        }
    }

    /// The page after `cursor`.
    pub fn after(surface: DurableSurface, cursor: impl Into<String>, limit: usize) -> Self {
        Self {
            surface,
            after: Some(cursor.into()),
            limit,
        }
    }
}

/// A read-only handle a host may inspect before wiring a runtime.
///
/// Implemented on a backend's dedicated preflight handle, never on the wired
/// store — see the module documentation for why the distinction is the whole
/// point. Every method reads; the trait has no mutating method, and an
/// implementation that writes is a contract violation rather than an
/// optimisation.
///
/// A host consumes it as `&dyn StorePreflight` and pairs the schema answer with
/// the facade's `formats` manifest, which owns the build-wide format versions
/// this trait deliberately does not report.
#[async_trait]
pub trait StorePreflight: Send + Sync {
    /// Which deployment this handle reads, for the report header.
    fn backend(&self) -> StoreBackend;

    /// Read every schema-carrying database's recorded version.
    ///
    /// Implementations must not take a write lock, create a database, apply
    /// DDL, or stamp a version — a probe that provisions the thing it was
    /// asked about has answered a different question. Failing to *read* is
    /// reported per database as [`StoreSchemaVerdict::Unreadable`]; the
    /// `Result` is for failures that leave nothing to report at all, such as
    /// an unreachable server.
    async fn schema_status(&self) -> Result<StoreSchemaStatus, StoreError>;

    /// Read one page of one durable surface.
    ///
    /// The same read-only obligations as [`StorePreflight::schema_status`]
    /// apply, and one more: a walk must not decode the payloads it returns.
    /// Deciding whether stored bytes open under this build is one build-wide
    /// question that belongs with the format manifest, and a backend that
    /// answered it locally would be a second place for the answer to drift.
    ///
    /// The default returns [`ScanCoverage::NotScanned`]. That is the honest
    /// answer for a backend that has not implemented the walk — reporting an
    /// empty page instead would let a host read "nothing refuses" out of a
    /// surface nobody read.
    async fn scan_durable(&self, scan: &DurableScan) -> Result<DurableScanPage, StoreError> {
        Ok(DurableScanPage {
            items: Vec::new(),
            next: None,
            coverage: ScanCoverage::NotScanned {
                reason: format!(
                    "the {} backend does not enumerate {}",
                    self.backend(),
                    scan.surface.name()
                ),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database(name: &str, verdict: StoreSchemaVerdict) -> StoreSchemaDatabase {
        StoreSchemaDatabase {
            name: name.to_string(),
            location: format!("/tmp/{name}.db"),
            expected: 37,
            verdict,
        }
    }

    #[test]
    fn only_a_version_mismatch_refuses_an_open() {
        assert!(StoreSchemaVerdict::Mismatch { found: 36 }.refuses_open());
        assert!(!StoreSchemaVerdict::Matches.refuses_open());
        assert!(!StoreSchemaVerdict::Absent.refuses_open());
        assert!(
            !StoreSchemaVerdict::Unreadable {
                reason: "disk I/O error".to_string(),
            }
            .refuses_open(),
            "an unreadable database is undecided, not refused"
        );
    }

    #[test]
    fn an_undecided_verdict_is_neither_a_refusal_nor_a_pass() {
        let undecided = StoreSchemaVerdict::Unreadable {
            reason: "permission denied".to_string(),
        };
        assert!(undecided.is_undecided());
        assert!(!undecided.refuses_open());
        for decided in [
            StoreSchemaVerdict::Matches,
            StoreSchemaVerdict::Absent,
            StoreSchemaVerdict::Mismatch { found: 1 },
        ] {
            assert!(!decided.is_undecided(), "{decided:?}");
        }
    }

    #[test]
    fn an_undecided_database_never_reads_as_ready() {
        // The defect the boolean had: nothing refuses, so "is it fine?" said
        // yes over a database nobody could read.
        let status = StoreSchemaStatus {
            databases: vec![
                database("durable core", StoreSchemaVerdict::Matches),
                database(
                    "effect replay",
                    StoreSchemaVerdict::Unreadable {
                        reason: "file is not a database".to_string(),
                    },
                ),
            ],
        };
        assert_eq!(status.outcome(), StoreSchemaOutcome::Undecided);
        assert_eq!(status.undecided().count(), 1);
        assert_eq!(status.refusals().count(), 0);
    }

    #[test]
    fn a_refusal_outranks_an_undecided_database_without_hiding_it() {
        let status = StoreSchemaStatus {
            databases: vec![
                database("durable core", StoreSchemaVerdict::Mismatch { found: 36 }),
                database(
                    "effect replay",
                    StoreSchemaVerdict::Unreadable {
                        reason: "file is not a database".to_string(),
                    },
                ),
            ],
        };
        assert_eq!(status.outcome(), StoreSchemaOutcome::Refused);
        assert_eq!(status.refusals().count(), 1);
        assert_eq!(
            status.undecided().count(),
            1,
            "the undecided database stays reportable behind the refusal"
        );
    }

    #[test]
    fn conformance_names_every_refusing_database_and_only_those() {
        let status = StoreSchemaStatus {
            databases: vec![
                database("durable core", StoreSchemaVerdict::Matches),
                database(
                    "process registry",
                    StoreSchemaVerdict::Mismatch { found: 23 },
                ),
                database("triggers", StoreSchemaVerdict::Absent),
                database(
                    "effect replay",
                    StoreSchemaVerdict::Unreadable {
                        reason: "file is not a database".to_string(),
                    },
                ),
            ],
        };

        assert_eq!(status.outcome(), StoreSchemaOutcome::Refused);
        let refused: Vec<&str> = status
            .refusals()
            .map(|database| database.name.as_str())
            .collect();
        assert_eq!(refused, vec!["process registry"]);
    }

    #[test]
    fn an_empty_deployment_is_ready() {
        let status = StoreSchemaStatus {
            databases: Vec::new(),
        };
        assert_eq!(status.outcome(), StoreSchemaOutcome::Ready);
        assert_eq!(status.refusals().count(), 0);
        assert_eq!(status.undecided().count(), 0);
    }

    #[test]
    fn rendering_names_the_found_and_expected_versions() {
        let status = StoreSchemaStatus {
            databases: vec![database(
                "durable core",
                StoreSchemaVerdict::Mismatch { found: 36 },
            )],
        };
        let rendered = status.to_string();
        assert!(rendered.contains("found version 36"), "{rendered}");
        assert!(rendered.contains("expected 37"), "{rendered}");
    }
}
