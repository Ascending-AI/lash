//! The `examples/codemode-parity` cells, linked against a host catalogue.
//!
//! The cells are the public surface's worked examples, so they are the one
//! place a retired spelling is most expensive and least visible: nothing
//! executes a `.ts` file on disk. Linking them here means a deleted form
//! (`defineProcess`, bare `start`, `wake`, `registerTrigger`, a `signals:`
//! block) fails this target rather than surviving in documentation.

use std::collections::BTreeSet;

const TURN: &str = include_str!("../../../examples/codemode-parity/turn.ts");
const DURABLE_PROCESS: &str = include_str!("../../../examples/codemode-parity/durable-process.ts");

/// The catalogue the parity cells are written against: the shipped process
/// controls the cells call, plus the one web authority `turn.ts` fetches with.
fn parity_environment() -> lashlang::LashlangHostEnvironment {
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

/// The forms ADR 0095 deleted.
///
/// `start`, `wake` and `registerTrigger` were bare calls; the surviving
/// spellings are qualified (`processes.start`, `triggers.register`), so a bare
/// call is the retired one and a receiver call is not.
fn assert_names_no_retired_form(label: &str, source: &str) {
    for retired in ["defineProcess", "signals:"] {
        assert!(
            !source.contains(retired),
            "{label} still names the retired form `{retired}`"
        );
    }
    for retired in ["start", "wake", "registerTrigger"] {
        let call = format!("{retired}(");
        let mut rest = source;
        while let Some(at) = rest.find(&call) {
            let qualified = rest[..at].ends_with('.');
            let word_start = rest[..at]
                .chars()
                .next_back()
                .is_none_or(|ch| !(ch.is_alphanumeric() || ch == '_' || ch == '$'));
            assert!(
                qualified || !word_start,
                "{label} still calls the retired bare form `{call})`"
            );
            rest = &rest[at + call.len()..];
        }
    }
}

#[test]
fn the_turn_example_links_against_its_host_catalogue() {
    lash_typescript::link(TURN, &parity_environment()).expect("turn.ts should link");
}

#[test]
fn the_durable_process_example_links_and_lifts_one_process() {
    let linked = lash_typescript::link(DURABLE_PROCESS, &parity_environment())
        .expect("durable-process.ts should link");
    let processes = linked
        .artifact
        .ir
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

#[test]
fn neither_parity_example_names_a_retired_form() {
    assert_names_no_retired_form("turn.ts", TURN);
    assert_names_no_retired_form("durable-process.ts", DURABLE_PROCESS);
}
