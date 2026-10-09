//! Collection primitives and ordinary kernel bodies (`K-COL`, `K-ITER`).
mod bodies;
mod copy;
mod native;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use lash_kernel_doc::{FunctionId, FunctionRegistry, ParseError, RegistryError, parse_definition};

/// A collection registry could not resolve a required definition or admit a body.
#[derive(Debug)]
pub enum CollectionError {
    /// Register the numeric library and the machine functions first.
    Missing(&'static str),
    /// A built-in kernel text definition could not be read.
    Parse(ParseError),
    /// A definition could not be admitted to this registry.
    Registry(RegistryError),
}

impl std::fmt::Display for CollectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(name) => write!(f, "collections require registered native `{name}`"),
            Self::Parse(error) => error.fmt(f),
            Self::Registry(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for CollectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Missing(_) => None,
            Self::Parse(error) => Some(error),
            Self::Registry(error) => Some(error),
        }
    }
}

/// Adds collection primitives and bodies to a registry, returning their names and identities.
///
/// Register `num.add`, `num.sub`, `num.lt`, `eq` and the machine's
/// `tasks.unfinished` first. Mutation and callback functions have bodies only;
/// their calls must occupy a statement, even when the callback does not wait.
/// All bodies are available to a second reader through the function catalog.
pub fn register_collections(
    registry: &mut FunctionRegistry,
) -> Result<BTreeMap<String, FunctionId>, CollectionError> {
    let mut uses = String::new();
    for name in ["num.add", "num.sub", "num.lt", "eq", "tasks.unfinished"] {
        let Some((id, function)) = registry
            .iter()
            .find(|(_, function)| function.definition.name.as_str() == name)
        else {
            return Err(CollectionError::Missing(name));
        };
        if function.native.is_none() {
            return Err(CollectionError::Missing(name));
        }
        uses.push_str(&format!("use {name} = @{id}\n"));
    }
    let mut ids = BTreeMap::new();
    for (definition, native) in native::functions()? {
        let name = definition.name.as_str().to_owned();
        let id = registry
            .register(definition, Some(native))
            .map_err(CollectionError::Registry)?;
        uses.push_str(&format!("use {name} = @{id}\n"));
        ids.insert(name, id);
    }
    for (signature, body, charge, errors) in bodies::functions() {
        let body_uses = uses
            .lines()
            .filter(|line| {
                let name = line.split_whitespace().nth(1).unwrap_or("");
                body.contains(&format!("{name}("))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!(
            "function {signature}\nkernel 1\ncharge {charge}\n{errors}\n{body_uses}\nbody {{ {body} }}\n"
        );
        let definition = parse_definition(&text).map_err(CollectionError::Parse)?;
        let name = definition.name.as_str().to_owned();
        let id = registry
            .register(definition, None)
            .map_err(CollectionError::Registry)?;
        uses.push_str(&format!("use {name} = @{id}\n"));
        ids.insert(name, id);
    }
    Ok(ids)
}

fn raise(kind: &str, message: &str) -> lash_kernel_doc::NativeError {
    lash_kernel_doc::NativeError::Raised(lash_kernel_doc::ErrorValue::new(kind, message))
}
