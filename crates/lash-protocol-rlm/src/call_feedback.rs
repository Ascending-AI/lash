//! What a cell's next prompt shows of the host calls it completed (FIG-6293).
//!
//! A failed cell does not undo the host calls it completed before it failed.
//! The model repairing the cell has to know which calls those were, with
//! their durable ids and what they answered, or it repeats them. Each call's
//! result is projected from its committed [`lash_core::ToolCallRecord`] when
//! the cell's record commits ([`with_result_summaries`]), as a bounded
//! structural preview, and rendered from that record on every
//! later projection ([`write_calls_block`]). The record under the call's id
//! stays the result's one source of truth. A summary is a presentation
//! projection, not a second results store.
//!
//! Both bounds are the session's recorded [`CallFeedbackPolicy`], so a reopen,
//! a redrive or another owner renders the same bytes whatever the live
//! deployment is configured with. A cell seeded into a frame from history
//! that recorded no summary renders `"result_summary":null`. That is not
//! evidence the call failed or produced nothing.

use std::fmt::Write as _;

/// The calls block's two byte bounds, recorded with the session's
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
    /// Every block must be able to report omitted calls by whole endpoint ids.
    #[error(
        "the calls block's {max_block_bytes} bytes are below the {minimum}-byte omission-notice minimum"
    )]
    BlockBelowMinimum {
        max_block_bytes: usize,
        minimum: usize,
    },
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

    /// Enough bytes for empty call groups, maximum-width omission counts,
    /// and the mandatory notice with two longest Lash-issued call ids.
    pub const MIN_BLOCK_BYTES: usize = BLOCK_FRAMING.len()
        + 2 * COUNT_BYTES
        + 1
        + NOTICE_START.len()
        + COUNT_BYTES
        + NOTICE_EARLIEST.len()
        + 2 * lash_core::ToolCallId::ENCODED_BYTES
        + NOTICE_LATEST.len()
        + 1;

    /// Standard preset: 512 bytes of each result's text and 16 KiB for one
    /// cell's whole block.
    pub const fn standard() -> Self {
        Self {
            max_result_bytes: 512,
            max_block_bytes: 16 * 1024,
        }
    }

    /// Bound structural result previews and the whole calls block. A zero
    /// result budget omits results while preserving operation and call id.
    /// The block must reserve at least [`Self::MIN_BLOCK_BYTES`] for an
    /// honest omission notice.
    ///
    /// # Errors
    ///
    /// [`CallFeedbackPolicyError`] when the result cut exceeds the block
    /// cut, or the block is outside [`Self::MIN_BLOCK_BYTES`] through
    /// [`Self::MAX_BLOCK_BYTES`].
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
        if max_block_bytes < Self::MIN_BLOCK_BYTES {
            return Err(CallFeedbackPolicyError::BlockBelowMinimum {
                max_block_bytes,
                minimum: Self::MIN_BLOCK_BYTES,
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
/// record in `records`, bounded by `max_result_bytes`. A call with no committed
/// record keeps no summary.
pub(crate) fn with_result_summaries(
    mut calls: Vec<lash_core::ExecutedCall>,
    records: &[lash_core::ToolCallRecord],
    max_result_bytes: usize,
) -> Vec<lash_core::ExecutedCall> {
    for call in &mut calls {
        call.result_summary = call
            .call_id
            .as_ref()
            .filter(|_| max_result_bytes > 0)
            .and_then(|id| {
                records
                    .iter()
                    .find(|record| &record.call_id == id)
                    .map(|record| {
                        summarize(&record.output.value_for_projection(), max_result_bytes)
                    })
            });
    }
    calls
}

/// A structural preview within `max_bytes`. Whole scalar spellings survive;
/// long strings lose their entire contents, since any prefix might be half
/// an identifier. The length marker makes that shortening explicit.
fn summarize(value: &serde_json::Value, max_bytes: usize) -> lash_core::CallResultSummary {
    structural_preview(value, max_bytes).unwrap_or_else(|| lash_core::CallResultSummary {
        text: if max_bytes >= "…".len() {
            "…".into()
        } else {
            String::new()
        },
        truncated: true,
    })
}

fn structural_preview(
    value: &serde_json::Value,
    budget: usize,
) -> Option<lash_core::CallResultSummary> {
    let mut whole = String::new();
    write_canonical_json(value, &mut whole);
    if whole.len() <= budget {
        return Some(lash_core::CallResultSummary {
            text: whole,
            truncated: false,
        });
    }
    if let serde_json::Value::String(text) = value {
        let shortened = json_string(&format!("… ({} characters)", text.chars().count()));
        return (shortened.len() <= budget).then_some(lash_core::CallResultSummary {
            text: shortened,
            truncated: true,
        });
    }
    let (open, close, noun, members) = match value {
        serde_json::Value::Array(items) => (
            '[',
            ']',
            "items",
            items
                .iter()
                .map(|value| (String::new(), value))
                .collect::<Vec<_>>(),
        ),
        serde_json::Value::Object(map) => {
            let mut entries = map.iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(key, _)| *key);
            (
                '{',
                '}',
                "members",
                entries
                    .into_iter()
                    .map(|(key, value)| (format!("{}:", json_string(key)), value))
                    .collect(),
            )
        }
        _ => return None,
    };
    let elision = |count| format!("… ({count} {noun} elided)");
    if 2 + elision(members.len()).len() > budget {
        return None;
    }
    let mut text = open.to_string();
    let mut kept = 0;
    for (index, (key, value)) in members.iter().enumerate() {
        let remaining = members.len() - index - 1;
        let tail = if remaining == 0 {
            0
        } else {
            1 + elision(remaining).len()
        };
        let overhead = text.len() + usize::from(kept > 0) + key.len() + tail + 1;
        let Some(available) = budget.checked_sub(overhead) else {
            break;
        };
        // Share the available bytes so a large early body cannot displace
        // every later identity. Unused room remains available to later members.
        let Some(preview) = structural_preview(value, available / (remaining + 1))
            .or_else(|| structural_preview(value, available))
        else {
            break;
        };
        if kept > 0 {
            text.push(',');
        }
        text.push_str(key);
        text.push_str(&preview.text);
        kept += 1;
    }
    if kept < members.len() {
        if kept > 0 {
            text.push(',');
        }
        text.push_str(&elision(members.len() - kept));
    }
    text.push(close);
    Some(lash_core::CallResultSummary {
        text,
        truncated: true,
    })
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

/// One call as the block lists it. All leaves are serialized whole.
fn row(call: &lash_core::ExecutedCall) -> String {
    serde_json::json!({
        "tool": call.operation,
        "call_id": call.call_id,
        "outcome": call.outcome.as_str(),
        "result_summary": call.result_summary,
    })
    .to_string()
}

const BLOCK_FRAMING: &str =
    "{\"completed_calls\":[],\"failed_calls\":[],\"omitted\":{\"ok\":,\"err\":}}";
const COUNT_BYTES: usize = usize::MAX.ilog10() as usize + 1;
const NOTICE_START: &str = "Omitted ";
const NOTICE_EARLIEST: &str = " completed calls; earliest call_id=";
const NOTICE_LATEST: &str = "; latest call_id=";

fn is_completed(call: &lash_core::ExecutedCall) -> bool {
    matches!(call.outcome, lash_core::ExecutedCallOutcome::Ok {})
}

fn calls_json(calls: &[lash_core::ExecutedCall], rows: &[String], kept: &[bool]) -> String {
    let mut out = String::from("{\"completed_calls\":[");
    for (completed, header) in [(true, ""), (false, "],\"failed_calls\":[")] {
        out.push_str(header);
        let mut separator = "";
        for ((call, row), kept) in calls.iter().zip(rows).zip(kept) {
            if *kept && is_completed(call) == completed {
                out.push_str(separator);
                out.push_str(row);
                separator = ",";
            }
        }
    }
    let omitted_ok = calls
        .iter()
        .zip(kept)
        .filter(|(call, kept)| !**kept && is_completed(call))
        .count();
    let omitted_err = calls
        .iter()
        .zip(kept)
        .filter(|(call, kept)| !**kept && !is_completed(call))
        .count();
    let _ = write!(
        out,
        "],\"omitted\":{{\"ok\":{omitted_ok},\"err\":{omitted_err}}}}}"
    );
    out
}

fn call_id(call: &lash_core::ExecutedCall) -> &str {
    call.call_id
        .as_ref()
        .map_or("none", lash_core::ToolCallId::as_str)
}

fn omission_notice(omitted: &[&lash_core::ExecutedCall], budget: usize) -> String {
    let mut notice = format!("{NOTICE_START}{} completed calls: ", omitted.len());
    for (index, call) in omitted.iter().enumerate() {
        let name = format!(
            "{} (call_id={})",
            json_string(&call.operation),
            call_id(call)
        );
        let separator = if index == 0 { "" } else { ", " };
        if notice.len() + separator.len() + name.len() + 1 > budget {
            // The policy floor reserves this entire fallback, even for the
            // largest usize count and two maximum-width Lash-issued ids.
            return match (omitted.first(), omitted.last()) {
                (Some(first), Some(last)) => format!(
                    "{NOTICE_START}{}{NOTICE_EARLIEST}{}{NOTICE_LATEST}{}.",
                    omitted.len(),
                    call_id(first),
                    call_id(last),
                ),
                _ => String::new(),
            };
        }
        notice.push_str(separator);
        notice.push_str(&name);
    }
    notice.push('.');
    notice
}

/// List completed calls separately from failed/interrupted calls. Reserve
/// the mandatory omission notice before selecting rows, keeping successful
/// calls first and admission order within each group. Names and ids are
/// never sliced, including when the notice itself needs its endpoint form.
pub(crate) fn write_calls_block(
    out: &mut String,
    calls: &[lash_core::ExecutedCall],
    max_calls: usize,
    policy: CallFeedbackPolicy,
) {
    let rows = calls.iter().map(row).collect::<Vec<_>>();
    let mut kept = vec![true; calls.len()];
    let full = calls_json(calls, &rows, &kept);
    if calls.len() <= max_calls && full.len() <= policy.max_block_bytes {
        out.push_str(&full);
        return;
    }
    kept.fill(false);
    let mut room = policy.max_block_bytes - CallFeedbackPolicy::MIN_BLOCK_BYTES;
    let mut kept_count = 0;
    for completed in [true, false] {
        for (index, _) in calls
            .iter()
            .enumerate()
            .filter(|(_, call)| is_completed(call) == completed)
        {
            let cost = rows[index].len() + 1;
            if kept_count < max_calls && cost <= room {
                room -= cost;
                kept[index] = true;
                kept_count += 1;
            }
        }
    }
    let mut block = calls_json(calls, &rows, &kept);
    let omitted = calls
        .iter()
        .zip(&kept)
        .filter_map(|(call, kept)| (!kept && is_completed(call)).then_some(call))
        .collect::<Vec<_>>();
    if !omitted.is_empty() {
        let notice = omission_notice(&omitted, policy.max_block_bytes - block.len() - 1);
        block.push('\n');
        block.push_str(&notice);
    }
    out.push_str(&block);
}

#[cfg(test)]
mod tests;
