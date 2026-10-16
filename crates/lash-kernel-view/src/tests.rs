//! Laws of the canvas read (`view`).

use lash_kernel_check::{Environment, WorkflowDocument};
use lash_kernel_doc::{
    Document, EffectName, Expr, FunctionCatalog, FunctionRegistry, Literal, Name, Place, Site,
    Stmt, Type, Unit, WorkflowDocumentEntry, parse_definition, parse_document,
};
use lash_kernel_edit::authoring::{Operation, plan};
use lash_kernel_edit::{Draft, Position};
use serde_json::Value;

use crate::{BindingKind, ScopeKind, SiteKind, WorkflowView, derive};

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
    derive(&checked(document, entry, &FunctionRegistry::default())).expect("view")
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
use text.pad = @LIBRARY
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
  do invoke text.pad(to, 2)
  do call wrap(to, 2)
  do spawn call wrap(to, 2)
  do sleep 10
}

fn wrap(value, copies) {
  return value
}

main {
  finish null
}
"#;

const PINNED_VIEW: &str = r#"dataflow:
  actions:
    {"args":[{"atom":{"variable":"to"},"binding":0,"name":"to","type":"text"}],"available":[0],"block":{"unit":{"function":"notify"}},"callee":{"effect":"mail.send"},"kind":"perform","result_type":"bool","site":{"path":[0,0],"unit":{"function":"notify"}},"statement":{"path":[0],"unit":{"function":"notify"}}}
    {"args":[{"atom":{"variable":"line"},"binding":2,"name":"line","type":"text"}],"available":[2,1,0],"block":{"path":[1,1,0,1],"unit":{"function":"notify"}},"callee":{"effect":"log.write"},"kind":"perform","result_type":"any","site":{"path":[1,1,0,1,0,0],"unit":{"function":"notify"}},"statement":{"path":[1,1,0,1,0],"unit":{"function":"notify"}}}
    {"args":[{"atom":{"variable":"to"},"binding":0,"name":"items","type":"text"},{"atom":{"literal":{"int":"2"}},"name":"count","type":"int"}],"available":[1,0],"block":{"unit":{"function":"notify"}},"callee":{"library":{"id":"90af90dfaae324d219d00df2ed2b76cf6a1be3489240512eb3e5c0f14e3b115d","name":"text.pad"}},"kind":"call","result_type":"text","site":{"path":[3,0],"unit":{"function":"notify"}},"statement":{"path":[3],"unit":{"function":"notify"}}}
    {"args":[{"atom":{"variable":"to"},"binding":0,"name":"value","type":"any"},{"atom":{"literal":{"int":"2"}},"name":"copies","type":"any"}],"available":[1,0],"block":{"unit":{"function":"notify"}},"callee":{"declared":"wrap"},"kind":"call","result_type":"any","site":{"path":[4,0],"unit":{"function":"notify"}},"statement":{"path":[4],"unit":{"function":"notify"}}}
    {"args":[{"atom":{"variable":"to"},"binding":0,"name":"value","type":"any"},{"atom":{"literal":{"int":"2"}},"name":"copies","type":"any"}],"available":[1,0],"block":{"unit":{"function":"notify"}},"callee":{"declared":"wrap"},"kind":"spawn","result_type":{"task":"any"},"site":{"path":[5,0],"unit":{"function":"notify"}},"statement":{"path":[5],"unit":{"function":"notify"}}}
    {"args":[{"atom":{"literal":{"int":"10"}},"name":"duration","type":"any"}],"available":[1,0],"block":{"unit":{"function":"notify"}},"kind":"sleep","result_type":"any","site":{"path":[6,0],"unit":{"function":"notify"}},"statement":{"path":[6],"unit":{"function":"notify"}}}
  assignments: []
  bindings:
    {"assigned":false,"block":{"unit":{"function":"notify"}},"captured":false,"id":0,"kind":"param","name":"to","site":{"unit":{"function":"notify"}},"type":"any","uses":[{"site":{"path":[0,0],"unit":{"function":"notify"}},"statement":{"path":[0],"unit":{"function":"notify"}}},{"site":{"path":[3,0],"unit":{"function":"notify"}},"statement":{"path":[3],"unit":{"function":"notify"}}},{"site":{"path":[4,0],"unit":{"function":"notify"}},"statement":{"path":[4],"unit":{"function":"notify"}}},{"site":{"path":[5,0],"unit":{"function":"notify"}},"statement":{"path":[5],"unit":{"function":"notify"}}}]}
    {"assigned":false,"block":{"unit":{"function":"notify"}},"captured":false,"id":1,"kind":"let","name":"sent","site":{"path":[0],"unit":{"function":"notify"}},"type":"any","uses":[{"site":{"path":[1,0],"unit":{"function":"notify"}},"statement":{"path":[1],"unit":{"function":"notify"}}},{"site":{"path":[2,0,0,0],"unit":{"function":"notify"}},"statement":{"path":[2,0,0],"unit":{"function":"notify"}}}],"value_site":{"path":[0,0],"unit":{"function":"notify"}}}
    {"assigned":false,"block":{"path":[1,1,0,1],"unit":{"function":"notify"}},"captured":false,"id":2,"kind":"for","name":"line","site":{"path":[1,1,0],"unit":{"function":"notify"}},"type":"text","uses":[{"site":{"path":[1,1,0,1,0,0],"unit":{"function":"notify"}},"statement":{"path":[1,1,0,1,0],"unit":{"function":"notify"}}}]}
    {"assigned":false,"block":{"path":[2,1],"unit":{"function":"notify"}},"captured":false,"id":3,"kind":"catch","name":"e","site":{"path":[2],"unit":{"function":"notify"}},"type":"any","uses":[{"site":{"path":[2,1,0,0],"unit":{"function":"notify"}},"statement":{"path":[2,1,0],"unit":{"function":"notify"}}}]}
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
  {"kind":"call","loops":[],"site":{"path":[3,0],"unit":{"function":"notify"}},"statement":{"path":[3],"unit":{"function":"notify"}}}
  {"kind":"call","loops":[],"site":{"path":[4,0],"unit":{"function":"notify"}},"statement":{"path":[4],"unit":{"function":"notify"}}}
  {"kind":"spawn","loops":[],"site":{"path":[5,0],"unit":{"function":"notify"}},"statement":{"path":[5],"unit":{"function":"notify"}}}
  {"kind":"sleep","loops":[],"site":{"path":[6,0],"unit":{"function":"notify"}},"statement":{"path":[6],"unit":{"function":"notify"}}}
scopes:
  {"block":{"unit":{"function":"notify"}},"depth":0,"kind":"entry","ordinal":2}
  {"block":{"path":[1,1],"unit":{"function":"notify"}},"depth":1,"kind":"then","ordinal":5,"statement":{"path":[1],"unit":{"function":"notify"}}}
  {"block":{"path":[1,1,0,1],"unit":{"function":"notify"}},"depth":2,"kind":"for","ordinal":7,"statement":{"path":[1,1,0],"unit":{"function":"notify"}}}
  {"block":{"path":[1,2],"unit":{"function":"notify"}},"depth":1,"kind":"else","ordinal":9,"statement":{"path":[1],"unit":{"function":"notify"}}}
  {"block":{"path":[2,0],"unit":{"function":"notify"}},"depth":1,"kind":"try","ordinal":11,"statement":{"path":[2],"unit":{"function":"notify"}}}
  {"block":{"path":[2,1],"unit":{"function":"notify"}},"depth":1,"kind":"catch","ordinal":13,"statement":{"path":[2],"unit":{"function":"notify"}}}
statements:
  {"action":{"path":[0,0],"unit":{"function":"notify"}},"block":{"unit":{"function":"notify"}},"depth":0,"ordinal":3,"site":{"path":[0],"unit":{"function":"notify"}},"statement":{"let":{"name":"sent","value":{"action":{"perform":{"args":[{"variable":"to"}],"effect":"mail.send","result":"any"}}}}}}
  {"block":{"unit":{"function":"notify"}},"depth":0,"ordinal":4,"site":{"path":[1],"unit":{"function":"notify"}},"statement":{"if":{"condition":{"variable":"sent"},"then_block":[{"for":{"binding":"line","body":[{"do":{"action":{"perform":{"args":[{"variable":"line"}],"effect":"log.write","result":"any"}}}}],"iterable":{"list":[{"literal":{"text":"a"}}]}}}]}}}
  {"block":{"path":[1,1],"unit":{"function":"notify"}},"depth":1,"ordinal":6,"site":{"path":[1,1,0],"unit":{"function":"notify"}},"statement":{"for":{"binding":"line","body":[{"do":{"action":{"perform":{"args":[{"variable":"line"}],"effect":"log.write","result":"any"}}}}],"iterable":{"list":[{"literal":{"text":"a"}}]}}}}
  {"action":{"path":[1,1,0,1,0,0],"unit":{"function":"notify"}},"block":{"path":[1,1,0,1],"unit":{"function":"notify"}},"depth":2,"ordinal":8,"site":{"path":[1,1,0,1,0],"unit":{"function":"notify"}},"statement":{"do":{"action":{"perform":{"args":[{"variable":"line"}],"effect":"log.write","result":"any"}}}}}
  {"block":{"unit":{"function":"notify"}},"depth":0,"ordinal":10,"site":{"path":[2],"unit":{"function":"notify"}},"statement":{"try":{"body":[{"return":{"value":{"variable":"sent"}}}],"catch":{"binding":"e","body":[{"fail":{"value":{"variable":"e"}}}]}}}}
  {"block":{"path":[2,0],"unit":{"function":"notify"}},"depth":1,"ordinal":12,"site":{"path":[2,0,0],"unit":{"function":"notify"}},"statement":{"return":{"value":{"variable":"sent"}}}}
  {"block":{"path":[2,1],"unit":{"function":"notify"}},"depth":1,"ordinal":14,"site":{"path":[2,1,0],"unit":{"function":"notify"}},"statement":{"fail":{"value":{"variable":"e"}}}}
  {"action":{"path":[3,0],"unit":{"function":"notify"}},"block":{"unit":{"function":"notify"}},"depth":0,"ordinal":15,"site":{"path":[3],"unit":{"function":"notify"}},"statement":{"do":{"action":{"call":{"args":[{"variable":"to"},{"literal":{"int":"2"}}],"callee":{"library":"90af90dfaae324d219d00df2ed2b76cf6a1be3489240512eb3e5c0f14e3b115d"}}}}}}
  {"action":{"path":[4,0],"unit":{"function":"notify"}},"block":{"unit":{"function":"notify"}},"depth":0,"ordinal":16,"site":{"path":[4],"unit":{"function":"notify"}},"statement":{"do":{"action":{"call":{"args":[{"variable":"to"},{"literal":{"int":"2"}}],"callee":{"declared":"wrap"}}}}}}
  {"action":{"path":[5,0],"unit":{"function":"notify"}},"block":{"unit":{"function":"notify"}},"depth":0,"ordinal":17,"site":{"path":[5],"unit":{"function":"notify"}},"statement":{"do":{"action":{"spawn":{"args":[{"variable":"to"},{"literal":{"int":"2"}}],"callee":{"declared":"wrap"}}}}}}
  {"action":{"path":[6,0],"unit":{"function":"notify"}},"block":{"unit":{"function":"notify"}},"depth":0,"ordinal":18,"site":{"path":[6],"unit":{"function":"notify"}},"statement":{"do":{"action":{"sleep":{"duration":{"literal":{"int":"10"}}}}}}}
"#;

/// The serialized view of one workflow with a branch, a loop, a try and
/// two effects: its whole wire shape, reviewed.
#[test]
fn the_view_of_a_branch_a_loop_a_try_and_two_effects_is_pinned() {
    let definition = parse_definition(
        "function text.pad(items: Text, count: Int) -> Text\nkernel 1\ncharge 1\nnative\n",
    )
    .expect("library declaration");
    let mut functions = FunctionRegistry::declarations();
    let function = functions
        .register(definition, None)
        .expect("register declaration");
    let document =
        parse_document(&PINNED.replace("LIBRARY", &function.to_string())).expect("pinned workflow");
    let view = derive(&checked(&document, "notify", &functions)).expect("view");
    assert_eq!(
        checked(&document, "notify", &functions)
            .declarations()
            .keys()
            .collect::<Vec<_>>(),
        vec![&function],
        "the holder keeps the declaration of the library function it linked"
    );
    assert!(
        WorkflowDocument::derive(
            document.clone(),
            WorkflowDocumentEntry::Entry {
                function: Name::new("notify")
            },
            &FunctionRegistry::default()
        )
        .is_err(),
        "a document whose declarations are absent is never held, so it is never drawn"
    );
    let wire = serde_json::to_value(&view).unwrap();
    assert_eq!(
        wire["dataflow"]["bindings"][1]["uses"],
        serde_json::json!([
            {"site": site("notify", &[1, 0]), "statement": site("notify", &[1])},
            {"site": site("notify", &[2, 0, 0, 0]), "statement": site("notify", &[2, 0, 0])},
        ])
    );
    assert_eq!(
        wire["scopes"][1]["statement"],
        serde_json::json!(site("notify", &[1]))
    );
    assert_eq!(
        wire["dataflow"]["actions"][0]["callee"],
        serde_json::json!({"effect": "mail.send"})
    );
    assert_eq!(wire["dataflow"]["actions"][0]["kind"], "perform");
    assert_eq!(wire["dataflow"]["actions"][5]["kind"], "sleep");
    assert_eq!(
        wire["dataflow"]["actions"][5]["args"][0]["name"],
        "duration"
    );
    assert_eq!(
        wire["dataflow"]["actions"][2]["callee"],
        serde_json::json!({"library": {"id": function, "name": "text.pad"}})
    );
    assert_eq!(wire["dataflow"]["actions"][2]["args"][0]["name"], "items");
    assert_eq!(wire["dataflow"]["actions"][2]["args"][1]["name"], "count");
    for index in [3, 4] {
        assert_eq!(
            wire["dataflow"]["actions"][index]["callee"],
            serde_json::json!({"declared": "wrap"})
        );
        assert_eq!(
            wire["dataflow"]["actions"][index]["args"][0]["name"],
            "value"
        );
        assert_eq!(
            wire["dataflow"]["actions"][index]["args"][1]["name"],
            "copies"
        );
    }
    let mut rendered = String::new();
    render(&serde_json::to_value(&view).unwrap(), 0, &mut rendered);
    assert_eq!(
        rendered, PINNED_VIEW,
        "the view drifted; it is now:\n{rendered}"
    );
    let decoded: WorkflowView =
        serde_json::from_value(serde_json::to_value(&view).unwrap()).expect("the view decodes");
    assert_eq!(decoded, view);
    for kind in [
        SiteKind::Call,
        SiteKind::Perform,
        SiteKind::Sleep,
        SiteKind::Join,
        SiteKind::JoinMany,
        SiteKind::Yield,
        SiteKind::Spawn,
        SiteKind::Cancel,
    ] {
        assert_eq!(serde_json::to_value(kind).unwrap(), kind.to_string());
    }
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
    let mut draft = Draft::open(document.clone()).expect("draft");
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
    use lash_kernel_doc::{SpawnIdentity, TaskIdentity};
    let document = parse_document("kernel 1\nnumbers float\nentry demo() -> Any\nfn demo() { if true {} return null }\nmain { finish null }\n").unwrap();
    let unit = Unit::Function(Name::new("demo"));
    let spawned = TaskIdentity::Spawned(SpawnIdentity {
        parent: std::sync::Arc::new(TaskIdentity::Main),
        site: site("demo", &[0]),
        occurrence: 0,
    });
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
                    task: TaskIdentity::Main,
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
            .all(|step| step.task == TaskIdentity::Main)
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
