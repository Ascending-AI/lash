use axum::{
    body::Body,
    http::{StatusCode, header},
    response::Response,
};
use bytes::Bytes;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::json;
use std::convert::Infallible;

pub(crate) fn ndjson_response<T>(
    stream: impl futures_util::Stream<Item = T> + Send + 'static,
) -> Response
where
    T: Serialize + Send + 'static,
{
    let stream = stream.map(|item| {
        let mut line = serde_json::to_string(&item).unwrap_or_else(|_err| {
            json!({
                "type": "unavailable",
            })
            .to_string()
        });
        line.push('\n');
        Ok::<Bytes, Infallible>(Bytes::from(line))
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .expect("valid streaming response")
}
