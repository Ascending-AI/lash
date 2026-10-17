//! Resumable segments (FIG-6709): a generated plan of a helper that can
//! reach guest code, yielding to the machine at its callback sites
//! (`docs/design/kernel-end-state.md` §J).
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

use std::sync::Arc;

use lash_kernel_doc::arena::ArenaHandle;
use lash_kernel_doc::plan::PlanExit;
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_state::paged::{PhaseRecord, RoleRecord};

use super::actions::Target;
use super::exec::Call;
use super::frames::Pending;
use super::loops::Header;
use super::operation_entry::AdmittedBody;
use super::ops::{Answer, Flow, Reg, Status};
use super::plans::{PlanRun, Walk};
use super::{Completion, Control, Eval, Frame, Halt, Interrupt, KernelMachine, LibraryCall, fault};
use crate::compile::segments::{Segment, SiteControl};
use crate::compile::{CodeId, Executable, LibId, LibRun, PreparedPlan};
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
        lib: LibId,
        admitted: AdmittedBody,
        args: Reg,
    },
    Code {
        code: CodeId,
        captures: Vec<ArenaHandle>,
        args: Reg,
        count: usize,
    },
}

/// The plans standing at callback sites of a call being offered, and what
/// a yield calls.
#[derive(Default)]
pub(super) struct Yielding {
    standing: Vec<Standing>,
    callee: Option<Callee>,
}

impl PlanRun<'_> {
    /// Stands at site `site` while its callee is offered the call: what the
    /// body's frame would hold there is the machine's.
    fn stand(&mut self, site: u32) {
        let kept = std::mem::replace(&mut self.kept, Kept::Window { keep: false });
        self.machine.segments.standing.push(Standing {
            plan: Arc::clone(self.prepared),
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
        self.stand(site);
        let answered = self
            .machine
            .answer(self.task, self.exe, lib, at, count, at)
            .map_err(|interrupt| interrupt.answering(self.exe.lib(lib)));
        match answered {
            Ok(Answer::Done) => self.sit(),
            Ok(Answer::Body(admitted)) => {
                self.machine.segments.callee = Some(Callee::Body {
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
            Target::Library(lib) => {
                self.offer(site, lib, at + 1, count)?;
                let value = self.machine.take(at + 1);
                self.machine.put(at, value);
                Ok(())
            }
            Target::Code(code, captures) => {
                self.stand(site);
                self.machine.segments.callee = Some(Callee::Code {
                    code,
                    captures,
                    args: at + 1,
                    count,
                });
                Err(PlanExit::Yield)
            }
        }
    }
}

impl KernelMachine {
    /// The plans that yielded stand as their bodies' frames, outermost
    /// first, the caller's frame waiting in statement `awaiting`, if any;
    /// then the innermost site's callee runs: `Step`.
    pub(super) fn stand_segments(
        &mut self,
        task: TaskId,
        exe: &Executable,
        awaiting: Option<crate::compile::StmtId>,
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
        let stood = self.stand_all(task, exe, standing, callee, awaiting);
        self.charging = charging;
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
    ) -> Status {
        for standing in standing {
            let stmt = self.stand_frame(task, exe, standing, awaiting)?;
            awaiting = Some(stmt);
        }
        match callee {
            Callee::Body {
                lib,
                admitted,
                args,
            } => self.enter_body(task, exe, lib, admitted, args, awaiting),
            Callee::Code {
                code,
                captures,
                args,
                count,
            } => {
                let call = Call::new(code, args, count)
                    .sharing(&captures)
                    .awaited_by(awaiting);
                self.push_frame(task, exe, call)?;
                Ok(Flow::Step)
            }
        }
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
        // What the frame holds, held as a bound value is, while the
        // registers still hold it. A parameter is held once, and a variable
        // a closure shares is in its cell.
        self.reserve_held(super::FRAME_BYTES)?;
        let local = |slot: u32| {
            plan.locals[slot as usize].ok_or_else(|| fault("a segment's site binds no variable"))
        };
        let code = exe.code(code_id);
        let mut held = Vec::new();
        for (index, param) in code.params.iter().enumerate() {
            let adopted = site
                .bound
                .iter()
                .find(|slot| plan.locals[**slot as usize].is_some_and(|l| l.at == param.at));
            match adopted {
                Some(slot) if !local(*slot)?.shared => held.push(slots + *slot as usize),
                None if !param.shared => held.push(args + index),
                _ => {}
            }
        }
        for slot in &site.bound {
            let local = local(*slot)?;
            if !local.shared && !segment.params.contains(&local.at) {
                held.push(slots + *slot as usize);
            }
        }
        held.extend(site.roles.iter().map(|(_, reg)| base + *reg as usize));
        for at in held {
            self.reserve_held(self.cost_of(&self.regs[at]))?;
        }
        let mut frame = self.fresh_frame(exe, code_id)?;
        frame.scalar_blocks = code.versions.select(&self.regs[args..args + arity]);
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
        frame.nested = site.act;
        if site.act.is_some() {
            let mut roles = Vec::with_capacity(site.roles.len());
            for (path, reg) in &site.roles {
                roles.push(RoleRecord {
                    site: path.clone(),
                    value: self.page_form(&self.regs[base + *reg as usize])?,
                });
            }
            frame.pending.set(Pending {
                site: site.pending.clone(),
                phase: PhaseRecord::Called,
                roles,
            });
        }
        let args = match kept {
            Kept::Window { keep: true } => self.regs[args..args + arity].to_vec().into(),
            Kept::Window { keep: false } => Box::default(),
            Kept::Frame(args) => args,
        };
        frame.library = Some(LibraryCall { lib, args });
        if let Some(stmt) = awaiting {
            self.frame(task)?.awaiting = Some(stmt);
        }
        let stmt = site.stmt;
        self.task(task)?.frames.push(frame);
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
        let Some((plan, body)) = segment_plan(&exe.lib(call.lib).run) else {
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

    /// `value` returned to the task's top frame: when it stands at a
    /// callback site of a segment this layout runs, the plan runs on after
    /// the site and gives the departure its frame is left with.
    pub(super) fn segment_returned(
        &mut self,
        task: TaskId,
        host: &mut dyn Host,
        exe: &Executable,
        value: Value,
    ) -> Result<Result<Option<Completion>, Value>, Halt> {
        let Some((lib, plan, site)) = self.standing_site(task, exe)? else {
            return Ok(Err(value));
        };
        let frame = self
            .task(task)?
            .frames
            .pop()?
            .ok_or_else(|| fault("a standing frame is missing"))?;
        self.refresh_charging(task, exe)?;
        let result = match self.resume_plan(task, exe, lib, &plan, site, &frame, value) {
            Ok(Some(result)) => result,
            Ok(None) => {
                // The plan stands again, in new frames.
                self.observe(Event::Pop { task });
                self.recycle(frame);
                return Ok(Ok(None));
            }
            Err(interrupt) => Err(*self.thrown_by(interrupt)?),
        };
        self.left(task, host, exe, frame, result).map(Ok)
    }

    /// Runs `plan` on after site `site`, from `frame`, the body's frame
    /// standing there, with `value` the site's: what the call ends with,
    /// or `None` when the plan yielded again and its frames stand.
    #[allow(clippy::too_many_arguments)]
    fn resume_plan(
        &mut self,
        task: TaskId,
        exe: &Executable,
        lib: LibId,
        plan: &Arc<PreparedPlan>,
        site: u32,
        frame: &Frame,
        value: Value,
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
        self.pins.borrowed.extend(kept.iter().copied());
        let charging = std::mem::replace(&mut self.charging, false);
        self.plans += 1;
        let mut run = PlanRun {
            machine: self,
            task,
            exe,
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
                        .map(|role| run.machine.admit_page(&role.value));
                    match role {
                        Some(Ok(role)) => run.machine.put(base + *reg as usize, role),
                        Some(Err(halt)) => run.halt = Some(Box::new(halt)),
                        None => {
                            run.halt =
                                Some(Box::new(fault("a saved continuation role is missing")));
                        }
                    }
                }
                // The value is in no variable until the statement uses it.
                run.machine.pins.borrowed.push(value);
                run.machine.put(base + stood.dst as usize, value);
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
        self.charging = charging;
        if matches!(outcome, Err(PlanExit::Yield)) && halt.is_none() {
            self.stand_segments(task, exe, None)?;
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
fn segment_plan(run: &LibRun) -> Option<(&Arc<PreparedPlan>, CodeId)> {
    match run {
        LibRun::Plan { plan, body } => Some((plan, *body)),
        LibRun::Native {
            fallback: Some(plan),
            body: Some(body),
            ..
        } => Some((plan, *body)),
        _ => None,
    }
}

/// The resolved segment of `lib`'s plan and its body's code.
fn segment_of<'p>(
    exe: &Executable,
    lib: LibId,
    plan: &'p PreparedPlan,
) -> Result<(&'p Segment, CodeId), Halt> {
    let (_, body) = segment_plan(&exe.lib(lib).run)
        .ok_or_else(|| fault("a segment's function runs no plan"))?;
    let segment = plan
        .segment
        .as_ref()
        .ok_or_else(|| fault("a plan with no callback site yielded"))?;
    Ok((segment, body))
}
