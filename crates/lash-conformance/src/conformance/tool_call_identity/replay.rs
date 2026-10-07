//! Identity across code cells replayed over their journal. A call replayed
//! across the boundary keeps its identity; a fresh call on the other side of
//! it is a different call, even when the provider hands it the same call id.

use super::laws::{assert_distinct_keys, assert_one_identity, crash_while_held, only};
use super::{ToolCallIdentityTier, World, assert_finished, cell};

/// An RLM turn runs two code cells: the first calls the probe once, the
/// second calls it twice, the second of those holding after its effect, and
/// the turn dies there and is redriven. The replayed cells read their
/// recorded calls back and never run them again; the held call's every run
/// sees one call id; and the three calls are three identities, though the
/// same tool is called from the same kind of statement each time.
pub async fn code_cells_keep_identity_and_distinguish_fresh_calls(tier: ToolCallIdentityTier) {
    let world = World::code(&tier, "code-cells");
    let turn = world.turn(
        "turn",
        vec![
            cell(r#"await tools.identity_probe({ label: "cell-one" });"#),
            cell(
                r#"await tools.identity_probe({ label: "cell-two-a" });
finish(await tools.identity_probe({ label: "cell-two-b", hold: true }));"#,
            ),
        ],
    );
    let assembled = crash_while_held(&world, &turn, "cell-two-b").await;
    assert_finished("the redriven RLM turn", &assembled);
    let one = only(&world, "cell-one");
    let two_a = only(&world, "cell-two-a");
    assert_one_identity("cell-two-b", &world.witness.of("cell-two-b"));
    let two_b = world.witness.of("cell-two-b").remove(0);
    for (what, left, right) in [
        ("two cells' calls", &one, &two_a),
        ("two calls of one cell", &two_a, &two_b),
        ("the first cell's call and the held call", &one, &two_b),
    ] {
        assert_distinct_keys(what, &left.identity, &right.identity);
    }
}
