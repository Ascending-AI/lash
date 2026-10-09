//! The `examples/typescript-host-flows` cells, linked against a host catalogue.
//!
//! The cells are the public surface's worked examples, so they are the one
//! place a retired spelling is most expensive and least visible: nothing
//! executes a `.ts` file on disk. Linking these cells checks that their calls
//! and process definitions match the host catalogue.

const DURABLE_PROCESS: &str =
    include_str!("../../../examples/typescript-host-flows/durable-process.ts");

/// The catalogue the host-flow cells are written against: the shipped process
/// controls the cells call, plus the one web authority `turn.ts` fetches with.
fn host_environment() -> lash_vm::LashVmHostEnvironment {
    let mut catalog = lash_vm::LashVmHostCatalog::new();
    catalog
        .add_module_operation_contract(
            ["processes"],
            "Processes",
            "start",
            "tool:processes/start",
            &lash_vm::OperationContract::new(
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
            ["host"],
            "Host",
            "approval",
            "tool:host/approval",
            &lash_vm::OperationContract::new(
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "request": {} },
                    "required": ["request"]
                }),
                serde_json::json!({}),
            ),
        )
        .expect("host approval operation");
    catalog
        .add_module_operation_contract(
            ["web"],
            "Web",
            "fetch",
            "tool:web/fetch",
            &lash_vm::OperationContract::new(
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
    lash_vm::LashVmHostEnvironment::new(catalog)
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
            lash_vm::Declaration::Process(process) => Some(process),
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
}
