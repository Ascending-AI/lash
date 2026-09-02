use lash::remote::turn_input::RemoteTurnInput;
use lash::remote::{Envelope, REMOTE_PROTOCOL_VERSION};

#[test]
fn shared_envelope_stamps_once_and_round_trips() {
    let input = RemoteTurnInput::text("hello");
    let wire = input.encode_json().expect("encode");
    let value: serde_json::Value = serde_json::from_slice(&wire).expect("JSON");
    assert_eq!(value["protocol_version"], REMOTE_PROTOCOL_VERSION);
    let decoded = RemoteTurnInput::decode_json(&wire).expect("decode");
    assert_eq!(decoded, input);

    let envelope = Envelope::new(decoded);
    assert_eq!(envelope.protocol_version(), REMOTE_PROTOCOL_VERSION);
    let wire = envelope.encode_json().expect("encode envelope");
    let decoded = Envelope::<RemoteTurnInput>::decode_json(&wire).expect("decode envelope");
    assert_eq!(decoded.body, input);
    assert_eq!(decoded.into_body(), input);
}
