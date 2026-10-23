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
    use lash_vocabulary::sites::Unit;
    let (root, locals) = parse_definition_with_locals("function app.make(value: Any) -> Any\nkernel 1\nmetering statements\nbody { let f = fn() { return value } return f }").unwrap();
    let anonymous = locals[0].identity();
    let root_id = root.identity();
    let mut document = Document::new(NumberPolicy::Float, vec![]);
    document.functions.insert(root_id, root);
    document
        .functions
        .extend(locals.into_iter().map(|f| (f.identity(), f)));
    let link = lash_vocabulary::Link {
        document: document.identity(),
        fills: Default::default(),
    };
    let mut content = lash_kernel_package::LinkedContent::default();
    content
        .documents
        .insert(document.identity(), document.clone());
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
