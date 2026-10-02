//! Physical-turn identity within a logical run: the run itself is physical
//! turn 0, and every later physical turn (a frame switch's follow-on, a
//! terminal-checkpoint follow-on) is `{run}:agent-frame:{n}`.

use crate::TurnId;

/// The separator between a logical run's turn id and a later physical
/// turn's index.
const PHYSICAL_TURN_SEPARATOR: &str = ":agent-frame:";

/// The physical-turn ids of one logical run.
#[derive(Clone, Copy, Debug)]
pub struct PhysicalTurn;

impl PhysicalTurn {
    /// Physical turn `physical_ordinal` of `run`.
    pub fn derive_turn_id(run: &TurnId, physical_ordinal: u64) -> TurnId {
        if physical_ordinal == 0 {
            run.clone()
        } else {
            run.with_suffix(format_args!("{PHYSICAL_TURN_SEPARATOR}{physical_ordinal}"))
        }
    }

    /// The inverse of [`Self::derive_turn_id`]: the logical run's turn and
    /// this physical turn's index within it.
    pub fn split_turn_id(turn_id: &TurnId) -> (TurnId, u64) {
        turn_id
            .as_str()
            .rsplit_once(PHYSICAL_TURN_SEPARATOR)
            .and_then(|(run, index)| {
                let parsed = index.parse::<u64>().ok()?;
                if parsed == 0 || parsed.to_string() != index {
                    return None;
                }
                Some((TurnId::parse(run).ok()?, parsed))
            })
            .unwrap_or_else(|| (turn_id.clone(), 0))
    }

    /// The physical ordinal of `turn` within `run`: the inverse of
    /// [`Self::derive_turn_id`], `None` when `turn` is not one of `run`'s
    /// physical turns.
    #[must_use]
    pub fn physical_ordinal_of(run: &TurnId, turn: &TurnId) -> Option<u64> {
        if turn == run {
            return Some(0);
        }
        let ordinal = turn
            .as_str()
            .strip_prefix(run.as_str())?
            .strip_prefix(PHYSICAL_TURN_SEPARATOR)?
            .parse::<u64>()
            .ok()?;
        (ordinal > 0 && Self::derive_turn_id(run, ordinal) == *turn).then_some(ordinal)
    }
}
