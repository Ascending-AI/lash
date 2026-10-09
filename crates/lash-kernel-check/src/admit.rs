//! Derivation and admission: the two things a caller asks of the checker.

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

/// Links a library function's kernel-code body and derives its view: one
/// unit, [`Unit::Library`] of the definition's identity. A definition with
/// no body derives an empty view.
pub fn derive_definition(
    definition: &FunctionDefinition,
    catalog: &dyn FunctionCatalog,
) -> Result<Graph, Refusal> {
    let mut errors = Vec::new();
    match library(definition, catalog, &mut errors) {
        Some(graph) if errors.is_empty() => Ok(graph),
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
        match environment.effects.get(effect) {
            None => errors.push(refused(
                None,
                RefusalReason::MissingEffect {
                    effect: effect.clone(),
                },
            )),
            Some(provided) if !serves(provided, expected) => errors.push(refused(
                None,
                RefusalReason::EffectSignature {
                    effect: effect.clone(),
                    expected: expected.clone(),
                    provided: provided.clone(),
                },
            )),
            Some(_) => {}
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
    let missing = manifest::missing_functions(document, catalog);
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
    for function in manifest::requirements(document, catalog).functions.keys() {
        if let Some(definition) = catalog.definition(function) {
            library(definition, catalog, errors);
        }
    }
    manifest::check(document, catalog, errors);

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

fn library(
    definition: &FunctionDefinition,
    catalog: &dyn FunctionCatalog,
    errors: &mut Vec<Refused>,
) -> Option<Graph> {
    if let Err(invalid) = validate_definition(definition, catalog) {
        errors.push(invalid.into());
        return None;
    }
    let Some(body) = definition.body() else {
        return Some(Graph::empty());
    };
    // Validation has taken the identity already, to name the body's sites.
    let function = definition.identity().ok()?;
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
    Some(graph)
}
