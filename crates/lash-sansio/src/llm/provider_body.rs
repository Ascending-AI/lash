//! The exact body a provider sends for one model call (ADR 0133 §6,
//! FIG-5259).

use std::sync::Arc;

use super::types::{GenerationReceipt, LlmRequest, ProviderRouteIdentity};

/// The exact body one model call sends: lowered once, by the provider of
/// the route that serves the call, before the call is admitted.
///
/// A resend of an admitted call sends these bytes: no renderer, projector,
/// attachment resolver or provider builder runs for it again, so a provider
/// whose builder changed since sends the body the call admitted.
/// Authentication and transport headers are not part of a body: the
/// provider binds them fresh for every attempt.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRequestBody {
    /// The route that lowered the body. Only that route sends it.
    pub route: ProviderRouteIdentity,
    /// Whether the body asks for a streamed response.
    pub stream: bool,
    /// The receipt of the generation settings the body carries, as the
    /// provider built it: the response of every attempt reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationReceipt>,
    /// The exact bytes. Every provider body is UTF-8 JSON.
    pub body: Arc<str>,
}

impl ProviderRequestBody {
    /// The canonical encoding of `request` itself, for a provider with no
    /// wire format of its own, such as an in-process double: the logical
    /// request is what it receives.
    ///
    /// # Errors
    ///
    /// The encoding error when the request does not encode.
    pub fn of_request(
        route: ProviderRouteIdentity,
        request: &LlmRequest,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            route,
            stream: request.stream_events.is_some(),
            generation: None,
            body: Arc::from(serde_json::to_string(request)?),
        })
    }
}
