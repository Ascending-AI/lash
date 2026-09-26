#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct BatchResultRow {
    pub index: usize,
    pub tool: String,
    pub success: bool,
    /// Rows journaled before durations left recorded content still carry this
    /// field; `deny_unknown_fields` would otherwise refuse them. It decodes
    /// and discards — never read, never re-emitted.
    #[serde(rename = "duration_ms", default, skip_serializing)]
    legacy_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
}

impl BatchResultRow {
    pub fn success(index: usize, tool: impl Into<String>, result: serde_json::Value) -> Self {
        Self {
            index,
            tool: tool.into(),
            success: true,
            legacy_duration_ms: None,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(index: usize, tool: impl Into<String>, error: serde_json::Value) -> Self {
        Self {
            index,
            tool: tool.into(),
            success: false,
            legacy_duration_ms: None,
            result: None,
            error: Some(error),
        }
    }

    pub fn value(&self) -> &serde_json::Value {
        if self.success {
            self.result.as_ref().unwrap_or(&serde_json::Value::Null)
        } else {
            self.error.as_ref().unwrap_or(&serde_json::Value::Null)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_names_each_missing_required_field() {
        let missing_tool = serde_json::from_value::<BatchResultRow>(serde_json::json!({
            "index": 0,
            "success": true,
            "result": "ok"
        }))
        .expect_err("row without tool must fail");
        assert!(
            missing_tool.to_string().contains("missing field `tool`"),
            "{missing_tool}"
        );
    }
}
