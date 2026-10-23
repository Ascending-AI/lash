//! Resumable segments (FIG-6709): a generated plan of a helper that can
//! reach guest code or the host, yielding to the machine at its callback
//! sites (`docs/design/kernel-end-state.md` §J).
//!
//! At a callback site whose callee runs in a frame (a closure, or a
//! library body), the plan yields. Every plan standing at a callback site
//! of the yielding call, outermost first, becomes the body's frame
//! standing at that site exactly as the interpreted body's would: its
//! variables, its blocks and loops, the action it waits in with the
//! operands its statement holds, and its call's arguments. The callee then
//! runs as ordinary code. No Rust stack is saved: between the yield and the
//! callee's return the machine holds only frames, which project and restore
//! as any frame does.
//!
//! When the callee returns to a frame standing at a site of its function's
//! segment, under a layout that runs the segment, the frame's state goes
//! back into the plan's variables and registers and the plan runs on after
//! that site; under one that runs bodies, the body runs on. A frame parked
//! by either restores into the other, since both stand at the body's
//! coordinate. A callback that raises unwinds the frame as the body's own
//! frame unwinds; a segment stands in a `try`'s body or handler, never in
//! its `finally`.
//!
//! A `perform` in the body is a callback site too (FIG-6916). The plan
//! copies its arguments out where the body's action does and yields; its
//! frames stand, and the innermost waits in the effect: a recorded one is
//! issued from that frame, through the body's own path, and its outcome is
//! consumed there when delivered, then goes back into the plan as a
//! callee's return does. A read-bound one is answered before the step
//! ends, with nothing sealed between, so the task keeps its turn
//! (`K-EFF-012`). Requests, identities, origins, values and charge are the
//! body's, since what issues, names and consumes them is the frame.

use lash_vocabulary::package::OperationMode;
use std::sync::Arc;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::plan::PlanExit;
use lash_kernel_doc::{Datum, TaskId, Value};

use lash_kernel_state::paged::PhaseRecord;
use lash_vocabulary::sites::Site;
use lash_vocabulary::types::Type;

use super::actions::Target;
use super::exec::{Call, ExportBoundary, ResultOrigin};
use super::frames::Role;
use super::islands::{Ran, SteadyIsland};
use super::loops::Header;
use super::operation_entry::AdmittedBody;
use super::ops::{Answer, Flow, Reg, Status};
use super::plans::{PlanRun, Steady, Walk};
use super::{Completion, Control, Eval, Frame, Halt, Interrupt, KernelMachine, LibraryCall, fault};
use crate::compile::islands::MAX_SLOTS;
use crate::compile::segments::{Segment, SiteControl};
use crate::compile::{
    ActionKind, CodeId, Executable, Form, LibId, LibRun, NestedKind, PreparedPlan,
};
use crate::interface::Host;
use crate::observe::Event;

/// What the body's frame keeps of a plan's call's arguments.
pub(super) enum Kept {
    /// The arguments in the plan's argument registers, when the call is
    /// one whose frame keeps them.
    Window { keep: bool },
    /// What the frame the plan resumed from kept.
    Frame(Box<[Value]>),
}

/// A plan standing at one of its callback sites while the site's callee
/// is offered the call.
pub(super) struct Standing {
    link: lash_vocabulary::LinkId,
    plan: Arc<PreparedPlan>,
    lib: LibId,
    site: u32,
    args: Reg,
    base: Reg,
    slots: Reg,
    window: Reg,
    cursors: Vec<Walk>,
    whiles: Vec<u64>,
    kept: Kept,
}

/// What the innermost site calls once the frames stand.
pub(super) enum Callee {
    Body {
        link: lash_vocabulary::LinkId,
        lib: LibId,
        admitted: AdmittedBody,
        args: Reg,
    },
    Code {
        link: lash_vocabulary::LinkId,
        code: CodeId,
        captures: Vec<ArenaHandle>,
        args: Reg,
        count: usize,
    },
    /// The callee ran as an island and exited, leaving these virtual
    /// frames, innermost first.
    Island(Vec<super::islands::Level>),
    /// The site is the body's `perform`, on these arguments, copied out
    /// (`K-EFF-002`).
    Perform(Vec<Datum>),
}

/// A read-bound `perform` a standing frame waits in, answered before the
/// step ends (`K-EFF-012`).
pub(super) struct Read {
    /// The task whose frame stands in it: a spawn may stand a child's.
    task: TaskId,
    effect: lash_vocabulary::names::OperationUse,
    result: Type,
    site: Site,
    args: Vec<Datum>,
}

/// The plans standing at callback sites of a call being offered, what a
/// yield calls, and the read a standing frame waits in.
#[derive(Default)]
pub(super) struct Yielding {
    standing: Vec<Standing>,
    callee: Option<Callee>,
    read: Option<Read>,
    /// How many `perform`s a plan has made in this machine, and how many
    /// were made in all, issued or answered as reads.
    #[cfg(test)]
    planned: u64,
    #[cfg(test)]
    performed: u64,
}

impl<const SHARED: bool> PlanRun<'_, SHARED> {
    /// Stands at site `site` while its callee is offered the call: what the
    /// body's frame would hold there is the machine's.
    fn stand(&mut self, site: u32) {
        let kept = std::mem::replace(&mut self.kept, Kept::Window { keep: false });
        self.machine.segments.standing.push(Standing {
            link: self.machine.current_link(),
            plan: Arc::clone(self.owner),
            lib: self.lib,
            site,
            args: self.args,
            base: self.base,
            slots: self.slots,
            window: self.window,
            cursors: std::mem::take(&mut self.cursors),
            whiles: std::mem::take(&mut self.whiles),
            kept,
        });
    }

    /// The callee answered: the plan goes on from its site.
    fn sit(&mut self) -> Result<(), PlanExit> {
        let Some(standing) = self.machine.segments.standing.pop() else {
            let lost = Err(fault("a segment sat at a site it did not stand at"));
            return self.held(lost);
        };
        self.cursors = standing.cursors;
        self.whiles = standing.whiles;
        self.kept = standing.kept;
        Ok(())
    }

    /// Offers library function `lib` the call at callback site `site`,
    /// with the `count` arguments from `at`, its value to `at`.
    pub(super) fn offer(
        &mut self,
        site: u32,
        lib: LibId,
        at: Reg,
        count: usize,
    ) -> Result<(), PlanExit> {
        self.offer_in_link(site, lib, self.machine.current_link(), at, count)
    }
    fn offer_in_link(
        &mut self,
        site: u32,
        lib: LibId,
        link: lash_vocabulary::LinkId,
        at: Reg,
        count: usize,
    ) -> Result<(), PlanExit> {
        self.stand(site);
        let caller = self.machine.active_link;
        self.machine.active_link = link;
        let answered = self
            .machine
            .answer(self.task, self.exe, lib, at, count, at)
            .map_err(|interrupt| interrupt.answering(self.exe.lib(lib)));
        self.machine.active_link = caller;
        match answered {
            Ok(Answer::Done) => self.sit(),
            Ok(Answer::Body(admitted)) => {
                self.machine.segments.callee = Some(Callee::Body {
                    link,
                    lib,
                    admitted,
                    args: at,
                });
                Err(PlanExit::Yield)
            }
            Ok(Answer::Yielded) => Err(PlanExit::Yield),
            Err(interrupt) => {
                self.sit()?;
                Err(self.exit(interrupt))
            }
        }
    }

    /// Applies the value in `at` at callback site `site` to the `count`
    /// arguments after it, its value to `at`.
    pub(super) fn apply_at(&mut self, site: u32, at: Reg, count: usize) -> Result<(), PlanExit> {
        let callee = self.machine.take(at);
        let target = self.machine.target(self.exe, callee);
        match self.check(target)? {
            Target::Export(..) => Err(self
                .exit(super::fault("a resident callback cannot be an import descriptor").into())),
            Target::Library(lib, link) => {
                self.offer_in_link(site, lib, link, at + 1, count)?;
                let value = self.machine.take(at + 1);
                self.machine.put(at, value);
                Ok(())
            }
            Target::Code(code, captures, link) => {
                let ran = self.machine.in_link(link, |machine| {
                    machine.callback_island(
                        &mut self.gate,
                        self.task,
                        self.exe,
                        (code, &captures),
                        at + 1,
                        count,
                    )
                });
                match self.check(ran)? {
                    Some(Ran::Returned(value)) => {
                        self.machine.spent_captures(captures);
                        self.returned(at, value);
                        Ok(())
                    }
                    ran => Err(self.stand_callee(site, code, link, captures, (at, count), ran)),
                }
            }
        }
    }

    /// The body's `perform` at callback site `site`, on the `count`
    /// arguments from `at`, copied out as the body's action copies them
    /// (`K-EFF-002`): the plan yields, and its frames stand to wait in the
    /// effect.
    pub(super) fn perform_at(&mut self, site: u32, at: Reg, count: usize) -> Result<(), PlanExit> {
        let (segment, _) = match segment_of(self.exe, self.lib, self.owner) {
            Ok(segment) => segment,
            Err(halt) => return self.held(Err(halt)),
        };
        let segment_site = &segment.sites[site as usize];
        let (stream, stmt, _) = self.exe.statement(segment_site.stmt);
        let effect = match segment_site.act {
            Some(act) => match &stream.nested[act.ix()].kind {
                NestedKind::Perform { slot, .. } => Some(*slot),
                _ => None,
            },
            None => match &stmt.form {
                Form::Action(action) => match &action.kind {
                    ActionKind::Perform { slot, .. } => Some(*slot),
                    _ => None,
                },
                _ => None,
            },
        };
        let Some(slot) = effect else {
            return self.held(Err(fault("a segment perform site has no effect")));
        };
        let effect = self
            .machine
            .operation_in_link(self.machine.current_link(), slot);
        let effect = self.check(effect.map_err(Interrupt::from))?;
        // A statement's own `perform` reads its arguments as atoms, each a
        // boundary charge of 1 (`K-CHG-002`, `K-CHG-007`); a nested one's
        // are its statement's operands, covered by the formula.
        let own = segment_site.act.is_none();
        if self.held(Ok(own))? {
            let charged = self.machine.charge_boundary(count as u64);
            self.held(charged)?;
        }
        let mut copied = Vec::with_capacity(count);
        for (index, at) in (at..at + count).enumerate() {
            let value = self.machine.take(at);
            let datum = self.machine.copy_out_tool_arg(&effect, index, &value);
            copied.push(self.check(datum)?);
        }
        #[cfg(test)]
        {
            self.machine.segments.planned += 1;
        }
        self.stand(site);
        self.machine.segments.callee = Some(Callee::Perform(copied));
        Err(PlanExit::Yield)
    }

    /// [`Self::apply_at`] of `callee` at a region's steady site
    /// (`super::plan_regions`), which would take it from `at`: the callback
    /// its variable holds for the whole walk is resolved once, at the
    /// walk's first call of a closure, as [`Self::apply_at`] resolves it,
    /// with the island it enters under the walk's gate and what of that
    /// island's entry no call of the walk changes (FIG-6978).
    pub(super) fn apply_steady(
        &mut self,
        site: u32,
        at: Reg,
        count: usize,
        callee: Value,
    ) -> Result<(), PlanExit> {
        let Value::Closure(closure) = callee else {
            self.machine.put(at, callee);
            return self.apply_at(site, at, count);
        };
        if !self.steadied(closure) {
            self.steady = Some(self.resolve_steady(closure, callee)?);
        }
        let mut values = [Value::Absent; MAX_SLOTS];
        let ran = match values.get_mut(..count) {
            Some(held) => {
                held.copy_from_slice(&self.machine.regs[at + 1..at + 1 + count]);
                self.steady_entry(&values[..count])
            }
            None => Ok(None),
        };
        self.steady_ran(site, (at, count), ran)
    }

    /// Whether the walk's steady callback is resolved, and is `closure`.
    #[inline(always)]
    pub(super) fn steadied(&self, closure: ArenaHandle) -> bool {
        self.steady
            .as_ref()
            .is_some_and(|steady| steady.closure == closure)
    }

    /// The resolved steady callback's island entry on `values`, where its
    /// island takes them: `None` otherwise, and nothing changed.
    #[inline(always)]
    pub(super) fn steady_entry(&mut self, values: &[Value]) -> Eval<Option<Ran>> {
        match (&self.steady, &self.gate) {
            (
                Some(Steady {
                    island: Some(island),
                    cells,
                    link,
                    ..
                }),
                Some(gate),
            ) if values.len() <= island.arity => self.machine.in_link(*link, |machine| {
                machine.steady_entry(gate, self.task, self.exe, (island, cells), values)
            }),
            _ => Ok(None),
        }
    }

    /// The steady callback applied from `at` to the `count` operands after
    /// it, which they hold, came to `ran`: its value to `at` where its
    /// island returned; otherwise the plan stands at site `site` while the
    /// callback runs.
    #[inline(always)]
    pub(super) fn steady_ran(
        &mut self,
        site: u32,
        (at, count): (Reg, usize),
        ran: Eval<Option<Ran>>,
    ) -> Result<(), PlanExit> {
        match self.check(ran)? {
            Some(Ran::Returned(value)) => {
                self.returned(at, value);
                Ok(())
            }
            ran => {
                let Some(steady) = self.steady.take() else {
                    return Err(self.lost("a steady site applied no callback"));
                };
                Err(self.stand_callee(
                    site,
                    steady.code,
                    steady.link,
                    steady.cells,
                    (at, count),
                    ran,
                ))
            }
        }
    }

    /// The steady callback `closure`, the value `callee`: its code and
    /// cells, and the island it runs as under the walk's gate where that
    /// island takes the walk's calls.
    #[cold]
    #[inline(never)]
    fn resolve_steady(&mut self, closure: ArenaHandle, callee: Value) -> Result<Steady, PlanExit> {
        let target = self.machine.target(self.exe, callee);
        let Target::Code(code, cells, link) = self.check(target)? else {
            return Err(self.lost("a closure resolved to no code"));
        };
        let island = self.machine.in_link(link, |machine| {
            machine.callback_plan(&mut self.gate, self.task, self.exe, code)
        });
        let island = self.check(island)?.and_then(|plan| {
            let gate = self.gate.as_ref()?;
            SteadyIsland::of(gate, self.exe, plan, cells.len())
        });
        Ok(Steady {
            link,
            closure,
            code,
            cells,
            island,
        })
    }

    /// The callback applied from `at` returned `value`.
    #[inline(always)]
    pub(super) fn returned(&mut self, at: Reg, value: Value) {
        self.machine.put(at, value);
        self.machine.borrow_value(value);
    }

    /// Stands at site `site` for the callback of code `code` with
    /// `captures`, applied to the `count` arguments after `at`, whose
    /// island `ran` exited or did not run: the yield.
    fn stand_callee(
        &mut self,
        site: u32,
        code: CodeId,
        link: lash_vocabulary::LinkId,
        captures: Vec<ArenaHandle>,
        (at, count): (Reg, usize),
        ran: Option<Ran>,
    ) -> PlanExit {
        let callee = match ran {
            Some(Ran::Exited(levels)) => {
                self.machine.spent_captures(captures);
                Callee::Island(levels)
            }
            _ => Callee::Code {
                link,
                code,
                captures,
                args: at + 1,
                count,
            },
        };
        self.stand(site);
        self.machine.segments.callee = Some(callee);
        PlanExit::Yield
    }

    #[cold]
    fn lost(&mut self, fault: &str) -> PlanExit {
        self.halt = Some(Box::new(super::fault(fault)));
        PlanExit::Halt
    }
}

impl KernelMachine {
    /// How many `perform`s a generated plan has made in this machine: the
    /// laws' check that a layout selected the plan.
    #[cfg(test)]
    pub(crate) fn planned_performs(&self) -> u64 {
        self.segments.planned
    }

    /// How many `perform`s this machine issued or answered as reads.
    #[cfg(test)]
    pub(crate) fn performs(&self) -> u64 {
        self.segments.performed
    }

    /// Counts a `perform` issued or answered as a read.
    #[cfg(test)]
    pub(super) fn count_perform(&mut self) {
        self.segments.performed += 1;
    }

    /// The plans that yielded stand as their bodies' frames, outermost
    /// first, the caller's frame waiting in statement `awaiting`, if any;
    /// then the innermost site's callee runs: `Step`.
    pub(super) fn stand_segments(
        &mut self,
        task: TaskId,
        exe: &Executable,
        awaiting: Option<crate::compile::StmtId>,
        export: Option<ExportBoundary<'_>>,
    ) -> Status {
        let standing = std::mem::take(&mut self.segments.standing);
        let callee = self
            .segments
            .callee
            .take()
            .ok_or_else(|| fault("a segment yielded with no callee"))?;
        let Some(release) = standing.first().map(|standing| standing.window) else {
            return Err(fault("a segment yielded standing nowhere").into());
        };
        let charging = std::mem::replace(&mut self.charging, false);
        let paced = std::mem::replace(&mut self.paced, false);
        let stood = self.stand_all(task, exe, standing, callee, awaiting, export);
        (self.charging, self.paced) = (charging, paced);
        // What the plans' registers held is the frames' now.
        self.regs.release(release);
        self.heap.execution_roots_moved();
        stood
    }

    fn stand_all(
        &mut self,
        task: TaskId,
        exe: &Executable,
        standing: Vec<Standing>,
        callee: Callee,
        mut awaiting: Option<crate::compile::StmtId>,
        mut export: Option<ExportBoundary<'_>>,
    ) -> Status {
        for standing in standing {
            let stmt = self.stand_frame(task, exe, standing, awaiting)?;
            // The first standing frame is the imported helper's own body.
            // Install its contract before entering any nested callback frame.
            if let Some(export) = export.take() {
                let frame = self.frame(task)?;
                frame.convention = Some(export.convention.clone());
                frame.public_result = Some(export.result.clone());
            }
            awaiting = Some(stmt);
        }
        match callee {
            Callee::Body {
                link,
                lib,
                admitted,
                args,
            } => {
                let caller = self.active_link;
                self.active_link = link;
                let result = self.enter_body(
                    task,
                    exe,
                    lib,
                    admitted,
                    super::exec::Invocation::new(args, exe.lib(lib).arity)
                        .awaited_by(awaiting)
                        .with_boundary(None),
                );
                self.active_link = caller;
                result
            }
            Callee::Code {
                link,
                code,
                captures,
                args,
                count,
            } => {
                let call = Call::new(code, args, count)
                    .in_link(link)
                    .sharing(&captures)
                    .awaited_by(awaiting);
                let pushed = self.push_frame(task, exe, call);
                self.spent_captures(captures);
                pushed?;
                Ok(Flow::Step)
            }
            Callee::Island(levels) => {
                if let Some(stmt) = awaiting {
                    self.frame(task)?.awaiting = Some(stmt);
                }
                self.stand_island(task, exe, levels)?;
                Ok(Flow::Step)
            }
            Callee::Perform(args) => {
                // The innermost frame stands at the body's `perform`, and
                // what it performs is that action's.
                let frame = self.frame(task)?;
                let stmt = frame.awaiting;
                let (Some(stmt), Some((site, Some((effect, result))))) =
                    (stmt, super::actions::waiting(exe, frame))
                else {
                    return Err(fault("a segment performed at no `perform`").into());
                };
                let effect = self.operation(task, effect)?;
                if self.effect_mode(&effect)? == OperationMode::Read {
                    self.segments.read = Some(Read {
                        task,
                        effect,
                        result,
                        site,
                        args,
                    });
                } else {
                    self.issue_effect(task, stmt, &site, effect, args)?;
                }
                Ok(Flow::Step)
            }
        }
    }

    /// Answers the read-bound `perform` a frame stood at in this step, if
    /// any, and runs its plan on with the answer (`K-EFF-012`): the task
    /// it stands in, which a spawn may have started in this step, and what
    /// the step goes on to settle for it. Nothing seals between.
    pub(super) fn segment_read(
        &mut self,
        host: &mut dyn Host,
        exe: &Executable,
    ) -> Option<(TaskId, super::Eval<Completion>)> {
        let read = self.segments.read.take()?;
        let task = read.task;
        let answered = self.perform_read(
            task,
            host,
            exe,
            &read.effect,
            &read.args,
            &read.site,
            &read.result,
        );
        let outcome = answered.and_then(|value| {
            match self.segment_returned(task, host, exe, value, ResultOrigin::Delivered)? {
                Ok(departure) => Ok(departure.unwrap_or(Completion::Normal)),
                Err(_) => Err(fault("a read's frame stands at no segment site").into()),
            }
        });
        Some((task, outcome))
    }

    /// Pushes the frame `standing`'s body stands in at its site, the
    /// frame below waiting in statement `awaiting`, if any: the statement
    /// the new frame waits in.
    fn stand_frame(
        &mut self,
        task: TaskId,
        exe: &Executable,
        standing: Standing,
        awaiting: Option<crate::compile::StmtId>,
    ) -> Result<crate::compile::StmtId, Interrupt> {
        let Standing {
            link,
            plan,
            lib,
            site,
            args,
            base,
            slots,
            cursors,
            whiles,
            kept,
            ..
        } = standing;
        let (segment, code_id) = segment_of(exe, lib, &plan)?;
        let site = &segment.sites[site as usize];
        let arity = exe.lib(lib).arity;
        let local = |slot: u32| {
            plan.locals[slot as usize].ok_or_else(|| fault("a segment's site binds no variable"))
        };
        let code = exe.code(code_id);
        let mut frame = self.fresh_frame(exe, code_id)?;
        frame.link = link;
        frame.slots.fresh(code.slots_by_at.len());
        for (index, at) in segment.params.iter().enumerate() {
            let value = self.regs[args + index];
            frame.set_slot(*at, super::SlotState::Value(value));
        }
        for slot in &site.bound {
            let local = local(*slot)?;
            let value = self.regs[slots + *slot as usize];
            let state = match value {
                Value::ObjectRef(cell) if local.shared => super::SlotState::Cell(cell),
                _ if local.shared => return Err(fault("a shared variable has no cell").into()),
                value => super::SlotState::Value(value),
            };
            frame.set_slot(local.at, state);
        }
        let mut cursors = cursors.into_iter();
        let mut whiles = whiles.into_iter();
        for control in &site.control {
            frame.control.push(match *control {
                SiteControl::Block { block, next } => Control::Block { block, next },
                SiteControl::For { stmt, .. } => {
                    let walk = cursors
                        .next()
                        .ok_or_else(|| fault("a segment stands in a loop it has not opened"))?;
                    Control::For {
                        stmt,
                        cursor: walk.cursor,
                        started: walk.started,
                        header: Header::Body,
                        guard: None,
                    }
                }
                SiteControl::Try { stmt, catch } => Control::Try {
                    stmt,
                    phase: if catch {
                        super::TryPhase::Catch
                    } else {
                        super::TryPhase::Body
                    },
                },
                SiteControl::While { stmt } => Control::While {
                    stmt,
                    started: whiles
                        .next()
                        .ok_or_else(|| fault("a segment stands in a loop it has not counted"))?,
                },
            });
        }
        if cursors.next().is_some() || whiles.next().is_some() {
            return Err(fault("a segment holds a loop its site is not in").into());
        }
        frame.awaiting = Some(site.stmt);
        if let Some(act) = site.act {
            // The roles stay in the plan's registers as well: the frame
            // holds a copy of each.
            let phase = if site.performs {
                PhaseRecord::Waiting
            } else {
                PhaseRecord::Called
            };
            let pending =
                self.continuation(&site.pending, phase, site.roles.len(), |machine, at| {
                    let (path, reg) = &site.roles[at];
                    Role {
                        site: Arc::clone(path),
                        value: machine.regs[base + *reg as usize],
                    }
                })?;
            frame.stand_at(act, pending);
        }
        let args = match kept {
            Kept::Window { keep: true } => self.regs[args..args + arity].to_vec().into(),
            Kept::Window { keep: false } => Box::default(),
            Kept::Frame(args) => args,
        };
        frame.library = (!code.charged).then_some(LibraryCall { lib, args });
        if let Some(stmt) = awaiting {
            self.frame(task)?.awaiting = Some(stmt);
        }
        let stmt = site.stmt;
        self.task(task)?.frames.push(frame);
        self.grown();
        self.observe(Event::Push {
            task,
            code: code_id,
        });
        Ok(stmt)
    }

    /// Whether the task's top frame stands at a callback site of its
    /// function's segment, which this layout runs: the site.
    fn standing_site(
        &mut self,
        task: TaskId,
        exe: &Executable,
    ) -> Result<Option<(LibId, Arc<PreparedPlan>, u32)>, Halt> {
        let Some(frame) = self.task(task)?.frames.last_mut()? else {
            return Ok(None);
        };
        let (Some(call), Some(stmt), None) = (&frame.library, frame.awaiting, &frame.cleaning)
        else {
            return Ok(None);
        };
        let Some((plan, body)) = segment_plan(exe, call.lib) else {
            return Ok(None);
        };
        let Some(segment) = &plan.segment else {
            return Ok(None);
        };
        if body != frame.code {
            return Ok(None);
        }
        Ok(segment
            .by_stand
            .get(&(stmt, frame.nested))
            .map(|site| (call.lib, Arc::clone(plan), *site)))
    }

    /// `value` returned to the task's top frame, or was delivered to it as
    /// its `perform`'s outcome: when it stands at a callback site of a
    /// segment this layout runs, the plan runs on after the site and gives
    /// the departure its frame is left with.
    pub(super) fn segment_returned(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        value: Value,
        origin: ResultOrigin,
    ) -> Result<Result<Option<Completion>, Value>, Halt> {
        let Some((lib, plan, site)) = self.standing_site(task, exe)? else {
            return Ok(Err(value));
        };
        let frame = self
            .task(task)?
            .frames
            .pop()?
            .ok_or_else(|| fault("a standing frame is missing"))?;
        self.refresh_charging(task)?;
        let resumed = if plan.shared.is_some() {
            self.resume_plan::<true>(task, exe, lib, &plan, site, &frame, (value, origin))
        } else {
            self.resume_plan::<false>(task, exe, lib, &plan, site, &frame, (value, origin))
        };
        let result = match resumed {
            Ok(Some(result)) => result,
            Ok(None) => {
                // The plan stands again, in new frames.
                self.observe(Event::Pop { task });
                self.recycle(frame);
                return Ok(Ok(None));
            }
            Err(interrupt) => Err(*self.thrown_by(task, interrupt)?),
        };
        self.left(task, host, exe, frame, result).map(Ok)
    }

    /// Runs `plan` on after site `site`, from `frame`, the body's frame
    /// standing there, with `value` the site's, returned or delivered as
    /// `origin` says: what the call ends with, or `None` when the plan
    /// yielded again and its frames stand.
    #[allow(clippy::too_many_arguments)]
    fn resume_plan<const SHARED: bool>(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        plan: &Arc<PreparedPlan>,
        site: u32,
        frame: &Frame,
        (value, origin): (Value, ResultOrigin),
    ) -> Eval<Option<Result<Value, Value>>> {
        let (segment, _) = segment_of(exe, lib, plan)?;
        let stood = &segment.sites[site as usize];
        let arity = exe.lib(lib).arity;
        let args = self.regs.extent();
        let base = args + arity;
        let slots = base + plan.plan.registers as usize;
        self.regs.window(slots + plan.plan.slots.len());
        for (index, at) in segment.params.iter().enumerate() {
            let value = match frame.slot(*at) {
                super::SlotState::Value(value) => *value,
                super::SlotState::Cell(cell) => {
                    let cell = *cell;
                    self.in_arena(|arena| arena.cell(cell))?
                }
                super::SlotState::Empty => {
                    return Err(fault("a segment's frame holds no parameter").into());
                }
            };
            self.put(args + index, value);
        }
        for slot in &stood.bound {
            let local = plan.locals[*slot as usize]
                .ok_or_else(|| fault("a segment's site binds no variable"))?;
            let value = match (frame.slot(local.at), local.shared) {
                (super::SlotState::Value(value), false) => *value,
                (super::SlotState::Cell(cell), true) => Value::ObjectRef(*cell),
                _ => return Err(fault("a segment's frame holds another variable").into()),
            };
            self.put(slots + *slot as usize, value);
        }
        let mut cursors = Vec::new();
        let mut whiles = Vec::new();
        let mut loops = stood
            .control
            .iter()
            .filter(|control| !matches!(control, SiteControl::Block { .. }));
        for control in &frame.control {
            if matches!(control, Control::Block { .. }) {
                continue;
            }
            match (control, loops.next()) {
                (
                    Control::Try {
                        phase: super::TryPhase::Body,
                        ..
                    },
                    Some(SiteControl::Try { catch: false, .. }),
                )
                | (
                    Control::Try {
                        phase: super::TryPhase::Catch,
                        ..
                    },
                    Some(SiteControl::Try { catch: true, .. }),
                ) => {}
                (
                    Control::For {
                        cursor,
                        started,
                        header: Header::Body,
                        guard: None,
                        ..
                    },
                    Some(SiteControl::For { walked, .. }),
                ) => {
                    let held = slots + *walked as usize;
                    if let Some(walked) = cursor.walked() {
                        self.put(held, walked);
                    }
                    cursors.push(Walk {
                        cursor: cursor.clone(),
                        held,
                        started: *started,
                        region: plan.region(*walked),
                    });
                }
                (Control::While { started, .. }, Some(SiteControl::While { .. })) => {
                    whiles.push(*started);
                }
                _ => return Err(fault("a segment's frame stands in another loop").into()),
            }
        }
        if loops.next().is_some() {
            return Err(fault("a segment's frame stands outside its site's loop").into());
        }
        // The frame's kept arguments stay held while it is off the stack.
        let kept = frame
            .library
            .as_ref()
            .map(|call| call.args.clone())
            .unwrap_or_default();
        if self.bounds.memory_meter != crate::MemoryMeter::Off {
            self.pins.borrowed.extend(kept.iter().copied());
        }
        let charging = std::mem::replace(&mut self.charging, false);
        let paced = std::mem::replace(&mut self.paced, false);
        self.plans += 1;
        let mut run = PlanRun::<SHARED> {
            machine: self,
            task,
            exe,
            owner: plan,
            prepared: plan,
            lib,
            args,
            entry: None,
            base,
            slots,
            window: args,
            cursors,
            whiles,
            kept: Kept::Frame(kept),
            statement: None,
            halt: None,
            steady: None,
            gate: None,
            depth: None,
        };
        let opened = run.open_statement();
        let outcome = match opened {
            Ok(()) => {
                let roles = frame.pending.peek().map(|pending| pending.roles.clone());
                for (path, reg) in &stood.roles {
                    let role = roles
                        .iter()
                        .flatten()
                        .find(|role| role.site == *path)
                        .map(|role| role.value);
                    match role {
                        Some(role) => run.machine.put(base + *reg as usize, role),
                        None => {
                            run.halt =
                                Some(Box::new(fault("a saved continuation role is missing")));
                        }
                    }
                }
                // The value is in no variable until the statement uses it;
                // a delivered outcome is held as the body's statement holds
                // it.
                let dst = base + stood.dst as usize;
                match origin {
                    ResultOrigin::Returned => {
                        run.machine.borrow_value(value);
                        run.machine.put(dst, value);
                    }
                    ResultOrigin::Delivered => {
                        run.machine.put(dst, value);
                        if let Err(halt) = run.machine.pin(dst) {
                            run.halt = Some(Box::new(halt));
                        }
                    }
                }
                if run.halt.is_some() {
                    Err(PlanExit::Halt)
                } else {
                    (plan.plan.run)(&mut run, site + 1)
                }
            }
            Err(exit) => Err(exit),
        };
        let halt = run.halt.take();
        self.plans -= 1;
        (self.charging, self.paced) = (charging, paced);
        if matches!(outcome, Err(PlanExit::Yield)) && halt.is_none() {
            let boundary = frame
                .convention
                .as_ref()
                .zip(frame.public_result.as_ref())
                .map(|(convention, result)| ExportBoundary { convention, result });
            self.stand_segments(task, exe, None, boundary)?;
            return Ok(None);
        }
        self.regs.release(args);
        self.heap.execution_roots_moved();
        // The frame's return is held as it leaves the frame; its caller
        // holds it again as it arrives.
        self.plan_outcome(exe, lib, outcome, halt, charging, 1)
            .map(Some)
    }
}

/// The plan `run` runs a call by, its own or the one its native falls
/// back to, and its body's code.
fn segment_plan(exe: &Executable, lib: LibId) -> Option<(&Arc<PreparedPlan>, CodeId)> {
    match &exe.lib(lib).run {
        LibRun::Plan { plan, body } => Some((plan, *body)),
        LibRun::Native {
            body: Some(body), ..
        }
        | LibRun::DeferredPlan { body, .. } => Some((exe.fallback(lib)?, *body)),
        _ => None,
    }
}

/// The resolved segment of `lib`'s plan and its body's code.
fn segment_of<'p>(
    exe: &Executable,
    lib: LibId,
    plan: &'p PreparedPlan,
) -> Result<(&'p Segment, CodeId), Halt> {
    let (_, body) =
        segment_plan(exe, lib).ok_or_else(|| fault("a segment's function runs no plan"))?;
    let segment = plan
        .segment
        .as_ref()
        .ok_or_else(|| fault("a plan with no callback site yielded"))?;
    Ok((segment, body))
}
