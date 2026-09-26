//! Laws of `plan_config_commands` (FIG-3541): the stale check precedes
//! validation, an applied patch advances the revision exactly once, a refused
//! route changes nothing, and each refused command's window runs to the next
//! config command of the drain.

use super::*;
use crate::{PromptLayer, SessionPolicy, TurnBudget};

fn running() -> PersistedSessionConfig {
    let mut config = PersistedSessionConfig::from(&SessionPolicy::new(TurnBudget::Unbounded));
    config.provider_id = "provider-a".to_string();
    config.model = ModelSpec {
        id: "model-a".to_string(),
        ..ModelSpec::default()
    };
    config
}

fn model(id: &str) -> ModelSpec {
    ModelSpec {
        id: id.to_string(),
        ..ModelSpec::default()
    }
}

fn model_patch(base: u64, id: &str) -> ApplyConfigPatch {
    ApplyConfigPatch {
        base_config_revision: base,
        model: Some(model(id)),
        ..ApplyConfigPatch::default()
    }
}

fn command(seq: u64, patch: &ApplyConfigPatch) -> (IngressItemId, u64, &ApplyConfigPatch) {
    (IngressItemId::new(format!("item-{seq}")), seq, patch)
}

fn accept_all(_: &str, _: &ModelSpec) -> Result<(), ConfigRefusalCode> {
    Ok(())
}

fn result_of(plan: &ConfigCommandPlan, index: usize) -> &IngressCommandResult {
    &plan.outcomes[index].result
}

#[test]
fn a_stale_patch_settles_stale_before_any_validation() {
    let patch = model_patch(7, "model-b");
    let patch = ApplyConfigPatch {
        provider_id: Some("provider-b".to_string()),
        ..patch
    };
    let commands = [command(3, &patch)];
    let calls = std::cell::Cell::new(0usize);
    let plan = plan_config_commands(&running(), &commands, &|provider, model| {
        calls.set(calls.get() + 1);
        let _ = (provider, model);
        Err(ConfigRefusalCode::ProviderRouteUnknown)
    });
    assert_eq!(calls.get(), 0, "a stale patch is never validated");
    assert_eq!(
        *result_of(&plan, 0),
        IngressCommandResult::StaleConfigRevision { base: 7, head: 0 }
    );
    assert_eq!(plan.config.config_revision, 0);
    assert!(plan.refused_windows.is_empty());
}

#[test]
fn each_applied_patch_advances_the_revision_once_in_enqueue_order() {
    let patch_a = model_patch(0, "model-b");
    let patch_b = ApplyConfigPatch {
        base_config_revision: 1,
        turn_budget: Some(TurnBudget::Unbounded),
        ..ApplyConfigPatch::default()
    };
    // The slice arrives out of order; the drain's own order is enqueue order.
    let commands = [command(9, &patch_b), command(4, &patch_a)];
    let plan = plan_config_commands(&running(), &commands, &accept_all);
    assert_eq!(
        plan.outcomes
            .iter()
            .map(|outcome| outcome.item_id.as_str().to_string())
            .collect::<Vec<_>>(),
        vec!["item-4".to_string(), "item-9".to_string()]
    );
    assert!(matches!(result_of(&plan, 0), IngressCommandResult::Applied));
    assert!(matches!(result_of(&plan, 1), IngressCommandResult::Applied));
    assert_eq!(plan.config.config_revision, 2);
    assert_eq!(plan.config.model.id, "model-b");
}

#[test]
fn a_patch_restating_the_running_route_applies_without_validating() {
    let patch = ApplyConfigPatch {
        provider_id: Some("provider-a".to_string()),
        model: Some(model("model-a")),
        ..model_patch(0, "model-a")
    };
    let commands = [command(1, &patch)];
    let calls = std::cell::Cell::new(0usize);
    let plan = plan_config_commands(&running(), &commands, &|provider, model| {
        calls.set(calls.get() + 1);
        let _ = (provider, model);
        Err(ConfigRefusalCode::ProviderCredentialsMissing)
    });
    assert_eq!(calls.get(), 0, "a restated route is not a route change");
    assert!(matches!(result_of(&plan, 0), IngressCommandResult::Applied));
    assert_eq!(
        plan.config.config_revision, 1,
        "a restatement is still an applied patch"
    );
}

#[test]
fn a_patch_that_changes_no_route_field_never_validates() {
    let patch = ApplyConfigPatch {
        base_config_revision: 0,
        prompt: Some(PromptLayer::new()),
        turn_budget: Some(TurnBudget::Unbounded),
        ..ApplyConfigPatch::default()
    };
    let commands = [command(2, &patch)];
    let plan = plan_config_commands(&running(), &commands, &|provider, model| {
        let _ = (provider, model);
        Err(ConfigRefusalCode::ProviderRouteUnknown)
    });
    assert!(matches!(result_of(&plan, 0), IngressCommandResult::Applied));
    assert_eq!(plan.config.config_revision, 1);
    assert_eq!(
        plan.config.prompt,
        Some(PromptLayer::new()),
        "the overlay applied to the planned head config"
    );
}

#[test]
fn a_refused_patch_leaves_the_running_config_and_revision_standing() {
    let patch = model_patch(0, "model-b");
    let commands = [command(5, &patch)];
    let plan = plan_config_commands(&running(), &commands, &|provider, model| {
        assert_eq!(provider, "provider-a");
        assert_eq!(model.id, "model-b");
        Err(ConfigRefusalCode::ProviderCredentialsMissing)
    });
    assert_eq!(
        *result_of(&plan, 0),
        IngressCommandResult::Refused {
            code: ConfigRefusalCode::ProviderCredentialsMissing
        }
    );
    assert_eq!(plan.config, running());
    assert_eq!(
        plan.refused_windows,
        vec![IngressRefusedWindow {
            after: 5,
            before: None,
            code: ConfigRefusalCode::ProviderCredentialsMissing,
        }]
    );
}

#[test]
fn a_refused_window_runs_to_the_next_config_command_of_the_drain() {
    let refused = model_patch(0, "model-b");
    let applied = model_patch(0, "model-c");
    let tail = model_patch(1, "model-d");
    let commands = [
        command(10, &refused),
        command(14, &applied),
        command(17, &tail),
    ];
    let plan = plan_config_commands(&running(), &commands, &|_, model| {
        if model.id == "model-b" {
            Err(ConfigRefusalCode::ProviderRouteUnknown)
        } else {
            Ok(())
        }
    });
    // `refused` leaves the revision at 0, so `applied` still meets its base
    // and advances to 1; `tail` then applies on 1.
    assert_eq!(
        plan.outcomes
            .iter()
            .map(|outcome| outcome.result.clone())
            .collect::<Vec<_>>(),
        vec![
            IngressCommandResult::Refused {
                code: ConfigRefusalCode::ProviderRouteUnknown
            },
            IngressCommandResult::Applied,
            IngressCommandResult::Applied,
        ]
    );
    assert_eq!(
        plan.refused_windows,
        vec![IngressRefusedWindow {
            after: 10,
            before: Some(14),
            code: ConfigRefusalCode::ProviderRouteUnknown,
        }]
    );
    assert_eq!(plan.config.config_revision, 2);
    assert_eq!(plan.config.model.id, "model-d");
}

#[test]
fn every_refused_command_opens_its_own_window() {
    let first = model_patch(0, "model-b");
    let second = model_patch(0, "model-c");
    let commands = [command(3, &first), command(6, &second)];
    let plan = plan_config_commands(&running(), &commands, &|_, _| {
        Err(ConfigRefusalCode::ProviderCredentialsMissing)
    });
    assert_eq!(
        plan.refused_windows,
        vec![
            IngressRefusedWindow {
                after: 3,
                before: Some(6),
                code: ConfigRefusalCode::ProviderCredentialsMissing,
            },
            IngressRefusedWindow {
                after: 6,
                before: None,
                code: ConfigRefusalCode::ProviderCredentialsMissing,
            },
        ]
    );
    assert_eq!(plan.config.config_revision, 0);
}
