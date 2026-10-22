//! Names of bodies implemented by the law's ProcessSteps, not external tool imports.
pub fn tool(step: &lash_execution_contracts::process::StepRequest) -> lash_vocabulary::ToolId {
    match step {
        lash_execution_contracts::process::StepRequest::Engine { kind, .. } => {
            lash_vocabulary::ToolId::new(&kind.0)
        }
        _ => panic!("the scripted engine issues only its own bodies"),
    }
}
