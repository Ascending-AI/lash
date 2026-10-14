//! TOOL-LIVE-WIRE: subscription items and filters cross a host's transport
//! as tagged JSON and read back exactly (figments feedback-14 a3).
use super::*;

fn process() -> crate::ProcessId {
    crate::ProcessId::fixture("wire-process")
}
fn owner() -> ExecutionOwner {
    ExecutionOwner::Process {
        process_id: process(),
    }
}
fn address() -> CallAddress {
    CallAddress {
        owner: owner(),
        call_id: ToolCallId::fixture("wire-call"),
        attempt: 2,
    }
}
fn cursor(position: u64) -> ToolLiveCursor {
    ToolLiveCursor {
        owner: owner(),
        incarnation: "wire-incarnation".into(),
        position,
    }
}
fn id(ordinal: u64) -> ToolLiveEmissionId {
    ToolLiveEmissionId {
        producer_epoch: "wire-epoch".into(),
        ordinal,
    }
}

#[test]
fn every_item_and_filter_round_trips_as_tagged_json() {
    let items = [
        ToolLiveItem::Emission {
            cursor: cursor(1),
            emission: ToolLiveEmission {
                address: address(),
                id: id(1),
                payload: ToolLivePayload::Fact {
                    fact: ToolLiveFact::Progress {
                        progress: 0.5,
                        total: Some(1.0),
                        message: Some("half".into()),
                    },
                },
            },
        },
        ToolLiveItem::Emission {
            cursor: cursor(2),
            emission: ToolLiveEmission {
                address: address(),
                id: id(2),
                payload: ToolLivePayload::Omitted {
                    cause: ToolLiveOmission::NonFinite,
                    count: 3,
                },
            },
        },
        ToolLiveItem::OwnerOmitted {
            cursor: cursor(3),
            omission: ToolLiveOwnerOmission {
                owner: owner(),
                id: id(3),
                cause: ToolLiveOmission::RoutesFull,
                count: 1,
            },
        },
        ToolLiveItem::Fault {
            cursor: cursor(4),
            fault: ToolLiveReadFault::CorruptPayload {
                detail: "expected f64".into(),
            },
        },
        ToolLiveItem::Gap {
            cursor: cursor(5),
            reason: ToolLiveGapReason::Trimmed,
        },
    ];
    let tags = ["emission", "emission", "owner_omitted", "fault", "gap"];
    for (item, tag) in items.into_iter().zip(tags) {
        let wire = serde_json::to_value(&item).unwrap();
        assert_eq!(wire["kind"], tag);
        assert_eq!(serde_json::from_value::<ToolLiveItem>(wire).unwrap(), item);
    }
    for (filter, tag) in [
        (ToolLiveFilter::Owner(owner()), "owner"),
        (ToolLiveFilter::Call(address()), "call"),
    ] {
        let wire = serde_json::to_value(&filter).unwrap();
        assert_eq!(wire["kind"], tag);
        assert_eq!(
            serde_json::from_value::<ToolLiveFilter>(wire).unwrap(),
            filter
        );
    }
    assert_eq!(
        serde_json::to_value(ToolLiveFilter::Call(address())).unwrap(),
        serde_json::json!({
            "kind": "call",
            "owner": {"process": {"process_id": process()}},
            "call_id": address().call_id,
            "attempt": 2,
        })
    );
}
