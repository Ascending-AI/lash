use std::collections::{BTreeMap, VecDeque};

use lash_core::ToolCallOutput;
use lashlang::Value as FlowValue;
use serde_json::Value;

use crate::projection::flow_to_json_value;

const MAX_ENTRIES: usize = 256;

/// A view belongs to the structured result, never to a program value.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct ModelViews {
    entries: BTreeMap<String, String>,
    oldest_first: VecDeque<String>,
}

impl ModelViews {
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub(super) fn record(&mut self, output: &ToolCallOutput) {
        let Some((key, view)) = output_entry(output) else {
            return;
        };
        self.insert(key, view);
    }

    pub(super) fn record_at(
        &mut self,
        index: usize,
        output: &ToolCallOutput,
        latest_in_cell: &mut BTreeMap<String, usize>,
    ) {
        let Some((key, view)) = output_entry(output) else {
            return;
        };
        if latest_in_cell
            .get(&key)
            .is_some_and(|previous| *previous > index)
        {
            return;
        }
        latest_in_cell.insert(key.clone(), index);
        self.insert(key, view);
    }

    pub(super) fn for_print(&self, value: &FlowValue) -> Option<&str> {
        let structured = matches!(value, FlowValue::Record(fields) if !fields.is_empty())
            || matches!(value, FlowValue::List(items) | FlowValue::Tuple(items) if !items.is_empty());
        if self.entries.is_empty() || !structured || value.contains_projected() {
            return None;
        }
        self.entries
            .get(&key(&flow_to_json_value(value)))
            .map(String::as_str)
    }

    fn insert(&mut self, key: String, view: String) {
        if self.entries.contains_key(&key) {
            self.oldest_first.retain(|previous| previous != &key);
        }
        self.entries.insert(key.clone(), view);
        self.oldest_first.push_back(key);
        if self.entries.len() > MAX_ENTRIES
            && let Some(oldest) = self.oldest_first.pop_front()
        {
            self.entries.remove(&oldest);
        }
    }
}

fn output_entry(output: &ToolCallOutput) -> Option<(String, String)> {
    let view = output.model_view.as_ref()?;
    if !output.is_success() || view.len() > crate::MAX_INLINE_TOOL_OUTPUT_SCALAR_BYTES {
        return None;
    }
    let value = output.value_for_projection();
    if !matches!(&value, Value::Object(fields) if !fields.is_empty())
        && !matches!(&value, Value::Array(items) if !items.is_empty())
    {
        return None;
    }
    Some((key(&value), view.clone()))
}

#[expect(clippy::expect_used, reason = "a serde_json::Value always serializes")]
fn key(value: &Value) -> String {
    // serde_json's default map order is sorted. Rebuild maps in key order as
    // well, so this remains canonical if a caller enables preserve_order.
    fn ordered(value: &Value) -> Value {
        match value {
            Value::Array(items) => Value::Array(items.iter().map(ordered).collect()),
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .map(|(name, value)| (name.clone(), ordered(value)))
                    .collect(),
            ),
            Value::Number(number) => {
                let Some(value) = number.as_f64() else {
                    return Value::Number(number.clone());
                };
                if value.fract() == 0.0 && (value as i64 as f64) == value {
                    Value::Number((value as i64).into())
                } else {
                    Value::Number(number.clone())
                }
            }
            _ => value.clone(),
        }
    }
    let bytes = serde_json::to_vec(&ordered(value)).expect("JSON values always serialize");
    lash_sansio::core_support::blake3_domain_hash_hex("lash-rlm-model-view/v1", bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_call_wins_even_when_replies_finish_out_of_order() {
        let value = serde_json::json!({"items": []});
        let mut views = ModelViews::default();
        let mut order = BTreeMap::new();
        views.record_at(
            2,
            &ToolCallOutput::success(value.clone()).with_model_view("second"),
            &mut order,
        );
        views.record_at(
            1,
            &ToolCallOutput::success(value.clone()).with_model_view("first"),
            &mut order,
        );
        assert_eq!(
            views.for_print(&lashlang::from_json(value.clone())),
            Some("second")
        );
        views.record_at(
            0,
            &ToolCallOutput::success(value.clone()).with_model_view("next cell"),
            &mut BTreeMap::new(),
        );
        assert_eq!(
            views.for_print(&lashlang::from_json(value)),
            Some("next cell")
        );
    }

    #[test]
    fn scalar_and_empty_results_do_not_supply_views_to_program_values() {
        let mut views = ModelViews::default();
        for value in [
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!(0),
            serde_json::json!("ok"),
            serde_json::Value::Null,
        ] {
            views.record(&ToolCallOutput::success(value.clone()).with_model_view("tool view"));
            assert_eq!(views.for_print(&lashlang::from_json(value)), None);
        }
        assert!(views.is_empty());
        views.record(
            &ToolCallOutput::success(serde_json::json!({ "id": 1 })).with_model_view("view"),
        );
        for value in [
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!(0),
            serde_json::json!("ok"),
            serde_json::Value::Null,
        ] {
            assert_eq!(views.for_print(&lashlang::from_json(value)), None);
        }
    }

    #[test]
    fn oldest_view_is_evicted_and_replacement_becomes_newest() {
        let mut views = ModelViews::default();
        for id in 0..MAX_ENTRIES {
            views.record(
                &ToolCallOutput::success(serde_json::json!({ "id": id }))
                    .with_model_view(format!("view {id}")),
            );
        }
        views.record(
            &ToolCallOutput::success(serde_json::json!({ "id": 0 })).with_model_view("newest"),
        );
        let mut views: ModelViews =
            serde_json::from_value(serde_json::to_value(views).expect("serialize views"))
                .expect("restore views");
        views.record(
            &ToolCallOutput::success(serde_json::json!({ "id": MAX_ENTRIES }))
                .with_model_view("overflow"),
        );
        assert_eq!(views.entries.len(), MAX_ENTRIES);
        assert_eq!(
            views.for_print(&lashlang::from_json(serde_json::json!({ "id": 0 }))),
            Some("newest")
        );
        assert_eq!(
            views.for_print(&lashlang::from_json(serde_json::json!({ "id": 1 }))),
            None
        );
        assert_eq!(
            views.for_print(&lashlang::from_json(
                serde_json::json!({ "id": MAX_ENTRIES })
            )),
            Some("overflow")
        );
    }
}
