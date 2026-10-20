//! A trigger registration explicitly publishes the function the tool received.
//! The host row keeps its inline input before publication, so boot recovery
//! can finish under the same pin without relying on cell-owned definitions.

use super::*;
use lash::workflow::document::{FunctionValue, FunctionValueLimits};

fn checked_function(json: &Value) -> Result<FunctionValue, String> {
    let functions = lash::package::lash_packages()
        .map(|packages| std::sync::Arc::clone(packages.functions()))
        .map_err(|error| error.to_string())?;
    FunctionValue::from_transport_json(
        json,
        functions.as_ref(),
        FunctionValueLimits::standard(),
        lash::persistence::KernelDataLimits::standard(),
    )
    .map_err(|refusal| refusal.to_string())
}

/// The workflow descriptor this explicit publication will have, derived without storage.
fn definition(
    value: &FunctionValue,
    packages: lash::package::CellPackages,
) -> Result<lash::process::ProcessDefinition, String> {
    let kernel = lash::process::KernelProcessDefinition::of(
        &value.document,
        value.document.identity(),
        value.entry.clone(),
        packages.digest(),
    );
    let draft = lash::process::ProcessDefinitionDraft::new(
        lash::process::LASH_VM_ENGINE_KIND,
        serde_json::to_value(&kernel).map_err(|error| error.to_string())?,
        kernel.artifacts(),
    )
    .map_err(|error| error.to_string())?;
    let signature = value
        .signature()
        .ok_or_else(|| "the function has no entry signature".to_owned())?;
    Ok(lash::process::ProcessDefinition::new(
        draft.id(),
        lash::process::ProcessSignature::known(
            serde_json::to_value(signature).map_err(|error| error.to_string())?,
        ),
    ))
}

impl HostTriggers {
    /// Check and record one registration under `call_id`, then take hold of
    /// its definition. The registration exists once its row is `pinned`.
    pub(super) async fn register(
        &self,
        call_id: &str,
        owner: &SessionId,
        input: TriggerRegistrationInput,
    ) -> Result<String, String> {
        if let TriggerSource::Cron { expr, tz } = &input.source {
            crate::cron::schedule(expr, tz.as_deref())?;
        }
        let core = self.core().map_err(|error| error.to_string())?;
        let value = checked_function(&input.definition)?;
        let packages = core
            .host_artifacts()
            .cell_packages()
            .await
            .map_err(|error| error.to_string())?;
        let definition = definition(&value, packages.clone())?;
        let (pin, state, definition) = self
            .insert_pending(call_id, owner, &input, &definition, packages)
            .map_err(|error| error.to_string())?;
        match state {
            // A redriven call whose first run finished.
            SubscriptionState::Pinned => return Ok(call_id.to_owned()),
            SubscriptionState::Deleting => {
                return Err(format!("registration {call_id} is being removed"));
            }
            SubscriptionState::Pending => {}
        }
        if let Err(error) = self
            .publish_registered(&core, call_id, &pin, &definition)
            .await
        {
            self.abandon(call_id).await;
            return Err(error);
        }
        if self
            .mark_pinned(call_id)
            .map_err(|error| error.to_string())?
        {
            Ok(call_id.to_owned())
        } else {
            Err(format!(
                "registration {call_id} was removed while it was made"
            ))
        }
    }

    /// Insert the registration as `pending` unless its call already did, and
    /// answer the pin and state its row holds.
    fn insert_pending(
        &self,
        call_id: &str,
        owner: &SessionId,
        input: &TriggerRegistrationInput,
        definition: &lash::process::ProcessDefinition,
        packages: lash::package::CellPackages,
    ) -> Result<
        (
            lash::process::HostArtifactPin,
            SubscriptionState,
            lash::process::ProcessDefinition,
        ),
        HostTriggerError,
    > {
        let now_ms = self.now_ms();
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO subscriptions (
               id, owner_session, name, source_json, definition_json, function_json,
               event_arg, args_json, pin, created_at_ms, packages, state
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'pending')
             ON CONFLICT(id) DO NOTHING",
            params![
                call_id,
                owner.as_str(),
                input.name,
                serde_json::to_string(&input.source)?,
                serde_json::to_string(definition)?,
                serde_json::to_string(&input.definition)?,
                input.event_arg,
                serde_json::to_string(&input.args)?,
                lash::process::HostArtifactPin::mint().as_str(),
                now_ms,
                packages.digest().to_string(),
            ],
        )?;
        let (pin, state, definition): (String, String, String) = connection.query_row(
            "SELECT pin, state, definition_json FROM subscriptions WHERE id = ?1",
            [call_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok((
            lash::process::HostArtifactPin::try_from(pin)
                .map_err(|error| HostTriggerError::Pin(error.to_string()))?,
            subscription_state(&state)?,
            serde_json::from_str(&definition)?,
        ))
    }

    /// Publish exactly the snapshot the pending row recorded under its stable pin.
    pub(super) async fn publish_registered(
        &self,
        core: &LashCore,
        id: &str,
        pin: &lash::process::HostArtifactPin,
        expected: &lash::process::ProcessDefinition,
    ) -> Result<(), String> {
        let (function, args, source, event_arg, packages): (String, String, String, String, String) = self.connection().map_err(|error| error.to_string())?
            .query_row("SELECT function_json, args_json, source_json, event_arg, packages FROM subscriptions WHERE id = ?1", [id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))
            .map_err(|error| error.to_string())?;
        let function: Value = serde_json::from_str(&function).map_err(|error| error.to_string())?;
        let value = checked_function(&function)?;
        let packages: lash::package::PackageSetDigest = packages
            .parse()
            .map_err(|error: lash::package::InvalidDigest| error.to_string())?;
        let held = lash::package::lash_packages().map_err(|error| error.to_string())?;
        if held.digest() != packages {
            return Err(format!(
                "subscription {id}'s package set {packages} is not this build's"
            ));
        }
        let packages = lash::package::CellPackages::held(&held);
        let environment = delivered_process_environment().map_err(|error| error.to_string())?;
        let outcome = core
            .host_artifacts()
            .publish_workflow(
                pin,
                &lash::workflow::document::DocumentBytes::from(value.document),
                &value.entry,
                &environment,
                packages,
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
        match outcome {
            lash::workflow::WorkflowPublish::Published(publication)
                if publication.definition == *expected => {}
            other => {
                return Err(format!(
                    "the trigger function could not be published: {other:?}"
                ));
            }
        }
        let mut args: Map<String, Value> =
            serde_json::from_str(&args).map_err(|error| error.to_string())?;
        let source: TriggerSource =
            serde_json::from_str(&source).map_err(|error| error.to_string())?;
        args.insert(event_arg, source.sample_event());
        core.process_definitions()
            .check_args(expected, &args, lash::process::ArgsMode::Complete)
            .await
            .map_err(|error| format!("the definition could not be checked: {error}"))?
            .map_err(|refusal| {
                format!(
                    "the inputs do not fit the definition ({}): {refusal}",
                    refusal.code()
                )
            })
    }
}
