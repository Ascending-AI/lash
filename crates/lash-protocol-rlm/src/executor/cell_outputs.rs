//! A cell records its complete print array inline or as one attachment archive.

use super::*;

/// Outputs prepared for the cell's answer. The enclosing cell/turn commit
/// publishes them with the rest of the answer; they have no separate journal.
struct CellOutputs {
    prints: Vec<lash_core::CellPrint>,
    prints_retained: Option<lash_core::RetainedValue>,
    finish_retained: Option<lash_core::RetainedValue>,
}

fn encoding_failed(error: &serde_json::Error) -> lash_core::RuntimeEffectControllerError {
    lash_core::RuntimeEffectControllerError::new(
        lash_core::RuntimeErrorCode::RecordEncodingFailed,
        error.to_string(),
    )
}

/// Renders prints and retains oversized values before the cell's answer is
/// consumed by its commit owner. Attachment errors stop the cell through the
/// enclosing execution's typed failure path.
pub(super) async fn prepare_cell_outputs(
    ctx: &RuntimeExecutionContext<'_>,
    cell: &cell_run::CellRun,
    code_renderer: &crate::render::CodeRendererSlot,
    events: &crate::projection::EventDecoder,
    values: Vec<lash_kernel_doc::Datum>,
    response: &mut ExecResponse,
) {
    // Only prints are rendered: a cell that finished without printing needs
    // no recorded renderer.
    let params = if values.is_empty() {
        None
    } else {
        let recorded = lash_core::RecordedRender::require_available(
            ctx.recorded_render(),
            code_renderer.0.id(),
        );
        match recorded.and_then(|recorded| {
            <crate::render::ResolvedRlmRender as serde::Deserialize>::deserialize(
                recorded.params.as_ref(),
            )
            .map_err(|_| lash_core::RuntimeErrorCode::RecordedRendererUnavailable)
        }) {
            Ok(params) => Some(params),
            Err(code) => {
                let error = lash_core::RuntimeEffectControllerError::new(
                    code,
                    "recorded RLM renderer or parameters are unavailable",
                );
                fail_cell_on_nested_error(ctx, response, error);
                return;
            }
        }
    };
    let renderer = code_renderer.0.as_ref();
    let history_index = match crate::projection::HistoryIndex::from_context(ctx, events) {
        Ok(history) => history.len(),
        Err(corruption) => {
            let mut error = lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::RecordEncodingFailed,
                corruption.to_string(),
            );
            error.cause = Some(lash_core::RuntimeErrorCause::StoredDataCorrupt {
                corruption: Box::new(corruption),
            });
            fail_cell_on_nested_error(ctx, response, error);
            return;
        }
    };
    let label = cell.outputs_label();
    let attachments = ctx.attachment_store();
    let finish_value = response.finish_value().cloned();
    let outcome: Result<CellOutputs, lash_core::RuntimeEffectControllerError> = async {
        let policy = attachments.output_retention();
        let mut observations = Vec::new();
        if let Some(params) = &params {
            for value in values {
                let typed = datum_json(&value);
                let observation = crate::render::rendered_print(
                    renderer,
                    &typed,
                    &params.print,
                    history_index,
                    observations.len(),
                );
                observations.push(observation);
            }
        }
        // The prints are retained when history could not hold them
        // inline, and the archive holds what a program reads of
        // them: their values, in order.
        let inline_bytes = serde_json::to_vec(&observations)
            .map_err(|error| encoding_failed(&error))?
            .len();
        let prints_retained = if observations.is_empty() || !policy.retains(inline_bytes) {
            None
        } else {
            let values = serde_json::Value::Array(
                observations
                    .drain(..)
                    .map(|observation| observation.value)
                    .collect(),
            );
            let encoded =
                serde_json::to_string(&values).map_err(|error| encoding_failed(&error))?;
            Some(
                retain_value(
                    &attachments,
                    policy,
                    &values,
                    encoded,
                    &format!("{label}:prints"),
                )
                .await?,
            )
        };
        let finish_retained = match &finish_value {
            Some(value) => {
                let encoded =
                    serde_json::to_string(value).map_err(|error| encoding_failed(&error))?;
                if policy.retains(encoded.len()) {
                    Some(
                        retain_value(
                            &attachments,
                            policy,
                            value,
                            encoded,
                            &format!("{label}:final"),
                        )
                        .await?,
                    )
                } else {
                    None
                }
            }
            None => None,
        };
        Ok(CellOutputs {
            prints: observations,
            prints_retained,
            finish_retained,
        })
    }
    .await;
    match outcome {
        Ok(outputs) => {
            response.prints = outputs.prints;
            response.prints_retained = outputs.prints_retained;
            // History records the retention in a too-long finish value's
            // place; the value itself stays the turn's answer.
            if let Some(retained) = outputs.finish_retained
                && let lash_core::CellOutcome::Controlled {
                    control: lash_core::CellControl::Finish { value },
                    ..
                } = &mut response.result
            {
                *value = lash_core::OutputValue::Retained(retained);
                response.retained_finish_value = finish_value;
            }
        }
        Err(error) => fail_cell_on_nested_error(ctx, response, error),
    }
}

/// Retains `value` out of history: its JSON `encoded` is put as a session
/// attachment, and history keeps the value's shape, for a program to read in
/// its place, and the witness — the encoding's first bytes and a notice
/// naming the attachment — with its reference.
async fn retain_value(
    attachments: &lash_core::facade_support::RuntimeAttachmentStore,
    policy: lash_core::OutputRetentionPolicy,
    value: &serde_json::Value,
    encoded: String,
    label: &str,
) -> Result<lash_core::RetainedValue, lash_core::RuntimeEffectControllerError> {
    let meta = lash_core::AttachmentCreateMeta::new(
        lash_core::MediaType::parse("application/json").map_err(|error| {
            lash_core::RuntimeEffectControllerError::output_retention_failed(
                &lash_core::AttachmentStoreError::Contract(format!(
                    "the retained-output media type is fixed and cannot parse: {error}"
                )),
            )
        })?,
        None,
        Some(label.to_string()),
    );
    let byte_len = encoded.len();
    let reference = attachments
        .put(encoded.clone().into_bytes(), meta)
        .await
        .map_err(|error| {
            lash_core::RuntimeEffectControllerError::output_retention_failed(&error)
        })?;
    let notice = format!(
        "…[value retained: {byte_len} bytes exceed the {}-byte history limit; full value: attachment {}]",
        policy.inline_limit_bytes, reference.id
    );
    Ok(lash_core::RetainedValue {
        output: lash_core::RetainedOutput {
            witness: policy.witness(&encoded, &notice),
            reference,
        },
        shape: lash_core::ValueShape::of_with_limits(value, policy.shape),
    })
}
