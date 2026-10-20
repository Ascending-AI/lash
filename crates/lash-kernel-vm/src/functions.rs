//! The library functions the machine implements itself (`K-LIB-010`).
//!
//! `deref` needs the kind of the object a ref names, `tasks.unfinished`
//! needs the task set and the caller, and `brand.built` the library body its
//! call is written in; a native function sees none of them. Their
//! definitions live here, an embedder registers them with
//! [`register_machine_functions`], and the machine recognises them by
//! identity.

use std::collections::BTreeSet;
use std::sync::{Arc, LazyLock};

use lash_kernel_doc::{
    Formula, FunctionDefinition, FunctionId, FunctionRegistry, Implementation, KERNEL_VERSION,
    Name, NativeCall, NativeError, NativeFunction, Operand, Param, QualifiedName, RegistryError,
    Signature, Type, Value,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MachineFunction {
    Deref,
    TasksUnfinished,
    BrandBuilt,
}

/// The three, in the order they register.
const ALL: [MachineFunction; 3] = [
    MachineFunction::Deref,
    MachineFunction::TasksUnfinished,
    MachineFunction::BrandBuilt,
];

/// Whether `name` is one of their names.
pub(crate) fn machine_name(name: &str) -> bool {
    matches!(name, "deref" | "tasks.unfinished" | "brand.built")
}

/// The identities of the machine's own functions in a registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MachineFunctions {
    /// `deref(r)`: the object or task handle a ref names (`K-KEY-004`).
    pub deref: FunctionId,
    /// `tasks.unfinished()`: the handles of every other task that has not
    /// ended, in spawn order (`K-TASK-021`).
    pub tasks_unfinished: FunctionId,
    /// `brand.built()`: the identity of the library function whose body,
    /// or a closure in it, the call is written in; absent in a document's
    /// own code (`K-LIB-010`). A package value records its constructor's.
    pub brand_built: FunctionId,
}

/// The identity of `brand.built` under this build's kernel version: the
/// lowering answers a call of it where the call is written.
pub(crate) fn brand_built() -> FunctionId {
    static BRAND_BUILT: LazyLock<FunctionId> =
        LazyLock::new(|| definition(MachineFunction::BrandBuilt, KERNEL_VERSION).identity());
    *BRAND_BUILT
}

#[expect(
    clippy::expect_used,
    reason = "the machine's functions have constant, well-formed qualified names"
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
        // Answered where its call is written, as a literal is: the node's
        // own unit is all the call costs.
        MachineFunction::BrandBuilt => (
            "brand.built",
            Vec::new(),
            Type::Union(vec![Type::Bytes, Type::Absent]),
            Formula::Constant(0),
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
    if !machine_name(candidate.name.as_str()) {
        return None;
    }
    ALL.into_iter()
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

/// Marks `deref`, `tasks.unfinished` and `brand.built`, whose definitions
/// the build's release image holds (FIG-6843), as run by the machine.
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
        brand_built: implement(MachineFunction::BrandBuilt)?,
    })
}

/// Registers `deref`, `tasks.unfinished` and `brand.built`, the three
/// functions of the kernel library that the machine implements itself.
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
        brand_built: register(MachineFunction::BrandBuilt)?,
    })
}

/// The machine-owned functions share a native marker; execution dispatches
/// them through the machine's own entry before that marker is called.
pub fn machine_native(name: &str) -> Option<Arc<dyn NativeFunction>> {
    ALL.into_iter()
        .any(|function| definition(function, KERNEL_VERSION).name.as_str() == name)
        .then(|| Arc::new(RunByTheMachine) as Arc<dyn NativeFunction>)
}
