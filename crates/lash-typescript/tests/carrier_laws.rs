//! Laws L3 and L4 of FIG-3571: one executable program feeds node identity.
//!
//! L3: alpha-renaming private binders (the slots the front end generates and
//! the locals an author binds inside a function) never moves a node path or
//! id inside an unchanged structural owner. A lifted process literal is the
//! named exception: its owner digests its body (ADR 0100 R0), so a rename
//! inside it re-mints its owner, and every id under that owner, on purpose;
//! and the lens, the linker, the stored artifact and the runtime all name that
//! once-minted owner alike.
//!
//! L4: the editor's draft projection of a source and the projection of the
//! artifact that source admits to agree on node ids, node kinds, execution
//! sites and lifted owners; a draft claims no identity; and source that is
//! edited, rendered, reparsed and admitted reaches a fixed point that admits
//! to the same module as the source it came from.
#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-expect-in-tests only exempts #[test] functions, and the helpers around them in this target are test code too"
)]

use std::collections::{BTreeMap, BTreeSet};

#[path = "workflow_graph/goldens.rs"]
mod goldens;

use lash_typescript::workflow_graph::{
    TypeScriptStatementText, workflow_graph_from_artifact, workflow_graph_from_source,
    workflow_graph_to_source,
};
use lashlang::testing::harness::test_environment;
use lashlang::{
    Declaration, Expr, LinkedModule, ModuleArtifact, Program, WorkflowDeclaration, WorkflowGraph,
    WorkflowNodeKind,
};

fn link(source: &str) -> LinkedModule {
    lash_typescript::link(source, &test_environment())
        .unwrap_or_else(|error| panic!("corpus source links: {error}\n{source}"))
}

type NodeFacts = BTreeMap<String, (String, Vec<lashlang::WorkflowExecutionSite>)>;

/// Every node's kind and execution sites, by id, processes included.
fn node_facts(graph: &WorkflowGraph) -> NodeFacts {
    let mut facts = graph
        .nodes()
        .map(|node| {
            (
                node.id.to_string(),
                (kind_tag(&node.kind), node.execution_sites.clone()),
            )
        })
        .collect::<NodeFacts>();
    for declaration in &graph.declarations {
        if let WorkflowDeclaration::Process(process) = declaration {
            facts.insert(process.id.to_string(), ("process".to_string(), Vec::new()));
        }
    }
    facts
}

/// A node kind's closed tag (and a container's kind): the payload holds IR,
/// which the linker resolves in place (a module path to its resource), so the
/// tag is what a draft and its admitted artifact must agree on.
fn kind_tag(kind: &WorkflowNodeKind) -> String {
    let value = serde_json::to_value(kind).expect("node kind serializes");
    let tag = value["kind"].as_str().unwrap_or_default().to_string();
    match value.get("container_kind").and_then(|kind| kind.as_str()) {
        Some(container) => format!("{tag}:{container}"),
        None => tag,
    }
}

fn assert_same_facts(left: &NodeFacts, right: &NodeFacts, context: &str) {
    let differing = left
        .iter()
        .filter(|(id, facts)| right.get(*id) != Some(facts))
        .map(|(id, facts)| format!("{id}: {facts:?} vs {:?}", right.get(id)))
        .chain(
            right
                .keys()
                .filter(|id| !left.contains_key(*id))
                .map(|id| format!("{id}: missing vs {:?}", right.get(id))),
        )
        .collect::<Vec<_>>();
    assert!(differing.is_empty(), "{context}\n{}", differing.join("\n"));
}

fn process_owners(graph: &WorkflowGraph) -> BTreeSet<String> {
    graph
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            WorkflowDeclaration::Process(process) => Some(process.name.clone()),
            _ => None,
        })
        .collect()
}

/// The binders `program` keeps private: its generated slots, and every name
/// an author binds inside a function (a parameter, a local, a loop or catch
/// binding there) that is not also bound at the top level.
fn private_binders(program: &Program) -> BTreeSet<String> {
    fn bound_in_functions(expr: &Expr, inside: bool, names: &mut BTreeSet<String>) {
        let inside = match expr {
            Expr::Function(function) => {
                names.extend(function.params.iter().map(ToString::to_string));
                true
            }
            Expr::Assign { target, .. } if inside && target.is_simple() => {
                names.insert(target.root.to_string());
                inside
            }
            Expr::For { binding, .. } if inside => {
                names.insert(binding.to_string());
                inside
            }
            Expr::Try(scope) if inside => {
                if let Some(catch) = &scope.catch {
                    names.insert(catch.binding.to_string());
                }
                inside
            }
            _ => inside,
        };
        for child in expr.children() {
            bound_in_functions(child, inside, names);
        }
    }
    fn bound_at_top(expr: &Expr, names: &mut BTreeSet<String>) {
        match expr {
            Expr::Function(_) | Expr::ProcessLiteral(_) => return,
            Expr::Assign { target, .. } => {
                names.insert(target.root.to_string());
            }
            Expr::For { binding, .. } => {
                names.insert(binding.to_string());
            }
            _ => {}
        }
        for child in expr.children() {
            bound_at_top(child, names);
        }
    }
    let mut private = program
        .private_bindings
        .iter()
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>();
    let mut top = BTreeSet::new();
    bound_at_top(&program.main, &mut top);
    let mut local = BTreeSet::new();
    bound_in_functions(&program.main, false, &mut local);
    for declaration in &program.declarations {
        match declaration {
            Declaration::Process(process) => bound_in_functions(&process.body, false, &mut local),
            Declaration::Function(function) => bound_in_functions(&function.body, true, &mut local),
            Declaration::Type(_) => {}
        }
    }
    private.extend(local.difference(&top).cloned());
    private
}

/// Consistently renames every private binder of `program`.
fn alpha_rename(program: &Program, private: &BTreeSet<String>) -> Program {
    let rename = |name: &mut lashlang::AstString| {
        if private.contains(name.as_str()) {
            *name = format!("renamed_{}", name.as_str()).into();
        }
    };
    fn walk(expr: &mut Expr, rename: &dyn Fn(&mut lashlang::AstString)) {
        match expr {
            Expr::Variable(name) => rename(name),
            Expr::Assign { target, .. } => rename(&mut target.root),
            Expr::For { binding, .. } => rename(binding),
            Expr::Function(function) => {
                if let Some(name) = function.name.as_mut() {
                    rename(name);
                }
                function.params.iter_mut().for_each(rename);
                function.captures.iter_mut().for_each(rename);
            }
            Expr::Try(scope) => {
                if let Some(catch) = scope.catch.as_mut() {
                    rename(&mut catch.binding);
                }
            }
            _ => {}
        }
        for child in expr.children_mut() {
            walk(child, rename);
        }
    }
    let mut renamed = program.clone();
    walk(&mut renamed.main, &rename);
    for declaration in &mut renamed.declarations {
        match declaration {
            Declaration::Process(process) => walk(&mut process.body, &rename),
            Declaration::Function(function) => walk(&mut function.body, &rename),
            Declaration::Type(_) => {}
        }
    }
    renamed.private_bindings = renamed
        .private_bindings
        .iter()
        .map(|name| {
            let mut name = name.clone();
            rename(&mut name);
            name
        })
        .collect();
    renamed
}

#[test]
fn l3_alpha_renaming_private_binders_preserves_node_ids() {
    let mut authored_locals = BTreeSet::new();
    for source in goldens::CARRIER_LAWS {
        let program = link(source).artifact.ir().clone();
        let private = private_binders(&program);
        authored_locals.extend(
            private
                .iter()
                .filter(|name| !program.private_bindings.contains(name.as_str()))
                .cloned(),
        );
        let renamed = alpha_rename(&program, &private);
        assert_ne!(program, renamed, "the corpus source has private binders");
        let original = lashlang::workflow_graph_from_program(&program, &TypeScriptStatementText);
        let alpha = lashlang::workflow_graph_from_program(&renamed, &TypeScriptStatementText);
        let ids = |graph: &WorkflowGraph| {
            graph
                .nodes()
                .map(|node| node.id.to_string())
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(
            ids(&original),
            ids(&alpha),
            "renaming private binders must not move a node:\n{source}"
        );
    }
    assert!(
        authored_locals.contains("limit") && authored_locals.contains("step"),
        "the corpus renames authored locals, not only generated slots: {authored_locals:?}"
    );
}

/// The owner every stage names a lifted literal by.
struct LiftedOwner {
    lens: String,
    linked: String,
    reloaded: String,
    runtime: BTreeSet<String>,
}

fn lifted_owner(source: &str) -> LiftedOwner {
    let draft = workflow_graph_from_source(source).expect("the variant projects");
    let owners = process_owners(&draft).into_iter().collect::<Vec<_>>();
    let [lens] = owners.as_slice() else {
        panic!("the variant lifts one literal")
    };
    let lens = lens.clone();
    let linked = link(source);
    let lifted = |artifact: &ModuleArtifact| {
        artifact
            .ir()
            .declarations
            .iter()
            .find_map(|declaration| match declaration {
                Declaration::Process(process) if process.origin.is_lifted() => {
                    Some(process.name.to_string())
                }
                _ => None,
            })
            .expect("the artifact declares the lifted literal")
    };
    let reloaded = ModuleArtifact::from_store_bytes(
        &linked
            .artifact
            .to_store_bytes()
            .expect("the artifact encodes"),
    )
    .expect("the stored artifact reloads");
    let name = lifted(&linked.artifact);
    let compiled = lashlang::compile(
        &linked.artifact,
        lashlang::Entry::Process(
            linked
                .artifact
                .process_ref(&name)
                .expect("the lifted literal is exported"),
        ),
        Some(linked.spans()),
    )
    .expect("the lifted process compiles");
    LiftedOwner {
        lens,
        linked: name,
        reloaded: lifted(&reloaded),
        runtime: lashlang::testing::harness::compiled_execution_sites(&compiled)
            .into_iter()
            .map(|site| site.workflow_site.owner.clone())
            .collect(),
    }
}

/// L3's named exception: renaming a binder inside a lifted literal re-mints
/// the literal's owner, and the lens, the linker, the stored artifact and the
/// runtime all name the one owner the variant mints; the main nodes outside
/// the literal keep their ids.
#[test]
fn l3_a_rename_inside_a_lifted_literal_remints_one_owner_everywhere() {
    let variant = |local: &str| {
        format!(
            "const worker = async () => {{\n  const {local} = await tools.echo({{ value: 1 }});\n  return {local};\n}};\nconst started = await processes.start({{ definition: worker }});\nfinish(\"started\");\n"
        )
    };
    let (first, second) = (variant("reply"), variant("answer"));
    let owners = [lifted_owner(&first), lifted_owner(&second)];
    for owner in &owners {
        assert_eq!(owner.lens, owner.linked, "the lens and the linker agree");
        assert_eq!(owner.linked, owner.reloaded, "the stored artifact agrees");
        assert_eq!(
            owner.runtime,
            BTreeSet::from([format!("process:{}", owner.linked)]),
            "the runtime's sites name the same owner"
        );
    }
    assert_ne!(
        owners[0].linked, owners[1].linked,
        "a rename inside the literal re-mints its owner"
    );
    let main_ids = |source: &str| {
        workflow_graph_from_source(source)
            .expect("the variant projects")
            .main
            .nodes
            .iter()
            .map(|node| node.id.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        main_ids(&first),
        main_ids(&second),
        "main nodes outside the literal keep their ids"
    );
}

#[test]
fn l4_draft_and_admitted_projections_agree() {
    for source in goldens::CARRIER_LAWS {
        let draft = workflow_graph_from_source(source).expect("corpus source projects");
        let linked = link(source);
        let admitted = workflow_graph_from_artifact(&linked.artifact);
        assert_eq!(
            process_owners(&draft),
            process_owners(&admitted),
            "lifted owners agree:\n{source}"
        );
        assert_same_facts(
            &node_facts(&draft),
            &node_facts(&admitted),
            &format!("draft and admitted nodes agree on id, kind and execution sites:\n{source}"),
        );
        assert_eq!(draft.source_identity, None, "a draft claims no identity");
        assert_eq!(
            admitted.source_identity,
            Some(linked.artifact.source_identity()),
            "the runnable view names its artifact's identity:\n{source}"
        );
        // Edit/render/reparse/admit reaches a fixed point, and the rendered
        // source admits to the same module as the source it came from.
        let rendered = workflow_graph_to_source(&draft).expect("the draft renders");
        assert_eq!(
            link(&rendered).artifact.source_identity(),
            linked.artifact.source_identity(),
            "the rendered source admits to the source's module:\n{source}\n---\n{rendered}"
        );
        let reprojected = workflow_graph_from_source(&rendered)
            .unwrap_or_else(|error| panic!("rendered source projects: {error}\n{rendered}"));
        assert_same_facts(
            &node_facts(&draft),
            &node_facts(&reprojected),
            &format!("the reprojection is the draft:\n{source}"),
        );
        assert_eq!(
            workflow_graph_to_source(&reprojected).expect("the reprojection renders"),
            rendered,
            "rendering is a fixed point:\n{source}"
        );
    }
}
