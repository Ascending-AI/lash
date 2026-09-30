use crate::{Envelope, REMOTE_PROTOCOL};

#[test]
fn wide_envelope_refuses_before_dto_decode() {
    #[derive(Debug)]
    struct Dto;
    impl<'de> serde::Deserialize<'de> for Dto {
        fn deserialize<D: serde::Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
            Err(serde::de::Error::custom("DTO decoder was entered"))
        }
    }
    let body = "0,".repeat(1_000_000) + "0";
    let bytes = format!(
        r#"{{"protocol_version":{},"items":[{body}]}}"#,
        REMOTE_PROTOCOL.max()
    );
    let error = Envelope::<Dto>::decode_json(bytes.as_bytes(), REMOTE_PROTOCOL)
        .expect_err("wide envelope refuses");
    assert!(
        error.to_string().contains("JSON decode nodes limit"),
        "{error}"
    );
}

#[test]
fn configured_envelopes_and_turn_inputs_accept_exact_limits() {
    use crate::{
        JsonDecodeError, JsonDecodeLimits, RemoteProtocolError, RemoteTurnInput, RemoteTurnRequest,
    };
    let input = format!(
        r#"{{"protocol_version":{},"items":[{{"type":"text","text":"hello"}}]}}"#,
        REMOTE_PROTOCOL.max()
    );
    let usage = JsonDecodeLimits::default().check(input.as_bytes()).unwrap();
    let limits = JsonDecodeLimits {
        max_bytes: usage.bytes,
        max_nodes: usage.nodes,
        max_depth: usage.depth,
        max_estimated_allocation_bytes: usage.estimated_allocation_bytes,
    };
    assert_eq!(
        RemoteTurnInput::decode_json_with_limits(input.as_bytes(), limits).unwrap(),
        RemoteTurnInput::text("hello")
    );
    for tight in [
        JsonDecodeLimits {
            max_bytes: limits.max_bytes - 1,
            ..limits
        },
        JsonDecodeLimits {
            max_nodes: limits.max_nodes - 1,
            ..limits
        },
        JsonDecodeLimits {
            max_depth: limits.max_depth - 1,
            ..limits
        },
        JsonDecodeLimits {
            max_estimated_allocation_bytes: limits.max_estimated_allocation_bytes - 1,
            ..limits
        },
    ] {
        assert!(matches!(
            RemoteTurnInput::decode_json_with_limits(input.as_bytes(), tight),
            Err(RemoteProtocolError::DecodeBudget(
                JsonDecodeError::LimitExceeded { .. }
            ))
        ));
    }
    let request = format!(
        r#"{{"protocol_version":{},"session_id":"s","turn_id":"t","input":{{"items":[]}}}}"#,
        REMOTE_PROTOCOL.max()
    );
    assert!(
        RemoteTurnRequest::decode_json_with_limits(request.as_bytes(), JsonDecodeLimits::default())
            .is_ok()
    );
    assert!(
        RemoteTurnRequest::decode_json_with_limits(
            request.as_bytes(),
            JsonDecodeLimits {
                max_nodes: 1,
                ..JsonDecodeLimits::default()
            }
        )
        .is_err()
    );
}
