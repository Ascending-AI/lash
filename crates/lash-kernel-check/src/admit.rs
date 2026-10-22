//! Scope/type checking reads document-local facts. Package admission owns fills and visibility.
use crate::{Graph, Refusal, RefusalReason, Refused};
use crate::{link::Linker, typing::Typer};
use lash_kernel_doc::{Document, FunctionCatalog, FunctionDefinition};

use lash_vocabulary::names::DocumentId;
use lash_vocabulary::names::Name;

use lash_vocabulary::sites::Unit;
use std::collections::BTreeSet;

pub struct Environment<'a> {
    pub functions: &'a dyn FunctionCatalog,
    pub bindings: BTreeSet<Name>,
}
impl<'a> Environment<'a> {
    pub fn new(functions: &'a dyn FunctionCatalog) -> Self {
        Self {
            functions,
            bindings: BTreeSet::new(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admitted {
    pub identity: DocumentId,
    pub graph: Graph,
}
pub fn derive(document: &Document, catalog: &dyn FunctionCatalog) -> Result<Graph, Refusal> {
    lash_kernel_doc::validate_document(document, catalog).map_err(|error| Refusal {
        errors: vec![error.into()],
    })?;
    let mut linker = Linker::new(&document.functions);
    if let Some(main) = &document.main {
        linker.unit(Unit::Main, &[], main, None);
    }
    // Anonymous definitions are checked in their lexical parent, with the defining Unit.
    let mut lexical = BTreeSet::new();
    for (_, block) in document.units() {
        let mut nodes = vec![lash_kernel_doc::Node::Block(block)];
        while let Some(node) = nodes.pop() {
            if let lash_kernel_doc::Node::Expr(lash_kernel_doc::Expr::Closure(closure)) = node {
                lexical.insert(closure.function);
            }
            nodes.extend(node.children());
        }
    }
    for (id, function) in &document.functions {
        if lexical.contains(id) {
            continue;
        }
        let Some(body) = function.body() else {
            continue;
        };
        let params = function
            .signature
            .params
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>();
        linker.unit(
            Unit::Library(*id),
            &params,
            &body.block,
            Some(function.signature.clone()),
        );
    }
    let mut errors = linker.errors;
    let mut graph = linker.graph;
    Typer {
        graph: &mut graph,
        presentations: &linker.presentations,
        catalog: &document.functions,
        imports: &document.imports,
        errors: &mut errors,
    }
    .run();
    crate::flow::derive(&mut graph, &document.functions);
    if errors.is_empty() {
        Ok(graph)
    } else {
        Err(Refusal { errors })
    }
}
pub fn derive_view(document: &Document, catalog: &dyn FunctionCatalog) -> Result<Graph, Refusal> {
    derive(document, catalog)
}
pub fn derive_entry(
    document: &Document,
    catalog: &dyn FunctionCatalog,
    _entry: &Name,
) -> Result<Graph, Refusal> {
    derive(document, catalog)
}
pub fn derive_definition(
    definition: &FunctionDefinition,
    catalog: &dyn FunctionCatalog,
) -> Result<Graph, Refusal> {
    lash_kernel_doc::validate_definition(definition, catalog).map_err(|error| Refusal {
        errors: vec![error.into()],
    })?;
    let Some(body) = definition.body() else {
        return Ok(Graph::empty());
    };
    let params = definition
        .signature
        .params
        .iter()
        .map(|p| p.name.clone())
        .collect::<Vec<_>>();
    let mut linker = Linker::new(catalog);
    linker.unit(
        Unit::Library(definition.identity()),
        &params,
        &body.block,
        Some(definition.signature.clone()),
    );
    let mut errors = linker.errors;
    let mut graph = linker.graph;
    Typer {
        graph: &mut graph,
        presentations: &linker.presentations,
        catalog,
        imports: &body.imports,
        errors: &mut errors,
    }
    .run();
    crate::flow::derive(&mut graph, catalog);
    if errors.is_empty() {
        Ok(graph)
    } else {
        Err(Refusal { errors })
    }
}
pub fn admit(document: &Document, environment: &Environment<'_>) -> Result<Admitted, Refusal> {
    let graph = derive(document, environment.functions)?;
    let mut errors = Vec::new();
    for (name, nodes) in graph.session_reads() {
        if !environment.bindings.contains(name) {
            for id in nodes {
                errors.push(Refused {
                    site: Some(graph.node(*id).site.clone()),
                    reason: RefusalReason::UnboundVariable { name: name.clone() },
                });
            }
        }
    }
    if errors.is_empty() {
        Ok(Admitted {
            identity: document.identity(),
            graph,
        })
    } else {
        Err(Refusal { errors })
    }
}
