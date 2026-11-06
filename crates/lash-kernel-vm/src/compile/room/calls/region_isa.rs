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
    callable: Option<CallRole>,
    callable_origin: Option<CallRole>,
    targets: Targets,
    occurrence: Option<usize>,
    result: CallRole,
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
        .map(|(index, &(statement, pc))| {
            if sites[..index].contains(&(statement, pc)) {
                return Err(Refused::Source);
            }
            CallPlan::compile(exe, draft.code(), statement, pc)
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
        let (shape, occurrence) = match &site {
            target::CallSite::Static(resolved) => (resolved.shape(), resolved.occurrence),
            target::CallSite::Dynamic(site) => (site.shape(), site.occurrence()),
        };
        let CallerRoles {
            arguments,
            recovery: saved,
            callable,
        } = caller_roles(&source, shape)?;
        let callable_origin = shape
            .dynamic
            .then(|| target::root_slot(&source, shape.at))
            .transpose()?
            .map(CallRole::new);
        let result = CallRole::new(slot(&source, &Owner::Operand(shape.at.site.clone()))?);
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
            callable: callable.map(CallRole::new),
            callable_origin,
            result,
            targets,
            occurrence,
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
        let expected = Self::compile(exe, self.caller, self.statement, self.pc)?;
        if self.at != expected.at
            || self.resume != expected.resume
            || self.arguments != expected.arguments
            || self.saved != expected.saved
            || self.callable != expected.callable
            || self.callable_origin != expected.callable_origin
            || self.result != expected.result
            || self.occurrence != expected.occurrence
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
            || draft.call_sources().get(site.index() as usize) != Some(&(self.statement, self.pc))
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
    /// A statement call holds its argument atoms until its Commit; a
    /// nested call moves its operands into the callee.
    pub(crate) fn rooted(&self) -> bool {
        self.resume.phase == Phase::Commit
    }
    pub(crate) fn arguments(&self) -> &[CallRole] {
        &self.arguments
    }
    pub(crate) fn saved_roles(&self) -> &[CallRole] {
        &self.saved
    }
    pub(crate) fn callable(&self) -> Option<CallRole> {
        self.callable
    }
    pub(crate) fn callable_origin(&self) -> Option<CallRole> {
        self.callable_origin
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
