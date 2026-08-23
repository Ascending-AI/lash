#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedDataType {
    name: String,
    ty: TypeExpr,
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
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum LashlangHostCatalogError {
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
    #[error(
        "trigger source `{source_type}` already emits `{existing}`, cannot change it to `{incoming}`"
    )]
    ConflictingTriggerSource {
        source_type: String,
        existing: String,
        incoming: String,
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
        TypeExpr::Process { .. } => Err(NamedDataTypeError::UnsupportedType { ty: "process" }),
        TypeExpr::TriggerHandle(_) => Err(NamedDataTypeError::UnsupportedType {
            ty: "trigger handle",
        }),
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerSourceBinding {
    event_type: NamedDataType,
}

impl TriggerSourceBinding {
    fn new(event_type: NamedDataType) -> Self {
        Self { event_type }
    }

    pub fn event_type(&self) -> &NamedDataType {
        &self.event_type
    }

    pub fn event_ty(&self) -> &TypeExpr {
        self.event_type.ty()
    }

    pub fn event_type_name(&self) -> &str {
        self.event_type.name()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LashlangHostEnvironment {
    #[serde(default)]
    pub resources: LashlangHostCatalog,
    /// Names already present in the live execution namespace. The linker uses
    /// this set to distinguish persisted or host-projected bindings from
    /// misspelled top-level names before execution starts.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub globals: BTreeSet<String>,
    #[serde(default)]
    pub abilities: LashlangAbilities,
    #[serde(default)]
    pub language_features: LashlangLanguageFeatures,
}

impl LashlangHostEnvironment {
    pub fn new(resources: LashlangHostCatalog, abilities: LashlangAbilities) -> Self {
        Self {
            resources,
            globals: BTreeSet::new(),
            abilities,
            language_features: LashlangLanguageFeatures::default(),
        }
    }

    pub fn with_globals(mut self, globals: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.globals.extend(globals.into_iter().map(Into::into));
        self
    }

    pub fn with_language_features(mut self, language_features: LashlangLanguageFeatures) -> Self {
        self.language_features = language_features;
        self
    }

    pub fn satisfies(&self, requirements: &HostRequirements) -> bool {
        self.abilities.satisfies(requirements.abilities)
            && requirements.globals.is_subset(&self.globals)
            && self
                .language_features
                .satisfies(requirements.language_features)
            && self.resources.satisfies(&requirements.resources)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LashlangLanguageFeatures {
    pub label_annotations: bool,
}

impl LashlangLanguageFeatures {
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LashlangAbilities {
    pub processes: bool,
    pub sleep: bool,
    pub process_signals: bool,
    pub triggers: bool,
}

impl LashlangAbilities {
    pub fn union(self, other: Self) -> Self {
        Self {
            processes: self.processes || other.processes,
            sleep: self.sleep || other.sleep,
            process_signals: self.process_signals || other.process_signals,
            triggers: self.triggers || other.triggers,
        }
    }

    pub fn satisfies(self, required: Self) -> bool {
        (!required.processes || self.processes)
            && (!required.sleep || self.sleep)
            && (!required.process_signals || self.process_signals)
            && (!required.triggers || self.triggers)
    }

    pub fn with_processes(mut self) -> Self {
        self.processes = true;
        self
    }

    pub fn with_sleep(mut self) -> Self {
        self.sleep = true;
        self
    }

    pub fn with_process_signals(mut self) -> Self {
        self.process_signals = true;
        self
    }

    pub fn with_triggers(mut self) -> Self {
        self.triggers = true;
        self
    }

    pub fn all() -> Self {
        Self::default()
            .with_sleep()
            .with_processes()
            .with_process_signals()
            .with_triggers()
    }
}

fn module_path_key(path: &[impl AsRef<str>]) -> String {
    path.iter()
        .map(|segment| segment.as_ref())
        .collect::<Vec<_>>()
        .join(".")
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkedModule {
    pub module_ref: crate::ModuleRef,
    pub host_requirements_ref: crate::HostRequirementsRef,
    pub artifact: ModuleArtifact,
    #[serde(skip)]
    linked_program: Option<Program>,
}

impl LinkedModule {
    pub fn link(
        program: Program,
        surface: impl Borrow<LashlangHostEnvironment>,
    ) -> Result<Self, LinkError> {
        Self::link_with_dialect(program, surface, crate::CompilationDialect::Lashlang)
    }

    pub fn link_with_dialect(
        program: Program,
        surface: impl Borrow<LashlangHostEnvironment>,
        dialect: crate::CompilationDialect,
    ) -> Result<Self, LinkError> {
        crate::ast::validate_ast(&program)?;
        let surface = surface.borrow();
        let mut linker = Linker::new(&program, surface).with_dialect(dialect);
        let program = linker.link_program()?;
        let program = materialize_default_trigger_keys(program)?;
        let requirements = host_requirements_for_program_with_catalog(&program, &surface.resources);
        let artifact = ModuleArtifact::from_program_with_requirements_and_dialect(
            program.clone(),
            requirements,
            dialect,
        )
        .map_err(|err| LinkError::ModuleHash {
            message: err.to_string(),
        })?;
        Ok(Self {
            module_ref: artifact.module_ref.clone(),
            host_requirements_ref: artifact.host_requirements_ref.clone(),
            artifact,
            linked_program: Some(program),
        })
    }

    pub fn program(&self) -> &Program {
        self.linked_program
            .as_ref()
            .unwrap_or(&self.artifact.canonical_ir)
    }
}
