//! The browser UI, served as one self-contained document.

use axum::response::Html;

/// The single-page chat client.
pub const INDEX_HTML: &str = include_str!("../../assets/index.html");

/// `GET /` — serve the UI.
pub async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}
