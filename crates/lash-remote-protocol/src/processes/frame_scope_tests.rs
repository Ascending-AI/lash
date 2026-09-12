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

#[cfg(feature = "core-conversions")]
#[test]
fn core_conversions_return_errors_for_empty_agent_frame_ids() {
    let scope = super::RemoteSessionScope {
        session_id: "session".into(),
        agent_frame_id: Some(String::new()),
    };
    assert!(lash_core::SessionScope::try_from(scope).is_err());

    let originator = RemoteProcessOriginator::Session {
        session_id: "session".into(),
        agent_frame_id: Some(String::new()),
    };
    assert!(lash_core::ProcessOriginator::try_from(originator).is_err());
}
