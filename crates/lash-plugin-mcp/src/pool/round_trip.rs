//! MCP 2026-07-28 input_required rounds, within one Lash attempt's wall cap.
//!
//! rmcp 3.5.1 still requires the removed URL elicitationId in its input union.
//! Its CustomResult seam preserves conforming modern results. Decode that
//! envelope here into the modern host payload, without manufacturing a legacy ID.
#![allow(
    deprecated,
    reason = "existing roots and sampling remain supported during their deprecation window"
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use rmcp::ClientHandler;
use rmcp::model::{RequestMetaObject, ServerResult};
use rmcp::service::{Peer, RequestContext, RoleClient};
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::host::{LashMcpClientHandler, McpElicitationParams};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputRequired {
    input_requests: Option<BTreeMap<String, InputRequest>>,
    request_state: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "method", content = "params")]
enum InputRequest {
    #[serde(rename = "sampling/createMessage")]
    Sampling(rmcp::model::CreateMessageRequestParams),
    #[serde(rename = "elicitation/create")]
    Elicitation(McpElicitationParams),
    #[serde(rename = "roots/list")]
    Roots,
}

pub(super) struct Retry {
    pub responses: Option<BTreeMap<String, Value>>,
    pub state: Option<String>,
}

pub(super) async fn fulfill(
    result: ServerResult,
    handler: &LashMcpClientHandler,
    peer: &Peer<RoleClient>,
    cancellation: &CancellationToken,
) -> Result<Retry, rmcp::ServiceError> {
    let raw = match result {
        ServerResult::InputRequiredResult(result) => serde_json::to_value(result),
        ServerResult::CustomResult(result) => Ok(result.0),
        _ => return Err(rmcp::ServiceError::UnexpectedResponse),
    }
    .map_err(protocol_decode)?;
    if raw.get("resultType").and_then(Value::as_str) != Some("input_required") {
        return Err(rmcp::ServiceError::UnexpectedResponse);
    }
    let result: InputRequired = serde_json::from_value(raw).map_err(protocol_decode)?;
    let requests = result.input_requests.unwrap_or_default();
    if requests.is_empty() && result.request_state.is_none() {
        return Err(rmcp::ServiceError::UnexpectedResponse);
    }
    let mut responses = BTreeMap::new();
    for (key, request) in requests {
        let mut context = RequestContext::new(
            rmcp::model::NumberOrString::String(Arc::from(key.as_str())),
            peer.clone(),
        );
        context.ct = cancellation.clone();
        let response = match request {
            InputRequest::Sampling(mut params) => {
                context.meta = params.meta.take().unwrap_or_else(RequestMetaObject::new);
                let response = handler
                    .create_message(params, context)
                    .await
                    .map_err(rmcp::ServiceError::McpError)?;
                serde_json::to_value(response)
            }
            InputRequest::Elicitation(params) => {
                let response = handler
                    .fulfill_elicitation(params, cancellation.clone())
                    .await
                    .map_err(rmcp::ServiceError::McpError)?;
                serde_json::to_value(response)
            }
            InputRequest::Roots => {
                let response = handler
                    .list_roots(context)
                    .await
                    .map_err(rmcp::ServiceError::McpError)?;
                serde_json::to_value(response)
            }
        }
        .map_err(protocol_decode)?;
        responses.insert(key, response);
    }
    if responses.is_empty() {
        // A state-only retry may represent an out-of-band interaction. Avoid
        // a tight loop while retaining the same total attempt deadline.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Ok(Retry {
        responses: (!responses.is_empty()).then_some(responses),
        state: result.request_state,
    })
}

fn protocol_decode(error: serde_json::Error) -> rmcp::ServiceError {
    rmcp::ServiceError::McpError(rmcp::model::ErrorData::invalid_params(
        format!("invalid MCP input_required result: {error}"),
        None,
    ))
}
