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
