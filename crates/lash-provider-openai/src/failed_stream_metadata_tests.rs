#[tokio::test]
async fn native_adapters_preserve_allowlisted_metadata_on_failed_streams() {
    let chat_prefix = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}],\"cost\":1,\"private\":\"hidden\"}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\" output\"}}],\"cost\":2,\"private\":\"hidden\"}\n\n"
    );
    let responses_prefix = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\",\"cost\":1,\"private\":\"hidden\"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\" output\",\"cost\":2,\"private\":\"hidden\"}\n\n"
    );
    for endpoint in [
        CompletionEndpoint::ChatCompletions,
        CompletionEndpoint::Responses,
    ] {
        for explicit_error in [false, true] {
            let prefix = match endpoint {
                CompletionEndpoint::ChatCompletions => chat_prefix,
                CompletionEndpoint::Responses => responses_prefix,
            };
            let suffix = match (explicit_error, endpoint) {
                (true, CompletionEndpoint::ChatCompletions) => {
                    "data: {\"error\":{\"message\":\"fixture stream failed\",\"code\":\"server_error\",\"type\":\"server_error\"}}\n\n"
                }
                (true, CompletionEndpoint::Responses) => {
                    "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"server_error\",\"message\":\"fixture stream failed\"}}}\n\n"
                }
                (false, _) => "",
            };
            // The fixture needs owned bytes without a leaked static lifetime.
            let transport = OwnedFailedMetadataStream(format!("{prefix}{suffix}"));
            let options = ProviderOptions {
                response_metadata_headers: vec!["X-Request-Cost".into()],
                response_metadata_body_paths: vec!["/cost".into(), "/missing".into()],
                ..Default::default()
            };
            let transport: Arc<dyn LlmHttpTransport> = Arc::new(transport);
            let mut provider: Box<dyn Provider> = match endpoint {
                CompletionEndpoint::ChatCompletions => Box::new(
                    OpenAiCompatibleProvider::new("key", "https://proxy.example/v1")
                        .with_compat(OpenAiCompat {
                            stream_termination: Some(StreamTermination::RequireTerminalEvidence),
                            ..Default::default()
                        })
                        .with_options(options)
                        .with_transport(transport),
                ),
                CompletionEndpoint::Responses => Box::new(
                    OpenAiProvider::new("key")
                        .with_options(options)
                        .with_transport(transport),
                ),
            };
            let failure = provider
                .complete(streamed_request(Arc::new(std::sync::Mutex::new(vec![]))))
                .await
                .expect_err("truncated and error streams fail");
            if explicit_error {
                assert!(
                    failure.message.contains("fixture stream failed"),
                    "{failure:?}"
                );
            }
            let partial = failure
                .partial_response
                .as_ref()
                .expect("failed stream carries a partial");
            assert_eq!(partial.full_text(), "partial output");
            assert_eq!(
                partial.response_metadata,
                BTreeMap::from([
                    ("header:x-request-cost".into(), json!("0.03")),
                    ("body:/cost".into(), json!(2)),
                ]),
                "{endpoint:?} explicit_error={explicit_error}"
            );
        }
    }
}

#[derive(Debug)]
struct OwnedFailedMetadataStream(String);
#[async_trait]
impl LlmHttpTransport for OwnedFailedMetadataStream {
    async fn send(
        &self,
        _: LlmHttpRequest,
        _: Option<std::time::Duration>,
    ) -> Result<lash_llm_transport::LlmHttpResponse, LlmTransportError> {
        Ok(lash_llm_transport::LlmHttpResponse {
            status: 200,
            headers: vec![
                ("content-type".into(), "text/event-stream".into()),
                ("X-Request-Cost".into(), "0.03".into()),
                ("set-cookie".into(), "secret".into()),
            ],
            body: LlmHttpBody::buffered(self.0.clone()),
        })
    }
}
