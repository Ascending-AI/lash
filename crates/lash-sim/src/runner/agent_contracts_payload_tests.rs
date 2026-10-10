//! Process event normalization preserves semantic facts and identity relations.

use super::*;

#[test]
fn process_effect_outcome_contract_normalizes_only_opaque_replay_identity() {
    let first = json!({"replay_key": "first", "node_id": "node:a", "outcome_class": "success"});
    let second = json!({"replay_key": "second", "node_id": "node:a", "outcome_class": "success"});
    let changed_outcome =
        json!({"replay_key": "second", "node_id": "node:a", "outcome_class": "failure"});

    assert_eq!(
        normalize_contract_process_event_payload("process.effect_outcome", first.clone()),
        normalize_contract_process_event_payload("process.effect_outcome", second)
    );
    assert_ne!(
        normalize_contract_process_event_payload("process.effect_outcome", first.clone()),
        normalize_contract_process_event_payload("process.effect_outcome", changed_outcome)
    );
    assert_eq!(
        normalize_contract_process_event_payload("process.completed", first.clone()),
        first
    );

    let wait = |call: &str, time: u64, tool: &str| json!({"wait": {"kind": {"kind": "call", "call_id": call, "tool_id": tool}, "since_ms": time}});
    let normalize_pair = |waiting: Value, resumed: Value| {
        let mut identities = ContractEventIdentities::default();
        vec![
            identities.normalize("process.waiting", waiting),
            identities.normalize("process.resumed", resumed),
        ]
    };
    let original = normalize_pair(wait("first", 100, "tool:a"), wait("first", 100, "tool:a"));
    assert_eq!(
        original,
        normalize_pair(wait("fresh", 900, "tool:a"), wait("fresh", 900, "tool:a")),
        "fresh identities retain the same waiting/resumed relationship"
    );
    assert_eq!(original[0]["wait"]["kind"]["call_id"], "call-1");
    assert_eq!(original[0], original[1]);
    let mut identities = ContractEventIdentities::default();
    identities.normalize("process.waiting", wait("first", 100, "tool:a"));
    let effect = |call: Value| json!({"call_id": call, "outcome_class": "success"});
    assert_eq!(
        identities.normalize("process.effect_outcome", effect(json!("first")))["call_id"],
        "call-1"
    );
    assert_eq!(
        identities.normalize("process.effect_outcome", effect(json!("other")))["call_id"],
        "call-2"
    );
    assert!(
        identities.normalize("process.effect_outcome", effect(Value::Null))["call_id"].is_null()
    );
    for changed in [
        wait("other", 100, "tool:a"),
        wait("first", 101, "tool:a"),
        wait("first", 100, "tool:b"),
        json!({"wait": {"kind": {"kind": "signal", "name": "answer"}, "since_ms": 100}}),
        json!({"wait": {"kind": {"kind": "call", "tool_id": "tool:a"}, "since_ms": 100}}),
    ] {
        assert_ne!(
            original,
            normalize_pair(wait("first", 100, "tool:a"), changed),
            "a changed call, timestamp relationship, tool, kind or missing identity stays observable"
        );
    }
}

/// FIG-5386: the durable input row hashes identically on re-execution however
/// late its host resolves the key. A host that answers inside the park's
/// commit window and one that answers after the park committed are the same
/// contract execution; each is run concurrently, under the load of the others,
/// and compared with the replay the generated oracle compares against.
#[test]
fn durable_input_row_hashes_identically_however_late_its_host_resolves() {
    let contract = "agent.durable_input_suspension_resolution";
    let row = agent_contract_row(contract).expect("the durable input row is registered");
    let runs = [0_u64, 1, 5, 25, 250]
        .into_iter()
        .map(|delay_ms| {
            std::thread::spawn(move || {
                let payload = run_on_sim_harness_stack(
                    format!("durable-input-host-delay-{delay_ms}ms"),
                    SIM_HARNESS_STACK_LIMIT_BYTES,
                    move || {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .map_err(FixedScriptRunnerError::Io)?
                            .block_on(facade_agent_durable_input_execution(
                                std::time::Duration::from_millis(delay_ms),
                            ))
                    },
                );
                (delay_ms, payload)
            })
        })
        .collect::<Vec<_>>();
    let replayed = replay_contract_execution(contract).expect("the row re-executes");
    for run in runs {
        let (delay_ms, payload) = run.join().expect("the host-delay run joins");
        let observed = contract_execution_payload(row, payload.expect("the row executes"))
            .expect("the row's payload hashes");
        assert_eq!(
            observed["result"], replayed["result"],
            "a host resolving {delay_ms}ms after the key changed the recorded execution"
        );
        assert_eq!(observed["source"], replayed["source"]);
    }
}

/// A process a cell creates carries no author-chosen name: a host-declared
/// `label` replaces the runtime's derived one as display metadata without
/// touching the process's identity. The evidence is structural (the
/// process's definition reads as an entry of an admitted kernel document),
/// so a renamed display label must not change the oracle's verdict.
#[tokio::test]
async fn process_display_name_does_not_change_the_oracle_verdict() {
    let expected = json!({ "ok": true });
    let result = facade_agent_process_execution(
        "lash_runtime agent renamed process",
        &SessionId::from("sim-agent-renamed-process-contract"),
        "Start a process under a declared display label and return its value.",
        vec![
            r#"<typescript>
const lookup = async () => {
  return { ok: true };
};
const handle = await processes.start({ definition: lookup, label: "renamed display label" });
const result = await handle;
await control.finish(result);
</typescript>"#,
        ],
        &expected,
        None,
    )
    .await
    .expect("renamed process contract world");

    let entries = result
        .pointer("/process_facts/completed_entries")
        .and_then(Value::as_array)
        .expect("process_facts records completed entries");
    assert_eq!(
        entries.iter().filter_map(Value::as_str).collect::<Vec<_>>(),
        vec!["renamed display label"],
        "the display label, not a derived name, is what observation records",
    );

    crate::oracles::require_agent_document_entry_processes(&result, 1, "test")
        .expect("a renamed display label must not change the document-entry verdict");
}
