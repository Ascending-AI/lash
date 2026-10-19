//! A package value's brand (FIG-6917): `brand.of`, the body every reader
//! of a brand calls. A brand selects a package type's members; it is not a
//! capability, so a member still checks its arguments.

use lash_kernel_doc::{
    FunctionId, FunctionRegistry, NativeCall, NativeError, NativeFunction, Value, parse_definition,
};
use std::sync::Arc;

use crate::CollectionError;

/// `(package path, type name)` of a package value, or absent for any value
/// that holds no well-formed brand.
const BRAND_OF: &str = r#"function brand.of(value: Any) -> Union(Tuple(Text, Text), Absent)
kernel 1
charge 8
native
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
        .register(definition, Some(Arc::new(BrandOf)))
        .map_err(CollectionError::Registry)
}

/// Installs the optional native beside the held `brand.of` definition.
///
/// # Errors
///
/// The registry refusal when `brand.of` was not declared.
pub fn implement_brands(registry: &mut FunctionRegistry) -> Result<(), CollectionError> {
    registry
        .implement_named("brand.of", Arc::new(BrandOf))
        .map(|_| ())
        .map_err(CollectionError::Registry)
}

pub(super) fn native(name: &str) -> Option<Arc<dyn NativeFunction>> {
    (name == "brand.of").then(|| Arc::new(BrandOf) as Arc<dyn NativeFunction>)
}

struct BrandOf;
impl NativeFunction for BrandOf {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let Some(value @ Value::Record(_)) = call.args.get(0) else {
            return Ok(Value::Absent);
        };
        let Some(fields) = call.args.table(value)? else {
            return Ok(Value::Absent);
        };
        let key = Value::Text(
            lash_kernel_doc::arena::ShortText::new("__lash_brand")
                .unwrap_or_else(|| unreachable!("the brand field is inline")),
        );
        let Some(brand @ Value::Tuple(_)) = call.args.lookup(fields, &key)? else {
            return Ok(Value::Absent);
        };
        let Some(parts) = call.args.sequence(brand)? else {
            return Ok(Value::Absent);
        };
        if parts.len() == 2
            && matches!(parts.get(0)?, Some(Value::Text(_) | Value::LongText(_)))
            && matches!(parts.get(1)?, Some(Value::Text(_) | Value::LongText(_)))
        {
            Ok(*brand)
        } else {
            Ok(Value::Absent)
        }
    }
}
