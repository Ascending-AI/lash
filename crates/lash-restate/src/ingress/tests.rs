use super::{
    DeploymentOpenInvocations, RestateInvocationLifecycle, RestateInvocationStatus,
    open_invocation_statuses_sql,
};

#[derive(Debug, Default)]
struct CaptureTransport(std::sync::Mutex<Vec<super::HttpRequest>>);

#[async_trait::async_trait]
impl super::HttpTransport for CaptureTransport {
    async fn send(
        &self,
        request: super::HttpRequest,
        _timeout: Option<std::time::Duration>,
    ) -> Result<super::HttpResponse, super::LlmTransportError> {
        self.0.lock().expect("requests").push(request);
        Ok(super::HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: lash_http_transport::HttpResponseBody::buffered(
                r#"{"invocationId":"inv-1","status":"Accepted","ok":true}"#,
            ),
        })
    }
}

#[tokio::test]
async fn ingress_propagates_explicit_transport_context_without_changing_input() {
    let transport = std::sync::Arc::new(CaptureTransport::default());
    let client = super::RestateIngressClient::new(super::RestateConnection::with_transport(
        "http://unused",
        transport.clone(),
    ));
    let carrier = lash_trace::TraceCarrier::parse_w3c(
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00",
        Some("rojo=1"),
    )
    .expect("carrier");
    let mut traced = client.clone();
    traced.transport_context = Some(carrier.clone());
    let input = serde_json::json!({"business":"unchanged"});
    traced
        .send_service_json("Service", "run", &input)
        .await
        .expect("service send");
    traced
        .call_workflow_json::<_, serde_json::Value>("Workflow", "key", "run", &input)
        .await
        .expect("workflow call");
    traced
        .send_object_json_idempotent("Object", "key", "run", &input, "stable")
        .await
        .expect("object send");
    traced.transport_context = None;
    traced
        .send_service_json("Service", "run", &input)
        .await
        .expect("untraced send");
    let requests = transport.0.lock().expect("requests");
    assert_eq!(requests.len(), 4);
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request.body).expect("input"),
            input
        );
        let expected_parent = (index < 3).then(|| carrier.traceparent());
        let expected_state = (index < 3).then_some("rojo=1");
        assert_eq!(
            lash_http_transport::first_header_value(&request.headers, "traceparent"),
            expected_parent.as_deref()
        );
        assert_eq!(
            lash_http_transport::first_header_value(&request.headers, "tracestate"),
            expected_state
        );
    }
    assert_eq!(
        lash_http_transport::first_header_value(&requests[2].headers, "idempotency-key"),
        Some("stable")
    );
}

#[test]
fn an_unrecognized_status_decodes_to_unknown_and_reports_open() {
    let row: RestateInvocationStatus = serde_json::from_str(
        r#"{
            "id": "invocation-1",
            "target": "service/handler",
            "target_service_name": "service",
            "target_handler_name": "handler",
            "status": "zombie-from-a-future-restate"
        }"#,
    )
    .expect("an unrecognized status must not fail decoding");

    assert_eq!(
        row.status,
        RestateInvocationLifecycle::Unknown("zombie-from-a-future-restate".to_string())
    );
    assert!(
        row.is_still_active(),
        "an unknown status is open: a drain must not declare itself complete on it"
    );
    assert_eq!(row.status.as_str(), "zombie-from-a-future-restate");
}

#[test]
fn the_open_status_sql_filter_names_exactly_the_known_open_variants() {
    assert_eq!(
        open_invocation_statuses_sql(),
        "status IN ('pending', 'ready', 'running', 'backing-off', 'suspended', 'paused')"
    );
    for status in [
        RestateInvocationLifecycle::Pending,
        RestateInvocationLifecycle::Ready,
        RestateInvocationLifecycle::Running,
        RestateInvocationLifecycle::BackingOff,
        RestateInvocationLifecycle::Suspended,
        RestateInvocationLifecycle::Paused,
    ] {
        assert!(status.is_open(), "{status} is a known open status");
    }
    for status in [
        RestateInvocationLifecycle::Completed,
        RestateInvocationLifecycle::Failed,
    ] {
        assert!(!status.is_open(), "{status} is terminal");
    }
}

#[test]
fn invocation_status_deserializes_captured_rows_with_and_without_deployment() {
    let pinned: RestateInvocationStatus = serde_json::from_str(
        r#"{
            "id": "invocation-1",
            "target": "service/handler",
            "target_service_name": "service",
            "pinned_deployment_id": "deployment-a",
            "target_service_key": null,
            "target_handler_name": "handler",
            "status": "running",
            "completion_result": null,
            "completion_failure": null
        }"#,
    )
    .expect("pinned invocation row should deserialize");
    assert_eq!(pinned.pinned_deployment_id.as_deref(), Some("deployment-a"));

    let legacy: RestateInvocationStatus = serde_json::from_str(
        r#"{
            "id": "invocation-2",
            "target": "service/handler",
            "target_service_name": "service",
            "target_handler_name": "handler",
            "status": "pending"
        }"#,
    )
    .expect("row without a deployment column should deserialize");
    assert_eq!(legacy.pinned_deployment_id, None);
}

#[test]
fn deployment_counts_deserialize_unpinned_rows_without_filtering_them() {
    let rows: Vec<DeploymentOpenInvocations> = serde_json::from_str(
        r#"[
            {"pinned_deployment_id": "deployment-a", "open_count": 2},
            {"pinned_deployment_id": null, "open_count": 1}
        ]"#,
    )
    .expect("deployment count rows should deserialize");

    assert_eq!(
        rows[0].pinned_deployment_id.as_deref(),
        Some("deployment-a")
    );
    assert_eq!(rows[0].open_count, 2);
    assert_eq!(rows[1].pinned_deployment_id, None);
    assert_eq!(rows[1].open_count, 1);
}
