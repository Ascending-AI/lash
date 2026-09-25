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

use crate::ProcessId;
use crate::SessionId;
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
}

impl std::fmt::Display for StoreBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreBackend::Sqlite { location } => write!(f, "sqlite ({location})"),
            StoreBackend::Postgres { location } => write!(f, "postgres ({location})"),
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
    /// A version was read that the next open will migrate to the expected
    /// version. The preflight itself remains read-only and performs no DDL.
    Migratable {
        /// The version stamped in the store before migration.
        found: i64,
    },
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

/// One schema-carrying component and the version it was stamped at.
///
/// Carried inside a release stamp rather than derived at read time: the point
/// of the stamp is to say what the *writing* build required, and a tuple
/// recomputed by the reading build would say what the reader requires instead.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StoreComponentVersion {
    /// The component's operator-facing name, e.g. `durable core`. Never
    /// contains `;` or `=`, which delimit the durable encoding.
    pub component: String,
    /// The version that component required when the stamp was written.
    pub version: i64,
}

/// Which lash release wrote a durable store, and when.
///
/// The release string is the writing build's crate version. On `main` that is
/// the honest `0.0.0-dev` placeholder every workspace manifest carries; a
/// released build carries the version the release workflow stamped into its
/// ephemeral checkout, so the constant *is* the release-time injection rather
/// than a substitute for one (see `scripts/release_version.py`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreReleaseStamp {
    /// The writing build's crate version.
    pub release: String,
    /// What each schema-carrying component required when the stamp was
    /// written, in component order.
    pub schema_versions: Vec<StoreComponentVersion>,
    /// Wall-clock time the stamp was written, in epoch milliseconds.
    ///
    /// The instant this *release* first wrote the store, not the instant of
    /// the most recent open: a reopen under the same release leaves the stamp
    /// alone, so the field answers "since when has this release owned these
    /// bytes?".
    pub written_at_epoch_ms: i64,
}

impl StoreReleaseStamp {
    /// The durable encoding of [`StoreReleaseStamp::schema_versions`].
    ///
    /// Deliberately not serde: the store contract carries no derives, and the
    /// two SQL backends must agree on one byte-for-byte text so the shared
    /// conformance law can compare them. Components are joined with `;` and
    /// each is `name=version`.
    pub fn encode_schema_versions(versions: &[StoreComponentVersion]) -> String {
        versions
            .iter()
            .map(|version| format!("{}={}", version.component, version.version))
            .collect::<Vec<_>>()
            .join(";")
    }

    /// `None` for text this build cannot read, which a caller reports as
    /// [`StoreReleaseState::Unreadable`] rather than as an empty tuple: a
    /// stamp nobody could parse is not a stamp that recorded nothing.
    pub fn decode_schema_versions(encoded: &str) -> Option<Vec<StoreComponentVersion>> {
        if encoded.is_empty() {
            return Some(Vec::new());
        }
        encoded
            .split(';')
            .map(|entry| {
                let (component, version) = entry.rsplit_once('=')?;
                if component.is_empty() {
                    return None;
                }
                Some(StoreComponentVersion {
                    component: component.to_string(),
                    version: version.parse().ok()?,
                })
            })
            .collect()
    }
}

/// What a store said about the release that wrote it.
///
/// Three answers rather than an `Option`, for the reason
/// [`StoreSchemaVerdict::Unreadable`] exists: a store that carries no stamp and
/// a stamp that could not be read are different findings, and collapsing them
/// into "absent" would report an unread stamp as an observed absence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum StoreReleaseState {
    /// The store records which release wrote it.
    Stamped(StoreReleaseStamp),
    /// The store is readable and records no release. Either nothing has opened
    /// it yet, or it was last written by a build older than the stamp itself.
    ///
    /// This is the default because a handle that has read nothing has observed
    /// no stamp, and the absence is the honest starting point.
    #[default]
    Unstamped,
    /// The stamp could not be read. Carries the backend's own words.
    Unreadable {
        /// The backend's diagnostic, verbatim.
        reason: String,
    },
}

impl StoreReleaseState {
    /// The writing release, when one was read.
    pub fn release(&self) -> Option<&str> {
        match self {
            StoreReleaseState::Stamped(stamp) => Some(stamp.release.as_str()),
            StoreReleaseState::Unstamped | StoreReleaseState::Unreadable { .. } => None,
        }
    }
}

impl std::fmt::Display for StoreReleaseState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreReleaseState::Stamped(stamp) => write!(
                f,
                "written by lash release {} at epoch ms {} ({})",
                stamp.release,
                stamp.written_at_epoch_ms,
                StoreReleaseStamp::encode_schema_versions(&stamp.schema_versions)
            ),
            StoreReleaseState::Unstamped => {
                write!(f, "no release stamp (nothing has stamped this store)")
            }
            StoreReleaseState::Unreadable { reason } => {
                write!(f, "release stamp unreadable: {reason}")
            }
        }
    }
}

/// Precedence of two release strings, or `None` when either is not a version
/// this build can order.
///
/// Semantic-version precedence over the two shapes `scripts/release_version.py`
/// can produce — `X.Y.Z` and `X.Y.Z-prerelease` — implemented here rather than
/// taken as a dependency, because the whole comparison is the update rule and
/// it has to be readable next to it.
pub fn compare_releases(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    let left = parse_release(left)?;
    let right = parse_release(right)?;
    Some(left.cmp(&right))
}

/// Whether `candidate` replaces `existing` as the store's release stamp.
///
/// The update rule, in one place both SQL backends call: a strictly newer
/// release advances the stamp, and nothing else touches it. A store written by
/// a newer release than the one opening it keeps the newer stamp — that is the
/// "never downgraded" half, and it is what keeps the stamp answering "which
/// release wrote these bytes" rather than "which build last booted". Two
/// releases this build cannot order leave the existing stamp in place, because
/// an unorderable pair is not evidence that the candidate is newer.
pub fn release_stamp_advances(existing: &str, candidate: &str) -> bool {
    compare_releases(existing, candidate) == Some(std::cmp::Ordering::Less)
}

/// A release parsed into the tuple semantic-version precedence orders on.
///
/// A release with no pre-release sorts above every pre-release of the same
/// core version, which the `Option` ordering would invert, so the presence
/// flag is carried as a `bool` that sorts `false` (a pre-release) first.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct ParsedRelease {
    core: (u64, u64, u64),
    is_release: bool,
    prerelease: Vec<PrereleaseIdentifier>,
}

/// One dot-separated pre-release identifier. Numeric identifiers sort below
/// alphanumeric ones and compare numerically; alphanumeric ones compare as
/// ASCII.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum PrereleaseIdentifier {
    Numeric(u64),
    Alphanumeric(String),
}

fn parse_release(release: &str) -> Option<ParsedRelease> {
    let (core, prerelease) = match release.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (release, None),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let identifiers = match prerelease {
        None => Vec::new(),
        Some(prerelease) => {
            if prerelease.is_empty() {
                return None;
            }
            prerelease
                .split('.')
                .map(|identifier| {
                    if identifier.is_empty() {
                        return None;
                    }
                    if identifier.chars().all(|c| c.is_ascii_digit()) {
                        identifier.parse().ok().map(PrereleaseIdentifier::Numeric)
                    } else {
                        Some(PrereleaseIdentifier::Alphanumeric(identifier.to_string()))
                    }
                })
                .collect::<Option<Vec<_>>>()?
        }
    };
    Some(ParsedRelease {
        core: (major, minor, patch),
        is_release: identifiers.is_empty(),
        prerelease: identifiers,
    })
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
    /// Which lash release wrote this store, when the store records one.
    ///
    /// The schema integers above say what this build requires; they never say
    /// which build produced the data, which is the question a host upgrading
    /// crate versions actually has. It rides on the same report because it is
    /// read on the same read-only pass.
    pub release: StoreReleaseState,
    /// The durable-format generation the store's writers emit, when the store
    /// records one (ADR 0106 §1 `F`).
    ///
    /// A row that reads back here is the one the schema-open transaction
    /// seeded and durable writers consult for their writer version; a
    /// deployment opened only by builds that predate the row reports
    /// [`super::FleetFormatState::Unrecorded`].
    pub fleet_format: super::FleetFormatState,
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
        writeln!(f, "release: {}", self.release)?;
        writeln!(f, "fleet format: {}", self.fleet_format)?;
        for database in &self.databases {
            match &database.verdict {
                StoreSchemaVerdict::Matches => {
                    writeln!(f, "{}: version {} (ok)", database.name, database.expected)?
                }
                StoreSchemaVerdict::Migratable { found } => writeln!(
                    f,
                    "{}: found version {found}, migrates to {} on open at {}",
                    database.name, database.expected, database.location
                )?,
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
    /// One start record per non-terminal process that has started: the
    /// executable generation its incarnation runs under (FIG-3571), which a
    /// first-segment process carries with no parked handover to read it from.
    StartedProcess,
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
            DurableSurface::StartedProcess => "started processes",
            DurableSurface::SessionCheckpoint => "session checkpoints",
            DurableSurface::SessionExecutionState => "session execution state",
        }
    }

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
    pub process_id: Option<ProcessId>,
    /// The session this payload belongs to, when it belongs to one.
    pub session_id: Option<SessionId>,
    /// The store's own status word for the owner, e.g.
    /// Reported verbatim so an operator reads the store's vocabulary rather than a translation
    /// of it.
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
    use super::super::FleetFormatState;
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
        assert!(!StoreSchemaVerdict::Migratable { found: 36 }.refuses_open());
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
            StoreSchemaVerdict::Migratable { found: 1 },
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
            release: StoreReleaseState::Unstamped,
            fleet_format: FleetFormatState::Unrecorded,
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
            release: StoreReleaseState::Unstamped,
            fleet_format: FleetFormatState::Unrecorded,
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
            release: StoreReleaseState::Unstamped,
            fleet_format: FleetFormatState::Unrecorded,
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
            release: StoreReleaseState::Unstamped,
            fleet_format: FleetFormatState::Unrecorded,
            databases: Vec::new(),
        };
        assert_eq!(status.outcome(), StoreSchemaOutcome::Ready);
        assert_eq!(status.refusals().count(), 0);
        assert_eq!(status.undecided().count(), 0);
    }

    fn stamp(release: &str) -> StoreReleaseStamp {
        StoreReleaseStamp {
            release: release.to_string(),
            schema_versions: vec![StoreComponentVersion {
                component: "durable core".to_string(),
                version: 66,
            }],
            written_at_epoch_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn a_newer_release_advances_the_stamp_and_nothing_else_does() {
        assert!(release_stamp_advances("0.1.0", "0.2.0"));
        assert!(release_stamp_advances("0.1.0-alpha.1", "0.1.0"));
        assert!(release_stamp_advances("0.0.0-dev", "0.1.0"));
        assert!(
            !release_stamp_advances("0.2.0", "0.1.0"),
            "an older release must never downgrade the stamp"
        );
        assert!(
            !release_stamp_advances("0.1.0", "0.1.0"),
            "a reopen under the same release leaves the written-at instant alone"
        );
        assert!(
            !release_stamp_advances("0.1.0", "not-a-version"),
            "an unorderable candidate is not evidence that it is newer"
        );
        assert!(
            !release_stamp_advances("garbage", "0.1.0"),
            "an unorderable existing stamp is left for an operator to read"
        );
    }

    #[test]
    fn prerelease_identifiers_order_the_way_semver_precedence_does() {
        assert_eq!(
            compare_releases("0.1.0-alpha.2", "0.1.0-alpha.10"),
            Some(std::cmp::Ordering::Less),
            "numeric identifiers compare numerically, not as text"
        );
        assert_eq!(
            compare_releases("0.1.0-alpha", "0.1.0-beta"),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(
            compare_releases("0.1.0-alpha", "0.1.0"),
            Some(std::cmp::Ordering::Less),
            "a release outranks every pre-release of the same core version"
        );
        assert_eq!(
            compare_releases("0.1.0", "1.0.0-x"),
            Some(std::cmp::Ordering::Less)
        );
        assert_eq!(compare_releases("0.1.0", "0.1"), None);
        assert_eq!(compare_releases("0.1.0.1", "0.1.0"), None);
        assert_eq!(compare_releases("0.1.0-", "0.1.0"), None);
    }

    #[test]
    fn the_schema_tuple_survives_the_durable_encoding() {
        let versions = vec![
            StoreComponentVersion {
                component: "durable core".to_string(),
                version: 66,
            },
            StoreComponentVersion {
                component: "process registry".to_string(),
                version: 38,
            },
        ];
        let encoded = StoreReleaseStamp::encode_schema_versions(&versions);
        assert_eq!(encoded, "durable core=66;process registry=38");
        assert_eq!(
            StoreReleaseStamp::decode_schema_versions(&encoded),
            Some(versions)
        );
        assert_eq!(
            StoreReleaseStamp::decode_schema_versions(""),
            Some(Vec::new())
        );
        assert_eq!(
            StoreReleaseStamp::decode_schema_versions("durable core"),
            None
        );
        assert_eq!(StoreReleaseStamp::decode_schema_versions("=5"), None);
        assert_eq!(
            StoreReleaseStamp::decode_schema_versions("durable core=x"),
            None,
            "a tuple this build cannot read is unreadable, not empty"
        );
    }

    #[test]
    fn an_unstamped_store_is_reported_as_an_absence_not_a_blank_release() {
        let unstamped = StoreReleaseState::Unstamped;
        assert_eq!(unstamped.release(), None);
        let unreadable = StoreReleaseState::Unreadable {
            reason: "no such table: release_stamp".to_string(),
        };
        assert_eq!(unreadable.release(), None);
        assert_ne!(
            unstamped, unreadable,
            "an unread stamp is a different finding from an absent one"
        );
        assert_eq!(
            StoreReleaseState::Stamped(stamp("0.4.1")).release(),
            Some("0.4.1")
        );
    }

    #[test]
    fn the_report_names_the_writing_release() {
        let status = StoreSchemaStatus {
            release: StoreReleaseState::Stamped(stamp("0.4.1")),
            fleet_format: FleetFormatState::Unrecorded,
            databases: vec![database("durable core", StoreSchemaVerdict::Matches)],
        };
        let rendered = status.to_string();
        assert!(
            rendered.contains("written by lash release 0.4.1"),
            "{rendered}"
        );
        assert!(rendered.contains("durable core=66"), "{rendered}");

        let unstamped = StoreSchemaStatus {
            release: StoreReleaseState::Unstamped,
            fleet_format: FleetFormatState::Unrecorded,
            databases: Vec::new(),
        };
        assert!(
            unstamped.to_string().contains("no release stamp"),
            "{unstamped}"
        );
    }

    #[test]
    fn rendering_names_the_found_and_expected_versions() {
        let status = StoreSchemaStatus {
            release: StoreReleaseState::Unstamped,
            fleet_format: FleetFormatState::Unrecorded,
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
