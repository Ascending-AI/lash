//! A package value's brand (FIG-6917): `brand.of`, the body every reader
//! of a brand calls. A brand selects a package type's members; it is not a
//! capability, so a member still checks its arguments.

use lash_kernel_doc::{FunctionId, FunctionRegistry, parse_definition};

use crate::CollectionError;

/// `(package path, type name)` of a package value, or absent for any value
/// that holds no well-formed brand.
const BRAND_OF: &str = r#"function brand.of(value: Any) -> Union(Tuple(Text, Text), Absent)
kernel 1
charge 8
body {
  if same(kind(value), "record") {
    if record.contains(value, "__lash_brand") {
      let brand = value.__lash_brand
      if same(kind(brand), "tuple") {
        if eq(tuple.len(brand), 2) {
          if same(kind(brand[0]), "text") {
            if same(kind(brand[1]), "text") { return brand }
          }
        }
      }
    }
  }
  return absent
}
"#;

/// Registers `brand.of` over the kernel's natives, which must be
/// registered first.
///
/// # Errors
///
/// [`CollectionError::Missing`] when a native it calls is not registered,
/// and the parse and registry refusals.
pub fn register_brands(registry: &mut FunctionRegistry) -> Result<FunctionId, CollectionError> {
    let mut text = BRAND_OF.to_owned();
    for name in ["same", "kind", "record.contains", "eq", "tuple.len"] {
        let function = registry
            .named(name)
            .next()
            .ok_or(CollectionError::Missing(name))?;
        text.push_str(&format!("use {name} = @{function}\n"));
    }
    let definition = parse_definition(&text).map_err(CollectionError::Parse)?;
    registry
        .register(definition, None)
        .map_err(CollectionError::Registry)
}
