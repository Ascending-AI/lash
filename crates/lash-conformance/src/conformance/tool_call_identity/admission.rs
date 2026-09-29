//! Identity under a process root: a call a Lashlang process body issues is
//! named by the process it runs in, keeps that name across a replay, and
//! differs from the same call in another process.

use super::laws::{assert_distinct_keys, assert_one_identity, crash_while_held, only};
use super::{ToolCallIdentityTier, World, assert_finished, cell};

/// A cell that starts a process whose body calls the probe once under
/// `label`, and awaits its terminal; `finish` closes the turn on it.
fn process_cell(label: &str, hold: bool, finish: bool) -> crate::LlmResponse {
    let await_line = if finish {
        "finish(await handle);"
    } else {
        "await handle;"
    };
    cell(&format!(
        "const body = async () => {{\n  return await tools.identity_probe({{ label: \"{label}\", hold: {hold} }});\n}};\n\
         const handle = await processes.start({{ definition: body }});\n{await_line}"
    ))
}

/// An RLM turn starts two processes from two cells, each running the same
/// body: one probe call. The second process's call holds after its effect,
/// and the turn dies there and is redriven. The first process's call ran
/// once; every run of the held call sees one call id; and the two calls,
/// the same statement at the same position of two processes, are two
/// identities.
///
/// `process_rlm` is the RLM protocol with its process lifecycle on, and the
/// process controls a cell's `processes.start` needs.
pub async fn process_admission_names_each_call_and_survives_replay(
    tier: ToolCallIdentityTier,
    process_rlm: Vec<std::sync::Arc<dyn crate::facade_support::PluginFactory>>,
) {
    let world = World::code_with_processes(&tier, process_rlm, "process-admission");
    let turn = world.turn(
        "turn",
        vec![
            process_cell("process-one", false, false),
            process_cell("process-two", true, true),
        ],
    );
    let assembled = crash_while_held(&world, &turn, "process-two").await;
    assert_finished("the redriven turn that started two processes", &assembled);
    let one = only(&world, "process-one");
    assert_one_identity("process-two", &world.witness.of("process-two"));
    let two = world.witness.of("process-two").remove(0);
    assert_distinct_keys(
        "the same call at the same position of two processes",
        &one.identity,
        &two.identity,
    );
}
