use super::*;

#[test]
fn host_llm_profile_capability_validates_reasoning_effort_selections() {
    use lash::provider::{
        LlmProfileEffortValidationCategory, LlmProfileEffortValidationError, ReasoningCapability,
        ReasoningEncoding, ReasoningSelection,
    };

    let capability = workbench_llm_profile_capability();
    let unsupported: LlmProfileEffortValidationError = capability
        .validate_selection(
            "workbench-model",
            "workbench-provider",
            &ReasoningSelection::Effort("ultra".to_string()),
        )
        .expect_err("host capability must reject an unadvertised effort");
    assert_eq!(
        unsupported.category,
        LlmProfileEffortValidationCategory::UnsupportedEffort
    );
    assert!(unsupported.message.contains("Unsupported effort `ultra`"));
    capability
        .validate_selection(
            "workbench-model",
            "workbench-provider",
            &ReasoningSelection::Effort("high".to_string()),
        )
        .expect("host capability accepts an advertised effort");
    assert_eq!(
        capability
            .validate_selection(
                "workbench-model",
                "workbench-provider",
                &ReasoningSelection::Effort(" HIGH ".to_string()),
            )
            .expect_err("effort names match exactly")
            .category,
        LlmProfileEffortValidationCategory::UnsupportedEffort
    );

    let not_configurable = lash::provider::LlmProfileCapability::default()
        .validate_selection(
            "plain-model",
            "workbench-provider",
            &ReasoningSelection::Effort("low".to_string()),
        )
        .expect_err("plain model must reject configurable effort");
    assert_eq!(
        not_configurable.category,
        LlmProfileEffortValidationCategory::EffortNotConfigurable
    );
    assert!(
        not_configurable
            .message
            .contains("does not expose configurable effort")
    );

    let mut required_capability = capability.clone();
    required_capability
        .reasoning
        .as_mut()
        .expect("workbench reasoning capability")
        .mandatory = true;
    let required = required_capability
        .validate_selection(
            "required-model",
            "workbench-provider",
            &ReasoningSelection::ProviderDefault,
        )
        .expect_err("mandatory reasoning must require an explicit effort");
    assert_eq!(
        required.category,
        LlmProfileEffortValidationCategory::EffortRequired
    );
    assert!(required.message.contains("requires an explicit effort"));

    let malformed_capability = lash::provider::LlmProfileCapability {
        reasoning: Some(ReasoningCapability {
            efforts: vec!["low".to_string(), "high".to_string()],
            encoding: ReasoningEncoding::Budget(BTreeMap::from([("low".to_string(), 1_024)])),
            disable: false,
            mandatory: false,
        }),
        ..Default::default()
    };
    let malformed = malformed_capability
        .validate_selection(
            "malformed-model",
            "workbench-provider",
            &ReasoningSelection::Effort("low".to_string()),
        )
        .expect_err("budget map must cover every advertised effort");
    assert_eq!(
        malformed.category,
        LlmProfileEffortValidationCategory::MalformedCapability
    );
    assert!(
        malformed
            .message
            .contains("missing advertised effort `high`")
    );
}
