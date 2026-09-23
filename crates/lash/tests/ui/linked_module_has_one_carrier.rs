// FIG-3571 law L5: a linked module carries one executable program, the
// artifact's `ir`. There is no second program to read and no serialized form
// that could reload the module onto a different carrier.

use lash::rlm::lang::LinkedModule;

fn no_second_program(linked: &LinkedModule) {
    let _ = linked.program();
}

fn no_serialized_form(linked: &LinkedModule) {
    let _ = serde_json::to_string(linked);
}

fn main() {
    let _ = no_second_program;
    let _ = no_serialized_form;
}
