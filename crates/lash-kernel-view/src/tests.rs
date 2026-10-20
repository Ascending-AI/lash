//! Laws of the canvas read (`view`).

use lash_kernel_check::{Environment, WorkflowDocument};
use lash_kernel_doc::{
    Document, EffectName, Expr, FunctionCatalog, FunctionRegistry, Literal, Name, Place, Site,
    Stmt, Type, Unit, WorkflowDocumentEntry, parse_document,
};
use lash_kernel_edit::authoring::{DataValue, Operation, ValueTarget};
use lash_kernel_edit::{Draft, Position, Transaction};
use serde_json::Value;

use crate::{BindingKind, CalleeView, ScopeKind, SiteKind, WorkflowView, derive};

fn checked(document: &Document, entry: &str, functions: &dyn FunctionCatalog) -> WorkflowDocument {
    WorkflowDocument::derive(
        document.clone(),
        WorkflowDocumentEntry::Entry {
            function: Name::new(entry),
        },
        functions,
    )
    .expect("checked document")
}

fn view_of(document: &Document, entry: &str) -> WorkflowView {
    derive(
        &checked(document, entry, &FunctionRegistry::default()),
        &crate::test_packages(),
    )
    .expect("view")
}

fn site(entry: &str, path: &[u32]) -> Site {
    Site::new(Unit::Function(Name::new(entry)), path)
}

/// FIG-6894: operations, real edit refusals and derived views share Site's
/// spelling, so a host can join their root addresses without normalising JSON.
#[test]
fn a_root_site_omits_its_path_in_operations_refusals_and_views() {
    let document = parse_document(
        "kernel 1\nnumbers float\nentry demo() -> Any\nfn demo() { return null }\nmain { finish null }",
    )
    .expect("workflow");
    let root = site("demo", &[]);
    let spelled = serde_json::json!({"unit": {"function": "demo"}});
    let mut draft = Draft::open(document.clone()).expect("draft");
    let operation = Operation::RemoveStep {
        base: draft.identity(),
        action: root.clone(),
    };
    let encoded = serde_json::to_value(&operation).unwrap();
    assert_eq!(encoded["remove_step"]["action"], spelled);

    let functions = FunctionRegistry::default();
    let refusal = draft
        .apply(
            &Transaction {
                base: draft.identity(),
                edits: vec![operation],
            },
            &Environment::new(&functions),
        )
        .expect_err("a root block is not a removable perform step");
    assert_eq!(
        refusal.diagnostics[0].location,
        Some(lash_kernel_edit::Location::Base(root.clone())),
    );
    let encoded = serde_json::to_value(refusal).unwrap();
    assert_eq!(encoded["diagnostics"][0]["location"]["base"], spelled);

    let view = view_of(&document, "demo");
    let encoded = serde_json::to_value(&view).unwrap();
    for (scope, encoded) in view
        .scopes
        .iter()
        .zip(encoded["scopes"].as_array().unwrap())
    {
        if scope.block == root {
            assert_eq!(encoded["block"], spelled);
        }
    }
    assert_eq!(view.scopes[0].block, root);
    // Non-root sites keep their child positions; omission denotes only the root.
    assert_eq!(
        serde_json::to_value(site("demo", &[0])).unwrap(),
        serde_json::json!({"unit": {"function": "demo"}, "path": [0]}),
    );
}

/// A nested block's scope is one deeper than the scope holding the
/// statement that opens it, at the same depth convention the block's own
/// statements carry.
#[test]
fn a_nested_scopes_depth_is_its_parents_plus_one_matching_its_statements() {
    let document = parse_document(
        r#"kernel 1
numbers float
entry demo() -> Any
fn demo() {
  if true {
    for item in [1] {
      let seen = item
    }
  }
  return null
}
main { finish null }
"#,
    )
    .expect("nested workflow");
    let view = view_of(&document, "demo");
    let scope = |kind| {
        view.scopes
            .iter()
            .find(|scope| scope.kind == kind)
            .expect("scope")
    };
    let (entry, then, for_body) = (
        scope(ScopeKind::Entry),
        scope(ScopeKind::Then),
        scope(ScopeKind::For),
    );
    assert_eq!(entry.depth, 0);
    assert_eq!(then.depth, entry.depth + 1);
    assert_eq!(for_body.depth, then.depth + 1);
    let inner = view
        .statements
        .iter()
        .find(|statement| statement.block == for_body.block)
        .expect("the loop's statement");
    assert_eq!(inner.depth, for_body.depth);
    let opening = view
        .statements
        .iter()
        .find(|statement| Some(&statement.site) == for_body.statement.as_ref())
        .expect("the opening for statement");
    assert_eq!((&opening.block, opening.depth), (&then.block, then.depth));
}

/// The contracts the crate ships are byte-identical to the checked-in host
/// schema files, so a host that vendors lash checks the same contracts.
#[test]
fn the_shipped_view_contracts_are_the_checked_in_schema_files() {
    assert_eq!(
        crate::TYPESCRIPT_DECLARATION,
        include_str!("../../../schemas/host/kernel-workflow-view/v1.d.ts")
    );
    assert_eq!(
        crate::JSON_SCHEMA,
        include_str!("../../../schemas/host/kernel-workflow-view/v1.schema.json")
    );
}

/// FIG-6448: hosts may splice the view's definitions into their OpenAPI schemas
/// because every reference resolves within the root `$defs`.
#[test]
fn the_view_schema_is_self_contained_with_resolving_defs_only_references() {
    fn walk(value: &Value, schema: &Value, is_root: bool) {
        match value {
            Value::Object(fields) => {
                assert!(
                    is_root || !fields.contains_key("$id"),
                    "a nested $id would change the root of $defs references"
                );
                if let Some(reference) = fields.get("$ref") {
                    let reference = reference.as_str().expect("$ref must be a string");
                    let name = reference.strip_prefix("#/$defs/").unwrap_or_else(|| {
                        panic!("$ref must use #/$defs/<name>, without external or $id-relative references: {reference}")
                    });
                    assert!(
                        !name.is_empty() && !name.contains('/'),
                        "$ref must name one root definition: {reference}"
                    );
                    assert!(
                        schema.pointer(&reference[1..]).is_some(),
                        "$ref must resolve in the root $defs: {reference}"
                    );
                }
                for child in fields.values() {
                    walk(child, schema, false);
                }
            }
            Value::Array(items) => {
                for child in items {
                    walk(child, schema, false);
                }
            }
            _ => {}
        }
    }

    let schema: Value = serde_json::from_str(crate::JSON_SCHEMA).expect("view JSON Schema");
    schema["$defs"].as_object().expect("root $defs object");
    walk(&schema, &schema, true);
}

/// An effect inserted by an authoring operation is an execution site of
/// the result, at the statement just before where the correspondence moved
/// the insertion's anchor.
#[test]
fn an_inserted_effect_is_an_execution_site_at_the_correspondences_new_anchor() {
    let document = parse_document(
        r#"kernel 1
numbers float
effect mail.send(to: Text) -> Bool contract "1"
entry notify(to: Text) -> Any

fn notify(to) {
  let sent = perform mail.send(to) as Any
  return sent
}

main {
  finish null
}
"#,
    )
    .expect("base workflow");
    let mut draft = Draft::open(document.clone()).expect("draft");
    let functions = FunctionRegistry::default();
    let mut environment = Environment::new(&functions);
    environment.effects = document.manifest.effects.clone();
    let anchor = site("notify", &[1]);
    let effect = EffectName::new("mail.send").unwrap();
    let transaction = Transaction {
        base: draft.identity(),
        edits: vec![Operation::InsertStep {
            base: draft.identity(),
            at: Position::before(anchor.clone()).unwrap(),
            effect: effect.clone(),
            args: vec![DataValue::Literal(Literal::Text("ops".into()))],
            result: Type::Any,
            binding: Some(Name::new("copied")),
        }],
    };
    let base = view_of(draft.document(), "notify");
    let applied = draft.apply(&transaction, &environment).expect("admitted");
    let moved = applied
        .correspondence
        .successor(&anchor)
        .expect("the anchor survives")
        .clone();
    let mut inserted = moved.clone();
    *inserted.path.last_mut().unwrap() -= 1;

    let view = view_of(draft.document(), "notify");
    let performs = |view: &WorkflowView| {
        view.execution_sites
            .iter()
            .filter(|site| site.kind == SiteKind::Perform)
            .map(|site| site.statement.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(performs(&base), [site("notify", &[0])]);
    assert_eq!(performs(&view), [site("notify", &[0]), inserted.clone()]);
    let statement = view
        .statements
        .iter()
        .find(|statement| statement.site == inserted)
        .expect("the inserted statement is listed");
    assert_eq!(statement.actions, [inserted.child(0)]);
    assert!(matches!(&statement.statement, Stmt::Let { name, .. } if name.as_str() == "copied"));
    assert!(
        view.dataflow
            .actions
            .iter()
            .any(|action| action.statement == inserted && action.args[0].name.is_some())
    );
    assert!(matches!(
        view.statements.iter().find(|statement| statement.site == moved),
        Some(statement) if matches!(statement.statement, Stmt::Return { .. })
    ));
}

/// A closure's body is a scope of its own, and an effect inside it is
/// listed at its exact site, so a host can show and target it.
#[test]
fn an_effect_in_a_closure_body_is_listed_at_its_site() {
    let document = parse_document(
        r#"kernel 1
numbers float
effect display.show_message(input: Any) -> Any contract "1"
entry demo(seed: Any) -> Any
fn demo(seed) {
  let task = fn() {
    let input = seed
    do perform display.show_message(input) as Any
  }
  return null
}
main { finish null }
"#,
    )
    .expect("a kernel closure fixture");
    let view = view_of(&document, "demo");
    let body = site("demo", &[0, 0, 0]);
    let effect = site("demo", &[0, 0, 0, 1]);
    assert!(view.scopes.iter().any(|scope| scope.block == body
        && scope.kind == ScopeKind::Closure
        && scope.statement == Some(site("demo", &[0]))));
    let reads = &view.dataflow.bindings[0].uses;
    assert_eq!(reads.len(), 2, "capture and body read are both named");
    assert_eq!(reads[0].site, site("demo", &[0, 0]));
    assert_eq!(reads[0].statement, site("demo", &[0]));
    assert_eq!(reads[1].site, site("demo", &[0, 0, 0, 0, 0]));
    assert_eq!(reads[1].statement, site("demo", &[0, 0, 0, 0]));
    let listed = view
        .statements
        .iter()
        .find(|statement| statement.site == effect)
        .expect("the closure's effect statement is listed");
    assert_eq!((&listed.block, listed.depth), (&body, 1));
    assert_eq!(listed.actions, [effect.child(0)]);
    assert!(
        view.execution_sites
            .iter()
            .any(|site| site.statement == effect)
    );
}

/// K-ADM-005: aliases passed to another function can change record fields,
/// so neither the field read nor the entry's result keeps a stale type.
#[test]
fn helper_mutations_do_not_leave_stale_field_types() {
    for mutation in [
        "let change = call mutator(record) do apply change()",
        "do call mutate(record)",
    ] {
        let document = parse_document(&format!(
            r#"kernel 1
numbers float
entry demo() -> Any
fn mutate(record) {{ set record.value = 42 }}
fn mutator(record) {{ return fn() {{ set record.value = 42 }} }}
fn demo() {{
  let record = {{value: "initial"}}
  {mutation}
  return record.value
}}
main {{ finish null }}
"#
        ))
        .expect("canonical helper mutation fixture");
        let view = view_of(&document, "demo");
        let output = view.dataflow.returns.first().expect("entry return");
        assert_eq!(
            output.ty,
            Type::Any,
            "{mutation}: a helper can replace the text"
        );
        let expression = view
            .dataflow
            .expressions
            .iter()
            .find(|expression| expression.site == output.expression_site)
            .expect("returned field expression");
        assert_eq!(expression.ty, Type::Any);
    }
}

/// A name resolves as lexical lookup would: an initializer cannot read its
/// own binding, and a later or shadowed binding is not available.
#[test]
fn available_bindings_preserve_shadowing_and_exclude_own_initializers() {
    let document = parse_document(
        r#"kernel 1
numbers float
entry demo(source: Text) -> Any
fn demo(source) {
  let shared = source
  let capture = fn(source) {
    let selected = source
    return selected
  }
  let source = shared
  return source
}
main { finish null }
"#,
    )
    .expect("valid canonical fixture");
    let view = view_of(&document, "demo");
    let bindings = &view.dataflow.bindings;
    let params: Vec<_> = bindings
        .iter()
        .filter(|binding| binding.name.as_str() == "source" && binding.kind == BindingKind::Param)
        .collect();
    assert_eq!(params.len(), 2);
    let selected = bindings
        .iter()
        .find(|binding| binding.name.as_str() == "selected")
        .expect("selected");
    let initializer = view
        .dataflow
        .expressions
        .iter()
        .find(|expression| Some(&expression.site) == selected.value_site.as_ref())
        .expect("initializer");
    assert_eq!(initializer.references, vec![params[1].id]);
    assert!(initializer.available.contains(&params[1].id));
    assert!(!initializer.available.contains(&params[0].id));
    assert!(!initializer.available.contains(&selected.id));
    let replacement = bindings
        .iter()
        .find(|binding| binding.name.as_str() == "source" && binding.kind == BindingKind::Let)
        .expect("replacement");
    let tail = view
        .dataflow
        .expressions
        .iter()
        .find(|expression| expression.site.path == [3, 0])
        .expect("return source");
    assert_eq!(tail.references, vec![replacement.id]);
    assert!(tail.available.contains(&replacement.id));
    assert!(!tail.available.contains(&params[0].id));
}

/// A `return` belongs to the closure that holds it and a `finish` to the
/// entry; every assignment keeps the site of the value it writes.
#[test]
fn returns_and_assignments_keep_their_callable_and_write() {
    let document = parse_document(
        r#"kernel 1
numbers float
effect demo.read() -> Text contract "1"
entry demo() -> Text
fn demo() {
  let state = "initial"
  set state = perform demo.read() as Any
  set state = "changed"
  let record = {value: "initial"}
  set record.value = state
  set record["value"] = state
  let nested = fn() { return state }
  if true { return state }
  return state
}
main { finish null }
"#,
    )
    .expect("canonical state and return fixture");
    let view = view_of(&document, "demo");
    let dataflow = &view.dataflow;
    assert_eq!(
        dataflow
            .assignments
            .iter()
            .map(|write| write.value_site.path.clone())
            .collect::<Vec<_>>(),
        vec![vec![1, 0], vec![2, 0], vec![4, 1], vec![5, 2]]
    );
    assert_eq!(
        dataflow.actions[0].destination,
        Some(Place::Variable(Name::new("state")))
    );
    let nested = dataflow
        .returns
        .iter()
        .find(|output| output.callable.path == [6, 0])
        .expect("closure return belongs to closure");
    assert_eq!(nested.expected_type, Type::Any);
    let entry_returns: Vec<_> = dataflow
        .returns
        .iter()
        .filter(|output| output.callable.path.is_empty())
        .collect();
    assert_eq!(entry_returns.len(), 2);
    assert!(
        entry_returns
            .iter()
            .all(|output| output.expected_type == Type::Text)
    );
}

/// Builtin action operands use their kernel names, including both task waits.
#[test]
fn builtin_operands_keep_the_kernel_names() {
    let document = parse_document(
        r#"kernel 1
numbers float
entry demo(task: Task(Any), tasks: List(Task(Any))) -> Any
fn demo(task, tasks) {
  do sleep 10
  do join task
  do join all tasks
  do cancel task
}
main { finish null }
"#,
    )
    .expect("builtin operands");
    let view = view_of(&document, "demo");
    let actions = &view.dataflow.actions;
    assert_eq!(actions.len(), 4);
    for (action, (kind, name)) in actions.iter().zip([
        (SiteKind::Sleep, "duration"),
        (SiteKind::Join, "task"),
        (SiteKind::JoinMany, "tasks"),
        (SiteKind::Cancel, "task"),
    ]) {
        assert_eq!(action.kind, kind);
        assert_eq!(action.args.len(), 1);
        assert_eq!(action.args[0].name, Some(Name::new(name)));
    }
}

/// FIG-6371: the digest list path with three topics and one urgent item.
/// Counts here are the hand-written path, including an untaken arm and
/// empty else blocks; they are not generated by a second counting algorithm.
#[test]
fn coverage_reads_the_digest_list_path_and_empty_arms() {
    use crate::{RanCounts, RunObservation, coverage};
    let document = parse_document(
        r#"kernel 1
numbers float
entry digest(topics: List(Any), list: Bool, missing: Bool, urgent: Bool) -> Any
fn digest(topics, list, missing, urgent) {
  for topic in topics {
    let selected = topic
    if list {
      if missing { set selected = null }
    } else { set selected = null }
    if urgent { do sleep 1000 }
    set selected = topic
  }
  return null
}
main { finish null }
"#,
    )
    .unwrap();
    let view = view_of(&document, "digest");
    // Main occupies 0..2. Function: body=2, for=3, loop body=4,
    // let=5, if list=6, then=7, if missing=8, then=9, set=10,
    // empty else=11, outer else=12, set=13, if urgent=14,
    // then=15, sleep=16, empty else=17, set=18, return=19.
    let result = coverage(
        &document,
        &Unit::Function(Name::new("digest")),
        &RanCounts {
            observation: Some(RunObservation::Counts),
            ran: vec![
                (3, 1),
                (5, 3),
                (6, 3),
                (8, 3),
                (11, 3),
                (14, 3),
                (16, 1),
                (17, 2),
                (18, 3),
                (19, 1),
            ],
        },
    );
    assert_eq!(
        result
            .statements
            .iter()
            .map(|row| row.runs)
            .collect::<Vec<_>>(),
        [
            Some(1),
            Some(3),
            Some(3),
            Some(3),
            Some(0),
            Some(0),
            Some(3),
            Some(1),
            Some(3),
            Some(1)
        ]
    );
    assert_eq!(
        result
            .scopes
            .iter()
            .map(|row| row.entries)
            .collect::<Vec<_>>(),
        [
            Some(1),
            Some(3),
            Some(3),
            Some(0),
            Some(3),
            Some(0),
            Some(1),
            Some(2)
        ]
    );
    for statement in &view.statements {
        assert_eq!(statement.ordinal, document.site_ordinals()[&statement.site]);
    }
    for scope in &view.scopes {
        assert_eq!(scope.ordinal, document.site_ordinals()[&scope.block]);
    }
}

/// FIG-6371: an off process or cell, and a cell without its start, are
/// unobserved. Empty observed counts mean zero; empty unobserved counts do not.
#[test]
fn coverage_keeps_off_and_missing_observation_distinct_from_zero() {
    use crate::{RanCounts, RunObservation, coverage};
    let document = parse_document("kernel 1\nnumbers float\nentry demo() -> Any\nfn demo() { return null }\nmain { finish null }\n").unwrap();
    let unit = Unit::Function(Name::new("demo"));
    for observation in [None, Some(RunObservation::Off)] {
        let result = coverage(
            &document,
            &unit,
            &RanCounts {
                observation,
                ran: vec![(3, 9)],
            },
        );
        assert_eq!(result.observation, observation);
        assert_eq!(result.statements[0].runs, None);
        assert_eq!(result.scopes[0].entries, None);
    }
    let result = coverage(
        &document,
        &unit,
        &RanCounts {
            observation: Some(RunObservation::Counts),
            ran: vec![],
        },
    );
    assert_eq!(result.statements[0].runs, Some(0));
    assert_eq!(result.scopes[0].entries, Some(0));
}

/// FIG-6371: replay mapping retains each task's order and repetitions,
/// emitting scope entries beside their first statement or empty-block start.
#[test]
fn a_replayed_path_maps_statement_and_scope_steps_in_task_order() {
    use crate::{PathSite, ReplayPath, TaskPath, path};

    let document = parse_document("kernel 1\nnumbers float\nentry demo() -> Any\nfn demo() { if true {} return null }\nmain { finish null }\n").unwrap();
    let unit = Unit::Function(Name::new("demo"));
    let spawned = lash_kernel_doc::testkit::spawned_task(
        lash_kernel_doc::testkit::root_task(),
        site("demo", &[0]),
        0,
    );
    let steps = path(
        &document,
        &unit,
        &ReplayPath {
            tasks: vec![
                TaskPath {
                    task: spawned.clone(),
                    starts: vec![3, 4, 5, 6],
                },
                TaskPath {
                    task: lash_kernel_doc::testkit::root_task(),
                    starts: vec![3, 4, 4, 6],
                },
            ],
        },
    );
    let expected = vec![
        PathSite::Scope {
            block: site("demo", &[]),
        },
        PathSite::Statement {
            site: site("demo", &[0]),
        },
        PathSite::Scope {
            block: site("demo", &[0, 1]),
        },
        PathSite::Scope {
            block: site("demo", &[0, 2]),
        },
        PathSite::Statement {
            site: site("demo", &[1]),
        },
    ];
    assert_eq!(
        steps[..5]
            .iter()
            .map(|step| step.site.clone())
            .collect::<Vec<_>>(),
        expected
    );
    assert!(steps[..5].iter().all(|step| step.task == spawned));
    assert_eq!(
        steps[5..]
            .iter()
            .map(|step| step.site.clone())
            .collect::<Vec<_>>(),
        vec![
            expected[0].clone(),
            expected[1].clone(),
            expected[2].clone(),
            expected[2].clone(),
            expected[4].clone()
        ]
    );
    assert!(
        steps[5..]
            .iter()
            .all(|step| step.task == lash_kernel_doc::testkit::root_task())
    );
}

/// K-FORM-003 and K-EVAL-002: each expression child is addressable,
/// and only the Let body can see its binding.
#[test]
fn expression_let_bindings_are_available_only_in_the_body() {
    let document = parse_document(
        r#"kernel 1
numbers float
entry demo(source: Int) -> Any
fn demo(source) { return (let(source = source, if(true, source, 0)), source) }
main { finish null }
"#,
    )
    .unwrap();
    let view = view_of(&document, "demo");
    let binding = view
        .dataflow
        .bindings
        .iter()
        .find(|binding| binding.kind == BindingKind::Let)
        .unwrap();
    assert!(binding.value_site.is_some());
    for expression in &view.dataflow.expressions {
        let inside = expression.site.path.starts_with(&[0, 0, 0, 1]);
        assert_eq!(
            expression.available.contains(&binding.id),
            inside,
            "{}",
            expression.site
        );
    }
    assert_eq!(
        view.dataflow
            .expressions
            .iter()
            .find(|expression| expression.site.path == [0, 0, 0, 1, 1])
            .unwrap()
            .references,
        vec![binding.id]
    );
}

const NESTED: &str = r#"kernel 1
numbers float
effect mail.send(to: Text, message: Any) -> Bool contract "1"
effect log.write(line: Any) -> Any contract "1"
entry notify(to: Text) -> Any

fn notify(to) {
  let sent = perform mail.send(to, {subject: "hi", text: perform log.write(to) as Any}) as Any
  let pair = [call wrap(perform log.write("a") as Any), spawn call wrap(1)]
  let task = fn() {
    let inner = perform log.write("closure") as Any
    return inner
  }
  return [sent, pair]
}

fn wrap(value) {
  return value
}

main {
  finish null
}
"#;

/// VIEW-NESTED (`K-SITE-002`, `K-EDIT-004`): a statement is one node whose
/// actions, nested ones included, are listed in the order a run issues
/// them, each with its kind and callee, its arguments as written and the
/// innermost action it is an argument of. A closure's actions are its own
/// statements'. Replacing an action's argument expression re-derives the
/// action with the new argument and survives as the same action, and the
/// inverse edit gives back the document it started from. The view is read
/// from the document and its declarations alone: there is no source to
/// read.
#[test]
fn the_view_lists_nested_actions_in_issue_order_and_their_arguments_edit_in_place() {
    let document = parse_document(NESTED).expect("a nested-action workflow");
    let view = view_of(&document, "notify");
    let at = |path: &[u32]| site("notify", path);
    let listed = |view: &WorkflowView, path: &[u32]| {
        view.statements
            .iter()
            .find(|statement| statement.site == at(path))
            .expect("the statement is listed")
            .actions
            .clone()
    };
    assert_eq!(listed(&view, &[0]), [at(&[0, 0, 1, 1]), at(&[0, 0])]);
    assert_eq!(
        listed(&view, &[1]),
        [at(&[1, 0, 0, 0]), at(&[1, 0, 0]), at(&[1, 0, 1])]
    );
    assert_eq!(listed(&view, &[2]), Vec::<Site>::new());
    assert_eq!(listed(&view, &[2, 0, 0, 0]), [at(&[2, 0, 0, 0, 0])]);
    let issued: Vec<Site> = view
        .statements
        .iter()
        .flat_map(|s| s.actions.clone())
        .collect();
    let actions: Vec<Site> = view
        .dataflow
        .actions
        .iter()
        .map(|a| a.site.clone())
        .collect();
    assert_eq!(
        issued.iter().collect::<std::collections::BTreeSet<_>>(),
        actions.iter().collect::<std::collections::BTreeSet<_>>(),
        "every listed action has its dataflow entry"
    );

    let action = |view: &WorkflowView, path: &[u32]| {
        view.dataflow
            .actions
            .iter()
            .find(|action| action.site == at(path))
            .expect("the action is in the dataflow")
            .clone()
    };
    let send = action(&view, &[0, 0]);
    assert_eq!(
        (send.kind, send.callee.clone(), send.statement.clone()),
        (
            SiteKind::Perform,
            Some(CalleeView::Effect(EffectName::new("mail.send").unwrap())),
            at(&[0])
        )
    );
    assert_eq!(send.result_type, Type::Bool);
    assert_eq!(
        send.args
            .iter()
            .map(|arg| (arg.name.clone(), arg.site.clone(), arg.binding))
            .collect::<Vec<_>>(),
        [
            (Some(Name::new("to")), Some(at(&[0, 0, 0])), Some(0)),
            (Some(Name::new("message")), Some(at(&[0, 0, 1])), None),
        ]
    );
    assert!(matches!(send.args[1].expression, Expr::Record(_)));
    let wrap = action(&view, &[1, 0, 0]);
    assert_eq!(
        (wrap.kind, wrap.callee, wrap.args[0].name.clone()),
        (
            SiteKind::Call,
            Some(CalleeView::Declared(Name::new("wrap"))),
            Some(Name::new("value"))
        )
    );
    assert_eq!(action(&view, &[1, 0, 1]).kind, SiteKind::Spawn);
    let within = |path: &[u32]| {
        view.execution_sites
            .iter()
            .find(|site| site.site == at(path))
            .expect("an execution site")
            .within
            .clone()
    };
    assert_eq!(within(&[0, 0]), None);
    assert_eq!(within(&[0, 0, 1, 1]), Some(at(&[0, 0])));
    assert_eq!(within(&[1, 0, 0, 0]), Some(at(&[1, 0, 0])));
    assert_eq!(within(&[1, 0, 1]), None);

    let mut draft = Draft::open(document.clone()).expect("draft");
    let functions = FunctionRegistry::default();
    let mut environment = Environment::new(&functions);
    environment.effects = document.manifest.effects.clone();
    let subject = at(&[0, 0, 1, 0]);
    let replace = |draft: &Draft, with: &str| Transaction {
        base: draft.identity(),
        edits: vec![Operation::SetValue {
            base: draft.identity(),
            target: ValueTarget::Literal {
                expression: subject.clone(),
            },
            value: DataValue::Literal(Literal::Text(with.into())),
        }],
    };
    let applied = draft
        .apply(&replace(&draft, "hello"), &environment)
        .expect("the argument is replaced");
    let survivor = |path: &[u32]| {
        applied
            .correspondence
            .survivor(&at(path))
            .map(|survivor| (survivor.to.clone(), survivor.edited))
    };
    assert_eq!(survivor(&[0, 0]), Some((at(&[0, 0]), false)));
    assert_eq!(survivor(&[0, 0, 1, 0]), Some((at(&[0, 0, 1, 0]), true)));
    let edited = view_of(draft.document(), "notify");
    assert_eq!(listed(&edited, &[0]), listed(&view, &[0]));
    let Expr::Record(entries) = &action(&edited, &[0, 0]).args[1].expression else {
        panic!("the argument is still a record");
    };
    assert_eq!(
        entries[0].value,
        Expr::Literal(Literal::Text("hello".into()))
    );
    draft
        .apply(&replace(&draft, "hi"), &environment)
        .expect("the inverse edit applies");
    assert_eq!(draft.document(), &document);
    assert_eq!(draft.identity(), document.identity());
    assert_eq!(view_of(draft.document(), "notify"), view);
}

/// FIG-6471: text and graph round trips preserve a discarded computed
/// action and its argument sites in the host view.
#[test]
fn do_computed_actions_round_trip_through_the_view() {
    let document = parse_document(
        r#"kernel 1
numbers float
effect show(value: Any) -> Any contract "1"
entry demo() -> Any
fn demo() { do perform show({text: "x"}) as Any return null }
main { finish null }
"#,
    )
    .unwrap();
    let view = view_of(&document, "demo");
    let action = &view.dataflow.actions[0];
    assert_eq!(action.site, site("demo", &[0, 0]));
    assert_eq!(
        view.statements[0].actions.as_slice(),
        std::slice::from_ref(&action.site)
    );
    assert_eq!(action.kind, SiteKind::Perform);
    assert_eq!(action.args[0].site, Some(site("demo", &[0, 0, 0])));
    assert!(matches!(action.args[0].expression, Expr::Record(_)));
    let printed = lash_kernel_doc::print_document(&document);
    let reparsed = parse_document(&printed).unwrap();
    let bytes = lash_kernel_doc::graph_tokens::encode_body(&document);
    let decoded = lash_kernel_doc::graph_tokens::decode_body::<Document>(&bytes).unwrap();
    assert_eq!(view_of(&reparsed, "demo"), view);
    assert_eq!(view_of(&decoded, "demo"), view);
}

/// K-CAP-003's view join: an action or control node shows its carrier
/// statement's caption, a statement with several actions only on the node
/// of its first outermost action, a code node every caption among its members and their
/// arms, a control its arms', and a function's on its callable. K-CAP-007's:
/// the entry's parameters show by their position in its signature, and its
/// types' fields by path. Captions of another document, and a workflow
/// document the view is not of, are refused.
#[test]
fn captions_join_the_view_by_node_arm_callable_parameter_and_field() {
    use lash_kernel_doc::{
        Caption, CaptionEntry, CaptionTarget, Captions, FieldPath, FieldRoot, FieldTarget,
        ParamTarget,
    };
    let document = parse_document(
        r#"kernel 1
numbers float
effect one() -> Any contract "1"
effect two() -> Any contract "1"
effect test() -> Bool contract "1"
effect send(x: Any) -> Any contract "1"
entry flow(event: Record{answer: Record{choice: Text}}, n: Int) -> Record{seen: Int}
fn flow(event, n) {
  let a = 1
  let b = 2
  if true { let c = 3 } else { let d = 4 }
  let x = perform send([perform one() as Any, perform two() as Any]) as Any
  if perform test() as Bool { do perform two() as Any } else {}
  return x
}
main { finish null }
"#,
    )
    .expect("workflow");
    let holder = checked(&document, "flow", &FunctionRegistry::default());
    let view = derive(&holder, &crate::test_packages()).expect("view");
    let caption = |title: &str| Caption::new(title, None).expect("caption");
    let entry = |path: &[u32], title: &str| CaptionEntry::site(site("flow", path), caption(title));
    let field = |root: FieldRoot, fields: &[&str], title: &str| CaptionEntry {
        target: CaptionTarget::Field(FieldTarget {
            function: Name::new("flow"),
            path: FieldPath {
                root,
                fields: fields.iter().map(|field| (*field).to_owned()).collect(),
            },
        }),
        caption: caption(title),
    };
    let captions = Captions::new(
        &document,
        vec![
            entry(&[1], "Second"),
            entry(&[2, 2], "Code else"),
            entry(&[3], "Send"),
            entry(&[4], "Decide"),
            entry(&[4, 2], "Otherwise"),
            entry(&[], "Workflow"),
            CaptionEntry {
                target: CaptionTarget::Param(ParamTarget {
                    function: Name::new("flow"),
                    param: Name::new("n"),
                }),
                caption: caption("How many"),
            },
            field(FieldRoot::Result, &["seen"], "Seen"),
            field(
                FieldRoot::Param(Name::new("event")),
                &["answer", "choice"],
                "The choice",
            ),
        ],
    )
    .expect("captions");
    let joined = view.captions(&holder, &captions).expect("one document");
    let node = |kind: crate::WorkflowNodeKind, statement: &[u32]| {
        view.nodes
            .iter()
            .position(|node| node.kind == kind && node.members.contains(&site("flow", statement)))
            .expect("node")
    };
    let code = node(crate::WorkflowNodeKind::Code, &[0]);
    let first_action = joined
        .nodes
        .iter()
        .find(|caption| caption.site == site("flow", &[3]))
        .expect("statement caption")
        .node;
    assert!(
        view.nodes[first_action].within.is_none(),
        "K-CAP-003: the statement caption belongs to its outermost action"
    );
    assert_eq!(
        view.dataflow
            .actions
            .iter()
            .find(|action| action.site == view.nodes[first_action].site)
            .expect("caption's action")
            .callee,
        Some(CalleeView::Effect(
            EffectName::new("send").expect("effect name")
        )),
        "the caption joins to the perform that consumes the argument actions"
    );
    let control = node(crate::WorkflowNodeKind::Control, &[4]);
    assert_eq!(
        view.nodes
            .iter()
            .filter(|node| node.statement == site("flow", &[3]))
            .count(),
        3,
        "one node per action of the statement"
    );
    let shown: Vec<(usize, Site, &str)> = joined
        .nodes
        .iter()
        .map(|shown| (shown.node, shown.site.clone(), shown.caption.title()))
        .collect();
    assert_eq!(
        shown,
        [
            (code, site("flow", &[1]), "Second"),
            (code, site("flow", &[2, 2]), "Code else"),
            (first_action, site("flow", &[3]), "Send"),
            (control, site("flow", &[4]), "Decide"),
        ]
    );
    let arms: Vec<_> = joined
        .arms
        .iter()
        .map(|arm| (arm.node, arm.block.clone(), arm.caption.title()))
        .collect();
    assert_eq!(arms, [(control, site("flow", &[4, 2]), "Otherwise")]);
    assert_eq!(
        view.nodes[control].arms[joined.arms[0].arm].block,
        site("flow", &[4, 2])
    );
    let titled: Vec<_> = joined
        .callables
        .iter()
        .map(|callable| {
            (
                view.callables[callable.callable].block.clone(),
                callable.caption.title(),
            )
        })
        .collect();
    assert_eq!(titled, [(site("flow", &[]), "Workflow")]);
    let params: Vec<_> = joined
        .params
        .iter()
        .map(|param| (param.index, param.name.as_str(), param.caption.title()))
        .collect();
    assert_eq!(params, [(1, "n", "How many")]);
    let fields: Vec<_> = joined
        .fields
        .iter()
        .map(|field| (field.path.to_string(), field.caption.title()))
        .collect();
    assert_eq!(
        fields,
        [
            ("event.answer.choice".to_owned(), "The choice"),
            ("return.seen".to_owned(), "Seen"),
        ]
    );
    let other = parse_document("kernel 1\nnumbers float\nmain { finish null }\n").expect("other");
    assert!(matches!(
        view.captions(&holder, &Captions::empty(other.identity())),
        Err(crate::CaptionViewRefusal::Document { .. })
    ));
    let another = parse_document(
        "kernel 1\nnumbers float\nentry flow() -> Any\nfn flow() { return null }\nmain { finish null }\n",
    )
    .expect("another");
    let other_holder = checked(&another, "flow", &FunctionRegistry::default());
    assert!(matches!(
        view.captions(&other_holder, &captions),
        Err(crate::CaptionViewRefusal::Workflow { .. })
    ));
}

/// Alias writes, converging aliases and recursive alias cycles keep every
/// reachable document callable in the may-call set.
#[test]
fn agreeing_writes_of_one_alias_resolve_to_its_callee() {
    let document = parse_document(
        r#"kernel 1
numbers float
entry flow() -> Any
fn flow() {
  let work = fn() { return null }
  let alias = work
  set alias = work
  let twice = apply alias()
  let left = work
  let right = work
  let joined = left
  set joined = right
  let diamond = apply joined()
  let ring = work
  let back = ring
  set ring = back
  let cycled = apply ring()
  let other = fn() { return null }
  let mixed = work
  set mixed = other
  let conflicted = apply mixed()
  return null
}
main { finish null }
"#,
    )
    .expect("alias workflow");
    let view = view_of(&document, "flow");
    let applies = |statement: u32| {
        &view
            .dataflow
            .actions
            .iter()
            .find(|action| action.statement == site("flow", &[statement]))
            .expect("the apply")
            .applies
    };
    let work = site("flow", &[0, 0]);
    assert_eq!(applies(3), &vec![work.clone()], "one alias written twice");
    assert_eq!(applies(8), &vec![work.clone()], "a diamond of aliases");
    assert_eq!(applies(12), &vec![work.clone()], "a cyclic alias");
    assert_eq!(
        applies(16),
        &vec![work, site("flow", &[13, 0])],
        "disagreeing writes contribute both possible callees",
    );
}

/// FIG-6925, origin projection: a library effect is drawn at its authored
/// statement without rewriting the effect's library-site identity. A nested
/// sibling keeps its own occurrence and cannot inherit the library's outcome.
#[test]
fn a_library_effects_origin_places_its_occurrence_on_the_authored_statement() {
    let document = parse_document(
        "kernel 1\nnumbers float\neffect mail.send(value: Any) -> Any contract \"1\"\nentry demo() -> Any\nfn demo() { let sent = perform mail.send(perform mail.send(null) as Any) as Any return sent }\nmain { finish null }",
    )
    .expect("workflow");
    let view = view_of(&document, "demo");
    let carrier = view
        .nodes
        .iter()
        .find(|node| node.members.contains(&site("demo", &[0])))
        .unwrap();
    let sibling = view
        .dataflow
        .actions
        .iter()
        .find(|action| action.site != carrier.site)
        .unwrap();
    let library_site = Site::new(
        Unit::Library(lash_kernel_doc::FunctionId::from_bytes([0xab; 32])),
        vec![0],
    );
    let identity = lash_kernel_doc::testkit::effect(
        lash_kernel_doc::testkit::root_task(),
        library_site,
        2,
        Vec::new(),
    );
    let observation: crate::ActionObservation<String> = serde_json::from_value(serde_json::json!({
        "identity": identity,
        "origin": site("demo", &[0]),
        "state": "completed",
        "outcome": "rows",
    }))
    .expect("an effect observation carries its authored origin");
    let activity = view
        .activity(view.document, &[observation])
        .expect("library origin is in this entry");
    let drawn = activity
        .iter()
        .find(|action| action.site == carrier.site)
        .unwrap();
    assert_eq!(drawn.state, Some(crate::ActionState::Completed));
    assert_eq!(drawn.occurrences[0].identity, identity);
    assert_eq!(drawn.occurrences[0].outcome.as_deref(), Some("rows"));
    let unrelated = activity
        .iter()
        .find(|action| action.site == sibling.site)
        .unwrap();
    assert_eq!(unrelated.state, None);
    assert!(unrelated.occurrences.is_empty());
}

/// VIEW-PERFORMS: direct performs and declared callees use the checker's
/// transitive effect derivation, deduplicated in canonical order.
#[test]
fn an_actions_performs_include_the_effects_of_its_declared_callee() {
    let document = parse_document(
        "kernel 1\nnumbers float\neffect dataframe.run() -> Any contract \"1\"\neffect dataframe.read() -> Any contract \"1\"\nentry demo() -> Any\nfn demo() { let collected = call collect() return collected }\nfn collect() { let rows = call load_rows() do perform dataframe.run() as Any return rows }\nfn load_rows() { return perform dataframe.read() as Any }\nmain { finish null }",
    ).expect("workflow");
    let view = view_of(&document, "demo");
    assert_eq!(view.dataflow.actions.len(), 1);
    assert_eq!(
        view.dataflow.actions[0].performs,
        ["dataframe.read", "dataframe.run"].map(|name| EffectName::new(name).unwrap())
    );
}

/// VIEW-PERFORMS and VIEW-MEMBER (G1): the installed collect definition
/// performs dataframe.run through its body and backs both exported names.
#[test]
fn an_installed_collect_lists_its_performs_and_every_surface_member() {
    use lash_kernel_package::{HeldPackages, Package, PackageSources};
    let sources = PackageSources {
        kernel: "effect dataframe.run(plan: Any) -> List(Any) contract \"dataframe.run/1\"\nfunction dataframe.collect(plan: Any) -> List(Any)\nkernel 1\ncharge 1\nbody { let rows = perform dataframe.run(plan) as List(Any) return rows }\n",
        surface: "async function collect(plan: Any) -> List(Any) = dataframe.collect\nasync function collect_rows(plan: Any) -> List(Any) = dataframe.collect\noperation run(plan: Any) -> List(Any) contract \"dataframe.run/1\"\n",
        labels: "",
    };
    let package = Package::build("dataframe", "1.0", &sources, &[]).unwrap();
    let held = HeldPackages::install([package]).unwrap();
    let packages = held.view(&held.own()).unwrap();
    let id = *held
        .functions()
        .iter()
        .find(|(_, registered)| registered.definition().name.as_str() == "dataframe.collect")
        .unwrap()
        .0;
    let document = parse_document(&format!(
        "kernel 1\nnumbers float\nuse dataframe.collect = @{id}\neffect dataframe.run(plan: Any) -> List(Any) contract \"dataframe.run/1\"\nentry demo() -> Any\nfn demo() {{ let rows = invoke dataframe.collect(null) return rows }}\nmain {{ finish null }}"
    )).unwrap();
    let checked = checked(&document, "demo", held.functions().as_ref());
    let before = checked.declarations()[&id].identity();
    let view = derive(&checked, &packages).unwrap();
    assert_eq!(
        view.dataflow.actions[0].performs,
        [EffectName::new("dataframe.run").unwrap()]
    );
    let Some(CalleeView::Library { member, .. }) = &view.dataflow.actions[0].callee else {
        panic!("library call")
    };
    assert_eq!(member.len(), 2);
    assert_eq!(
        member
            .iter()
            .map(|m| (m.package.to_string(), m.member.export.as_str()))
            .collect::<Vec<_>>(),
        [
            ("dataframe".into(), "collect"),
            ("dataframe".into(), "collect_rows")
        ]
    );
    let private = derive(&checked, &crate::test_packages()).unwrap();
    let Some(CalleeView::Library { member, .. }) = &private.dataflow.actions[0].callee else {
        panic!("library call")
    };
    assert!(member.is_empty());
    assert_eq!(checked.declarations()[&id].identity(), before);
}

/// VIEW-PERFORMS: a statically known callback contributes its own body's
/// effects; an unrelated closure in the same unit contributes none.
#[test]
fn performs_follow_an_applied_closure_without_other_closures_in_its_unit() {
    let document = parse_document("kernel 1\nnumbers float\neffect dataframe.run() -> Any contract \"1\"\neffect dataframe.explain() -> Any contract \"1\"\nentry demo() -> Any\nfn demo() { let callback = fn() { return perform dataframe.run() as Any } let unrelated = fn() { return perform dataframe.explain() as Any } let rows = apply callback() return rows }\nmain { finish null }").unwrap();
    let view = view_of(&document, "demo");
    let action = view
        .dataflow
        .actions
        .iter()
        .find(|action| action.statement == site("demo", &[2]))
        .unwrap();
    assert_eq!(action.performs, [EffectName::new("dataframe.run").unwrap()]);
}
