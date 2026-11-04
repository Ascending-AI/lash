//! One source entry joins a callback's caller, value flow and ordered debt.
use super::*;
use crate::compile::room::{
    placement::DenseActivation,
    region::calls::{
        VerifiedCall,
        scope::{CallScope, ScopedCall},
    },
};
use crate::compile::{CodeId, Executable, StmtId};
use lash_kernel_doc::Value;
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::logical::{Dependencies, InvocationRange, Slot},
};
use std::{cell::RefCell, collections::BTreeMap};
mod billing;
pub(in crate::compile::room) mod caller;
mod runtime;
pub(crate) use runtime::CallbackPublication;

pub(crate) enum CallbackTarget {
    Value(Value),
    Declared(CodeId),
}

type CallerKey = (Point, Point, Vec<(Site, bool)>);
type CallerCache = BTreeMap<CallerKey, Result<Arc<CallerProgram>, Refused>>;
pub(crate) struct CallbackProgram {
    executable: u64,
    code: CodeId,
    statement: StmtId,
    pc: u32,
    call: Arc<VerifiedCall>,
    arguments: Box<[Slot]>,
    callers: std::sync::Mutex<CallerCache>,
}
impl CallbackProgram {
    pub(crate) fn compile(
        exe: &Executable,
        code: CodeId,
        statement: StmtId,
        pc: u32,
        callee: CodeId,
    ) -> Result<Self, Refused> {
        let call = Arc::new(VerifiedCall::compile_callback(
            exe,
            code,
            statement,
            pc,
            exe.stmt(statement).ops.start,
            callee,
        )?);
        let arguments = call.argument_roles().collect();
        Ok(Self {
            executable: exe.local(),
            code,
            statement,
            pc,
            call,
            arguments,
            callers: Default::default(),
        })
    }
    pub(crate) fn resolve(exe: &Executable, arena: &Arena, value: Value) -> Option<CodeId> {
        match value {
            Value::Closure(handle) => {
                let (identity, _, _) = arena.closure(handle).ok()?;
                crate::machine::project::closure_code(
                    exe,
                    &identity.callable,
                    &identity.site.site,
                    &identity.capture_names,
                )
                .ok()
            }
            Value::Function(handle) => {
                let id = arena.callable(handle).ok()?;
                if !exe.admits_callable(&id) {
                    return None;
                }
                exe.declared.get(&id.function).copied().or_else(|| {
                    let lib = exe.lib(exe.library_lib(&id.function)?);
                    match lib.run {
                        crate::compile::LibRun::Body(code)
                        | crate::compile::LibRun::Native {
                            body: Some(code), ..
                        } => Some(code),
                        _ => None,
                    }
                })
            }
            _ => None,
        }
    }
    pub(in crate::compile::room) fn call(&self) -> &Arc<VerifiedCall> {
        &self.call
    }
    pub(in crate::compile::room) fn caller_code(&self) -> CodeId {
        self.code
    }
    pub(in crate::compile::room) fn arguments(&self) -> &[Slot] {
        &self.arguments
    }
    pub(crate) fn caller_program(
        &self,
        exe: &Executable,
        first: Point,
        through: Point,
        choices: &[(Site, bool)],
    ) -> Result<Arc<CallerProgram>, Refused> {
        if self.executable != exe.local() {
            return Err(Refused::Source);
        }
        let key = (first.clone(), through.clone(), choices.to_vec());
        let mut callers = self.callers.lock().map_err(|_| Refused::Source)?;
        if let Some(result) = callers.get(&key) {
            return result.clone();
        }
        if callers.len() >= 128 {
            return Err(Refused::Path);
        }
        let result = (|| {
            let route = caller::caller_route(
                exe,
                self.code,
                self.statement,
                first,
                through,
                &self.call,
                choices,
            )?;
            let proof = self.compile_caller_values(exe, &route.interval)?;
            let billing = billing::Billing::compile(self, exe, &route, choices, proof.depth())?;
            Ok(Arc::new(CallerProgram {
                route,
                proof,
                billing,
            }))
        })();
        callers.insert(key, result.clone());
        result
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "bind one actual callback activation and its affine entry"
    )]
    fn bind<'a>(
        &self,
        exe: &Executable,
        frontier: &Frontier,
        ledger: &'a Ledger,
        arena: &Arena,
        caller: &Invocation,
        activation: &DenseActivation<'_>,
        range: InvocationRange,
        target: CallbackTarget,
    ) -> Option<CallScope<'a>> {
        if self.executable != exe.local() {
            return None;
        }
        let entry = frontier.call_entry()?;
        match target {
            CallbackTarget::Value(value) => self
                .call
                .bind_callback_scope(exe, entry, ledger, arena, caller, activation, range, value),
            CallbackTarget::Declared(code) => self
                .call
                .bind_scope(exe, entry, code, ledger, caller, activation, range),
        }
    }
}

pub(in crate::compile::room) trait CallerProof: Send + Sync {
    fn depth(&self) -> u64;
    fn evaluate(
        &self,
        context: CallerContext<'_, '_, '_, '_, '_>,
    ) -> Result<Option<CallerFacts>, HeapFault>;
}
pub(in crate::compile::room) struct CallerContext<'input, 'frame, 'plan, 'scope, 'ledger> {
    pub exe: &'input Executable,
    pub activation: &'input DenseActivation<'frame>,
    pub ledger: &'input Ledger,
    pub heap: &'input PagedHeap,
    pub arena: &'input mut Arena,
    pub invocation: &'input Invocation,
    pub callback: &'input CallbackEvaluation<'plan, 'scope, 'ledger>,
}
pub(in crate::compile::room) struct CallerFacts {
    pub room: CheckedRoom,
    pub dependencies: Dependencies,
    pub inputs: Vec<logical::Owner>,
    pub outputs: Vec<(logical::Owner, logical::PublicationValue)>,
    pub members: Vec<MemberEdit>,
    pub values: Vec<(Slot, Value)>,
    pub clears: Vec<Slot>,
    pub formulas: BTreeMap<Point, (FunctionId, Vec<Value>, Value)>,
}
pub(crate) struct CallerProgram {
    route: caller::CallerRoute,
    proof: Arc<dyn CallerProof>,
    billing: billing::Billing,
}
pub(in crate::compile::room) struct CallbackEvaluation<'plan, 'scope, 'ledger> {
    program: &'plan CallbackProgram,
    exe: &'scope Executable,
    scope: &'scope CallScope<'ledger>,
    candidates: RefCell<Vec<ScopedCall<'plan>>>,
    expected: &'scope [Value],
}
pub(in crate::compile::room) struct CallbackAnswer {
    pub result: Value,
    pub room: CheckedRoom,
    pub function: FunctionId,
}
impl<'plan> CallbackEvaluation<'plan, '_, '_> {
    pub(in crate::compile::room) fn answer(
        &self,
        expected: &Arc<VerifiedCall>,
        at: &Point,
        arena: &Arena,
        roles: &[(Slot, Value)],
        arguments: &[Value],
    ) -> Result<Option<CallbackAnswer>, HeapFault> {
        if !Arc::ptr_eq(expected, &self.program.call)
            || expected.at() != at
            || self.program.arguments.len() != arguments.len()
            || self
                .program
                .arguments
                .iter()
                .zip(arguments)
                .any(|(slot, expected)| {
                    !roles
                        .iter()
                        .any(|(actual, value)| actual == slot && value.same(expected))
                })
        {
            return Ok(None);
        }
        let mut candidates = self.candidates.borrow_mut();
        if candidates.is_empty()
            && (arguments.len() != self.expected.len()
                || arguments.iter().zip(self.expected).any(|(a, b)| !a.same(b)))
        {
            return Ok(None);
        }
        let offset = candidates.last().map_or(Some(0), ScopedCall::next_offset);
        let Some(offset) = offset else {
            return Ok(None);
        };
        let Some(candidate) = self.program.call.evaluate_scoped(
            self.exe,
            self.program.call.callee(),
            self.scope,
            offset,
            arena,
            roles,
        )?
        else {
            return Ok(None);
        };
        let room = candidate.checked_room();
        let doc::constant_pool::PoolOwner::Function(function) = room.owner else {
            return Ok(None);
        };
        let result = candidate.result();
        candidates.push(candidate);
        Ok(Some(CallbackAnswer {
            result,
            room,
            function,
        }))
    }
    fn finish(self) -> Vec<ScopedCall<'plan>> {
        self.candidates.into_inner()
    }
}
