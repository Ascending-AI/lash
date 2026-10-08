//! A cell records its complete print array inline or as one attachment archive.

use super::*;

/// The journaled record of what a cell put into history (FIG-1643): its
/// inline prints or one archive for their aggregate, and a retained terminal
/// value when needed, under the retention
/// policy the step read, which it records too.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedCellOutputs {
    policy: lash_core::OutputRetentionPolicy,
    prints: Vec<lash_core::CellPrint>,
    prints_retained: Option<lash_core::RetainedOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finish_retained: Option<lash_core::RetainedOutput>,
}

/// Renders the cell's prints and retains their aggregate when too long for history,
/// once, inside one journaled step keyed under the cell (`{cell}:outputs`).
///
/// The step runs for every cell that printed or finished — never on whether
/// something is oversized — so a replay under a changed policy issues the
/// same step and is served the recorded observations, witnesses and
/// references verbatim. A retention that fails is the step's typed
/// attachment-store error: the cell stops, and the outputs never
/// enter history in its place.
pub(super) async fn record_cell_outputs(
    ctx: &RuntimeExecutionContext<'_>,
    cell: &cell_run::CellRun,
    code_renderer: &crate::render::CodeRendererSlot,
    values: Vec<lashlang::Value>,
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
            serde_json::from_value::<crate::render::ResolvedRlmRender>(recorded.params.clone())
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
    let renderer = Arc::clone(&code_renderer.0);
    let history_index =
        match crate::projection::rlm_history_projection(ctx.chronological_projection().as_ref()) {
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
    let namespace = cell
        .identities()
        .namespace()
        .as_str()
        .trim_end_matches(":lk2")
        .to_string();
    let key = format!("{namespace}:outputs");
    let attachments = ctx.attachment_store();
    let finish_value = response.finish_value().cloned();
    let recorded_finish = finish_value.clone();
    let outcome = ctx
        .journaled_language_value_with(
            key.clone(),
            format!("rlm.outputs:{}", renderer.id()),
            move || async move {
                let policy = attachments.output_retention();
                let mut observations = Vec::new();
                if let Some(params) = &params {
                    for value in values {
                        let observation = crate::render::rendered_print(
                            renderer.as_ref(),
                            &value,
                            &params.print,
                            history_index,
                            observations.len(),
                            flow_to_json_value(&value),
                        );
                        observations.push(observation);
                    }
                }
                let prints_retained = if observations.is_empty() {
                    None
                } else {
                    retain_oversized_value(
                        &attachments,
                        policy,
                        &serde_json::to_value(&observations).map_err(|error| {
                            lash_core::RuntimeEffectControllerError::new(
                                lash_core::RuntimeErrorCode::RecordEncodingFailed,
                                error.to_string(),
                            )
                        })?,
                        &format!("{key}:prints"),
                    )
                    .await?
                };
                if prints_retained.is_some() {
                    observations.clear();
                }
                let finish_retained = match &recorded_finish {
                    Some(value) => {
                        retain_oversized_value(&attachments, policy, value, &format!("{key}:final"))
                            .await?
                    }
                    None => None,
                };
                serde_json::to_value(RecordedCellOutputs {
                    policy,
                    prints: observations,
                    prints_retained,
                    finish_retained,
                })
                .map_err(|error| {
                    lash_core::RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::RecordEncodingFailed,
                        error.to_string(),
                    )
                })
            },
        )
        .await;
    match outcome {
        Ok(value) => match serde_json::from_value::<RecordedCellOutputs>(value) {
            Ok(recorded) => {
                response.prints = recorded.prints;
                response.prints_retained = recorded.prints_retained;
                // History records the retention in a too-long finish value's
                // place; the value itself stays the turn's answer.
                if let Some(retained) = recorded.finish_retained {
                    response.result =
                        lash_core::CellResult::Finished(lash_core::OutputValue::Retained(retained));
                    response.retained_finish_value = finish_value;
                }
            }
            Err(error) => fail_cell(
                response,
                lash_core::CellFailure::new(lash_core::CellFailureKind::Host, error.to_string()),
            ),
        },
        Err(error) => fail_cell_on_nested_error(ctx, response, error),
    }
}

/// Retains `value` when its JSON encoding is longer than `policy` allows in
/// history: the encoding is put as a session attachment, and the answer is
/// the witness — the encoding's first bytes and a notice naming the
/// attachment — with its reference. `None` for a value history keeps inline.
pub(super) async fn retain_oversized_value(
    attachments: &lash_core::facade_support::RuntimeAttachmentStore,
    policy: lash_core::OutputRetentionPolicy,
    value: &serde_json::Value,
    label: &str,
) -> Result<Option<lash_core::RetainedOutput>, lash_core::RuntimeEffectControllerError> {
    let encoded = serde_json::to_string(value).map_err(|error| {
        lash_core::RuntimeEffectControllerError::new(
            lash_core::RuntimeErrorCode::RecordEncodingFailed,
            error.to_string(),
        )
    })?;
    if !policy.retains(encoded.len()) {
        return Ok(None);
    }
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
    Ok(Some(lash_core::RetainedOutput {
        witness: policy.witness(&encoded, &notice),
        reference,
    }))
}
