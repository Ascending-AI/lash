#[tokio::test]
async fn native_adapters_preserve_allowlisted_metadata_on_failed_streams() {
    let prefix = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}},\"billing\":{\"cost\":1}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"},\"billing\":{\"cost\":2},\"private\":\"hidden\"}\n\n"
    );
    for (case, body) in [
        prefix.to_owned(),
        format!(
            "{prefix}data: {{\"type\":\"error\",\"error\":{{\"type\":\"overloaded_error\",\"message\":\"fixture failed\"}}}}\n\n"
        ),
    ].into_iter().enumerate() {
        let mut provider = AnthropicProvider::new("key")
            .with_options(ProviderOptions {
                response_metadata_headers: vec!["X-Request-Cost".into()],
                response_metadata_body_paths: vec!["/billing/cost".into(), "/missing".into()],
                ..Default::default()
            })
            .with_transport(Arc::new(OwnedMetadataSseTransport(body)));
        let mut req = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
        req.stream_events = Some(LlmEventSender::new(|_| {}));
        let error = provider
            .complete(req)
            .await
            .expect_err("missing stop and explicit error fail");
        if case == 1 {
            assert!(error.message.contains("fixture failed"), "{error:?}");
        }
        let partial = error.partial_response.as_ref().expect("partial response");
        assert_eq!(partial.full_text(), "partial");
        assert_eq!(
            partial.response_metadata,
            BTreeMap::from([
                ("header:x-request-cost".into(), json!("0.02")),
                ("body:/billing/cost".into(), json!(2)),
            ])
        );
    }
}

#[derive(Debug)]
struct OwnedMetadataSseTransport(String);
#[async_trait::async_trait]
impl lash_llm_transport::LlmHttpTransport for OwnedMetadataSseTransport {
    async fn send(
        &self,
        _: lash_llm_transport::LlmHttpRequest,
        _: Option<std::time::Duration>,
    ) -> Result<lash_llm_transport::LlmHttpResponse, lash_core::facade_support::LlmTransportError>
    {
        Ok(lash_llm_transport::LlmHttpResponse {
            status: 200,
            headers: vec![
                ("content-type".into(), "text/event-stream".into()),
                ("x-request-cost".into(), "0.02".into()),
                ("set-cookie".into(), "secret".into()),
            ],
            body: lash_llm_transport::LlmHttpBody::buffered(self.0.clone()),
        })
    }
}
