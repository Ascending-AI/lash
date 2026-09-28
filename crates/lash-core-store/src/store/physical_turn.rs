//! Physical-turn identity within a logical root: the root itself is physical
//! turn 0, and every later physical turn (a frame switch's follow-on, a
//! terminal-checkpoint follow-on) is `{root}:agent-frame:{n}`.

use crate::TurnId;

/// The separator between a logical root's turn id and a later physical
/// turn's index.
const PHYSICAL_TURN_SEPARATOR: &str = ":agent-frame:";

/// The physical-turn ids of one logical root.
#[derive(Clone, Copy, Debug)]
pub struct PhysicalTurn;

impl PhysicalTurn {
    /// Physical turn `physical_ordinal` of `root`.
    pub fn derive_turn_id(root: &TurnId, physical_ordinal: u64) -> TurnId {
        if physical_ordinal == 0 {
            root.clone()
        } else {
            TurnId::from(format!("{root}{PHYSICAL_TURN_SEPARATOR}{physical_ordinal}"))
        }
    }

    /// The inverse of [`Self::derive_turn_id`]: the logical root's turn and
    /// this physical turn's index within it.
    pub fn split_turn_id(turn_id: &TurnId) -> (TurnId, u64) {
        turn_id
            .as_str()
            .rsplit_once(PHYSICAL_TURN_SEPARATOR)
            .and_then(|(root, index)| {
                let parsed = index.parse::<u64>().ok()?;
                (parsed > 0 && parsed.to_string() == index).then(|| (TurnId::from(root), parsed))
            })
            .unwrap_or_else(|| (turn_id.clone(), 0))
    }

    /// The physical ordinal of `turn` within `root`: the inverse of
    /// [`Self::derive_turn_id`], `None` when `turn` is not one of `root`'s
    /// physical turns.
    #[must_use]
    pub fn physical_ordinal_of(root: &TurnId, turn: &TurnId) -> Option<u64> {
        if turn == root {
            return Some(0);
        }
        let ordinal = turn
            .as_str()
            .strip_prefix(root.as_str())?
            .strip_prefix(PHYSICAL_TURN_SEPARATOR)?
            .parse::<u64>()
            .ok()?;
        (ordinal > 0 && Self::derive_turn_id(root, ordinal) == *turn).then_some(ordinal)
    }
}
