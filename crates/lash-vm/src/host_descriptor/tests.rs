use super::*;
use crate::TypeExpr;

#[derive(Debug, Deserialize, PartialEq)]
struct ScheduleSource {
    expr: String,
    #[serde(default)]
    tz: Option<String>,
}

fn resources() -> LashVmHostCatalog {
    let mut catalog = LashVmHostCatalog::new();
    catalog
        .add_value_constructor(
            ["cron", "Schedule"],
            TypeExpr::Any,
            TypeExpr::Ref("cron.Schedule".into()),
        )
        .expect("schedule descriptor");
    catalog
}

#[test]
fn host_descriptor_typed_decode_rejects_unknown_source_type() {
    let decoded = HostDescriptor::new("missing.Source", serde_json::json!({ "expr": "*" }));
    let err = decoded
        .decode_as::<ScheduleSource>(&resources())
        .expect_err("unknown source type should fail");

    assert!(
        matches!(err, HostDescriptorError::UnknownSourceType { source_type } if source_type == "missing.Source")
    );
}

#[test]
fn host_descriptor_typed_decode_reports_malformed_payload() {
    let decoded = HostDescriptor::new("cron.Schedule", serde_json::json!({ "expr": 1 }));
    let err = decoded
        .decode_as::<ScheduleSource>(&resources())
        .expect_err("malformed source payload should fail");

    assert!(
        matches!(err, HostDescriptorError::MalformedPayload { source_type, .. } if source_type == "cron.Schedule")
    );
}
