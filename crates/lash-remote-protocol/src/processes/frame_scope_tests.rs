use super::RemoteProcessOriginator;
use crate::RemoteProtocolError;

#[test]
fn session_originator_rejects_an_empty_agent_frame_id() {
    let originator = RemoteProcessOriginator::Session {
        session_id: "session".into(),
        agent_frame_id: Some(String::new()),
    };

    assert!(matches!(
        originator.validate("RemoteProcessOriginator"),
        Err(RemoteProtocolError::MissingRequiredField {
            field: "agent_frame_id",
            ..
        })
    ));
}
