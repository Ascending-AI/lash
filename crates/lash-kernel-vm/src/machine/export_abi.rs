//! Public argument validation before adapting a source-language calling convention.
use super::ops::Reg;
use super::{Eval, KernelMachine, fault, raise};
use crate::compile::Executable;
use lash_kernel_doc::{NewObject, Value};
impl KernelMachine {
    pub(super) fn export_arguments(
        &mut self,
        exe: &Executable,
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
                && !self.public_value_fits(exe, value, &param.ty, self.limits.value_depth())?
            {
                return raise(
                    "function_argument",
                    format!(
                        "argument `{}` does not fit the imported function signature",
                        param.name
                    ),
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
