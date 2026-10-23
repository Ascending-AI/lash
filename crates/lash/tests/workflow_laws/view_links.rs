//! FIG-6811: a workflow view links an action to the closure it runs and
//! says which actions reach outside the document's code, for a TypeScript
//! and a Python workflow, read with no engine.
use lash::workflow::document::{self as k, Site};
use lash::workflow::view::{
    ActionRole, ActionView, CalleeView, SiteKind, WorkflowView, derive_callable,
};
use std::collections::BTreeMap;

fn echo() -> BTreeMap<k::EffectName, lash::schema::Signature> {
    BTreeMap::from([(
        k::EffectName::new("tools.echo").expect("a tool's name"),
        lash::schema::Signature {
            params: vec![lash::schema::Param {
                name: lash::schema::Name::new("x"),
                ty: lash::schema::Type::Any,
                optional: false,
            }],
            result: lash::schema::Type::Any,
        },
    )])
}

/// The TypeScript and Python views, each with its canonical source for
/// failure messages.
fn views() -> [(&'static str, WorkflowView); 2] {
    let embedding = lash_vm_worker::built_embedding(&Default::default()).expect("embedding");
    let effects = echo();
    let environment_namespaces = super::namespaces(effects);
    let environment = lash::dialect::Environment::new(embedding.library(), &environment_namespaces);
    let typescript = lash::typescript::lower(
        "export async function flow(count: number): any {
  const step = async (value: number) => { await sleep(5); return await tools.echo(value); };
  const pending = sleep(1);
  const first = await step(count);
  await pending;
  return first;
}",
        &environment,
        lash::dialect::Mode::Module,
    )
    .expect("import TypeScript workflow")
    .document
    .into_document();
    let python = lash::python::lower(
        "import asyncio\nasync def work(value):\n    await asyncio.sleep(1)\n    return await tools.echo(value)\ntask = asyncio.create_task(work(2))\nresult = await task\nprint(result)\n",
        &environment,
        lash::dialect::Mode::Cell(lash::dialect::CellEnvironment::first()),
    )
    .expect("lower Python")
    .document
    .into_document();
    let unit = super::export_unit(&typescript, "flow");
    let (typescript, ts_context) = super::holder(typescript);
    let (python, py_context) = super::holder(python);
    [
        (
            "TypeScript",
            derive_callable(&typescript, &unit, &ts_context).expect("view"),
        ),
        (
            "Python",
            derive_callable(&python, &k::Unit::Main, &py_context).expect("view"),
        ),
    ]
}

fn library(action: &ActionView) -> Option<&str> {
    match &action.callee {
        Some(CalleeView::Library { name, .. }) => Some(name.as_str()),
        _ => None,
    }
}

/// The actions of the callable at `callable`, its nested closures' included.
fn within<'a>(view: &'a WorkflowView, callable: &Site) -> Vec<&'a ActionView> {
    let block = &view
        .callables
        .iter()
        .find(|candidate| candidate.site == *callable)
        .unwrap_or_else(|| panic!("{callable:?} is a callable of the view"))
        .block;
    view.dataflow
        .actions
        .iter()
        .filter(|action| {
            action.block.unit == block.unit && action.block.path.starts_with(&block.path)
        })
        .collect()
}

/// FIG-6811 (a).4: an action that runs a closure the view derives names that
/// closure's callable, so a host opens the body without matching sites by
/// position. TypeScript's `ts.promise.start(<closure>)` opens onto the
/// async body and onto the unawaited `sleep`'s body. Python's `asyncio.create_task` spawns
/// `py.task.run(<closure>)`, which opens onto the closure that calls
/// `work`, and that call opens onto `work`'s body.
#[test]
fn an_action_that_runs_a_derivable_closure_names_its_callable() {
    for (dialect, view) in views() {
        let source = &view.canonical_source;
        let wrapper = match dialect {
            "TypeScript" => "ts.promise.start",
            _ => "py.task.run",
        };
        let wrappers: Vec<_> = view
            .dataflow
            .actions
            .iter()
            .filter(|action| library(action) == Some(wrapper))
            .collect();
        assert!(!wrappers.is_empty(), "{dialect} calls {wrapper}:\n{source}");
        let mut opened = Vec::new();
        for action in &wrappers {
            let [body] = action.applies.as_slice() else {
                panic!(
                    "{dialect} {wrapper} at {:?} runs one closure: {:?}\n{source}",
                    action.site, action.applies
                );
            };
            opened.extend(within(&view, body));
        }
        // Through every link, the bodies a run reaches hold its sleep and
        // its tool call.
        let mut pending: Vec<&ActionView> = opened.clone();
        let mut reached = Vec::new();
        while let Some(action) = pending.pop() {
            reached.push(action.role);
            for callable in &action.applies {
                pending.extend(within(&view, callable));
            }
        }
        assert!(
            reached.contains(&ActionRole::Wait) && reached.contains(&ActionRole::Effect),
            "{dialect} opens onto the bodies that sleep and call the tool: {reached:?}\n{source}"
        );
        // Python calls `work` through the variable its `def` writes once. A
        // TypeScript call goes through `ts.callable` of a function token,
        // which the view resolves from the helper's kernel return paths.
        if dialect == "Python" {
            assert!(
                view.dataflow
                    .actions
                    .iter()
                    .any(|action| action.kind == SiteKind::Call
                        && action.callee.is_none()
                        && action.applies.len() == 1),
                "{dialect}: a call through a variable holding a closure names it\n{source}"
            );
        }
        let serialized = serde_json::to_value(&view).expect("view serializes");
        let decoded: WorkflowView = serde_json::from_value(serialized).expect("view decodes");
        assert_eq!(view, decoded);
    }
}

/// FIG-6811 (b).1: an action's role says whose code it reaches, so the
/// steps a person recognises (tool calls, waits and the document's own
/// functions) are picked without reading callee names: no library call is
/// among them, and every `perform` and `sleep` is.
#[test]
fn an_actions_role_picks_the_steps_a_person_recognises() {
    for (dialect, view) in views() {
        let source = &view.canonical_source;
        let mut recognised = BTreeMap::<&str, usize>::new();
        for action in &view.dataflow.actions {
            let expected = match (action.kind, &action.callee) {
                (SiteKind::Perform, _) => ActionRole::Effect,
                (SiteKind::Sleep, _) => ActionRole::Wait,
                (SiteKind::Call | SiteKind::Spawn, Some(CalleeView::Library { .. }))
                | (SiteKind::Await, _) => ActionRole::Library,
                (SiteKind::Call | SiteKind::Spawn, _) => ActionRole::SubWorkflow,
                (SiteKind::Join | SiteKind::JoinMany | SiteKind::Cancel | SiteKind::Yield, _) => {
                    ActionRole::Task
                }
            };
            assert_eq!(
                action.role, expected,
                "{dialect} {:?} at {:?}\n{source}",
                action.kind, action.site
            );
            if matches!(
                action.role,
                ActionRole::Effect | ActionRole::Wait | ActionRole::SubWorkflow
            ) {
                assert_eq!(library(action), None, "{dialect} {:?}", action.site);
                *recognised
                    .entry(match action.role {
                        ActionRole::Effect => "effect",
                        ActionRole::Wait => "wait",
                        _ => "sub_workflow",
                    })
                    .or_default() += 1;
            }
        }
        let (waits, effects) = match dialect {
            "TypeScript" => (2, 1),
            _ => (1, 1),
        };
        assert_eq!(recognised.get("wait"), Some(&waits), "{dialect}\n{source}");
        assert_eq!(
            recognised.get("effect"),
            Some(&effects),
            "{dialect}\n{source}"
        );
        assert!(
            recognised.contains_key("sub_workflow"),
            "{dialect} calls its own function\n{source}"
        );
        assert!(
            view.dataflow
                .actions
                .iter()
                .any(|action| action.role == ActionRole::Library),
            "{dialect} also calls its dialect's library\n{source}"
        );
        let wire = serde_json::to_value(&view.dataflow.actions).expect("actions serialize");
        assert!(
            wire.as_array()
                .expect("actions")
                .iter()
                .all(|action| action["role"].is_string()),
            "every action carries its role on the wire"
        );
    }
}
