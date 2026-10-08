use super::*;

#[tokio::test]
async fn cloud_request_passthrough_refuses_owned_nested_and_header_conflicts() {
    let provider = GoogleOAuthProvider::for_test();
    let base = request(None);
    let build = |req: &LlmRequest| GoogleOAuthProvider::build_request(&provider, req, vec![], None);
    for extra in [
        json!({"contents":[]}),
        json!({"contents":null}),
        json!({"sessionId":"other"}),
    ] {
        let mut req = base.clone();
        req.model.metadata_mut().extra_body = extra.as_object().cloned().unwrap();
        assert_eq!(
            build(&req)
                .unwrap_err()
                .code
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("lash:passthrough_conflict")
        );
    }
    let mut req = base.clone();
    req.generation.temperature = Some(lash_core::NonNegativeFiniteF64::new(0.2).unwrap());
    req.model.metadata_mut().extra_body = json!({"generationConfig":{"temperature":0.4}})
        .as_object()
        .cloned()
        .unwrap();
    assert!(build(&req).is_err());
    let mut req = base.clone();
    req.generation.stop_sequences = vec!["END".into()];
    req.generation.suppress_stop_sequences_for_protocol();
    req.model.metadata_mut().extra_body = json!({"generationConfig":{"stopSequences":["END"]}})
        .as_object()
        .cloned()
        .unwrap();
    assert!(
        build(&req)
            .unwrap_err()
            .message
            .contains("/generationConfig/stopSequences")
    );
    let mut req = base.clone();
    req.model.metadata_mut().capability.sampling = lash_core::SamplingCapability::Pinned;
    req.model.metadata_mut().extra_body = json!({"generationConfig":{"temperature":0.3}})
        .as_object()
        .cloned()
        .unwrap();
    assert!(
        build(&req)
            .unwrap_err()
            .message
            .contains("/generationConfig/temperature")
    );
    let mut req = base.clone();
    req.model.metadata_mut().extra_body = json!({"generationConfig":{"host":true}})
        .as_object()
        .cloned()
        .unwrap();
    let body = build(&req).unwrap();
    assert_eq!(body["request"]["generationConfig"]["host"], true);
    assert!(body.get("generationConfig").is_none());
    let mut provider = provider.with_extra_headers(vec![("CoNtEnT-TyPe".into(), "other".into())]);
    assert_eq!(
        provider
            .complete(
                base,
                &lash_core::provider::NoSlotDeliveries,
                &lash_core::provider::LiveCallHorizon::fixture()
            )
            .await
            .unwrap_err()
            .code
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("lash:passthrough_conflict")
    );
}
