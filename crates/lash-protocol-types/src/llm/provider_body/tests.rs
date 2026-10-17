use super::*;

#[test]
fn response_mode_has_one_authority_and_preserves_literal_bytes() {
    let route = ProviderRouteIdentity {
        provider: "fixture".into(),
        endpoint: "https://fixture.test".into(),
        model: "model".into(),
    };
    let body = " { \"stream\" : true, \"input\" : \"literal\" } ";
    assert!(RecordedRequestTemplate::literal(route.clone(), false, None, body).is_err());
    let template = RecordedRequestTemplate::literal(route.clone(), true, None, body).unwrap();
    let encoded = serde_json::to_value(&template).unwrap();
    assert!(encoded.get("stream").is_none());
    assert!(encoded.get("transport_stream").is_none());
    let decoded: RecordedRequestTemplate = serde_json::from_value(encoded.clone()).unwrap();
    let live = LiveRequestBody::fill(Arc::new(decoded), vec![]).unwrap();
    assert!(live.stream());
    assert_eq!(live.to_wire_string(), body);
    for stream in [false, true] {
        let mut duplicate = encoded.clone();
        duplicate["transport_stream"] = serde_json::json!(stream);
        assert!(serde_json::from_value::<RecordedRequestTemplate>(duplicate).is_err());
    }
    let mut builder = RecordedRequestTemplate::builder(route.clone(), false, None);
    builder.literal(body);
    assert!(builder.finish().is_err());
    for invalid in [
        "{\"stream\":null}",
        "{\"stream\":1}",
        "{\"stream\":\"true\"}",
        "{\"stream\":true,\"stream\":false}",
    ] {
        assert!(RecordedRequestTemplate::literal(route.clone(), true, None, invalid).is_err());
    }
    let mut conflicting = encoded;
    conflicting["stream"] = serde_json::json!(false);
    assert!(serde_json::from_value::<RecordedRequestTemplate>(conflicting).is_err());
    for stream in [false, true] {
        let template = RecordedRequestTemplate::literal(route.clone(), stream, None, "{}").unwrap();
        let mut encoded = serde_json::to_value(template).unwrap();
        let decoded: RecordedRequestTemplate = serde_json::from_value(encoded.clone()).unwrap();
        encoded.as_object_mut().unwrap().remove("transport_stream");
        assert!(serde_json::from_value::<RecordedRequestTemplate>(encoded).is_err());
        assert_eq!(
            LiveRequestBody::fill(Arc::new(decoded), vec![])
                .unwrap()
                .stream(),
            stream
        );
    }
}
