//! The `examples/typescript-host-flows` cells, linked against a host catalogue.
//!
//! The cells are the public surface's worked examples, so they are the one
//! place a retired spelling is most expensive and least visible: nothing
//! executes a `.ts` file on disk. Linking them here means a deleted form
//! (`defineProcess`, bare `start`, `wake`, `registerTrigger`, a `signals:`
//! block) fails this target rather than surviving in documentation.

use std::collections::BTreeSet;

const DURABLE_PROCESS: &str =
    include_str!("../../../examples/typescript-host-flows/durable-process.ts");

/// The catalogue the host-flow cells are written against: the shipped process
/// controls the cells call, plus the one web authority `turn.ts` fetches with.
fn host_environment() -> lashlang::LashlangHostEnvironment {
    let mut catalog = lashlang::LashlangHostCatalog::new();
    catalog
        .add_module_operation_contract(
            ["processes"],
            "Processes",
            "start",
            "tool:processes/start",
            &lashlang::OperationContract::new(
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "definition": { "x-lash": { "kind": "process_unknown" } },
                        "args": { "type": "object" },
                    },
                    "required": ["definition"]
                }),
                serde_json::json!({ "x-lash": { "kind": "process_unknown" } }),
            ),
        )
        .expect("process start operation");
    catalog
        .add_module_operation_contract(
            ["processes"],
            "Processes",
            "emit",
            "tool:processes/emit",
            &lashlang::OperationContract::new(
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "value": {} },
                    "required": ["value"]
                }),
                serde_json::json!({}),
            ),
        )
        .expect("process emit operation");
    catalog
        .add_module_operation_contract(
            ["web"],
            "Web",
            "fetch",
            "tool:web/fetch",
            &lashlang::OperationContract::new(
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "url": { "type": "string" } },
                    "required": ["url"]
                }),
                serde_json::json!({}),
            ),
        )
        .expect("web fetch operation");
    lashlang::LashlangHostEnvironment::new(catalog, lashlang::LashlangAbilities::all())
}

#[test]
fn the_durable_process_example_links_and_lifts_one_process() {
    let linked = lash_typescript::link(DURABLE_PROCESS, &host_environment())
        .expect("durable-process.ts should link");
    let processes = linked
        .artifact
        .ir()
        .declarations
        .iter()
        .filter_map(|declaration| match declaration {
            lashlang::Declaration::Process(process) => Some(process),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [approval] = processes.as_slice() else {
        panic!(
            "expected exactly one lifted process, found {}",
            processes.len()
        )
    };
    assert_eq!(
        approval
            .params
            .iter()
            .map(|param| param.name.to_string())
            .collect::<Vec<_>>(),
        vec!["request".to_string()],
    );
    // The signal set is inferred from the `waitSignal` the body reaches, not
    // declared: there is no `signals:` block to carry it any more.
    assert_eq!(
        approval
            .signals
            .iter()
            .map(|signal| signal.name.to_string())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["approved".to_string()]),
    );
}
