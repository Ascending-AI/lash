//! The library functions the machine implements itself (`K-LIB-010`).
//!
//! `deref` needs the kind of the object a ref names and `tasks.unfinished`
//! needs the task set and the caller; a native function sees neither. Their
//! definitions live here, an embedder registers them with
//! [`register_machine_functions`], and the machine recognises them by
//! identity.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_kernel_doc::{
    Formula, FunctionDefinition, FunctionId, FunctionRegistry, Implementation, KERNEL_VERSION,
    Name, NativeCall, NativeError, NativeFunction, Operand, Param, QualifiedName, RegistryError,
    Signature, Type, Value,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MachineFunction {
    Deref,
    TasksUnfinished,
}

/// The identities of the machine's own functions in a registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MachineFunctions {
    /// `deref(r)`: the object or task handle a ref names (`K-KEY-004`).
    pub deref: FunctionId,
    /// `tasks.unfinished()`: the handles of every other task that has not
    /// ended, in spawn order (`K-TASK-021`).
    pub tasks_unfinished: FunctionId,
}

#[expect(
    clippy::expect_used,
    reason = "`deref` and `tasks.unfinished` are constant, well-formed qualified names"
)]
fn definition(function: MachineFunction, kernel: u32) -> FunctionDefinition {
    let (name, params, result, charge) = match function {
        MachineFunction::Deref => (
            "deref",
            vec![Param {
                name: Name::new("r"),
                ty: Type::Any,
                optional: false,
            }],
            Type::Any,
            Formula::Constant(1),
        ),
        MachineFunction::TasksUnfinished => (
            "tasks.unfinished",
            Vec::new(),
            Type::List(Box::new(Type::Task(Box::new(Type::Any)))),
            Formula::Size(Operand::Result),
        ),
    };
    FunctionDefinition {
        kernel,
        name: QualifiedName::new(name).expect("a constant qualified name"),
        signature: Signature { params, result },
        errors: BTreeSet::new(),
        charge,
        guard: None,
        implementation: Implementation::Native,
        native_version: lash_kernel_doc::FIRST_NATIVE_VERSION,
    }
}

/// Which of the machine's functions a definition is, if either.
pub(crate) fn machine_function(candidate: &FunctionDefinition) -> Option<MachineFunction> {
    // Only a definition with one of their names can be either; most are
    // told apart without building theirs.
    if !matches!(candidate.name.as_str(), "deref" | "tasks.unfinished") {
        return None;
    }
    [MachineFunction::Deref, MachineFunction::TasksUnfinished]
        .into_iter()
        .find(|function| definition(*function, candidate.kernel) == *candidate)
}

/// Stands in the registry for a function the machine runs itself. A
/// machine never calls it.
struct RunByTheMachine;

impl NativeFunction for RunByTheMachine {
    fn call(&self, _call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        Err(NativeError::refused(
            "type_error",
            "this function is implemented by the machine that runs the document",
        ))
    }
}

/// Marks `deref` and `tasks.unfinished`, whose definitions the build's
/// release image holds (FIG-6843), as run by the machine.
///
/// # Errors
///
/// The first [`FunctionRegistry::implement_named`] refuses.
pub fn implement_machine_functions(
    registry: &mut FunctionRegistry,
) -> Result<MachineFunctions, RegistryError> {
    let mut implement = |function| {
        let name = definition(function, KERNEL_VERSION).name;
        registry.implement_named(name.as_str(), Arc::new(RunByTheMachine))
    };
    Ok(MachineFunctions {
        deref: implement(MachineFunction::Deref)?,
        tasks_unfinished: implement(MachineFunction::TasksUnfinished)?,
    })
}

/// Registers `deref` and `tasks.unfinished`, the two functions of the
/// kernel library that the machine implements itself.
pub fn register_machine_functions(
    registry: &mut FunctionRegistry,
) -> Result<MachineFunctions, RegistryError> {
    let mut register = |function| {
        registry.register(
            definition(function, KERNEL_VERSION),
            Some(Arc::new(RunByTheMachine)),
        )
    };
    Ok(MachineFunctions {
        deref: register(MachineFunction::Deref)?,
        tasks_unfinished: register(MachineFunction::TasksUnfinished)?,
    })
}

/// The machine-owned functions share a native marker; execution dispatches
/// them through the machine's own entry before that marker is called.
pub fn machine_native(name: &str) -> Option<Arc<dyn NativeFunction>> {
    [MachineFunction::Deref, MachineFunction::TasksUnfinished]
        .into_iter()
        .any(|function| definition(function, KERNEL_VERSION).name.as_str() == name)
        .then(|| Arc::new(RunByTheMachine) as Arc<dyn NativeFunction>)
}
