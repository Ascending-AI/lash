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

/// A source call edge and its possible admitted targets. Dynamic dispatch
/// selects by the actual callable's CodeId, never by evaluating candidate paths.
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
    targets: Box<[CalleePlan]>,
    occurrence: Option<usize>,
    result: CallRole,
    dispatch: dispatch::Dispatch,
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
        let static_target = target::resolve(exe, caller, statement, pc, None);
        let (resolved, targets) = match static_target {
            Ok(resolved) => {
                let callee = CalleePlan::compile(exe, caller, &resolved)?;
                (resolved, vec![callee])
            }
            Err(_) => {
                let mut targets = Vec::new();
                let mut selected = None;
                for index in 0..exe.code_count() {
                    let code = CodeId(u32::try_from(index).map_err(|_| Refused::Path)?);
                    let Ok(resolved) = target::resolve(exe, caller, statement, pc, Some(code))
                    else {
                        continue;
                    };
                    if !resolved.dynamic {
                        return Err(Refused::Source);
                    }
                    if let Ok(callee) = CalleePlan::compile(exe, caller, &resolved) {
                        selected = Some(resolved);
                        targets.push(callee);
                    }
                }
                (selected.ok_or(Refused::Source)?, targets)
            }
        };
        let CallerRoles {
            arguments,
            recovery: saved,
            callable,
        } = caller_roles(&source, &resolved)?;
        let callable_origin = resolved
            .dynamic
            .then(|| target::root_slot(&source, &resolved.at))
            .transpose()?
            .map(CallRole::new);
        let result = CallRole::new(slot(&source, &Owner::Operand(resolved.at.site.clone()))?);
        let resume = Point {
            site: resolved.at.site.clone(),
            phase: if resolved.rooted {
                Phase::Commit
            } else {
                Phase::Restore
            },
        };
        source.compile_event(&resume, ExitKind::Normal)?;
        let dispatch = dispatch::Dispatch::compile(exe, &targets)?;
        Ok(Self {
            executable: exe.local(),
            caller,
            source,
            statement,
            pc,
            at: resolved.at,
            resume,
            arguments: arguments.iter().copied().map(CallRole::new).collect(),
            saved: saved.iter().copied().map(CallRole::new).collect(),
            callable: callable.map(CallRole::new),
            callable_origin,
            result,
            dispatch,
            targets: targets.into(),
            occurrence: resolved.occurrence,
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
            || self.dispatch != expected.dispatch
            || self.occurrence != expected.occurrence
            || self.targets.len() != expected.targets.len()
        {
            return Err(Refused::Source);
        }
        for (actual, expected) in self.targets.iter().zip(&expected.targets) {
            actual.verify_placement(expected)?;
        }
        Ok(())
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
    pub(crate) fn targets(&self) -> &[CalleePlan] {
        &self.targets
    }
    pub(crate) fn static_target(&self) -> Option<&CalleePlan> {
        if self.callable.is_none() {
            self.targets.first()
        } else {
            None
        }
    }
    pub(crate) fn result_role(&self) -> CallRole {
        self.result
    }
    /// Bind actual lease-local or imported callable identities before entry.
    /// Unsupported/opaque callbacks end the lease before invocation.
    pub(crate) fn bind_callable<'plan, 'lease>(
        &'plan self,
        exe: &Executable,
        lease: &'lease mut StateLease<'_>,
        value: LeaseValue,
    ) -> Result<Option<CallBinding<'plan, 'lease>>, LeaseFault> {
        if self.callable.is_none() || self.executable != exe.local() {
            return Ok(None);
        }
        let view = lease.callable(value)?;
        let Some(index) = self.dispatch.target(exe, view.identity(), view.closure()) else {
            return Ok(None);
        };
        let plan = &self.targets[index];
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
