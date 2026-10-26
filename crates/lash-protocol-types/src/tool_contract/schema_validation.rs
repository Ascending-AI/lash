use lash_vocabulary::tool_contract::ToolContract;
use lash_vocabulary::json_schema::ValueMismatch;

pub fn validate_tool_input(
    contract: &ToolContract,
    args: &serde_json::Value,
) -> Result<(), ValueMismatch> {
    contract.input_schema.canonical.validate(args)
}
