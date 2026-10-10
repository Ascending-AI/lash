//! A profile of sampled guest contexts: the stacks the samples saw, folded
//! as Inferno reads them, and the count of every tick's outcome.
//!
//! A sample is where the machine was when a timer fired: it measures wall
//! residency of a run, not CPU time. The sampler that fires the timer is
//! the embedder's; this only reads and counts.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::context::{GuestContext, GuestPhase, GuestSample, ReadRefused};
use super::symbols::GuestSymbols;

/// How many times one tick reads a context that the machine keeps writing
/// before it gives the tick up.
pub const READS_PER_TICK: u32 = 16;

/// What every tick of a profile came to. The outcomes of the ticks are
/// exclusive:
/// `ticks = accepted + inactive + parked + lost_to_races + missed + unknown_phase`,
/// and the accepted samples split by phase:
/// `accepted = interpret + native + formula + collect`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Completeness {
    /// Deadlines the sampler's schedule reached.
    pub ticks: u64,
    /// Samples of a running machine, folded.
    pub accepted: u64,
    /// Samples of no `run` under way.
    pub inactive: u64,
    /// Samples of a parked run: the embedder held the requests.
    pub parked: u64,
    /// Ticks whose every read raced a write.
    pub lost_to_races: u64,
    /// Deadlines that passed without a read: the sampler ran late.
    pub missed: u64,
    /// Ticks that read a phase no machine writes.
    pub unknown_phase: u64,
    /// Reads refused because the machine wrote meanwhile, counting each
    /// retry.
    pub races: u64,
    /// Accepted samples deeper than the context keeps.
    pub truncated: u64,
    pub interpret: u64,
    pub native: u64,
    pub formula: u64,
    pub collect: u64,
}

impl Completeness {
    /// Whether the outcomes account for every tick and the phases for
    /// every accepted sample.
    pub fn adds_up(&self) -> bool {
        self.ticks
            == self.accepted
                + self.inactive
                + self.parked
                + self.lost_to_races
                + self.missed
                + self.unknown_phase
            && self.accepted == self.interpret + self.native + self.formula + self.collect
            && self.truncated <= self.accepted
    }
}

/// A frame of a folded stack, by token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Fold {
    Truncated,
    /// The machine's own work with no frame: a task that has just ended,
    /// or the end of the run.
    Machine,
    Code(u32),
    Native(u32),
    Formula(u32),
    Collect,
}

/// The profile one sampler builds.
#[derive(Clone, Debug, Default)]
pub struct GuestProfile {
    completeness: Completeness,
    stacks: BTreeMap<Vec<Fold>, u64>,
    /// Interpreted samples by their innermost frame's code and statement.
    statements: BTreeMap<(u32, u32), u64>,
    sample: GuestSample,
    key: Vec<Fold>,
}

/// One symbol's samples: as the innermost frame, and anywhere in a stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolTime {
    /// The frame's name in the folded stacks.
    pub frame: String,
    pub self_samples: u64,
    pub inclusive_samples: u64,
}

impl GuestProfile {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn completeness(&self) -> Completeness {
        self.completeness
    }

    /// Deadlines that passed without a tick.
    pub fn missed(&mut self, deadlines: u64) {
        self.completeness.ticks += deadlines;
        self.completeness.missed += deadlines;
    }

    /// One tick: reads the context, retrying a raced read, and counts what
    /// it saw.
    pub fn tick(&mut self, context: &GuestContext) {
        let counts = &mut self.completeness;
        counts.ticks += 1;
        let mut read = Err(ReadRefused::Raced);
        for _ in 0..READS_PER_TICK {
            read = context.read(&mut self.sample);
            if read != Err(ReadRefused::Raced) {
                break;
            }
            counts.races += 1;
            std::hint::spin_loop();
        }
        match read {
            Err(ReadRefused::Raced) => counts.lost_to_races += 1,
            Err(ReadRefused::Unknown) => counts.unknown_phase += 1,
            Ok(()) => self.record(),
        }
    }

    fn record(&mut self) {
        let counts = &mut self.completeness;
        let sample = &self.sample;
        let leaf = match sample.phase {
            GuestPhase::Inactive => {
                counts.inactive += 1;
                return;
            }
            GuestPhase::Park => {
                counts.parked += 1;
                return;
            }
            GuestPhase::Interpret => {
                counts.interpret += 1;
                None
            }
            GuestPhase::Native(lib) => {
                counts.native += 1;
                Some(Fold::Native(lib))
            }
            GuestPhase::Formula(lib) => {
                counts.formula += 1;
                Some(Fold::Formula(lib))
            }
            GuestPhase::Collect => {
                counts.collect += 1;
                Some(Fold::Collect)
            }
        };
        counts.accepted += 1;
        self.key.clear();
        if sample.truncated() {
            counts.truncated += 1;
            self.key.push(Fold::Truncated);
        }
        self.key
            .extend(sample.frames.iter().map(|frame| Fold::Code(frame.code)));
        self.key.extend(leaf);
        if self.key.is_empty() {
            self.key.push(Fold::Machine);
        }
        match self.stacks.get_mut(self.key.as_slice()) {
            Some(count) => *count += 1,
            None => {
                self.stacks.insert(self.key.clone(), 1);
            }
        }
        if leaf.is_none()
            && let Some(frame) = sample.frames.last()
            && !sample.truncated()
        {
            *self
                .statements
                .entry((frame.code, frame.statement))
                .or_default() += 1;
        }
    }

    fn name(fold: Fold, symbols: &GuestSymbols) -> String {
        let clean = |name: &str| name.replace([';', ' '], "_");
        match fold {
            Fold::Truncated => "[truncated]".to_string(),
            Fold::Machine => "[machine]".to_string(),
            Fold::Collect => "[collect]".to_string(),
            Fold::Code(token) => symbols.code(token).map_or_else(
                || format!("[unknown code {token}]"),
                |code| clean(&code.name),
            ),
            Fold::Native(token) => symbols.lib(token).map_or_else(
                || format!("[unknown native {token}]"),
                |lib| format!("native:{}", clean(&lib.name)),
            ),
            Fold::Formula(token) => symbols.lib(token).map_or_else(
                || format!("[unknown formula {token}]"),
                |lib| format!("formula:{}", clean(&lib.name)),
            ),
        }
    }

    /// The accepted samples as folded stacks, one line each: frames from
    /// the outermost, separated by `;`, a space and the count. `root`, when
    /// given, is every stack's first frame. The counts add up to the
    /// accepted samples.
    pub fn folded(&self, root: Option<&str>, symbols: &GuestSymbols) -> String {
        let mut lines: BTreeMap<String, u64> = BTreeMap::new();
        for (stack, count) in &self.stacks {
            let mut line = root
                .map(|root| root.replace([';', ' '], "_"))
                .unwrap_or_default();
            for fold in stack {
                if !line.is_empty() {
                    line.push(';');
                }
                line.push_str(&Self::name(*fold, symbols));
            }
            *lines.entry(line).or_default() += count;
        }
        let mut folded = String::new();
        for (line, count) in lines {
            let _ = writeln!(folded, "{line} {count}");
        }
        folded
    }

    /// Accepted samples that hold a token `symbols` does not name.
    pub fn unresolved(&self, symbols: &GuestSymbols) -> u64 {
        self.stacks
            .iter()
            .filter(|(stack, _)| {
                stack.iter().any(|fold| match fold {
                    Fold::Code(token) => symbols.code(*token).is_none(),
                    Fold::Native(token) | Fold::Formula(token) => symbols.lib(*token).is_none(),
                    Fold::Truncated | Fold::Machine | Fold::Collect => false,
                })
            })
            .map(|(_, count)| count)
            .sum()
    }

    /// Every frame's samples, the most inclusive first: self counts a
    /// sample whose innermost frame it is, inclusive one it appears in,
    /// once however often.
    pub fn symbol_times(&self, symbols: &GuestSymbols) -> Vec<SymbolTime> {
        let mut times: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for (stack, count) in &self.stacks {
            let names: Vec<String> = stack
                .iter()
                .map(|fold| Self::name(*fold, symbols))
                .collect();
            if let Some(leaf) = names.last() {
                times.entry(leaf.clone()).or_default().0 += count;
            }
            let mut seen: Vec<&String> = Vec::with_capacity(names.len());
            for name in &names {
                if !seen.contains(&name) {
                    seen.push(name);
                    times.entry(name.clone()).or_default().1 += count;
                }
            }
        }
        let mut times: Vec<SymbolTime> = times
            .into_iter()
            .map(|(frame, (self_samples, inclusive_samples))| SymbolTime {
                frame,
                self_samples,
                inclusive_samples,
            })
            .collect();
        times.sort_by(|a, b| {
            b.inclusive_samples
                .cmp(&a.inclusive_samples)
                .then(b.self_samples.cmp(&a.self_samples))
                .then(a.frame.cmp(&b.frame))
        });
        times
    }

    /// Interpreted samples by the statement their innermost frame was at:
    /// the code's name, the statement's site, and the count, most first.
    pub fn statement_times(&self, symbols: &GuestSymbols) -> Vec<(String, String, u64)> {
        let mut rows: Vec<(String, String, u64)> = self
            .statements
            .iter()
            .map(|((code, statement), count)| {
                let site = symbols
                    .statement(*statement)
                    .map_or_else(|| "-".to_string(), ToString::to_string);
                (Self::name(Fold::Code(*code), symbols), site, *count)
            })
            .collect();
        rows.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
        rows
    }
}
