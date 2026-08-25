impl From<lash_core::ProjectionMode> for RemoteProjectionMode {
    fn from(value: lash_core::ProjectionMode) -> Self {
        match value {
            lash_core::ProjectionMode::Auto => Self::Auto,
            lash_core::ProjectionMode::ExplicitOnly => Self::ExplicitOnly,
            lash_core::ProjectionMode::Exact => Self::Exact,
        }
    }
}

impl From<RemoteProjectionMode> for lash_core::ProjectionMode {
    fn from(value: RemoteProjectionMode) -> Self {
        match value {
            RemoteProjectionMode::Auto => Self::Auto,
            RemoteProjectionMode::ExplicitOnly => Self::ExplicitOnly,
            RemoteProjectionMode::Exact => Self::Exact,
        }
    }
}

impl From<lash_core::SchemaProjectionOverride> for RemoteSchemaProjectionOverride {
    fn from(value: lash_core::SchemaProjectionOverride) -> Self {
        let lash_core::SchemaProjectionOverride { dialect, schema } = value;
        Self { dialect, schema }
    }
}

impl From<RemoteSchemaProjectionOverride> for lash_core::SchemaProjectionOverride {
    fn from(value: RemoteSchemaProjectionOverride) -> Self {
        let RemoteSchemaProjectionOverride { dialect, schema } = value;
        Self { dialect, schema }
    }
}

impl From<lash_core::SchemaProjectionPolicy> for RemoteSchemaProjectionPolicy {
    fn from(value: lash_core::SchemaProjectionPolicy) -> Self {
        Self {
            mode: value.mode.into(),
            overrides: value.overrides.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<RemoteSchemaProjectionPolicy> for lash_core::SchemaProjectionPolicy {
    fn from(value: RemoteSchemaProjectionPolicy) -> Self {
        Self {
            mode: value.mode.into(),
            overrides: value.overrides.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<lash_core::SchemaContract> for RemoteSchemaContract {
    fn from(value: lash_core::SchemaContract) -> Self {
        Self {
            canonical: value.canonical,
            projection: value.projection.into(),
        }
    }
}

impl From<RemoteSchemaContract> for lash_core::SchemaContract {
    fn from(value: RemoteSchemaContract) -> Self {
        lash_core::SchemaContract {
            canonical: value.canonical,
            projection: value.projection.into(),
        }
    }
}

impl From<RemoteToolActivation> for lash_core::ToolActivation {
    fn from(value: RemoteToolActivation) -> Self {
        match value {
            RemoteToolActivation::Always => Self::Always,
            RemoteToolActivation::Internal => Self::Internal,
        }
    }
}

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

impl From<RemoteToolRetryPolicy> for lash_core::ToolRetryPolicy {
    fn from(value: RemoteToolRetryPolicy) -> Self {
        match value {
            RemoteToolRetryPolicy::Never => Self::Never,
            RemoteToolRetryPolicy::Safe {
                max_attempts,
                base_delay_ms,
                max_delay_ms,
            } => Self::Safe {
                max_attempts,
                base_delay_ms,
                max_delay_ms,
            },
            RemoteToolRetryPolicy::Idempotent {
                max_attempts,
                base_delay_ms,
                max_delay_ms,
            } => Self::Idempotent {
                max_attempts,
                base_delay_ms,
                max_delay_ms,
            },
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
            activation,
            argument_projection,
            retry_policy,
            bindings,
        } = value;
        let mut definition = ToolDefinition::raw(
            id.clone(),
            name.clone(),
            description.clone(),
            input_schema.canonical.clone(),
            output_schema.canonical.clone(),
        )
        .with_examples(examples.clone())
        .with_output_contract(output_contract.clone().into());
        definition.contract.input_schema.projection = input_schema.projection.clone().into();
        definition.contract.output_schema.projection = output_schema.projection.clone().into();
        definition.manifest.bindings = bindings.clone();
        if let Some(activation) = *activation {
            definition = definition.with_activation(activation.into());
        }
        if let Some(argument_projection) = argument_projection.clone() {
            definition = definition.with_argument_projection(argument_projection.into());
        }
        if let Some(retry_policy) = *retry_policy {
            definition = definition.with_retry_policy(retry_policy.into());
        }
        Ok(definition)
    }
}
