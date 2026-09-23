// FIG-3571 law L5: a linked module carries one executable program, the
// artifact's `ir`, and an artifact exists only as its validating builders
// and verifying store decoder made it. There is no second program to read, no
// serialized form that could reload a module onto a different carrier, and no
// way to edit or deserialize an artifact around validation
// (`module_artifact_cannot_be_assembled.rs` covers assembling one).

use lash::rlm::lang::{LinkedModule, ModuleArtifact, Program};

fn no_second_program(linked: &LinkedModule) {
    let _ = linked.program();
}

fn no_serialized_form(linked: &LinkedModule) {
    let _ = serde_json::to_string(linked);
}

fn no_field_write(mut artifact: ModuleArtifact, ir: Program) -> ModuleArtifact {
    artifact.ir = ir;
    artifact
}

fn no_unverified_decode(bytes: &[u8]) -> ModuleArtifact {
    serde_json::from_slice::<ModuleArtifact>(bytes).unwrap()
}

fn main() {
    let _ = no_second_program;
    let _ = no_serialized_form;
    let _ = no_field_write;
    let _ = no_unverified_decode;
}
