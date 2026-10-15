//! Laws of the canvas read (`view`).

use lash_kernel_check::Environment;
use lash_kernel_doc::{
    Document, EffectName, Expr, FunctionRegistry, Literal, Name, Place, Site, Stmt, Type, Unit,
    parse_document,
};
use serde_json::Value;

use crate::authoring::{Operation, plan};
use crate::view::{BindingKind, ScopeKind, SiteKind, WorkflowView, derive};
use crate::{Draft, Position};

fn view_of(document: &Document, entry: &str) -> WorkflowView {
    let graph = lash_kernel_check::derive(document, &FunctionRegistry::default()).expect("graph");
    derive(document, &graph, &Unit::Function(Name::new(entry))).expect("view")
}

fn site(entry: &str, path: &[u32]) -> Site {
    Site::new(Unit::Function(Name::new(entry)), path)
}

/// One line per element of every list, so a review reads one node a line.
fn render(value: &Value, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    let fields = value.as_object().into_iter().flatten();
    for (key, value) in fields {
        match value {
            Value::Array(items) if items.is_empty() => out.push_str(&format!("{pad}{key}: []\n")),
            Value::Array(items) => {
                out.push_str(&format!("{pad}{key}:\n"));
                for item in items {
                    out.push_str(&format!("{pad}  {item}\n"));
                }
            }
            Value::Object(_) => {
                out.push_str(&format!("{pad}{key}:\n"));
                render(value, indent + 2, out);
            }
            scalar => out.push_str(&format!("{pad}{key}: {scalar}\n")),
        }
    }
}

const PINNED: &str = r#"kernel 1
numbers float
effect mail.send(to: Text) -> Bool
effect log.write(line: Text) -> Any
entry notify(to: Text) -> Any

fn notify(to) {
  let sent = perform mail.send(to) as Any
  if sent {
    for line in ["a"] {
      do perform log.write(line) as Any
    }
  }
  try {
    return sent
  } catch e {
    fail e
  }
}

main {
  finish null
}
"#;

const PINNED_VIEW: &str = r#"dataflow:
  actions:
    {"args":[{"atom":{"variable":"to"},"binding":0,"name":"to","type":"text"}],"available":[0],"block":{"unit":{"function":"notify"}},"result_type":"bool","site":{"path":[0,0],"unit":{"function":"notify"}},"statement":{"path":[0],"unit":{"function":"notify"}}}
    {"args":[{"atom":{"variable":"line"},"binding":2,"name":"line","type":"text"}],"available":[2,1,0],"block":{"path":[1,1,0,1],"unit":{"function":"notify"}},"result_type":"any","site":{"path":[1,1,0,1,0,0],"unit":{"function":"notify"}},"statement":{"path":[1,1,0,1,0],"unit":{"function":"notify"}}}
  assignments: []
  bindings:
    {"assigned":false,"block":{"unit":{"function":"notify"}},"captured":false,"id":0,"kind":"param","name":"to","site":{"unit":{"function":"notify"}},"type":"any","uses":[{"path":[0,0],"unit":{"function":"notify"}}]}
    {"assigned":false,"block":{"unit":{"function":"notify"}},"captured":false,"id":1,"kind":"let","name":"sent","site":{"path":[0],"unit":{"function":"notify"}},"type":"any","uses":[{"path":[1,0],"unit":{"function":"notify"}},{"path":[2,0,0,0],"unit":{"function":"notify"}}],"value_site":{"path":[0,0],"unit":{"function":"notify"}}}
    {"assigned":false,"block":{"path":[1,1,0,1],"unit":{"function":"notify"}},"captured":false,"id":2,"kind":"for","name":"line","site":{"path":[1,1,0],"unit":{"function":"notify"}},"type":"text","uses":[{"path":[1,1,0,1,0,0],"unit":{"function":"notify"}}]}
    {"assigned":false,"block":{"path":[2,1],"unit":{"function":"notify"}},"captured":false,"id":3,"kind":"catch","name":"e","site":{"path":[2],"unit":{"function":"notify"}},"type":"any","uses":[{"path":[2,1,0,0],"unit":{"function":"notify"}}]}
  expressions:
    {"available":[1,0],"block":{"unit":{"function":"notify"}},"expression":{"variable":"sent"},"references":[1],"site":{"path":[1,0],"unit":{"function":"notify"}},"statement":{"path":[1],"unit":{"function":"notify"}},"type":"any"}
    {"available":[1,0],"block":{"path":[1,1],"unit":{"function":"notify"}},"expression":{"list":[{"literal":{"text":"a"}}]},"references":[],"site":{"path":[1,1,0,0],"unit":{"function":"notify"}},"statement":{"path":[1,1,0],"unit":{"function":"notify"}},"type":{"list":"text"}}
    {"available":[1,0],"block":{"path":[1,1],"unit":{"function":"notify"}},"expression":{"literal":{"text":"a"}},"references":[],"site":{"path":[1,1,0,0,0],"unit":{"function":"notify"}},"statement":{"path":[1,1,0],"unit":{"function":"notify"}},"type":"text"}
    {"available":[1,0],"block":{"path":[2,0],"unit":{"function":"notify"}},"expression":{"variable":"sent"},"references":[1],"site":{"path":[2,0,0,0],"unit":{"function":"notify"}},"statement":{"path":[2,0,0],"unit":{"function":"notify"}},"type":"any"}
    {"available":[3,1,0],"block":{"path":[2,1],"unit":{"function":"notify"}},"expression":{"variable":"e"},"references":[3],"site":{"path":[2,1,0,0],"unit":{"function":"notify"}},"statement":{"path":[2,1,0],"unit":{"function":"notify"}},"type":"any"}
  parameters:
    0
  returns:
    {"callable":{"unit":{"function":"notify"}},"expected_type":"any","expression_site":{"path":[2,0,0,0],"unit":{"function":"notify"}},"statement":{"path":[2,0,0],"unit":{"function":"notify"}},"type":"any"}
execution_sites:
  {"kind":"perform","loops":[],"site":{"path":[0,0],"unit":{"function":"notify"}},"statement":{"path":[0],"unit":{"function":"notify"}}}
  {"kind":"perform","loops":[{"path":[1,1,0],"unit":{"function":"notify"}}],"site":{"path":[1,1,0,1,0,0],"unit":{"function":"notify"}},"statement":{"path":[1,1,0,1,0],"unit":{"function":"notify"}}}
scopes:
  {"block":{"unit":{"function":"notify"}},"kind":"entry"}
  {"block":{"path":[1,1],"unit":{"function":"notify"}},"kind":"then"}
  {"block":{"path":[1,1,0,1],"unit":{"function":"notify"}},"kind":"for"}
  {"block":{"path":[1,2],"unit":{"function":"notify"}},"kind":"else"}
  {"block":{"path":[2,0],"unit":{"function":"notify"}},"kind":"try"}
  {"block":{"path":[2,1],"unit":{"function":"notify"}},"kind":"catch"}
statements:
  {"action":{"path":[0,0],"unit":{"function":"notify"}},"block":{"unit":{"function":"notify"}},"depth":0,"site":{"path":[0],"unit":{"function":"notify"}},"statement":{"let":{"name":"sent","value":{"action":{"perform":{"args":[{"variable":"to"}],"effect":"mail.send","result":"any"}}}}}}
  {"block":{"unit":{"function":"notify"}},"depth":0,"site":{"path":[1],"unit":{"function":"notify"}},"statement":{"if":{"condition":{"variable":"sent"},"then_block":[{"for":{"binding":"line","body":[{"do":{"action":{"perform":{"args":[{"variable":"line"}],"effect":"log.write","result":"any"}}}}],"iterable":{"list":[{"literal":{"text":"a"}}]}}}]}}}
  {"block":{"path":[1,1],"unit":{"function":"notify"}},"depth":1,"site":{"path":[1,1,0],"unit":{"function":"notify"}},"statement":{"for":{"binding":"line","body":[{"do":{"action":{"perform":{"args":[{"variable":"line"}],"effect":"log.write","result":"any"}}}}],"iterable":{"list":[{"literal":{"text":"a"}}]}}}}
  {"action":{"path":[1,1,0,1,0,0],"unit":{"function":"notify"}},"block":{"path":[1,1,0,1],"unit":{"function":"notify"}},"depth":2,"site":{"path":[1,1,0,1,0],"unit":{"function":"notify"}},"statement":{"do":{"action":{"perform":{"args":[{"variable":"line"}],"effect":"log.write","result":"any"}}}}}
  {"block":{"unit":{"function":"notify"}},"depth":0,"site":{"path":[2],"unit":{"function":"notify"}},"statement":{"try":{"body":[{"return":{"value":{"variable":"sent"}}}],"catch":{"binding":"e","body":[{"fail":{"value":{"variable":"e"}}}]}}}}
  {"block":{"path":[2,0],"unit":{"function":"notify"}},"depth":1,"site":{"path":[2,0,0],"unit":{"function":"notify"}},"statement":{"return":{"value":{"variable":"sent"}}}}
  {"block":{"path":[2,1],"unit":{"function":"notify"}},"depth":1,"site":{"path":[2,1,0],"unit":{"function":"notify"}},"statement":{"fail":{"value":{"variable":"e"}}}}
"#;

/// The serialized view of one workflow with a branch, a loop, a try and
/// two effects: its whole wire shape, reviewed.
#[test]
fn the_view_of_a_branch_a_loop_a_try_and_two_effects_is_pinned() {
    let document = parse_document(PINNED).expect("pinned workflow");
    let view = view_of(&document, "notify");
    let mut rendered = String::new();
    render(&serde_json::to_value(&view).unwrap(), 0, &mut rendered);
    assert_eq!(
        rendered, PINNED_VIEW,
        "the view drifted; it is now:\n{rendered}"
    );
    let decoded: WorkflowView =
        serde_json::from_value(serde_json::to_value(&view).unwrap()).expect("the view decodes");
    assert_eq!(decoded, view);
}

/// An effect inserted by an authoring operation is an execution site of
/// the result, at the statement just before where the correspondence moved
/// the insertion's anchor.
#[test]
fn an_inserted_effect_is_an_execution_site_at_the_correspondences_new_anchor() {
    let document = parse_document(
        r#"kernel 1
numbers float
effect mail.send(to: Text) -> Bool
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
    let mut draft = Draft::open(document.clone(), None).expect("draft");
    let functions = FunctionRegistry::default();
    let mut environment = Environment::new(&functions);
    environment.effects = document.manifest.effects.clone();
    let anchor = site("notify", &[1]);
    let effect = EffectName::new("mail.send").unwrap();
    let transaction = plan(
        &draft,
        [Operation::InsertEffect {
            at: Position::before(anchor.clone()).unwrap(),
            effect: effect.clone(),
            args: vec![Expr::Literal(Literal::Text("ops".into()))],
            result: Type::Any,
            binding: Name::new("copied"),
        }],
    );
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
    assert_eq!(statement.action, Some(inserted.child(0)));
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
effect display.show_message(input: Any) -> Any
entry demo() -> Any
fn demo() {
  let task = fn() {
    let input = {text: "inside closure"}
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
    assert!(
        view.scopes
            .iter()
            .any(|scope| scope.block == body && scope.kind == ScopeKind::Closure)
    );
    let listed = view
        .statements
        .iter()
        .find(|statement| statement.site == effect)
        .expect("the closure's effect statement is listed");
    assert_eq!((&listed.block, listed.depth), (&body, 1));
    assert_eq!(listed.action, Some(effect.child(0)));
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
effect demo.read() -> Text
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
