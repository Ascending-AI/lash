//! The store-to-runtime loader (FIG-6985): the library a pinned execution
//! runs, assembled from the definitions its document was written against.
//!
//! A document names every library function it reaches by identity, and an
//! identity is the digest of one definition's canonical bytes, so a node
//! that holds an identity holds that very definition. What a node lacks it
//! reads from the store, where the document's publication and its first
//! root keep each definition as its HelperDefinition node (kind 14). The
//! loader admits those as any outside code is admitted: each decoded under
//! the kernel's data limits, its identity checked against the bytes, its
//! kernel version the document's, and registered through the checker in
//! dependency order ([`FunctionRegistry::register`]), never through the
//! trusted release-image path. The node's natives and plans attach only by
//! identity, as accelerators: a native-only function the node has no
//! native for refuses this execution alone, naming it, and nothing else on
//! the node.
//!
//! When the node holds every function the document reaches, the node's own
//! registry is the library, unchanged: the same identities run the same
//! definitions under the same charges. A loaded library is cached by its
//! closure, which names exactly its definitions; the natives and plans
//! bound to it are the node's, fixed for its life.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use lash_kernel_doc::{
    Document, FunctionDefinition, FunctionRegistry, KernelVersion, RegistryError,
};
use {lash_kernel_value::KernelDataLimits, lash_kernel_value::names::FunctionId};

use super::closure::{Closure, ClosureRefusal};

/// Why an execution's library was not assembled: what its run refuses,
/// naming the function.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LoadRefusal {
    /// The document reaches a function neither the node nor the store
    /// holds: its closure is incomplete.
    #[error("function {function} is neither held here nor stored")]
    Missing { function: FunctionId },
    /// Only a native implements the function, and this node has none.
    #[error("function {function} needs a native implementation this node lacks")]
    NativeMissing { function: FunctionId },
    /// A stored definition does not decode.
    #[error("the stored definition of {function} does not decode: {problem}")]
    Undecodable {
        function: FunctionId,
        problem: String,
    },
    /// A stored definition is another function's.
    #[error("the definition stored as {function} is {found}")]
    Identity {
        function: FunctionId,
        found: FunctionId,
    },
    /// A definition was supplied that the document does not reach.
    #[error("the definition of {function} was supplied, and the document does not reach it")]
    Unlisted { function: FunctionId },
    /// A stored definition is written for a kernel version other than its
    /// document's, or one this build does not interpret: it is never
    /// relinked to run.
    #[error(
        "function {function} is written for kernel version {kernel}, its document for {document}"
    )]
    KernelVersion {
        function: FunctionId,
        kernel: u32,
        document: u32,
    },
    /// The functions' bodies call each other in a cycle.
    #[error("function {function} reaches itself through another function's body")]
    Cycle { function: FunctionId },
    /// The checker refuses a definition.
    #[error("the definition of {function} is refused: {problem}")]
    Invalid {
        function: FunctionId,
        problem: String,
    },
    /// A stored definition performs an operation a package declares
    /// internal, and no stored snapshot lists it as that package's own
    /// (R5).
    #[error(
        "function {function} performs `{operation}`, internal to a package it is not a member of"
    )]
    InternalOperation {
        function: FunctionId,
        operation: lash_kernel_value::names::EffectName,
    },
    /// A definition did not encode as a graph node.
    #[error("the closure does not encode: {problem}")]
    Graph { problem: String },
}

impl From<ClosureRefusal> for LoadRefusal {
    fn from(refusal: ClosureRefusal) -> Self {
        match refusal {
            ClosureRefusal::Missing { function } => Self::Missing { function },
            ClosureRefusal::Graph(refusal) => Self::Graph {
                problem: refusal.to_string(),
            },
        }
    }
}

impl From<LoadRefusal> for lash_protocol_types::engine_fault::KernelRestoreRefusal {
    fn from(refusal: LoadRefusal) -> Self {
        match refusal {
            LoadRefusal::NativeMissing { function } => Self::MissingNative {
                function: function.to_string(),
            },
            LoadRefusal::Missing { function }
            | LoadRefusal::Undecodable { function, .. }
            | LoadRefusal::Identity { function, .. }
            | LoadRefusal::Unlisted { function }
            | LoadRefusal::KernelVersion { function, .. }
            | LoadRefusal::Cycle { function }
            | LoadRefusal::Invalid { function, .. }
            | LoadRefusal::InternalOperation { function, .. } => Self::DefinitionRefused {
                function: function.to_string(),
                problem: refusal.to_string(),
            },
            LoadRefusal::Graph { problem } => Self::Malformed { problem },
        }
    }
}

/// `bytes`, stored as the definition of `function`, decoded under
/// `limits` and checked to be that function's.
///
/// # Errors
///
/// [`LoadRefusal::Undecodable`] and [`LoadRefusal::Identity`].
pub fn decode(
    function: FunctionId,
    bytes: &[u8],
    limits: KernelDataLimits,
) -> Result<FunctionDefinition, LoadRefusal> {
    let definition =
        lash_kernel_value::tokens::decode_body_with_limits::<FunctionDefinition>(bytes, limits)
            .map_err(|error| LoadRefusal::Undecodable {
                function,
                problem: error.to_string(),
            })?;
    let found = definition.identity();
    if found != function {
        return Err(LoadRefusal::Identity { function, found });
    }
    Ok(definition)
}

/// Decoded definitions by identity, bounded by how many it keeps: an
/// identity names one definition, so a hit is the definition the bytes
/// decode to, and a node that runs one closure again decodes nothing.
#[derive(Debug)]
pub struct Definitions {
    held: Mutex<Kept>,
    bound: usize,
}

/// The decoded definitions by identity, and their identities oldest first.
type Kept = (
    BTreeMap<FunctionId, Arc<FunctionDefinition>>,
    VecDeque<FunctionId>,
);

impl Default for Definitions {
    fn default() -> Self {
        Self::new(4096)
    }
}

impl Definitions {
    /// A cache that keeps at most `bound` definitions.
    #[must_use]
    pub fn new(bound: usize) -> Self {
        Self {
            held: Mutex::default(),
            bound: bound.max(1),
        }
    }

    /// The definition `bytes` encode, stored as `function`: decoded and
    /// checked once ([`decode`]), and answered from the cache after.
    ///
    /// # Errors
    ///
    /// [`decode`]'s.
    pub fn decode(
        &self,
        function: FunctionId,
        bytes: &[u8],
        limits: KernelDataLimits,
    ) -> Result<Arc<FunctionDefinition>, LoadRefusal> {
        if let Some(definition) = self.get(&function) {
            return Ok(definition);
        }
        let definition = Arc::new(decode(function, bytes, limits)?);
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (definitions, order) = &mut *held;
        if definitions
            .insert(function, Arc::clone(&definition))
            .is_none()
        {
            order.push_back(function);
            while order.len() > self.bound {
                if let Some(oldest) = order.pop_front() {
                    definitions.remove(&oldest);
                }
            }
        }
        Ok(definition)
    }

    /// The definition of `function`, when the cache holds it.
    #[must_use]
    pub fn get(&self, function: &FunctionId) -> Option<Arc<FunctionDefinition>> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
            .get(function)
            .cloned()
    }
}

/// An execution's library: the registry its machine runs, and the closure
/// of its document over it, which its roots pin.
#[derive(Clone, Debug)]
pub struct Assembled {
    pub registry: Arc<FunctionRegistry>,
    pub closure: Closure,
    /// Whether the registry is the node's own, unchanged.
    pub held: bool,
}

/// The library `document` runs on this node: `held`, the node's registry
/// with its natives and plans, when it holds every function the document
/// reaches; otherwise a registry of exactly the document's closure, each
/// function the node holds taken as it holds it and each it lacks from
/// `stored`, its definition read from the store.
///
/// # Errors
///
/// [`LoadRefusal`], naming the function.
pub fn assemble(
    document: &Document,
    held: &Arc<FunctionRegistry>,
    stored: &BTreeMap<FunctionId, Arc<FunctionDefinition>>,
    helpers: &lash_kernel_doc::BuildHelpers,
    selection: &lash_kernel_doc::Selection,
) -> Result<Assembled, LoadRefusal> {
    let admitted = admit(
        document.functions.keys().copied().collect(),
        Some(document.kernel()),
        held,
        stored,
        helpers,
        &document.library,
    )?;
    assemble_admitted(admitted, held, &document.library, selection)
}

fn assemble_admitted(
    admitted: Admitted,
    held: &Arc<FunctionRegistry>,
    library: &lash_kernel_doc::LibraryDependencies,
    selection: &lash_kernel_doc::Selection,
) -> Result<Assembled, LoadRefusal> {
    check_selected_natives(&admitted.closure, held, selection)?;
    assemble_checked_admitted(admitted, held, library, selection)
}

fn check_selected_natives(
    closure: &Closure,
    held: &FunctionRegistry,
    selection: &lash_kernel_doc::Selection,
) -> Result<(), LoadRefusal> {
    for member in closure
        .body()
        .functions()
        .iter()
        .filter(|member| selected(member.function, selection))
    {
        if member.native_only
            && held
                .get(&member.function)
                .is_none_or(|registered| registered.native().is_none())
        {
            return Err(LoadRefusal::NativeMissing {
                function: member.function,
            });
        }
    }
    Ok(())
}

fn assemble_checked_admitted(
    admitted: Admitted,
    held: &Arc<FunctionRegistry>,
    library: &lash_kernel_doc::LibraryDependencies,
    selection: &lash_kernel_doc::Selection,
) -> Result<Assembled, LoadRefusal> {
    let closure = admitted.closure;
    if closure.stored().is_empty() {
        return Ok(Assembled {
            registry: Arc::clone(held),
            closure,
            held: true,
        });
    }
    let mut registry = held.as_ref().clone();
    for function in dependency_order(&closure, held, &admitted.stored, library, selection)? {
        if registry.get(&function).is_none() {
            register(
                &mut registry,
                function,
                stored_definition(&admitted.stored, function)?,
                None,
            )?;
        }
    }
    Ok(Assembled {
        registry: Arc::new(registry),
        closure,
        held: false,
    })
}

/// The declaration mode of [`assemble`]: the definitions `listed`, the
/// library functions of documents written for kernel version `kernel`,
/// reach, each the node holds taken as it holds it and each it lacks from
/// `stored`, admitted as [`assemble`] admits them, and with no native
/// bound or required. Rebasing, adoption and import read the code they
/// move off or onto from it, each under its own policy: a function only a
/// native implements, which the node no longer has, is declared and never
/// run, and its signature is read and checked all the same.
///
/// # Errors
///
/// [`LoadRefusal`], naming the function; never
/// [`LoadRefusal::NativeMissing`].
pub fn declare(
    listed: impl IntoIterator<Item = FunctionId>,
    kernel: u32,
    held: &FunctionRegistry,
    stored: &BTreeMap<FunctionId, Arc<FunctionDefinition>>,
    helpers: &lash_kernel_doc::BuildHelpers,
    library: &lash_kernel_doc::LibraryDependencies,
    selection: &lash_kernel_doc::Selection,
) -> Result<FunctionRegistry, LoadRefusal> {
    let admitted = admit(
        listed.into_iter().collect(),
        Some(kernel),
        held,
        stored,
        helpers,
        library,
    )?;
    declare_admitted(admitted, held, library, selection)
}

fn declare_admitted(
    admitted: Admitted,
    held: &FunctionRegistry,
    library: &lash_kernel_doc::LibraryDependencies,
    selection: &lash_kernel_doc::Selection,
) -> Result<FunctionRegistry, LoadRefusal> {
    let mut registry = held.as_declarations();
    for function in dependency_order(
        &admitted.closure,
        held,
        &admitted.stored,
        library,
        selection,
    )? {
        if registry.get(&function).is_some() {
            continue;
        }
        let definition = stored_definition(&admitted.stored, function)?;
        register(&mut registry, function, definition, None)?;
    }
    Ok(registry)
}

/// What [`assemble`] and [`declare`] admit alike: the stored definitions
/// the node lacks, each `listed` and written for kernel version `kernel`,
/// and the closure of `listed` over them and the node's own.
struct Admitted {
    closure: Closure,
    stored: BTreeMap<FunctionId, Arc<FunctionDefinition>>,
}

fn admit(
    listed: BTreeSet<FunctionId>,
    kernel: Option<u32>,
    held: &FunctionRegistry,
    stored: &BTreeMap<FunctionId, Arc<FunctionDefinition>>,
    helpers: &lash_kernel_doc::BuildHelpers,
    library: &lash_kernel_doc::LibraryDependencies,
) -> Result<Admitted, LoadRefusal> {
    if let Some(function) = stored.keys().find(|function| !listed.contains(function)) {
        return Err(LoadRefusal::Unlisted {
            function: *function,
        });
    }
    for (function, definition) in stored {
        if kernel.is_some_and(|kernel| definition.kernel != kernel)
            || KernelVersion::of(definition.kernel).is_none()
        {
            return Err(LoadRefusal::KernelVersion {
                function: *function,
                kernel: definition.kernel,
                document: kernel.unwrap_or(definition.kernel),
            });
        }
    }
    // A function the node holds is taken as it holds it, whatever the
    // store answered for it: an identity names one definition.
    let stored: BTreeMap<FunctionId, Arc<FunctionDefinition>> = stored
        .iter()
        .filter(|(function, _)| held.get(function).is_none())
        .map(|(function, definition)| (*function, Arc::clone(definition)))
        .collect();
    let closure = super::closure::derive(listed, held, &stored, helpers, library)?;
    Ok(Admitted { closure, stored })
}

fn stored_definition(
    stored: &BTreeMap<FunctionId, Arc<FunctionDefinition>>,
    function: FunctionId,
) -> Result<FunctionDefinition, LoadRefusal> {
    Ok(stored
        .get(&function)
        .ok_or(LoadRefusal::Missing { function })?
        .as_ref()
        .clone())
}

/// Registers `definition` as `function` through the checker.
fn register(
    registry: &mut FunctionRegistry,
    function: FunctionId,
    definition: FunctionDefinition,
    native: Option<Arc<dyn lash_kernel_doc::NativeFunction>>,
) -> Result<(), LoadRefusal> {
    let found = registry
        .register(definition, native)
        .map_err(|error| refused(function, &error))?;
    if found != function {
        return Err(LoadRefusal::Identity { function, found });
    }
    Ok(())
}

/// What running an execution of `closure`, written for kernel version
/// `kernel`, requires of a node (FIG-6985): the native of each function it
/// reaches that only a native implements, and the kernel version, beside
/// what its operation `links` require. A pure projection of the closure
/// and the links, so a stored record is derived again to check it.
#[must_use]
pub fn requirements<'a>(
    closure: &Closure,
    kernel: u32,
    links: impl IntoIterator<Item = &'a lash_core_store::tool_surface::OperationRoute>,
) -> lash_vocabulary::claims::ClaimRequirements {
    let mut requirements = lash_vocabulary::claims::ClaimRequirements {
        machine: lash_kernel_value::claims::NodeRequirements {
            helpers: Default::default(),
            libraries: Default::default(),
            natives: closure
                .body()
                .functions()
                .iter()
                .filter(|member| member.native_only)
                .map(|member| member.function)
                .collect(),
            kernels: BTreeSet::from([kernel]),
        },
        ..Default::default()
    };
    for link in links {
        link.require(&mut requirements);
    }
    requirements
}

fn refused(function: FunctionId, error: &RegistryError) -> LoadRefusal {
    match error {
        RegistryError::NativeMissing { .. } => LoadRefusal::NativeMissing { function },
        error => LoadRefusal::Invalid {
            function,
            problem: error.to_string(),
        },
    }
}

/// The closure's functions, each after every function its body calls: the
/// order registration validates them in. A body calling itself is no
/// cycle.
fn dependency_order(
    closure: &Closure,
    held: &FunctionRegistry,
    stored: &BTreeMap<FunctionId, Arc<FunctionDefinition>>,
    library: &lash_kernel_doc::LibraryDependencies,
    selection: &lash_kernel_doc::Selection,
) -> Result<Vec<FunctionId>, LoadRefusal> {
    let callees = |function: &FunctionId| -> BTreeSet<FunctionId> {
        let mut callees: BTreeSet<FunctionId> = match held.get(function) {
            Some(registered) => registered.callees().clone(),
            None => stored
                .get(function)
                .and_then(|definition| definition.body())
                .map(|body| {
                    lash_kernel_doc::Node::Block(&body.block)
                        .functions_within()
                        .into_iter()
                        .collect()
                })
                .unwrap_or_default(),
        };
        callees.retain(|callee| callee != function && !library.contains(callee));
        callees
    };
    let mut waiting: BTreeMap<FunctionId, BTreeSet<FunctionId>> = closure
        .body()
        .identities()
        .filter(|function| selected(*function, selection))
        .map(|function| (function, callees(&function)))
        .collect();
    let mut order = Vec::with_capacity(waiting.len());
    while !waiting.is_empty() {
        let ready: Vec<FunctionId> = waiting
            .iter()
            .filter(|(_, callees)| callees.is_empty())
            .map(|(function, _)| *function)
            .collect();
        let Some(&first) = waiting.keys().next() else {
            break;
        };
        if ready.is_empty() {
            return Err(LoadRefusal::Cycle { function: first });
        }
        for function in &ready {
            waiting.remove(function);
        }
        for callees in waiting.values_mut() {
            for function in &ready {
                callees.remove(function);
            }
        }
        order.extend(ready);
    }
    Ok(order)
}

/// Libraries this node assembled, by the closure they were assembled for,
/// at most `bound` of them: a closure names exactly its definitions, and
/// the natives and plans a node binds to them are fixed for its life, so
/// one closure and exact helper member set assemble one library.
#[derive(Debug)]
pub struct Libraries<L> {
    held: Mutex<VecDeque<(LibraryKey, L)>>,
    bound: usize,
}

type LibraryKey = (
    lash_core_store::checkpoint_graph::NodeId,
    lash_kernel_value::HelperLibraryId,
    lash_kernel_doc::Selection,
);

impl<L: Clone> Libraries<L> {
    /// A cache that keeps at most `bound` libraries.
    #[must_use]
    pub fn new(bound: usize) -> Self {
        Self {
            held: Mutex::default(),
            bound: bound.max(1),
        }
    }

    /// The library assembled for `closure`, or `make`'s, kept.
    ///
    /// # Errors
    ///
    /// `make`'s.
    pub fn get_or_try<E>(
        &self,
        closure: lash_core_store::checkpoint_graph::NodeId,
        helpers: lash_kernel_value::HelperLibraryId,
        make: impl FnOnce() -> Result<L, E>,
    ) -> Result<L, E> {
        self.get_or_try_selected(closure, helpers, &lash_kernel_doc::Selection::Fresh, make)
    }

    /// Cache derived execution only for this exact restored permission set.
    pub fn get_or_try_selected<E>(
        &self,
        closure: lash_core_store::checkpoint_graph::NodeId,
        helpers: lash_kernel_value::HelperLibraryId,
        selection: &lash_kernel_doc::Selection,
        make: impl FnOnce() -> Result<L, E>,
    ) -> Result<L, E> {
        let closure = (closure, helpers, selection.clone());
        {
            let mut held = self
                .held
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(at) = held.iter().position(|(id, _)| *id == closure)
                && let Some(hit) = held.remove(at)
            {
                let library = hit.1.clone();
                held.push_back(hit);
                return Ok(library);
            }
        }
        let library = make()?;
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while held.len() >= self.bound {
            held.pop_front();
        }
        held.push_back((closure, library.clone()));
        Ok(library)
    }
}

/// Assemble only selected reach while keeping every retained Document intact.
/// The temporary loader input is never published or assigned a content identity.
pub fn declare_reach(
    root: &Document,
    documents: &BTreeMap<lash_kernel_value::names::DocumentId, Arc<Document>>,
    functions: &BTreeMap<
        lash_kernel_value::package::LinkId,
        std::collections::BTreeSet<FunctionId>,
    >,
    machine: &lash_kernel_value::claims::NodeRequirements,
    held: &Arc<FunctionRegistry>,
    helpers: &lash_kernel_doc::BuildHelpers,
    selection: &lash_kernel_doc::Selection,
) -> Result<Assembled, LoadRefusal> {
    let (admitted, library) = admit_reach(root, documents, functions, machine, held, helpers)?;
    let closure = admitted.closure.clone();
    let registry = declare_admitted(admitted, held, &library, selection)?;
    Ok(Assembled {
        registry: Arc::new(registry),
        closure,
        held: false,
    })
}

/// Linked source and selected native availability, checked before any cache hit.
/// The token owns the exact registry and selection its assembly may use.
pub struct PreparedReach {
    admitted: Admitted,
    library: lash_kernel_doc::LibraryDependencies,
    held: Arc<FunctionRegistry>,
    selection: lash_kernel_doc::Selection,
}

impl PreparedReach {
    pub fn closure(&self) -> &Closure {
        &self.admitted.closure
    }

    pub fn is_held(&self) -> bool {
        self.admitted.closure.stored().is_empty()
    }

    /// Derive the registry only when the caller's bounded cache misses.
    pub fn assemble(self) -> Result<Assembled, LoadRefusal> {
        assemble_checked_admitted(self.admitted, &self.held, &self.library, &self.selection)
    }
}

/// Check every original source and selected native before consulting a cache.
pub fn prepare_reach(
    root: &Document,
    documents: &BTreeMap<lash_kernel_value::DocumentId, Arc<Document>>,
    functions: &BTreeMap<lash_kernel_value::LinkId, std::collections::BTreeSet<FunctionId>>,
    machine: &lash_kernel_value::claims::NodeRequirements,
    held: &Arc<FunctionRegistry>,
    helpers: &lash_kernel_doc::BuildHelpers,
    selection: &lash_kernel_doc::Selection,
) -> Result<PreparedReach, LoadRefusal> {
    let (admitted, library) = admit_reach(root, documents, functions, machine, held, helpers)?;
    check_selected_natives(&admitted.closure, held, selection)?;
    Ok(PreparedReach {
        admitted,
        library,
        held: Arc::clone(held),
        selection: selection.clone(),
    })
}

/// Assemble executable linked reach, checking each retained definition against
/// its own document's version and requiring every selected native implementation.
pub fn assemble_reach(
    root: &Document,
    documents: &BTreeMap<lash_kernel_value::names::DocumentId, Arc<Document>>,
    functions: &BTreeMap<
        lash_kernel_value::package::LinkId,
        std::collections::BTreeSet<FunctionId>,
    >,
    machine: &lash_kernel_value::claims::NodeRequirements,
    held: &Arc<FunctionRegistry>,
    helpers: &lash_kernel_doc::BuildHelpers,
    selection: &lash_kernel_doc::Selection,
) -> Result<Assembled, LoadRefusal> {
    let (admitted, library) = admit_reach(root, documents, functions, machine, held, helpers)?;
    assemble_admitted(admitted, held, &library, selection)
}

fn admit_reach(
    root: &Document,
    documents: &BTreeMap<lash_kernel_value::names::DocumentId, Arc<Document>>,
    functions: &BTreeMap<
        lash_kernel_value::package::LinkId,
        std::collections::BTreeSet<FunctionId>,
    >,
    machine: &lash_kernel_value::claims::NodeRequirements,
    held: &Arc<FunctionRegistry>,
    helpers: &lash_kernel_doc::BuildHelpers,
) -> Result<(Admitted, lash_kernel_doc::LibraryDependencies), LoadRefusal> {
    for document in std::iter::once(root).chain(
        documents
            .values()
            .map(Arc::as_ref)
            .filter(|document| !std::ptr::eq(*document, root)),
    ) {
        lash_kernel_doc::validate_document_structure(document).map_err(|error| {
            LoadRefusal::Graph {
                problem: error.to_string(),
            }
        })?;
    }
    // This union decides graph retention only; each original Document above
    // authenticates origin independently, and Program retains that boundary.
    let library = lash_kernel_doc::LibraryDependencies(
        std::iter::once(root)
            .chain(documents.values().map(Arc::as_ref))
            .flat_map(|document| document.library.iter().copied())
            .collect(),
    );
    let mut stored = BTreeMap::new();
    for function in functions.values().flatten() {
        if let Some(definition) = documents
            .values()
            .find_map(|document| document.functions.get(function))
        {
            stored
                .entry(*function)
                .or_insert_with(|| Arc::new(definition.clone()));
        } else if !machine.natives.contains(function) && !helpers.contains(function) {
            return Err(LoadRefusal::Missing {
                function: *function,
            });
        }
    }
    let admitted = admit(
        stored.keys().copied().collect(),
        None,
        held,
        &stored,
        helpers,
        &library,
    )?;
    Ok((admitted, library))
}

/// A restored selection narrows executable admission only. The durable
/// closure retains the original ordinary definitions and its identity, so
/// root.closure still authenticates the original code independently of which
/// authored functions remain live after a helper-library upgrade.
fn selected(function: FunctionId, selection: &lash_kernel_doc::Selection) -> bool {
    match selection {
        lash_kernel_doc::Selection::Fresh => true,
        lash_kernel_doc::Selection::Restored(selection) => selection
            .live
            .iter()
            .any(|callable| callable.function == function),
    }
}

#[cfg(test)]
mod laws {
    use super::*;

    /// FIG-7169: the immutable helper handle may share a registry with
    /// ordinary installed code; declaration must preserve and reuse it.
    #[test]
    fn declaring_stored_code_reuses_installed_ordinary_definitions() {
        let mut registry = FunctionRegistry::new();
        let ordinary = registry
            .register(
                lash_kernel_doc::parse_definition(
                    "function ordinary.value() -> Any\nkernel 1\ncharge 1\nbody { return 7 }",
                )
                .unwrap(),
                None,
            )
            .unwrap();
        let helper = registry
            .register(
                lash_kernel_doc::parse_definition(
                    "function helper.value() -> Any\nkernel 1\ncharge 1\nbody { return 9 }",
                )
                .unwrap(),
                None,
            )
            .unwrap();
        let helpers = lash_kernel_doc::BuildHelpers::new(registry.clone(), [helper]).unwrap();
        let stored = lash_kernel_doc::parse_definition(&format!(
            "function stored.value() -> Any\nkernel 1\ncharge 1\nuse ordinary.value = @{ordinary}\nbody {{ let value = invoke ordinary.value() return value }}")).unwrap();
        let stored_id = stored.identity();
        let declared = declare(
            [ordinary, stored_id],
            1,
            &registry,
            &BTreeMap::from([(stored_id, Arc::new(stored))]),
            &helpers,
            &lash_kernel_doc::LibraryDependencies::default(),
            &lash_kernel_doc::Selection::Fresh,
        )
        .unwrap();
        assert!(declared.get(&ordinary).is_some());
        assert!(declared.get(&helper).is_some());
        assert!(declared.get(&stored_id).is_some());
        assert!(registry.get(&stored_id).is_none());
    }
}
