//! The matrix's per-dialect tables and its checked-in location.
use std::path::{Path, PathBuf};

pub(super) fn dialect_model(dialect: &str) -> &'static str {
    match dialect {
        "anthropic.messages" => "claude-matrix",
        "google.generate-content" => "gemini-matrix",
        "openai.chat-completions" => "openai/matrix",
        "openai.responses" => "gpt-matrix",
        "codex.responses-sse" | "codex.responses-websocket" => "gpt-matrix-codex",
        other => panic!("unknown matrix dialect {other}"),
    }
}

pub(super) fn dialect_path(dialect: &str) -> &'static str {
    match dialect {
        "anthropic.messages" => "/v1/messages",
        "google.generate-content" => "/v1internal:streamGenerateContent",
        "openai.chat-completions" => "/chat/completions",
        "openai.responses" => "/responses",
        "codex.responses-sse" => "/backend-api/codex/responses",
        other => panic!("unknown HTTP matrix dialect {other}"),
    }
}

pub(super) fn matrix_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("provider-scripts/variation-matrix/v1")
}

pub(super) fn matrix_generator() -> PathBuf {
    matrix_dir()
        .parent()
        .expect("version directory has generator parent")
        .join("generate.py")
}
