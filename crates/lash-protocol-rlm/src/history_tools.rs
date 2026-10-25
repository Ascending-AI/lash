//! The protocol's transcript surface. Reads borrow the run's immutable history
//! cut; only the requested items are projected into guest data.
use std::sync::Arc;

use async_trait::async_trait;
use lash_core::{ReadCall, ReadError, ToolCall, ToolProvider, ToolReads};
use lash_vocabulary::tool_contract::{ExecutionPolicy, ToolContract, ToolDefinition, ToolManifest};
use serde_json::{Value, json};

/// Bounds a single answer's item count, so a cell pages large histories instead
/// of making a transcript-sized allocation in its heap. Item bytes remain subject
/// to the ordinary cell bounds.
pub(crate) const MAX_SLICE: usize = 256;

pub(crate) fn register(
    reg: &mut lash_core::plugin::PluginRegistrar<crate::plugin::RlmProtocolPlugin>,
) -> Result<(), lash_execution_contracts::PluginError> {
    let key = lash_core::tool_provider_key!("history");
    reg.tools().provider_with_reads(key, |owner, _| {
        let provider = Arc::new(HistoryTools {
            images: owner.config.prompt_features.images,
        });
        Ok(lash_core::ToolProviderRegistration::new(provider.clone()).with_reads(provider))
    })?;
    reg.package_provider_bound(key, move |owner, _| {
        Ok(Arc::new(lash_tool_support::ToolPackageProvider::new(
            crate::RLM_PROTOCOL_PLUGIN_ID,
            key,
            [lash_vocabulary::names::Segment::new("rlm")
                .unwrap_or_else(|_| unreachable!("static root"))]
            .into(),
            Arc::new(HistoryTools {
                images: owner.config.prompt_features.images,
            }),
        )))
    })
}

pub(crate) struct HistoryTools {
    images: bool,
}

impl Default for HistoryTools {
    fn default() -> Self {
        Self {
            images: crate::protocol::RlmPromptFeatures::default().images,
        }
    }
}

impl HistoryTools {
    #[expect(clippy::expect_used, reason = "fixed tool schemas")]
    fn definitions(images: bool) -> [ToolDefinition; 3] {
        let document = lash_rlm_types::history_item_document(images);
        let mut item = document.root_schema().clone();
        let mut definitions = item
            .as_object_mut()
            .expect("HistoryItem is an object schema")
            .remove("$defs")
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        definitions.insert("HistoryItem".into(), item);
        let item = json!({"$ref":"#/$defs/HistoryItem"});
        [
            ("length", json!({"type":"object", "additionalProperties":false}), json!({"type":"integer"}),
             "Number of entries in the transcript at this cell's start."),
            ("at", json!({"type":"number", "multipleOf":1}), json!({"anyOf":[item.clone(), {"type":"null"}], "$defs":definitions.clone()}),
             "One transcript item by index; negative indices count from the end. Out of range returns null (None in Python)."),
            ("slice", json!({"type":"object", "properties":{"from":{"type":"number", "multipleOf":1},"to":{"type":"number", "multipleOf":1}},"additionalProperties":false}), json!({"type":"array", "items":item, "$defs":definitions}),
             "Transcript items from inclusive from to exclusive to. Omitted bounds mean start/end; negative bounds count from the end and bounds clamp. At most 256 items; page beyond that."),
        ].map(|(member, input, output, description)| {
            ToolDefinition::raw(
                format!("tool:rlm.history.{member}"), format!("rlm.history.{member}"),
                description, input, output,
            ).expect("valid history schemas")
             .with_execution(std::time::Duration::from_secs(30))
             .with_execution_policy(ExecutionPolicy::Read)
        })
    }
}

fn images(
    config: &lash_core::OwnerConfigView,
    fallback: bool,
) -> Result<bool, lash_execution_contracts::PluginError> {
    config
        .decode::<crate::plugin::RlmRecordedConfig>(crate::RLM_PROTOCOL_PLUGIN_ID)
        .map(|recorded| {
            recorded.map_or(fallback, |recorded| {
                recorded.behaviour.prompt_features.images
            })
        })
        .map_err(|error| {
            lash_execution_contracts::PluginError::Session(format!(
                "invalid recorded RLM session config: {error}"
            ))
        })
}

#[async_trait]
impl ToolProvider for HistoryTools {
    fn tool_manifests(
        &self,
        config: &lash_core::OwnerConfigView,
    ) -> Result<Vec<ToolManifest>, lash_execution_contracts::PluginError> {
        Ok(Self::definitions(images(config, self.images)?)
            .into_iter()
            .map(|definition| definition.manifest())
            .collect())
    }
    fn resolve_contract(
        &self,
        name: &str,
        config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<ToolContract>> {
        Self::definitions(images(config, self.images).ok()?)
            .into_iter()
            .find(|definition| definition.manifest.tool() == name)
            .map(|definition| Arc::new(definition.contract()))
    }
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match answer(call.context.read_history(), &call.name(), call.args) {
            Ok(value) => lash_core::ToolOutcome::ok(value).into(),
            Err(error) => lash_core::ToolOutcome::failure(lash_core::ToolFailure::invalid_request(
                error
                    .data
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("history_read"),
                error.message,
            ))
            .into(),
        }
    }
}

#[async_trait]
impl ToolReads for HistoryTools {
    async fn read(&self, call: ReadCall<'_>) -> Result<Value, ReadError> {
        answer(call.history, &call.manifest.tool(), call.args)
    }
}

// The immutable view is the cache key. The value holds no Arc back to the
// view, so its metadata and validated index retire with that run pin.
struct IndexedHistory {
    projection: Arc<lash_core::facade_support::ChronologicalProjection>,
    index: crate::projection::HistoryIndex,
    positions: Vec<usize>,
}

fn history_corrupt(error: &lash_core::StoredDataCorruption) -> ReadError {
    ReadError::new(error.to_string()).with_data(json!({"code":"history_corrupt"}))
}

fn answer(
    history: Option<&lash_core::ReadHistory>,
    operation: &str,
    args: &Value,
) -> Result<Value, ReadError> {
    let history = history.ok_or_else(|| {
        ReadError::new("history requires a cell run")
            .with_data(json!({"code":"history_requires_cell"}))
    })?;
    let indexed = history.derived(|history| {
        #[cfg(test)]
        note_index_build(history);
        let projection = history.projection();
        let index = crate::projection::HistoryIndex::from_projection(
            &projection,
            &crate::projection::EventDecoder,
        )?;
        let positions = index.positions();
        Ok::<_, lash_core::StoredDataCorruption>(IndexedHistory {
            projection,
            index,
            positions,
        })
    });
    let indexed = indexed.as_ref().as_ref().map_err(history_corrupt)?;
    let length = indexed.index.len();
    let item = |i: usize| -> Result<Value, ReadError> {
        let entry = indexed.projection.entries()[indexed.positions[i]].borrowed();
        crate::projection::context::history_item(entry.payload)
            .and_then(|item| {
                serde_json::to_value(item).map_err(|error| lash_core::StoredDataCorruption {
                    record_kind: "RLM history item".into(),
                    message: error.to_string(),
                })
            })
            .map_err(|error| history_corrupt(&error))
    };
    match operation {
        "rlm.history.length" => Ok(json!(length)),
        "rlm.history.at" => {
            let i = integer(args)?;
            let i = if i < 0 {
                (length as i128).saturating_add(i)
            } else {
                i
            };
            if i < 0 || i >= length as i128 {
                Ok(Value::Null)
            } else {
                item(i as usize)
            }
        }
        "rlm.history.slice" => {
            let bound = |name, default| -> Result<usize, ReadError> {
                match args.get(name) {
                    None => Ok(default),
                    Some(value) => {
                        let i = integer(value)?;
                        let i = if i < 0 {
                            (length as i128).saturating_add(i)
                        } else {
                            i
                        };
                        Ok(i.clamp(0, length as i128) as usize)
                    }
                }
            };
            let from = bound("from", 0)?;
            let to = bound("to", length)?.max(from);
            if to - from > MAX_SLICE {
                return Err(
                    ReadError::new("history slice exceeds 256 items; page the range")
                        .with_data(json!({"code":"history_slice_limit", "limit":MAX_SLICE})),
                );
            }
            (from..to)
                .map(item)
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        }
        _ => Err(ReadError::new("unknown history operation")),
    }
}

fn integer(value: &Value) -> Result<i128, ReadError> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
        .or_else(|| {
            value.as_f64().filter(|i| i.is_finite() && i.fract() == 0.0)
            // Saturating conversion preserves out-of-range and clamping semantics
            // even for whole JSON numbers larger than the host's index range.
            .map(|i| i as i128)
        })
        .ok_or_else(|| {
            ReadError::new("history index must be an integer")
                .with_data(json!({"code":"history_index"}))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::transcript::TranscriptDecoderPlugin as _;

    /// TOOL-OUTPUT-DECLARED: deliberate unconstrained results are visible to
    /// the model in both dialects' tool declarations.
    #[test]
    fn explicit_any_has_a_visible_return_type_in_both_dialects() {
        use lash_tool_presenter_api::{ToolPresentationEntry, ToolPresenter};
        let tool = ToolDefinition::raw(
            "echo",
            "tools.echo",
            "Echo any value",
            json!({}),
            ToolDefinition::any_output("Echoes the arbitrary value supplied by the caller."),
        )
        .unwrap()
        .with_execution(std::time::Duration::from_secs(30));
        let entry = ToolPresentationEntry {
            manifest: &tool.manifest,
            contract: &tool.contract,
            call_path: "tools.echo",
            pinned: false,
        };
        for (presenter, return_type) in [
            (
                &lash_tool_presenter_typescript::TypescriptToolPresenter as &dyn ToolPresenter,
                "Promise<unknown>",
            ),
            (
                &lash_tool_presenter_python::PythonToolPresenter as &dyn ToolPresenter,
                "-> Any",
            ),
        ] {
            let config = presenter.admit_config(&json!({})).unwrap();
            let declaration = presenter.tool_doc(&entry, &config).unwrap();
            assert!(declaration.contains(return_type), "{declaration}");
        }
    }

    /// RLM-HISTORY-READ: output declarations validate the values these reads
    /// return, and advertise images only when this session enables them.
    #[test]
    fn history_reads_declare_the_owned_item_shape_and_session_images() {
        let message = json!({"kind":"message","id":"m1","role":"user","content":"hello"});
        for images in [false, true] {
            let [length, at, slice] = HistoryTools::definitions(images);
            assert!(
                length
                    .contract
                    .output_schema
                    .canonical
                    .validate(&json!(1))
                    .is_ok()
            );
            assert!(
                length
                    .contract
                    .output_schema
                    .canonical
                    .validate(&json!(1.5))
                    .is_err()
            );
            for value in [Value::Null, message.clone()] {
                assert!(at.contract.output_schema.canonical.validate(&value).is_ok());
            }
            assert!(
                at.contract
                    .output_schema
                    .canonical
                    .validate(&json!("invalid item"))
                    .is_err()
            );
            assert!(
                slice
                    .contract
                    .output_schema
                    .canonical
                    .validate(&json!([message]))
                    .is_ok()
            );
            assert!(
                slice
                    .contract
                    .output_schema
                    .canonical
                    .validate(&json!([null]))
                    .is_err()
            );
            let item = &slice.contract.output_schema.canonical()["$defs"]["HistoryItem"];
            let members = item
                .get("oneOf")
                .or_else(|| item.get("anyOf"))
                .unwrap()
                .as_array()
                .unwrap();
            assert_eq!(
                members
                    .iter()
                    .any(|member| member["properties"].get("images").is_some()),
                images
            );
        }
    }

    /// RLM-HISTORY-READ / typed stored-data refusal: replacing the eager
    /// projection must preserve corruption refusal in reads and transcripts.
    #[test]
    fn corrupt_history_refuses_reads_and_transcripts() {
        for payload in [
            json!({}),
            json!({"format":u32::MAX}),
            json!({"format":crate::RLM_PROTOCOL_EVENT_VERSION, "event":null}),
        ] {
            let event = lash_core::ProtocolEvent {
                plugin_id: crate::RLM_PROTOCOL_PLUGIN_ID.into(),
                payload: payload.into(),
            };
            let history = lash_core::ReadHistory::from_turn(
                vec![lash_core::SessionHistoryRecord::Protocol(event.clone())].into(),
                Default::default(),
            );
            for _ in 0..3 {
                let error = answer(Some(&history), "rlm.history.at", &json!(-1))
                    .expect_err("corrupt requested history refuses");
                assert_eq!(error.data["code"], "history_corrupt");
            }
            assert_eq!(index_builds(&history), 1, "a pin caches corruption too");
            let corruption = crate::projection::transcript::RlmTranscriptDecoder
                .decode_event(&event)
                .expect_err("corrupt transcript refuses");
            assert_eq!(corruption.record_kind, "RLM protocol event");
        }
    }
}

#[cfg(test)]
#[derive(Default)]
struct IndexBuildCount(std::sync::atomic::AtomicUsize);

#[cfg(test)]
fn note_index_build(history: &lash_core::ReadHistory) {
    history
        .derived(|_| IndexBuildCount::default())
        .0
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn index_builds(history: &lash_core::ReadHistory) -> usize {
    history
        .derived(|_| IndexBuildCount::default())
        .0
        .load(std::sync::atomic::Ordering::SeqCst)
}
