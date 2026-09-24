//! Structural invariants of every admitted artifact, over every corpus.
//!
//! Each program of every corpus is admitted (linked) and its artifact is held
//! to the invariants the carrier arc (FIG-3570) rests on:
//!
//! 1. declaration names are unique, and each lifted literal is declared
//!    exactly once, at one site;
//! 2. every compiled execution site (instruction and batch tables, main and
//!    every process) is a node of the trace map — the artifact's projection,
//!    flattened as the trace skeleton flattens it — with the same kind, owner
//!    path and branch memberships, and the map holds nothing else (L1,
//!    generalized from one corpus to all of them);
//! 3. only session-visible bindings are exported: every generated slot is
//!    private, and a session cell exports exactly the names its session's
//!    Node answer binds;
//! 4. a `ProcessOrigin` is derived, never authored: each lifted declaration
//!    is the literal at its site, by digest and hidden parameters, and
//!    re-admission derives the same module;
//! 5. a draft projection carries no artifact identity, and the admitted view
//!    carries exactly its artifact's;
//! 6. the artifact is sufficient on its own: its stored bytes reload to an
//!    equal artifact that compiles to the same inventory, with no front end
//!    consulted (the code-level boundary is `scripts/check-workflow-graph-model.sh`,
//!    the dependency guard).
//!
//! Beside the corpora, one direct-IR program holds a literal inside a
//! *declared* process, the shape where the linker once declared a lifted
//! literal twice (FIG-3571 phase 1); TypeScript never declares a process, so
//! no TypeScript corpus can reach it.

use std::collections::{BTreeMap, BTreeSet};

use lashlang::{
    Declaration, Entry, Expr, LinkedModule, ModuleArtifact, ProcessOrigin, WorkflowContainer,
    WorkflowNodeKind, WorkflowSubgraph,
};

use super::corpora::{self, CorpusProgram};
use super::{RESERVED_PREFIX, sessions};

/// A compiled or mapped site: node id, kind, owner and path, and the branch
/// arms (branch node id, `then`) it sits in.
type Site = (
    String,
    lash_sansio::ExecutionNodeKind,
    lash_sansio::WorkflowExecutionSite,
    BTreeSet<(String, bool)>,
);

/// What a session cell's Node answer says it binds: the names bound after
/// it, and the names whose value reaches a function (which the
/// `closure-boundary` entry drops at the cell's end).
struct SessionAnswer {
    bound_after: BTreeSet<String>,
    closures: BTreeSet<String>,
    /// Whether the cell ran to its end; a cell that throws declares names it
    /// never reaches.
    completes: bool,
}

fn check(program: &CorpusProgram, session: Option<&SessionAnswer>) -> Vec<String> {
    let environment = program.environment();
    let linked = match lash_typescript::link(&program.source, &environment) {
        Ok(linked) => linked,
        Err(error) => return vec![format!("does not admit: {error}")],
    };
    let mut failures = check_artifact(&linked);
    let ir = linked.artifact.ir();

    // 3. Only session-visible bindings are exported.
    let exported = exported_names(ir);
    let generated = exported
        .iter()
        .filter(|name| name.starts_with(RESERVED_PREFIX))
        .collect::<Vec<_>>();
    if !generated.is_empty() {
        failures.push(format!("generated slots are exported: {generated:?}"));
    }
    if let Some(answer) = session {
        let new = answer
            .bound_after
            .difference(&program.globals)
            .cloned()
            .collect::<BTreeSet<_>>();
        let missing = new.difference(&exported).collect::<Vec<_>>();
        if !missing.is_empty() {
            failures.push(format!(
                "the cell binds {missing:?} but does not export them"
            ));
        }
        let stray = exported
            .iter()
            .filter(|name| !answer.bound_after.contains(*name) && !answer.closures.contains(*name))
            .collect::<Vec<_>>();
        if answer.completes && !stray.is_empty() {
            failures.push(format!("exported, yet no session binding: {stray:?}"));
        }
    }

    // 4. Origins are the literals at their sites.
    match lash_typescript::parse_with_globals(&program.source, &program.globals) {
        Ok(draft) => failures.extend(origins_are_derived(ir, &draft)),
        Err(error) => failures.push(format!("the draft does not parse: {error}")),
    }

    // 5. Drafts claim no identity; the admitted view claims its artifact's.
    // A draft projects through the printer's canonical text, so a program
    // the printer refuses (an allowlisted refusal of the round-trip law) has
    // no draft to hold to this.
    if program.globals.is_empty() {
        match lash_typescript::workflow_graph::workflow_graph_from_source(&program.source) {
            Ok(draft) if draft.source_identity.is_some() => {
                failures.push("a draft projection carries an artifact identity".to_string());
            }
            Ok(_)
            | Err(lash_typescript::workflow_graph::WorkflowGraphBuildError::CanonicalSource(_)) => {
            }
            Err(error) => failures.push(format!("the draft does not project: {error}")),
        }
    }
    let admitted = lash_typescript::workflow_graph::workflow_graph_from_artifact(&linked.artifact);
    if admitted.source_identity.as_deref() != Some(linked.artifact.source_identity().as_str()) {
        failures.push("the admitted view does not carry its artifact's identity".to_string());
    }
    // A host's view of source that admits is the admitted view: its
    // structure and its identity both come from the artifact, never a draft's
    // structure under the artifact's identity.
    if program.globals.is_empty() {
        match lash_typescript::workflow_graph::workflow_graph_from_source_with_facets(
            &program.source,
            Some(&environment),
        ) {
            Ok(view) => {
                if view.source_identity != admitted.source_identity {
                    failures.push("the host view does not carry the admitted identity".to_string());
                }
                if let Some(difference) = super::first_difference(
                    &without_facets(&view),
                    &without_facets(&admitted),
                    "graph",
                ) {
                    failures.push(format!(
                        "the host view carries the artifact's identity over another graph ({difference})"
                    ));
                }
            }
            Err(lash_typescript::workflow_graph::WorkflowGraphBuildError::CanonicalSource(_)) => {}
            Err(error) => failures.push(format!("the host view does not project: {error}")),
        }
    }
    failures
}

/// A graph as JSON without its host-derived type facets: the one thing a
/// host's view adds to the admitted view.
fn without_facets(graph: &lashlang::WorkflowGraph) -> serde_json::Value {
    fn strip(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(fields) => {
                fields.remove("type_facets");
                fields.remove("facet_schema_version");
                fields.values_mut().for_each(strip);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let mut value = serde_json::to_value(graph).expect("a graph serializes");
    strip(&mut value);
    value
}

/// Invariants 1, 2 and 6 over one linked module.
fn check_artifact(linked: &LinkedModule) -> Vec<String> {
    let artifact = &linked.artifact;
    let ir = artifact.ir();
    let mut failures = Vec::new();

    // 1. Unique declarations; each lifted literal declared once, at one site.
    let mut names = BTreeMap::<String, usize>::new();
    let mut sites = BTreeMap::<(String, Vec<u32>), usize>::new();
    for declaration in &ir.declarations {
        let name = match declaration {
            Declaration::Process(process) => {
                if let ProcessOrigin::Lifted { site, .. } = &process.origin {
                    *sites
                        .entry((format!("{:?}", site.root), site.steps.clone()))
                        .or_default() += 1;
                }
                process.name.to_string()
            }
            Declaration::Function(function) => function.name.to_string(),
            Declaration::Type(ty) => ty.name.to_string(),
        };
        *names.entry(name).or_default() += 1;
    }
    for (name, count) in names.iter().filter(|(_, count)| **count > 1) {
        failures.push(format!("`{name}` is declared {count} times"));
    }
    for (site, count) in sites.iter().filter(|(_, count)| **count > 1) {
        failures.push(format!("the literal at {site:?} is lifted {count} times"));
    }

    // 2. The compiled inventory is the trace map, entry by entry.
    let graph = lashlang::workflow_graph_from_artifact(artifact, &lashlang::NoStatementText);
    let mut entries = vec![("main".to_string(), Entry::Main, &graph.main)];
    for declaration in &ir.declarations {
        if let Declaration::Process(process) = declaration {
            let (Some(reference), Some(projected)) = (
                artifact.process_ref(process.name.as_str()),
                graph.process(process.name.as_str()),
            ) else {
                failures.push(format!(
                    "process `{}` is not exported and projected",
                    process.name
                ));
                continue;
            };
            entries.push((
                format!("process {}", process.name),
                Entry::Process(reference),
                &projected.body,
            ));
        }
    }
    for (label, entry, subgraph) in entries {
        let compiled = match lashlang::compile(artifact, entry, Some(linked.spans())) {
            Ok(compiled) => compiled,
            Err(error) => {
                failures.push(format!("{label} does not compile: {error}"));
                continue;
            }
        };
        let compiled_sites = compiled_sites(&compiled);
        let mapped = mapped_sites(subgraph);
        for site in compiled_sites.difference(&mapped) {
            failures.push(format!(
                "{label}: compiled site {site:?} is not mapped as compiled"
            ));
        }
        for site in mapped.difference(&compiled_sites) {
            failures.push(format!("{label}: mapped site {site:?} is no compiled site"));
        }
    }

    // 6. The stored artifact is sufficient on its own: the verifying decoder
    // re-admits it (origins included) to the same module and bytes.
    let stored = artifact.to_store_bytes().map_err(|error| error.to_string());
    match stored.clone().and_then(|bytes| {
        ModuleArtifact::from_store_bytes(&bytes).map_err(|error| error.to_string())
    }) {
        Ok(reloaded)
            if reloaded.module_ref() == artifact.module_ref()
                && reloaded.source_identity() == artifact.source_identity()
                && reloaded.to_store_bytes().ok() == stored.ok() =>
        {
            if let Err(error) = lashlang::compile(&reloaded, Entry::Main, None) {
                failures.push(format!("the reloaded artifact does not compile: {error}"));
            }
        }
        Ok(_) => failures.push("the stored artifact reloads to another artifact".to_string()),
        Err(error) => failures.push(format!("the artifact does not store and reload: {error}")),
    }
    failures
}

/// The compiled sites of one entry, each with the branch arms that enclose
/// it in the IR: a site under a compiled branch's `then` or `else` child.
fn compiled_sites(compiled: &lashlang::CompiledProgram) -> BTreeSet<Site> {
    let sites = lashlang::testing::harness::compiled_execution_sites(compiled);
    let branches = sites
        .iter()
        .filter(|site| site.node_kind == lash_sansio::ExecutionNodeKind::Branch)
        .map(|site| {
            (
                site.workflow_site.owner.clone(),
                site.workflow_site.path.clone(),
                site.node_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    sites
        .iter()
        .map(|site| {
            let arms = branches
                .iter()
                .filter(|(owner, path, _)| {
                    *owner == site.workflow_site.owner
                        && site.workflow_site.path.len() > path.len() + 1
                        && site.workflow_site.path.starts_with(path)
                })
                .filter_map(|(_, path, id)| match site.workflow_site.path[path.len()] {
                    1 => Some((id.clone(), true)),
                    2 => Some((id.clone(), false)),
                    _ => None,
                })
                .collect();
            (
                site.node_id.clone(),
                site.node_kind,
                site.workflow_site.clone(),
                arms,
            )
        })
        .collect()
}

/// The trace map of one subgraph, as the trace skeleton flattens it: every
/// node's execution sites, with the arms of the `if` containers above it.
fn mapped_sites(graph: &WorkflowSubgraph) -> BTreeSet<Site> {
    fn walk(graph: &WorkflowSubgraph, arms: &BTreeSet<(String, bool)>, out: &mut BTreeSet<Site>) {
        for node in &graph.nodes {
            for site in &node.execution_sites {
                out.insert((node.id.to_string(), site.kind, site.clone(), arms.clone()));
            }
            if let WorkflowNodeKind::Container(container) = &node.kind {
                for (slot, child) in container.child_subgraphs() {
                    let mut inner = arms.clone();
                    if let WorkflowContainer::If { .. } = container {
                        inner.insert((node.id.to_string(), slot == "then"));
                    }
                    walk(child, &inner, out);
                }
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(graph, &BTreeSet::new(), &mut out);
    out
}

/// The names `main` binds outside every function and process body, less the
/// ones the program marks private: what a cell exports to its session.
fn exported_names(ir: &lashlang::Program) -> BTreeSet<String> {
    fn walk(expr: &Expr, names: &mut BTreeSet<String>) {
        match expr {
            Expr::Function(_) | Expr::ProcessLiteral(_) => return,
            Expr::Assign { target, .. } => {
                names.insert(target.root.to_string());
            }
            Expr::For { binding, .. } => {
                names.insert(binding.to_string());
            }
            Expr::Try(scope) => {
                if let Some(catch) = &scope.catch {
                    names.insert(catch.binding.to_string());
                }
            }
            _ => {}
        }
        for child in expr.children() {
            walk(child, names);
        }
    }
    // A `globalThis.name` write inside a function reaches the session through
    // the root-global set intrinsic when it runs (FIG-3620), so it exports the
    // name it spells from wherever it sits.
    fn walk_global_sets(expr: &Expr, names: &mut BTreeSet<String>) {
        if let Expr::BuiltinCall { name, args } = expr
            && name.as_str() == "__typescript_global_set"
            && let Some(Expr::String(global)) = args.first()
        {
            names.insert(global.to_string());
        }
        for child in expr.children() {
            walk_global_sets(child, names);
        }
    }
    let mut names = BTreeSet::new();
    walk(&ir.main, &mut names);
    walk_global_sets(&ir.main, &mut names);
    names
        .into_iter()
        .filter(|name| !ir.private_bindings.contains(name.as_str()))
        .collect()
}

/// Every lifted declaration is the literal the draft holds at its site: the
/// literal digests to the declaration's name and carries as many hidden
/// arguments as the declaration has hidden parameters. TypeScript declares
/// no process.
fn origins_are_derived(ir: &lashlang::Program, draft: &lashlang::Program) -> Vec<String> {
    let mut failures = Vec::new();
    for declaration in &ir.declarations {
        let Declaration::Process(process) = declaration else {
            continue;
        };
        let ProcessOrigin::Lifted {
            site,
            hidden_params,
        } = &process.origin
        else {
            failures.push(format!(
                "`{}` is a declared process; TypeScript declares none",
                process.name
            ));
            continue;
        };
        let literal = (site.root == lashlang::AstRoot::Main)
            .then(|| expr_at(&draft.main, &site.steps))
            .flatten();
        match literal {
            Some(Expr::ProcessLiteral(literal))
                if lashlang::lifted_process_identity(&literal.body, &site.steps)
                    == process.name.as_str()
                    && literal.hidden_args.len() == *hidden_params as usize => {}
            _ => failures.push(format!(
                "`{}` does not derive from the literal at its site {:?}",
                process.name, site
            )),
        }
    }
    failures
}

fn expr_at<'e>(expr: &'e Expr, steps: &[u32]) -> Option<&'e Expr> {
    match steps.split_first() {
        None => Some(expr),
        Some((step, rest)) => expr_at(expr.children().nth(*step as usize)?, rest),
    }
}

#[test]
fn every_admitted_artifact_holds_the_structural_invariants() {
    let bound_after = session_bindings();
    let mut failures = Vec::new();
    let mut checked = 0usize;
    for program in corpora::all() {
        // A program the round-trip law records as not admitting is that
        // law's concern; every admitted one is held to the invariants here.
        let problems = check(&program, bound_after.get(&program.id));
        checked += 1;
        failures.extend(
            problems
                .into_iter()
                .map(|problem| format!("{}: {problem}", program.id)),
        );
    }
    let direct = match declared_process_holding_a_literal() {
        Ok(linked) => check_artifact(&linked),
        Err(error) => vec![format!("does not admit: {error}")],
    };
    failures.extend(
        direct
            .into_iter()
            .map(|problem| format!("direct-ir:declared-process-literal: {problem}")),
    );
    assert!(
        failures.is_empty(),
        "{} of {checked} programs break an invariant:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Each session cell's Node answer, by program id. A cell with a stated lash
/// answer deviates from Node by a registered entry and ends its session.
fn session_bindings() -> BTreeMap<String, SessionAnswer> {
    let mut out = BTreeMap::new();
    for session in sessions::corpus().sessions {
        for (index, cell) in session.cells.iter().enumerate() {
            if cell.reject.is_none() && cell.lash.is_none() {
                out.insert(
                    format!("session:{}:{index}", session.id),
                    SessionAnswer {
                        bound_after: session.bound_after(cell),
                        closures: cell.node.closures.iter().cloned().collect(),
                        completes: matches!(cell.node.outcome.as_str(), "normal" | "finish"),
                    },
                );
            }
        }
    }
    out
}

/// A declared process whose body holds a literal: TypeScript lowered, then
/// its outer literal made a declaration of the direct IR.
fn declared_process_holding_a_literal() -> Result<LinkedModule, lashlang::LinkError> {
    let authored = "const worker=async()=>{const inner=async()=>{await sleep(2);return 2;};await sleep(1);return 1;};";
    let mut program = lash_typescript::parse(authored).expect("the fixture parses");
    let Expr::Block(statements) = &mut program.main else {
        panic!("a lowered program's main is a block")
    };
    let [Expr::Assign { expr, .. }] = statements.as_mut_slice() else {
        panic!("the fixture binds one process literal")
    };
    let Expr::ProcessLiteral(literal) = std::mem::replace(
        expr.as_mut(),
        Expr::ProcessRef {
            process: "worker".into(),
        },
    ) else {
        panic!("the fixture binds a process literal")
    };
    program
        .declarations
        .push(Declaration::Process(lashlang::ProcessDecl {
            name: "worker".into(),
            params: Vec::new(),
            signals: Vec::new(),
            return_ty: None,
            label: None,
            origin: ProcessOrigin::Declared,
            body: *literal.body,
        }));
    LinkedModule::link(program, lashlang::testing::harness::test_environment())
}
