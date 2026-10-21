//! COARSE laws: the canvas is a checked-document read, and execution over
//! SQLite supplies action issues independently of statement starts.

use super::*;
use lash::workflow::document::{AssignOrder, FunctionRegistry, Site, parse_document};
use lash::workflow::view::{
    self, ActionObservation, ActionState, FlowKind, FlowTarget, ReadableLens, ReadableRefusal,
    WorkflowNodeKind as Kind,
};
use std::collections::BTreeSet;

fn checked(text: &str, entry: &str) -> lash::workflow::WorkflowDocument {
    lash::workflow::WorkflowDocument::derive(
        parse_document(text).unwrap(),
        WorkflowDocumentEntry::Entry {
            function: Name::new(entry),
        },
        &FunctionRegistry::default(),
    )
    .unwrap()
}
fn project(text: &str) -> view::WorkflowView {
    view::derive(&checked(text, "flow"), &super::test_packages()).unwrap()
}
fn site(path: &[u32]) -> Site {
    Site::new(Unit::Function(Name::new("flow")), path)
}
const HEADER: &str = "kernel 1\nnumbers float\nentry flow() -> Any\n";

/// COARSE-PARTITION: maximal runs, pure controls, empty arms, shadowed names
/// and lexical closures have one canonical carrier for every statement.
#[test]
fn coarse_partition() {
    let view = project(&format!(
        "{HEADER} fn flow() {{
      let x = 1 if true {{ let x = 2 }} else {{}}
      let work = fn() {{ do yield return 1 }}
      do yield let after = 3 return after
    }} main {{ finish null }}"
    ));
    let members: Vec<_> = view
        .nodes
        .iter()
        .flat_map(|node| node.members.iter())
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
        .filter(|node| node.block == site(&[]))
        .collect();
    assert_eq!(
        entry.iter().map(|node| node.kind).collect::<Vec<_>>(),
        [Kind::Code, Kind::Action, Kind::Code]
    );
    assert_eq!(
        entry[0].members.len(),
        4,
        "pure if and its body collapse; closure body is separate"
    );
    assert!(
        view.nodes
            .iter()
            .all(|node| !node.members.is_empty() || node.kind == Kind::Action)
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
        "{HEADER} fn flow() {{
        let removed = {{ field: 1 }} let assigned = 0 do yield
        remove removed.field set assigned = 1 set assigned = 2 return [removed, assigned]
    }} main {{ finish null }}"
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
        .find(|node| node.members.contains(&site(&[3])))
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
      effect one() -> Any contract \"1\" effect two() -> Any contract \"1\" effect send(x: Any) -> Any contract \"1\"
      effect test() -> Bool contract \"1\" effect items() -> Any contract \"1\" effect target() -> Any contract \"1\"
      effect index() -> Any contract \"1\" effect value() -> Any contract \"1\"
      fn flow() {{
        let x = perform send([perform one() as Any, perform two() as Any]) as Any
        let y = if(true, perform one() as Any, perform two() as Any)
        if perform test() as Bool {{}} else {{}}
        for item in perform items() as Any {{ do yield }}
        set x[0] = null
        return perform one() as Any
      }} main {{ finish null }}"
    ))
    .unwrap();
    let action = |name| {
        Expr::Act(Box::new(lash::workflow::document::ActExpr {
            kind: lash::workflow::document::ActKind::Perform {
                effect: lash::workflow::document::EffectName::new(name).unwrap(),
                result: lash::workflow::document::Type::Any,
                then_yield: false,
            },
            args: Vec::new(),
        }))
    };
    // K-EVAL-003: a value-first assignment issues its right-hand side
    // before its place's target and index; a place-first one after them.
    let pure_place = match &document.functions[&Name::new("flow")].body[4] {
        Stmt::Assign { place, .. } => place.clone(),
        _ => unreachable!(),
    };
    let acting_place = Place::Member(lash::workflow::document::Member::Index {
        target: action("target"),
        index: action("index"),
    });
    let value = || lash::workflow::document::Action::Perform {
        effect: lash::workflow::document::EffectName::new("value").unwrap(),
        args: Vec::new(),
        result: lash::workflow::document::Type::Any,
        then_yield: false,
    };
    let issued = |assignment: Stmt| {
        let mut candidate = document.clone();
        candidate
            .functions
            .get_mut(&Name::new("flow"))
            .unwrap()
            .body[4] = assignment;
        let holder = lash::workflow::WorkflowDocument::derive(
            candidate,
            WorkflowDocumentEntry::Entry {
                function: Name::new("flow"),
            },
            &FunctionRegistry::default(),
        )
        .unwrap();
        let view = view::derive(&holder, &super::test_packages()).unwrap();
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
                    Some(view::CalleeView::Effect(name)) => name.as_str().to_owned(),
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
    document.functions.get_mut(&Name::new("flow")).unwrap().body[4] = Stmt::Assign {
        place: pure_place,
        value: Rhs::Action(value()),
        order: AssignOrder::ValueFirst,
    };
    for name in ["target", "index"] {
        document
            .manifest
            .effects
            .remove(&lash::workflow::document::EffectName::new(name).unwrap());
    }
    let holder = lash::workflow::WorkflowDocument::derive(
        document,
        WorkflowDocumentEntry::Entry {
            function: Name::new("flow"),
        },
        &FunctionRegistry::default(),
    )
    .unwrap();
    let view = view::derive(&holder, &super::test_packages()).unwrap();
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
                    Some(view::CalleeView::Effect(name)) => name.as_str(),
                    _ => "task",
                }
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(names(0), ["one", "two", "send"]);
    let first = view
        .action_node(&view.statements[0].actions[0].position())
        .unwrap();
    assert_eq!(first.within, Some(view.statements[0].actions[2].clone()));
    let lazy: Vec<_> = view
        .nodes
        .iter()
        .filter(|node| node.statement == site(&[1]) && node.kind == Kind::Action)
        .collect();
    assert_eq!(lazy.len(), 2);
    assert_eq!(lazy[0].lazy[0].arm, 1);
    assert_eq!(lazy[1].lazy[0].arm, 2);
    let control = view
        .nodes
        .iter()
        .find(|node| node.site == site(&[2]))
        .unwrap();
    assert_eq!(control.kind, Kind::Control);
    assert_eq!(control.header.as_ref().unwrap().actions.len(), 1);
    assert_eq!(
        control.arms.len(),
        2,
        "empty alternatives retain their roles"
    );
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
        Some(view::CalleeView::Effect(
            lash::workflow::document::EffectName::new(name).unwrap(),
        ))
    };
    assert_eq!(ordered, [effect("value")]);
    assert!(
        view.nodes
            .iter()
            .any(|node| node.departures == [site(&[5])] && node.kind == Kind::Action)
    );
}

/// COARSE-CALL: recursion and dynamic calls are opaque caller actions; lexical
/// closure bodies occur once under their own roots, without inlining callees.
#[test]
fn coarse_call() {
    let text = format!(
        "{HEADER} fn recursive(flag) {{ if flag {{ do call recursive(false) }} return 1 }}
      fn flow() {{ let callback = fn() {{ do yield return 1 }}
        let x = call recursive(true) let y = apply callback() return y }} main {{ finish null }}"
    );
    let view = project(&text);
    assert_eq!(view.dataflow.actions.len(), 3);
    assert_eq!(view.callables.len(), 2);
    assert!(
        view.nodes
            .iter()
            .all(|node| node.site.unit == site(&[]).unit)
    );
    let called = view
        .dataflow
        .actions
        .iter()
        .find(|action| action.site == site(&[1, 0]))
        .unwrap();
    assert_eq!(
        called.callee,
        Some(view::CalleeView::Declared(Name::new("recursive")))
    );
    let callee = view::derive_callable(
        &checked(&text, "flow"),
        &Unit::Function(Name::new("recursive")),
        &super::test_packages(),
    )
    .unwrap();
    assert_eq!(
        callee
            .nodes
            .iter()
            .filter(|node| node.kind == Kind::Action)
            .count(),
        1
    );
}

/// COARSE-DEPARTURE: grouped pure departures retain exact destinations and
/// finally-unwind edges; a code group never falls through an unconditional exit.
#[test]
fn coarse_departure() {
    let view = project(&format!(
        "{HEADER} fn flow() {{
      for x in [1] {{ try {{ if true {{ continue }} else {{ break }} }} finally {{ do yield }} }}
      try {{ throw {{kind: \"x\"}} }} catch error {{ return 1 }} finally {{ do yield }}
      finish 2 fail \"failed\" return 3 do yield
    }} main {{ finish null }}"
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
            .find(|node| node.site == site(&[1]))
            .unwrap()
            .falls_through,
        "a try whose body and catch depart cannot fall through after finally"
    );
    let returned = view
        .flow
        .iter()
        .find(|edge| edge.from == site(&[1, 1, 0]) && edge.kind == FlowKind::Return)
        .unwrap();
    assert!(
        matches!(&returned.to, FlowTarget::Site(destination) if *destination == site(&[1, 2, 0]))
    );
    assert!(view.flow.iter().any(|edge| edge.from == site(&[1, 2])
        && edge.kind == FlowKind::Return
        && edge.to == FlowTarget::Leave));
    let code = view
        .nodes
        .iter()
        .find(|node| node.members.contains(&site(&[2])))
        .unwrap();
    assert_eq!(code.kind, Kind::Code);
    assert!(!code.falls_through);
    assert!(code.departures.contains(&site(&[2])));
    assert!(
        !view
            .flow
            .iter()
            .any(|edge| edge.from == site(&[2]) && edge.kind == FlowKind::Next)
    );
}

/// COARSE-SPANS: canonical ranges exist without a readable lens; shadowed
/// binding ids/scopes remain distinct and ranges cannot be used after reimport.
#[test]
fn coarse_spans() {
    let document = checked(
        &format!(
            "{HEADER} fn flow() {{ let x = 1 if true {{ let x = 2 }} return x }} main {{ finish null }}"
        ),
        "flow",
    );
    let mut view = view::derive(&document, &super::test_packages()).unwrap();
    let original = view.nodes.clone();
    view.readable = ReadableLens::Refused {
        name: "typescript".into(),
        cause: ReadableRefusal::Invalid {
            message: "no readable spelling".into(),
        },
    };
    assert_eq!(view.nodes, original);
    assert!(view.nodes.iter().all(|node| node.canonical.first > 0
        && node.canonical.last <= view.canonical_source.lines().count()));
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
    let embedding = lash_vm_worker::built_embedding(&Default::default()).expect("embedding");
    let printed = lash::typescript::print(
        document.document(),
        None,
        embedding.library(),
        lash::dialect::PrintOptions {
            source_map: true,
            ..Default::default()
        },
    )
    .unwrap();
    let map = printed.source_map.unwrap();
    view.readable_ranges(
        document.document(),
        "generated_typescript".into(),
        &printed.source,
        &map,
    )
    .unwrap();
    assert!(view.nodes.iter().all(|node| !node.readable.is_empty()));
    let changed = checked(
        &format!("{HEADER} fn flow() {{ let x = 99 return x }} main {{ finish null }}"),
        "flow",
    );
    assert_ne!(view.document, changed.identity());
    assert_eq!(
        view.readable_ranges(
            changed.document(),
            "typescript".into(),
            &printed.source,
            &map
        ),
        Err(lash::workflow::document::SourceMapRefusal::Document)
    );
}

async fn launch(
    core: &lash::LashCore,
    text: &str,
    key: &str,
) -> (lash::ProcessId, lash::workflow::WorkflowDocument) {
    let document = parse_document(text).unwrap();
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let env = linking(&environment(false), &[]);
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(document.clone()),
                &Name::new("flow"),
                &env,
                artifacts
                    .cell_packages()
                    .await
                    .expect("the package gate answers")
                    .declare_all(),
                None,
            )
            .await
            .unwrap(),
    );
    let env_ref = artifacts.publish_process_env(&pin, &env).await.unwrap();
    let settings = core
        .processes()
        .kernel_engine_defaults()
        .unwrap()
        .observing(lash::process::ObservationMode::Counts)
        .into_engine_settings();
    let document = super::publication_document(core, &publication).await;
    let process = core
        .processes()
        .start(
            lash::process::ProcessStartRequest::definition(
                publication.definition.id,
                Some(publication.definition.signature),
                serde_json::Map::new(),
                lash::process::EngineLaunch::new(env_ref, Some(settings)),
                lash::process::ProcessOriginator::host(),
                lash::process::Lifetime::Detached,
            )
            .with_host_start_key(key),
            core.effect_host(),
        )
        .await
        .unwrap()
        .process_id;
    (process, document)
}
async fn counts(core: &lash::LashCore, process: &lash::ProcessId) -> view::RanCounts {
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
    let (process, document) = launch(
        &core,
        &format!(
            "{HEADER}
      fn answer() {{ return 7 }} fn worker() {{ return 1 }}
      fn flow() {{
        let answer = if(true, call answer(), call answer())
        let initial = 0 let keep = true while keep {{ set keep = false do yield }}
        if false {{ do yield }} else {{}}
        let task = spawn call worker() let joined = join task do cancel task
        let tasks = [task] let every = join all tasks
        return answer
      }} main {{ finish null }}"
        ),
        "coarse-counts",
    )
    .await;
    assert_eq!(finished(&core, &process).await, serde_json::json!(7));
    let view = view::derive(&document, &super::test_packages()).unwrap();
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
            node.kind == Kind::Control
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
    for observation in [None, Some(view::RunObservation::Off)] {
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
        .find(|node| node.kind == Kind::Code && node.members.len() > 1)
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
    let (process, document) = launch(
        &first,
        &format!(
            "{HEADER}
      fn child() {{ return 9 }} fn pair(a, b) {{ return b }}
      fn flow() {{
        let worker = fn() {{ let out = call pair(sleep 5, call child()) return out }}
        let first = spawn apply worker() let second = spawn apply worker()
        let tasks = [first, second] let results = join all tasks return results
      }} main {{ finish null }}"
        ),
        "coarse-resume",
    )
    .await;
    let due = timer_park_after(&first, &process, 0).await;
    let view = view::derive(&document, &super::test_packages()).unwrap();
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
