//! Decoder parity with core's registration validator.
//!
//! `ProcessRecord::from_registration` `.expect()`s on any validation error, so
//! a peer record core would refuse used to abort the host instead of yielding a
//! typed error (FIG-2985). The corpus driving these tests lives in
//! `lash_core::runtime::refused_process_registrations`, whose match over
//! `ProcessRegistrationRefusal::ALL` is exhaustive: a new core rule without a
//! fixture fails to compile, and a fixture the remote decoder still accepts
//! fails here.

use super::*;

use lash_core::runtime::{
    ProcessRegistrationRefusal, accepted_process_registration, refused_process_registrations,
};

/// Projects a core registration onto the peer-facing record DTO.
///
/// Deliberately field-by-field rather than through
/// `RemoteProcessRecord::try_from(ProcessRecord)`: building the core record is
/// exactly the panicking step under test, so a refused registration can never
/// travel that way.
fn peer_record(
    registration: lash_core::ProcessRegistration,
) -> Result<RemoteProcessRecord, RemoteProtocolError> {
    let lash_core::ProcessRegistration {
        start_key,
        input,
        disposition,
        lifecycle,
        max_attempts,
        identity,
        event_types,
        provenance,
        env_ref,
        wake_session_id: _,
    } = registration;
    Ok(RemoteProcessRecord {
        process_id: lash_sansio::ProcessId::fixture("registration-parity"),
        start_key_digest: start_key.map(|key| key.as_str().to_string()),
        last_event_sequence: 0,
        input: input.as_ref().clone().try_into()?,
        disposition: disposition.into(),
        lifecycle: lifecycle.into(),
        max_attempts,
        identity: identity.into(),
        event_types: event_types.into_iter().map(Into::into).collect(),
        provenance: provenance.into(),
        env_ref: env_ref
            .map(|env_ref| env_ref.as_str().parse())
            .transpose()?,
        created_at_ms: 10,
        updated_at_ms: 10,
        external_ref: None,
        first_started: None,
        abandon_request: None,
        cancel_request: None,
        wait: None,
        park: None,
        status: RemoteProcessStatus::Running,
        outcome: None,
    })
}

#[test]
fn the_accepted_fixture_decodes_so_refusals_are_the_mutation_talking() {
    let record = peer_record(accepted_process_registration())
        .expect("the accepted fixture projects onto the record DTO");
    record
        .validate("RemoteProcessRecord")
        .expect("the accepted fixture passes DTO validation");
    lash_core::ProcessRecord::try_from(record).expect("the accepted fixture decodes");
}

#[test]
fn every_core_refusal_is_a_typed_remote_refusal_not_a_decoder_panic() {
    let mut refused_at_projection = 0usize;
    let mut refused_at_decode = 0usize;
    for rule in ProcessRegistrationRefusal::ALL {
        let fixtures = refused_process_registrations(*rule);
        assert!(
            !fixtures.is_empty(),
            "rule {rule:?} contributes no fixture, so the decoder is not proved against it"
        );
        for (index, registration) in fixtures.into_iter().enumerate() {
            lash_core::runtime::prepare_process_registration(registration.clone())
                .expect_err("the fixture must be refused by core, or it proves nothing here");

            // A projection failure is itself a typed refusal at the DTO
            // boundary; otherwise the record must be refused by the decoder.
            // Both outcomes are counted, so a fixture that silently stops
            // reaching the decoder shows up as a moved tally rather than as a
            // quietly skipped rule.
            match peer_record(registration) {
                Err(RemoteProtocolError::InvalidEnvelope { .. })
                | Err(RemoteProtocolError::MissingRequiredField { .. }) => {
                    refused_at_projection += 1;
                }
                Err(other) => {
                    panic!("{rule:?}/{index}: expected a typed projection refusal, got {other:?}")
                }
                Ok(record) => match lash_core::ProcessRecord::try_from(record) {
                    Err(RemoteProtocolError::InvalidEnvelope { .. })
                    | Err(RemoteProtocolError::MissingRequiredField { .. }) => {
                        refused_at_decode += 1;
                    }
                    Err(other) => {
                        panic!("{rule:?}/{index}: expected an envelope refusal, got {other:?}")
                    }
                    Ok(_) => {
                        panic!("{rule:?}/{index}: the decoder accepted a record core refuses")
                    }
                },
            }
        }
    }
    let total = refused_at_projection + refused_at_decode;
    assert_eq!(
        total,
        ProcessRegistrationRefusal::ALL
            .iter()
            .map(|rule| refused_process_registrations(*rule).len())
            .sum::<usize>(),
        "every fixture must be accounted for by exactly one refusal point"
    );
    assert!(
        refused_at_decode > 0,
        "at least one fixture must reach the decoder guard, or this test only \
         proves the DTO layer"
    );
}

#[test]
fn tool_call_input_refuses_a_blank_call_id_or_tool_name() {
    for (prepared_tool_call, field) in [
        (
            serde_json::json!({ "call_id": "  ", "tool_name": "echo" }),
            "prepared_tool_call.call_id",
        ),
        (
            serde_json::json!({ "call_id": "call-1", "tool_name": "" }),
            "prepared_tool_call.tool_name",
        ),
        (serde_json::json!({}), "prepared_tool_call.call_id"),
    ] {
        let input = RemoteProcessInput::ToolCall { prepared_tool_call };
        match input.validate("RemoteProcessInput") {
            Err(RemoteProtocolError::MissingRequiredField {
                field: refused_field,
                ..
            }) => assert_eq!(refused_field, field),
            other => panic!("expected `{field}` to be required, got {other:?}"),
        }
    }
}

/// `prepare_process_registration` normalizes event types — it fills in the core
/// set and then strips the runtime lifecycle names the runtime re-supplies — so
/// the decoder guard sits between the peer's declared list and the decoded
/// record. This pins that the guard hands back exactly what core's own
/// normalization produces, and in particular that the peer's own declaration
/// survives rather than being dropped with the runtime names.
#[test]
fn the_decoder_guard_preserves_the_peer_declared_event_types() {
    let mut registration = accepted_process_registration();
    registration.event_types.push(lash_core::ProcessEventType {
        name: "app.declared".to_string(),
        payload_schema: lash_core::LashSchema::any(),
        semantics: lash_core::ProcessEventSemanticsSpec::default(),
    });
    let normalized = lash_core::runtime::prepare_process_registration(registration.clone())
        .expect("the fixture is accepted by core");
    let expected = normalized
        .event_types
        .iter()
        .map(|event_type| event_type.name.clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        expected.contains("app.declared"),
        "core normalization must keep the peer's own declaration, or this proves nothing"
    );

    let record = peer_record(registration).expect("the fixture projects onto the record DTO");
    let decoded =
        lash_core::ProcessRecord::try_from(record).expect("the fixture decodes through the guard");

    let decoded_names = decoded
        .event_types
        .iter()
        .map(|event_type| event_type.name.clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        decoded_names, expected,
        "the decoder guard must round-trip the peer's event-type declarations"
    );
}
