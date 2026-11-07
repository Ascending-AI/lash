//! Source-authenticated call topology. Execution borrows the region's slots;
//! this module never evaluates arguments, branches or a callee body.
#![allow(dead_code, reason = "lane C connects verified region call execution")]
use super::*;
use crate::compile::region_isa::{CallId, RegionDraft, RegionOp};
use crate::machine::region_isa::{CallActivation, RegionError};
use lash_kernel_state::kernel_internals::logical::{
    LeaseCallable, LeaseFault, LeaseValue, StateLease,
};
#[path = "region_isa/dispatch.rs"]
mod dispatch;
#[path = "region_isa/roles.rs"]
mod roles;
pub(crate) use roles::{CallBinding, CallRole, ParameterBinding};
#[path = "region_isa/callee.rs"]
pub(crate) mod callee;
#[path = "region_isa/debt.rs"]
mod debt;
#[cfg(test)]
#[path = "region_isa/tests.rs"]
mod tests;
pub(crate) use callee::CalleePlan;
pub(crate) use debt::CallDebt;

/// A source call edge and its admitted targets. Dynamic dispatch selects by
/// the actual callable's CodeId, never by evaluating candidate paths, and
/// compiles a target's plan the first time a run reaches it.
#[derive(Clone)]
pub(crate) struct CallPlan {
    executable: u64,
    caller: CodeId,
    source: Arc<Source>,
    statement: StmtId,
    pc: u32,
    at: Point,
    resume: Point,
    arguments: Box<[CallRole]>,
    saved: Box<[CallRole]>,
    /// The roles a dynamic call reads its callee from: a slot, or the
    /// session cell its name resolves to.
    callable: Option<Role>,
    callable_origin: Option<Role>,
    targets: Targets,
    occurrence: Option<usize>,
    result: CallRole,
    /// The helper whose signature the arguments must fit before entry.
    checked: Option<LibId>,
    /// Whether the caller holds the arguments across the call.
    rooted: bool,
    /// The protocol `for` hook this call runs, at that phase of its loop, or
    /// a `return via` policy at `Phase::Policy`.
    hook: Option<Phase>,
}

#[derive(Clone)]
enum Targets {
    Static(Arc<CalleePlan>),
    Dynamic {
        site: Arc<target::DynamicSite>,
        dispatch: Arc<std::sync::Mutex<dispatch::Dispatch>>,
    },
}

pub(crate) fn compile_calls(
    exe: &Executable,
    draft: &RegionDraft,
) -> Result<Box<[CallPlan]>, Refused> {
    if draft.code().0 as usize >= exe.code_count()
        || !Arc::ptr_eq(draft.source(), &exe.code(draft.code()).source_room)
    {
        return Err(Refused::Source);
    }
    let sites = draft.call_sources();
    let mut referenced = vec![false; sites.len()];
    for op in draft.ops() {
        if let RegionOp::Call { site, .. } = op {
            *referenced
                .get_mut(site.index() as usize)
                .ok_or(Refused::Source)? = true;
        }
    }
    if referenced.iter().any(|used| !used) {
        return Err(Refused::Source);
    }
    sites
        .iter()
        .enumerate()
        .map(|(index, &(statement, pc, hook))| {
            if sites[..index].contains(&(statement, pc, hook)) {
                return Err(Refused::Source);
            }
            match hook {
                Some(phase) => {
                    let plan = CallPlan::compile_hook(exe, draft.code(), statement, phase)?;
                    if plan.pc != pc {
                        return Err(Refused::Source);
                    }
                    Ok(plan)
                }
                None => CallPlan::compile(exe, draft.code(), statement, pc),
            }
        })
        .collect()
}

impl CallPlan {
    pub(crate) fn compile(
        exe: &Executable,
        caller: CodeId,
        statement: StmtId,
        pc: u32,
    ) -> Result<Self, Refused> {
        if statement.0 as usize >= exe.stmt_count() {
            return Err(Refused::Source);
        }
        let stmt = exe.stmt(statement);
        if exe.stmt_at(caller, &stmt.site) != Some(statement) {
            return Err(Refused::Source);
        }
        let source = Arc::clone(&exe.code(caller).source_room);
        source.verify()?;
        let site = target::call_site(exe, caller, statement, pc)?;
        let (shape, occurrence, checked) = match &site {
            target::CallSite::Static(resolved) => (
                resolved.shape(),
                resolved.occurrence,
                resolved
                    .checked
                    .then_some(resolved.library)
                    .map(|library| library.ok_or(Refused::Source))
                    .transpose()?,
            ),
            target::CallSite::Dynamic(site) => (site.shape(), site.occurrence(), None),
        };
        let CallerRoles {
            arguments,
            recovery: saved,
            callable,
        } = caller_roles(&source, shape)?;
        let callable_origin = shape
            .dynamic
            .then(|| target::root_role(&source, shape.at))
            .transpose()?;
        let result = CallRole::new(slot(&source, &Owner::Operand(shape.at.site.clone()))?);
        let rooted = shape.rooted;
        let resume = Point {
            site: shape.at.site.clone(),
            phase: if shape.rooted {
                Phase::Commit
            } else {
                Phase::Restore
            },
        };
        source.compile_event(&resume, ExitKind::Normal)?;
        let at = shape.at.clone();
        let targets = match site {
            target::CallSite::Static(resolved) => {
                Targets::Static(Arc::new(CalleePlan::compile(exe, caller, &resolved)?))
            }
            target::CallSite::Dynamic(site) => Targets::Dynamic {
                site: Arc::new(site),
                dispatch: Arc::default(),
            },
        };
        Ok(Self {
            executable: exe.local(),
            caller,
            source,
            statement,
            pc,
            at,
            resume,
            arguments: arguments.iter().copied().map(CallRole::new).collect(),
            saved: saved.iter().copied().map(CallRole::new).collect(),
            callable,
            callable_origin,
            result,
            targets,
            occurrence,
            checked,
            rooted,
            hook: None,
        })
    }

    /// A protocol `for`'s hook call (`K-ITER-005`). `next` and `check` take
    /// what the loop walks and `item` the item its header holds too, which
    /// the loop keeps; `open` moves the iterated value into its callee.
    /// `check` takes the guard's counts besides. A hook resumes in its
    /// loop's header: `item` at the Test that takes its result.
    pub(crate) fn compile_hook(
        exe: &Executable,
        caller: CodeId,
        statement: StmtId,
        phase: Phase,
    ) -> Result<Self, Refused> {
        use callee::{Argument, GuardValue};
        let source = Arc::clone(&exe.code(caller).source_room);
        source.verify()?;
        let (resolved, pc) = resolved_hook(exe, caller, statement, phase)?;
        let site = resolved.at.site.clone();
        // A `return via` policy takes the frame's departing value; its result
        // reaches the caller as the frame's return.
        let (cursor, header) = if phase == Phase::Policy {
            let departure = slot(&source, &Owner::ActiveDeparture(source.site.clone()))?;
            (departure, departure)
        } else {
            (
                slot(&source, &Owner::LoopCursor(site.clone()))?,
                slot(&source, &Owner::LoopHeader(site.clone()))?,
            )
        };
        let arguments: Box<[_]> = match phase {
            Phase::Policy => Box::new([Argument::Role(cursor)]),
            Phase::Open => Box::new([Argument::Role(slot(
                &source,
                &Owner::Operand(site.child(0)),
            )?)]),
            Phase::Next => Box::new([Argument::Role(cursor)]),
            Phase::Check => Box::new([
                Argument::Role(cursor),
                Argument::Guard(GuardValue::Size),
                Argument::Guard(GuardValue::Remaining),
                Argument::Guard(GuardValue::More),
            ]),
            Phase::Item => Box::new([Argument::Role(cursor), Argument::Role(header)]),
            _ => return Err(Refused::Source),
        };
        let callee = CalleePlan::placed(exe, caller, &resolved, &arguments)?;
        Ok(Self {
            executable: exe.local(),
            caller,
            source,
            statement,
            pc,
            at: resolved.at.clone(),
            resume: Point {
                site: site.clone(),
                phase: match phase {
                    Phase::Item => Phase::Test,
                    Phase::Policy => Phase::Restore,
                    phase => phase,
                },
            },
            arguments: arguments
                .iter()
                .filter_map(|argument| match argument {
                    Argument::Role(slot) => Some(CallRole::new(*slot)),
                    Argument::Guard(_) => None,
                })
                .collect(),
            saved: Box::new([]),
            callable: None,
            callable_origin: None,
            targets: Targets::Static(Arc::new(callee)),
            occurrence: None,
            result: CallRole::new(header),
            checked: None,
            rooted: resolved.rooted,
            hook: Some(phase),
        })
    }

    /// Recompile immutable placement from source, without executing a value.
    /// The ISA verifier separately checks its register substitutions and edges.
    pub(crate) fn verify(&self, exe: &Executable) -> Result<(), Refused> {
        if self.executable != exe.local()
            || self.caller.0 as usize >= exe.code_count()
            || !Arc::ptr_eq(&self.source, &exe.code(self.caller).source_room)
        {
            return Err(Refused::Source);
        }
        let expected = match self.hook {
            Some(phase) => Self::compile_hook(exe, self.caller, self.statement, phase)?,
            None => Self::compile(exe, self.caller, self.statement, self.pc)?,
        };
        if self.pc != expected.pc
            || self.rooted != expected.rooted
            || self.hook != expected.hook
            || self.at != expected.at
            || self.resume != expected.resume
            || self.arguments != expected.arguments
            || self.saved != expected.saved
            || self.callable != expected.callable
            || self.callable_origin != expected.callable_origin
            || self.result != expected.result
            || self.occurrence != expected.occurrence
            || self.checked != expected.checked
        {
            return Err(Refused::Source);
        }
        match (&self.targets, &expected.targets) {
            (Targets::Static(actual), Targets::Static(expected)) => {
                actual.verify_placement(expected)
            }
            // A dynamic site's targets are compiled from source on demand.
            (Targets::Dynamic { .. }, Targets::Dynamic { .. }) => Ok(()),
            _ => Err(Refused::Source),
        }
    }
    pub(crate) fn verify_region(
        &self,
        exe: &Executable,
        draft: &RegionDraft,
        site: CallId,
    ) -> Result<(), Refused> {
        if draft.code() != self.caller
            || !Arc::ptr_eq(draft.source(), &self.source)
            || draft.call_sources().get(site.index() as usize)
                != Some(&(self.statement, self.pc, self.hook))
            || !draft
                .ops()
                .iter()
                .any(|op| matches!(op, RegionOp::Call { site: actual, .. } if *actual == site))
        {
            return Err(Refused::Source);
        }
        self.verify(exe)
    }
    #[cfg(test)]
    fn static_target_for_law(&mut self) -> &mut CalleePlan {
        match &mut self.targets {
            Targets::Static(plan) => Arc::make_mut(plan),
            Targets::Dynamic { .. } => panic!("law needs a static call"),
        }
    }
    pub(crate) fn belongs_to(&self, exe: &Executable) -> bool {
        self.executable == exe.local()
    }
    pub(crate) fn caller(&self) -> CodeId {
        self.caller
    }
    pub(crate) fn point(&self) -> &Point {
        &self.at
    }
    pub(crate) fn resume(&self) -> &Point {
        &self.resume
    }
    /// The pc of the opcode this call's source edge is.
    pub(crate) fn pc(&self) -> u32 {
        self.pc
    }
    /// A statement call holds its argument atoms until its Commit; a
    /// nested call moves its operands into the callee.
    pub(crate) fn rooted(&self) -> bool {
        self.rooted
    }
    /// The protocol `for` hook this call runs, if any.
    pub(crate) fn hook(&self) -> Option<Phase> {
        self.hook
    }
    pub(crate) fn arguments(&self) -> &[CallRole] {
        &self.arguments
    }
    pub(crate) fn saved_roles(&self) -> &[CallRole] {
        &self.saved
    }
    pub(crate) fn callable(&self) -> Option<Role> {
        self.callable
    }
    pub(crate) fn callable_origin(&self) -> Option<Role> {
        self.callable_origin
    }
    /// The helper whose signature this call's arguments must fit.
    pub(crate) fn checked(&self) -> Option<LibId> {
        self.checked
    }
    pub(crate) fn occurrence(&self) -> Option<usize> {
        self.occurrence
    }
    /// The static target, which a call without a callable value enters.
    pub(crate) fn static_target(&self) -> Option<&Arc<CalleePlan>> {
        match &self.targets {
            Targets::Static(plan) if self.callable.is_none() => Some(plan),
            _ => None,
        }
    }
    /// Whether the site selects its target by the actual callable.
    pub(crate) fn dynamic(&self) -> bool {
        matches!(self.targets, Targets::Dynamic { .. })
    }
    /// The plan for `code` as this site's target, or None when the site
    /// does not admit it.
    pub(crate) fn target(&self, exe: &Executable, code: CodeId) -> Option<Arc<CalleePlan>> {
        match &self.targets {
            Targets::Static(plan) => (plan.code() == code).then(|| Arc::clone(plan)),
            Targets::Dynamic { site, dispatch } => {
                if self.executable != exe.local() {
                    return None;
                }
                dispatch.lock().ok()?.code(exe, self.caller, site, code)
            }
        }
    }
    pub(crate) fn result_role(&self) -> CallRole {
        self.result
    }
    /// Bind actual lease-local or imported callable identities before entry.
    /// Unsupported/opaque callbacks end the lease before invocation.
    pub(crate) fn bind_callable<'lease>(
        &self,
        exe: &Executable,
        lease: &'lease mut StateLease<'_>,
        value: LeaseValue,
    ) -> Result<Option<CallBinding<'lease>>, LeaseFault> {
        let Targets::Dynamic { site, dispatch } = &self.targets else {
            return Ok(None);
        };
        if self.callable.is_none() || self.executable != exe.local() {
            return Ok(None);
        }
        let view = lease.callable(value)?;
        let Some(plan) = dispatch.lock().ok().and_then(|mut dispatch| {
            dispatch.callable(exe, self.caller, site, view.identity(), view.closure())
        }) else {
            return Ok(None);
        };
        if view.captures().len() != plan.captures().len() {
            return Ok(None);
        }
        Ok(Some(CallBinding {
            plan,
            callable: value,
            view,
        }))
    }
}

pub(crate) trait RegionCalls {
    fn enter_call(&mut self, site: CallId) -> Result<CallActivation, RegionError>;
    fn leave_call(&mut self, activation: CallActivation) -> Result<(), RegionError>;
}

#[cfg(any(test, feature = "testing"))]
pub(crate) mod fixtures {
    use super::*;
    pub(crate) fn empty_plan() -> Box<[CallPlan]> {
        Box::new([])
    }
    /// Uses the real source checks; a fixture supplies no extra authority.
    pub(crate) fn plan(
        exe: &Executable,
        caller: CodeId,
        statement: StmtId,
        pc: u32,
    ) -> Result<CallPlan, Refused> {
        CallPlan::compile(exe, caller, statement, pc)
    }
}
