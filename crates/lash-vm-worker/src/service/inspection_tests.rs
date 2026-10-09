//! Worker inspection returns a complete typed workflow document.

use lash_vm::testing::ast_builders as b;

fn region_program() -> lash_vm::Program {
    b::program(vec![
        b::try_expr(
            b::block(vec![b::print(b::string("body"))]),
            Some(b::catch("error", b::block(vec![b::print(b::var("error"))]))),
            Some(b::block(vec![b::print(b::string("finally"))])),
        ),
        b::role(
            lash_vm::StructuralRole::Scope,
            b::block(vec![b::print(b::string("scoped"))]),
        ),
    ])
}

/// The kind of every node under `value`, in document order: a container's
/// own kind, then its bodies' nodes.
fn node_kinds(value: &serde_json::Value, kinds: &mut Vec<String>) {
    match value {
        serde_json::Value::Array(values) => {
            values.iter().for_each(|value| node_kinds(value, kinds));
        }
        serde_json::Value::Object(object) => {
            if object.contains_key("id")
                && let Some(kind) = object.get("kind").and_then(serde_json::Value::as_object)
            {
                let name = kind.get("container_kind").or_else(|| kind.get("kind"));
                kinds.extend(name.and_then(serde_json::Value::as_str).map(str::to_string));
            }
            for (key, value) in object {
                if key != "edges" {
                    node_kinds(value, kinds);
                }
            }
        }
        _ => {}
    }
}

/// FIG-5572: a `try`/`catch`/`finally` and a scoped region used to reach the
/// host as opaque nodes whose source text was empty, so the inspected graph
/// did not hold the program. Each is a typed region whose statements are
/// nodes.
#[test]
fn inspection_projects_try_and_scope_as_typed_regions() {
    let artifact =
        lash_vm::ModuleArtifact::from_program(region_program()).expect("the program is valid IR");
    let inspected = super::inspect(&artifact).expect("the artifact inspects");
    let graph = serde_json::to_value(&inspected.graph).expect("the graph serializes");
    let mut kinds = Vec::new();
    node_kinds(&graph["main"], &mut kinds);
    assert_eq!(
        kinds,
        ["try", "effect", "effect", "effect", "scope", "effect"],
        "every statement of every region is a typed node: {graph:#}"
    );
    assert_eq!(
        &lash_vm::workflow_program_from_graph(&inspected.graph)
            .expect("the inspected document reconstructs"),
        artifact.ir(),
        "the inspected document is the program the artifact runs"
    );
}
