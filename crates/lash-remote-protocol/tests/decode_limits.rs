use lash_remote_protocol::{
    Envelope, JsonDecodeError, JsonDecodeLimits, Negotiated, Negotiation, REMOTE_PROTOCOL,
    REMOTE_PROTOCOL_VERSION, RemoteProtocolError, RemoteTurnInput, RemoteTurnRequest, VersionRange,
};

#[test]
fn synthetic_next_decoders_bound_structure_and_retain_the_selected_shape() {
    let input = |version| {
        format!(
            r#"{{"protocol_version":{version},"items":[{{"type":"text","text":"hello"}}],"synthetic_next_note":"note"}}"#
        )
    };
    let at_n = input(REMOTE_PROTOCOL_VERSION);
    let usage = JsonDecodeLimits::default().check(at_n.as_bytes()).unwrap();
    let exact = JsonDecodeLimits {
        max_bytes: usage.bytes,
        max_nodes: usage.nodes,
        max_depth: usage.depth,
        max_estimated_allocation_bytes: usage.estimated_allocation_bytes,
    };
    let decoded = RemoteTurnInput::decode_json_with_limits(at_n.as_bytes(), exact).unwrap();
    assert_eq!(decoded, RemoteTurnInput::text("hello"));
    for limits in [
        JsonDecodeLimits {
            max_bytes: exact.max_bytes - 1,
            ..exact
        },
        JsonDecodeLimits {
            max_nodes: exact.max_nodes - 1,
            ..exact
        },
        JsonDecodeLimits {
            max_depth: exact.max_depth - 1,
            ..exact
        },
        JsonDecodeLimits {
            max_estimated_allocation_bytes: exact.max_estimated_allocation_bytes - 1,
            ..exact
        },
    ] {
        assert!(matches!(
            RemoteTurnInput::decode_json_with_limits(at_n.as_bytes(), limits),
            Err(RemoteProtocolError::DecodeBudget(
                JsonDecodeError::LimitExceeded { .. }
            ))
        ));
    }
    let at_next = input(REMOTE_PROTOCOL.max());
    let decoded = RemoteTurnInput::decode_json(at_next.as_bytes()).unwrap();
    assert_eq!(decoded.synthetic_next_note.as_deref(), Some("note"));
    let at_n_wire = Negotiated::from_accept(
        REMOTE_PROTOCOL,
        &Negotiation::Accept {
            supported: VersionRange::exactly(REMOTE_PROTOCOL_VERSION),
            selected: REMOTE_PROTOCOL_VERSION,
        },
    )
    .unwrap();
    assert_eq!(
        RemoteTurnInput::decode_json(&decoded.encode_json(&at_n_wire).unwrap()).unwrap(),
        RemoteTurnInput::text("hello")
    );

    for version in [REMOTE_PROTOCOL_VERSION, REMOTE_PROTOCOL.max()] {
        let request = format!(
            r#"{{"protocol_version":{version},"session_id":"s","turn_id":"t","input":{{"items":[],"synthetic_next_note":"note"}}}}"#
        );
        let decoded = RemoteTurnRequest::decode_json(request.as_bytes()).unwrap();
        assert_eq!(
            decoded.input.synthetic_next_note.as_deref(),
            (version > REMOTE_PROTOCOL_VERSION).then_some("note")
        );
    }

    #[derive(Debug)]
    struct Dto;
    impl<'de> serde::Deserialize<'de> for Dto {
        fn deserialize<D: serde::Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
            Err(serde::de::Error::custom("DTO decoder was entered"))
        }
    }
    let wide = format!(
        r#"{{"protocol_version":{},"items":[{}0]}}"#,
        REMOTE_PROTOCOL.max(),
        "0,".repeat(1_000_000)
    );
    assert!(matches!(
        Envelope::<Dto>::decode_json(wide.as_bytes(), REMOTE_PROTOCOL),
        Err(RemoteProtocolError::DecodeBudget(
            JsonDecodeError::LimitExceeded {
                resource: "nodes",
                ..
            }
        ))
    ));
}
