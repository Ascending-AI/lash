//! FIG-6940: closure provenance survives the dialect's value conventions.
use lash_kernel_check::{ExprNode, NodeKind, StmtNode, WorkflowDocument};
use lash_kernel_dialect::{CellEnvironment, Environment, Mode};
use lash_kernel_doc::{Name, WorkflowDocumentEntry};

fn lowered_holder(source: &str, typescript: bool, cell: bool) -> WorkflowDocument {
    let embedding = lash_vm_worker::built_embedding(&Default::default()).unwrap();
    let namespaces = lash_kernel_dialect::Namespaces::of_effects(
        &Default::default(),
        &lash_kernel_doc::ContractId::new("1"),
        &Default::default(),
    )
    .expect("no effects are operations");
    let env = Environment::new(embedding.library(), &namespaces);
    let mode = if cell {
        Mode::Cell(CellEnvironment::first())
    } else {
        Mode::Module
    };
    let lowered = if typescript {
        lash_dialect_typescript::lower(source, &env, mode).unwrap()
    } else {
        lash_dialect_python::lower(source, &env, mode).unwrap()
    };
    WorkflowDocument::derive(
        (*lowered.document).clone(),
        if cell {
            WorkflowDocumentEntry::Main
        } else {
            WorkflowDocumentEntry::Entry {
                function: Name::new("demo"),
            }
        },
        env.library,
    )
    .unwrap()
}

fn closure_calls(source: &str, typescript: bool, cell: bool, names: &[&str], together: bool) {
    let holder = lowered_holder(source, typescript, cell);
    let graph = holder.graph();
    let mut wanted = Vec::new();
    for name in names {
        let binding = graph
            .bindings()
            .iter()
            .find(|binding| binding.name.as_str() == *name)
            .unwrap();
        let NodeKind::Stmt(StmtNode::Let { value, .. }) = graph.node(binding.declared_at).kind
        else {
            panic!("let")
        };
        let mut pending = vec![value];
        let closure = loop {
            let id = pending.pop().expect("closure in binding");
            if matches!(
                graph.node(id).kind,
                NodeKind::Expr(ExprNode::Closure { .. })
            ) {
                break graph.node(id).site.clone();
            }
            pending.extend(graph.node(id).children().into_iter().rev());
        };
        wanted.push(closure);
    }
    wanted.sort();
    let view = crate::derive(&holder, &crate::test_packages()).unwrap();
    let actions: Vec<_> = view.dataflow.actions.iter().collect();
    for site in &wanted {
        assert!(
            actions.iter().any(|action| action.applies.contains(site)),
            "missing {site:?}: {actions:#?}"
        );
    }
    if together {
        assert!(
            actions.iter().any(|action| action.applies == wanted),
            "one call must name both in site order: {actions:#?}"
        );
    }
    // A may-call set has one stable spelling, independent of discovery order.
    for action in actions {
        assert!(action.applies.windows(2).all(|pair| pair[0] < pair[1]));
    }
}

#[test]
fn typescript_welcome_is_placed_in_workflows_and_cells() {
    for cell in [false, true] {
        let body = "const welcome = async (n) => { return n; }; await welcome('Ada');";
        let source = if cell {
            body.to_owned()
        } else {
            format!("export async function demo() {{ {body} }}")
        };
        closure_calls(&source, true, cell, &["welcome"], false);
    }
}

#[test]
fn python_welcome_is_placed_in_workflows_and_cells() {
    for cell in [false, true] {
        let source = if cell {
            "async def welcome(n):\n    return n\nawait welcome('Ada')\n"
        } else {
            "async def demo():\n    async def welcome(n):\n        return n\n    return await welcome('Ada')\n"
        };
        closure_calls(source, false, cell, &["welcome"], false);
    }
}

#[test]
fn typescript_map_and_promise_all_place_callbacks() {
    closure_calls(
        "export async function demo() { const work = async (n) => n; return await Promise.all([1, 2].map(work)); }",
        true,
        false,
        &["work"],
        false,
    );
}

#[test]
fn python_comprehension_and_gather_place_callbacks() {
    closure_calls(
        "async def demo():\n    work = lambda n: n\n    return [work(n) for n in [1, 2]]\n",
        false,
        false,
        &["work"],
        false,
    );
    closure_calls(
        "import asyncio\nasync def demo():\n    async def work(n):\n        return n\n    return await asyncio.gather(work(1), work(2))\n",
        false,
        false,
        &["work"],
        false,
    );
}

#[test]
fn typescript_inline_helper_follows_callback_parameters() {
    closure_calls(
        "export async function demo() { const work = (n) => n; const helper = (callback) => callback(1); return helper(work); }",
        true,
        false,
        &["work", "helper"],
        false,
    );
}

#[test]
fn python_inline_helper_follows_callback_parameters() {
    closure_calls(
        "async def demo():\n    work = lambda n: n\n    def helper(callback):\n        return callback(1)\n    return helper(work)\n",
        false,
        false,
        &["work", "helper"],
        false,
    );
}

#[test]
fn typescript_branch_call_lists_both_closures() {
    closure_calls(
        "export async function demo(flag: boolean) { const left = (n) => n; const right = (n) => n + 1; let chosen; if (flag) { chosen = right; } else { chosen = left; } return chosen(1); }",
        true,
        false,
        &["left", "right"],
        true,
    );
}

#[test]
fn python_branch_call_lists_both_closures() {
    closure_calls(
        "async def demo(flag: bool):\n    left = lambda n: n\n    right = lambda n: n + 1\n    if flag:\n        chosen = right\n    else:\n        chosen = left\n    return chosen(1)\n",
        false,
        false,
        &["left", "right"],
        true,
    );
}

fn kernel_view(text: &str) -> crate::WorkflowView {
    let document = lash_kernel_doc::parse_document(text).unwrap();
    let holder = WorkflowDocument::derive(
        document,
        WorkflowDocumentEntry::Entry {
            function: Name::new("flow"),
        },
        &lash_kernel_doc::FunctionRegistry::default(),
    )
    .unwrap();
    crate::derive(&holder, &crate::test_packages()).unwrap()
}

fn action_sites(view: &crate::WorkflowView, statement: u32) -> Vec<lash_kernel_doc::Site> {
    view.dataflow
        .actions
        .iter()
        .find(|action| action.statement == site("flow", &[statement]))
        .unwrap()
        .applies
        .clone()
}

/// A recursive edge loses no other write; host inputs and effect results
/// add no invented identity and cannot erase known document callables.
#[test]
fn writes_union_across_alias_diamonds_cycles_and_outside_values() {
    let view = kernel_view(
        r#"kernel 1
numbers float
effect read() -> Any contract "1"
entry flow(input: Any) -> Any
fn flow(input) {
  let left = fn() { return null }
  let right = fn() { return null }
  let a = left
  let b = a
  set a = b
  set b = right
  set b = input
  set b = perform read() as Any
  let answered = apply a()
  let c = left
  let d = left
  let merged = c
  set merged = d
  let diamond = apply merged()
  let same = left
  set same = left
  let once = apply same()
  let unknown = apply input()
  let empty = null
  let back = empty
  set empty = back
  let cycle = apply empty()
  return null
}
main { finish null }
"#,
    );
    let left = site("flow", &[0, 0]);
    let right = site("flow", &[1, 0]);
    assert_eq!(action_sites(&view, 8), [left.clone(), right]);
    assert_eq!(
        action_sites(&view, 13).as_slice(),
        std::slice::from_ref(&left)
    );
    assert_eq!(action_sites(&view, 16), [left]);
    assert!(action_sites(&view, 17).is_empty());
    assert!(action_sites(&view, 21).is_empty());
}

/// Declared function parameters collect all document callers, including
/// callers reached through function values and recursive forwarding.
#[test]
fn declared_parameters_follow_every_call_site_and_returns() {
    let document = lash_kernel_doc::parse_document(
        r#"kernel 1
numbers float
entry flow() -> Any
fn helper(callback) { let answer = apply callback() return callback }
fn flow() {
  let left = fn() { return null }
  let right = fn() { return null }
  let first = call helper(left)
  let named = &helper
  let second = apply named(right)
  let later = apply first()
  return null
}
main { finish null }
"#,
    )
    .unwrap();
    let holder = WorkflowDocument::derive(
        document,
        WorkflowDocumentEntry::Entry {
            function: Name::new("flow"),
        },
        &lash_kernel_doc::FunctionRegistry::default(),
    )
    .unwrap();
    let view = crate::derive_callable(
        &holder,
        &lash_kernel_doc::Unit::Function(Name::new("helper")),
        &crate::test_packages(),
    )
    .unwrap();
    let wanted = [site("flow", &[0, 0]), site("flow", &[1, 0])];
    assert_eq!(view.dataflow.actions[0].applies, wanted);
    let view = crate::derive(&holder, &crate::test_packages()).unwrap();
    assert_eq!(action_sites(&view, 5), wanted);
    assert_eq!(action_sites(&view, 4), [site("helper", &[])]);
}

/// Library summaries preserve collection membership and distinguish a
/// returned applier from a returned collection containing callables.
#[test]
fn library_collection_elements_and_returned_appliers_keep_their_paths() {
    use lash_kernel_dialect::{NamedLibrary, define_functions};
    let mut library = NamedLibrary::new();
    let definitions = define_functions(
        r#"
function each(callbacks: Any) -> Any
kernel 1
charge 1
body { for callback in callbacks { let answer = apply callback() } return null }
function wrap(callback: Any) -> Any
kernel 1
charge 1
body { let wrapper = fn() { let answer = apply callback() return answer } return wrapper }
function identity(value: Any) -> Any
kernel 1
charge 1
body { return value }
"#,
        &mut library,
    )
    .unwrap();
    let text = format!(
        r#"kernel 1
numbers float
use each = @{}
use wrap = @{}
use identity = @{}
entry flow() -> Any
fn flow() {{
  let left = fn() {{ return null }}
  let right = fn() {{ return null }}
  let together = invoke each([right, left, right])
  let wrapper = invoke wrap(left)
  let wrapped = apply wrapper()
  let alias = invoke identity(right)
  let direct = apply alias()
  return null
}}
main {{ finish null }}
"#,
        definitions[0].identity(),
        definitions[1].identity(),
        definitions[2].identity()
    );
    let document = lash_kernel_doc::parse_document(&text).unwrap();
    let holder = WorkflowDocument::derive(
        document,
        WorkflowDocumentEntry::Entry {
            function: Name::new("flow"),
        },
        &library,
    )
    .unwrap();
    let view = crate::derive(&holder, &crate::test_packages()).unwrap();
    let left = site("flow", &[0, 0]);
    let right = site("flow", &[1, 0]);
    assert_eq!(action_sites(&view, 2), [left.clone(), right.clone()]);
    assert_eq!(action_sites(&view, 4), [left]);
    assert_eq!(action_sites(&view, 6), [right]);
}

fn site(entry: &str, path: &[u32]) -> lash_kernel_doc::Site {
    lash_kernel_doc::Site::new(lash_kernel_doc::Unit::Function(Name::new(entry)), path)
}

/// Aggregates await tasks, while each nested start runs the closure. These
/// laws cover variable-held callbacks and inline member closures separately.
#[test]
fn aggregate_members_name_closures_at_their_starts_not_at_the_join() {
    for (typescript, source, start, aggregate) in [
        (
            true,
            "export async function demo() { const work = async (n) => n; return await Promise.all([work(1), (async (n) => n)(2)]); }",
            "ts.promise.start",
            "ts.promise.all",
        ),
        (
            false,
            "import asyncio\nasync def demo():\n    async def work(n):\n        return n\n    return await asyncio.gather(work(1), work(2))\n",
            "py.task.run",
            "py.gather",
        ),
    ] {
        let holder = lowered_holder(source, typescript, false);
        let view = crate::derive(&holder, &crate::test_packages()).unwrap();
        let named = |name: &str| {
            view.dataflow.actions.iter().filter(|action| {
            matches!(&action.callee, Some(crate::CalleeView::Library { name: called, .. }) if called.as_str() == name)
        }).collect::<Vec<_>>()
        };
        let starts = named(start);
        assert!(!starts.is_empty());
        for action in starts {
            assert_eq!(action.applies.len(), 1, "{start} at {:?}", action.site);
            assert!(
                holder
                    .graph()
                    .node_at(&action.applies[0])
                    .is_some_and(|node| matches!(
                        node.kind,
                        NodeKind::Expr(ExprNode::Closure { .. })
                    ))
            );
        }
        let aggregates = named(aggregate);
        assert_eq!(aggregates.len(), 1);
        assert!(aggregates[0].applies.is_empty());
    }
}

/// Session writes made in this document have provenance. Earlier session
/// values have no identity in WorkflowDocument, even if a declared function
/// happens to have the same name.
#[test]
fn session_writes_resolve_without_guessing_saved_binding_identities() {
    let document = lash_kernel_doc::parse_document(
        r#"kernel 1
numbers float
fn earlier() { return null }
main {
  set current = fn() { return null }
  let known = apply current()
  let outside = apply earlier()
  finish null
}
"#,
    )
    .unwrap();
    let holder = WorkflowDocument::new(document, Default::default()).unwrap();
    let view = crate::derive(&holder, &crate::test_packages()).unwrap();
    assert_eq!(
        view.dataflow.actions[0].applies,
        [lash_kernel_doc::Site::new(
            lash_kernel_doc::Unit::Main,
            [0, 0]
        )]
    );
    assert!(view.dataflow.actions[1].applies.is_empty());
}
