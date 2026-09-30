use super::*;
use crate::message::{
    EffectKind, EffectOutcome, EffectRequest, EffectRequestId, EffectResponse, EncodedPayload,
    ExecutionLease, FrameEpoch, MessageFence, OwnerEpoch, ParentMessage, ProgramSource, Start,
    StartState, VmLimits, WorkerMessage,
};
use crate::state::{OpaqueVmState, VmOwner, VmStateKind};

fn codec() -> FrameCodec {
    FrameCodec::new(
        BuildIdentity::new("lash test build 1"),
        DecodeLimits::standard(),
    )
}

fn fence() -> MessageFence {
    MessageFence::new(ExecutionLease(7), OwnerEpoch(3), FrameEpoch(2))
}

fn start_frame() -> ParentFrame {
    ParentFrame {
        header: fence().next_header(),
        message: ParentMessage::Start(Box::new(Start {
            owner: VmOwner::new("session-a"),
            program: ProgramSource::Source {
                dialect: "typescript".to_string(),
                text: "finish(1 + 1);".to_string(),
            },
            contexts: Vec::new(),
            state: StartState::Continuation(OpaqueVmState::seal(
                VmStateKind::Continuation,
                VmOwner::new("session-a"),
                "vm-contract",
                29,
                vec![1, 2, 3, 4],
            )),
            limits: VmLimits {
                instruction_budget: Some(1_000),
                memory_limit_bytes: None,
                max_frame_depth: 64,
            },
        })),
    }
}

fn request_frame() -> WorkerFrame {
    WorkerFrame {
        header: fence().next_header(),
        message: WorkerMessage::EffectRequest(EffectRequest {
            id: EffectRequestId(0),
            kind: EffectKind::ResourceOperation,
            payload: EncodedPayload(b"{\"op\":\"echo\"}".to_vec()),
        }),
    }
}

/// A frame header around an arbitrary payload, for hostile payloads.
fn frame_around(codec: &FrameCodec, payload: &[u8]) -> Vec<u8> {
    let mut bytes = FRAME_MAGIC.to_vec();
    bytes.extend_from_slice(&codec.build().digest());
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

#[test]
fn every_message_round_trips() {
    let codec = codec();
    let start = start_frame();
    assert_eq!(
        codec.decode_parent(&codec.encode_parent(&start).unwrap()),
        Ok(start)
    );
    let parents = [
        ParentMessage::EffectResponse(EffectResponse {
            id: EffectRequestId(4),
            outcome: EffectOutcome::Checkpoint { cancelled: true },
        }),
        ParentMessage::Park,
        ParentMessage::Cancel,
        ParentMessage::Reset,
        ParentMessage::Shutdown,
    ];
    for message in parents {
        let frame = ParentFrame {
            header: fence().next_header(),
            message,
        };
        assert_eq!(
            codec.decode_parent(&codec.encode_parent(&frame).unwrap()),
            Ok(frame)
        );
    }
    let state = OpaqueVmState::seal(
        VmStateKind::Snapshot,
        VmOwner::new("session-a"),
        "vm-contract",
        1,
        vec![9; 16],
    );
    let workers = [
        WorkerMessage::Ready {
            build: BuildIdentity::new("lash test build 1"),
        },
        request_frame().message,
        WorkerMessage::Progress {
            phase: crate::WorkerPhase::Computing,
            cpu_nanos: 10,
        },
        WorkerMessage::LimitExceeded {
            limit: crate::WorkerLimit::Fuel,
        },
        WorkerMessage::PayloadTooLarge {
            limit: 10,
            size: 11,
        },
        WorkerMessage::Suspended {
            state: state.clone(),
        },
        WorkerMessage::Complete {
            state: state.clone(),
            value: EncodedPayload(vec![0xc3]),
        },
        WorkerMessage::GuestError {
            state: None,
            error: EncodedPayload(b"TypeError".to_vec()),
        },
        WorkerMessage::Cancelled,
        WorkerMessage::ResetDone,
    ];
    for message in workers {
        let frame = WorkerFrame {
            header: fence().next_header(),
            message,
        };
        assert_eq!(
            codec.decode_worker(&codec.encode_worker(&frame).unwrap()),
            Ok(frame)
        );
    }
}

#[test]
fn a_frame_of_another_build_is_refused() {
    let other = FrameCodec::new(
        BuildIdentity::new("lash test build 2"),
        DecodeLimits::standard(),
    );
    let bytes = other.encode_worker(&request_frame()).unwrap();
    assert!(matches!(
        codec().decode_worker(&bytes),
        Err(CodecRefusal::WrongBuild { .. })
    ));
}

#[test]
fn a_truncated_frame_is_refused_at_every_cut() {
    let codec = codec();
    let bytes = codec.encode_worker(&request_frame()).unwrap();
    for cut in 0..bytes.len() {
        assert!(
            matches!(
                codec.decode_worker(&bytes[..cut]),
                Err(CodecRefusal::Truncated { .. })
            ),
            "cut at {cut}"
        );
    }
}

#[test]
fn an_oversized_declaration_is_refused_from_the_header_alone() {
    let codec = codec();
    let mut bytes = FRAME_MAGIC.to_vec();
    bytes.extend_from_slice(&codec.build().digest());
    bytes.extend_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(
        codec.decode_worker(&bytes),
        Err(CodecRefusal::FrameTooLarge {
            limit: u64::from(DecodeLimits::standard().max_frame_bytes),
            declared: u64::from(u32::MAX) + FRAME_HEADER_BYTES as u64,
        })
    );
    let mut reader = FrameReader::new(codec.clone());
    assert!(matches!(
        reader.push(&bytes),
        Err(CodecRefusal::FrameTooLarge { .. })
    ));
}

#[test]
fn an_oversized_frame_is_refused_when_encoded() {
    let codec = FrameCodec::new(
        BuildIdentity::new("lash test build 1"),
        DecodeLimits {
            max_frame_bytes: 64,
            ..DecodeLimits::standard()
        },
    );
    let mut frame = request_frame();
    frame.message = WorkerMessage::EffectRequest(EffectRequest {
        id: EffectRequestId(0),
        kind: EffectKind::Print,
        payload: EncodedPayload(vec![0; 1024]),
    });
    assert!(matches!(
        codec.encode_worker(&frame),
        Err(CodecRefusal::FrameTooLarge { .. })
    ));
}

#[test]
fn malformed_payloads_are_refused() {
    let codec = codec();
    let cases: [(&str, Vec<u8>); 5] = [
        ("not a frame message", vec![0xc3]),
        ("unused marker", vec![0xc1]),
        ("extension marker", vec![0xd4, 0x01, 0x00]),
        (
            "string past the end",
            vec![0xdb, 0x00, 0x00, 0x10, 0x00, b'a'],
        ),
        ("two values", vec![0xc3, 0xc3]),
    ];
    for (name, payload) in cases {
        let refusal = codec.decode_worker(&frame_around(&codec, &payload));
        assert!(
            matches!(
                refusal,
                Err(CodecRefusal::Malformed { .. } | CodecRefusal::TrailingBytes { .. })
            ),
            "{name}: {refusal:?}"
        );
    }
    let mut bad_magic = codec.encode_worker(&request_frame()).unwrap();
    bad_magic[0] = b'X';
    assert_eq!(codec.decode_worker(&bad_magic), Err(CodecRefusal::BadMagic));
}

#[test]
fn nesting_value_counts_and_declared_allocation_are_bounded() {
    let limits = DecodeLimits {
        max_frame_bytes: 1 << 20,
        max_depth: 8,
        max_nodes: 64,
        max_allocation_bytes: 4096,
    };
    let codec = FrameCodec::new(BuildIdentity::new("lash test build 1"), limits);

    let deep = vec![0x91; 32].into_iter().chain([0xc0]).collect::<Vec<_>>();
    assert_eq!(
        codec.decode_worker(&frame_around(&codec, &deep)),
        Err(CodecRefusal::DepthExceeded { limit: 8 })
    );

    let mut wide = vec![0xdc, 0x00, 0x80];
    wide.extend(std::iter::repeat_n(0xc0, 128));
    assert_eq!(
        codec.decode_worker(&frame_around(&codec, &wide)),
        Err(CodecRefusal::AllocationExceeded {
            limit: 4096,
            requested: 128 * 128,
        })
    );

    let many = DecodeLimits {
        max_allocation_bytes: u64::MAX,
        ..limits
    };
    let codec = FrameCodec::new(BuildIdentity::new("lash test build 1"), many);
    assert_eq!(
        codec.decode_worker(&frame_around(&codec, &wide)),
        Err(CodecRefusal::NodeLimitExceeded { limit: 64 })
    );
}

#[test]
fn the_standard_preset_charges_map_keys_as_nodes() {
    let codec = codec();
    let limit = DecodeLimits::standard().max_nodes;
    assert_eq!(limit, 100_000);

    // An array of `limit - 1` nils is `limit` values: inside the budget, so
    // it fails as a message, not on the node bound.
    let mut at_limit = vec![0xdd];
    at_limit.extend_from_slice(&((limit - 1) as u32).to_be_bytes());
    at_limit.extend(std::iter::repeat_n(0xc0, (limit - 1) as usize));
    assert!(matches!(
        codec.decode_worker(&frame_around(&codec, &at_limit)),
        Err(CodecRefusal::Malformed { .. })
    ));

    // A map of `limit / 2` entries is one map, `limit / 2` keys and as many
    // values: its keys push it past the budget.
    let entries = limit / 2;
    let mut map = vec![0xdf];
    map.extend_from_slice(&(entries as u32).to_be_bytes());
    map.extend(std::iter::repeat_n(0xc0, (entries * 2) as usize));
    assert_eq!(
        codec.decode_worker(&frame_around(&codec, &map)),
        Err(CodecRefusal::NodeLimitExceeded { limit })
    );
}

#[test]
fn the_standard_bounds_are_the_measured_presets() {
    let bounds = crate::ProtocolBounds::standard();
    assert_eq!(bounds.decode, DecodeLimits::standard());
    assert_eq!(bounds.decode.max_frame_bytes, 4 * 1024 * 1024);
    assert_eq!(bounds.decode.max_allocation_bytes, 64 * 1024 * 1024);
    assert_eq!(bounds.max_vm_state_bytes, 2 * 1024 * 1024);
    assert_eq!(bounds.max_effect_value_bytes, 1024 * 1024);
    assert_eq!(bounds.max_source_bytes, 64 * 1024);
    assert_eq!(
        bounds.no_response_watchdog,
        std::time::Duration::from_secs(5)
    );
    // A maximal VM state fits a frame with room for its envelope.
    assert!(bounds.max_vm_state_bytes * 2 <= u64::from(bounds.decode.max_frame_bytes));
}

#[test]
fn a_stream_yields_whole_frames_and_refuses_a_partial_tail() {
    let codec = codec();
    let first = request_frame();
    let mut second = request_frame();
    second.header.sequence = crate::message::TransportSequence(1);
    let mut stream = codec.encode_worker(&first).unwrap();
    stream.extend(codec.encode_worker(&second).unwrap());
    let partial = codec.encode_worker(&first).unwrap();
    stream.extend_from_slice(&partial[..partial.len() - 1]);

    let mut reader = FrameReader::new(codec);
    for chunk in stream.chunks(7) {
        reader.push(chunk).unwrap();
    }
    assert_eq!(reader.next_worker(), Ok(Some(first)));
    assert_eq!(reader.next_worker(), Ok(Some(second)));
    assert_eq!(reader.next_worker(), Ok(None));
    assert!(matches!(
        reader.finish(),
        Err(CodecRefusal::Truncated { .. })
    ));
}

#[test]
fn the_fence_admits_only_the_current_lease_epochs_and_next_sequence() {
    use crate::message::{HeaderRefusal, TransportSequence};
    let mut sender = fence();
    let mut receiver = fence();
    let first = sender.next_header();
    let second = sender.next_header();
    assert_eq!(
        receiver.admit(&second),
        Err(HeaderRefusal::OutOfSequence {
            expected: TransportSequence(0),
            found: TransportSequence(1),
        })
    );
    assert_eq!(receiver.admit(&first), Ok(()));
    assert!(matches!(
        receiver.admit(&first),
        Err(HeaderRefusal::OutOfSequence { .. })
    ));
    assert_eq!(receiver.admit(&second), Ok(()));

    let mut stale = sender.next_header();
    stale.lease = ExecutionLease(6);
    assert!(matches!(
        receiver.admit(&stale),
        Err(HeaderRefusal::StaleLease { .. })
    ));
    receiver.open_frame(FrameEpoch(3));
    let old_frame = sender.next_header();
    assert!(matches!(
        receiver.admit(&old_frame),
        Err(HeaderRefusal::StaleFrameEpoch { .. })
    ));
}

#[test]
fn envelope_and_encoding_allocation_fit_the_frame_bound() {
    let frame = request_frame();
    let bytes = codec().encode_worker(&frame).unwrap();
    let bounded = |limit| {
        FrameCodec::new(
            codec().build().clone(),
            DecodeLimits {
                max_frame_bytes: limit,
                ..DecodeLimits::standard()
            },
        )
    };
    assert_eq!(
        bounded(bytes.len() as u32).encode_worker(&frame).unwrap(),
        bytes
    );
    assert!(matches!(
        bounded(bytes.len() as u32 - 1).encode_worker(&frame),
        Err(CodecRefusal::FrameTooLarge { .. })
    ));
    assert!(matches!(
        bounded(bytes.len() as u32 - 1).frame_len(&bytes[..FRAME_HEADER_BYTES]),
        Err(CodecRefusal::FrameTooLarge { .. })
    ));
    let mut writer = CappedWriter {
        bytes: Vec::new(),
        limit: 8,
        refused: None,
    };
    assert!(std::io::Write::write_all(&mut writer, &[0; 1024]).is_err());
    assert!(writer.bytes.is_empty());
    assert_eq!(writer.refused, Some(1024));
}

#[test]
fn bounded_encoder_capacity_does_not_double_past_the_cap() {
    let mut writer = CappedWriter {
        bytes: Vec::new(),
        limit: 10,
        refused: None,
    };
    std::io::Write::write_all(&mut writer, &[0; 7]).unwrap();
    std::io::Write::write_all(&mut writer, &[0; 3]).unwrap();
    assert_eq!(writer.bytes.len(), 10);
    assert!(writer.bytes.capacity() <= 10);
    assert!(std::io::Write::write_all(&mut writer, &[0]).is_err());
    assert_eq!(writer.bytes.len(), 10);
}
