//! Derivation and admission: the two things a caller asks of the checker.

use lash_kernel_doc::core_support::RegisteredFunctionCoreSupport;
use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{
    Document, DocumentId, EffectName, FunctionCatalog, FunctionDefinition, InvalidReason, Name,
    Signature, Unit, validate_definition, validate_document,
};

use crate::flow;
use crate::graph::Graph;
use crate::link::Linker;
use crate::manifest;
use crate::refusal::{Refusal, RefusalReason, Refused, refused};
use crate::types::serves;
use crate::typing::Typer;

/// What an embedder provides a document with (`K-ADM-001` to `K-ADM-004`).
pub struct Environment<'a> {
    /// The effects it can run, each with its signature.
    pub effects: BTreeMap<EffectName, Signature>,
    /// The library functions it holds, by identity.
    pub functions: &'a dyn FunctionCatalog,
    /// The session bindings `main` starts with (`K-SES-001`).
    pub bindings: BTreeSet<Name>,
}

impl<'a> Environment<'a> {
    /// An environment that holds `functions` and provides nothing else.
    pub fn new(functions: &'a dyn FunctionCatalog) -> Self {
        Self {
            effects: BTreeMap::new(),
            functions,
            bindings: BTreeSet::new(),
        }
    }
}

/// A document an environment can run, with everything derived from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admitted {
    pub identity: DocumentId,
    pub graph: Graph,
}

/// Links a document and derives its view, against the definitions in
/// `catalog`. The result is a function of the document and those
/// definitions and of nothing else.
///
/// A variable `main` does not declare is a session binding here and is
/// reported by [`Graph::session_reads`]; [`admit`] refuses one the
/// environment does not provide.
pub fn derive(document: &Document, catalog: &dyn FunctionCatalog) -> Result<Graph, Refusal> {
    let mut errors = Vec::new();
    match check(document, catalog, &mut errors) {
        Some(graph) if errors.is_empty() => Ok(graph),
        _ => Err(Refusal { errors }),
    }
}

/// The view of a document whose functions resolve and whose shape is
/// valid, whatever faults its type and scope rules find: what a reader of
/// its structure needs, such as an entry's linked-effect set, when the
/// code it reads is checked where it runs. [`derive`] refuses those faults.
///
/// # Errors
///
/// A function the document names is missing, or its shape is invalid.
pub fn derive_view(document: &Document, catalog: &dyn FunctionCatalog) -> Result<Graph, Refusal> {
    let mut errors = Vec::new();
    check(document, catalog, &mut errors).ok_or(Refusal { errors })
}

/// Links a library function's kernel-code body and derives its view: one
/// unit, [`Unit::Library`] of the definition's identity. A definition with
/// no body derives an empty view.
pub fn derive_definition(
    definition: &FunctionDefinition,
    catalog: &dyn FunctionCatalog,
) -> Result<Graph, Refusal> {
    let mut errors = Vec::new();
    let checked = library(definition, catalog, catalog.registration(definition));
    errors.extend(checked.errors.iter().cloned());
    match &checked.graph {
        Some(graph) if errors.is_empty() => Ok(graph.clone()),
        _ => Err(Refusal { errors }),
    }
}

/// Admits a document against an environment, or refuses it naming every
/// effect the environment lacks, every effect whose signature does not
/// serve, every function identity it does not hold, and every fault of the
/// document itself.
pub fn admit(document: &Document, environment: &Environment<'_>) -> Result<Admitted, Refusal> {
    let mut errors = Vec::new();
    for (effect, expected) in &document.manifest.effects {
        let reason = match environment.effects.get(effect) {
            None => Some(RefusalReason::MissingEffect {
                effect: effect.clone(),
            }),
            Some(provided) => {
                serves(provided, expected).map(|mismatch| RefusalReason::EffectSignature {
                    effect: effect.clone(),
                    expected: expected.clone(),
                    provided: provided.clone(),
                    mismatch: Box::new(mismatch),
                })
            }
        };
        if let Some(reason) = reason {
            errors.push(refused(None, reason));
        }
    }
    let graph = check(document, environment.functions, &mut errors);
    if let Some(graph) = &graph {
        for (name, nodes) in graph.session_reads() {
            if environment.bindings.contains(name) {
                continue;
            }
            for node in nodes {
                errors.push(refused(
                    Some(&graph.node(*node).site),
                    RefusalReason::UnboundVariable { name: name.clone() },
                ));
            }
        }
    }
    let identity = document.identity().map_err(|error| Refusal {
        errors: vec![refused(
            None,
            RefusalReason::Invalid(InvalidReason::Identity {
                message: error.message,
            }),
        )],
    })?;
    match graph {
        Some(graph) if errors.is_empty() => Ok(Admitted { identity, graph }),
        _ => Err(Refusal { errors }),
    }
}

/// Checks a document against `catalog`, pushing every fault it finds. The
/// view is `None` when the document cannot be linked at all.
fn check(
    document: &Document,
    catalog: &dyn FunctionCatalog,
    errors: &mut Vec<Refused>,
) -> Option<Graph> {
    // The statement rule reads the definitions of the functions a statement
    // names, so a missing definition is named before anything else is read.
    let (effects, reached) = manifest::reach(document, catalog);
    let missing = manifest::missing_functions(document, catalog, &reached);
    if !missing.is_empty() {
        errors.extend(missing);
        return None;
    }
    if let Err(invalid) = validate_document(document, catalog) {
        errors.push(invalid.into());
        return None;
    }
    // The statement rule and the scope rule hold of every body the document
    // reaches, as they do of the document (`K-STMT-003`).
    for function in reached.keys() {
        if let Some(definition) = catalog.definition(function) {
            let checked = library(definition, catalog, catalog.registered(function));
            errors.extend(checked.errors.iter().cloned());
        }
    }
    manifest::check(document, catalog, &effects, &reached, errors);

    let mut linker = Linker::new();
    linker.unit(Unit::Main, &[], &document.main, None);
    for (name, function) in &document.functions {
        linker.unit(
            Unit::Function(name.clone()),
            &function.params,
            &function.body,
            document.entries.get(name).cloned(),
        );
    }
    errors.append(&mut linker.errors);
    let mut graph = linker.graph;
    Typer {
        graph: &mut graph,
        catalog,
        effects: Some(&document.manifest.effects),
        declared: Some(&document.functions),
        errors,
    }
    .run();
    flow::derive(&mut graph, catalog);
    Some(graph)
}

#[cfg(test)]
std::thread_local! {
    pub(crate) static LIBRARY_WORK: std::cell::Cell<(usize, usize, usize)> = const { std::cell::Cell::new((0, 0, 0)) };
}

struct CheckedLibrary {
    graph: Option<Graph>,
    errors: Vec<Refused>,
}

fn library(
    definition: &FunctionDefinition,
    catalog: &dyn FunctionCatalog,
    registered: Option<&lash_kernel_doc::RegisteredFunction>,
) -> std::sync::Arc<CheckedLibrary> {
    if let Some(registered) = registered {
        return registered.derive(|| {
            let mut errors = Vec::new();
            let graph = library_graph(definition, registered.identity(), catalog, &mut errors);
            CheckedLibrary {
                graph: Some(graph),
                errors,
            }
        });
    }
    let mut errors = Vec::new();
    let graph = unregistered_library(definition, catalog, &mut errors);
    std::sync::Arc::new(CheckedLibrary { graph, errors })
}

fn unregistered_library(
    definition: &FunctionDefinition,
    catalog: &dyn FunctionCatalog,
    errors: &mut Vec<Refused>,
) -> Option<Graph> {
    #[cfg(test)]
    LIBRARY_WORK.with(|work| {
        let (v, i, g) = work.get();
        work.set((v + 1, i, g));
    });
    if let Err(invalid) = validate_definition(definition, catalog) {
        errors.push(invalid.into());
        return None;
    }
    if definition.body().is_none() {
        return Some(Graph::empty());
    }
    #[cfg(test)]
    LIBRARY_WORK.with(|work| {
        let (v, i, g) = work.get();
        work.set((v, i + 1, g));
    });
    let function = definition.identity().ok()?;
    Some(library_graph(definition, function, catalog, errors))
}

fn library_graph(
    definition: &FunctionDefinition,
    function: lash_kernel_doc::FunctionId,
    catalog: &dyn FunctionCatalog,
    errors: &mut Vec<Refused>,
) -> Graph {
    let Some(body) = definition.body() else {
        return Graph::empty();
    };
    #[cfg(test)]
    LIBRARY_WORK.with(|work| {
        let (v, i, g) = work.get();
        work.set((v, i, g + 1));
    });
    let params: Vec<Name> = definition
        .signature
        .params
        .iter()
        .map(|param| param.name.clone())
        .collect();
    let mut linker = Linker::new();
    linker.unit(
        Unit::Library(function),
        &params,
        &body.block,
        Some(definition.signature.clone()),
    );
    errors.append(&mut linker.errors);
    let mut graph = linker.graph;
    Typer {
        graph: &mut graph,
        catalog,
        effects: None,
        declared: None,
        errors,
    }
    .run();
    flow::derive(&mut graph, catalog);
    graph
}
