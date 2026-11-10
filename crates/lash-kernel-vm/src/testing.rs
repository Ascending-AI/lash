//! Differential execution controls for embedding laws: a library's helper
//! bodies run in place of their natives keep the same semantic result,
//! effects and exact charge.
use crate::{KernelMachine, PreparedLibrary, StartError};

/// Seals and reads the canonical cold equality domain at a legal boundary.
/// The bytecode core seals site-keyed run records, which this reader does
/// not read, so this always refuses.
///
/// # Errors
/// Always: [`crate::SealError::Fault`] naming the format.
pub fn semantic_checkpoint(
    machine: &mut KernelMachine,
) -> Result<lash_kernel_state::semantic::Checkpoint, crate::SealError> {
    let _ = machine;
    Err(crate::SealError::Fault {
        problem: "the bytecode core's checkpoints are run records".into(),
    })
}

/// The installed definitions with every native beside a body removed, so the
/// core calls the body where it would call the native (`K-LIB-006`): the two
/// give equal values, errors and charge. A definition with no body keeps its
/// native.
///
/// # Panics
/// When the installed registry refuses one of its own definitions again.
pub fn bodies(library: &PreparedLibrary) -> PreparedLibrary {
    PreparedLibrary::with_helpers(
        std::sync::Arc::new(library.registry().bodies()),
        std::sync::Arc::clone(library.helpers()),
    )
}

/// A compiled and validated bytecode program, without a running machine.
pub struct BytecodeProgram {
    _code: crate::bytecode::program::Program,
}

/// Compiles every unit reached by this document and validates the bytecode.
/// No session, heap, host operations or machine start is required.
///
/// # Errors
/// Returns a typed start refusal for an unsupported document or malformed code.
pub fn compile_document(
    document: &lash_kernel_doc::Document,
    registry: &lash_kernel_doc::FunctionRegistry,
    link: lash_kernel_value::LinkId,
    entry: Option<&lash_kernel_doc::FunctionExport>,
) -> Result<BytecodeProgram, StartError> {
    let entry = entry.map(|export| crate::bytecode::compile::Entry {
        function: export.function,
        convention: &export.convention,
        captures: None,
    });
    let mut code = crate::bytecode::compile::compile(
        document,
        registry,
        link,
        &Default::default(),
        entry.as_ref(),
        None,
        &crate::bytecode::library::shared(registry, None),
    )
    .map_err(|problem| StartError::Linked {
        problem: problem.to_string(),
    })?;
    crate::bytecode::inline::inline(&mut code);
    crate::bytecode::validate::validate(&code).map_err(|problem| StartError::Linked {
        problem: problem.to_string(),
    })?;
    Ok(BytecodeProgram { _code: code })
}

/// How a registered definition runs on the core.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DefinitionClass {
    /// A native answers every call.
    NativeOnly,
    /// The kernel-code body runs every call.
    BodyOnly,
    /// The native answers; on a decline, or with natives off, the body.
    Both,
    /// The machine answers it itself (`K-LIB-010`).
    Machine,
    /// Its native is stated but not installed: no machine can call it.
    Declared,
}

/// Classifies every definition `registry` holds and compiles each body, with
/// everything it calls, on the core as a run's entry. Gives each function's
/// class, or the core's refusal to compile its body.
pub fn library_inventory(
    registry: &lash_kernel_doc::FunctionRegistry,
) -> Vec<(
    lash_kernel_value::names::FunctionId,
    String,
    Result<DefinitionClass, StartError>,
)> {
    let empty = lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::Float, Vec::new());
    let link = lash_kernel_value::LinkId::from_bytes([0; 32]);
    let library = crate::bytecode::library::shared(registry, None);
    registry
        .iter()
        .map(|(function, registered)| {
            let definition = registered.definition();
            let class = if crate::functions::machine_function(definition).is_some() {
                Ok(DefinitionClass::Machine)
            } else {
                let compiled = match definition.body() {
                    Some(_) => crate::bytecode::compile::compile(
                        &empty,
                        registry,
                        link,
                        &Default::default(),
                        Some(&crate::bytecode::compile::Entry {
                            function: *function,
                            convention: &lash_kernel_doc::StartConvention::Direct,
                            captures: None,
                        }),
                        None,
                        &library,
                    )
                    .map(drop)
                    .map_err(|problem| StartError::Linked {
                        problem: problem.to_string(),
                    }),
                    None => Ok(()),
                };
                compiled.map(|()| {
                    match (definition.body().is_some(), registered.native().is_some()) {
                        (true, true) => DefinitionClass::Both,
                        (true, false) => DefinitionClass::BodyOnly,
                        (false, true) => DefinitionClass::NativeOnly,
                        (false, false) => DefinitionClass::Declared,
                    }
                })
            };
            (*function, definition.name.as_str().to_owned(), class)
        })
        .collect()
}
