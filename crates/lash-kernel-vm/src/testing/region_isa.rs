//! Independent qualification for the prefix-exact region ISA, FIG-7634.
//!
//! These diagnostics never authorize execution. Missing observations are errors,
//! including on a build where only the declaration or fixture lanes exist.
use crate::{Bounds, CellStart, Program};
use lash_kernel_state::paged::PageSource;

/// An admitted source run and a repeatable initial page factory.
pub struct QualificationCase {
    program: Program,
    input: CellStart,
    bounds: Bounds,
    initial: fn() -> Box<dyn PageSource>,
    schedule: Box<[(u64, Interruption)]>,
}

impl QualificationCase {
    /// Source admission, never a compiler/verifier summary.
    pub fn program(&self) -> &Program {
        &self.program
    }
    /// Actual entry values and identities.
    pub fn input(&self) -> &CellStart {
        &self.input
    }
    /// Exact source limits.
    pub fn bounds(&self) -> &Bounds {
        &self.bounds
    }
    /// A fresh reader for each independently executed run.
    pub fn initial(&self) -> Box<dyn PageSource> {
        (self.initial)()
    }
    /// Prefix ordinals include repeated occurrences of the same source site.
    pub fn schedule(&self) -> &[(u64, Interruption)] {
        &self.schedule
    }
}

/// Identity domains must not alias each other during canonical comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IdentityKind {
    Entity,
    Invocation,
    Occurrence,
    Provenance,
    Lineage,
}

/// An observed identity. Numeric equality alone never proves correspondence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Identity {
    pub kind: IdentityKind,
    pub id: u64,
}

/// Canonical scalar bytes or a reference retaining its observed identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SemanticValue {
    Scalar(Vec<u8>),
    Reference(Identity),
}

/// A source coordinate distinguishes each invocation, occurrence and phase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coordinate {
    pub site: u64,
    pub source_occurrence: u64,
    pub invocation: Identity,
    pub occurrence: Identity,
    pub phase: u32,
}

/// One actual, ordered canonical charge, including its source position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChargeEntry {
    pub at: Coordinate,
    pub kind: u32,
    pub units: u64,
}

/// All facts required by the frozen E prefix law. Lists use source role/member
/// order, retaining multiplicity. No prices stand in for identities or values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixFacts {
    pub at: Coordinate,
    pub values: Vec<(u32, SemanticValue, Identity)>,
    pub roots: Vec<(u32, Identity, u64)>,
    pub edges: Vec<(Identity, u32, Identity, u64)>,
    pub retained: Vec<(Identity, Vec<u8>)>,
    pub released: Vec<Identity>,
    pub dependencies: Vec<(Identity, u64, u64)>,
    pub balance: u64,
    pub peak: u64,
    pub charges: Vec<ChargeEntry>,
    pub debt_cursor: u64,
    pub debt: Vec<ChargeEntry>,
    pub depth: u32,
    pub call_offsets: Vec<(Identity, u64)>,
    pub failure: Option<(String, Coordinate)>,
    pub operands: Vec<SemanticValue>,
    pub pending: Option<SemanticValue>,
    pub continuation: Vec<u8>,
    pub interruption: Option<Interruption>,
}

/// A complete observation at one reached legal source prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixSnapshot {
    facts: PrefixFacts,
}

impl PrefixSnapshot {
    /// Records observed facts. This constructor mints no execution capability.
    pub fn observed(facts: PrefixFacts) -> Self {
        Self { facts }
    }
    pub fn facts(&self) -> &PrefixFacts {
        &self.facts
    }
}

/// Evidence from both ISA layouts. Fixture evidence never qualifies a cutover.
#[derive(Debug)]
pub struct QualificationReport {
    pub reached_prefixes: usize,
    pub layouts: [IsaLayout; 2],
    pub fixture_only: bool,
}

/// Exact failing prefix and the independently observed expected/actual states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualificationFailure {
    pub prefix: usize,
    pub layout: Option<IsaLayout>,
    pub expected: Option<Box<PrefixSnapshot>>,
    pub actual: Option<Box<PrefixSnapshot>>,
    pub evidence: String,
}

impl QualificationFailure {
    fn unavailable(evidence: &str) -> Self {
        Self {
            prefix: 0,
            layout: None,
            expected: None,
            actual: None,
            evidence: evidence.into(),
        }
    }
}
impl std::fmt::Display for QualificationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "prefix {} {:?}: {}",
            self.prefix, self.layout, self.evidence
        )
    }
}
impl std::error::Error for QualificationFailure {}

/// Layouts of one verified ISA, never a source/ISA runtime selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsaLayout {
    Generic,
    Specialized,
}

/// Requested interruption at a reached legal source prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interruption {
    Slice,
    Observer,
    Handoff,
    BeforePrepareFailure,
    BeforeCommitFailure,
    SealReopen,
}

/// Constructs the frozen, testing-only qualification input.
pub fn qualification_case(
    program: crate::Program,
    input: crate::CellStart,
    bounds: Bounds,
    initial: fn() -> Box<dyn PageSource>,
    schedule: Box<[(u64, Interruption)]>,
) -> QualificationCase {
    QualificationCase {
        program,
        input,
        bounds,
        initial,
        schedule,
    }
}

/// Runs the source evaluator with the independent itemized resource oracle.
///
/// # Errors
/// Source prefix/itemized observations are not yet exposed by C/D.
pub fn source_oracle(
    _case: &QualificationCase,
) -> Result<Vec<PrefixSnapshot>, QualificationFailure> {
    Err(QualificationFailure::unavailable(
        "C/D source-prefix and itemized-resource observation adapters are not integrated",
    ))
}

/// Observes every reached prefix of the verified ISA in the requested layout.
///
/// # Errors
/// A/B/C/D execution and materialization are not yet integrated.
pub fn isa_prefixes(
    _case: &QualificationCase,
    _layout: IsaLayout,
) -> Result<Vec<PrefixSnapshot>, QualificationFailure> {
    Err(QualificationFailure::unavailable(
        "A/B/C/D verified ISA prefix adapter is not integrated",
    ))
}

/// Compares both layouts with independently executed source observations.
///
/// # Errors
/// Missing observations or any semantic/resource discrepancy.
pub fn qualify(case: &QualificationCase) -> Result<QualificationReport, QualificationFailure> {
    let _source = source_oracle(case)?;
    let _generic = isa_prefixes(case, IsaLayout::Generic)?;
    let _specialized = isa_prefixes(case, IsaLayout::Specialized)?;
    Err(QualificationFailure::unavailable(
        "prefix comparator has not been installed",
    ))
}
