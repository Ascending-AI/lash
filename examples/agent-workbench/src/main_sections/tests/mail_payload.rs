use super::*;

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

/// Register a `mail.received` trigger in each session.
const MAIL_TRIGGER_REGISTRATION: &str = r#"<typescript>
const listen = async (event: unknown) => {
  return event.title;
};
await triggers.register({
  source: mail.received({}),
  target: { definition: listen },
  inputs: (event) => ({ event: event }),
  name: "mail listener"
});
finish("registered");
</typescript>"#;

/// The subscriptions `session_id` registered.
async fn session_subscriptions(
    state: &AppState,
    session_id: &lash::SessionId,
) -> Vec<lash::triggers::TriggerSubscriptionRecord> {
    state
        .trigger_store
        .list_subscriptions(lash::triggers::TriggerSubscriptionFilter::for_session(
            session_id,
        ))
        .await
        .expect("list the session's subscriptions")
}

/// A delivery injected for one session emits its `mail.received` occurrence
/// in that session only: its own matching subscription delivers, and the
/// same subscription another session registered does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inject_message_scopes_emission_to_requested_session() {
    let workbench = Workbench::replying(MAIL_TRIGGER_REGISTRATION).await;
    let state = &workbench.state;
    let other_session_id = state.current_session_id();
    let Json(scoped) = create_session(
        State(state.clone()),
        Json(SessionCreateRequest {
            name: Some("scoped".to_string()),
        }),
    )
    .await
    .expect("create the scoped session");
    let scoped_session_id = scoped.session_id;
    run_turn_in(state, &other_session_id, "register the mail listener").await;
    run_turn_in(state, &scoped_session_id, "register the mail listener").await;
    let [scoped_subscription] = session_subscriptions(state, &scoped_session_id)
        .await
        .try_into()
        .expect("the scoped session registered one subscription");
    let [other_subscription] = session_subscriptions(state, &other_session_id)
        .await
        .try_into()
        .expect("the other session registered one subscription");
    let slug = state
        .mail_world
        .add_account("Test Inbox")
        .expect("add the mock account")
        .slug;

    let Json(accepted) = inject_message(
        AxumPath(slug),
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(scoped_session_id.clone()),
        }),
        Json(InjectMessageRequest {
            title: "Important Update".to_string(),
            text: "Hello from test".to_string(),
            model: None,
            model_variant: None,
        }),
    )
    .await
    .expect("the injected message is accepted");
    assert!(accepted.accepted);

    let deliveries = state
        .trigger_store
        .list_deliveries_by_subscription_id(&scoped_subscription.subscription_id)
        .await
        .expect("list the scoped session's deliveries");
    let [delivery] = deliveries.as_slice() else {
        panic!("the scoped session's subscription delivers once: {deliveries:?}");
    };
    let process_id = delivery
        .process_id()
        .cloned()
        .expect("the delivery started its process");
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        state.core.processes().await_output(&process_id),
    )
    .await
    .expect("the listener finishes in time")
    .expect("the listener finishes");
    let lash::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the listener settles: {output:?}");
    };
    assert_eq!(
        output.value_for_projection(),
        json!("Important Update"),
        "the listener read the injected delivery"
    );
    assert!(
        state
            .trigger_store
            .list_deliveries_by_subscription_id(&other_subscription.subscription_id)
            .await
            .expect("list the other session's deliveries")
            .is_empty(),
        "the injected message must not deliver to another session's subscription"
    );
    workbench.shutdown().await;
}
