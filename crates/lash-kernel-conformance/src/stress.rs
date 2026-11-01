//! Whole-corpus stress modes (FIG-7589): a dialect's corpus runs once plain
//! and once in a mode, and the two runs must agree on how they ended, what
//! they printed, the requests they made, their exact charge and the session
//! they carried. Test262 and the Python corpus both judge through
//! [`first_divergence`].
//!
//! - **Park everywhere** seals the run and opens it cold at every park and
//!   at the charge points [`stops`] picks, rotating through the layouts
//!   PLAN-INDEP rotates through (`K-MACH-007`).
//! - **Bodies only** runs on [`installed_bodies`]: every library function a
//!   kernel body defines answers from that body (`K-LIB-006`, `K-CHG-007`).

use lash_kernel_doc::Datum;
use lash_kernel_state::paged::PageSource;
use lash_kernel_vm::kernel_internals::{self, Layout};
use lash_kernel_vm::{
    Bounds, CellStart, End, Host, KernelMachine, Machine, Outcome, Park, Program, Request, Step,
    WaitId,
};

/// The layouts a reopened run takes by turns: the native declining, the
/// prepared entries off, and a shuffled table order.
const TURNS: [Layout; 3] = [Layout(3), Layout(1), Layout(0x5eed)];

/// How many interior charge points a park-everywhere run of a case reopens
/// at besides its first and last unit.
const DRAWN: usize = 8;

/// How a run is driven between its start and its end.
#[derive(Clone, Debug)]
pub enum Drive {
    /// One unbounded slice per park.
    Straight,
    /// Sealed and opened cold at every park and at every charge point,
    /// ascending.
    ParkEverywhere { stops: Vec<u64> },
}

/// The charge points a park-everywhere run of `case` reopens at, given that
/// its plain run charged `charged`: its first unit, its last, and [`DRAWN`]
/// more drawn from a hash of the case's name. Sealing at every unit of every
/// case costs far more than the nightly can carry; a fixed draw per case
/// keeps a divergence reproducible.
#[must_use]
pub fn stops(case: &str, charged: u64) -> Vec<u64> {
    if charged < 2 {
        return Vec::new();
    }
    let interior = charged - 1;
    let wanted = usize::try_from(interior).map_or(DRAWN + 2, |all| all.min(DRAWN + 2));
    let mut points = std::collections::BTreeSet::from([1, interior]);
    // FNV-1a of the name seeds a SplitMix64 sequence.
    let mut state = case.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    while points.len() < wanted {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed = state;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^= mixed >> 31;
        points.insert(1 + mixed % interior);
    }
    points.into_iter().collect()
}

/// What a driven run delivers at a park, or why the host refuses it.
pub type Answer<'a> = dyn FnMut(&Park) -> Result<Vec<(WaitId, Outcome)>, String> + 'a;

/// A driven run: how it ended, every request its parks handed out, and its
/// exact charge.
#[derive(Debug)]
pub struct Run {
    pub end: End,
    pub requests: Vec<Request>,
    pub charged: u64,
}

/// Runs `program` from `start` to its end. `source` opens the page store
/// the run seals into; `answer` gives the outcomes to deliver at a park.
///
/// # Errors
///
/// The machine's refusal to start, run, seal, open or take a delivery, or
/// `answer`'s refusal of a park.
pub fn drive(
    program: &Program,
    bounds: Bounds,
    start: CellStart,
    source: &dyn Fn() -> Box<dyn PageSource>,
    host: &mut dyn Host,
    drive: &Drive,
    answer: &mut Answer<'_>,
) -> Result<Run, String> {
    let mut machine = KernelMachine::start(program.clone(), bounds, start, source())
        .map_err(|error| format!("start: {error}"))?;
    let (reopening, stops) = match drive {
        Drive::Straight => (false, &[][..]),
        Drive::ParkEverywhere { stops } => (true, stops.as_slice()),
    };
    let mut stops = stops.iter().copied().peekable();
    let mut turns = TURNS.iter().copied().cycle();
    let mut reopen = |machine: &mut KernelMachine| -> Result<(), String> {
        let sealed = machine
            .checkpoint()
            .map_err(|error| format!("seal: {error}"))?;
        let layout = turns.next().unwrap_or_default();
        *machine = kernel_internals::open_with_layout(
            program.clone(),
            bounds,
            kernel_internals::sealed_root(&sealed),
            source(),
            layout,
        )
        .map_err(|error| format!("open in {layout:?}: {error}"))?;
        Ok(())
    };
    let mut requests = Vec::new();
    loop {
        let charged = machine.meters().charged.unwrap_or(0);
        while stops.next_if(|stop| *stop <= charged).is_some() {}
        let slice = stops.peek().map_or(u64::MAX, |stop| stop - charged);
        match machine
            .run(host, slice)
            .map_err(|error| format!("run: {error}"))?
        {
            Step::Ended(end) => {
                return Ok(Run {
                    end,
                    requests,
                    charged: machine.meters().charged.unwrap_or(0),
                });
            }
            Step::Slice => {
                if reopening {
                    reopen(&mut machine)?;
                }
            }
            Step::Parked(park) => {
                requests.extend(park.requests.iter().cloned());
                if reopening {
                    reopen(&mut machine)?;
                }
                for (wait, outcome) in answer(&park)? {
                    machine
                        .deliver(wait, outcome)
                        .map_err(|error| format!("deliver: {error}"))?;
                }
            }
        }
    }
}

/// What two runs of a case are compared on. The carried session is its
/// reachable graph: a run reopened cold may lay its pages out otherwise.
#[derive(Debug, PartialEq)]
pub struct Observation {
    pub end: End,
    pub prints: Vec<Datum>,
    pub requests: Vec<Request>,
    pub charged: u64,
    pub carry: Option<lash_kernel_state::semantic::Checkpoint>,
}

impl Observation {
    /// `run`'s observation, with the lines its host printed; `source`
    /// reads the session it carried.
    ///
    /// # Errors
    ///
    /// The carried session cannot be read.
    pub fn of(
        mut run: Run,
        prints: Vec<Datum>,
        source: Box<dyn PageSource>,
    ) -> Result<Self, String> {
        let carry = match &mut run.end {
            End::Finished(finished) => finished
                .carry
                .take()
                .map(|root| lash_kernel_state::semantic::session(&root, source))
                .transpose()
                .map_err(|error| format!("read the carried session: {error}"))?,
            _ => None,
        };
        Ok(Self {
            end: run.end,
            prints,
            requests: run.requests,
            charged: run.charged,
            carry,
        })
    }
}

/// The first field in which `mode` differs from `plain`, in the order end,
/// prints, requests, charge, carry, with both values shown.
#[must_use]
pub fn first_divergence(plain: &Observation, mode: &Observation) -> Option<(&'static str, String)> {
    fn shown(
        field: &'static str,
        plain: &dyn std::fmt::Debug,
        mode: &dyn std::fmt::Debug,
    ) -> (&'static str, String) {
        let clip = |value: &dyn std::fmt::Debug| {
            let text = format!("{value:?}");
            match text.char_indices().nth(400) {
                Some((at, _)) => format!("{}…", &text[..at]),
                None => text,
            }
        };
        (
            field,
            format!("plain {} / mode {}", clip(plain), clip(mode)),
        )
    }
    if plain.end != mode.end {
        return Some(shown("end", &plain.end, &mode.end));
    }
    if plain.prints != mode.prints {
        return Some(shown("prints", &plain.prints, &mode.prints));
    }
    if plain.requests != mode.requests {
        return Some(shown("requests", &plain.requests, &mode.requests));
    }
    if plain.charged != mode.charged {
        return Some(shown("charge", &plain.charged, &mode.charged));
    }
    if plain.carry != mode.carry {
        return Some(shown("carry", &plain.carry, &mode.carry));
    }
    None
}

/// A whole-corpus stress mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Sealed and opened cold at every park and every drawn charge point,
    /// in the layouts PLAN-INDEP rotates through.
    ParkEverywhere,
    /// Every library function a kernel body defines answers from it.
    BodiesOnly,
}

impl Mode {
    /// The mode's name, which also names its ratchet shard.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::ParkEverywhere => "park-everywhere",
            Self::BodiesOnly => "bodies-only",
        }
    }
}

/// How a case ran in a mode beside its plain run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Agreement {
    /// The plain run did not pass, or nothing ran.
    Skipped,
    Agreed,
    /// The first field the runs differ in, and both its values.
    Diverged {
        field: &'static str,
        detail: String,
    },
}

impl Agreement {
    /// `mode` against `plain`: a run the mode could not finish diverges in
    /// `run`.
    #[must_use]
    pub fn of(plain: &Observation, mode: Result<Observation, String>) -> Self {
        match mode {
            Err(detail) => Self::Diverged {
                field: "run",
                detail,
            },
            Ok(mode) => match first_divergence(plain, &mode) {
                None => Self::Agreed,
                Some((field, detail)) => Self::Diverged { field, detail },
            },
        }
    }
}

/// A mode's ratchet shard, `shard` its text: each known divergence's case
/// with the field its runs first differ in and the ticket that owns it.
///
/// # Panics
///
/// A row that is not a case, a field and a `FIG-` ticket.
#[must_use]
pub fn known_divergences(shard: &str) -> std::collections::BTreeMap<String, (String, String)> {
    shard
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|line| {
            let fields: Vec<_> = line.split('\t').collect();
            let [case, field, ticket] = fields[..] else {
                panic!("a divergence row is a case, a field and a ticket: {line}")
            };
            assert!(ticket.starts_with("FIG-"), "{case} names no ticket");
            (case.to_string(), (field.to_string(), ticket.to_string()))
        })
        .collect()
}

/// What breaks the ratchet among `observations`: a divergence `known` does
/// not record in that field, and a recorded case that no longer diverges,
/// whose row must go.
#[must_use]
pub fn ratchet(
    observations: &[(String, Agreement)],
    known: &std::collections::BTreeMap<String, (String, String)>,
) -> Vec<String> {
    let mut problems = Vec::new();
    for (case, agreement) in observations {
        match (agreement, known.get(case)) {
            (Agreement::Diverged { field, .. }, Some((recorded, _))) if field == recorded => {}
            (Agreement::Diverged { field, detail }, _) => {
                problems.push(format!("{case}: a new divergence in {field}: {detail}"));
            }
            (_, Some((field, ticket))) => problems.push(format!(
                "{case} no longer diverges in {field}; remove its {ticket} row, the ratchet only shrinks"
            )),
            (_, None) => {}
        }
    }
    problems
}
