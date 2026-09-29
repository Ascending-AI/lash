#[cfg(feature = "testing")]
pub mod cache_regression;
#[cfg(feature = "testing")]
pub mod conformance;
pub mod http;
pub mod normalize;
#[cfg(feature = "proptest-support")]
pub mod proptest_support;
pub mod response_metadata;
pub mod streaming;
pub mod timeouts;
pub mod util;

pub use http::{
    LlmByteStream, LlmHttpBody, LlmHttpMethod, LlmHttpRequest, LlmHttpResponse, LlmHttpTransport,
    ReqwestByteStream, ReqwestLlmHttpTransport, first_header_value, header_contains,
    read_http_body_bytes, read_http_body_text,
};
pub use normalize::{
    frame_sse_payload, http_error_envelope, merge_usage,
    openai_terminal_reason_from_chat_finish_reason, openai_terminal_reason_from_chat_value,
    openai_terminal_reason_from_response_value, openai_usage_from_response_value,
    openai_usage_from_usage_value, serialize_options_tail, terminal_reason_from_parts,
};
pub use response_metadata::ResponseMetadataCapture;
pub use timeouts::{
    DEFAULT_CHUNK_TIMEOUT_MS, DEFAULT_REQUEST_TIMEOUT_MS, LlmTimeouts, build_http_client,
    header_pairs, response_start_timeout, run_with_timeout,
};
pub use util::{
    cache_intent_disposition, emit_provider_request_trace, emit_provider_trace,
    extract_error_detail, parse_i64,
};
