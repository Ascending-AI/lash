use super::*;
use lash_core::ProviderFailureKind;
use lash_core::llm::transport::TransportRetryVerdict;

const BLOCK_LIMIT: usize = 1024;

fn start(index: u64) -> String {
    json!({"type": "content_block_start", "index": index, "content_block": {"type": "text"}})
        .to_string()
}

fn refuse_start(index: u64, state: &mut StreamState) {
    let length = state.blocks.len();
    let capacity = state.blocks.capacity();
    let error = AnthropicProvider::process_sse_event(&start(index), state, None, false)
        .expect_err("an invalid start must refuse before allocating block slots");
    assert_eq!(error.kind, ProviderFailureKind::Stream);
    assert_eq!(error.retry_verdict, TransportRetryVerdict::NotRetryable);
    assert_eq!(state.blocks.len(), length);
    assert_eq!(state.blocks.capacity(), capacity);
}

#[test]
fn last_dense_index_completes_at_block_limit() {
    let mut state = StreamState::default();
    for index in 0..BLOCK_LIMIT {
        AnthropicProvider::process_sse_event(&start(index as u64), &mut state, None, false)
            .expect("each dense index below the block count bound is valid");
    }
    assert_eq!(state.blocks.len(), BLOCK_LIMIT);
    assert!(state.blocks.capacity() <= BLOCK_LIMIT);
    refuse_start(BLOCK_LIMIT as u64, &mut state);
}

#[test]
fn boundary_plus_one_refuses_without_allocating_holes() {
    refuse_start((BLOCK_LIMIT + 1) as u64, &mut StreamState::default());
}

// The old parser can abort on allocation failure. Keep that abort in a
// subprocess with a 512 MiB address-space ceiling and no core dump.
#[expect(
    clippy::disallowed_methods,
    reason = "the allocation-abort witness must isolate the parser in a memory-limited child process"
)]
fn memory_limited_index_test(name: &str, index: u64) {
    const CHILD: &str = "LASH_STREAM_INDEX_CHILD";
    if std::env::var_os(CHILD).is_some() {
        refuse_start(index, &mut StreamState::default());
        return;
    }
    let output = std::process::Command::new("sh")
        .args([
            "-c",
            "ulimit -c 0; ulimit -v 524288; exec \"$1\" --exact \"$2\" --nocapture",
            "stream-index-law",
        ])
        .arg(std::env::current_exe().expect("test executable"))
        .arg(format!("tests::stream_bounds::{name}"))
        .env(CHILD, "1")
        .output()
        .expect("run the memory-limited parser witness");
    assert!(
        output.status.success(),
        "index {index} must return a malformed-stream error inside 512 MiB: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

#[test]
fn billion_index_refuses_with_bounded_memory() {
    memory_limited_index_test("billion_index_refuses_with_bounded_memory", 1_000_000_000);
}

#[test]
fn maximum_wire_index_refuses_with_bounded_memory() {
    memory_limited_index_test("maximum_wire_index_refuses_with_bounded_memory", u64::MAX);
}

#[test]
fn sparse_and_repeated_starts_refuse_without_mutating_existing_blocks() {
    let mut state = StreamState::default();
    refuse_start(1, &mut state);
    AnthropicProvider::process_sse_event(&start(0), &mut state, None, false).expect("first block");
    refuse_start(2, &mut state);
    refuse_start(0, &mut state);
}

#[test]
fn invalid_indices_and_unstarted_references_refuse_for_every_block_event() {
    for kind in [
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
    ] {
        for index in [Value::Null, json!(-1), json!("0"), json!(0.5)] {
            let mut state = StreamState::default();
            let raw = json!({"type": kind, "index": index}).to_string();
            assert!(
                AnthropicProvider::process_sse_event(&raw, &mut state, None, false).is_err(),
                "{raw}"
            );
            assert!(state.blocks.is_empty());
            assert!(state.stopped_blocks.is_empty());
        }
        let mut state = StreamState::default();
        let raw = json!({"type": kind}).to_string();
        assert!(
            AnthropicProvider::process_sse_event(&raw, &mut state, None, false).is_err(),
            "{raw}"
        );
    }
    for kind in ["content_block_delta", "content_block_stop"] {
        for index in [
            0,
            BLOCK_LIMIT as u64,
            (BLOCK_LIMIT + 1) as u64,
            1_000_000_000,
            u64::MAX,
        ] {
            let mut state = StreamState::default();
            let raw = json!({"type": kind, "index": index}).to_string();
            assert!(
                AnthropicProvider::process_sse_event(&raw, &mut state, None, false).is_err(),
                "{raw}"
            );
            assert!(state.stopped_blocks.is_empty());
        }
    }
}

#[tokio::test]
async fn malformed_start_is_terminal_and_preserves_only_the_valid_prefix() {
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"kept\"}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"text\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"text_delta\",\"text\":\"discarded\"}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let error = AnthropicProvider::new("key")
        .with_transport(Arc::new(StaticSseTransport(body)))
        .complete(request(vec![LlmMessage::text(LlmRole::User, "hello")]))
        .await
        .expect_err("a later terminal marker cannot turn a malformed stream into success");
    assert_eq!(error.kind, ProviderFailureKind::Stream);
    assert_eq!(error.retry_verdict, TransportRetryVerdict::NotRetryable);
    let partial = error.partial_response.expect("retained prefix");
    assert_eq!(partial.full_text(), "kept");
    assert_eq!(partial.usage.input_tokens, 7);
}
