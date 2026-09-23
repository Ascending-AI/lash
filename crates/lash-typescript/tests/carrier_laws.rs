//! Laws L3 and L4 of FIG-3571: one executable program feeds node identity.
//!
//! L3: alpha-renaming private binders never moves a node path or id inside an
//! unchanged structural owner. A lifted process literal is the named
//! exception: its owner digests its body (ADR 0100 R0), so a rename inside it
//! re-mints its owner, and every id under that owner, on purpose.
//!
//! L4: the editor's draft projection of a source and the projection of the
//! artifact that source admits to agree on node ids, lifted owners and, once
//! admitted, source identity.

use std::collections::BTreeSet;

use lash_typescript::workflow_graph::{TypeScriptStatementText, workflow_graph_from_source};
use lashlang::testing::harness::test_environment;
use lashlang::{Declaration, Expr, LinkedModule, Program, WorkflowDeclaration, WorkflowGraph};

/// Sources that exercise every structure the ownership walk distinguishes.
const CORPUS: &[&str] = &[
    // A multi-statement loop body with a branch, a key loop, member
    // assignment in braced and unbraced arms, try, and an array callback.
    r#"const items = [1, 2];
const box = { value: 0 };
for (const item of items) {
  await tools.echo({ value: item });
  if (item > 1) {
    await tools.echo({ value: "then" });
  } else {
    box.value = await tools.echo({ value: "else" });
  }
}
for (const field in box) {
  await tools.echo({ value: field });
  await tools.echo({ value: "keys" });
}
if (box.value === 0) box.value = await tools.echo({ value: 1 });
if (box.value === 1) {
  box.value = await tools.echo({ value: 2 });
}
try {
  await tools.echo({ value: "try" });
} catch (error) {
  await tools.echo({ value: "catch" });
}
const doubled = items.map((item) => item * 2);
finish(doubled);
"#,
    // A process literal with a multi-statement loop, and a nested literal.
    r#"const worker = async (limit: number) => {
  for (const step of [1, 2]) {
    await tools.echo({ value: step });
    await tools.echo({ value: limit });
  }
  const handle = await processes.start({
    definition: async () => {
      await tools.echo({ value: "inner" });
      return 1;
    }
  });
  return 2;
};
const started = await processes.start({ definition: worker, args: { limit: 3 } });
finish("started");
"#,
    // Non-canonically formatted source.
    "const   a=1;for(const x of [a,2]){await tools.echo({value:x});await tools.echo({value:a})}\nfinish(a)",
];

fn link(source: &str) -> LinkedModule {
    let program = lash_typescript::parse(source).expect("corpus source parses");
    LinkedModule::link(program, test_environment()).expect("corpus source links")
}

fn node_ids(graph: &WorkflowGraph) -> BTreeSet<String> {
    let mut ids = graph
        .nodes()
        .map(|node| node.id.to_string())
        .collect::<BTreeSet<_>>();
    for declaration in &graph.declarations {
        if let WorkflowDeclaration::Process(process) = declaration {
            ids.insert(process.id.to_string());
        }
    }
    ids
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

/// Consistently renames every private binder of `program`: every name the
/// front end generated for a scope-local binding.
fn alpha_rename(program: &Program) -> Program {
    fn rename(name: &mut lashlang::AstString) {
        if let Some(rest) = name.as_str().strip_prefix("__typescript_") {
            *name = format!("private_{rest}").into();
        }
    }
    fn walk(expr: &mut Expr) {
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
            walk(child);
        }
    }
    let mut renamed = program.clone();
    walk(&mut renamed.main);
    for declaration in &mut renamed.declarations {
        match declaration {
            Declaration::Process(process) => walk(&mut process.body),
            Declaration::Function(function) => walk(&mut function.body),
            Declaration::Type(_) => {}
        }
    }
    renamed
}

#[test]
fn l3_alpha_renaming_private_binders_preserves_node_ids() {
    for source in CORPUS {
        let program = link(source).program().clone();
        let renamed = alpha_rename(&program);
        assert_ne!(program, renamed, "the corpus source has private binders");
        let original = lashlang::workflow_graph_from_program(&program, &TypeScriptStatementText);
        let alpha = lashlang::workflow_graph_from_program(&renamed, &TypeScriptStatementText);
        let main_ids = |graph: &WorkflowGraph| {
            graph
                .main
                .nodes
                .iter()
                .flat_map(|node| std::iter::once(node).chain(descendants(node)))
                .map(|node| node.id.to_string())
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(
            main_ids(&original),
            main_ids(&alpha),
            "renaming private binders must not move a main node:\n{source}"
        );
    }
}

fn descendants(node: &lashlang::WorkflowNode) -> Vec<&lashlang::WorkflowNode> {
    let mut out = Vec::new();
    if let lashlang::WorkflowNodeKind::Container(container) = &node.kind {
        for (_, child) in container.child_subgraphs() {
            for inner in &child.nodes {
                out.push(inner);
                out.extend(descendants(inner));
            }
        }
    }
    out
}

#[test]
fn l4_draft_and_admitted_projections_agree() {
    for source in CORPUS {
        let draft = workflow_graph_from_source(source).expect("corpus source projects");
        let linked = link(source);
        let admitted =
            lashlang::workflow_graph_from_artifact(&linked.artifact, &TypeScriptStatementText);
        assert_eq!(
            process_owners(&draft),
            process_owners(&admitted),
            "lifted owners agree:\n{source}"
        );
        assert_eq!(
            node_ids(&draft),
            node_ids(&admitted),
            "draft and admitted node ids agree:\n{source}"
        );
        assert_eq!(
            draft.source_identity, admitted.source_identity,
            "draft and admitted source identity agree:\n{source}"
        );
    }
}
