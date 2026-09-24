//! Seeded scheduling perturbation.
//!
//! A drive that is deterministic must not care when, relative to one another,
//! its operations become ready. A perturbed run holds each settled operation
//! back for a seeded number of executor rounds and wakes the ones released in
//! a round in a seeded order, so a drive that races two operations, or reads
//! which one landed first, issues different commands under different seeds.

/// Holds are drawn from `0..=MAX_HOLD` rounds.
const MAX_HOLD: u64 = 3;

/// The scheduling policy of one run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// Every settled operation is delivered at once, in issue order: how an
    /// engine replays a journal whose entries are already complete.
    Immediate,
    /// Holds and wake order drawn from a seeded generator.
    Perturbed {
        /// The generator's seed.
        seed: u64,
    },
}

/// SplitMix64: small, seedable, and the same on every platform. Engine legs
/// use it to perturb their own scheduling under a check's seed.
#[derive(Clone, Debug)]
pub struct SeededRng {
    state: u64,
}

impl SeededRng {
    /// A generator seeded with `seed`.
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next value.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }

    /// A value in `0..bound`; `bound` must be positive.
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound.max(1)
    }

    /// Shuffle `items` in place (Fisher-Yates).
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for index in (1..items.len()).rev() {
            let bound = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            let pick = usize::try_from(self.below(bound)).unwrap_or(0);
            items.swap(index, pick);
        }
    }
}

/// The scheduler state of one run: the policy plus its generator.
#[derive(Debug)]
pub(super) struct Scheduler {
    rng: Option<SeededRng>,
}

impl Scheduler {
    pub(super) fn new(schedule: Schedule) -> Self {
        Self {
            rng: match schedule {
                Schedule::Immediate => None,
                Schedule::Perturbed { seed } => Some(SeededRng::new(seed)),
            },
        }
    }

    /// Rounds a newly settled operation is held back before delivery.
    pub(super) fn hold(&mut self) -> u32 {
        self.rng
            .as_mut()
            .map_or(0, |rng| u32::try_from(rng.below(MAX_HOLD + 1)).unwrap_or(0))
    }

    /// Order the operations released in one round.
    pub(super) fn order<T>(&mut self, released: &mut [T]) {
        if let Some(rng) = self.rng.as_mut() {
            rng.shuffle(released);
        }
    }
}

/// Derive the `index`-th perturbation seed of a check from its seed.
pub(super) fn derived_seed(seed: u64, index: u32) -> u64 {
    let mut rng = SeededRng::new(seed ^ 0xD1B5_4A32_D192_ED03);
    let mut value = rng.next_u64();
    for _ in 0..index {
        value = rng.next_u64();
    }
    value
}
