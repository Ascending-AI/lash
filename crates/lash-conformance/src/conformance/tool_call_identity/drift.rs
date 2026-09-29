//! A call's retained request is bound to its identity: a redrive that would
//! run the call under the same identity with a different request is refused
//! before any effect (ADR 0117 §7, ADR 0116 §2.2).

use super::laws::crash_while_held_result;
use super::{DRIFTING, ProbeArgs, ToolCallIdentityTier, World, calls, text};
use std::sync::atomic::Ordering;

/// The drifting probe's prepare phase seals a new payload every time it
/// runs. The turn is admitted, its call prepared and dispatched, and the
/// turn dies while the call is held between its admission and its effect;
/// the redrive may prepare the call again, sealing a different payload under
/// the same identity. Whatever the tier does with the retained call — serve
/// it, run it from its retained request, or refuse the drift — no effect ever
/// runs with a payload other than the one sealed when the call was admitted.
pub async fn retained_payload_drift_is_refused_before_effects(tier: ToolCallIdentityTier) {
    let world = World::new(&tier, "payload-drift");
    let turn = world.turn(
        "turn",
        vec![
            calls(&[("call_drift", DRIFTING, ProbeArgs::label("drift"))]),
            text("the drifting call settled"),
        ],
    );
    let ended = crash_while_held_result(&world, &turn, "drift").await;
    let prepares = world.witness.prepares.load(Ordering::SeqCst);
    let admitted = serde_json::json!({ "seal": 1 });
    let executions = world.witness.of("drift");
    for execution in &executions {
        assert_eq!(
            execution.prepared, admitted,
            "an effect ran with a payload other than the one sealed at admission (the prepare \
             phase ran {prepares} times; the redrive ended {ended:?}): {executions:?}"
        );
    }
    eprintln!(
        "retained_payload_drift_is_refused_before_effects: prepared {prepares} time(s), \
         {} effect run(s), the redrive {}",
        executions.len(),
        match &ended {
            Ok(turn) => format!("ended {:?}", turn.outcome),
            Err(error) => format!("refused: {error}"),
        }
    );
}
