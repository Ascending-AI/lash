use super::*;

#[test]
fn bounded_error_projection_preserves_retry_delay_and_message_after_large_echo() {
    let value = json!({
        "echo": "x".repeat(80 * 1024),
        "error": {"message": "try again", "details": [{"retryDelay": "1.5s"}]},
    });
    let metadata = crate::request_work::error_metadata(&value).unwrap();
    assert!(metadata.len() < 100);
    let failure = http_error_envelope("request failed", 429, Vec::new(), metadata, None);
    assert_eq!(failure.message, "request failed: try again");
    assert_eq!(
        failure.retry_verdict,
        TransportRetryVerdict::RetryableThrottle {
            retry_after: Some(std::time::Duration::from_millis(1500)),
        }
    );
}
