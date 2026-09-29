//! HTTP bootstrap for the example host's remote observation streams.

use axum::http::HeaderMap;
use lash_remote_protocol::{Negotiated, Negotiation, REMOTE_PROTOCOL, answer};

use crate::state::{AppError, AppResult};

const REMOTE_HELLO_HEADER: &str = "x-lash-protocol-hello";

pub(crate) fn negotiate_remote(headers: &HeaderMap) -> AppResult<(Negotiated, String)> {
    let hello = headers
        .get(REMOTE_HELLO_HEADER)
        .ok_or_else(|| AppError::bad_request("missing remote protocol Hello"))?
        .to_str()
        .map_err(|_| AppError::bad_request("invalid remote protocol Hello header"))?;
    let hello: Negotiation = serde_json::from_str(hello)
        .map_err(|_| AppError::bad_request("invalid remote protocol Hello"))?;
    let accept = answer(REMOTE_PROTOCOL, &hello);
    let negotiated = Negotiated::from_accept(REMOTE_PROTOCOL, &accept)
        .map_err(|error| AppError::bad_request(error.to_string()))?;
    let accept_json = serde_json::to_string(&accept)
        .map_err(|error| AppError::internal(format!("encode protocol Accept: {error}")))?;
    Ok((negotiated, accept_json))
}

#[cfg(test)]
pub(crate) fn test_remote_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        REMOTE_HELLO_HEADER,
        serde_json::to_string(&Negotiation::Hello {
            supported: REMOTE_PROTOCOL,
        })
        .expect("encode test Hello")
        .parse()
        .expect("valid test Hello header"),
    );
    headers
}
