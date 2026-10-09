//! Argument validation shared by starts and host registration checks.
use crate::LashVmArtifacts;
use lash_core::ArgsMismatch;
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps;

pub(crate) async fn check_args(
    workers: &lash_vm_client::service::Service,
    artifacts: &LashVmArtifacts,
    params: &std::collections::BTreeMap<String, lash_vm::TypeExpr>,
    args: &serde_json::Map<String, serde_json::Value>,
    mode: lash_core::ArgsMode,
) -> Result<(), ArgsMismatch> {
    for name in args.keys() {
        if !params.contains_key(name) {
            return Err(ArgsMismatch::Argument {
                path: name.clone(),
                message: "argument is not declared by the target process".to_owned(),
            });
        }
    }
    for (name, expected) in params {
        match args.get(name) {
            Some(value) => {
                validate_process_claims(workers, artifacts, value, expected, name.clone()).await?
            }
            None if mode == lash_core::ArgsMode::Complete => {
                return Err(ArgsMismatch::Argument {
                    path: name.clone(),
                    message: "required argument is missing".to_owned(),
                });
            }
            None => {}
        }
    }
    Ok(())
}

fn validate_process_claims<'a>(
    workers: &'a lash_vm_client::service::Service,
    artifact_store: &'a LashVmArtifacts,
    value: &'a serde_json::Value,
    expected: &'a lash_vm::TypeExpr,
    path: String,
) -> lash_sansio::future::SendBoxFuture<'a, Result<(), ArgsMismatch>> {
    Box::pin(async move {
        let invalid = |message: String| ArgsMismatch::Argument {
            path: path.clone(),
            message,
        };
        match expected {
            lash_vm::TypeExpr::Any => Ok(()),
            lash_vm::TypeExpr::Str => value
                .is_string()
                .then_some(())
                .ok_or_else(|| invalid("expected string".to_string())),
            lash_vm::TypeExpr::Int => value
                .as_i64()
                .is_some()
                .then_some(())
                .ok_or_else(|| invalid("expected integer".to_string())),
            lash_vm::TypeExpr::Float => value
                .is_number()
                .then_some(())
                .ok_or_else(|| invalid("expected number".to_string())),
            lash_vm::TypeExpr::Bool => value
                .is_boolean()
                .then_some(())
                .ok_or_else(|| invalid("expected boolean".to_string())),
            lash_vm::TypeExpr::Null => value
                .is_null()
                .then_some(())
                .ok_or_else(|| invalid("expected null".to_string())),
            lash_vm::TypeExpr::Enum(values) => value
                .as_str()
                .is_some_and(|value| values.iter().any(|item| item.as_str() == value))
                .then_some(())
                .ok_or_else(|| invalid("expected enum value".to_string())),
            lash_vm::TypeExpr::Dict => value
                .is_object()
                .then_some(())
                .ok_or_else(|| invalid("expected object".to_string())),
            lash_vm::TypeExpr::List(item) => {
                let items = value
                    .as_array()
                    .ok_or_else(|| invalid("expected list".to_string()))?;
                for (index, item_value) in items.iter().enumerate() {
                    validate_process_claims(
                        workers,
                        artifact_store,
                        item_value,
                        item,
                        format!("{path}[{index}]"),
                    )
                    .await?;
                }
                Ok(())
            }
            lash_vm::TypeExpr::Object(fields) => {
                let object = value
                    .as_object()
                    .ok_or_else(|| invalid("expected object".to_string()))?;
                for field in fields {
                    match object.get(field.name.as_str()) {
                        Some(field_value) => {
                            validate_process_claims(
                                workers,
                                artifact_store,
                                field_value,
                                &field.ty,
                                format!("{path}.{}", field.name),
                            )
                            .await?;
                        }
                        None if field.optional => {}
                        None => {
                            return Err(invalid(format!(
                                "required field `{}` is missing",
                                field.name
                            )));
                        }
                    }
                }
                Ok(())
            }
            lash_vm::TypeExpr::Union(items) => {
                let mut errors = Vec::new();
                for item in items {
                    match validate_process_claims(
                        workers,
                        artifact_store,
                        value,
                        item,
                        path.clone(),
                    )
                    .await
                    {
                        Ok(()) => return Ok(()),
                        Err(ArgsMismatch::Argument { message, .. }) => errors.push(message),
                        Err(error) => return Err(error),
                    }
                }
                Err(invalid(format!(
                    "value matches no union variant ({})",
                    errors.join("; ")
                )))
            }
            lash_vm::TypeExpr::Process(expected_process) => {
                let expected_signature = expected_process.as_signature().ok_or_else(|| {
                    invalid("program signature is unexpectedly unknown".to_string())
                })?;
                let identity = lash_vm::ProcessDefinitionIdentity::from_process_value(value)
                    .map_err(|error| invalid(error.to_string()))?;
                let actual_artifact = workers
                    .inspect_artifact(artifact_store, &identity.module_ref)
                    .await
                    .map_err(|error| ArgsMismatch::DefinitionRead {
                        source: lash_core::PluginError::from(error),
                    })?
                    .ok_or_else(|| {
                        invalid(format!(
                            "missing process artifact `{}`",
                            identity.module_ref
                        ))
                    })?;
                let actual = actual_artifact
                    .process_type(&identity)
                    .map_err(|error| invalid(error.to_string()))?;
                let expected = lash_vm::TypeExpr::Process(lash_vm::ProcessType::known(
                    expected_signature.clone(),
                ));
                if lash_vm::is_resolved_type_assignable(&actual, &expected) {
                    Ok(())
                } else {
                    Err(invalid(format!(
                        "immutable process signature `{actual}` is not assignable to `{expected}`"
                    )))
                }
            }
            lash_vm::TypeExpr::Ref(_) => Ok(()),
        }
    })
}
