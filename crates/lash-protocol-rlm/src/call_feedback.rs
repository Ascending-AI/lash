//! What a cell's next prompt shows of the host calls it completed (FIG-6293).
//!
//! A failed cell does not undo the host calls it completed before it failed.
//! The model repairing the cell has to know which calls those were, with
//! their durable ids and what they answered, or it repeats them. Each call's
//! result is projected from its committed [`lash_core::ToolCallRecord`] when
//! the cell's record commits ([`with_result_summaries`]), as a bounded prefix
//! of its canonical compact JSON, and rendered from that record on every
//! later projection ([`write_calls_block`]). The record under the call's id
//! stays the result's one source of truth. A summary is a presentation
//! projection, not a second results store.
//!
//! Both cuts are the session's recorded [`CallFeedbackPolicy`], so a reopen,
//! a redrive or another owner renders the same bytes whatever the live
//! deployment is configured with. A cell seeded into a frame from history
//! that recorded no summary renders `"result_summary":null`. That is not
//! evidence the call failed or produced nothing.

use std::fmt::Write as _;

/// The calls block's two byte cuts, recorded with the session's
/// presentation. They are presentation limits, not retention limits: the
/// committed ledger keeps every result whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    try_from = "CallFeedbackPolicySpelling",
    into = "CallFeedbackPolicySpelling"
)]
pub struct CallFeedbackPolicy {
    max_result_bytes: usize,
    max_block_bytes: usize,
}

/// A [`CallFeedbackPolicy`] as it is recorded: both cuts, unchecked.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CallFeedbackPolicySpelling {
    max_result_bytes: usize,
    max_block_bytes: usize,
}

/// Why a [`CallFeedbackPolicy`] was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CallFeedbackPolicyError {
    /// One result's text could not fit in the block that renders it.
    #[error(
        "a call result's {max_result_bytes} bytes exceed the calls block's {max_block_bytes} bytes"
    )]
    ResultExceedsBlock {
        max_result_bytes: usize,
        max_block_bytes: usize,
    },
    /// The block may not take more than the whole composed prompt may.
    #[error("the calls block's {max_block_bytes} bytes exceed the {ceiling}-byte ceiling")]
    BlockExceedsCeiling {
        max_block_bytes: usize,
        ceiling: usize,
    },
}

impl CallFeedbackPolicy {
    /// The most bytes a calls block may take: the standard composed prompt's
    /// whole budget (`PromptLimits::standard`, 256 KiB).
    pub const MAX_BLOCK_BYTES: usize = 256 * 1024;

    /// Standard preset: 512 bytes of each result's text and 16 KiB for one
    /// cell's whole block.
    pub const fn standard() -> Self {
        Self {
            max_result_bytes: 512,
            max_block_bytes: 16 * 1024,
        }
    }

    /// A policy cutting each result's text at `max_result_bytes` and a
    /// cell's whole block at `max_block_bytes`. Zero is a valid cut: it
    /// renders every call as omitted.
    ///
    /// # Errors
    ///
    /// [`CallFeedbackPolicyError`] when the result cut exceeds the block
    /// cut, or the block cut exceeds [`Self::MAX_BLOCK_BYTES`].
    pub const fn new(
        max_result_bytes: usize,
        max_block_bytes: usize,
    ) -> Result<Self, CallFeedbackPolicyError> {
        if max_block_bytes > Self::MAX_BLOCK_BYTES {
            return Err(CallFeedbackPolicyError::BlockExceedsCeiling {
                max_block_bytes,
                ceiling: Self::MAX_BLOCK_BYTES,
            });
        }
        if max_result_bytes > max_block_bytes {
            return Err(CallFeedbackPolicyError::ResultExceedsBlock {
                max_result_bytes,
                max_block_bytes,
            });
        }
        Ok(Self {
            max_result_bytes,
            max_block_bytes,
        })
    }

    /// The bytes of one result's text a summary keeps.
    pub const fn max_result_bytes(&self) -> usize {
        self.max_result_bytes
    }

    /// The bytes one cell's encoded calls object may take, its framing and
    /// omission counts included.
    pub const fn max_block_bytes(&self) -> usize {
        self.max_block_bytes
    }
}

impl Default for CallFeedbackPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

impl TryFrom<CallFeedbackPolicySpelling> for CallFeedbackPolicy {
    type Error = CallFeedbackPolicyError;

    fn try_from(spelling: CallFeedbackPolicySpelling) -> Result<Self, Self::Error> {
        Self::new(spelling.max_result_bytes, spelling.max_block_bytes)
    }
}

impl From<CallFeedbackPolicy> for CallFeedbackPolicySpelling {
    fn from(policy: CallFeedbackPolicy) -> Self {
        Self {
            max_result_bytes: policy.max_result_bytes,
            max_block_bytes: policy.max_block_bytes,
        }
    }
}

/// What a failed cell's observation adds after its failure when it
/// completed host calls.
pub(crate) const FAILED_CELL_CALLS_GUIDANCE: &str = "The completed calls above remain completed even though this cell failed. Do not repeat successful effecting calls when repairing the cell. A failed or interrupted call may still have produced effects; inspect its recorded outcome before deciding to retry.";

/// `calls` with each call's result summary projected from its committed
/// record in `records`, cut at `max_result_bytes`. A call with no committed
/// record keeps no summary.
pub(crate) fn with_result_summaries(
    mut calls: Vec<lash_core::ExecutedCall>,
    records: &[lash_core::ToolCallRecord],
    max_result_bytes: usize,
) -> Vec<lash_core::ExecutedCall> {
    for call in &mut calls {
        call.result_summary = call.call_id.as_ref().and_then(|id| {
            records
                .iter()
                .find(|record| &record.call_id == id)
                .map(|record| summarize(&record.output.value_for_projection(), max_result_bytes))
        });
    }
    calls
}

/// `value` as canonical compact JSON, cut to its longest UTF-8 prefix of at
/// most `max_bytes` bytes.
fn summarize(value: &serde_json::Value, max_bytes: usize) -> lash_core::CallResultSummary {
    let mut text = String::new();
    write_canonical_json(value, &mut text);
    let truncated = text.len() > max_bytes;
    if truncated {
        let mut end = max_bytes;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    lash_core::CallResultSummary { text, truncated }
}

/// Compact JSON with every object's keys in byte order, whatever map order
/// the value was built with. Strings and numbers use serde_json's spelling.
fn write_canonical_json(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical_json(item, out);
            }
            out.push(']');
        }
        serde_json::Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(key, _)| *key);
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&json_string(key));
                out.push(':');
                write_canonical_json(item, out);
            }
            out.push('}');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

fn json_string(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}

/// One call as the block lists it.
#[derive(serde::Serialize)]
struct Row<'a> {
    tool: &'a str,
    call_id: Option<&'a lash_core::ToolCallId>,
    outcome: &'static str,
    result_summary: Option<&'a lash_core::CallResultSummary>,
}

impl<'a> Row<'a> {
    fn of(call: &'a lash_core::ExecutedCall) -> Self {
        Self {
            tool: &call.operation,
            call_id: call.call_id.as_ref(),
            outcome: call.outcome.as_str(),
            result_summary: call.result_summary.as_ref(),
        }
    }
}

/// The block's framing with `omitted` counts spelled at their widest:
/// what it takes before any row, whatever is omitted.
fn framing(total: usize) -> String {
    format!("{{\"completed_calls\":[],\"omitted\":{{\"ok\":{total},\"err\":{total}}}}}")
}

/// Append `calls`' block to `out`: one compact JSON object listing whole
/// rows, each call's tool path, durable id, outcome and result summary, and
/// counting the rows it leaves out.
///
/// Rows are chosen deterministically: successful calls first, then failed
/// ones, each in admission order, a row kept while it is within `max_calls`
/// and the block's byte cut and skipped whole otherwise. The kept rows are
/// listed in admission order. A row is never cut, so a call id is never
/// shortened into another identity. The framing, its omission counts spelled
/// at their widest, is fixed overhead: a cut smaller than it renders the
/// framing alone, every call counted as omitted.
pub(crate) fn write_calls_block(
    out: &mut String,
    calls: &[lash_core::ExecutedCall],
    max_calls: usize,
    policy: CallFeedbackPolicy,
) {
    let rows = calls
        .iter()
        .map(|call| serde_json::to_string(&Row::of(call)).unwrap_or_default())
        .collect::<Vec<_>>();
    let mut room = policy
        .max_block_bytes
        .saturating_sub(framing(calls.len()).len());
    let mut kept = vec![false; calls.len()];
    let mut kept_count = 0usize;
    let (mut omitted_ok, mut omitted_err) = (0usize, 0usize);
    let selection = [true, false].into_iter().flat_map(|ok| {
        calls
            .iter()
            .enumerate()
            .filter(move |(_, call)| (call.outcome == lash_core::ExecutedCallOutcome::Ok {}) == ok)
            .map(|(index, _)| index)
    });
    for index in selection {
        let cost = rows[index].len() + usize::from(kept_count > 0);
        if kept_count < max_calls && cost <= room {
            room -= cost;
            kept[index] = true;
            kept_count += 1;
        } else if calls[index].outcome == (lash_core::ExecutedCallOutcome::Ok {}) {
            omitted_ok += 1;
        } else {
            omitted_err += 1;
        }
    }
    out.push_str("{\"completed_calls\":[");
    for (position, row) in rows
        .iter()
        .zip(&kept)
        .filter_map(|(row, kept)| kept.then_some(row))
        .enumerate()
    {
        if position > 0 {
            out.push(',');
        }
        out.push_str(row);
    }
    let _ = write!(
        out,
        "],\"omitted\":{{\"ok\":{omitted_ok},\"err\":{omitted_err}}}}}"
    );
}

#[cfg(test)]
mod tests;
