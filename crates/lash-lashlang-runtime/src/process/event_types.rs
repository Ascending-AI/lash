use super::lashlang_type_expr_schema;

pub fn lashlang_process_event_types() -> Vec<lash_core::ProcessEventType> {
    vec![
        // `process.yield` is the one progress emission a running process can
        // make: the guest `yield` form and the shipped `processes.emit` leaf
        // tool both append under it (ADR 0095 deleted the `wake` special form
        // that used to carry the wake). A progress emission is therefore what
        // reaches the declaring session, so the wake is declared here — the
        // event type is the only place a wake can be materialized from an
        // append, and both producers go through it.
        lash_core::ProcessEventType {
            name: "process.yield".to_string(),
            payload_schema: lash_core::LashSchema::any(),
            semantics: lash_core::ProcessEventSemanticsSpec {
                wake: Some(lash_core::ProcessWakeSpec {
                    when: None,
                    input: lash_core::ProcessValueSelector::Payload,
                }),
                ..lash_core::ProcessEventSemanticsSpec::default()
            },
        },
        lash_core::ProcessEventType {
            name: "process.wake".to_string(),
            payload_schema: lash_core::LashSchema::any(),
            semantics: lash_core::ProcessEventSemanticsSpec {
                wake: Some(lash_core::ProcessWakeSpec {
                    when: None,
                    input: lash_core::ProcessValueSelector::Pointer("/text".to_string()),
                }),
                ..lash_core::ProcessEventSemanticsSpec::default()
            },
        },
    ]
}

#[expect(
    clippy::expect_used,
    reason = "lashlang process signal names are parser-validated before reaching this registration, which the message states"
)]
pub fn lashlang_process_signal_event_types(
    process: &lashlang::ProcessDecl,
) -> Vec<lash_core::ProcessEventType> {
    process
        .signals
        .iter()
        .map(|signal| lash_core::ProcessEventType {
            name: lash_core::facade_support::process_signal_event_type(signal.name.as_str())
                .expect("lashlang process signal declarations use parser-validated names"),
            payload_schema: lash_core::LashSchema::new(lashlang_type_expr_schema(&signal.ty)),
            semantics: lash_core::ProcessEventSemanticsSpec::default(),
        })
        .collect()
}
