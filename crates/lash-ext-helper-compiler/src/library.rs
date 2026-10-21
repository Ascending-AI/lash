//! The libraries the compiler reads: each dialect's helpers as a worker
//! registers them, over the kernel library and the extensions they call.

use lash_kernel_dialect::NamedLibrary;
use lash_kernel_doc::{FunctionDefinition, FunctionRegistry};
use lash_vocabulary::FunctionId;

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
    pub names: NamedLibrary,
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
        let mut registry = FunctionRegistry::declarations();
        lash_kernel_vm::register_machine_functions(&mut registry).map_err(|e| e.to_string())?;
        lash_kernel_lib::register_numbers(&mut registry).map_err(|e| e.to_string())?;
        lash_kernel_lib::register_collections(&mut registry).map_err(|e| e.to_string())?;
        lash_kernel_lib::register_text_json(&mut registry).map_err(|e| e.to_string())?;
        lash_kernel_lib::register_brands(&mut registry).map_err(|e| e.to_string())?;
        if dialect == Dialect::TypeScript {
            lash_ext_regex_ecma::register(&mut registry).map_err(|e| e.to_string())?;
            lash_ext_date_ecma::register(&mut registry).map_err(|e| e.to_string())?;
            lash_ext_url_whatwg::register(&mut registry).map_err(|e| e.to_string())?;
        }
        let mut names = NamedLibrary::from_registry(&registry).map_err(|e| e.to_string())?;
        let definitions = match dialect {
            Dialect::Python => lash_dialect_python::define_helpers(&mut names),
            Dialect::TypeScript => lash_dialect_typescript::define_helpers(&mut names),
        }
        .map_err(|e| e.to_string())?;
        let helpers = definitions
            .iter()
            .map(FunctionDefinition::identity)
            .collect();
        for definition in definitions {
            registry
                .register(definition, None)
                .map_err(|e| e.to_string())?;
        }
        Ok(Self {
            names,
            registry,
            helpers,
        })
    }

    /// The package dependencies, before the dialect definitions:
    /// independent helper-source laws extend this view.
    ///
    /// # Errors
    ///
    /// A registry refusal when the admitted dependencies cannot be registered.
    pub fn base_registry(&self) -> Result<FunctionRegistry, String> {
        let helpers: std::collections::BTreeSet<_> = self.helpers.iter().copied().collect();
        let mut visited = std::collections::BTreeSet::new();
        let mut definitions = vec![];
        for (id, _) in self.registry.iter() {
            if !helpers.contains(id) {
                ordered(*id, &self.registry, &mut visited, &mut definitions);
            }
        }
        let mut base = FunctionRegistry::declarations();
        for definition in definitions {
            let native = self
                .registry
                .get(&definition.identity())
                .and_then(|function| function.native().cloned());
            base.register(definition, native)
                .map_err(|e| e.to_string())?;
        }
        Ok(base)
    }

    pub fn definition(&self, function: &FunctionId) -> Option<&FunctionDefinition> {
        self.registry
            .get(function)
            .map(|registered| registered.definition().as_ref())
    }
}

fn ordered(
    id: FunctionId,
    registry: &FunctionRegistry,
    visited: &mut std::collections::BTreeSet<FunctionId>,
    definitions: &mut Vec<FunctionDefinition>,
) {
    if !visited.insert(id) {
        return;
    }
    let Some(function) = registry.get(&id) else {
        return;
    };
    if let Some(body) = function.definition().body() {
        for callee in body.functions.keys() {
            ordered(*callee, registry, visited, definitions);
        }
    }
    definitions.push(FunctionDefinition::clone(function.definition()));
}
