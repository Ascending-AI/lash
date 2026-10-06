use super::*;

impl From<RemoteToolOutputContract> for lash_core::ToolOutputContract {
    fn from(value: RemoteToolOutputContract) -> Self {
        match value {
            RemoteToolOutputContract::Static => Self::Static,
            RemoteToolOutputContract::FromInputSchema {
                input_field,
                default_schema,
            } => Self::FromInputSchema {
                input_field,
                default_schema,
            },
        }
    }
}

impl From<lash_core::ToolOutputContract> for RemoteToolOutputContract {
    fn from(value: lash_core::ToolOutputContract) -> Self {
        match value {
            lash_core::ToolOutputContract::Static => Self::Static,
            lash_core::ToolOutputContract::FromInputSchema {
                input_field,
                default_schema,
            } => Self::FromInputSchema {
                input_field,
                default_schema,
            },
        }
    }
}

impl From<RemoteToolArgumentProjectionPolicy> for lash_core::ToolArgumentProjectionPolicy {
    fn from(value: RemoteToolArgumentProjectionPolicy) -> Self {
        match value {
            RemoteToolArgumentProjectionPolicy::MaterializeProjectedValues => {
                Self::MaterializeProjectedValues
            }
            RemoteToolArgumentProjectionPolicy::PreserveProjectedRefsInField { field } => {
                Self::PreserveProjectedRefsInField { field }
            }
        }
    }
}

impl TryFrom<&RemoteToolGrant> for ToolDefinition {
    type Error = RemoteProtocolError;

    fn try_from(value: &RemoteToolGrant) -> Result<Self, Self::Error> {
        value.validate()?;
        let RemoteToolGrant {
            id,
            name,
            description,
            input_schema,
            output_schema,
            output_contract,
            examples,
            argument_projection,
            execution_policy,
            bindings,
        } = value;
        let mut definition = ToolDefinition::new(
            id.clone(),
            name.clone(),
            description.clone(),
            input_schema.clone().into(),
            output_schema.clone().into(),
        )
        .with_examples(examples.clone())
        .with_output_contract(output_contract.clone().into());
        definition.contract.input_schema.projection = input_schema.projection.clone().into();
        definition.contract.output_schema.projection = output_schema.projection.clone().into();
        definition.manifest.bindings = bindings.clone();
        if let Some(argument_projection) = argument_projection.clone() {
            definition = definition.with_argument_projection(argument_projection.into());
        }
        if let Some(execution_policy) = *execution_policy {
            definition = definition.with_execution_policy(execution_policy);
        }
        Ok(definition)
    }
}
