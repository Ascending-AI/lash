use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedDataType {
    pub(super) name: String,
    pub(super) ty: TypeExpr,
}

impl NamedDataType {
    pub fn new(name: impl Into<String>, ty: TypeExpr) -> Result<Self, NamedDataTypeError> {
        let name = name.into();
        if !is_qualified_type_name(&name) {
            return Err(NamedDataTypeError::InvalidName { name });
        }
        if !matches!(ty, TypeExpr::Object(_)) {
            return Err(NamedDataTypeError::ExpectedObject { name });
        }
        validate_named_data_shape(&ty)?;
        Ok(Self { name, ty })
    }

    pub fn object(
        name: impl Into<String>,
        fields: Vec<TypeField>,
    ) -> Result<Self, NamedDataTypeError> {
        Self::new(name, TypeExpr::Object(fields))
    }

    pub fn from_schema(
        name: impl Into<String>,
        schema: &serde_json::Value,
    ) -> Result<Self, NamedDataTypeError> {
        let name = name.into();
        let ty = crate::json_schema_to_type_expr(schema).map_err(|source| {
            NamedDataTypeError::UnreadableSchema {
                name: name.clone(),
                reason: source.to_string(),
            }
        })?;
        Self::new(name, ty)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn ty(&self) -> &TypeExpr {
        &self.ty
    }

    pub fn to_ref_ty(&self) -> TypeExpr {
        TypeExpr::Ref(self.name.clone().into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum NamedDataTypeError {
    #[error("host data type name `{name}` must be qualified")]
    InvalidName { name: String },
    #[error("host data type `{name}` must be an object type")]
    ExpectedObject { name: String },
    #[error("host data type object has duplicate field `{field}`")]
    DuplicateField { field: String },
    #[error("host data type enum has duplicate value `{value}`")]
    DuplicateEnumValue { value: String },
    #[error("host data type shape cannot contain nested type ref `{name}`")]
    NestedRef { name: String },
    #[error("host data type shape cannot contain {ty}")]
    UnsupportedType { ty: &'static str },
    #[error("host data type `{name}` declares a schema lash cannot read: {reason}")]
    UnreadableSchema { name: String, reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum LashVmHostCatalogError {
    #[error("conflicting host data type definition `{name}`")]
    ConflictingNamedDataType { name: String },
    #[error(
        "module `{alias}` already has resource type `{existing}`, cannot change it to `{incoming}`"
    )]
    ConflictingModuleInstance {
        alias: String,
        existing: String,
        incoming: String,
    },
    #[error(
        "module `{module}` operation `{operation}` already dispatches to `{existing}`, cannot change it to `{incoming}`"
    )]
    ConflictingModuleOperation {
        module: String,
        operation: String,
        existing: String,
        incoming: String,
    },
    #[error("resource type `{resource_type}` is already registered")]
    ConflictingResourceType { resource_type: String },
    #[error("resource type `{resource_type}` operation `{operation}` is already registered")]
    ConflictingResourceOperation {
        resource_type: String,
        operation: String,
    },
    #[error("value constructor `{path}` is already registered")]
    ConflictingValueConstructor { path: String },
    #[error(
        "module `{module}` cannot use resource type `{resource_type}` operation `{operation}` without a host-operation binding"
    )]
    UnboundModuleOperation {
        module: String,
        resource_type: String,
        operation: String,
    },
    #[error("host operation `{operation}` declares a schema lash cannot read: {source}")]
    UnreadableOperationSchema {
        operation: String,
        #[source]
        source: crate::json_schema::JsonSchemaError,
    },
}

fn is_qualified_type_name(name: &str) -> bool {
    let mut segments = name.split('.');
    let mut count = 0usize;
    for segment in segments.by_ref() {
        count += 1;
        let mut chars = segment.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !(first.is_ascii_alphabetic() || first == '_') {
            return false;
        }
        if !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
            return false;
        }
    }
    count >= 2
}

fn validate_named_data_shape(ty: &TypeExpr) -> Result<(), NamedDataTypeError> {
    match ty {
        TypeExpr::Any
        | TypeExpr::Str
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Bool
        | TypeExpr::Dict
        | TypeExpr::Null => Ok(()),
        TypeExpr::Enum(values) => {
            let mut seen = BTreeSet::new();
            for value in values {
                if !seen.insert(value.to_string()) {
                    return Err(NamedDataTypeError::DuplicateEnumValue {
                        value: value.to_string(),
                    });
                }
            }
            Ok(())
        }
        TypeExpr::List(item) => validate_named_data_shape(item),
        TypeExpr::Object(fields) => {
            let mut seen = BTreeSet::new();
            for field in fields {
                if !seen.insert(field.name.to_string()) {
                    return Err(NamedDataTypeError::DuplicateField {
                        field: field.name.to_string(),
                    });
                }
                validate_named_data_shape(&field.ty)?;
            }
            Ok(())
        }
        TypeExpr::Union(items) => {
            for item in items {
                validate_named_data_shape(item)?;
            }
            Ok(())
        }
        TypeExpr::Ref(name) => Err(NamedDataTypeError::NestedRef {
            name: name.to_string(),
        }),
        TypeExpr::Process(_) => Err(NamedDataTypeError::UnsupportedType { ty: "process" }),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTypeCatalog {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub operations: BTreeMap<String, ResourceOperationBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleInstanceCatalog {
    pub path: Vec<String>,
    pub resource_type: String,
    pub alias: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub operations: BTreeMap<String, ModuleOperationBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceOperationBinding {
    pub input_ty: TypeExpr,
    pub output_ty: TypeExpr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_from_input: Option<OutputFromInputBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputFromInputBinding {
    pub input_field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_schema: Option<TypeExpr>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleOperationBinding {
    pub host_operation: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedOperation<'a> {
    pub host_operation: &'a str,
    pub binding: &'a ResourceOperationBinding,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValueConstructorBinding {
    pub path: Vec<String>,
    pub type_name: String,
    pub input_ty: TypeExpr,
    pub output_ty: TypeExpr,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LashVmHostEnvironment {
    #[serde(default)]
    pub resources: LashVmHostCatalog,
    /// Names already present in the live execution namespace. The linker uses
    /// this set to distinguish persisted or host-projected bindings from
    /// misspelled top-level names before execution starts.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub globals: BTreeSet<String>,
    /// Names in `globals` whose live values are process handles. Dialects use
    /// this semantic fact when classifying an ambient binding; it is separate
    /// from membership because an ordinary restored value is not awaitable.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub process_handles: BTreeSet<String>,
    /// Session globals a cell boundary dropped for holding a function
    /// (`State::expired_functions`). None of them is in `globals`; a dialect
    /// refuses a reference to one by name rather than as an unknown name.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub expired_functions: BTreeSet<String>,
    #[serde(default)]
    pub language_features: LashVmLanguageFeatures,
}

impl LashVmHostEnvironment {
    pub fn new(resources: LashVmHostCatalog) -> Self {
        Self {
            resources,
            globals: BTreeSet::new(),
            process_handles: BTreeSet::new(),
            expired_functions: BTreeSet::new(),
            language_features: LashVmLanguageFeatures::default(),
        }
    }

    pub fn with_globals(mut self, globals: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.globals.extend(globals.into_iter().map(Into::into));
        self
    }

    pub fn with_expired_functions(
        mut self,
        expired_functions: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.expired_functions
            .extend(expired_functions.into_iter().map(Into::into));
        self
    }

    pub fn with_process_handles(
        mut self,
        process_handles: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.process_handles
            .extend(process_handles.into_iter().map(Into::into));
        self
    }

    pub fn with_language_features(mut self, language_features: LashVmLanguageFeatures) -> Self {
        self.language_features = language_features;
        self
    }

    pub fn satisfies(&self, requirements: &HostRequirements) -> bool {
        requirements.globals.is_subset(&self.globals)
            && self
                .language_features
                .satisfies(requirements.language_features)
            && self.resources.satisfies(&requirements.resources)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LashVmLanguageFeatures {
    pub label_annotations: bool,
}

impl LashVmLanguageFeatures {
    pub fn union(self, other: Self) -> Self {
        Self {
            label_annotations: self.label_annotations || other.label_annotations,
        }
    }

    pub fn satisfies(self, required: Self) -> bool {
        !required.label_annotations || self.label_annotations
    }

    pub fn with_label_annotations(mut self) -> Self {
        self.label_annotations = true;
        self
    }
}

pub(super) fn module_path_key(path: &[impl AsRef<str>]) -> String {
    path.iter()
        .map(|segment| segment.as_ref())
        .collect::<Vec<_>>()
        .join(".")
}

/// A linked module: the admitted [`ModuleArtifact`] and the authored source
/// spans its diagnostics point at.
///
/// The artifact's program is the one executable carrier. The spans are a
/// non-durable side table keyed by that program's AST paths: they are neither
/// identity nor persisted, so a linked module has no serialized form.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkedModule {
    pub artifact: ModuleArtifact,
    spans: BTreeMap<AstPath, Span>,
}

impl LinkedModule {
    pub fn link(
        program: Program,
        surface: impl Borrow<LashVmHostEnvironment>,
    ) -> Result<Self, LinkError> {
        crate::ast::validate_ast(&program)?;
        // The linker derives every lifted declaration; a program handed to it
        // declares its processes and cannot claim one was lifted.
        if let Some(process) = program
            .declarations
            .iter()
            .find_map(|declaration| match declaration {
                crate::Declaration::Process(process) if process.origin.is_lifted() => Some(process),
                _ => None,
            })
        {
            return Err(LinkError::InvalidAst {
                source: crate::InvalidAst::InvalidProcessOrigin {
                    process: process.name.to_string(),
                    reason: "a linked program's lifted processes are derived by the linker",
                },
            });
        }
        let surface = surface.borrow();
        let mut linker = Linker::new(&program, surface);
        let mut program = linker.link_program()?;
        let spans = std::mem::take(&mut program.spans);
        let requirements = host_requirements_for_program_with_catalog(&program, &surface.resources);
        let artifact =
            ModuleArtifact::from_ir_and_requirements(program, requirements).map_err(|err| {
                LinkError::ModuleHash {
                    message: err.to_string(),
                }
            })?;
        Ok(Self { artifact, spans })
    }

    /// The authored source spans of the artifact's program, by AST path.
    pub fn spans(&self) -> &BTreeMap<AstPath, Span> {
        &self.spans
    }
}
