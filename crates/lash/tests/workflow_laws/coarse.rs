//! COARSE laws: the canvas is a checked-document read, and execution over
//! SQLite supplies action issues independently of statement starts.

use super::*;
use lash::workflow::document::{AssignOrder, Site, parse_document};
use lash::workflow::view::{
    self, ActionObservation, ActionState, FlowKind, FlowTarget, ReadableLens, ReadableRefusal,
    WorkflowNodeBody as Body,
};
use std::collections::BTreeSet;

fn checked(text: &str, _entry: &str) -> lash::workflow::WorkflowDocument {
    let document = parse_document(text).unwrap();
    let link = fixture_link(&document);
    lash::workflow::WorkflowDocument::entered(
        document,
        lash::package::LinkedStart {
            link: link.identity(),
            selector: lash::package::StartSelector::Main,
        },
        lash::vm::helper_release().unwrap().helpers.as_ref(),
    )
    .unwrap()
}
fn fixture_link(document: &Document) -> lash::package::Link {
    let fills = document
        .imports
        .iter()
        .map(|(slot, import)| {
            let lash::package::ImportKind::Operation { signature } = &import.kind else {
                panic!("operation fixture")
            };
            (
                *slot,
                lash::package::Fill::Operation(lash_kernel_value::OperationRequirement::new(
                    signature.clone(),
                    lash_kernel_value::ContractId::new("1"),
                )),
            )
        })
        .collect();
    lash::package::Link {
        helper_fills: Default::default(),
        document: document.identity(),
        fills,
    }
}
fn context(holder: &lash::workflow::WorkflowDocument) -> view::ViewContext {
    let document = holder.document().clone();
    let link = fixture_link(&document);
    let mut content = lash_kernel_package::LinkedContent::default();
    content
        .documents
        .insert(document.identity(), document.into());
    content.links.insert(link.identity(), link.clone());
    view::ViewContext::new(
        holder.reference().clone(),
        content,
        Default::default(),
        Arc::clone(&lash::vm::helper_release().unwrap().helpers),
    )
    .unwrap()
}
fn project(text: &str) -> view::WorkflowView {
    let holder = checked(text, "flow");
    view::derive(&holder, &context(&holder)).unwrap()
}
fn site(view: &view::WorkflowView, path: &[u32]) -> Site {
    Site::new(view.statements[0].site.unit.clone(), path)
}
const HEADER: &str = "kernel 1\nnumbers float\ndefinition \"function answer() -> Int\\nkernel 1\\nmetering statements\\nbody {return 7}\"\ndefinition \"function worker() -> Int\\nkernel 1\\nmetering statements\\nbody {return 1}\"\ndefinition \"function child() -> Int\\nkernel 1\\nmetering statements\\nbody {return 9}\"\ndefinition \"function pair(a: Any,b: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {return b}\"\ndefinition \"function recursive(self: Any,flag: Bool) -> Int\\nkernel 1\\nmetering statements\\nbody {if flag {do apply self(self,false)} return 1}\"\n";

/// COARSE-PARTITION: maximal runs, pure controls, empty arms, shadowed names
/// and lexical closures have one canonical carrier for every statement.
#[test]
fn coarse_partition() {
    let view = project(&format!(
        "{HEADER} main {{
      let x = 1 if true {{ let x = 2 }} else {{}}
      let work = fn() {{ do yield return 1 }}
      do yield let after = 3 return after
    }}"
    ));
    let members: Vec<_> = view
        .nodes
        .iter()
        .flat_map(|node| node.members().iter())
        .collect();
    assert_eq!(members.len(), view.statements.len());
    assert_eq!(
        members.iter().copied().collect::<BTreeSet<_>>().len(),
        members.len()
    );
    assert!(
        view.statements
            .iter()
            .all(|statement| members.contains(&&statement.site))
    );
    assert_eq!(view.callables.len(), 2);
    let entry: Vec<_> = view
        .nodes
        .iter()
        .filter(|node| node.block == site(&view, &[]))
        .collect();
    assert!(matches!(
        entry
            .iter()
            .map(|node| &node.body)
            .collect::<Vec<_>>()
            .as_slice(),
        [Body::Code { .. }, Body::Action { .. }, Body::Code { .. }]
    ));
    assert_eq!(
        entry[0].members().len(),
        4,
        "pure if and its body collapse; closure body is separate"
    );
    assert!(
        view.nodes
            .iter()
            .all(|node| !node.members().is_empty() || matches!(node.body, Body::Action { .. }))
    );
    let shadowed: Vec<_> = view
        .dataflow
        .bindings
        .iter()
        .filter(|binding| binding.name.as_str() == "x")
        .collect();
    assert_eq!(shadowed.len(), 2);
    assert_ne!(shadowed[0].id, shadowed[1].id);
    assert_ne!(shadowed[0].block, shadowed[1].block);
    let writes_view = project(&format!(
        "{HEADER} main {{
        let removed = {{ field: 1 }} let assigned = 0 do yield
        remove removed.field set assigned = 1 set assigned = 2 return [removed, assigned]
    }}"
    ));
    let identities: BTreeSet<_> = writes_view
        .dataflow
        .bindings
        .iter()
        .filter(|binding| ["removed", "assigned"].contains(&binding.name.as_str()))
        .map(|binding| binding.id)
        .collect();
    let code = writes_view
        .nodes
        .iter()
        .find(|node| node.members().contains(&site(&view, &[3])))
        .unwrap();
    assert_eq!(
        code.writes.iter().copied().collect::<BTreeSet<_>>(),
        identities,
        "group writes include member removals and assignments"
    );
    assert_eq!(
        code.writes.len(),
        identities.len(),
        "ports name each binding once"
    );
    assert!(
        identities
            .iter()
            .all(|identity| code.reads.contains(identity))
    );
}

/// COARSE-EVAL: post-order arguments, lazy alternatives, header/return sites
/// and each assignment order's issue of its value and place survive
/// projection.
#[test]
fn coarse_eval() {
    let mut document = parse_document(&format!(
        "{HEADER}
      import 0 operation fixture.one() -> Any import 1 operation fixture.two() -> Any import 2 operation fixture.send(x: Any) -> Any
      import 3 operation fixture.test() -> Bool import 4 operation fixture.items() -> Any import 5 operation fixture.target() -> Any
      import 6 operation fixture.index() -> Any import 7 operation fixture.value() -> Any
      main {{
        let x = perform 2([perform 0() as Any, perform 1() as Any]) as Any
        let y = if(true, perform 0() as Any, perform 1() as Any)
        if perform 3() as Bool {{}} else {{}}
        for item in perform 4() as Any {{ do yield }}
        set x[0] = null
        return perform 0() as Any
      }}"
    ))
    .unwrap();
    let action = |name| {
        Expr::Act(Box::new(lash::workflow::document::ActExpr {
            kind: lash::workflow::document::ActKind::Perform {
                slot: document
                    .imports
                    .iter()
                    .find(|(_, import)| import.name.member.as_str() == name)
                    .unwrap()
                    .0
                    .to_owned(),
                result: lash::schema::Type::Any,
                then_yield: false,
            },
            args: Vec::new(),
        }))
    };
    // K-EVAL-003: a value-first assignment issues its right-hand side
    // before its place's target and index; a place-first one after them.
    let pure_place = match &document.main.as_ref().unwrap()[4] {
        Stmt::Assign { place, .. } => place.clone(),
        _ => unreachable!(),
    };
    let acting_place = Place::Member(lash::workflow::document::Member::Index {
        target: action("target"),
        index: action("index"),
    });
    let value = || lash::workflow::document::Action::Perform {
        slot: lash::package::ImportSlot(7),
        args: Vec::new(),
        result: lash::schema::Type::Any,
        then_yield: false,
    };
    let issued = |assignment: Stmt| {
        let mut candidate = document.clone();
        candidate.main.as_mut().unwrap()[4] = assignment;
        let holder = checked(&lash_kernel_doc::print_document(&candidate), "flow");
        let view = view::derive(&holder, &context(&holder)).unwrap();
        let assignment = view
            .statements
            .iter()
            .find(|statement| matches!(statement.statement, Stmt::Assign { .. }));
        assignment
            .unwrap()
            .actions
            .iter()
            .map(|site| {
                let action = view.dataflow.actions.iter().find(|a| a.site == *site);
                match &action.unwrap().callee {
                    Some(view::CalleeView::Effect(name)) => document.imports[&match name.slot {
                        lash::package::OperationSlot::Import(slot) => slot,
                        _ => panic!("authored operation"),
                    }]
                        .name
                        .member
                        .as_str()
                        .to_owned(),
                    other => format!("{other:?}"),
                }
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        issued(Stmt::Assign {
            place: acting_place.clone(),
            value: Rhs::Action(value()),
            order: AssignOrder::ValueFirst,
        }),
        ["value", "target", "index"]
    );
    assert_eq!(
        issued(Stmt::Assign {
            place: acting_place,
            value: Rhs::Expr(value().into()),
            order: AssignOrder::PlaceFirst,
        }),
        ["target", "index", "value"]
    );
    document.main.as_mut().unwrap()[4] = Stmt::Assign {
        place: pure_place,
        value: Rhs::Action(value()),
        order: AssignOrder::ValueFirst,
    };
    for name in ["target", "index"] {
        document
            .imports
            .retain(|_, import| import.name.member.as_str() != name);
    }
    let holder = checked(&lash_kernel_doc::print_document(&document), "flow");
    let view = view::derive(&holder, &context(&holder)).unwrap();
    let names = |statement: usize| {
        view.statements[statement]
            .actions
            .iter()
            .map(|site| {
                let action = view
                    .dataflow
                    .actions
                    .iter()
                    .find(|action| action.site == *site)
                    .unwrap();
                match &action.callee {
                    Some(view::CalleeView::Effect(name)) => document.imports[&match name.slot {
                        lash::package::OperationSlot::Import(slot) => slot,
                        _ => panic!("authored operation"),
                    }]
                        .name
                        .member
                        .as_str(),
                    _ => "task",
                }
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(names(0), ["one", "two", "send"]);
    let first = view
        .action_node(&view.statements[0].actions[0].position())
        .unwrap();
    let Body::Action { within, .. } = &first.body else {
        panic!("action")
    };
    assert_eq!(*within, Some(view.statements[0].actions[2].clone()));
    let lazy: Vec<_> = view
        .nodes
        .iter()
        .filter(|node| {
            node.statement == site(&view, &[1]) && matches!(node.body, Body::Action { .. })
        })
        .collect();
    assert_eq!(lazy.len(), 2);
    for (node, expected) in lazy.iter().zip([1, 2]) {
        let Body::Action { lazy, .. } = &node.body else {
            panic!("action")
        };
        assert_eq!(lazy[0].arm, expected);
    }
    let control = view
        .nodes
        .iter()
        .find(|node| node.site == site(&view, &[2]))
        .unwrap();
    let Body::Control { header, arms, .. } = &control.body else {
        panic!("control")
    };
    assert_eq!(header.as_ref().unwrap().actions.len(), 1);
    assert_eq!(arms.len(), 2, "empty alternatives retain their roles");
    let assignment = view
        .statements
        .iter()
        .find(|statement| matches!(statement.statement, Stmt::Assign { .. }))
        .unwrap();
    let ordered: Vec<_> = assignment
        .actions
        .iter()
        .map(|site| {
            view.dataflow
                .actions
                .iter()
                .find(|action| action.site == *site)
                .unwrap()
                .callee
                .clone()
        })
        .collect();
    let effect = |name| {
        Some(view::CalleeView::Effect(lash::package::OperationUse {
            link: holder.reference().link(),
            slot: document
                .imports
                .iter()
                .find(|(_, import)| import.name.member.as_str() == name)
                .unwrap()
                .0
                .to_owned()
                .into(),
        }))
    };
    assert_eq!(ordered, [effect("value")]);
    assert!(
        view.nodes
            .iter()
            .any(|node| node.departures == [site(&view, &[5])]
                && matches!(node.body, Body::Action { .. }))
    );
}

/// COARSE-CALL: recursion and dynamic calls are opaque caller actions; lexical
/// closure bodies occur once under their own roots, without inlining callees.
#[test]
fn coarse_call() {
    let text = format!(
        "{HEADER}
      main {{ let callback = fn() {{ do yield return 1 }}
        let x = call recursive(&recursive,true) let y = apply callback() return y }}"
    );
    let view = project(&text);
    assert_eq!(view.dataflow.actions.len(), 3);
    assert_eq!(view.callables.len(), 2);
    assert_eq!(
        view.nodes
            .iter()
            .map(|node| node.site.unit.clone())
            .collect::<BTreeSet<_>>(),
        view.callables
            .iter()
            .map(|callable| callable.block.unit.clone())
            .collect::<BTreeSet<_>>(),
        "each lifted lexical body has its own callable Unit",
    );
    let called = view
        .dataflow
        .actions
        .iter()
        .find(|action| action.site == site(&view, &[1, 0]))
        .unwrap();
    assert_eq!(
        called.callee,
        Some(view::CalleeView::Library {
            id: checked(&text, "flow")
                .document()
                .functions
                .iter()
                .find(|(_, f)| f.name.as_str() == "recursive")
                .unwrap()
                .0
                .to_owned(),
            link: view
                .dataflow
                .actions
                .iter()
                .find_map(|a| match a.callee {
                    Some(view::CalleeView::Library { link, .. }) => Some(link),
                    _ => None,
                })
                .unwrap(),
            name: lash::workflow::document::FunctionName::new("recursive").unwrap(),
            exports: Vec::new()
        })
    );
    let callee = view::derive_callable(
        &checked(&text, "flow"),
        &Unit::Library(
            checked(&text, "flow")
                .document()
                .functions
                .iter()
                .find(|(_, f)| f.name.as_str() == "recursive")
                .unwrap()
                .0
                .to_owned(),
        ),
        &context(&checked(&text, "flow")),
    )
    .unwrap();
    assert_eq!(
        callee
            .nodes
            .iter()
            .filter(|node| matches!(node.body, Body::Action { .. }))
            .count(),
        1
    );
}

/// COARSE-DEPARTURE: grouped pure departures retain exact destinations and
/// finally-unwind edges; a code group never falls through an unconditional exit.
#[test]
fn coarse_departure() {
    let view = project(&format!(
        "{HEADER} main {{
      for x in [1] {{ try {{ if true {{ continue }} else {{ break }} }} finally {{ do yield }} }}
      try {{ throw {{kind: \"x\"}} }} catch error {{ return 1 }} finally {{ do yield }}
      finish 2 fail \"failed\" return 3 do yield
    }}"
    ));
    for kind in [
        FlowKind::Break,
        FlowKind::Continue,
        FlowKind::Throw,
        FlowKind::Return,
        FlowKind::End,
    ] {
        assert!(view.flow.iter().any(|edge| edge.kind == kind), "{kind:?}");
    }
    assert!(
        !view
            .nodes
            .iter()
            .find(|node| node.site == site(&view, &[1]))
            .unwrap()
            .falls_through,
        "a try whose body and catch depart cannot fall through after finally"
    );
    let returned = view
        .flow
        .iter()
        .find(|edge| edge.from == site(&view, &[1, 1, 0]) && edge.kind == FlowKind::Return)
        .unwrap();
    assert!(
        matches!(&returned.to, FlowTarget::Site(destination) if *destination == site(&view, &[1, 2, 0]))
    );
    assert!(
        view.flow
            .iter()
            .any(|edge| edge.from == site(&view, &[1, 2])
                && edge.kind == FlowKind::Return
                && edge.to == FlowTarget::Leave)
    );
    let code = view
        .nodes
        .iter()
        .find(|node| node.members().contains(&site(&view, &[2])))
        .unwrap();
    assert!(matches!(code.body, Body::Code { .. }));
    assert!(!code.falls_through);
    assert!(code.departures.contains(&site(&view, &[2])));
    assert!(
        !view
            .flow
            .iter()
            .any(|edge| edge.from == site(&view, &[2]) && edge.kind == FlowKind::Next)
    );
}

/// COARSE-SPANS / FIG-7476: host views carry no kernel source; readable ranges
/// address only the printed source and cannot be used after reimport.
#[test]
fn coarse_spans() {
    let definition=lash_kernel_doc::parse_definition("function flow() -> Int\nkernel 1\nmetering statements\nbody {let x = 1 if true {let x = 2} return x}").unwrap();
    let mut source = super::function_document(definition, "flow");
    source.main = None;
    let link = fixture_link(&source);
    let document = lash::workflow::WorkflowDocument::entered(
        source,
        lash::package::LinkedStart {
            link: link.identity(),
            selector: lash::package::StartSelector::Export(Name::new("flow")),
        },
        lash::vm::helper_release().unwrap().helpers.as_ref(),
    )
    .unwrap();
    let mut view = view::derive(&document, &context(&document)).unwrap();
    let wire = serde_json::to_value(&view).expect("view serializes");
    assert!(
        wire.get("canonical_source").is_none(),
        "a host view carries no kernel text"
    );
    assert!(
        wire["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|node| node.get("canonical").is_none())
    );
    let original = view.nodes.clone();
    view.refuse_readable(
        "typescript".into(),
        ReadableRefusal::Invalid {
            message: "no readable spelling".into(),
        },
    );
    assert_eq!(view.nodes, original);
    assert!(view.nodes.iter().all(|node| node.readable.is_empty()));
    assert!(
        view.statements
            .iter()
            .all(|statement| statement.readable.is_empty())
    );
    assert!(
        serde_json::to_value(&view).unwrap()["readable"]
            .get("source")
            .is_none()
    );
    let shadowed: Vec<_> = view
        .dataflow
        .bindings
        .iter()
        .filter(|binding| binding.name.as_str() == "x")
        .collect();
    assert_eq!(shadowed.len(), 2);
    assert_ne!(shadowed[0].id, shadowed[1].id);
    assert!(view.nodes[0].bindings.contains(&shadowed[0].id));
    assert!(view.nodes[0].bindings.contains(&shadowed[1].id));
    let embedding = lash_vm_worker::built_embedding(&Default::default()).unwrap();
    let namespaces = Default::default();
    let environment = lash::dialect::Environment::new(embedding.library(), &namespaces);
    let authored = lash::typescript::lower(
        "export async function flow(): number { let x = 1; if (true) { let x = 2; } return x; }",
        &environment,
        lash::dialect::Mode::Module,
    )
    .unwrap()
    .document
    .into_document();
    let link = fixture_link(&authored);
    let document = lash::workflow::WorkflowDocument::entered(
        authored,
        lash::package::LinkedStart {
            link: link.identity(),
            selector: lash::package::StartSelector::Export(Name::new("flow")),
        },
        lash::vm::helper_release().unwrap().helpers.as_ref(),
    )
    .unwrap();
    let mut view = view::derive(&document, &context(&document)).unwrap();
    let printed = lash::typescript::print(
        document.document(),
        None,
        embedding.library(),
        lash::dialect::PrintOptions {
            source_map: true,
            ..Default::default()
        },
    )
    .expect("readable TypeScript");
    let source = printed.source.as_str();
    let map = printed.source_map.expect("requested source map");
    view.readable_ranges(document.document(), "typescript".into(), source, &map)
        .unwrap();
    assert!(
        matches!(&view.readable, ReadableLens::Available { name, source: text }
        if name == "typescript" && text == source)
    );
    let range_valid = |range: &view::LineRange| {
        range.first > 0 && range.first <= range.last && range.last <= source.lines().count()
    };
    assert!(view.nodes.iter().any(|node| !node.readable.is_empty()));
    assert!(
        view.statements
            .iter()
            .any(|statement| !statement.readable.is_empty())
    );
    assert!(
        view.nodes
            .iter()
            .flat_map(|node| &node.readable)
            .all(range_valid)
    );
    assert!(
        view.statements
            .iter()
            .flat_map(|statement| &statement.readable)
            .all(range_valid)
    );
    let wire = serde_json::to_value(&view).unwrap();
    assert!(wire.get("canonical_source").is_none());
    assert_eq!(wire["readable"]["source"], source);
    let changed = checked(&format!("{HEADER} main {{ let x = 99 return x }}"), "flow");
    assert_ne!(view.document, changed.identity());
    assert_eq!(
        view.readable_ranges(changed.document(), "typescript".into(), source, &map),
        Err(lash::workflow::document::SourceMapRefusal::Document)
    );
    view.refuse_readable("typescript".into(), ReadableRefusal::Bytes { limit: 1 });
    assert!(view.nodes.iter().all(|node| node.readable.is_empty()));
    assert!(
        view.statements
            .iter()
            .all(|statement| statement.readable.is_empty())
    );
    assert!(
        serde_json::to_value(&view).unwrap()["readable"]
            .get("source")
            .is_none()
    );
}

async fn launch(
    core: &lash::LashCore,
    text: &str,
    key: &str,
) -> (
    lash_vocabulary::identity::ProcessId,
    lash::workflow::WorkflowDocument,
    view::ViewContext,
) {
    let document = parse_document(text).unwrap();
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let env = environment(false);
    let (packages, namespace) = namespace(core, &pin, &env).await;
    let linked = artifacts
        .publish_workflow(
            &pin,
            &document.clone().into(),
            &lash::package::StartSelector::Main,
            &namespace,
            None,
            None,
        )
        .await
        .unwrap();
    let env = artifacts
        .link_process_env(
            &env,
            &lash::process::LinkTarget::Linked(linked.clone()),
            &packages,
            &Default::default(),
        )
        .await
        .unwrap();
    let env_ref = artifacts.publish_process_env(&pin, &env).await.unwrap();
    let draft = lash::vm::definition_draft(&lash::process::KernelProcessDefinition {
        code: lash::vm::KernelInlineRequest::Linked {
            start: linked.clone(),
        },
    })
    .unwrap();
    let definition = artifacts.publish_definition(&pin, &draft).await.unwrap();
    let settings = core
        .processes()
        .kernel_engine_defaults()
        .unwrap()
        .observing(lash_kernel_value::observation::RunObservation::Counts)
        .into_engine_settings();
    let lash::workflow::WorkflowDocumentRead::Read(document) =
        artifacts.execution_document(&linked).await.unwrap()
    else {
        panic!("retained Main")
    };
    let document = *document;
    let process = core
        .processes()
        .start(
            lash::OperationId::new(key),
            lash::process::ProcessStartRequest::definition(
                definition.id,
                Some(definition.signature),
                serde_json::Map::new(),
                lash::process::EngineLaunch::new(env_ref, Some(settings)),
                lash::process::ProcessOriginator::host(),
                lash::process::Lifetime::Detached,
            ),
        )
        .await
        .unwrap()
        .process_id;
    let context = artifacts
        .workflow_view_context(&document, &env.execution().publications)
        .await
        .unwrap();
    (process, document, context)
}
async fn counts(
    core: &lash::LashCore,
    process: &lash_vocabulary::identity::ProcessId,
) -> view::RanCounts {
    let snapshot = core.processes().observe(process).snapshot().await.unwrap();
    let lash::process::ProcessReadView::Retained(read) = snapshot.read_view else {
        panic!("retained");
    };
    read.workflow.unwrap().ran_counts()
}

/// COARSE-COUNTS: real SQLite execution distinguishes lazy issues, statements,
/// header tests, loop iterations, untaken arms and task actions.
#[tokio::test]
async fn coarse_counts() {
    let core = core().await;
    let (process, document, context) = Box::pin(launch(
        &core,
        &format!(
            "{HEADER}

      main {{
        let answer = if(true, call answer(), call answer())
        let initial = 0 let keep = true while keep {{ set keep = false do yield }}
        if false {{ do yield }} else {{}}
        let task = spawn call worker() let joined = join task do cancel task
        let tasks = [task] let every = join all tasks
        return answer
      }}"
        ),
        "coarse-counts",
    ))
    .await;
    assert_eq!(finished(&core, &process).await, serde_json::json!(7));
    let view = view::derive(&document, &context).unwrap();
    let counts = counts(&core, &process).await;
    let coverage = view.coverage(&counts);
    let runs = |site: &Site| {
        coverage
            .iter()
            .find(|count| count.site == *site)
            .unwrap()
            .runs
    };
    let statement = &view.statements[0];
    assert_eq!(runs(&statement.actions[0]), Some(1));
    assert_eq!(runs(&statement.actions[1]), Some(0));
    let loop_node = view
        .nodes
        .iter()
        .find(|node| {
            matches!(
                view.statements
                    .iter()
                    .find(|s| s.site == node.statement)
                    .unwrap()
                    .statement,
                Stmt::While { .. }
            )
        })
        .unwrap();
    let loop_count = coverage
        .iter()
        .find(|count| count.site == loop_node.site)
        .unwrap();
    assert_eq!(loop_count.condition_tests, Some(2));
    assert_eq!(loop_count.arms[0].entries, Some(1));
    let branch = view
        .nodes
        .iter()
        .find(|node| {
            matches!(node.body, Body::Control { .. })
                && matches!(
                    view.statements
                        .iter()
                        .find(|s| s.site == node.statement)
                        .unwrap()
                        .statement,
                    Stmt::If { .. }
                )
        })
        .unwrap();
    let branch_count = coverage
        .iter()
        .find(|count| count.site == branch.site)
        .unwrap();
    assert_eq!(
        branch_count
            .arms
            .iter()
            .map(|arm| arm.entries)
            .collect::<Vec<_>>(),
        [Some(0), Some(1)]
    );
    for action in view.dataflow.actions.iter().filter(|action| {
        matches!(
            action.kind,
            view::SiteKind::Spawn
                | view::SiteKind::Cancel
                | view::SiteKind::Join
                | view::SiteKind::JoinMany
        )
    }) {
        assert_eq!(runs(&action.site), Some(1), "{:?}", action.kind);
    }
    for observation in [None, Some(lash::process::RunObservation::Off)] {
        assert!(
            view.coverage(&view::RanCounts {
                observation,
                ran: counts.ran.clone()
            })
            .iter()
            .all(|count| count.runs.is_none())
        );
    }
    let code = view
        .nodes
        .iter()
        .find(|node| matches!(node.body, Body::Code { .. }) && node.members().len() > 1)
        .unwrap();
    let first = view
        .statements
        .iter()
        .find(|s| s.site == code.statement)
        .unwrap()
        .ordinal;
    assert_eq!(
        runs(&code.site),
        counts
            .ran
            .iter()
            .find(|(ordinal, _)| *ordinal == first)
            .map(|(_, count)| *count)
    );
    core.shutdown().await.unwrap();
}

/// COARSE-RESUME: a nested sleep parks before sibling and parent issue. A cold
/// SQLite reopen and delivery count it once; concurrent occurrence attribution
/// keeps waits and outcome evidence on the exact action and task.
#[tokio::test]
async fn coarse_resume() {
    let files = Files::new();
    let first = core_over(files.open().await, "coarse-first").await;
    let (process, document, context) = Box::pin(launch(
        &first,
        &format!(
            "{HEADER}
      import 0 operation typescript.sleep(milliseconds: Int) -> Null
      main {{
        let worker = fn() {{ let out = call pair(perform 0(5) as Null, call child()) return out }}
        let first = spawn apply worker() let second = spawn apply worker()
        let tasks = [first, second] let results = join all tasks return results
      }}"
        ),
        "coarse-resume",
    ))
    .await;
    let due = timer_park_after(&first, &process, 0).await;
    let view = view::derive(&document, &context).unwrap();
    let actions = &view
        .statements
        .iter()
        .find(|statement| statement.actions.len() == 3)
        .unwrap()
        .actions;
    assert_eq!(actions.len(), 3);
    let parked = view.coverage(&counts(&first, &process).await);
    let runs = |coverage: &[view::NodeCoverage], site: &Site| {
        coverage
            .iter()
            .find(|count| count.site == *site)
            .unwrap()
            .runs
    };
    assert_eq!(runs(&parked, &actions[0]), Some(2));
    assert_eq!(runs(&parked, &actions[1]), Some(0));
    assert_eq!(runs(&parked, &actions[2]), Some(0));
    let snapshot = first
        .processes()
        .observe(&process)
        .snapshot()
        .await
        .unwrap();
    let lash::process::ProcessReadView::Retained(read) = snapshot.read_view else {
        panic!("retained");
    };
    let sleeping_tasks: Vec<_> = read
        .workflow
        .unwrap()
        .sites
        .into_iter()
        .filter(|site| site.at.position() == actions[0])
        .map(|site| site.at)
        .collect();
    assert_eq!(sleeping_tasks.len(), 2);
    assert_ne!(sleeping_tasks[0].task(), sleeping_tasks[1].task());
    first.shutdown().await.unwrap();
    drop(first);
    files.clock.set(due);
    let second = core_over(files.open().await, "coarse-second").await;
    assert_eq!(finished(&second, &process).await, serde_json::json!([9, 9]));
    let resumed = view.coverage(&counts(&second, &process).await);
    for site in actions {
        assert_eq!(runs(&resumed, site), Some(2));
    }
    let identity = |site: Site, task, occurrence| {
        lash::workflow::document::testkit::effect(task, site, occurrence, Vec::new())
    };
    let observations = [
        ActionObservation {
            origin: sleeping_tasks[0].position().site().clone(),
            identity: sleeping_tasks[0].clone(),
            state: ActionState::Completed,
            outcome: Some("sleep delivered"),
        },
        ActionObservation {
            origin: sleeping_tasks[1].position().site().clone(),
            identity: sleeping_tasks[1].clone(),
            state: ActionState::Waiting,
            outcome: None,
        },
        ActionObservation {
            origin: actions[1].clone(),
            identity: identity(
                actions[1].clone(),
                lash::workflow::document::testkit::root_task(),
                0,
            ),
            state: ActionState::Failed,
            outcome: Some("child failure"),
        },
    ];
    let activity = view.activity(view.document, &observations).unwrap();
    let sleeping = activity
        .iter()
        .find(|node| node.site == actions[0])
        .unwrap();
    assert_eq!(sleeping.state, Some(ActionState::Waiting));
    assert_eq!(sleeping.occurrences.len(), 2);
    assert_eq!(
        activity
            .iter()
            .find(|node| node.site == actions[2])
            .unwrap()
            .state,
        None
    );
    assert_eq!(
        activity
            .iter()
            .find(|node| node.site == actions[1])
            .unwrap()
            .occurrences[0]
            .outcome,
        Some("child failure")
    );
    second.shutdown().await.unwrap();
}
