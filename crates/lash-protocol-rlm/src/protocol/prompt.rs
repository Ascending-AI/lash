#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RlmPromptFeatures {
    pub images: bool,
    pub type_literals: bool,
    pub decomposition: bool,
}

impl Default for RlmPromptFeatures {
    fn default() -> Self {
        Self {
            images: true,
            type_literals: true,
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
/// `__typescript_runtime` — how the TypeScript lowerer reaches journaled
/// `Date.now()`/`Math.random()` — into *every* Lashlang host, and this section
/// advertised it: a Lashlang reader was handed
/// `await __typescript_runtime.now(any)? -> float`, an internal name in another
/// dialect's vocabulary.
///
/// Hiding rather than renaming is deliberate. The module path is a durable
/// link-time identifier: it is embedded in every lowered TypeScript program,
/// including the persisted bodies of durable processes that must still resolve
/// when a worker wakes them after a restart. ADR 0063 records the rule and the
/// carve-out list.
fn module_is_runtime_internal(path: &[String]) -> bool {
    path.first()
        .is_some_and(|segment| segment.starts_with("__"))
}

/// The host surface a dialect has to describe, walked once.
///
/// Both dialects advertise the same inventory and spell it differently, so the
/// walk lives here and each dialect formats the rows. A TypeScript session used
/// to receive no inventory at all: its execution section rendered tool
/// signatures and nothing else, so the trigger sources, their event types and
/// the `triggers.*` operations were invisible — while the host prompt told the
/// model to use them. A judged row watched a model search for `cron.Schedule`,
/// find nothing, and conclude the trigger APIs did not exist.
pub(crate) struct HostSurfaceInventory<'a> {
    pub(crate) operations: Vec<HostSurfaceOperation<'a>>,
    pub(crate) data_types: Vec<(String, &'a lashlang::TypeExpr)>,
    pub(crate) constructors: Vec<HostSurfaceConstructor<'a>>,
    /// `(trigger source type, event type name)`.
    pub(crate) trigger_sources: Vec<(String, String)>,
}

pub(crate) struct HostSurfaceOperation<'a> {
    pub(crate) alias: String,
    pub(crate) operation: String,
    pub(crate) input: &'a lashlang::TypeExpr,
    pub(crate) output: &'a lashlang::TypeExpr,
}

pub(crate) struct HostSurfaceConstructor<'a> {
    pub(crate) path: String,
    pub(crate) input: &'a lashlang::TypeExpr,
    /// Already resolved to a nominal name (`TriggerSource<cron.Tick>`), which
    /// is spelled the same in both dialects.
    pub(crate) output: String,
}

pub(crate) fn host_surface_inventory(
    surface: &lashlang::LashlangHostEnvironment,
) -> HostSurfaceInventory<'_> {
    // Operations with real Lashlang types (trigger and other host primitives)
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
                    input: &binding.input_ty,
                    output: &binding.output_ty,
                });
            }
        }
    }
    let data_types = surface
        .resources
        .named_data_types()
        .map(|(_, data_type)| (data_type.name().to_string(), data_type.ty()))
        .collect();
    let constructors = surface
        .resources
        .value_constructors()
        .map(|(_, constructor)| {
            let output = match &constructor.output_ty {
                lashlang::TypeExpr::Ref(name) => surface
                    .resources
                    .resolve_trigger_source(name.as_str())
                    .map(|binding| format!("TriggerSource<{}>", binding.event_type_name()))
                    .unwrap_or_else(|| lashlang::format_type_expr(&constructor.output_ty)),
                other => lashlang::format_type_expr(other),
            };
            HostSurfaceConstructor {
                path: constructor.path.join("."),
                input: &constructor.input_ty,
                output,
            }
        })
        .collect();
    let trigger_sources = surface
        .resources
        .trigger_sources()
        .map(|(source_ty, binding)| (source_ty.to_string(), binding.event_type_name().to_string()))
        .collect();
    HostSurfaceInventory {
        operations,
        data_types,
        constructors,
        trigger_sources,
    }
}

/// Operation-owned lifecycle and trigger guidance shared by both prompt dialects.
pub(crate) fn host_operation_description(module: &str, operation: &str) -> Option<&'static str> {
    match (module, operation) {
        ("processes", "list") => Some(
            "List visible process runs. Empty arguments select running runs; `definition` selects a definition and `status: \"any\"` includes visible run history.",
        ),
        ("triggers", "register") => Some(
            "Register a source value and process definition with every parameter supplied exactly once in inputs. `subscription_key` is stable within the caller's owner scope; supply it or omit it to have a stable key derived from the source and target. A different definition at an existing key conflicts. The source-owning host/plugin emits occurrences; constructors build source values.",
        ),
        ("triggers", "list") => Some(
            "List visible registrations; filter by target, name, source_type or enabled. Each row carries registrant provenance. Registrations remain until an explicit mutation or owner-lifecycle cleanup removes them.",
        ),
        ("triggers", "prune") => Some(
            "Remove selected subscriptions by subscription_keys. Prune is restricted to the acting owner namespace.",
        ),
        ("triggers", "disable") => Some(
            "Pause future deliveries. Supply subscription_key and expected_revision from the current receipt; mutations are revision-checked.",
        ),
        ("triggers", "enable") => Some(
            "Resume future deliveries. Supply subscription_key and expected_revision from the current receipt; mutations are revision-checked.",
        ),
        ("triggers", "delete") => Some(
            "Tombstone the subscription. Supply subscription_key and expected_revision from the current receipt; mutations are revision-checked.",
        ),
        _ => None,
    }
}
