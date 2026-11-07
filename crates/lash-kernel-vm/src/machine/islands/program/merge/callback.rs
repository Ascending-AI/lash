//! One source call descriptor for collection callbacks and native reductions.
use super::*;

#[derive(Clone)]
pub(in crate::machine::islands) struct Callback {
    target: Target,
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
    },
    Guest,
}
impl Callback {
    pub(in crate::machine::islands) fn library(&self) -> Option<LibId> {
        match self.target {
            Target::Native { lib, .. } | Target::Body { lib, .. } => Some(lib),
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
                LibRun::Body(_) | LibRun::Native { body: Some(_), .. } => Target::Body { lib },
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
                            local(value)?;
                            Target::Guest
                        }
                        crate::compile::expr::Callee::Declared(_) => Target::Guest,
                    }
                };
                (target, &**args)
            }
            _ => return None,
        };
        Some((Callback { target }, args))
    }
}
