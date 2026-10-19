//! FIG-6825: a workflow view states its entry's declared signature and what
//! a call can end with, and reads a condition as the comparison its dialect
//! wrote, through the variables a lowering binds once, for a TypeScript and
//! a Python workflow read with no engine.
use lash::workflow::document::{self as k};
use lash::workflow::view::{
    BindingKind, ComparisonView, ValueRef, ValueSource, WorkflowNodeKind, WorkflowView, derive,
};
use lash::workflow::{WorkflowDocument, WorkflowDocumentEntry};
use std::collections::{BTreeMap, BTreeSet};

const TRIAGE: &str = r#"export async function triage(title: string, priority: string, notes?: string) {
  if (priority === "high") {
    await inbox.send_item({ title: "Urgent request", body: "Needs an answer today." });
    return { routed: "urgent" };
  } else {
    await inbox.send_item({ title: "Request filed", body: "Filed for later." });
    return { routed: "later" };
  }
}"#;

#[derive(Clone, Copy)]
enum Dialect {
    TypeScript,
    Python,
}

/// The view of `entry` in `source`, lowered as a module against one
/// `inbox.send_item` tool.
fn view(dialect: Dialect, source: &str, entry: &str) -> WorkflowView {
    let embedding = lash_vm_worker::built_embedding(&Default::default()).expect("embedding");
    let registry = embedding.registry().expect("registry");
    let effects = BTreeMap::from([(
        k::EffectName::new("inbox.send_item").expect("a tool's name"),
        k::Signature {
            params: vec![k::Param {
                name: k::Name::new("input"),
                ty: k::Type::Any,
                optional: false,
            }],
            result: k::Type::Any,
        },
    )]);
    let roots = BTreeSet::from([k::Name::new("inbox")]);
    let environment = lash::dialect::Environment {
        library: embedding.library(),
        effects: &effects,
        tool_roots: &roots,
        controls: &Default::default(),
    };
    let mode = lash::dialect::Mode::Module;
    let document = match dialect {
        Dialect::TypeScript => lash::typescript::lower(source, &environment, mode)
            .expect("lower TypeScript")
            .document
            .into_document(),
        Dialect::Python => lash::python::lower(source, &environment, mode)
            .expect("lower Python")
            .document
            .into_document(),
    };
    let entry = WorkflowDocumentEntry::Entry {
        function: k::Name::new(entry),
    };
    let functions = registry.export_declarations();
    derive(
        &WorkflowDocument::derive(document, entry, &functions).expect("link"),
        &super::test_packages(),
    )
    .expect("view")
}

fn binding(view: &WorkflowView, name: &str) -> usize {
    view.dataflow
        .bindings
        .iter()
        .find(|binding| binding.name.as_str() == name)
        .unwrap_or_else(|| panic!("a binding `{name}`:\n{}", view.canonical_source))
        .id
}

/// The comparison of each control node, in document order. A control
/// with no action anywhere in it is code, with no header.
fn comparisons(view: &WorkflowView) -> Vec<Option<ComparisonView>> {
    view.nodes
        .iter()
        .filter(|node| node.kind == WorkflowNodeKind::Control)
        .map(|node| node.header.as_ref().expect("a header").comparison.clone())
        .collect()
}

fn parameter(binding: usize) -> ValueRef {
    ValueRef {
        source: ValueSource::Parameter { binding },
        fields: Vec::new(),
    }
}

fn text(value: &str) -> ValueRef {
    ValueRef {
        source: ValueSource::Literal {
            value: k::Literal::Text(value.into()),
        },
        fields: Vec::new(),
    }
}

/// FIG-6825 (1): the entry's parameters carry the types its signature
/// declares, not their bindings' facets, and its result facet is the union
/// of the records its reachable returns build: the lowering's closing
/// `return absent` follows an `if` whose arms both return, so it adds
/// nothing.
#[test]
fn the_triage_entry_states_its_declared_parameters_and_the_union_of_its_returns() {
    let view = view(Dialect::TypeScript, TRIAGE, "triage");
    let source = &view.canonical_source;
    let entry = &view.entry;
    assert_eq!(entry.name, Some(k::Name::new("triage")), "{source}");
    let params: Vec<_> = entry
        .params
        .iter()
        .map(|param| {
            (
                param.name.as_str(),
                &param.ty,
                param.optional,
                param.binding,
            )
        })
        .collect();
    let optional_text = k::Type::Union(vec![k::Type::Text, k::Type::Null, k::Type::Absent]);
    assert_eq!(
        params,
        [
            ("title", &k::Type::Text, false, binding(&view, "title")),
            (
                "priority",
                &k::Type::Text,
                false,
                binding(&view, "priority")
            ),
            ("notes", &optional_text, true, binding(&view, "notes")),
        ],
        "{source}"
    );
    let title = view
        .dataflow
        .bindings
        .iter()
        .find(|binding| binding.name.as_str() == "title")
        .expect("title's binding");
    assert_eq!(
        (title.kind, &title.ty),
        (BindingKind::Param, &k::Type::Any),
        "the binding's facet says less than the signature"
    );
    assert_eq!(entry.result, k::Type::Any, "the entry declares no result");
    let routed = k::Type::Record(k::RecordType {
        fields: vec![k::RecordTypeField {
            name: "routed".into(),
            ty: k::Type::Text,
            optional: false,
        }],
        rest: None,
    });
    assert!(
        view.dataflow
            .returns
            .iter()
            .any(|ending| ending.ty == k::Type::Absent),
        "the lowering closes the body with `return absent`:\n{source}"
    );
    assert_eq!(entry.result_facet, routed, "{source}");
}

/// FIG-6825 (1): an entry whose body can complete has `null` in its result
/// facet beside what its returns build.
#[test]
fn an_entry_that_may_fall_through_has_null_in_its_result_facet() {
    let view = view(
        Dialect::Python,
        "async def check(flag: bool):\n    if flag:\n        return \"yes\"\n",
        "check",
    );
    assert_eq!(
        view.entry.result_facet,
        k::Type::Union(vec![k::Type::Text, k::Type::Null]),
        "{}",
        view.canonical_source
    );
}

/// FIG-6825 (2): TypeScript's `===` and Python's `==` on a parameter and a
/// literal read as the same `eq`, each through the comparison its own
/// library declares.
#[test]
fn a_typescript_and_a_python_equality_read_as_eq_of_the_parameter_and_the_literal() {
    let python = "async def triage(title: str, priority: str):\n    if priority == \"high\":\n        await inbox.send_item({\"title\": \"Urgent request\"})\n        return {\"routed\": \"urgent\"}\n    else:\n        await inbox.send_item({\"title\": \"Request filed\"})\n        return {\"routed\": \"later\"}\n";
    for view in [
        view(Dialect::TypeScript, TRIAGE, "triage"),
        view(Dialect::Python, python, "triage"),
    ] {
        assert_eq!(
            comparisons(&view),
            [Some(ComparisonView::Eq {
                left: parameter(binding(&view, "priority")),
                right: text("high"),
            })],
            "{}",
            view.canonical_source
        );
    }
}

/// FIG-6825 (2): a condition variable bound once to a comparison, right
/// before its `if`, reads as that comparison, and a variable bound once to
/// an action reads as that action's result. Reassigned, either reads as
/// nothing.
#[test]
fn a_condition_reads_through_once_bound_variables_and_never_through_a_reassigned_one() {
    let view = view(
        Dialect::TypeScript,
        r#"export async function route(priority: string) {
  const reply = await inbox.send_item({ title: priority });
  const urgent = priority !== "low";
  if (urgent) { await inbox.send_item({ title: "1" }); }
  if (reply === "ok") { await inbox.send_item({ title: "2" }); }
  let again = priority === "high";
  again = false;
  if (again) { await inbox.send_item({ title: "3" }); }
  let later = await inbox.send_item({ title: priority });
  later = "no";
  if (later === "ok") { await inbox.send_item({ title: "4" }); }
}"#,
        "route",
    );
    let source = &view.canonical_source;
    let reply = view
        .dataflow
        .bindings
        .iter()
        .find(|binding| binding.name.as_str() == "reply")
        .and_then(|binding| binding.value_site.clone())
        .expect("reply holds the first send");
    assert!(
        view.dataflow
            .actions
            .iter()
            .any(|action| action.site == reply),
        "reply holds an action's result:\n{source}"
    );
    assert_eq!(
        comparisons(&view),
        [
            Some(ComparisonView::Ne {
                left: parameter(binding(&view, "priority")),
                right: text("low"),
            }),
            Some(ComparisonView::Eq {
                left: ValueRef {
                    source: ValueSource::Result { site: reply },
                    fields: Vec::new(),
                },
                right: text("ok"),
            }),
            None,
            None,
        ],
        "{source}"
    );
}

/// FIG-6825 (2): a condition on a reassigned parameter, or one that calls a
/// function no library declares a comparison, reads as nothing.
#[test]
fn a_condition_on_a_reassigned_parameter_or_a_non_comparison_call_has_no_comparison() {
    let view = view(
        Dialect::Python,
        "async def route(priority: str, tags: list):\n    if len(tags):\n        await inbox.send_item({\"title\": \"1\"})\n    priority = \"low\"\n    if priority == \"high\":\n        await inbox.send_item({\"title\": \"2\"})\n",
        "route",
    );
    assert_eq!(
        comparisons(&view),
        [None, None],
        "{}",
        view.canonical_source
    );
}
