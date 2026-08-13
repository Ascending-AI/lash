//! Provider-confirmed closure of the RLM-owned response boundary.
//!
//! RLM replaces caller stop sequences with the single Lashlang closing tag.
//! A provider-specific `stop_sequence` finish reason therefore proves that
//! the withheld delimiter is this protocol's delimiter. A generic `Stop`
//! does not: it may also mean a natural end of generation.

use lash_core::{GenerationOptionDisposition, LlmResponse, LlmTerminalReason};

use crate::cell_scan::{
    LASHLANG_END_TAG, complete_lashlang_start_tag_span, first_lashlang_cell_span,
};

pub(crate) fn stopped_at_owned_cell_boundary(response: &LlmResponse) -> bool {
    if response.terminal_reason != LlmTerminalReason::Stop {
        return false;
    }

    let stop_was_sent = response
        .generation_disposition
        .as_ref()
        .is_some_and(|disposition| {
            matches!(
                disposition.stop_sequences,
                GenerationOptionDisposition::Applied
                    | GenerationOptionDisposition::ReplacedProtocolOwned
            )
        });
    if !stop_was_sent {
        return false;
    }

    response
        .execution_evidence
        .as_ref()
        .and_then(|evidence| evidence.provider_finish_reason.as_deref())
        .is_some_and(|reason| reason.eq_ignore_ascii_case("stop_sequence"))
}

pub(crate) fn close_unclosed_owned_cell(text: &str) -> Option<String> {
    if first_lashlang_cell_span(text).is_some() || complete_lashlang_start_tag_span(text).is_none()
    {
        return None;
    }

    let mut closed = text.to_string();
    if !closed.ends_with('\n') && !closed.ends_with('\r') {
        closed.push('\n');
    }
    closed.push_str(LASHLANG_END_TAG);
    Some(closed)
}
