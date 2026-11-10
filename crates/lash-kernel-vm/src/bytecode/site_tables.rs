//! Each unit's resume and role tables (cutover contract §5).
//!
//! Persisted state names where a frame stands by document coordinates,
//! never by bytecode position (`K-SITE-001`): a site, the phase the frame
//! stands in there, and the values it holds, each by the expression that
//! produced it. The resume table maps that back to the pc dispatch goes on
//! from and the stack shape the frame must have there. The role table
//! fixes, for each action kind, which of its operands a frame still holds
//! once the action has issued.
//!
//! A frame can be saved at five kinds of point:
//! - under a callee's frame, at a call ([`Phase::Call`]);
//! - at an action before it issues ([`Phase::Ready`]);
//! - after an action issued and waits ([`Phase::Issued`]);
//! - after an action that completed at once, its value pushed, where the
//!   slice ended or a cancel forced a park ([`Phase::Went`]);
//! - at a step, where the slice is tested: a statement start
//!   ([`Phase::Statement`]) or a loop's back edge ([`Phase::Back`]).
//!
//! A frame's slots are named the same way: each by the site that declared
//! it and what it holds there (a variable, a value held while cleanup
//! runs, a loop's pass counter or its state), and each loop's state by the
//! loop's site.
//!
//! A site can be compiled more than once in a unit: a `finally` body is
//! lowered on each way out of its `try`. Its points and slots are told
//! apart by `copy`, their order in the unit's code.
//!
//! Role meanings and these tables are continuation format (contract §7).

use std::sync::Arc;

use lash_kernel_value::names::Name;
use lash_kernel_value::sites::Site;

use super::op::Op;
use super::program::{ActionSite, Callee, Unit};

/// An expression's coordinate: its path under the unit's site root.
pub type Path = Arc<[u32]>;

/// Where at a site a frame stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
    /// At a call, under the callee's frame: the frame goes on with the
    /// call's result pushed.
    Call,
    /// At an action, before it issues: the frame is the top frame, the
    /// action's atoms are unpaid and its operands are held.
    Ready,
    /// After an action issued: the frame waits, holding only the roles the
    /// role table keeps, and goes on with what the action resumes with
    /// pushed.
    Issued,
    /// After an action that completed at once: its value is on the stack.
    Went,
    /// At a statement's start, before its step.
    Statement,
    /// At a loop's back edge, before its step.
    Back,
}

impl Phase {
    /// Whether the frame goes on with one value pushed on top of
    /// [`Resume::stack`].
    pub fn pushes(self) -> bool {
        matches!(self, Self::Call | Self::Issued)
    }
}

/// What an action is, for its role table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActionKind {
    Perform,
    Spawn,
    Join,
    JoinMany,
    Yield,
    Cancel,
    CancelEffect,
}

impl ActionKind {
    pub fn of(action: &ActionSite) -> Self {
        match action {
            ActionSite::Perform { .. } => Self::Perform,
            ActionSite::Spawn { .. } => Self::Spawn,
            ActionSite::Join { .. } => Self::Join,
            ActionSite::JoinMany { .. } => Self::JoinMany,
            ActionSite::Yield { .. } => Self::Yield,
            ActionSite::Cancel { .. } => Self::Cancel,
            ActionSite::CancelEffect { .. } => Self::CancelEffect,
        }
    }

    /// The operands a frame still holds once the action has issued, by
    /// their index among the action's operands. A perform copies its
    /// arguments out and releases them at issue (`K-EFF-011`); a spawn
    /// moves its into the spawned task; a join, a list join and a cancel
    /// take their handle or list into the wait or the target.
    pub fn kept_after_issue(self) -> &'static [u8] {
        match self {
            Self::Perform
            | Self::Spawn
            | Self::Join
            | Self::JoinMany
            | Self::Yield
            | Self::Cancel
            | Self::CancelEffect => &[],
        }
    }

    /// Whether the action can issue and wait. The others complete at
    /// once, and the frame is saved after them only at [`Phase::Went`].
    pub fn waits(self) -> bool {
        match self {
            Self::Perform | Self::Spawn | Self::Join | Self::JoinMany | Self::Yield => true,
            Self::Cancel | Self::CancelEffect => false,
        }
    }
}

/// Where a frame goes on, and the stack it must hold there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resume {
    /// The op dispatch goes on from.
    pub pc: u32,
    /// The operand stack above the frame's locals, bottom first: each
    /// value by the expression that produced it, in `K-EVAL` order. At
    /// [`Phase::Ready`] it ends with the action's operands.
    pub stack: Arc<[Path]>,
}

/// One point a frame can be saved at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub site: Site,
    pub phase: Phase,
    /// Which compiled copy of the site, in code order, from 0.
    pub copy: u32,
    /// The action, at an action's phases.
    pub action: Option<ActionKind>,
    /// How many values at the top of the stack are the action's own
    /// roles: its operands at [`Phase::Ready`], what the role table keeps
    /// at [`Phase::Issued`].
    pub held: u8,
    pub resume: Resume,
}

/// What a frame's slot holds.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SlotRole {
    /// A parameter or a variable, by name.
    Variable(Name),
    /// A positional argument of a unit that names none: a function
    /// value's trampoline or a spawn's stub.
    Argument(u16),
    /// A value held while cleanup runs: a `return`'s value before its
    /// `finally` blocks or its `via` policy, or a raise's.
    Held,
    /// A loop's count of the passes it has begun (`K-EFF-008`).
    Passes,
    /// A `for` loop's state, by its place in [`LoopState::slots`].
    Loop(u8),
}

/// One slot of a frame, named by document coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Local {
    /// The site that declared it.
    pub site: Path,
    pub role: SlotRole,
    /// Which compiled copy of the declaring site, in slot order, from 0.
    pub copy: u32,
    pub slot: u16,
    /// The slot holds a cell (`K-CLO-001`): it is boxed at entry or when
    /// declared.
    pub cell: bool,
}

/// A `for` loop's state, in its slots' fixed order: what it walks and its
/// cursor, and for a protocol loop then the size, what remains, the raw
/// item and whether the step gave one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoopState {
    pub site: Path,
    /// Which compiled copy of the loop site, from 0.
    pub copy: u32,
    pub slots: Arc<[u16]>,
    /// The slot counting its passes.
    pub passes: Option<u16>,
}

/// A unit's resume table: every point a frame of it can be saved at, and
/// what each of its slots holds.
#[derive(Clone, Debug, Default)]
pub struct SiteTable {
    /// By site, phase and copy.
    entries: Vec<Entry>,
    /// Indexes into `entries`, by resume pc and phase.
    by_pc: Vec<u32>,
    /// By slot.
    locals: Vec<Local>,
    /// Indexes into `locals`, by site, role and copy.
    by_site: Vec<u16>,
    /// In loop order.
    loops: Vec<LoopState>,
}

impl SiteTable {
    /// Where a frame saved at `site` in `phase` goes on.
    pub fn resume(&self, site: &Site, phase: Phase, copy: u32) -> Option<&Entry> {
        self.entries
            .binary_search_by(|entry| {
                (&entry.site, entry.phase, entry.copy).cmp(&(site, phase, copy))
            })
            .ok()
            .map(|at| &self.entries[at])
    }

    /// The point a frame standing at `pc` in `phase` is saved as: the
    /// frame's pc is the op it goes on from.
    pub fn at(&self, pc: u32, phase: Phase) -> Option<&Entry> {
        self.by_pc
            .binary_search_by(|&at| {
                let entry = &self.entries[at as usize];
                (entry.resume.pc, entry.phase).cmp(&(pc, phase))
            })
            .ok()
            .map(|at| &self.entries[self.by_pc[at] as usize])
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// What slot `slot` holds.
    pub fn slot(&self, slot: u16) -> Option<&Local> {
        self.locals.get(usize::from(slot))
    }

    /// The slot declared at `site` for `role`.
    pub fn local(&self, site: &[u32], role: &SlotRole, copy: u32) -> Option<&Local> {
        self.by_site
            .binary_search_by(|&at| {
                let local = &self.locals[usize::from(at)];
                (&*local.site, &local.role, local.copy).cmp(&(site, role, copy))
            })
            .ok()
            .map(|at| &self.locals[usize::from(self.by_site[at])])
    }

    /// Every slot, by slot.
    pub fn locals(&self) -> &[Local] {
        &self.locals
    }

    /// Each `for` loop's state, in loop order (`LoopSite` index).
    pub fn loops(&self) -> &[LoopState] {
        &self.loops
    }
}

impl Entry {
    /// The action's own roles the frame holds at this point.
    pub fn held(&self) -> &[Path] {
        &self.resume.stack[self.resume.stack.len() - usize::from(self.held)..]
    }
}

/// The resume table of `unit`. Empty where its operand stack is
/// malformed: the validator refuses such a unit.
pub(crate) fn build(unit: &Unit) -> SiteTable {
    let root: Path = Arc::from(unit.site.path.as_slice());
    let origin = |pc: usize| unit.origin.get(pc).unwrap_or(&root).clone();
    let Some(stacks) = stacks(unit, &origin) else {
        return SiteTable::default();
    };
    let site = |pc: usize| Site::new(unit.site.unit.clone(), origin(pc).to_vec());
    let mut entries = Vec::new();
    for (pc, op) in unit.ops.iter().enumerate() {
        let Some(stack) = &stacks[pc] else {
            continue;
        };
        if !unit.slices(pc) && !matches!(op, Op::Call(_) | Op::Action(_)) {
            continue;
        }
        let stack = stack.to_vec();
        let next = pc as u32 + 1;
        if unit.slices(pc) {
            let phase = match op {
                Op::Jump(to) if (*to as usize) < pc => Phase::Back,
                _ => Phase::Statement,
            };
            entries.push(Entry {
                site: site(pc),
                phase,
                copy: 0,
                action: None,
                held: 0,
                resume: Resume {
                    pc: pc as u32,
                    stack: stack.clone().into(),
                },
            });
        }
        match *op {
            Op::Call(n) => {
                let call = &unit.calls[n as usize];
                let taken = usize::from(call.argc) + usize::from(call.callee == Callee::Value);
                let Some(split) = stack.len().checked_sub(taken) else {
                    continue;
                };
                entries.push(Entry {
                    site: call.site.clone(),
                    phase: Phase::Call,
                    copy: 0,
                    action: None,
                    held: 0,
                    resume: Resume {
                        pc: next,
                        stack: stack[..split].into(),
                    },
                });
            }
            Op::Action(n) => {
                let action = &unit.actions[n as usize];
                let kind = ActionKind::of(action);
                let taken = match action {
                    ActionSite::Perform { argc, .. } | ActionSite::Spawn { argc, .. } => {
                        usize::from(*argc)
                    }
                    ActionSite::Yield { .. } => 0,
                    ActionSite::Join { .. }
                    | ActionSite::JoinMany { .. }
                    | ActionSite::Cancel { .. }
                    | ActionSite::CancelEffect { .. } => 1,
                };
                let Some(split) = stack.len().checked_sub(taken) else {
                    continue;
                };
                let entry = |phase: Phase, pc: u32, stack: Vec<Path>, held: usize| Entry {
                    site: action.site().clone(),
                    phase,
                    copy: 0,
                    action: Some(kind),
                    held: held as u8,
                    resume: Resume {
                        pc,
                        stack: stack.into(),
                    },
                };
                entries.push(entry(Phase::Ready, pc as u32, stack.clone(), taken));
                let mut after: Vec<Path> = stack[..split].to_vec();
                let operands = &stack[split..];
                if kind.waits() {
                    let kept = kind.kept_after_issue();
                    let mut issued = after.clone();
                    issued.extend(kept.iter().map(|&at| operands[usize::from(at)].clone()));
                    entries.push(entry(Phase::Issued, next, issued, kept.len()));
                }
                after.push(origin(pc));
                entries.push(entry(Phase::Went, next, after, 0));
            }
            _ => {}
        }
    }
    entries.sort_by(|a, b| (&a.site, a.phase, a.resume.pc).cmp(&(&b.site, b.phase, b.resume.pc)));
    for at in 1..entries.len() {
        let (before, rest) = entries.split_at_mut(at);
        let (last, entry) = (&before[at - 1], &mut rest[0]);
        if last.site == entry.site && last.phase == entry.phase {
            entry.copy = last.copy + 1;
        }
    }
    let mut by_pc: Vec<u32> = (0..entries.len() as u32).collect();
    by_pc.sort_by_key(|&at| {
        let entry = &entries[at as usize];
        (entry.resume.pc, entry.phase)
    });
    let (locals, by_site, loops) = slots(unit, &root);
    SiteTable {
        entries,
        by_pc,
        locals,
        by_site,
        loops,
    }
}

/// What each slot of `unit` holds, the slots by declaring site, and each
/// `for` loop's state.
fn slots(unit: &Unit, root: &Path) -> (Vec<Local>, Vec<u16>, Vec<LoopState>) {
    let mut roles: Vec<(Path, SlotRole)> = (0..unit.locals)
        .map(|slot| match unit.declared.get(usize::from(slot)) {
            Some((site, Some(name))) => (site.clone(), SlotRole::Variable(name.clone())),
            Some((site, None)) => (site.clone(), SlotRole::Held),
            None => (root.clone(), SlotRole::Argument(slot)),
        })
        .collect();
    let mut cell = vec![false; roles.len()];
    for &slot in &unit.boxed {
        if let Some(flag) = cell.get_mut(usize::from(slot)) {
            *flag = true;
        }
    }
    let mut passes = Vec::new();
    for op in &unit.ops {
        match *op {
            Op::Box(slot) => {
                if let Some(flag) = cell.get_mut(usize::from(slot)) {
                    *flag = true;
                }
            }
            Op::LoopStart(slot) => {
                if let Some((_, role)) = roles.get_mut(usize::from(slot)) {
                    *role = SlotRole::Passes;
                }
                passes.push(slot);
            }
            _ => {}
        }
    }
    // A protocol loop keeps six slots of state, a plain one two.
    let protocol: Vec<u32> = unit
        .ops
        .iter()
        .filter_map(|op| match *op {
            Op::ProtoOpen(n) => Some(n),
            _ => None,
        })
        .collect();
    let mut loops = Vec::with_capacity(unit.loops.len());
    for (index, site) in unit.loops.iter().enumerate() {
        let width: u16 = if protocol.contains(&(index as u32)) {
            6
        } else {
            2
        };
        let slots: Arc<[u16]> = (site.slot..site.slot.saturating_add(width)).collect();
        for (at, &slot) in slots.iter().enumerate() {
            if let Some((_, role)) = roles.get_mut(usize::from(slot)) {
                *role = SlotRole::Loop(at as u8);
            }
        }
        let path: Path = Arc::from(site.site.path.as_slice());
        // The pass counter is the loop statement's own, declared at it.
        let counter = passes.iter().copied().find(|&slot| {
            roles
                .get(usize::from(slot))
                .is_some_and(|(at, _)| *at == path)
        });
        loops.push(LoopState {
            site: path,
            copy: 0,
            slots,
            passes: counter,
        });
    }
    let mut locals: Vec<Local> = roles
        .into_iter()
        .enumerate()
        .map(|(slot, (site, role))| Local {
            site,
            role,
            copy: 0,
            slot: slot as u16,
            cell: cell[slot],
        })
        .collect();
    let mut by_site: Vec<u16> = (0..locals.len() as u16).collect();
    by_site.sort_by(|&a, &b| {
        let (a, b) = (&locals[usize::from(a)], &locals[usize::from(b)]);
        (&a.site, &a.role, a.slot).cmp(&(&b.site, &b.role, b.slot))
    });
    for at in 1..by_site.len() {
        let (last, this) = (usize::from(by_site[at - 1]), usize::from(by_site[at]));
        if locals[last].site == locals[this].site && locals[last].role == locals[this].role {
            locals[this].copy = locals[last].copy + 1;
        }
    }
    let mut seen: Vec<(Path, u32)> = Vec::new();
    for state in &mut loops {
        let copies = seen.iter().filter(|(site, _)| *site == state.site).count();
        state.copy = copies as u32;
        seen.push((state.site.clone(), state.copy));
    }
    (locals, by_site, loops)
}

/// The operand stack before each op of `unit`, each value by the
/// expression that produced it: the expression the op that pushed it
/// lowered. Where paths meet with different producers, a value is the
/// innermost expression both are under: the conditional, `and` or `or`
/// whose branches produced it. `None` where the stack is malformed.
fn stacks(unit: &Unit, origin: &impl Fn(usize) -> Path) -> Option<Vec<Option<Stack>>> {
    let len = unit.ops.len();
    let mut seen: Vec<Option<Stack>> = vec![None; len];
    let mut work = vec![(0usize, Stack::default())];
    if let Some(cleanup) = unit.cleanup {
        let raised = cleanup.raised as usize;
        work.push((raised, Stack::default().push(origin(raised))));
    }
    while let Some((pc, stack)) = work.pop() {
        let known = seen.get_mut(pc)?;
        let stack = match known {
            None => stack,
            Some(known) => match known.meet(&stack)? {
                None => continue,
                Some(met) => met,
            },
        };
        seen[pc] = Some(stack.clone());
        let op = unit.ops[pc];
        let (pops, pushes) = unit.effect(op);
        let mut after = stack.pop(pops as usize)?;
        for _ in 0..pushes {
            after = after.push(origin(pc));
        }
        let mut go = |to: usize, stack: Stack| work.push((to, stack));
        match op {
            Op::Return | Op::Fall | Op::Raise | Op::Finish | Op::Fail | Op::Unavailable(_) => {}
            Op::Leave(_) | Op::Deopt(_) => {}
            Op::Jump(to) => go(to as usize, after),
            Op::JumpIfFalse(to) | Op::WhileFalse(to) => {
                go(to as usize, after.clone());
                go(pc + 1, after);
            }
            // A raise truncates to the depth at entry and pushes the
            // raised value, which the `try` stands for.
            Op::TryEnter(to) => {
                go(to as usize, after.push(origin(pc)));
                go(pc + 1, after);
            }
            Op::ForNext(n) | Op::ProtoMore(n) => {
                go(unit.loops[n as usize].exit as usize, after.clone());
                go(pc + 1, after);
            }
            Op::ProtoStep(n) => {
                if let Some(driver) = unit.loops[n as usize].driver {
                    go(driver as usize, after.clone());
                }
                go(pc + 1, after);
            }
            Op::ProtoGuard(n) => {
                if let Some(join) = unit.loops[n as usize].join {
                    go(join as usize, stack);
                }
                go(pc + 1, after);
            }
            _ => go(pc + 1, after),
        }
    }
    Some(seen)
}

/// An abstract operand stack, top first, sharing what lies below a push:
/// the walk keeps one before every op, so a copy must not cost its depth.
#[derive(Clone, Default)]
struct Stack(Option<Arc<Node>>);

struct Node {
    path: Path,
    len: usize,
    below: Stack,
}

impl Drop for Node {
    // Iterative, so a deep stack's last owner does not recurse its depth.
    fn drop(&mut self) {
        let mut below = self.below.0.take();
        while let Some(node) = below {
            match Arc::try_unwrap(node) {
                Ok(mut node) => below = node.below.0.take(),
                Err(_) => break,
            }
        }
    }
}

impl Stack {
    fn len(&self) -> usize {
        self.0.as_ref().map_or(0, |node| node.len)
    }

    fn push(&self, path: Path) -> Self {
        Self(Some(Arc::new(Node {
            path,
            len: self.len() + 1,
            below: self.clone(),
        })))
    }

    /// The stack below its top `n`; `None` when it holds fewer.
    fn pop(&self, n: usize) -> Option<Self> {
        let mut at = self;
        for _ in 0..n {
            at = &at.0.as_ref()?.below;
        }
        Some(at.clone())
    }

    /// Bottom first.
    fn to_vec(&self) -> Vec<Path> {
        let mut paths = Vec::with_capacity(self.len());
        let mut at = &self.0;
        while let Some(node) = at {
            paths.push(node.path.clone());
            at = &node.below.0;
        }
        paths.reverse();
        paths
    }

    /// Where two branches meet: each value by the longest coordinate both
    /// give it. `None` (outer) when their depths differ; `Some(None)` when
    /// `incoming` changes nothing.
    fn meet(&self, incoming: &Self) -> Option<Option<Self>> {
        if self.len() != incoming.len() {
            return None;
        }
        // Down to where both share their nodes, which agree below.
        let mut tops = Vec::new();
        let mut changed = false;
        let (mut held, mut other) = (self, incoming);
        while let (Some(a), Some(b)) = (&held.0, &other.0) {
            if Arc::ptr_eq(a, b) {
                break;
            }
            let shared = a
                .path
                .iter()
                .zip(b.path.iter())
                .take_while(|(x, y)| x == y)
                .count();
            if shared < a.path.len() {
                changed = true;
                tops.push(Path::from(&a.path[..shared]));
            } else {
                tops.push(a.path.clone());
            }
            (held, other) = (&a.below, &b.below);
        }
        if !changed {
            return Some(None);
        }
        let mut met = held.clone();
        for path in tops.into_iter().rev() {
            met = met.push(path);
        }
        Some(Some(met))
    }
}
