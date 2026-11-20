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
        helper_fills: Default::default(),
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
            helper_fills: Default::default(),
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

/// A view of `source`'s root function, its local closures included.
fn root_view(source: &str) -> (crate::WorkflowView, lash_kernel_check::WorkflowDocument) {
    use lash_kernel_doc::{Document, NumberPolicy, parse_definition_with_locals};
    use lash_kernel_package::{LinkedStart, StartSelector};
    use lash_kernel_value::sites::Unit;
    let (root, locals) = parse_definition_with_locals(source).unwrap();
    let root_id = root.identity();
    let mut document = Document::new(NumberPolicy::Float, vec![]);
    document.functions.insert(root_id, root);
    document
        .functions
        .extend(locals.into_iter().map(|f| (f.identity(), f)));
    let link = lash_kernel_value::Link {
        helper_fills: Default::default(),
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
    (view, checked)
}

/// The closure expressions of `checked`, in graph order.
fn closures(checked: &lash_kernel_check::WorkflowDocument) -> Vec<lash_kernel_value::sites::Site> {
    checked
        .graph()
        .nodes()
        .iter()
        .filter(|node| {
            matches!(
                node.kind,
                lash_kernel_check::NodeKind::Expr(lash_kernel_check::ExprNode::Closure { .. })
            )
        })
        .map(|node| node.site.clone())
        .collect()
}

/// FIG-7723: a value that holds itself (`x = {inner: x}`) still carries the
/// closure it held before, at every depth. The stack-cut analysis answered
/// `x` while `x` was being read and dropped `x.inner.inner`, missing a call.
#[test]
fn a_closure_reached_through_a_value_that_holds_itself_is_applied() {
    let (view, checked) = root_view(
        "function law.nest() -> Any\nkernel 1\nmetering statements\nbody { let g = fn() { return null } let x = { inner: g } set x = { inner: x } let a = x.inner let b = a.inner do apply b() return null }",
    );
    let [g] = closures(&checked).try_into().unwrap();
    let applied: Vec<_> = view
        .dataflow
        .actions
        .iter()
        .filter(|action| action.callee.is_none())
        .map(|action| action.applies.clone())
        .collect();
    assert_eq!(applied, [vec![g]]);
}

/// FIG-7723: helpers that call each other in a cycle and pass one
/// parameter along are solved in work linear in the graph. Each helper
/// `h_i(p)` applies `p`, `h_{i+1}` and `h_{i+2}` (indices mod N), and the
/// root applies `h_0(g)`. The stack-cut analysis followed every simple path
/// back through the callers, a Fibonacci number of them in N.
///
/// Bound, by hand: one result per graph node and binding, at most. Results
/// outside the cycle (the helper variables, `g`) are final before it is
/// solved and evaluated once. In the cycle, every value holds one closure at
/// its root (`g` for each `p`, the returns and the `do apply p()` results), so it
/// grows at most once, and a result is revisited at most once for each of
/// the at most two growing results or new callers it reads. So evaluations
/// are at most three per result, and nodes plus bindings bound results.
#[test]
fn helpers_feeding_each_others_parameters_derive_in_linear_work() {
    for helpers in [16, 64] {
        let mut body = String::new();
        for index in 0..helpers {
            body.push_str(&format!("let h{index} = null "));
        }
        for index in 0..helpers {
            let (next, after) = ((index + 1) % helpers, (index + 2) % helpers);
            body.push_str(&format!(
                "set h{index} = fn(p) {{ do apply p() do apply h{next}(p) do apply h{after}(p) return p }} "
            ));
        }
        body.push_str("let g = fn() { return null } do apply h0(g) return null");
        crate::applies::take_computed();
        let (view, checked) = root_view(&format!(
            "function law.ring() -> Any\nkernel 1\nmetering statements\nbody {{ {body} }}"
        ));
        let computed = crate::applies::take_computed();
        let graph = checked.graph();
        let bound = 3 * (graph.nodes().len() + graph.bindings().len()) as u64;
        println!("FIG-7723 helpers={helpers} computed={computed} bound={bound}");
        assert!(
            computed <= bound,
            "{helpers} helpers took {computed} evaluations, bound {bound}"
        );
        // Every `apply p()` runs `g`, which reaches each helper only around
        // the cycle.
        let g = closures(&checked).pop().unwrap();
        let through_p = view
            .dataflow
            .actions
            .iter()
            .filter(|action| action.applies == [g.clone()])
            .count();
        assert_eq!(through_p, helpers, "each helper applies its parameter");
    }
}
