#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RlmPromptFeatures {
    pub images: bool,
    pub decomposition: bool,
}

impl Default for RlmPromptFeatures {
    fn default() -> Self {
        Self {
            images: true,
            decomposition: true,
        }
    }
}

/// Modules the substrate binds for its own lowering, never for a reader.
///
/// The leading double underscore is the repository's reserved-namespace marker
/// (the TypeScript lowerer's generated bindings share it), so this is a rule
/// about a namespace rather than a list of names to keep in sync. It exists
/// because `lashlang_host_environment_from_tool_catalog` binds
/// `__lashlang_runtime` — how a front end reaches the journaled clock and
/// random source (TypeScript's `Date.now()`/`Math.random()`) — into *every*
/// host, and this section once advertised it: a reader was handed
/// `await __lashlang_runtime.now(any)? -> float`, an internal name no cell
/// writes.
///
/// The module is the substrate's, not a reader's, so it is hidden rather than
/// documented. ADR 0063 records the rule.
fn module_is_runtime_internal(path: &[String]) -> bool {
    path.first()
        .is_some_and(|segment| segment.starts_with("__"))
}

pub(crate) struct HostSurfaceInventory {
    pub(crate) operations: Vec<HostSurfaceOperation>,
    pub(crate) data_types: Vec<(String, lash_sansio::SchemaShape)>,
    pub(crate) constructors: Vec<HostSurfaceConstructor>,
}

pub(crate) struct HostSurfaceOperation {
    pub(crate) alias: String,
    pub(crate) operation: String,
    pub(crate) input: lash_sansio::SchemaShape,
    pub(crate) output: lash_sansio::SchemaShape,
}

pub(crate) struct HostSurfaceConstructor {
    pub(crate) path: String,
    pub(crate) input: lash_sansio::SchemaShape,
    pub(crate) output: HostSurfaceConstructorOutput,
}

pub(crate) enum HostSurfaceConstructorOutput {
    Shape(Box<lash_sansio::SchemaShape>),
}

pub(crate) fn host_surface_inventory(
    surface: &lashlang::LashlangHostEnvironment,
) -> HostSurfaceInventory {
    // Operations with real Lashlang types (host primitives)
    // are listed here. Tool-catalog operations are bridged with placeholder
    // `any` types and documented in full under **Tools**, so they are skipped to
    // avoid an uninformative `any -> any` duplicate of that section.
    let mut operations = Vec::new();
    for (_, module) in surface.resources.module_instances() {
        if module_is_runtime_internal(&module.path) {
            continue;
        }
        if let Some(resource_type) =
            surface
                .resources
                .resolve_alias(&lashlang::ResourceRefExpr::resolved(
                    module
                        .path
                        .iter()
                        .map(|segment| segment.as_str().into())
                        .collect(),
                    module.resource_type.clone(),
                    module.alias.clone(),
                ))
        {
            for (operation, binding) in &resource_type.operations {
                if matches!(binding.input_ty, lashlang::TypeExpr::Any)
                    && matches!(binding.output_ty, lashlang::TypeExpr::Any)
                {
                    continue;
                }
                operations.push(HostSurfaceOperation {
                    alias: module.alias.clone(),
                    operation: operation.clone(),
                    input: lashlang::type_expr_to_schema_shape(&binding.input_ty),
                    output: lashlang::type_expr_to_schema_shape(&binding.output_ty),
                });
            }
        }
    }
    let data_types = surface
        .resources
        .named_data_types()
        .filter(|(name, _)| !name.starts_with("__"))
        .map(|(_, data_type)| {
            (
                data_type.name().to_string(),
                lashlang::type_expr_to_schema_shape(data_type.ty()),
            )
        })
        .collect();
    let constructors = surface
        .resources
        .value_constructors()
        .filter(|(_, constructor)| !module_is_runtime_internal(&constructor.path))
        .map(|(_, constructor)| HostSurfaceConstructor {
            path: constructor.path.join("."),
            input: lashlang::type_expr_to_schema_shape(&constructor.input_ty),
            output: HostSurfaceConstructorOutput::Shape(Box::new(
                lashlang::type_expr_to_schema_shape(&constructor.output_ty),
            )),
        })
        .collect();
    HostSurfaceInventory {
        operations,
        data_types,
        constructors,
    }
}

/// Operation-owned lifecycle guidance shared by both prompt dialects.
pub(crate) fn host_operation_description(module: &str, operation: &str) -> Option<&'static str> {
    match (module, operation) {
        ("processes", "list") => Some(
            "List visible process runs. Empty arguments select running runs; `definition` selects a definition and `status: \"any\"` includes visible run history.",
        ),
        _ => None,
    }
}
