// FIG-3571 law L5: a module artifact cannot be assembled around its
// validating builders: its fields are private, so no struct literal can pair
// a raw program with refs that do not describe it.

use lash::rlm::lang::{ModuleArtifact, Program};

fn no_struct_literal(artifact: &ModuleArtifact, ir: Program) -> ModuleArtifact {
    ModuleArtifact {
        module_ref: artifact.module_ref().clone(),
        host_requirements_ref: artifact.host_requirements_ref().clone(),
        host_requirements: artifact.host_requirements().clone(),
        exports: artifact.exports().clone(),
        ir,
    }
}

fn main() {
    let _ = no_struct_literal;
}
