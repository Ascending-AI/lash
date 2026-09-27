use std::collections::BTreeMap;

use lash_core::ToolCallOutput;
use lashlang::Value as FlowValue;
use serde_json::Value;

use crate::projection::flow_to_json_value;

/// A view belongs to the structured result, never to a program value.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct ModelViews(BTreeMap<String, String>);

impl ModelViews {
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[cfg(test)]
    pub(super) fn record(&mut self, output: &ToolCallOutput) {
        let Some((key, view)) = output_entry(output) else {
            return;
        };
        self.0.insert(key, view);
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
        self.0.insert(key, view);
    }

    pub(super) fn for_print(&self, value: &FlowValue) -> Option<&str> {
        if self.0.is_empty() || value.contains_projected() {
            return None;
        }
        self.0
            .get(&key(&flow_to_json_value(value)))
            .map(String::as_str)
    }
}

fn output_entry(output: &ToolCallOutput) -> Option<(String, String)> {
    let view = output.model_view.as_ref()?;
    if !output.is_success() || view.len() > crate::MAX_INLINE_TOOL_OUTPUT_SCALAR_BYTES {
        return None;
    }
    Some((key(&output.value_for_projection()), view.clone()))
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
        let value = serde_json::json!({"id": 1});
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
}
