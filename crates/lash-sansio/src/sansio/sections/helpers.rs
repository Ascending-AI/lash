fn token_usage_from_llm_usage(usage: &crate::llm::types::LlmUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
    }
}

/// Ingress seam for a provider's raw usage counters.
///
/// Everything downstream aggregates these counters on the assumption that they
/// sum in range: the context-window refinement below, the checked cumulative
/// merge in `record_llm_usage`, the durable turn commit, and host-side bare
/// sums such as `LlmUsage::total`. Validate both aggregations once here, with a
/// typed error, so no consumer performs unchecked arithmetic on provider input.
///
/// Returns the kernel's usage value together with the validated prompt-side
/// subtotal.
fn checked_turn_usage_from_llm_usage(
    usage: &crate::llm::types::LlmUsage,
) -> Result<(TokenUsage, i64), TokenUsageOverflow> {
    let usage = token_usage_from_llm_usage(usage);
    usage.checked_total()?;
    let input_total = usage.checked_input_total()?;
    Ok((usage, input_total))
}

/// Reclassify a zero-output `OutputLimit` terminal reason as `ContextOverflow`
/// when the prompt nearly filled the model's context window.
///
/// Pure policy: the kernel owns the terminal-reason interpretation, so the
/// provider's raw reason is refined here (before it drives the finish decision
/// in `handle_terminal_llm_response`) rather than in the host I/O layer. A
/// `None` window disables the refinement. `prompt_input_tokens` is the
/// prompt-side subtotal already validated by
/// [`checked_turn_usage_from_llm_usage`].
fn refine_terminal_reason_for_context_window(
    response: &mut LlmResponse,
    prompt_input_tokens: i64,
    max_context_tokens: Option<usize>,
) {
    if response.terminal_reason != LlmTerminalReason::OutputLimit {
        return;
    }
    if response.usage.output_tokens != 0 {
        return;
    }
    let Some(max_context_tokens) = max_context_tokens.filter(|value| *value > 0) else {
        return;
    };
    let prompt_tokens = prompt_input_tokens.max(0) as usize;
    if prompt_tokens >= max_context_tokens.saturating_mul(95) / 100 {
        response.terminal_reason = LlmTerminalReason::ContextOverflow;
        response.terminal_diagnostic = Some(
            "Model produced no output because the prompt reached the configured context window."
                .to_string(),
        );
    }
}
