//! A migration between two kernel versions, and the refusals it answers.

use std::collections::BTreeMap;

use lash_kernel_doc::{
    Document, DocumentId, FunctionCatalog, FunctionDefinition, FunctionId, FunctionName,
    FunctionRegistry, KernelVersion, RegistryError, Site,
};
use lash_kernel_edit::Correspondence;
use lash_kernel_state::ParkedRun;
use serde::Serialize;

/// The migration a breaking kernel version ships from the version before
/// it (`K-VER-004`). Each function is total: it answers its result or a
/// typed refusal, and changes nothing.
#[derive(Clone, Copy, Debug)]
pub struct Migration {
    pub from: KernelVersion,
    pub to: KernelVersion,
    /// Redeclares a library function written for `from` as one written for
    /// `to`. Its identity changes, since a definition states its version.
    pub definition: fn(&FunctionDefinition) -> Result<FunctionDefinition, DocumentRefusal>,
    /// Rewrites a document written for `from`. `functions` holds the
    /// definitions the document's manifest lists.
    pub document: fn(&Document, &dyn FunctionCatalog) -> Result<Rewritten, DocumentRefusal>,
    /// Carries a run of a document, parked under `from`, onto that
    /// document rewritten.
    pub parked: fn(&ParkedRun, &Document, &Rewritten) -> Result<ParkedRun, ParkedRefusal>,
}

/// A document rewritten for the next kernel version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rewritten {
    pub document: Document,
    /// Every node of the old document that survives, with its site in the
    /// new one. A node of a library function's body is listed only when the
    /// migration vouches for where the redeclared body put it.
    pub correspondence: Correspondence,
    /// Every library function the old manifest lists, with the identity of
    /// the function redeclared for the new version.
    pub functions: BTreeMap<FunctionId, FunctionId>,
}

impl Rewritten {
    /// The identity of the rewritten document.
    pub fn identity(&self) -> DocumentId {
        self.correspondence.result
    }
}

/// Why a document is not rewritten for the next version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case", tag = "reason")]
#[non_exhaustive]
pub enum DocumentRefusal {
    #[error("the document is written for kernel version {found}; the migration starts at {from}")]
    Version { found: u32, from: u32 },
    #[error("the document lists library function {function}, which no definition here states")]
    FunctionNotHeld { function: FunctionId },
    #[error("library function `{name}` ({function}) has no counterpart in the next version")]
    FunctionRetired {
        function: FunctionId,
        name: FunctionName,
    },
    #[error("the node at {site} has no form in the next version: {detail}")]
    FormNotCarried { site: Site, detail: String },
    #[error("the rewritten document has no identity: {message}")]
    Encode { message: String },
    #[error("the rewrite states two nodes at one site")]
    Correspondence,
}

/// Why a parked run is not carried onto a rewritten document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case", tag = "reason")]
#[non_exhaustive]
pub enum ParkedRefusal {
    #[error("the run was parked under kernel version {found}; the migration starts at {from}")]
    Version { found: u32, from: u32 },
    #[error("the run was parked under document {parked}; the rewrite is of {rewritten}")]
    Document {
        parked: DocumentId,
        rewritten: DocumentId,
    },
    /// The run holds a coordinate at a node the rewrite does not carry: a
    /// task stands there, a variable or a closure was made there, a loop or
    /// a cleanup block there is under way, or a task or an effect is
    /// identified by it.
    #[error("the run stands on {site}, which the rewrite does not carry ({held})")]
    SiteNotCarried { site: Site, held: &'static str },
    #[error("the run pins library function {function}, which the rewrite does not redeclare")]
    FunctionNotCarried { function: FunctionId },
    #[error("the run holds a state the next version cannot express: {detail}")]
    StateNotCarried { detail: String },
}

/// The migration this build ships from `version` to the version that
/// replaced it, or `None` when `version` is the newest.
pub fn migration_from(version: KernelVersion) -> Option<&'static Migration> {
    match version {
        #[cfg(not(feature = "synthetic-next"))]
        KernelVersion::One => None,
        #[cfg(feature = "synthetic-next")]
        KernelVersion::One => Some(&crate::synthetic::MIGRATION),
        #[cfg(feature = "synthetic-next")]
        KernelVersion::SyntheticNext => None,
    }
}

/// Registers, beside every function of `registry` written for
/// `migration.from`, the function `migration` redeclares it as, under the
/// same native implementation. An embedder that interprets both versions
/// assembles its library for the older one and calls this once.
///
/// Returns how many functions it redeclared.
pub fn migrate_registry(
    registry: &mut FunctionRegistry,
    migration: &Migration,
) -> Result<usize, MigrateRegistryError> {
    let written: Vec<FunctionId> = registry
        .iter()
        .filter(|(_, function)| function.definition.kernel == migration.from.number())
        .map(|(id, _)| *id)
        .collect();
    let redeclared = crate::rewrite::redeclare(migration.definition, written, &*registry)?;
    let count = redeclared.len();
    // A function is redeclared after every function its body calls: the
    // order `register` validates in.
    for function in redeclared {
        let native = registry
            .get(&function.from)
            .and_then(|registered| registered.native.clone());
        match registry.register(function.definition, native) {
            Ok(_) | Err(RegistryError::AlreadyRegistered { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(count)
}

/// Why a library could not be redeclared for the next version.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MigrateRegistryError {
    #[error(transparent)]
    Refused(#[from] DocumentRefusal),
    #[error(transparent)]
    Registry(#[from] RegistryError),
}
