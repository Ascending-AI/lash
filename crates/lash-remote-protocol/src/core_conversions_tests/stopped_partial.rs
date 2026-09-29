//! The stopped-partial activities cross the remote observation stream
//! (ADR 0114 §2.2, §5.2).

use lash_sansio::SessionId;
use lash_sansio::TurnId;

use super::*;

#[test]
fn remote_activity_carries_tool_progress_and_the_stopped_partial_announcement() {
    let partial = lash_sansio::StoppedPartial::seal(
        lash_sansio::StoppedPartialId {
            session_id: SessionId::from("session"),
            root: TurnId::from("root"),
            turn_id: TurnId::from("turn"),
            base: lash_sansio::CaptureBase(0),
            sealed_through: 2,
        },
        lash_sansio::StopReason::UserCancel,
        false,
        lash_sansio::CaptureCoverage::Complete,
        vec![lash_sansio::PartialItem::Text {
            id: lash_sansio::PartialItemId::new("llm", 1, lash_sansio::PartialItemKey::Text("b0")),
            state: lash_sansio::CutState::Interrupted,
            text: "half an answ".to_string(),
        }],
    )
    .expect("seal partial");
    let chunk = lash_sansio::ToolOutputChunk {
        text: "step 1 of 2".to_string(),
    };
    let negotiated = crate::negotiation::test_negotiated();
    for (sequence, event, expected) in [
        (
            6,
            lash_core::TurnEvent::ToolOutputProgress {
                call_id: "call-1".to_string(),
                chunk: chunk.clone(),
            },
            RemoteTurnEvent::ToolOutputProgress {
                call_id: "call-1".to_string(),
                chunk,
            },
        ),
        (
            7,
            lash_core::TurnEvent::StoppedPartialAvailable {
                summary: partial.summary(),
            },
            RemoteTurnEvent::StoppedPartialAvailable {
                summary: partial.summary(),
            },
        ),
    ] {
        let remote =
            RemoteTurnActivity::from_core(sequence, lash_core::TurnActivity::independent(event))
                .expect("the activity has a remote form");
        remote.validate().expect("the remote activity validates");
        assert_eq!(remote.event, expected);
        let wire = remote.encode_json(&negotiated).expect("encode activity");
        assert_eq!(
            RemoteTurnActivity::decode_json(&wire).expect("decode activity"),
            remote
        );
    }

    let blank = RemoteTurnActivity {
        sequence: 8,
        id: "activity-8".to_string(),
        correlation_id: "correlation-8".to_string(),
        event: RemoteTurnEvent::ToolOutputProgress {
            call_id: " ".to_string(),
            chunk: lash_sansio::ToolOutputChunk {
                text: "chunk".to_string(),
            },
        },
    };
    assert!(blank.validate().is_err(), "a progress chunk names its call");
}
