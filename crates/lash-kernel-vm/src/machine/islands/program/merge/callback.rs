//! One source call descriptor for collection callbacks and native reductions.
use super::*;

type ProgramCache = std::sync::Arc<
    std::sync::Mutex<
        std::collections::BTreeMap<
            crate::compile::CodeId,
            Option<std::sync::Arc<crate::compile::room::publication::sort::CallbackProgram>>,
        >,
    >,
>;

#[derive(Clone)]
pub(in crate::machine::islands) struct Callback {
    statement: StmtId,
    pc: u32,
    target: Target,
    programs: ProgramCache,
}
#[derive(Clone)]
enum Target {
    Native {
        lib: LibId,
        checked: bool,
        depth: Depth,
    },
    Body {
        lib: LibId,
        code: crate::compile::CodeId,
    },
    Declared(crate::compile::CodeId),
    Binding(usize),
}
impl Callback {
    pub(in crate::machine::islands) fn program(
        &self,
        exe: &Executable,
        caller: crate::compile::CodeId,
        callee: crate::compile::CodeId,
    ) -> Option<std::sync::Arc<crate::compile::room::publication::sort::CallbackProgram>> {
        let mut programs = self.programs.lock().ok()?;
        programs
            .entry(callee)
            .or_insert_with(|| {
                crate::compile::room::publication::sort::CallbackProgram::compile(
                    exe,
                    caller,
                    self.statement,
                    self.pc,
                    callee,
                )
                .ok()
                .map(std::sync::Arc::new)
            })
            .clone()
    }
    pub(in crate::machine::islands) fn statement(&self) -> StmtId {
        self.statement
    }
    pub(in crate::machine::islands) fn library(&self) -> Option<LibId> {
        match self.target {
            Target::Native { lib, .. } | Target::Body { lib, .. } => Some(lib),
            _ => None,
        }
    }
    pub(in crate::machine::islands) fn body(&self) -> Option<crate::compile::CodeId> {
        match self.target {
            Target::Body { code, .. } | Target::Declared(code) => Some(code),
            _ => None,
        }
    }
    pub(in crate::machine::islands) fn binding(&self) -> Option<usize> {
        match self.target {
            Target::Binding(at) => Some(at),
            _ => None,
        }
    }
    pub(in crate::machine::islands) fn native(&self) -> Option<(LibId, bool, Depth)> {
        match self.target {
            Target::Native {
                lib,
                checked,
                depth,
            } => Some((lib, checked, depth)),
            _ => None,
        }
    }
}
impl Build<'_> {
    pub(super) fn callback<'a>(
        &self,
        statement: StmtId,
        value: &'a Expr,
    ) -> Option<(Callback, &'a [Expr])> {
        let value = unpin(value);
        let library = |lib, checked| {
            let function = self.exe.lib(lib);
            function.limit.is_none().then_some(())?;
            Some(match function.run {
                LibRun::Body(code)
                | LibRun::Native {
                    body: Some(code), ..
                } => Target::Body { lib, code },
                LibRun::Native { body: None, .. } => {
                    let native = function.specialization.native.immediate.map(Entry::Native);
                    let entry = native.as_ref().or_else(|| {
                        function
                            .specialization
                            .prepared
                            .as_ref()
                            .map(|(entry, _)| entry)
                    })?;
                    entry.immediate().then_some(())?;
                    let extra = entry.depth();
                    let depth = if function.operation() {
                        Depth::Operation(extra)
                    } else if extra > 0 {
                        Depth::Template(extra)
                    } else {
                        Depth::None
                    };
                    Target::Native {
                        lib,
                        checked,
                        depth,
                    }
                }
                _ => return None,
            })
        };
        let (target, args) = match &value.kind {
            Kind::Operator {
                lib, args, invoke, ..
            } => (
                library(*lib, invoke.is_some_and(|invoke| invoke.checked))?,
                &**args,
            ),
            Kind::Native { lib, args, .. } => (library(*lib, false)?, &**args),
            Kind::Call {
                invoke,
                callee,
                args,
                ..
            } => {
                let lib = invoke
                    .act
                    .and_then(|act| self.stream.nested[act.ix()].kind.operator_lib())
                    .or_else(|| match &self.exe.stmt(statement).form {
                        crate::compile::Form::Action(action) => match &action.kind {
                            crate::compile::ActionKind::Call { callee, .. } => callee.library(),
                            _ => None,
                        },
                        _ => None,
                    });
                let target = if let Some(lib) = lib {
                    library(lib, invoke.checked)?
                } else {
                    match callee {
                        crate::compile::expr::Callee::Read(value) => {
                            let at = local(value)? as usize;
                            (!self.shared[at]).then_some(())?;
                            Target::Binding(at)
                        }
                        crate::compile::expr::Callee::Declared(code) => Target::Declared(*code),
                    }
                };
                (target, &**args)
            }
            _ => return None,
        };
        Some((
            Callback {
                statement,
                pc: value.pc,
                target,
                programs: Default::default(),
            },
            args,
        ))
    }
}
