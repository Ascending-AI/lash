//! The installed packages shared by the conformance laws and loop audit.
#![allow(clippy::unwrap_used, reason = "fixture construction must succeed")]
use lash_ext_helper_compiler::library::{Dialect, Library};
use lash_kernel_doc::{FunctionRegistry, NativeCall, NativeError, NativeFunction, Value};
use lash_kernel_package::HeldPackages;
use std::sync::Arc;

pub(crate) fn installed() -> (HeldPackages, FunctionRegistry) {
    let ts = Library::of(Dialect::TypeScript).unwrap();
    let py = Library::of(Dialect::Python).unwrap();
    let mut packages = ts.packages;
    packages.extend(
        py.packages
            .into_iter()
            .filter(|p| p.path().to_string() == "python"),
    );
    let fixture = lash_kernel_package::testing::dataframe(&packages[0]).unwrap();
    packages.push(fixture);
    let held = HeldPackages::install(packages).unwrap();
    let mut registry = FunctionRegistry::declarations();
    held.register_into(&mut registry, &|id| {
        ts.registry
            .get(id)
            .or_else(|| py.registry.get(id))
            .and_then(|f| f.native().cloned())
    })
    .unwrap();
    for package in held.artifacts().values() {
        match package.path().to_string().as_str() {
            "typescript" => {
                lash_ext_ts_core::implement(&mut registry, package.code().ids().iter().copied())
                    .unwrap()
            }
            "python" => {
                lash_ext_py_core::implement(&mut registry, package.code().ids().iter().copied())
                    .unwrap()
            }
            "dataframe" => {
                registry
                    .implement(
                        package.function("dataframe.width").unwrap(),
                        Arc::new(Width),
                    )
                    .unwrap();
            }
            _ => {}
        }
    }
    (held, registry)
}

struct Width;
impl NativeFunction for Width {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let Some(value @ Value::List(_)) = call.args.get(0) else {
            return Err(NativeError::Decline);
        };
        let Some(sequence) = call.args.sequence(value)? else {
            return Err(NativeError::Decline);
        };
        Ok(Value::Int(i64::try_from(sequence.len()).unwrap()))
    }
}
