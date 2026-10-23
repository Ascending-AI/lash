//! A trigger registration explicitly publishes the function the tool received.
//! The host row keeps its inline input before publication, so boot recovery
//! can finish under the same pin without relying on cell-owned definitions.

use super::*;
use lash::workflow::document::FunctionValue;

fn checked_function(json: &Value) -> Result<FunctionValue, String> {
    let release = lash::vm::helper_release().map_err(|error| error.to_string())?;
    FunctionValue::from_transport_json(
        json,
        &release.functions,
        lash::workflow::document::FunctionValueLimits::standard(),
        lash::persistence::KernelDataLimits::standard(),
    )
    .map_err(|error| error.to_string())
}

/// Delivered functions have only their retained code and captures. The host
/// grants no operation namespace to a trigger's process.
fn environment(core: &LashCore) -> lash::vm::WorkflowEnvironmentRequest {
    use lash::package::{
        PublicationContext, PublicationDomain, PublicationScope, PublisherInstance, Segment,
    };
    lash::vm::WorkflowEnvironmentRequest {
        snapshot: Default::default(),
        context: PublicationContext {
            domain: PublicationDomain(core.backend().binding_identity().to_string()),
            scope: PublicationScope::Host,
            publisher: PublisherInstance("workbench-triggers".into()),
            root: Segment::new("workbench")
                .unwrap_or_else(|error| panic!("declared workbench package root: {error}")),
        },
        attachment: lash::package::Attachment {
            members: Default::default(),
            packages: Default::default(),
            deferred: Default::default(),
        },
    }
}

impl HostTriggers {
    /// Check and record one registration under `call_id`, then take hold of
    /// its definition. The registration exists once its row is `pinned`.
    pub(super) async fn register(
        &self,
        call_id: &str,
        owner: &SessionId,
        input: TriggerRegistrationInput,
        origin: &lash::package::CallableOrigin,
    ) -> Result<String, HostTriggerError> {
        if let TriggerSource::Cron { expr, tz } = &input.source {
            crate::cron::schedule(expr, tz.as_deref()).map_err(HostTriggerError::Registration)?;
        }
        let core = self.core()?;
        let value = checked_function(&input.definition).map_err(HostTriggerError::Registration)?;
        let environment = environment(&core);
        let (definition, _) = core
            .host_artifacts()
            .inspect_callable(&value, &environment, origin)
            .await?;
        let (pin, state, definition) =
            self.insert_pending(call_id, owner, &input, &definition, &environment, origin)?;
        match state {
            // A redriven call whose first run finished.
            SubscriptionState::Pinned => return Ok(call_id.to_owned()),
            SubscriptionState::Deleting => {
                return Err(HostTriggerError::Registration(format!(
                    "registration {call_id} is being removed"
                )));
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
        if self.mark_pinned(call_id)? {
            Ok(call_id.to_owned())
        } else {
            Err(HostTriggerError::Registration(format!(
                "registration {call_id} was removed while it was made"
            )))
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
        environment: &lash::vm::WorkflowEnvironmentRequest,
        origin: &lash::package::CallableOrigin,
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
               event_arg, args_json, pin, created_at_ms, environment_json, state
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
                serde_json::to_string(&(environment, origin))?,
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
    ) -> Result<(), HostTriggerError> {
        let (function, args, source, event_arg, environment): (String, String, String, String, String) = self.connection()?
            .query_row("SELECT function_json, args_json, source_json, event_arg, environment_json FROM subscriptions WHERE id = ?1", [id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))
            ?;
        let function: Value = serde_json::from_str(&function)?;
        let value = checked_function(&function).map_err(HostTriggerError::Registration)?;
        let (environment, origin): (
            lash::vm::WorkflowEnvironmentRequest,
            lash::package::CallableOrigin,
        ) = serde_json::from_str(&environment)?;
        let published = core
            .host_artifacts()
            .publish_callable(pin, &value, &environment, &origin, None)
            .await?;
        if published != *expected {
            return Err(HostTriggerError::Registration(
                "the trigger's retained callable changed its definition".into(),
            ));
        }
        let mut args: Map<String, Value> = serde_json::from_str(&args)?;
        let source: TriggerSource = serde_json::from_str(&source)?;
        args.insert(event_arg, source.sample_event());
        core.process_definitions()
            .check_args(expected, &args, lash::process::ArgsMode::Complete)
            .await?
            .map_err(|refusal| {
                HostTriggerError::Registration(format!(
                    "the inputs do not fit the definition ({}): {refusal}",
                    refusal.code()
                ))
            })
    }
}
