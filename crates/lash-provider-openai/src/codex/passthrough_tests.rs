use super::*;

#[test]
fn codex_passthrough_refuses_owned_nested_and_suppressed_controls() {
    let provider = CodexProvider::new("access", "refresh", u64::MAX);
    let base = request(vec![LlmMessage::text(LlmRole::User, "hello")]);
    for extra in [
        json!({"model":"other"}),
        json!({"tools":[]}),
        json!({"model":null}),
    ] {
        let mut req = base.clone();
        req.extra_body = extra.as_object().cloned().unwrap();
        assert_eq!(
            provider
                .build_request(&req, false)
                .unwrap_err()
                .code
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("lash:passthrough_conflict")
        );
    }
    let mut req = base.clone();
    req.output_spec = Some(LlmOutputSpec::JsonObject);
    req.extra_body = json!({"text":{"format":{"type":"other"}}})
        .as_object()
        .cloned()
        .unwrap();
    assert!(provider.build_request(&req, false).is_err());
    let mut req = base.clone();
    req.generation.stop_sequences = vec!["END".into()];
    req.generation.suppress_stop_sequences_for_protocol();
    req.extra_body = json!({"stop":["END"]}).as_object().cloned().unwrap();
    assert!(
        provider
            .build_request(&req, false)
            .unwrap_err()
            .message
            .contains("/stop")
    );
    let mut req = base.clone();
    req.model_capability.sampling = lash_core::SamplingCapability::Pinned;
    req.extra_body = json!({"temperature":0.3}).as_object().cloned().unwrap();
    assert!(
        provider
            .build_request(&req, false)
            .unwrap_err()
            .message
            .contains("/temperature")
    );
    let mut req = base.clone();
    req.extra_body = json!({"host":{"nested":true}})
        .as_object()
        .cloned()
        .unwrap();
    assert_eq!(
        provider.build_request(&req, false).unwrap().body["host"]["nested"],
        true
    );
    let provider = provider.with_extra_headers(vec![("SeSsIoN-Id".into(), "other".into())]);
    assert_eq!(
        provider
            .preflight(&base)
            .unwrap_err()
            .code
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("lash:passthrough_conflict")
    );
}
