//! The chaos soak (design-opus §7.2, "Chaos"): every crash-matrix workload
//! at once on the production durable runtime, both nodes serving, under a
//! seeded plan of node kills and restarts, whole-node pauses (a zombie
//! past its lease), partitions (heartbeats fail while bodies run on), and,
//! on PostgreSQL, lock-timeout storms and database restarts.
//!
//! An epoch draws its plan from its seed ([`plan::draw`]), applies each
//! step at its virtual time, then heals every fault, drives the run to its
//! end and checks the crash matrix's invariants (fencing, no `Once` body
//! twice, no replay, the fold, every actor terminal or in a durable wait)
//! and every workload's own laws. A failed epoch replays from its seed; a
//! shorter plan of the same seed is a prefix of a longer one.
//!
//! Only a fault that fired counts. A step the deployment gave nothing to
//! act on (a database fault on SQLite, a pause of a node that is not
//! running, a partition of one that is not serving, a database restart
//! with no connection to end) is recorded and counts for nothing. A
//! database fault the deployment could take and its executor did not
//! inject fails the epoch, as does an epoch that reached none of the
//! faults its plan asks the deployment for.
//!
//! [`SoakConfig::from_env`] reads `LASH_CHAOS_SOAK_SEED`,
//! `LASH_CHAOS_SOAK_EPOCHS`, `LASH_CHAOS_SOAK_STEPS` and
//! `LASH_CHAOS_SOAK_WITHOUT` (step kinds to leave out, comma-separated).

pub mod driver;
pub mod plan;
mod postgres;

use std::time::{Duration, Instant};

use crate::crash_matrix::deployment::Dialect;

pub use driver::Epoch;

/// One soak's settings.
#[derive(Clone, Debug)]
pub struct SoakConfig {
    /// The first epoch's seed; epoch `n` runs at `seed + n`.
    pub seed: u64,
    /// How many epochs to run.
    pub epochs: usize,
    /// How many fault steps each epoch's plan has.
    pub steps: usize,
    /// Step kinds left out, by name.
    pub without: Vec<String>,
    /// The database every epoch runs over.
    pub dialect: Dialect,
    /// Stop starting epochs once this much wall time has passed.
    pub cap: Duration,
}

impl SoakConfig {
    /// The settings the environment states, defaulting to `seed`, `epochs`
    /// epochs of `steps` steps and `cap`, over `dialect`.
    #[must_use]
    pub fn from_env(
        seed: u64,
        epochs: usize,
        steps: usize,
        cap: Duration,
        dialect: Dialect,
    ) -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let parse_seed = |value: String| match value.strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16).ok(),
            None => value.parse().ok(),
        };
        Self {
            seed: var("LASH_CHAOS_SOAK_SEED")
                .and_then(parse_seed)
                .unwrap_or(seed),
            epochs: var("LASH_CHAOS_SOAK_EPOCHS")
                .and_then(|value| value.parse().ok())
                .unwrap_or(epochs),
            steps: var("LASH_CHAOS_SOAK_STEPS")
                .and_then(|value| value.parse().ok())
                .unwrap_or(steps),
            without: var("LASH_CHAOS_SOAK_WITHOUT")
                .map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|kind| !kind.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            dialect,
            cap,
        }
    }

    /// The step kinds left out, by name.
    #[must_use]
    pub fn without_names(&self) -> Vec<&str> {
        self.without.iter().map(String::as_str).collect()
    }
}

/// A soak's outcome: every epoch it ran.
#[derive(Clone, Debug, Default)]
pub struct SoakReport {
    pub epochs: Vec<Epoch>,
}

impl SoakReport {
    /// Every epoch that broke an invariant or a law.
    #[must_use]
    pub fn failed(&self) -> Vec<&Epoch> {
        self.epochs.iter().filter(|epoch| !epoch.passed()).collect()
    }

    /// Every failed epoch no open finding
    /// ([`crate::crash_matrix::findings::OPEN`]) explains.
    #[must_use]
    pub fn unexplained(&self) -> Vec<&Epoch> {
        self.failed()
            .into_iter()
            .filter(|epoch| {
                crate::crash_matrix::findings::explaining_epoch(&epoch.violations).is_none()
            })
            .collect()
    }
}

/// Run `config`'s epochs, one after the other, until they are done or the
/// wall-time cap passed.
pub async fn run(config: SoakConfig) -> SoakReport {
    let started = Instant::now();
    let mut report = SoakReport::default();
    for index in 0..config.epochs {
        if started.elapsed() >= config.cap {
            break;
        }
        let seed = config.seed.wrapping_add(index as u64);
        report.epochs.push(driver::epoch(&config, seed).await);
    }
    report
}
