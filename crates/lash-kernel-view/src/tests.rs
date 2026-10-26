//! Schema laws independent of the replaced installed-set view.
use serde_json::Value;

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

/// K-CLO-001/K-SITE: a reference-only Closure still projects its retained body.
#[test]
fn closure_view_uses_retained_anonymous_body_and_unit() {
    use lash_kernel_doc::{Document, NumberPolicy, parse_definition_with_locals};

    use lash_kernel_package::{LinkedStart, StartSelector};
    use lash_kernel_value::sites::Unit;
    let (root, locals) = parse_definition_with_locals("function app.make(value: Any) -> Any\nkernel 1\nmetering statements\nbody { let f = fn() { return value } return f }").unwrap();
    let anonymous = locals[0].identity();
    let root_id = root.identity();
    let mut document = Document::new(NumberPolicy::Float, vec![]);
    document.functions.insert(root_id, root);
    document
        .functions
        .extend(locals.into_iter().map(|f| (f.identity(), f)));
    let link = lash_kernel_value::Link {
        document: document.identity(),
        fills: Default::default(),
    };
    let mut content = lash_kernel_package::LinkedContent::default();
    content
        .documents
        .insert(document.identity(), document.clone().into());
    content.links.insert(link.identity(), link.clone());
    let reference = LinkedStart {
        link: link.identity(),
        selector: StartSelector::Main,
    };
    let context = crate::ViewContext::new(
        reference.clone().into(),
        content,
        Default::default(),
        lash_kernel_doc::BuildHelpers::empty(),
    )
    .unwrap();
    let checked = lash_kernel_check::WorkflowDocument::entered(
        document.clone(),
        reference,
        &document.functions,
    )
    .unwrap();
    let view = crate::derive_callable(&checked, &Unit::Library(root_id), &context).unwrap();
    assert!(
        view.callables
            .iter()
            .any(|callable| callable.block.unit == Unit::Library(anonymous))
    );
    assert!(
        view.statements
            .iter()
            .any(|statement| statement.site.unit == Unit::Library(anonymous))
    );
}

/// FIG-7357: view reads admit helper closure once per Link, rather than
/// traversing the whole helper graph for every label, header and callable.
#[test]
fn helper_view_reads_walk_admission_edges_once_per_link() {
    use lash_kernel_doc::{
        BuildHelpers, Document, FunctionRegistry, NumberPolicy, parse_definition,
    };
    use lash_kernel_package::{LinkedContent, LinkedStart, StartSelector};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use {lash_kernel_value::CallableId, lash_kernel_value::Link};

    let mut registry = FunctionRegistry::declarations();
    let mut previous = None;
    let mut functions = Vec::new();
    for index in 0..12 {
        let dependency = previous.map_or_else(String::new, |id| {
            format!("use law.helper_{} = @{id}\n", index - 1)
        });
        let body = if index == 0 {
            "return null".into()
        } else {
            format!(
                "let answer = invoke law.helper_{}() return answer",
                index - 1
            )
        };
        let definition = parse_definition(&format!(
            "function law.helper_{index}() -> Null\nkernel 1\ncharge 1\n{dependency}body {{ {body} }}"
        )).unwrap();
        let id = definition.identity();
        registry.register(definition, None).unwrap();
        previous = Some(id);
        functions.push(id);
    }
    let root = previous.unwrap();
    let helpers = Arc::new(BuildHelpers::new(registry, [root]).unwrap());
    let mut document = Document::new(NumberPolicy::BySpelling, vec![]);
    document.library.insert(root);
    let empty = Document::new(NumberPolicy::BySpelling, vec![]);
    let mut content = LinkedContent::default();
    let mut links = Vec::new();
    for document in [document, empty] {
        let id = document.identity();
        let link = Link {
            document: id,
            fills: BTreeMap::new(),
        };
        links.push(link.identity());
        content.documents.insert(id, document.into());
        content.links.insert(link.identity(), link);
    }
    let labels = functions
        .iter()
        .map(|id| (*id, "\"Helper\"".into()))
        .collect();
    BuildHelpers::take_admission_edge_visits();
    let mut context = crate::ViewContext::new(
        LinkedStart {
            link: links[0],
            selector: StartSelector::Main,
        }
        .into(),
        content,
        BTreeMap::new(),
        helpers,
    )
    .unwrap();
    context.add_helper_labels(&labels).unwrap();
    for function in functions {
        let admitted = CallableId {
            link: links[0],
            function,
        };
        let excluded = CallableId {
            link: links[1],
            function,
        };
        assert!(context.header(&admitted).is_some());
        assert!(context.function(&admitted).is_some());
        assert!(context.labels.contains_key(&admitted));
        assert!(context.header(&excluded).is_none());
        assert!(context.function(&excluded).is_none());
        assert!(!context.labels.contains_key(&excluded));
    }
    let edges = BuildHelpers::take_admission_edge_visits();
    println!("VIEW-ADMIT-ONCE edges={edges} bound=11");
    assert!(
        edges <= 11,
        "view reads rewalked the helper closure: {edges} edges, bound 11"
    );
}
