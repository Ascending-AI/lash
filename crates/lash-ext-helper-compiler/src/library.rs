//! The libraries the compiler reads: each dialect's helpers as a worker
//! registers them, over the kernel library and the extensions they call.

use std::sync::Arc;

use lash_kernel_dialect::NamedLibrary;
use lash_kernel_doc::{FunctionDefinition, FunctionId, FunctionRegistry};

/// A dialect whose helpers the compiler reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Python,
    TypeScript,
}

impl Dialect {
    pub const ALL: [Self; 2] = [Self::Python, Self::TypeScript];

    pub fn name(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::TypeScript => "typescript",
        }
    }
}

/// A registry holding a dialect's helpers, and the helpers in the order
/// they were registered: each after every function its body calls.
pub struct Library {
    pub registry: FunctionRegistry,
    pub helpers: Vec<FunctionId>,
}

impl Library {
    /// The admitted canonical definitions of `dialect`'s helpers, as a
    /// worker registers them.
    ///
    /// # Errors
    ///
    /// Any refusal of registration or of the helper sources, as text.
    pub fn of(dialect: Dialect) -> Result<Self, String> {
        let mut registry = FunctionRegistry::new();
        lash_kernel_vm::register_machine_functions(&mut registry).map_err(|e| e.to_string())?;
        lash_kernel_lib::register_numbers(&mut registry).map_err(|e| e.to_string())?;
        lash_kernel_lib::register_collections(&mut registry).map_err(|e| e.to_string())?;
        lash_kernel_lib::register_text_json(&mut registry).map_err(|e| e.to_string())?;
        if dialect == Dialect::TypeScript {
            lash_ext_date_ecma::register(&mut registry).map_err(|e| e.to_string())?;
            lash_ext_url_whatwg::register(&mut registry).map_err(|e| e.to_string())?;
            lash_ext_regex_ecma::register(
                &mut registry,
                &Arc::new(lash_ext_regex_ecma::Engine::new(1)),
            )
            .map_err(|e| e.to_string())?;
        }
        let mut library = NamedLibrary::from_registry(&registry).map_err(|e| e.to_string())?;
        let definitions: Vec<FunctionDefinition> = match dialect {
            Dialect::Python => lash_dialect_python::define_helpers(&mut library),
            Dialect::TypeScript => lash_dialect_typescript::define_helpers(&mut library),
        }
        .map_err(|e| e.to_string())?;
        let mut helpers = Vec::with_capacity(definitions.len());
        for definition in definitions {
            helpers.push(
                registry
                    .register(definition, None)
                    .map_err(|e| e.to_string())?,
            );
        }
        Ok(Self { registry, helpers })
    }

    pub fn definition(&self, function: &FunctionId) -> Option<&FunctionDefinition> {
        self.registry
            .get(function)
            .map(|registered| registered.definition().as_ref())
    }
}
