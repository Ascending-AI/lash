//! Hermetic provider used only by the executable full-host E2E.
//!
//! This module is compiled only with the crate's `e2e` feature and is selected
//! only by the explicit `SLACK_CLONE_E2E_PROVIDER=scripted-v1` environment
//! value. Production and the manual judged runbooks therefore cannot silently
//! fall back to deterministic answers.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use lash::direct::LlmOutputPart;
use lash::provider::{LlmResponse, ProviderHandle};
use lash::{ModelSpec, sync::MutexExt as _};
use serde_json::json;

use crate::mcp_http_server::WORKSPACE_BADGE_TOOL;
use crate::mcp_server::{
    ELICIT_CONFIRMATION_TOOL, LIST_HOST_ROOTS_TOOL, SAMPLE_SUMMARY_TOOL, URL_ELICITATION_TOOL,
};

const SELECTOR_ENV: &str = "SLACK_CLONE_E2E_PROVIDER_DIR";

pub(super) fn scripted_provider_from_env() -> Result<(ProviderHandle, ModelSpec)> {
    let root = std::env::var(SELECTOR_ENV)
        .map(PathBuf::from)
        .with_context(|| format!("{SELECTOR_ENV} is required by the scripted E2E provider"))?;
    std::fs::create_dir_all(&root)
        .with_context(|| format!("create E2E provider directory {}", root.display()))?;
    let state = Arc::new(State {
        root,
        calls_by_journey: Mutex::new(BTreeMap::new()),
    });
    let provider_state = Arc::clone(&state);
    let provider = lash::testing::TestProvider::builder()
        .kind("slack-clone-full-host-e2e")
        .complete(move |request| {
            let state = Arc::clone(&provider_state);
            async move {
                let encoded = serde_json::to_string(&request).unwrap_or_else(|error| {
                    format!("provider request serialization failed: {error}")
                });
                state.record_request(&encoded);
                Ok(state.response(&encoded).await)
            }
        })
        .build()
        .into_handle();
    let model = ModelSpec::builder("test/slack-clone-e2e")
        .context_window_tokens(200_000)
        .build()
        .context("build deterministic E2E model metadata")?;
    eprintln!(
        "slack-clone-bot TEST-ONLY deterministic provider active; evidence: {}",
        state_path_for_log(&state.root).display()
    );
    Ok((provider, model))
}

struct State {
    root: PathBuf,
    calls_by_journey: Mutex<BTreeMap<&'static str, usize>>,
}

impl State {
    fn record_request(&self, encoded: &str) {
        let path = state_path_for_log(&self.root);
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{encoded}");
        }
    }

    async fn response(&self, request: &str) -> LlmResponse {
        if request.contains("Summarize this in one short sentence") {
            return text("Host-generated summary.");
        }
        let marker = latest_journey_marker(request);
        if marker == Some("FIG1341-MCP-DEPTH") {
            return match self.next("mcp") {
                0 => tool(
                    SAMPLE_SUMMARY_TOOL,
                    json!({"text": "Host policy stays with the embedding application"}),
                ),
                1 => tool(ELICIT_CONFIRMATION_TOOL, json!({})),
                2 => tool(URL_ELICITATION_TOOL, json!({})),
                3 => tool(LIST_HOST_ROOTS_TOOL, json!({})),
                _ => text(
                    "Host-generated summary. Form accepted yes. URL accepted and completion notified. Root slack-clone.",
                ),
            };
        }
        // The two attach markers are answered without consulting the catalog on
        // purpose: whether the tool is callable is decided by the host's pool,
        // not by this script, so a detached tool call must fail rather than be
        // quietly skipped here.
        if marker == Some("FIG1341-MCP-ATTACH") {
            return match self.next("mcp-attach") {
                0 => tool(WORKSPACE_BADGE_TOOL, json!({})),
                _ => text("The workspace badge came back from the attached HTTP server."),
            };
        }
        if marker == Some("FIG1341-MCP-DETACHED") {
            return text("The attached HTTP server is gone from this turn's catalog.");
        }
        if marker == Some("FIG1341-KILL-MID-TURN") {
            let entered = self.root.join("kill-provider-entered");
            if !entered.exists() {
                let _ = std::fs::write(&entered, b"entered\n");
                std::future::pending::<()>().await;
            }
            return text("Recovered the interrupted mention exactly once.");
        }
        if marker == Some("FIG1341-THREAD-TWO") {
            return text("The thread still remembers FIG1341-AMBIENT-ONE and remains isolated.");
        }
        if marker == Some("FIG1341-THREAD-ONE") {
            // Root recall is answered from the host's seed, never from a canned
            // string: a scripted answer that names the root without reading it
            // would pass the gate on a child that was never told its root.
            return match thread_root_seed(request) {
                Some(root) => text(&format!("The thread root said: {root}")),
                None => text("I cannot tell which message this thread started from."),
            };
        }
        if marker == Some("FIG1341-ROOM-MENTION") {
            return match self.next("room") {
                0 => tool("list_channels", json!({})),
                // Quoting the recalled facts back is what a real model does with
                // this prompt, and the driver must survive a channel reply that
                // carries the same markers as the human messages it recalls.
                _ => text(
                    "Ada's ambient facts are retained — FIG1341-AMBIENT-ONE says cobalt and \
                     FIG1341-AMBIENT-TWO says cedar — and #general exists.",
                ),
            };
        }
        text("Deterministic slack-clone E2E reply.")
    }

    fn next(&self, journey: &'static str) -> usize {
        let mut calls = self.calls_by_journey.lock_recover();
        let call = calls.entry(journey).or_insert(0);
        let current = *call;
        *call += 1;
        current
    }
}

/// The thread root the host seeded into this child, if it seeded one.
///
/// The seed is one labelled line inside an ordinary user message, so the read is
/// prefix-then-line: queued admissions concatenate, and everything after the
/// label's own line belongs to the next admission.
fn thread_root_seed(request: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(request).ok()?;
    value
        .get("messages")?
        .as_array()?
        .iter()
        .filter_map(|message| message.get("blocks").and_then(serde_json::Value::as_array))
        .flatten()
        .filter_map(|block| {
            block
                .pointer("/Text/text")
                .or_else(|| block.get("text"))
                .and_then(serde_json::Value::as_str)
        })
        // The label is only a label if it starts its own line: queued text inputs
        // concatenate with no separator, so a seed that lands mid-line reads as
        // the tail of whatever was copied ahead of it. Only line-leading labels
        // count, which is what makes this answer evidence rather than decoration.
        .flat_map(str::lines)
        .filter_map(|line| line.strip_prefix(super::threads::THREAD_ROOT_SEED_PREFIX))
        .map(|root| root.trim().to_string())
        .find(|root| !root.is_empty())
}

fn latest_journey_marker(request: &str) -> Option<&'static str> {
    const MARKERS: [&str; 7] = [
        "FIG1341-ROOM-MENTION",
        "FIG1341-THREAD-ONE",
        "FIG1341-THREAD-TWO",
        "FIG1341-KILL-MID-TURN",
        "FIG1341-MCP-DEPTH",
        "FIG1341-MCP-ATTACH",
        "FIG1341-MCP-DETACHED",
    ];
    let value: serde_json::Value = serde_json::from_str(request).ok()?;
    value
        .get("messages")?
        .as_array()?
        .iter()
        .rev()
        .filter_map(|message| message.get("blocks").and_then(serde_json::Value::as_array))
        .flatten()
        .filter_map(|block| {
            block
                .pointer("/Text/text")
                .or_else(|| block.get("text"))
                .and_then(serde_json::Value::as_str)
        })
        .find_map(|text| MARKERS.iter().copied().find(|marker| text.contains(marker)))
}

fn state_path_for_log(root: &Path) -> PathBuf {
    root.join("provider-requests.jsonl")
}

fn text(value: &str) -> LlmResponse {
    LlmResponse {
        full_text: value.to_string(),
        parts: vec![LlmOutputPart::Text {
            text: value.to_string(),
            response_meta: None,
        }],
        ..LlmResponse::default()
    }
}

fn tool(name: &str, args: serde_json::Value) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::ToolCall {
            call_id: format!("fig1341-{name}"),
            tool_name: name.to_string(),
            input_json: args.to_string(),
            replay: None,
        }],
        ..LlmResponse::default()
    }
}
