use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Deserialize;

use crate::{Envelope, Negotiated, Negotiation, RemoteProtocolError, VersionRange, answer};

fn range(min: u32, max: u32) -> VersionRange {
    VersionRange::new(min, max).expect("nonempty range")
}

#[test]
fn hello_answer_selects_the_highest_common_version() {
    let hello = Negotiation::Hello {
        supported: range(1, 3),
    };
    let accept = answer(range(2, 4), &hello);
    assert_eq!(
        accept,
        Negotiation::Accept {
            supported: range(2, 4),
            selected: 3,
        }
    );
    assert_eq!(
        Negotiated::from_accept(range(1, 3), &accept)
            .expect("accepted version")
            .selected(),
        3
    );
    assert!(matches!(
        Negotiated::from_accept(
            range(1, 3),
            &Negotiation::Accept {
                supported: range(2, 4),
                selected: 2,
            },
        ),
        Err(RemoteProtocolError::InvalidEnvelope {
            type_name: "Negotiation",
            ..
        })
    ));
}

static BODY_DECODES: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct CountedBody;

impl<'de> Deserialize<'de> for CountedBody {
    fn deserialize<D: serde::Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        BODY_DECODES.fetch_add(1, Ordering::SeqCst);
        Ok(Self)
    }
}

#[test]
fn disjoint_ranges_answer_unsupported_before_any_decode() {
    let local = range(2, 3);
    let peer = range(4, 5);
    let refusal = answer(local, &Negotiation::Hello { supported: peer });
    assert_eq!(refusal, Negotiation::Unsupported { local, peer });
    assert!(matches!(
        Negotiated::from_accept(peer, &refusal),
        Err(RemoteProtocolError::Unsupported { local: found_local, peer: found_peer })
            if found_local == peer && found_peer == local
    ));

    BODY_DECODES.store(0, Ordering::SeqCst);
    let wire = br#"{"protocol_version":4,"body":{"unknown":"shape"}}"#;
    assert!(matches!(
        Envelope::<CountedBody>::decode_json(wire, local),
        Err(RemoteProtocolError::Unsupported { local: found_local, peer: found_peer })
            if found_local == local && found_peer == VersionRange::exactly(4)
    ));
    assert_eq!(BODY_DECODES.load(Ordering::SeqCst), 0);
}

#[test]
fn each_request_is_validated_against_the_local_range() {
    let local = range(2, 3);
    let accepted = br#"{"protocol_version":2,"body":"ok"}"#;
    let request = Envelope::<serde_json::Value>::decode_json(accepted, local)
        .expect("first request accepted");
    assert_eq!(request.protocol_version(), 2);

    let refused = br#"{"protocol_version":4,"body":{"unexpected":true}}"#;
    assert!(matches!(
        Envelope::<serde_json::Value>::decode_json(refused, local),
        Err(RemoteProtocolError::Unsupported { local: found_local, peer })
            if found_local == local && peer == VersionRange::exactly(4)
    ));
    assert!(matches!(
        Envelope::<serde_json::Value>::decode_json(
            br#"{"protocol_version":0,"body":"invalid"}"#,
            local,
        ),
        Err(RemoteProtocolError::InvalidEnvelope {
            type_name: "Envelope",
            ..
        })
    ));
}

#[test]
fn replies_errors_and_streams_use_the_request_version() {
    let local = range(1, 2);
    let accept = answer(
        local,
        &Negotiation::Hello {
            supported: range(1, 1),
        },
    );
    let negotiated = Negotiated::from_accept(range(1, 1), &accept).expect("accepted");
    let request = Envelope::at(&negotiated, serde_json::json!({"request": true}));
    assert_eq!(request.protocol_version(), 1);

    for body in ["reply", "error", "stream"] {
        let response = Envelope::reply_to(&request, serde_json::json!({"kind": body}));
        assert_eq!(response.protocol_version(), request.protocol_version());
        let encoded = response.encode_json().expect("encodes");
        assert_eq!(
            Envelope::<serde_json::Value>::decode_json(&encoded, local)
                .expect("decodes")
                .protocol_version(),
            1
        );
    }
}
