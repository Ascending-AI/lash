use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
use std::collections::BTreeMap;

use lashlang::{ExecutionHostError, TriggerHostOperation};
use serde_json::Value;

use crate::{lashlang_process_event_types, lashlang_type_expr_schema};

/// Foreground code and durable processes share this adapter so trigger operations never depend on
/// tool-catalog membership and keep one implementation of the trigger mutation contract.
pub async fn execute_trigger_operation(
    workers: &lash_vm_client::service::Service,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    artifact_store: &lashlang::LashlangArtifacts,
    operation: TriggerHostOperation,
    payload: Value,
    effect_id: String,
) -> Result<lashlang::Value, ExecutionHostError> {
    let mut recorded = None;
    execute_trigger_operation_recording(
        workers,
        ctx,
        artifact_store,
        operation,
        payload,
        effect_id,
        &mut recorded,
    )
    .await
}

/// The outcome class and code of a trigger command whose effect result was
/// recorded — what a process incorporates into its durable effect summary.
pub(crate) type RecordedTriggerOutcome = Option<(
    lash_core::ProcessEffectOutcomeClass,
    Option<lash_core::FailureCode>,
)>;

/// As [`execute_trigger_operation`], also reporting through `recorded` the
/// outcome of the trigger effect, when the effect ran and recorded one.
pub(crate) async fn execute_trigger_operation_recording(
    workers: &lash_vm_client::service::Service,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    artifact_store: &lashlang::LashlangArtifacts,
    operation: TriggerHostOperation,
    payload: Value,
    effect_id: String,
    recorded: &mut RecordedTriggerOutcome,
) -> Result<lashlang::Value, ExecutionHostError> {
    match operation {
        TriggerHostOperation::List => list_triggers(ctx, payload, effect_id, recorded).await,
        TriggerHostOperation::Update => {
            update_trigger(
                workers,
                ctx,
                artifact_store,
                payload,
                effect_id,
                false,
                recorded,
            )
            .await
        }
        TriggerHostOperation::Enable => {
            set_trigger_enabled(ctx, payload, effect_id, true, recorded).await
        }
        TriggerHostOperation::Disable => {
            set_trigger_enabled(ctx, payload, effect_id, false, recorded).await
        }
        TriggerHostOperation::Delete => delete_trigger(ctx, payload, effect_id, recorded).await,
        TriggerHostOperation::Revive => {
            update_trigger(
                workers,
                ctx,
                artifact_store,
                payload,
                effect_id,
                true,
                recorded,
            )
            .await
        }
        TriggerHostOperation::Prune => prune_triggers(ctx, payload, effect_id, recorded).await,
    }
}

/// The registration-decoded parts of a subscription draft: everything a
/// registration derives from the request and the target's module artifact.
///
/// `env_ref` and `wake_target` are deliberately absent: which execution env a
/// subscription names and which session it wakes belong to the caller's
/// context, not to the registration record. The leaf tool resolves them from
/// its attempt context and declares them; the `update`/`revive` host
/// operations resolve them from the live runtime context (FIG-3116).
pub(crate) struct PreparedTriggerDraft {
    pub subscription_key: String,
    pub name: Option<String>,
    pub source_type: String,
    pub source_key: String,
    pub source: Value,
    pub payload_schema: lash_core::LashSchema,
    pub source_capture: lash_core::TriggerSourceCapture,
    pub target: lash_core::ProcessInput,
    pub target_identity: lash_core::ProcessIdentity,
    pub event_types: Vec<lash_core::ProcessEventType>,
    pub input_template: BTreeMap<String, lash_core::TriggerInputBinding>,
    pub target_label: Option<String>,
}

impl PreparedTriggerDraft {
    /// Completes the draft under `env_ref`/`wake_target` and runs the same
    /// validation a fully-formed draft met before this split existed.
    pub(crate) fn into_draft(
        self,
        env_ref: lash_core::ProcessExecutionEnvRef,
        wake_target: Option<lash_core::SessionScope>,
    ) -> Result<lash_core::TriggerSubscriptionDraft, ExecutionHostError> {
        let draft = lash_core::TriggerSubscriptionDraft {
            subscription_key: self.subscription_key,
            env_ref,
            wake_target,
            name: self.name,
            source_type: self.source_type,
            source_key: self.source_key,
            source: self.source,
            payload_schema: self.payload_schema,
            source_capture: self.source_capture,
            target: self.target,
            target_identity: self.target_identity,
            event_types: self.event_types,
            input_template: self.input_template,
            target_label: self.target_label,
        };
        draft
            .validate()
            .map_err(|err| ExecutionHostError::new(err.to_string()))?;
        Ok(draft)
    }
}

pub(crate) async fn prepare_trigger_draft(
    workers: &lash_vm_client::service::Service,
    artifact_store: &lashlang::LashlangArtifacts,
    engines: &lash_core::ProcessEngineRegistry,
    request: &lashlang::TriggerRegistrationRequest,
) -> Result<PreparedTriggerDraft, ExecutionHostError> {
    let ports = engines
        .artifact_ports()
        .ok_or_else(|| ExecutionHostError::new("definition artifact ports are unavailable"))?;
    let resolved = ports
        .read_definition(engines, request.target.definition_id())
        .await
        .map_err(|e| ExecutionHostError::new(e.to_string()))?
        .ok_or_else(|| ExecutionHostError::new("DefinitionMissing"))?;
    engines
        .verify_definition_claim(
            &resolved.draft,
            &lash_core::ProcessDefinition::new(
                request.target.definition_id().clone(),
                request.target.signature_claim().clone(),
            ),
        )
        .await
        .map_err(|e| ExecutionHostError::new(e.to_string()))?;
    let mut definition =
        lashlang::ProcessDefinitionIdentity::from_process_value(resolved.draft.value().as_json())
            .map_err(|e| ExecutionHostError::new(e.to_string()))?;
    let artifact = workers
        .inspect_artifact(artifact_store, &definition.module_ref)
        .await
        .map_err(|err| {
            ExecutionHostError::new(format!("failed to load lashlang module artifact: {err}"))
        })?
        .ok_or_else(|| {
            ExecutionHostError::new(format!(
                "missing lashlang module artifact `{}` for trigger target `{}`",
                definition.module_ref, definition.process_name
            ))
        })?;
    definition.process_name = artifact
        .process_name_for_ref(&definition.process_ref)
        .ok_or_else(|| ExecutionHostError::new("definition ProcessRef is not exported"))?
        .to_owned();
    let compatibility = match workers
        .request_accounted(lash_vm_client::service::Request::TriggerCompatibility {
            bytes: artifact.bytes().to_vec(),
            definition: definition.clone(),
            source_type: request.source.source_type.clone(),
            inputs: request.inputs.clone(),
        })
        .await
        .map_err(|error| ExecutionHostError::new(error.to_string()))?
    {
        lash_vm_client::service::Response::TriggerCompatibility(compatibility) => compatibility,
        lash_vm_client::service::Response::Refused { message, .. } => {
            return Err(ExecutionHostError::new(message));
        }
        _ => {
            return Err(ExecutionHostError::new(
                "unexpected worker trigger compatibility response",
            ));
        }
    };
    let source_key = lash_core::facade_support::default_trigger_source_key(
        &request.source.source_type,
        &request.source.value,
    );
    // Explicit keys ride the call verbatim. A registration without one is a
    // derived subscription: the compiler stopped materializing these into the
    // linked record (FIG-2997), so the owner of the descriptor and the target
    // derives the identity here, from what the registration actually carries.
    let subscription_key = materialized_trigger_subscription_key(
        request.subscription_key.as_deref(),
        &definition.process_name,
        &request.source.source_type,
        &source_key,
    )?;
    let target = lash_core::ProcessInput::Definition {
        definition_id: resolved.id().clone(),
        args: serde_json::Map::new(),
        signature_claim: Some(resolved.definition.signature.clone()),
    };
    let mut target_identity = lash_core::ProcessIdentity::labelled(
        resolved.draft.engine_kind().clone(),
        Some(definition.process_name.clone()),
    );
    target_identity.definition_id = Some(resolved.id().clone());
    let process = artifact.process(&definition.process_name).ok_or_else(|| {
        ExecutionHostError::new(format!(
            "trigger target artifact `{}` is missing process `{}`",
            definition.module_ref, definition.process_name
        ))
    })?;
    let event_types = lashlang_process_event_types()
        .into_iter()
        .chain(process.signals.clone())
        .collect::<Vec<_>>();
    Ok(PreparedTriggerDraft {
        subscription_key,
        name: request.name.clone(),
        source_type: request.source.source_type.clone(),
        source_key,
        source: request.source.to_json(),
        payload_schema: lash_core::LashSchema::new(lashlang_type_expr_schema(
            &compatibility.resolved_event_type,
        )),
        source_capture: captured_trigger_source(&request.source.source_type, &compatibility),
        target,
        target_identity,
        event_types,
        input_template: core_trigger_input_template(&request.inputs),
        target_label: Some(definition.process_name.clone()),
    })
}

/// Copies the admitted source contract and provider route out of the module
/// artifact's captured requirements and onto the subscription.
///
/// The artifact is what a durable process re-registering after the foreground
/// session has ended can still read, which is why the route travels there and
/// is copied here rather than resolved again. A source the linker admitted from
/// the resident surface has no provider route to carry.
fn captured_trigger_source(
    source_type: &str,
    compatibility: &lashlang::TriggerCompatibility,
) -> lash_core::TriggerSourceCapture {
    let constructor_path = source_type
        .split('.')
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let config_schema =
        lash_core::LashSchema::new(lashlang_type_expr_schema(&compatibility.config_type));
    match compatibility.provider_id.as_deref() {
        Some(provider_id) => lash_core::TriggerSourceCapture::provider(
            constructor_path,
            config_schema,
            provider_id,
            compatibility
                .route
                .as_deref()
                .and_then(|route| serde_json::from_str(route).ok())
                .unwrap_or(Value::Null),
        ),
        None => lash_core::TriggerSourceCapture::resident(constructor_path, config_schema),
    }
}

async fn list_triggers(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    payload: Value,
    effect_id: String,
    recorded: &mut RecordedTriggerOutcome,
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
        .map(|target| target.definition_id().to_tagged_json());
    execute_trigger_command(
        ctx,
        effect_id,
        lash_core::TriggerCommand::List {
            owner_scope,
            filter,
        },
        recorded,
    )
    .await
}

async fn update_trigger(
    workers: &lash_vm_client::service::Service,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    artifact_store: &lashlang::LashlangArtifacts,
    payload: Value,
    effect_id: String,
    revive: bool,
    recorded: &mut RecordedTriggerOutcome,
) -> Result<lashlang::Value, ExecutionHostError> {
    let request = lashlang::TriggerRegistrationRequest::decode(&payload)
        .map_err(|err| ExecutionHostError::new(err.to_string()))?;
    let subscription_key = request
        .subscription_key
        .clone()
        .ok_or_else(|| ExecutionHostError::new("trigger update requires `subscription_key`"))?;
    let expected_revision = trigger_expected_revision(&payload)?;
    let prepared =
        prepare_trigger_draft(workers, artifact_store, ctx.definition_engines(), &request).await?;
    // An environment this execution captures is published under its own
    // execution referrer; the command's journaled effect then holds it, with
    // the target module, under the revision it commits (ADR 0113 §3.4).
    let claim = ctx
        .execution_claim()
        .map_err(|err| ExecutionHostError::new(err.to_string()))?;
    let env_ref = ctx
        .captured_process_execution_env_ref(&claim)
        .await
        .map_err(|err| ExecutionHostError::new(err.to_string()))?;
    let draft = prepared.into_draft(env_ref, ctx.trigger_registration_wake_target())?;
    let owner_scope = trigger_owner_scope(ctx)?;
    let actor = ctx
        .trigger_actor()
        .map_err(|err| ExecutionHostError::new(err.to_string()))?;
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
    execute_trigger_command(ctx, effect_id, command, recorded).await
}

async fn set_trigger_enabled(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    payload: Value,
    effect_id: String,
    enabled: bool,
    recorded: &mut RecordedTriggerOutcome,
) -> Result<lashlang::Value, ExecutionHostError> {
    let (subscription_key, expected_revision) = trigger_key_and_revision(&payload)?;
    let owner_scope = trigger_owner_scope(ctx)?;
    let actor = ctx
        .trigger_actor()
        .map_err(|err| ExecutionHostError::new(err.to_string()))?;
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
    execute_trigger_command(ctx, effect_id, command, recorded).await
}

async fn delete_trigger(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    payload: Value,
    effect_id: String,
    recorded: &mut RecordedTriggerOutcome,
) -> Result<lashlang::Value, ExecutionHostError> {
    let (subscription_key, expected_revision) = trigger_key_and_revision(&payload)?;
    let command = lash_core::TriggerCommand::Delete {
        owner_scope: trigger_owner_scope(ctx)?,
        actor: ctx
            .trigger_actor()
            .map_err(|err| ExecutionHostError::new(err.to_string()))?,
        subscription_key,
        expected_revision,
    };
    execute_trigger_command(ctx, effect_id, command, recorded).await
}

async fn prune_triggers(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    payload: Value,
    effect_id: String,
    recorded: &mut RecordedTriggerOutcome,
) -> Result<lashlang::Value, ExecutionHostError> {
    let request = lashlang::TriggerPruneRequest::decode(&payload)
        .map_err(|err| ExecutionHostError::new(err.to_string()))?;
    let command = lash_core::TriggerCommand::Prune {
        owner_scope: trigger_owner_scope(ctx)?,
        actor: ctx
            .trigger_actor()
            .map_err(|err| ExecutionHostError::new(err.to_string()))?,
        subscription_keys: request.subscription_keys,
    };
    execute_trigger_command(ctx, effect_id, command, recorded).await
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
    recorded: &mut RecordedTriggerOutcome,
) -> Result<lashlang::Value, ExecutionHostError> {
    let outcome = ctx
        .execute_trigger_effect(effect_id, command)
        .await
        .map_err(|err| {
            // A replay mismatch is the run's, not the program's: recorded on
            // the execution so the bridge stops the run at this command
            // instead of handing the program a catchable failure (FIG-3586).
            if err.code.is_replay_mismatch() {
                ctx.record_nested_runtime_effect_error(err.clone());
            }
            ExecutionHostError::new(err.to_string())
        })?;
    *recorded = Some(match &outcome {
        Ok(_) => (lash_core::ProcessEffectOutcomeClass::Success, None),
        Err(error) => (
            lash_core::ProcessEffectOutcomeClass::Failure,
            Some(error.failure_code()),
        ),
    });
    let outcome = outcome.map_err(|err| ExecutionHostError::new(err.to_string()))?;
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
    process_name: &str,
    source_type: &str,
    source_key: &str,
) -> Result<String, ExecutionHostError> {
    match subscription_key {
        Some(key) => Ok(key.to_owned()),
        None => Ok(lash_core::facade_support::derived_trigger_subscription_key(
            process_name,
            source_type,
            source_key,
        )),
    }
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
    fn trigger_registration_materializes_a_derived_subscription_key() {
        let key =
            materialized_trigger_subscription_key(None, "scan", "timer.Schedule", "source-key")
                .expect("a registration without a key derives one at the boundary");
        assert!(key.starts_with("derived/"), "{key}");
    }

    #[test]
    fn trigger_registration_keeps_an_explicit_subscription_key_verbatim() {
        let key = materialized_trigger_subscription_key(
            Some("morning-scan"),
            "scan",
            "timer.Schedule",
            "source-key",
        )
        .expect("explicit key is kept");
        assert_eq!(key, "morning-scan");
    }
}
