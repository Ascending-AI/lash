//! The server's HTTP face: Restate's ingress API (service calls and sends,
//! awakeables, attach and output) and the admin API routes lash uses
//! (cancel, kill, resume, the `sys_invocation` query), served in process.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use lash_http_transport::{
    HttpMethod, HttpRequest, HttpResponse, HttpResponseBody, HttpTransport, LlmTransportError,
    run_with_timeout,
};
use serde_json::json;
use tokio::sync::oneshot;

use super::Shared;
use super::catalog::{HandlerKind, ServiceKind};
use super::model::{Outcome, Status, Target, Waiter};
use super::processor::{
    AttachTarget, ControlResult, Submission, Submitted, WORKFLOW_ALREADY_INVOKED,
};
use crate::protocol::generated::{self as pb, notification_template};

#[derive(Clone)]
pub(super) struct IngressTransport {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for IngressTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RestateTestServer ingress")
    }
}

impl IngressTransport {
    pub(super) fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }
}

#[async_trait::async_trait]
impl HttpTransport for IngressTransport {
    async fn send(
        &self,
        request: HttpRequest,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, LlmTransportError> {
        let message = request
            .response_start_timeout_message
            .clone()
            .unwrap_or_else(|| format!("{} timed out", request.url));
        run_with_timeout(async { Ok(self.route(request).await) }, timeout, &message).await
    }
}

fn respond(status: u16, body: impl Into<Bytes>) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: HttpResponseBody::buffered(body.into()),
    }
}

fn respond_json(status: u16, value: &serde_json::Value) -> HttpResponse {
    respond(status, value.to_string())
}

fn error(status: u16, message: impl Into<String>) -> HttpResponse {
    respond_json(status, &json!({ "message": message.into() }))
}

/// An invocation failure as Restate's ingress answers it: the failure code
/// as the status, `{code, message}` as the body.
fn failure_response(failure: &pb::Failure) -> HttpResponse {
    let status = u16::try_from(failure.code)
        .ok()
        .filter(|code| (400..=599).contains(code))
        .unwrap_or(500);
    respond_json(
        status,
        &json!({ "code": failure.code, "message": failure.message }),
    )
}

fn outcome_response(outcome: Outcome) -> HttpResponse {
    match outcome {
        Outcome::Success(bytes) => respond(200, bytes),
        Outcome::Failure(failure) => failure_response(&failure),
    }
}

fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(hex) = segment.get(index + 1..index + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            decoded.push(byte);
            index += 3;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// `?delay=` as ingress accepts it for sends: `500ms`, `10s`, `2m`, `1h`.
fn parse_delay(query: &str) -> Result<Option<Duration>, String> {
    for pair in query.split('&') {
        let Some(value) = pair.strip_prefix("delay=") else {
            continue;
        };
        let value = percent_decode(value);
        let (digits, unit) = value.split_at(
            value
                .find(|character: char| !character.is_ascii_digit())
                .unwrap_or(value.len()),
        );
        let amount: u64 = digits
            .parse()
            .map_err(|_| format!("cannot parse delay '{value}'"))?;
        let millis = match unit {
            "ms" => amount,
            "s" | "" => amount.saturating_mul(1000),
            "m" => amount.saturating_mul(60_000),
            "h" => amount.saturating_mul(3_600_000),
            _ => return Err(format!("cannot parse delay '{value}'")),
        };
        return Ok(Some(Duration::from_millis(millis)));
    }
    Ok(None)
}

impl IngressTransport {
    async fn route(&self, request: HttpRequest) -> HttpResponse {
        let base = self.shared.config.ingress_url.trim_end_matches('/');
        let rest = request
            .url
            .strip_prefix(base)
            .unwrap_or(&request.url)
            .trim_start_matches('/');
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        let segments: Vec<String> = path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(percent_decode)
            .collect();
        let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
        match (request.method, segments.as_slice()) {
            (HttpMethod::Get, ["health"] | ["restate", "health"]) => respond(200, ""),
            (HttpMethod::Post, ["deployments"]) => respond_json(201, &json!({})),
            (
                HttpMethod::Patch | HttpMethod::Put,
                ["invocations", id, action @ ("cancel" | "kill" | "resume")],
            ) => self.control(id, action),
            (HttpMethod::Post, ["query"]) => self.query(&request),
            (HttpMethod::Post, ["services", service, "state"]) => {
                self.modify_state(service, &request.body)
            }
            (
                HttpMethod::Post,
                [
                    "restate",
                    "awakeables" | "a",
                    id,
                    action @ ("resolve" | "reject"),
                ],
            ) => self.awakeable(id, action, request.body.clone()),
            (
                HttpMethod::Get | HttpMethod::Post,
                ["restate", "invocation", id, action @ ("attach" | "output")],
            ) => {
                self.attach(
                    AttachTarget::Invocation((*id).to_owned()),
                    *action == "attach",
                )
                .await
            }
            (
                HttpMethod::Get | HttpMethod::Post,
                [
                    "restate",
                    "workflow",
                    name,
                    key,
                    action @ ("attach" | "output"),
                ],
            ) => {
                self.attach(
                    AttachTarget::Workflow {
                        name: (*name).to_owned(),
                        key: (*key).to_owned(),
                    },
                    *action == "attach",
                )
                .await
            }
            (HttpMethod::Get | HttpMethod::Post, [service, rest @ ..]) => {
                self.invoke(&request, service, rest, query).await
            }
            _ => error(400, format!("unsupported ingress request {}", request.url)),
        }
    }

    async fn invoke(
        &self,
        request: &HttpRequest,
        service: &str,
        rest: &[&str],
        query: &str,
    ) -> HttpResponse {
        let Some(entry) = self.shared.catalog().service(service) else {
            return error(
                404,
                format!(
                    "service '{service}' not found, make sure to register the service before calling it."
                ),
            );
        };
        let keyed = entry.kind != ServiceKind::Service;
        let (key, handler, send) = match (keyed, rest) {
            (false, [handler]) => (None, *handler, false),
            (false, [handler, "send"]) => (None, *handler, true),
            (true, [key, handler]) => (Some(*key), *handler, false),
            (true, [key, handler, "send"]) => (Some(*key), *handler, true),
            _ => return error(400, format!("bad invocation path for {service}")),
        };
        let Some(spec) = entry.handlers.get(handler) else {
            return error(404, format!("handler '{service}/{handler}' not found"));
        };
        let idempotency_key = header(request, "idempotency-key").map(str::to_owned);
        if spec.kind == HandlerKind::WorkflowRun && idempotency_key.is_some() {
            return error(
                400,
                "idempotency key is not supported on workflow run handlers",
            );
        }
        let delay = match parse_delay(query) {
            Ok(delay) => delay,
            Err(message) => return error(400, message),
        };
        if delay.is_some() && !send {
            return error(400, "a delay is only allowed on a send");
        }
        let headers = request
            .headers
            .iter()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("idempotency-key"))
            .map(|(name, value)| pb::Header {
                key: name.clone(),
                value: value.clone(),
            })
            .collect();
        let (receiver, submitted, id) = {
            let mut state = self.shared.lock();
            let start_at_ms =
                delay.map(|delay| state.now_ms + super::processor::duration_ms(delay));
            let submission = Submission {
                target: Target {
                    service: service.to_owned(),
                    handler: handler.to_owned(),
                    key: key.map(str::to_owned),
                },
                input: request.body.clone(),
                headers,
                idempotency_key,
                start_at_ms,
                parent: None,
            };
            let (invocation, submitted) = match state.submit(&self.shared, submission) {
                Ok(submitted) => submitted,
                Err(refusal) => return error(404, refusal.message()),
            };
            let id = state.invocations[invocation.0].id.as_str().to_owned();
            let receiver = (!send && submitted != Submitted::WorkflowRunExists).then(|| {
                let (sender, receiver) = oneshot::channel();
                state.add_waiter(&self.shared, invocation, Waiter::Ingress(sender));
                receiver
            });
            (receiver, submitted, id)
        };
        if send {
            let status = match submitted {
                Submitted::Fresh => "Accepted",
                Submitted::Idempotent | Submitted::WorkflowRunExists => "PreviouslyAccepted",
            };
            let mut response = respond_json(202, &json!({ "invocationId": id, "status": status }));
            response.headers.push(("x-restate-id".into(), id));
            return response;
        }
        if submitted == Submitted::WorkflowRunExists {
            let (code, message) = WORKFLOW_ALREADY_INVOKED;
            return failure_response(&pb::Failure {
                code,
                message: message.to_owned(),
                metadata: Vec::new(),
            });
        }
        match receiver {
            Some(receiver) => match receiver.await {
                Ok(outcome) => {
                    let mut response = outcome_response(outcome);
                    response.headers.push(("x-restate-id".into(), id));
                    response
                }
                Err(_) => error(503, "the server dropped the invocation"),
            },
            None => error(500, "no result channel"),
        }
    }

    async fn attach(&self, target: AttachTarget, block: bool) -> HttpResponse {
        let receiver = {
            let mut state = self.shared.lock();
            let Some(invocation) = state.resolve_target(Some(target)) else {
                return error(404, "invocation not found");
            };
            match &state.invocations[invocation.0].status {
                Status::Completed(outcome) => return outcome_response(outcome.clone()),
                _ if !block => {
                    return error(470, "the invocation exists but has not completed yet");
                }
                _ => {}
            }
            let (sender, receiver) = oneshot::channel();
            state.add_waiter(&self.shared, invocation, Waiter::Ingress(sender));
            receiver
        };
        match receiver.await {
            Ok(outcome) => outcome_response(outcome),
            Err(_) => error(503, "the server dropped the invocation"),
        }
    }

    fn awakeable(&self, id: &str, action: &str, body: Bytes) -> HttpResponse {
        let result = if action == "resolve" {
            notification_template::Result::Value(pb::Value { content: body })
        } else {
            notification_template::Result::Failure(pb::Failure {
                code: 500,
                message: String::from_utf8_lossy(&body).into_owned(),
                metadata: Vec::new(),
            })
        };
        let mut state = self.shared.lock();
        state.complete_awakeable(&self.shared, id, result);
        respond(202, "")
    }

    fn control(&self, id: &str, action: &str) -> HttpResponse {
        let mut state = self.shared.lock();
        let Some(invocation) = state.lookup(id) else {
            return error(404, format!("invocation {id} not found"));
        };
        let result = match action {
            "cancel" => state.cancel(&self.shared, invocation),
            "kill" => state.kill(&self.shared, invocation),
            _ => {
                if state.resume(&self.shared, invocation) {
                    ControlResult::Done
                } else {
                    return error(409, format!("invocation {id} is not paused"));
                }
            }
        };
        match result {
            ControlResult::Appended => respond(202, ""),
            ControlResult::Done => respond(200, ""),
            ControlResult::AlreadyCompleted => {
                error(409, format!("invocation {id} is already completed"))
            }
        }
    }

    /// The admin API's service-state modification: replace one key's whole
    /// state with `new_state` (values are byte arrays).
    fn modify_state(&self, service: &str, body: &Bytes) -> HttpResponse {
        #[derive(serde::Deserialize)]
        struct ModifyState {
            object_key: String,
            new_state: std::collections::BTreeMap<String, Vec<u8>>,
        }
        let Ok(modify) = serde_json::from_slice::<ModifyState>(body) else {
            return error(400, "the body must be {\"object_key\", \"new_state\"}");
        };
        if self.shared.catalog().service(service).is_none() {
            return error(404, format!("service '{service}' not found"));
        }
        let mut state = self.shared.lock();
        state.replace_state(
            (service.to_owned(), modify.object_key),
            modify
                .new_state
                .into_iter()
                .map(|(name, value)| (name, Bytes::from(value)))
                .collect(),
        );
        respond(202, "")
    }

    fn query(&self, request: &HttpRequest) -> HttpResponse {
        #[derive(serde::Deserialize)]
        struct QueryRequest {
            query: String,
        }
        let Ok(QueryRequest { query }) = serde_json::from_slice(&request.body) else {
            return error(400, "the query body must be {\"query\": \"...\"}");
        };
        let state = self.shared.lock();
        match super::query::run(&state, &query) {
            Ok(rows) => respond_json(200, &json!({ "rows": rows })),
            Err(message) => error(400, message),
        }
    }
}
