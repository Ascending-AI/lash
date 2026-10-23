//! Public argument validation before adapting a source-language calling convention.
use super::actions::Target;
use super::exec::Call;
use super::ops::Reg;
use super::ops::{Flow, Status};
use super::{Eval, KernelMachine, fault, raise};
use crate::compile::Executable;
use crate::compile::LibId;
use lash_kernel_doc::{NewObject, Value};
impl KernelMachine {
    /// A document enters a body helper under its declared contract, as it
    /// enters imported code. The helper keeps the caller's Link for operations;
    /// its internal kernel calls and native expressions retain K-FN-007.
    pub(super) fn build_helper_target(&self, exe: &Executable, lib: LibId) -> Target {
        let function = exe.lib(lib);
        Target::Export(
            lash_vocabulary::CallableId {
                function: function.id,
                link: self.current_link(),
            },
            Box::new(lash_kernel_doc::FunctionExport {
                function: function.id,
                signature: function.header.signature.clone(),
                convention: lash_kernel_doc::StartConvention::Direct,
                asynchronous: false,
                rest: false,
            }),
        )
    }

    pub(super) fn import_target(
        &self,
        exe: &Executable,
        slot: lash_vocabulary::names::ImportSlot,
    ) -> Eval<Target> {
        let instance = self.program.instances.get(&self.current_link());
        match instance.and_then(|instance| instance.slots.get(&slot)) {
            Some(crate::interface::PreparedSlot::Function { callable, export }) => {
                let document = exe
                    .instances
                    .get(&callable.link)
                    .ok_or_else(|| fault("export Link is not loaded"))?;
                let descriptor = match (document.exports.get(&export.name), &export.member) {
                    (Some(lash_kernel_doc::Export::Function(function)), None) => function,
                    (Some(lash_kernel_doc::Export::Type(ty)), Some(member)) => {
                        match ty.members.get(member) {
                            Some(
                                lash_kernel_doc::TypeMember::Static(function)
                                | lash_kernel_doc::TypeMember::Method(function)
                                | lash_kernel_doc::TypeMember::Property(function),
                            ) => function,
                            _ => return Err(fault("imported Type member is absent").into()),
                        }
                    }
                    _ => return Err(fault("imported function Export is absent").into()),
                };
                Ok(Target::Export(*callable, Box::new(descriptor.clone())))
            }
            _ => Err(fault("call import does not name linked code").into()),
        }
    }

    pub(super) fn call_export(
        &mut self,
        task: lash_kernel_doc::TaskId,
        exe: &Executable,
        callable: lash_vocabulary::CallableId,
        descriptor: &lash_kernel_doc::FunctionExport,
        invocation: super::exec::Invocation<'_>,
    ) -> Status {
        let super::exec::Invocation {
            args,
            count,
            awaiting,
            ..
        } = invocation;
        let count = self.export_arguments(exe, callable, descriptor, args, count)?;
        let flow = match self.callable_target(exe, callable)? {
            Target::Library(lib, link) => self.call_lib_linked(
                task,
                exe,
                (lib, link),
                super::exec::Invocation::new(args, count)
                    .awaited_by(awaiting)
                    .with_boundary(Some(descriptor.into())),
            )?,
            Target::Code(code, captures, link) => {
                let pushed = self.push_frame(
                    task,
                    exe,
                    Call::new(code, args, count)
                        .in_link(link)
                        .sharing(&captures)
                        .awaited_by(awaiting)
                        .exported(Some(descriptor)),
                );
                self.spent_captures(captures);
                pushed?;
                Flow::Step
            }
            Target::Export(..) => return Err(fault("an export resolved to another export").into()),
        };
        match flow {
            Flow::Done => {
                let value = self.regs[args];
                self.check_public_result(exe, &value, &descriptor.signature.result, callable)?;
            }
            Flow::Step => {}
        }
        Ok(flow)
    }

    pub(super) fn export_arguments(
        &mut self,
        exe: &Executable,
        callable: lash_vocabulary::CallableId,
        descriptor: &lash_kernel_doc::FunctionExport,
        args: Reg,
        count: usize,
    ) -> Eval<usize> {
        use lash_kernel_doc::StartConvention;
        if matches!(
            descriptor.convention,
            StartConvention::Direct | StartConvention::PyDirect { .. }
        ) && !(descriptor.signature.required()..=descriptor.signature.params.len())
            .contains(&count)
        {
            return raise(
                "arity",
                "arguments do not fit the imported function signature",
            );
        }
        for (index, param) in descriptor.signature.params.iter().enumerate().take(count) {
            let value = self.regs[args + index];
            if !(param.optional && matches!(value, Value::Absent))
                && let Some(mismatch) =
                    self.public_value_fits(exe, value, &param.ty, self.limits.value_depth())?
            {
                return self.raise_typed(
                    "function_argument",
                    &lash_vocabulary::EntryTypeMismatch {
                        callable,
                        mismatch: mismatch.within(param.name.as_str()).within("input"),
                    },
                );
            }
        }
        match descriptor.convention {
            StartConvention::TsArgsList { .. } => {
                let values = self.regs[args..args + count].to_vec();
                let list = self.new_object(NewObject::List(values))?;
                self.put(args, Value::Absent);
                self.put(args + 1, list);
                Ok(2)
            }
            StartConvention::TsDirect { .. } => {
                // TsDirect permits omitted and dropped arguments. The public
                // arity matches the private declaration's fixed parameter slots.
                let arity = exe
                    .declared
                    .get(&descriptor.function)
                    .map(|code| exe.code(*code).params.len())
                    .or_else(|| {
                        exe.lib_of(&descriptor.function)
                            .map(|lib| exe.lib(lib).arity)
                    })
                    .ok_or_else(|| fault("export body is not loaded"))?;
                for index in count..arity {
                    self.put(args + index, Value::Absent);
                }
                Ok(count.min(arity))
            }
            _ => Ok(count),
        }
    }
}
