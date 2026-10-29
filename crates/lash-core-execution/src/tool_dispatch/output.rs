//! Settlement uses the output witness of the prepared call, after argument
//! transforms and provider preparation, never the registration placeholder.

use lash_kernel_value::json_schema::JsonSchema;
use lash_vocabulary::tool_contract::{ToolContract, ToolOutputContract};
use lash_vocabulary::tool_declaration::{DeclarationRefusal, ToolDeclaration};

pub(super) fn schema_for_call(
    contract: &ToolContract,
    args: &serde_json::Value,
) -> Result<JsonSchema, Box<crate::ToolFailure>> {
    match &contract.output_contract {
        ToolOutputContract::Static => Ok(contract.output_schema.canonical.clone()),
        ToolOutputContract::FromInputSchema {
            input_field,
            default_schema,
        } => {
            let schema =
                lash_vocabulary::schema_contract::parse_output_schema(args.get(input_field))
                    .map_err(|error| {
                        Box::new(crate::ToolFailure::invalid_request(
                            "invalid_output_schema",
                            error.to_string(),
                        ))
                    })?;
            match schema {
                Some(schema) => JsonSchema::admit(schema).map_err(|source| {
                    Box::new(
                        crate::ToolFailure::invalid_request(
                            "unusable_output_schema",
                            source.to_string(),
                        )
                        .with_cause(crate::ToolFailureCause::SchemaAdmission { source }),
                    )
                }),
                None => Ok(default_schema
                    .as_ref()
                    .unwrap_or(&contract.output_schema.canonical)
                    .clone()),
            }
        }
    }
}

pub(super) fn declaration_failure(
    tool_name: &str,
    refusal: DeclarationRefusal,
) -> crate::ToolFailure {
    crate::ToolFailure::runtime(
        crate::ToolFailureClass::Internal,
        "tool_outcome_not_declared",
        format!("tool `{tool_name}` returned an outcome its declaration does not admit: {refusal}"),
    )
    .with_cause(crate::ToolFailureCause::Declaration { refusal })
}

pub(crate) fn settle(
    output: crate::ToolCallOutput,
    declaration: &ToolDeclaration,
    contract: &ToolContract,
    args: &serde_json::Value,
    tool_name: &str,
) -> Result<crate::ToolCallOutput, Box<crate::ToolFailure>> {
    // A failed or cancelled call carries no success value to validate.
    let schema = if output.is_success() {
        schema_for_call(contract, args)?
    } else {
        contract.output_schema.canonical.clone()
    };
    output
        .settled(declaration, &schema)
        .map_err(|refusal| Box::new(declaration_failure(tool_name, refusal)))
}
