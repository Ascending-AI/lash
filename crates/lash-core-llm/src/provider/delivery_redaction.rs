//! The provider's outward callback and result boundary for transient deliveries.
use crate::llm::transport::{HttpFailureContext, LlmTransportError};
use lash_sansio::llm::types::{
    LiveRequestBody, LlmEventSender, LlmProviderTraceSender, LlmRequest, LlmResponse,
    LlmStreamEvent,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

type Scrubber = std::sync::Arc<dyn Fn(&str) -> String + Send + Sync>;
fn scrub_value(value: &mut Value, scrub: &Scrubber) {
    match value {
        Value::String(text) => *text = scrub(text),
        Value::Array(values) => values.iter_mut().for_each(|v| scrub_value(v, scrub)),
        Value::Object(values) => {
            let old = std::mem::take(values);
            for (key, mut value) in old {
                scrub_value(&mut value, scrub);
                values.insert(scrub(&key), value);
            }
        }
        _ => {}
    }
}
fn scrub_typed<T: Serialize + DeserializeOwned>(
    value: &mut T,
    scrub: &Scrubber,
) -> Result<(), LlmTransportError> {
    let mut json = serde_json::to_value(&*value).map_err(super::attachment_wire::template_error)?;
    scrub_value(&mut json, scrub);
    *value = serde_json::from_value(json).map_err(|_| {
        super::attachment_wire::template_error("provider text could not be safely redacted")
    })?;
    Ok(())
}

pub fn protect_callbacks(request: &mut LlmRequest, body: &LiveRequestBody) {
    if !body.has_secrets() {
        return;
    }
    let scrub = body.scrubber();
    if let Some(downstream) = request.provider_trace.take() {
        let scrub = scrub.clone();
        let redacted = body.redacted();
        request.provider_trace = Some(LlmProviderTraceSender::new(move |mut event| {
            // Raw stream fragments may split a secret across events. Keep the
            // request summary and suppress raw response fragments in this case.
            if event.request_endpoint().is_none() {
                return;
            }
            event.raw = redacted.clone();
            event.event_name = scrub(&event.event_name);
            downstream.send(event);
        }));
    }
    if let Some(downstream) = request.stream_events.take() {
        let redacted = body.redacted();
        request.stream_events = Some(LlmEventSender::new(move |mut event| {
            match &mut event {
                // Authoritative block-end and Part events carry the complete
                // text, allowing secret matching across network chunk splits.
                LlmStreamEvent::Delta { .. } | LlmStreamEvent::ReasoningDelta { .. } => return,
                LlmStreamEvent::TextBlockEnd { text, .. }
                | LlmStreamEvent::ReasoningBlockEnd { text, .. } => *text = scrub(text),
                LlmStreamEvent::Part(part) => {
                    if scrub_typed(part, &scrub).is_err() {
                        return;
                    }
                }
                LlmStreamEvent::Evidence(evidence) => {
                    evidence.request_body = Some(redacted.clone());
                    evidence.http_summary = evidence.http_summary.take().map(|t| scrub(&t));
                    if scrub_typed(&mut evidence.execution_evidence, &scrub).is_err() {
                        return;
                    }
                    if let Some(value) = &mut evidence.provider_usage {
                        scrub_value(value, &scrub);
                    }
                    for value in evidence.response_metadata.values_mut() {
                        scrub_value(value, &scrub);
                    }
                }
                LlmStreamEvent::RetryStatus { reason, .. } => *reason = scrub(reason),
                _ => {}
            }
            downstream.send(event);
        }));
    }
}

pub fn protect_result(
    result: Result<LlmResponse, LlmTransportError>,
    body: &LiveRequestBody,
) -> Result<LlmResponse, LlmTransportError> {
    let scrub = body.scrubber();
    match result {
        Ok(mut response) => {
            response.terminal_diagnostic = response.terminal_diagnostic.take().map(|s| scrub(&s));
            if body.has_secrets() {
                scrub_typed(&mut response, &scrub)?;
            }
            response.request_body = Some(body.redacted());
            Ok(response)
        }
        Err(mut error) => {
            error.message = scrub(&error.message);
            error.raw = error.raw.take().map(|text| Box::new(scrub(&text)));
            error.request_body = Some(Box::new(body.redacted()));
            for (name, value) in error.headers.iter_mut() {
                *name = scrub(name);
                *value = scrub(value);
            }
            if let HttpFailureContext::ResponseRead { detail } = error.context.as_mut() {
                *detail = scrub(detail).into();
            }
            if let Some(response) = error.partial_response.take() {
                error.partial_response = Some(Box::new(protect_result(Ok(*response), body)?));
            }
            if body.has_secrets()
                && error
                    .code
                    .as_ref()
                    .is_some_and(|code| scrub(code.spelling()) != code.spelling())
            {
                error.code = None;
            }
            Err(error)
        }
    }
}
