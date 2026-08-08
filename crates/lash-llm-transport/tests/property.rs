use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use lash_llm_transport::LlmHttpBody;
use lash_llm_transport::proptest_support::{
    ScriptedByteStream, chunk_partitions, sse_data_payload, sse_stream,
};
use lash_llm_transport::streaming::drive_sse_response;
use proptest::prelude::*;

fn frame_events(body: LlmHttpBody) -> Vec<String> {
    let mut events: Vec<String> = Vec::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(drive_sse_response(
            body,
            Duration::from_secs(5),
            "test stream chunk timed out",
            &mut lash_llm_transport::ResponseMetadataCapture::default(),
            |event| {
                events.push(event.to_string());
                Ok(())
            },
        ))
        .expect("scripted stream drives without error");
    events
}

fn frame_split(chunks: Vec<Vec<u8>>) -> Vec<String> {
    frame_events(LlmHttpBody::streamed(ScriptedByteStream::new(chunks)))
}

fn frame_unsplit(bytes: Vec<u8>) -> Vec<String> {
    frame_events(LlmHttpBody::buffered(bytes))
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    #[test]
    fn framing_is_invariant_under_arbitrary_chunk_splits(
        chunks in sse_stream().prop_flat_map(chunk_partitions)
    ) {
        let unsplit: Vec<u8> = chunks.concat();
        prop_assert_eq!(frame_split(chunks), frame_unsplit(unsplit));
    }

    #[test]
    fn driver_never_panics_on_arbitrary_byte_chunks(
        chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..64), 0..8)
    ) {
        let unsplit: Vec<u8> = chunks.concat();
        let split_events = catch_unwind(AssertUnwindSafe(|| frame_split(chunks)));
        let unsplit_events = catch_unwind(AssertUnwindSafe(|| frame_unsplit(unsplit)));
        prop_assert!(split_events.is_ok(), "split parse panicked");
        prop_assert!(unsplit_events.is_ok(), "unsplit parse panicked");
        prop_assert_eq!(split_events.expect("split"), unsplit_events.expect("unsplit"));
    }

    #[test]
    fn partial_final_frame_flush_matches_between_split_and_unsplit(
        (trailing, chunks) in (sse_stream(), sse_data_payload()).prop_flat_map(
            |(stream, trailing)| {
                let mut bytes = stream;
                bytes.extend_from_slice(format!("data: {trailing}").as_bytes());
                (Just(trailing), chunk_partitions(bytes))
            }
        )
    ) {
        let unsplit: Vec<u8> = chunks.concat();
        let split_events = frame_split(chunks);
        let unsplit_events = frame_unsplit(unsplit);
        prop_assert_eq!(&split_events, &unsplit_events);
        if !trailing.trim().is_empty() {
            prop_assert_eq!(
                split_events.last().map(String::as_str),
                Some(trailing.trim()),
                "terminal flush must surface the partial final frame"
            );
        }
    }
}
