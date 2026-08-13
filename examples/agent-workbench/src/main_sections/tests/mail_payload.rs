use axum::extract::FromRequest;

#[test]
fn wrong_field_mail_payload_is_rejected_as_unprocessable_entity() {
    run_async_test_on_stack_budget("workbench-mail-payload-rejection-test", || async {
        let request = axum::http::Request::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"subject":"ignored","body":"ignored"}"#))
            .expect("build malformed mail request");
        let rejection = Json::<InjectMessageRequest>::from_request(request, &())
            .await
            .expect_err("wrong mail field names must be rejected");

        assert_eq!(
            rejection.into_response().status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "malformed mail JSON should return HTTP 422"
        );
    });
}
