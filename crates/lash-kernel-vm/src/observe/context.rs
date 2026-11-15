//! The guest context a sampler outside the machine reads: which task runs,
//! its frames as code and statement tokens, and what the machine is doing.
//!
//! The machine is the context's only writer. It changes the frame stack
//! under a sequence lock of atomics: a push, a pop or a switch of task makes
//! the generation odd, stores the slots and makes it even again, and a read
//! keeps what it read only when the generation was the same even number
//! before and after. The phase and the innermost frame's statement are each
//! one atomic store that leaves the stack's shape as it was, so they take
//! no lock: a read pairs them with the stack they belong to, and the
//! statement it reads may be one the frame reached during the read. A
//! reader never dereferences anything of the machine's, and never blocks
//! it. The machine starts no thread: the embedder owns the sampler.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

/// No statement: a frame pushed whose first statement has not begun.
pub const NO_STATEMENT: u32 = u32::MAX;

/// What the machine is doing when a sample reads its context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GuestPhase {
    /// No `run` is under way: before the first, between slices, or after
    /// the end.
    Inactive,
    /// A `run` interprets kernel code: statements, expressions and the
    /// machine's own bookkeeping.
    Interpret,
    /// A native implementation runs; the token is its library function.
    Native(u32),
    /// The charge formula or guard limit of a library function is
    /// evaluated; the token is the function.
    Formula(u32),
    /// The heap is collected.
    Collect,
    /// The last `run` parked: the embedder holds the requests.
    Park,
}

impl GuestPhase {
    fn encode(self) -> u64 {
        let (kind, token) = match self {
            Self::Inactive => (0, 0),
            Self::Interpret => (1, 0),
            Self::Native(token) => (2, token),
            Self::Formula(token) => (3, token),
            Self::Collect => (4, 0),
            Self::Park => (5, 0),
        };
        kind | (u64::from(token) << 32)
    }

    fn decode(bits: u64) -> Option<Self> {
        let token = (bits >> 32) as u32;
        Some(match bits & 0xff {
            0 => Self::Inactive,
            1 => Self::Interpret,
            2 => Self::Native(token),
            3 => Self::Formula(token),
            4 => Self::Collect,
            5 => Self::Park,
            _ => return None,
        })
    }
}

/// One frame of a task as a sample reads it: its code's token and the
/// token of the statement it is at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GuestFrame {
    pub code: u32,
    /// [`NO_STATEMENT`] before the frame's first statement begins.
    pub statement: u32,
}

impl GuestFrame {
    fn decode(bits: u64) -> Self {
        Self {
            code: bits as u32,
            statement: (bits >> 32) as u32,
        }
    }
}

/// What one read of a [`GuestContext`] saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuestSample {
    pub task: u64,
    pub phase: GuestPhase,
    /// How deep the task's frame stack is. Only the outermost
    /// [`GuestContext::DEPTH`] frames are kept: a deeper sample is
    /// truncated.
    pub depth: u32,
    /// The task's frames, outermost first.
    pub frames: Vec<GuestFrame>,
}

impl GuestSample {
    /// An empty sample whose frames have room for a whole context.
    pub fn new() -> Self {
        Self {
            task: 0,
            phase: GuestPhase::Inactive,
            depth: 0,
            frames: Vec::with_capacity(GuestContext::DEPTH),
        }
    }

    /// The sample's stack is deeper than the frames it kept.
    pub fn truncated(&self) -> bool {
        self.depth as usize > self.frames.len()
    }
}

impl Default for GuestSample {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a read kept nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadRefused {
    /// The machine wrote the context while it was read.
    Raced,
    /// The context held a phase no machine writes.
    Unknown,
}

/// The context one machine publishes and a sampler reads. Its storage is
/// allocated once, when it is made; publishing writes atomics only.
#[derive(Debug)]
pub struct GuestContext {
    generation: AtomicU64,
    task: AtomicU64,
    phase: AtomicU64,
    depth: AtomicU32,
    frames: Box<[AtomicU64]>,
}

impl GuestContext {
    /// The frames a context keeps: a profiler's capacity, not a bound on
    /// a guest's call depth.
    pub const DEPTH: usize = 512;

    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            generation: AtomicU64::new(0),
            task: AtomicU64::new(0),
            phase: AtomicU64::new(GuestPhase::Inactive.encode()),
            depth: AtomicU32::new(0),
            frames: (0..Self::DEPTH).map(|_| AtomicU64::new(0)).collect(),
        })
    }

    /// Allocation-free innermost guest code, statement and encoded phase.
    /// The allocating thread is also the publisher, so no write can race it.
    #[cfg(any(test, feature = "perf-witness"))]
    pub fn allocation_context(&self) -> Option<(u32, u32, u64)> {
        let depth = self.depth.load(Ordering::Relaxed) as usize;
        let frame = GuestFrame::decode(
            self.frames
                .get(depth.checked_sub(1)?)?
                .load(Ordering::Relaxed),
        );
        Some((
            frame.code,
            frame.statement,
            self.phase.load(Ordering::Relaxed),
        ))
    }

    /// Reads the context into `sample`, or refuses when the machine wrote
    /// it meanwhile. A refused read leaves `sample` unspecified.
    pub fn read(&self, sample: &mut GuestSample) -> Result<(), ReadRefused> {
        let before = self.generation.load(Ordering::Acquire);
        if before % 2 == 1 {
            return Err(ReadRefused::Raced);
        }
        let task = self.task.load(Ordering::Relaxed);
        let phase = self.phase.load(Ordering::Relaxed);
        let depth = self.depth.load(Ordering::Relaxed);
        sample.frames.clear();
        for slot in self.frames.iter().take(depth as usize) {
            sample
                .frames
                .push(GuestFrame::decode(slot.load(Ordering::Relaxed)));
        }
        fence(Ordering::Acquire);
        if self.generation.load(Ordering::Relaxed) != before {
            return Err(ReadRefused::Raced);
        }
        sample.task = task;
        sample.depth = depth;
        sample.phase = GuestPhase::decode(phase).ok_or(ReadRefused::Unknown)?;
        Ok(())
    }
}
