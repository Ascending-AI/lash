//! The chaos soak (FIG-3873): randomized workloads under deployment kills,
//! leader-lease loss and rolling deploys, checked against the crash matrix's
//! end-state invariants.
//!
//! A soak runs **epochs** until its duration is spent. Each epoch builds one
//! [`CrashWorld`](crate::crash_matrix::world::CrashWorld) — lash-restate's
//! engine on the in-process server double over a SQLite memory store set,
//! with one deployment — and runs the seeded [`plan`] of its seed:
//!
//! - **Workload.** Sends and batched sends on plain sessions, session
//!   commands, held roots that are cancelled or whose session is deleted
//!   (some with a child process that lives until the root ends), session
//!   deletes, child sessions, and Lashlang processes that sleep and finish
//!   with an engine waiter on their terminal.
//! - **Faults.** The deployment killed where it stands, a first attempt of a
//!   lash invocation cut at a seeded journal command, the host killed at one
//!   of the crash matrix's seam boundaries, the recovery leader lease taken by
//!   another holder for a few ticks, and rolling deploys: build N+1 is
//!   registered, the deployment moves onto it, generation N is drained from
//!   it and N's build is removed once N holds nothing.
//! - **Time.** Recovery ticks move the server's virtual clock by one
//!   jittered `T` each.
//!
//! After its last step an epoch clears every armed fault and ticks recovery
//! until the end state holds ([`checks`]): the crash matrix's
//! [`invariants`](crate::crash_matrix::invariants) — every admitted input
//! driven exactly once, every obligation settled or stalled typed, no orphaned
//! child, every closed scope settled, every deleted session deleted, no drive
//! wedged — plus the soak's own: an admission the host never saw answered
//! took effect at most once, every engine waiter was answered, every root's
//! scope close delivered, and every retired generation holds nothing. A live
//! session must then still drive a fresh input.
//!
//! **Replay.** An epoch's plan is a function of its seed alone, and epoch 0's
//! seed is the soak's. A failed epoch prints its seed and the trace of the
//! steps it ran; `LASH_CHAOS_SOAK_SEED=<seed> LASH_CHAOS_SOAK_EPOCHS=1` runs
//! exactly that plan again, and `LASH_CHAOS_SOAK_STEPS=<n>` cuts it to its
//! first `n` steps. Task interleavings on the multi-threaded runtime are not
//! seeded, so a failure that needs a particular race may take a few replays.

pub mod checks;
pub mod driver;
pub mod findings;
pub mod host;
pub mod plan;

use std::time::{Duration, Instant};

use crate::crash_matrix::invariants;

/// The recovery ticks an epoch's end may take to reach its end state: a
/// quarter hour of virtual time, past every §1.8 bound but the attempt
/// ceiling (which a soak without refusals never meets).
const FINAL_TICKS: usize = 90;

/// How long one epoch may run in wall time.
const EPOCH_WALL_LIMIT: Duration = Duration::from_secs(30 * 60);

/// Steps per epoch when `LASH_CHAOS_SOAK_STEPS` is unset.
pub const DEFAULT_STEPS: usize = 200;

/// What a soak runs.
#[derive(Clone, Debug)]
pub struct SoakConfig {
    /// Epoch 0's seed; every later epoch's derives from it.
    pub seed: u64,
    /// Run epochs until this much wall time is spent (at least one).
    pub duration: Duration,
    /// Stop after this many epochs, when set.
    pub max_epochs: Option<usize>,
    /// The plan length of every epoch.
    pub steps: usize,
    /// Step kinds every epoch leaves out, for triage (a subset of
    /// [`plan::OPTIONAL_KINDS`]).
    pub without: Vec<String>,
}

impl SoakConfig {
    /// The soak the environment asks for: `LASH_CHAOS_SOAK_SEED` (decimal
    /// or `0x` hex), `LASH_CHAOS_SOAK_DURATION` (`90m`, `2h`, `120s`, or
    /// seconds), `LASH_CHAOS_SOAK_EPOCHS` and `LASH_CHAOS_SOAK_STEPS`, each
    /// over the given default.
    #[must_use]
    pub fn from_env(seed: u64, duration: Duration, max_epochs: Option<usize>) -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        Self {
            seed: var("LASH_CHAOS_SOAK_SEED")
                .and_then(|value| parse_seed(&value))
                .unwrap_or(seed),
            duration: var("LASH_CHAOS_SOAK_DURATION")
                .and_then(|value| parse_duration(&value))
                .unwrap_or(duration),
            max_epochs: var("LASH_CHAOS_SOAK_EPOCHS")
                .and_then(|value| value.parse().ok())
                .or(max_epochs),
            steps: var("LASH_CHAOS_SOAK_STEPS")
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_STEPS),
            without: var("LASH_CHAOS_SOAK_WITHOUT")
                .map(|kinds| {
                    kinds
                        .split(',')
                        .map(|kind| kind.trim().to_owned())
                        .filter(|kind| plan::OPTIONAL_KINDS.contains(&kind.as_str()))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// A seed in decimal or `0x` hex.
#[must_use]
pub fn parse_seed(value: &str) -> Option<u64> {
    let value = value.trim();
    match value.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => value.parse().ok(),
    }
}

/// A duration as `<n>h`, `<n>m`, `<n>s` or bare seconds.
#[must_use]
pub fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    let (number, unit) = match value.char_indices().last()? {
        (at, unit @ ('h' | 'm' | 's')) => (&value[..at], unit),
        _ => (value, 's'),
    };
    let number: u64 = number.parse().ok()?;
    Some(Duration::from_secs(match unit {
        'h' => number * 3_600,
        'm' => number * 60,
        _ => number,
    }))
}

/// Epoch `index`'s seed: the soak's own for epoch 0, a mix of it after.
#[must_use]
pub fn epoch_seed(seed: u64, index: usize) -> u64 {
    if index == 0 {
        return seed;
    }
    let mut mixed = seed ^ (index as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^ (mixed >> 31)
}

/// What one epoch did and found.
#[derive(Clone, Debug, Default)]
pub struct EpochReport {
    pub index: usize,
    pub seed: u64,
    pub steps: usize,
    /// The step kinds the epoch left out.
    pub without: Vec<String>,
    pub violations: Vec<String>,
    /// One line per step run: the step and what the host saw.
    pub trace: Vec<String>,
    /// Stalled obligations at the end, and what the epoch drew.
    pub notes: Vec<String>,
    pub counts: driver::Counts,
    pub inputs: usize,
    pub held: usize,
    pub processes: usize,
    pub deletes: usize,
    pub commands: usize,
    pub wall: Duration,
}

impl EpochReport {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }

    /// The one-line summary a soak prints per epoch.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "chaos soak epoch {} seed {:#x}: {} steps, {} inputs, {} held, {} commands, {} processes, {} deletes; {} kills, {} crashes, {} rolls, {}/{} lease losses taken, {} ticks; {:?}; {}",
            self.index,
            self.seed,
            self.steps,
            self.inputs,
            self.held,
            self.commands,
            self.processes,
            self.deletes,
            self.counts.kills,
            self.counts.crashes,
            self.counts.rolls,
            self.counts.lease_losses_taken,
            self.counts.lease_losses,
            self.counts.ticks,
            self.wall,
            if self.passed() {
                "passed".to_owned()
            } else {
                format!("FAILED with {} violation(s)", self.violations.len())
            }
        )
    }

    /// A failed epoch's evidence: its violations, how to replay it, and its
    /// trace.
    #[must_use]
    pub fn evidence(&self) -> String {
        let mut out = format!(
            "{}\nreplay: LASH_CHAOS_SOAK_SEED={:#x} LASH_CHAOS_SOAK_EPOCHS=1 LASH_CHAOS_SOAK_STEPS={}{}\nviolations:\n",
            self.summary(),
            self.seed,
            self.steps,
            if self.without.is_empty() {
                String::new()
            } else {
                format!(" LASH_CHAOS_SOAK_WITHOUT={}", self.without.join(","))
            }
        );
        for violation in &self.violations {
            out.push_str(&format!("  - {violation}\n"));
        }
        out.push_str("notes:\n");
        for note in &self.notes {
            out.push_str(&format!("  - {note}\n"));
        }
        out.push_str("trace:\n");
        for line in &self.trace {
            out.push_str(&format!("  {line}\n"));
        }
        out
    }
}

/// Run one epoch: its plan of `steps` steps under `seed` (less the kinds
/// `without` names), then its end.
pub async fn run_epoch(index: usize, seed: u64, steps: usize, without: &[String]) -> EpochReport {
    let started = Instant::now();
    let mut report = EpochReport {
        index,
        seed,
        steps,
        without: without.to_vec(),
        ..EpochReport::default()
    };
    if tokio::time::timeout(
        EPOCH_WALL_LIMIT,
        Box::pin(run_epoch_inner(seed, steps, without, &mut report)),
    )
    .await
    .is_err()
    {
        report.violations.push(format!(
            "the epoch ran past {EPOCH_WALL_LIMIT:?} of wall time"
        ));
    }
    report.wall = started.elapsed();
    report
}

async fn run_epoch_inner(seed: u64, steps: usize, without: &[String], report: &mut EpochReport) {
    let mut driver = match driver::Driver::new(seed).await {
        Ok(driver) => driver,
        Err(error) => {
            report.violations.push(format!("build the world: {error}"));
            return;
        }
    };
    for (index, step) in plan::plan(seed, steps, without).iter().enumerate() {
        let at_ms = driver.world.now_ms();
        match driver.step(seed, step).await {
            Ok(outcome) => report
                .trace
                .push(format!("#{index} @{at_ms} {step:?} -> {outcome}")),
            Err(error) => {
                report
                    .trace
                    .push(format!("#{index} @{at_ms} {step:?} -> FAILED: {error}"));
                report
                    .violations
                    .push(format!("step #{index} {step:?}: {error}"));
                break;
            }
        }
    }
    Box::pin(finish(&mut driver, report)).await;
    let ledger = &driver.ledger;
    report.inputs = ledger.inputs.len();
    report.held = ledger.held.len();
    report.processes = ledger.processes.len();
    report.deletes = ledger
        .sessions
        .iter()
        .filter(|slot| slot.deleted.is_some())
        .count();
    report.commands = ledger.commands;
    report.counts = driver.counts.clone();
    driver.world.finish().await;
}

/// The epoch's end: disarm every fault, tick recovery until the end state
/// holds, and probe every live session.
async fn finish(driver: &mut driver::Driver, report: &mut EpochReport) {
    if let Ok(double) = driver.world.double() {
        double.server().clear_crashes();
    }
    driver.world.faults().clear();
    if let Err(error) = driver.settle_crash().await {
        report.violations.push(format!("the last restart: {error}"));
        return;
    }
    let expected = match checks::expected(&driver.world, &driver.ledger).await {
        Ok(expected) => expected,
        Err(error) => {
            report
                .violations
                .push(format!("resolve the end state: {error}"));
            return;
        }
    };
    let mut last = Vec::new();
    let mut held = false;
    for tick in 0..=FINAL_TICKS {
        driver.world.quiesce().await;
        last = invariants::check(&driver.world, &expected).await;
        if last.is_empty() {
            held = true;
            report
                .notes
                .push(format!("the end state held after {tick} final tick(s)"));
            break;
        }
        if tick < FINAL_TICKS
            && let Err(error) = driver.tick().await
        {
            report.violations.push(format!("a final tick: {error}"));
            break;
        }
    }
    if held {
        // A held root the soak never released keeps its session's lane by
        // design, so a probe input there would queue behind it: the probe
        // asks only the live sessions that hold no held root.
        let probed = invariants::Expected {
            live_sessions: expected
                .live_sessions
                .iter()
                .filter(|session| {
                    !driver
                        .ledger
                        .held
                        .iter()
                        .any(|held| held.session == **session)
                })
                .cloned()
                .collect(),
            ..invariants::Expected::default()
        };
        report
            .violations
            .extend(invariants::probe_live_sessions(&driver.world, &probed, 6).await);
    } else {
        report.violations.push(format!(
            "the end state never held within {FINAL_TICKS} recovery ticks"
        ));
        report.violations.extend(last);
        report.violations.extend(
            invariants::diagnose(&driver.world)
                .await
                .into_iter()
                .chain(checks::diagnose_recovery(&driver.world, driver.live_since_wall_ms).await)
                .map(|line| format!("diagnosis: {line}")),
        );
    }
    report.notes.extend(checks::stalls(&driver.world).await);
}

/// What a soak did.
#[derive(Clone, Debug)]
pub struct SoakReport {
    pub config: SoakConfig,
    pub epochs: Vec<EpochReport>,
    pub wall: Duration,
}

impl SoakReport {
    #[must_use]
    pub fn failed(&self) -> Vec<&EpochReport> {
        self.epochs.iter().filter(|epoch| !epoch.passed()).collect()
    }
}

/// Run epochs until `config`'s duration or epoch count is spent, printing
/// each epoch's summary as it ends and each failed epoch's evidence.
pub async fn run(config: SoakConfig) -> SoakReport {
    let started = Instant::now();
    println!(
        "chaos soak: seed {:#x}, duration {:?}, {} steps per epoch, without {:?}, epochs {}",
        config.seed,
        config.duration,
        config.steps,
        config.without,
        config.max_epochs.map_or_else(
            || "until the duration".to_owned(),
            |epochs| epochs.to_string()
        )
    );
    // `LASH_CHAOS_SOAK_TRACE=1` prints every epoch's trace, not only a
    // failed one's.
    let trace = std::env::var("LASH_CHAOS_SOAK_TRACE").is_ok_and(|value| value == "1");
    let mut epochs = Vec::new();
    for index in 0.. {
        if config.max_epochs.is_some_and(|max| index >= max)
            || (index > 0 && started.elapsed() >= config.duration)
        {
            // Whichever limit is spent first ends the soak.
            break;
        }
        let epoch = Box::pin(run_epoch(
            index,
            epoch_seed(config.seed, index),
            config.steps,
            &config.without,
        ))
        .await;
        println!("{}", epoch.summary());
        if !epoch.passed() || trace {
            println!("{}", epoch.evidence());
        }
        epochs.push(epoch);
    }
    let report = SoakReport {
        config,
        epochs,
        wall: started.elapsed(),
    };
    println!(
        "chaos soak: {} epoch(s) in {:?}, {} failed",
        report.epochs.len(),
        report.wall,
        report.failed().len()
    );
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_seeds_parse() {
        assert_eq!(parse_duration("90m"), Some(Duration::from_secs(5_400)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7_200)));
        assert_eq!(parse_duration("120s"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("m"), None);
        assert_eq!(parse_seed("0x3873"), Some(0x3873));
        assert_eq!(parse_seed("42"), Some(42));
        assert_eq!(epoch_seed(9, 0), 9);
        assert_ne!(epoch_seed(9, 1), epoch_seed(9, 2));
    }
}
