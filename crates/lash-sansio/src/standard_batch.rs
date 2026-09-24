#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct BatchResultRow {
    pub index: usize,
    pub tool: String,
    pub success: bool,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
}

impl BatchResultRow {
    pub fn success(
        index: usize,
        tool: impl Into<String>,
        duration_ms: u64,
        result: serde_json::Value,
    ) -> Self {
        Self {
            index,
            tool: tool.into(),
            success: true,
            duration_ms,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(
        index: usize,
        tool: impl Into<String>,
        duration_ms: u64,
        error: serde_json::Value,
    ) -> Self {
        Self {
            index,
            tool: tool.into(),
            success: false,
            duration_ms,
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
            "duration_ms": 0,
            "result": "ok"
        }))
        .expect_err("row without tool must fail");
        assert!(
            missing_tool.to_string().contains("missing field `tool`"),
            "{missing_tool}"
        );

        let missing_duration = serde_json::from_value::<BatchResultRow>(serde_json::json!({
            "index": 0,
            "tool": "probe",
            "success": true,
            "result": "ok"
        }))
        .expect_err("row without duration_ms must fail");
        assert!(
            missing_duration
                .to_string()
                .contains("missing field `duration_ms`"),
            "{missing_duration}"
        );
    }
}
