#[tokio::test]
async fn native_adapters_preserve_allowlisted_metadata_on_failed_streams() {
    let prefix = concat!(
        "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}],\"billing\":{\"cost\":1}}}\n\n",
        "data: {\"response\":{\"billing\":{\"cost\":2},\"private\":\"hidden\"}}\n\n"
    );
    for (case, body) in [
        prefix.to_owned(),
        format!("{prefix}data: malformed-json\n\n"),
    ]
    .into_iter()
    .enumerate()
    {
        let provider = GoogleOAuthProvider::new(
            "access",
            "refresh",
            0,
            crate::GoogleOAuthClient {
                id: "fixture".into(),
                secret: "fixture".into(),
            },
        )
        .with_options(ProviderOptions {
            response_metadata_headers: vec!["X-Request-Cost".into()],
            response_metadata_body_paths: vec![
                "/response/billing/cost".into(),
                "/missing".into(),
            ],
            ..Default::default()
        })
        .with_transport(Arc::new(StaticSseTransport::with_headers(
            body,
            vec![
                ("content-type".into(), "text/event-stream".into()),
                ("x-request-cost".into(), "0.03".into()),
                ("set-cookie".into(), "secret".into()),
            ],
        )));
        let error = provider
            .execute_request(
                "access",
                json!({"model": "gemini-test"}),
                Some(LlmEventSender::new(|_| {})),
                None,
                StreamTermination::RequireTerminalEvidence,
                None,
            )
            .await
            .expect_err("truncation and malformed events fail");
        if case == 1 {
            assert!(
                error.message.contains("Invalid Cloud Code SSE payload"),
                "{error:?}"
            );
        }
        let partial = error.partial_response.as_ref().expect("partial response");
        assert_eq!(partial.full_text(), "partial");
        assert_eq!(
            partial.response_metadata,
            std::collections::BTreeMap::from([
                ("header:x-request-cost".into(), json!("0.03")),
                ("body:/response/billing/cost".into(), json!(2)),
            ])
        );
    }
}
