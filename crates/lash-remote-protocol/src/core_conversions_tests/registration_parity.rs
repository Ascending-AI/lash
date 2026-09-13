//! Decoder parity with core's registration validator.
//!
//! `ProcessRecord::from_registration` `.expect()`s on any validation error, so
//! a peer record core would refuse used to abort the host instead of yielding a
//! typed error (FIG-2985). The corpus driving these tests lives in
//! `lash_core::runtime::refused_process_registration`, whose match over
//! `ProcessRegistrationRefusal::ALL` is exhaustive: a new core rule without a
//! fixture fails to compile, and a fixture the remote decoder still accepts
//! fails here.

use super::*;

use lash_core::runtime::{
    ProcessRegistrationRefusal, accepted_process_registration, refused_process_registration,
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
        id,
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
        process_id: id,
        incarnation: 1,
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
    for rule in ProcessRegistrationRefusal::ALL {
        let registration = refused_process_registration(*rule);
        lash_core::runtime::prepare_process_registration(registration.clone())
            .expect_err("the fixture must be refused by core, or it proves nothing here");

        // A projection failure is itself a typed refusal at the DTO boundary;
        // otherwise the record must be refused by the decoder.
        let Ok(record) = peer_record(registration) else {
            continue;
        };
        match lash_core::ProcessRecord::try_from(record) {
            Err(RemoteProtocolError::InvalidEnvelope { .. })
            | Err(RemoteProtocolError::MissingRequiredField { .. }) => {}
            Err(other) => panic!("{rule:?}: expected an envelope refusal, got {other:?}"),
            Ok(_) => panic!("{rule:?}: the remote decoder accepted a record core refuses"),
        }
    }
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
