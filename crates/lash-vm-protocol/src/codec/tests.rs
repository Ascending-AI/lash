use super::*;
use crate::message::{
    EffectKind, EffectRequest, EffectRequestId, EncodedPayload, ExecutionLease, FrameEpoch,
    MessageFence, OwnerEpoch, WorkerMessage,
};

fn codec() -> FrameCodec {
    FrameCodec::new(DecodeLimits::standard())
}

fn fence() -> MessageFence {
    MessageFence::new(ExecutionLease(7), OwnerEpoch(3), FrameEpoch(2))
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
fn frame_around(_codec: &FrameCodec, payload: &[u8]) -> Vec<u8> {
    let mut bytes = FRAME_MAGIC.to_vec();
    bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
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
    let codec = FrameCodec::new(limits);

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
    let codec = FrameCodec::new(many);
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
        FrameCodec::new(DecodeLimits {
            max_frame_bytes: limit,
            ..DecodeLimits::standard()
        })
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
    std::io::Write::write_all(&mut writer, &[0; 1024]).unwrap();
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
    std::io::Write::write_all(&mut writer, &[0; 3]).unwrap();
    assert_eq!(writer.refused, Some(13));
    std::io::Write::write_all(&mut writer, &[0; 4]).unwrap();
    assert_eq!(writer.refused, Some(17));
    assert_eq!(writer.bytes.len(), 10);
}

/// An observation-shaped value: a map holding a string and a small array.
fn observation(index: u64) -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct Observed {
        site: String,
        path: Vec<u64>,
        occurrence: u64,
    }
    rmp_serde::to_vec_named(&Observed {
        site: format!("process:main/{}", index % 7),
        path: vec![0, index % 3, 1],
        occurrence: index,
    })
    .expect("an observation encodes")
}

/// However many observations a step makes, each payload crosses in one
/// frame within the codec's bounds and passes the bounds its receiver
/// checks it against; together the payloads hold every observation, in
/// order, and weigh what the chunker says (FIG-4458).
#[test]
fn observation_chunks_each_cross_in_one_frame_within_the_bounds() {
    for limits in [
        DecodeLimits::standard(),
        DecodeLimits {
            max_frame_bytes: 2048,
            max_depth: 4,
            max_nodes: 200,
            max_allocation_bytes: 8192,
        },
        DecodeLimits {
            max_frame_bytes: 1 << 20,
            max_depth: 128,
            max_nodes: 100_000,
            max_allocation_bytes: 16 * 1024,
        },
    ] {
        let codec = FrameCodec::new(limits);
        let observations = (0..40_000).map(observation).collect::<Vec<_>>();
        let mut chunker = codec.observation_chunker().expect("a chunker");
        for observation in &observations {
            chunker
                .push(observation)
                .expect("an observation fits a frame");
        }
        let bytes = chunker.bytes();
        let chunks = chunker.finish();
        assert!(chunks.len() > 1, "{limits:?}: the stream is chunked");
        let mut fence = MessageFence::new(
            ExecutionLease(u64::MAX),
            OwnerEpoch(u64::MAX),
            FrameEpoch(u64::MAX),
        );
        let mut received = Vec::new();
        for chunk in &chunks {
            codec
                .check_payload(&chunk.0)
                .unwrap_or_else(|refusal| panic!("{limits:?}: a chunk is refused: {refusal}"));
            let frame = codec
                .encode_worker(&WorkerFrame {
                    header: fence.next_header(),
                    message: WorkerMessage::Observations {
                        payload: chunk.clone(),
                    },
                })
                .unwrap_or_else(|refusal| panic!("{limits:?}: a frame is refused: {refusal}"));
            assert!(frame.len() <= limits.max_frame_bytes as usize);
            codec
                .decode_worker(&frame)
                .unwrap_or_else(|refusal| panic!("{limits:?}: a frame is refused: {refusal}"));
            let values: Vec<serde_json::Value> = rmp_serde::from_slice(&chunk.0).expect("an array");
            received.extend(values);
        }
        let sent = observations
            .iter()
            .map(|observation| {
                rmp_serde::from_slice::<serde_json::Value>(observation).expect("an observation")
            })
            .collect::<Vec<_>>();
        assert_eq!(received, sent, "{limits:?}: every observation, in order");
        assert_eq!(
            bytes,
            chunks.iter().map(|chunk| chunk.0.len() as u64).sum::<u64>(),
            "{limits:?}: the chunker weighs what crosses"
        );
    }
}

/// An observation that alone outgrows one frame is refused with the bound
/// it outgrows, and the observations before it keep their chunks.
#[test]
fn an_observation_no_frame_carries_is_refused_with_its_bound() {
    let limits = DecodeLimits {
        max_frame_bytes: 2048,
        max_depth: 4,
        max_nodes: 200,
        max_allocation_bytes: 64 * 1024,
    };
    let codec = FrameCodec::new(limits);
    let mut chunker = codec.observation_chunker().expect("a chunker");
    chunker.push(&observation(1)).expect("a small observation");

    let long = rmp_serde::to_vec_named(&"x".repeat(4096)).expect("a string");
    assert!(matches!(
        chunker.push(&long),
        Err(CodecRefusal::FrameTooLarge { limit: 2048, .. })
    ));
    let wide = rmp_serde::to_vec_named(&vec![0_u8; 200]).expect("an array");
    assert_eq!(
        chunker.push(&wide),
        Err(CodecRefusal::NodeLimitExceeded { limit: 200 })
    );
    let deep = rmp_serde::to_vec_named(&vec![vec![vec![vec![0_u8]]]]).expect("nested arrays");
    assert_eq!(
        chunker.push(&deep),
        Err(CodecRefusal::DepthExceeded { limit: 4 })
    );
    assert_eq!(chunker.finish().len(), 1);
}
