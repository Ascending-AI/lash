use std::collections::BTreeMap;

use lashlang::{ExecutionHostError, TriggerHostOperation};
use serde_json::Value;

use crate::{
    LASHLANG_ENGINE_KIND, LashlangProcessInput, lashlang_process_event_types,
    lashlang_process_signal_event_types, lashlang_type_expr_schema,
};

/// Executes one Lashlang trigger host operation through the runtime's typed trigger command path.
///
/// Foreground code and durable processes share this adapter so trigger operations never depend on
/// tool-catalog membership and keep one implementation of the trigger mutation contract.
pub async fn execute_trigger_operation(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    artifact_store: &dyn lashlang::LashlangArtifactStore,
    operation: TriggerHostOperation,
    payload: Value,
    effect_id: String,
) -> Result<lashlang::Value, ExecutionHostError> {
    match operation {
        TriggerHostOperation::Register => {
            register_trigger(ctx, artifact_store, payload, effect_id).await
        }
        TriggerHostOperation::List => list_triggers(ctx, payload, effect_id).await,
        TriggerHostOperation::Update => {
            update_trigger(ctx, artifact_store, payload, effect_id, false).await
        }
        TriggerHostOperation::Enable => set_trigger_enabled(ctx, payload, effect_id, true).await,
        TriggerHostOperation::Disable => set_trigger_enabled(ctx, payload, effect_id, false).await,
        TriggerHostOperation::Delete => delete_trigger(ctx, payload, effect_id).await,
        TriggerHostOperation::Revive => {
            update_trigger(ctx, artifact_store, payload, effect_id, true).await
        }
        TriggerHostOperation::Prune => prune_triggers(ctx, payload, effect_id).await,
    }
}

async fn register_trigger(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    artifact_store: &dyn lashlang::LashlangArtifactStore,
    payload: Value,
    effect_id: String,
) -> Result<lashlang::Value, ExecutionHostError> {
    let request = lashlang::TriggerRegistrationRequest::decode(&payload)
        .map_err(|err| ExecutionHostError::new(err.to_string()))?;
    let draft = prepare_trigger_draft(ctx, artifact_store, &request).await?;
    let command = lash_core::TriggerCommand::Register {
        owner_scope: trigger_owner_scope(ctx)?,
        actor: ctx.trigger_actor(),
        draft,
    };
    execute_trigger_command(ctx, effect_id, command).await
}

async fn prepare_trigger_draft(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    artifact_store: &dyn lashlang::LashlangArtifactStore,
    request: &lashlang::TriggerRegistrationRequest,
) -> Result<lash_core::TriggerSubscriptionDraft, ExecutionHostError> {
    let artifact = artifact_store
        .get_module_artifact(&request.target.module_ref)
        .await
        .map_err(|err| {
            ExecutionHostError::new(format!("failed to load lashlang module artifact: {err}"))
        })?
        .ok_or_else(|| {
            ExecutionHostError::new(format!(
                "missing lashlang module artifact `{}` for trigger target `{}`",
                request.target.module_ref, request.target.process_name
            ))
        })?;
    let compatibility =
        lashlang::check_trigger_compatibility(lashlang::TriggerCompatibilityRequest {
            artifact: artifact.as_ref(),
            definition: &request.target,
            source_type: &request.source.source_type,
            inputs: &request.inputs,
        })
        .map_err(|err| ExecutionHostError::new(err.to_string()))?;
    let subscription_key =
        materialized_trigger_subscription_key(request.subscription_key.as_deref())?;
    let source_key = lash_core::facade_support::default_trigger_source_key(
        &request.source.source_type,
        &request.source.value,
    );
    let target = trigger_target_process_input(&request.target).map_err(|err| {
        ExecutionHostError::new(format!("failed to encode trigger target: {err}"))
    })?;
    let target_identity = lashlang_process_identity_for_definition(&request.target);
    let process = artifact
        .canonical_ir
        .process(&request.target.process_name)
        .ok_or_else(|| {
            ExecutionHostError::new(format!(
                "trigger target artifact `{}` is missing process `{}`",
                request.target.module_ref, request.target.process_name
            ))
        })?;
    let event_types = lashlang_process_event_types()
        .into_iter()
        .chain(lashlang_process_signal_event_types(process))
        .collect::<Vec<_>>();
    let env_ref = ctx
        .captured_process_execution_env_ref(&ctx.artifact_owner())
        .await
        .map_err(|err| ExecutionHostError::new(err.to_string()))?;
    let draft = lash_core::TriggerSubscriptionDraft {
        subscription_key,
        env_ref,
        wake_target: ctx.trigger_registration_wake_target(),
        name: request.name.clone(),
        source_type: request.source.source_type.clone(),
        source_key,
        source: request.source.to_json(),
        payload_schema: lash_core::LashSchema::new(lashlang_type_expr_schema(
            &compatibility.resolved_event_type,
        )),
        target,
        target_identity,
        event_types,
        input_template: core_trigger_input_template(&request.inputs),
        target_label: Some(request.target.process_name.clone()),
    };
    draft
        .validate()
        .map_err(|err| ExecutionHostError::new(err.to_string()))?;
    Ok(draft)
}

async fn list_triggers(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    payload: Value,
    effect_id: String,
) -> Result<lashlang::Value, ExecutionHostError> {
    let request = lashlang::TriggerListRequest::decode(&payload)
        .map_err(|err| ExecutionHostError::new(err.to_string()))?;
    let owner_scope = trigger_owner_scope(ctx)?;
    let mut filter =
        lash_core::TriggerSubscriptionFilter::for_registrant_scope(owner_scope.namespace());
    filter.name = request.name;
    filter.source_type = request.source_type;
    filter.enabled = request.enabled;
    filter.target = request
        .target
        .as_ref()
        .map(lashlang_process_definition_for_identity);
    execute_trigger_command(
        ctx,
        effect_id,
        lash_core::TriggerCommand::List {
            owner_scope,
            filter,
        },
    )
    .await
}

async fn update_trigger(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    artifact_store: &dyn lashlang::LashlangArtifactStore,
    payload: Value,
    effect_id: String,
    revive: bool,
) -> Result<lashlang::Value, ExecutionHostError> {
    let request = lashlang::TriggerRegistrationRequest::decode(&payload)
        .map_err(|err| ExecutionHostError::new(err.to_string()))?;
    let subscription_key = request
        .subscription_key
        .clone()
        .ok_or_else(|| ExecutionHostError::new("trigger update requires `subscription_key`"))?;
    let expected_revision = trigger_expected_revision(&payload)?;
    let draft = prepare_trigger_draft(ctx, artifact_store, &request).await?;
    let owner_scope = trigger_owner_scope(ctx)?;
    let actor = ctx.trigger_actor();
    let command = if revive {
        lash_core::TriggerCommand::Revive {
            owner_scope,
            actor,
            subscription_key,
            draft,
            expected_revision,
        }
    } else {
        lash_core::TriggerCommand::Update {
            owner_scope,
            actor,
            subscription_key,
            draft,
            expected_revision,
        }
    };
    execute_trigger_command(ctx, effect_id, command).await
}

async fn set_trigger_enabled(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    payload: Value,
    effect_id: String,
    enabled: bool,
) -> Result<lashlang::Value, ExecutionHostError> {
    let (subscription_key, expected_revision) = trigger_key_and_revision(&payload)?;
    let owner_scope = trigger_owner_scope(ctx)?;
    let actor = ctx.trigger_actor();
    let command = if enabled {
        lash_core::TriggerCommand::Enable {
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
        }
    } else {
        lash_core::TriggerCommand::Disable {
            owner_scope,
            actor,
            subscription_key,
            expected_revision,
        }
    };
    execute_trigger_command(ctx, effect_id, command).await
}

async fn delete_trigger(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    payload: Value,
    effect_id: String,
) -> Result<lashlang::Value, ExecutionHostError> {
    let (subscription_key, expected_revision) = trigger_key_and_revision(&payload)?;
    let command = lash_core::TriggerCommand::Delete {
        owner_scope: trigger_owner_scope(ctx)?,
        actor: ctx.trigger_actor(),
        subscription_key,
        expected_revision,
    };
    execute_trigger_command(ctx, effect_id, command).await
}

async fn prune_triggers(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    payload: Value,
    effect_id: String,
) -> Result<lashlang::Value, ExecutionHostError> {
    let request = lashlang::TriggerPruneRequest::decode(&payload)
        .map_err(|err| ExecutionHostError::new(err.to_string()))?;
    let command = lash_core::TriggerCommand::Prune {
        owner_scope: trigger_owner_scope(ctx)?,
        actor: ctx.trigger_actor(),
        subscription_keys: request.subscription_keys,
    };
    execute_trigger_command(ctx, effect_id, command).await
}

fn trigger_owner_scope(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<lash_core::TriggerOwnerScope, ExecutionHostError> {
    ctx.trigger_owner_scope()
        .map_err(|err| ExecutionHostError::new(err.to_string()))
}

async fn execute_trigger_command(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    effect_id: String,
    command: lash_core::TriggerCommand,
) -> Result<lashlang::Value, ExecutionHostError> {
    let outcome = ctx
        .execute_trigger_effect(effect_id, command)
        .await
        .map_err(|err| ExecutionHostError::new(err.to_string()))?
        .map_err(|err| ExecutionHostError::new(err.to_string()))?;
    let value = match outcome {
        lash_core::TriggerCommandOutcome::Mutation { receipt } => {
            let mut value = serde_json::to_value(&receipt).map_err(|err| {
                ExecutionHostError::new(format!("failed to encode trigger receipt: {err}"))
            })?;
            let object = value.as_object_mut().ok_or_else(|| {
                ExecutionHostError::new("trigger mutation receipt must encode as a record")
            })?;
            object.insert("type".to_string(), serde_json::json!("trigger_handle"));
            object.insert(
                "id".to_string(),
                serde_json::json!(receipt.subscription_key),
            );
            value
        }
        lash_core::TriggerCommandOutcome::List { records } => serde_json::to_value(
            records
                .iter()
                .map(lash_core::facade_support::TriggerRegistration::from)
                .collect::<Vec<_>>(),
        )
        .map_err(|err| {
            ExecutionHostError::new(format!("failed to encode trigger records: {err}"))
        })?,
        lash_core::TriggerCommandOutcome::Prune { receipts } => {
            let values = receipts
                .iter()
                .map(|receipt| {
                    let mut value = serde_json::to_value(receipt).map_err(|err| {
                        ExecutionHostError::new(format!(
                            "failed to encode trigger prune receipt: {err}"
                        ))
                    })?;
                    let object = value.as_object_mut().ok_or_else(|| {
                        ExecutionHostError::new("trigger prune receipt must encode as a record")
                    })?;
                    object.insert("type".to_string(), serde_json::json!("trigger_handle"));
                    object.insert(
                        "id".to_string(),
                        serde_json::json!(receipt.subscription_key),
                    );
                    Ok(value)
                })
                .collect::<Result<Vec<_>, ExecutionHostError>>()?;
            Value::Array(values)
        }
    };
    Ok(lashlang::from_json(value))
}

fn lashlang_process_input_for_definition(
    definition: &lashlang::ProcessDefinitionIdentity,
) -> LashlangProcessInput {
    LashlangProcessInput {
        module_ref: definition.module_ref.clone(),
        process_ref: definition.process_ref.clone(),
        host_requirements_ref: definition.host_requirements_ref.clone(),
        process_name: definition.process_name.clone(),
        args: serde_json::Map::new(),
    }
}

fn lashlang_process_definition_for_identity(
    definition: &lashlang::ProcessDefinitionIdentity,
) -> Value {
    lashlang_process_input_for_definition(definition).definition()
}

fn lashlang_process_identity_for_definition(
    definition: &lashlang::ProcessDefinitionIdentity,
) -> lash_core::ProcessIdentity {
    lash_core::ProcessIdentity::new(LASHLANG_ENGINE_KIND)
        .with_label(Some(definition.process_name.clone()))
        .with_definition(Some(lashlang_process_definition_for_identity(definition)))
}

fn trigger_key_and_revision(payload: &Value) -> Result<(String, u64), ExecutionHostError> {
    let subscription_key = payload
        .get("subscription_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ExecutionHostError::new("trigger operation requires `subscription_key`"))?;
    Ok((subscription_key, trigger_expected_revision(payload)?))
}

fn trigger_expected_revision(payload: &Value) -> Result<u64, ExecutionHostError> {
    payload
        .get("expected_revision")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            ExecutionHostError::new(
                "trigger operation requires a non-negative integer `expected_revision`",
            )
        })
}

fn materialized_trigger_subscription_key(
    subscription_key: Option<&str>,
) -> Result<String, ExecutionHostError> {
    subscription_key.map(ToOwned::to_owned).ok_or_else(|| {
        ExecutionHostError::new(
            "linked lashlang trigger registrations must carry a materialized `subscription_key`",
        )
    })
}

fn trigger_target_process_input(
    definition: &lashlang::ProcessDefinitionIdentity,
) -> Result<lash_core::ProcessInput, serde_json::Error> {
    lashlang_process_input_for_definition(definition).into_process_input()
}

fn core_trigger_input_template(
    input: &lashlang::TriggerInputTemplate,
) -> BTreeMap<String, lash_core::TriggerInputBinding> {
    input
        .entries()
        .map(|(name, binding)| {
            let binding = match binding {
                lashlang::TriggerInputBinding::Event => lash_core::TriggerInputBinding::Event,
                lashlang::TriggerInputBinding::Fixed { value } => {
                    lash_core::TriggerInputBinding::Fixed {
                        value: value.clone(),
                    }
                }
            };
            (name.to_string(), binding)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::materialized_trigger_subscription_key;

    #[test]
    fn trigger_registration_rejects_an_unmaterialized_subscription_key() {
        let error =
            materialized_trigger_subscription_key(None).expect_err("missing key must be rejected");
        assert_eq!(
            error.to_string(),
            "linked lashlang trigger registrations must carry a materialized `subscription_key`"
        );
    }
}
