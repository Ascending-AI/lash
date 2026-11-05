//! Independent qualification for the prefix-exact region ISA, FIG-7634.
//!
//! These diagnostics never authorize execution. Missing observations are errors,
//! including on a build where only the declaration or fixture lanes exist.
use crate::{Bounds, CellStart, Program};
use lash_kernel_state::kernel_internals::logical::Owner;
use lash_kernel_state::paged::PageSource;

mod comparison;
#[cfg(test)]
mod tests;

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
    /// Canonical identities supplied by the initial graph never rename.
    ExistingEntity,
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
    Object { kind: u32, identity: Identity },
    Tuple(Vec<SemanticValue>),
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

/// A value occurrence is tied to its semantic producer and ordered inputs.
/// A fresh opaque identity alone cannot establish value provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValueOrigin {
    pub value: Identity,
    pub at: Coordinate,
    pub kernel: Vec<u8>,
    pub inputs: Vec<Identity>,
}

/// Full source owner identity, direct holding price and outgoing multiplicity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootFact {
    pub owner: Owner,
    pub direct_bytes: u64,
    pub edges: Vec<(Identity, u64)>,
}

/// All facts required by the frozen E prefix law. Lists use source role/member
/// order, retaining multiplicity. No prices stand in for identities or values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixFacts {
    pub at: Coordinate,
    pub values: Vec<(Owner, SemanticValue, Identity)>,
    pub origins: Vec<ValueOrigin>,
    pub roots: Vec<RootFact>,
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
    pub stop_payload: Option<SemanticValue>,
    pub operands: Vec<SemanticValue>,
    pub pending: Option<SemanticValue>,
    pub continuation: Vec<u8>,
    pub interruption: Option<Interruption>,
}

/// A complete observation at one reached legal source prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixSnapshot {
    facts: PrefixFacts,
    witness: Option<ExecutionWitness>,
}

impl PrefixSnapshot {
    /// Records observed facts. This constructor mints no execution capability.
    pub fn observed(facts: PrefixFacts) -> Self {
        Self {
            facts,
            witness: None,
        }
    }
    pub fn facts(&self) -> &PrefixFacts {
        &self.facts
    }

    /// Physical counters observed from the ISA, separate from source semantics.
    pub fn with_witness(mut self, witness: ExecutionWitness) -> Self {
        self.witness = Some(witness);
        self
    }
}

/// Instrumentation from C's real runner. These are diagnostics, never semantic
/// capabilities. Source execution cannot satisfy this structural requirement.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecutionWitness {
    pub verified_operations: u64,
    pub interior_root_transactions: u64,
    pub source_fallbacks: u64,
    pub interior_roots: u64,
    pub interior_ledger_calls: u64,
    pub interior_transactions: u64,
    pub authentication_hashes: u64,
    pub table_snapshots: u64,
    pub phase_proof_allocations: u64,
    pub max_recovery_values: u64,
    pub max_active_frames: u64,
}

/// Evidence from both ISA layouts. Fixture evidence never qualifies a cutover.
#[derive(Debug)]
pub struct QualificationReport {
    reached_prefixes: usize,
    layouts: [IsaLayout; 2],
    fixture_only: bool,
    coverage: std::collections::BTreeMap<String, u64>,
    witnesses: Vec<(IsaLayout, ExecutionWitness)>,
}

impl QualificationReport {
    pub fn reached_prefixes(&self) -> usize {
        self.reached_prefixes
    }
    pub fn layouts(&self) -> &[IsaLayout; 2] {
        &self.layouts
    }
    pub fn fixture_only(&self) -> bool {
        self.fixture_only
    }
    pub fn coverage(&self) -> &std::collections::BTreeMap<String, u64> {
        &self.coverage
    }
    pub fn witnesses(&self) -> &[(IsaLayout, ExecutionWitness)] {
        &self.witnesses
    }
}

/// Exact failing prefix and the independently observed expected/actual states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualificationFailure {
    prefix: usize,
    layout: Option<IsaLayout>,
    expected: Option<Box<PrefixSnapshot>>,
    actual: Option<Box<PrefixSnapshot>>,
    evidence: String,
}

impl QualificationFailure {
    pub fn prefix(&self) -> usize {
        self.prefix
    }
    pub fn layout(&self) -> Option<IsaLayout> {
        self.layout
    }
    pub fn expected(&self) -> Option<&PrefixSnapshot> {
        self.expected.as_deref()
    }
    pub fn actual(&self) -> Option<&PrefixSnapshot> {
        self.actual.as_deref()
    }
    pub fn evidence(&self) -> &str {
        &self.evidence
    }
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
    let source = source_oracle(case)?;
    comparison::qualify_observations(&source, |layout| isa_prefixes(case, layout), false)
}
