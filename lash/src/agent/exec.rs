use tokio::sync::mpsc;

use crate::session::Session;

use super::{AgentEvent, TokenUsage, send_event};

/// Accumulated state from executing code blocks during a single LLM turn.
pub(crate) struct ExecAccumulator {
    pub tool_calls: Vec<crate::ToolCallRecord>,
    pub images: Vec<crate::ToolImage>,
    pub combined_output: String,
    pub final_response: String,
    pub exec_error: Option<String>,
    pub had_failure: bool,
}

impl ExecAccumulator {
    pub fn new() -> Self {
        Self {
            tool_calls: Vec::new(),
            images: Vec::new(),
            combined_output: String::new(),
            final_response: String::new(),
            exec_error: None,
            had_failure: false,
        }
    }
}

#[allow(dead_code)]
fn extract_exec_error_summary(raw: &str) -> String {
    let mut cleaned = raw.trim();
    if let Some(rest) = cleaned.strip_prefix("Runtime error:") {
        cleaned = rest.trim();
    }

    let lines: Vec<&str> = cleaned
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return "Execution failed".to_string();
    }

    // Prefer the terminal Python exception line (e.g. NameError: ...).
    for line in lines.iter().rev() {
        if line.starts_with("Traceback")
            || line.starts_with("File ")
            || line.starts_with("During handling of the above exception")
        {
            continue;
        }
        if line.contains(':') {
            return (*line).to_string();
        }
    }

    lines.last().unwrap_or(&"Execution failed").to_string()
}

#[allow(dead_code)]
fn format_exec_error_for_output(raw: &str) -> String {
    if matches!(
        std::env::var("LASH_VERBOSE_RUNTIME_ERRORS")
            .ok()
            .as_deref()
            .map(|v| v.to_ascii_lowercase())
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    ) {
        return raw.trim().to_string();
    }
    extract_exec_error_summary(raw)
}

/// Execute a code block, emit events, and collect results into the accumulator.
#[allow(dead_code)]
pub(crate) async fn execute_and_collect(
    session: &mut Session,
    code: &str,
    acc: &mut ExecAccumulator,
    event_tx: &mpsc::Sender<AgentEvent>,
) {
    match session.run_code(code).await {
        Ok(r) => {
            for tc in &r.tool_calls {
                send_event(
                    event_tx,
                    AgentEvent::ToolCall {
                        name: tc.tool.clone(),
                        args: tc.args.clone(),
                        result: tc.result.clone(),
                        success: tc.success,
                        duration_ms: tc.duration_ms,
                    },
                )
                .await;
                if tc.tool == "agent_result"
                    && let Some(sub) = tc.result.get("_sub_agent")
                {
                    let task = sub
                        .get("task")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let usage: TokenUsage = sub
                        .get("usage")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();
                    let tc_count =
                        sub.get("tool_calls").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let iters =
                        sub.get("iterations").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    send_event(
                        event_tx,
                        AgentEvent::SubAgentDone {
                            task,
                            usage,
                            tool_calls: tc_count,
                            iterations: iters,
                            success: tc.success,
                        },
                    )
                    .await;
                }
            }
            if !r.output.is_empty() || r.error.is_some() {
                let normalized_error = r.error.as_deref().map(format_exec_error_for_output);
                send_event(
                    event_tx,
                    AgentEvent::CodeOutput {
                        output: r.output.clone(),
                        error: normalized_error,
                    },
                )
                .await;
            }

            acc.tool_calls.extend(r.tool_calls);
            acc.images.extend(r.images);
            if !r.output.is_empty() {
                acc.combined_output.push_str(&r.output);
            }
            if !r.response.is_empty() {
                acc.final_response = r.response;
            }
            if let Some(raw_error) = r.error {
                tracing::debug!("runtime execution error (raw): {}", raw_error);
                acc.exec_error = Some(format_exec_error_for_output(&raw_error));
                acc.had_failure = true;
            }
        }
        Err(e) => {
            let raw_error = format!("{}", e);
            tracing::debug!("runtime execution error (raw): {}", raw_error);
            send_event(
                event_tx,
                AgentEvent::CodeOutput {
                    output: String::new(),
                    error: Some(format_exec_error_for_output(&raw_error)),
                },
            )
            .await;
            acc.exec_error = Some(format_exec_error_for_output(&raw_error));
            acc.had_failure = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::extract_exec_error_summary;

    #[test]
    fn summarizes_python_traceback_to_exception_line() {
        let raw = "Runtime error: Traceback (most recent call last):\n  File \"repl_1.py\", line 2, in <module>\nNameError: name 'now' is not defined";
        assert_eq!(
            extract_exec_error_summary(raw),
            "NameError: name 'now' is not defined"
        );
    }

    #[test]
    fn keeps_single_line_error() {
        let raw = "ValueError: bad input";
        assert_eq!(extract_exec_error_summary(raw), "ValueError: bad input");
    }
}
