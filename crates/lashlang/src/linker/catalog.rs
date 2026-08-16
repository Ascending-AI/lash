#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LashlangHostCatalog {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    module_instances: BTreeMap<String, ModuleInstanceCatalog>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    resource_types: BTreeMap<String, ResourceTypeCatalog>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    named_data_types: BTreeMap<String, NamedDataType>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    value_constructors: BTreeMap<String, ValueConstructorBinding>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    trigger_sources: BTreeMap<String, TriggerSourceBinding>,
}

impl LashlangHostCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tool_default(operations: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mut catalog = Self::new();
        for operation in operations {
            let operation = operation.into();
            catalog
                .add_module_operation(
                    ["tools"],
                    "Tools",
                    operation.clone(),
                    operation,
                    TypeExpr::Any,
                    TypeExpr::Any,
                )
                .expect("tool-default operations are idempotent");
        }
        catalog
    }

    pub fn add_module_instance(
        &mut self,
        module_path: impl IntoIterator<Item = impl Into<String>>,
        resource_type: impl Into<String>,
    ) -> Result<(), LashlangHostCatalogError> {
        let path = module_path.into_iter().map(Into::into).collect::<Vec<_>>();
        assert!(!path.is_empty(), "module path must not be empty");
        let resource_type = resource_type.into();
        let key = module_path_key(&path);
        if let Some(existing) = self.module_instances.get(&key) {
            if existing.resource_type != resource_type {
                return Err(LashlangHostCatalogError::ConflictingModuleInstance {
                    alias: key,
                    existing: existing.resource_type.clone(),
                    incoming: resource_type,
                });
            }
            self.ensure_resource_type(resource_type);
            return Ok(());
        }
        self.module_instances.insert(
            key.clone(),
            ModuleInstanceCatalog {
                path,
                resource_type: resource_type.clone(),
                alias: key,
                operations: BTreeMap::new(),
            },
        );
        self.ensure_resource_type(resource_type);
        Ok(())
    }

    pub fn ensure_resource_type(&mut self, resource_type: impl Into<String>) {
        self.resource_types.entry(resource_type.into()).or_default();
    }

    pub fn add_operation(
        &mut self,
        resource_type: impl Into<String>,
        operation: impl Into<String>,
        input_ty: TypeExpr,
        output_ty: TypeExpr,
    ) {
        self.add_operation_binding(
            resource_type,
            operation,
            ResourceOperationBinding {
                input_ty,
                output_ty,
                output_from_input: None,
            },
        );
    }

    pub fn add_operation_binding(
        &mut self,
        resource_type: impl Into<String>,
        operation: impl Into<String>,
        binding: ResourceOperationBinding,
    ) {
        self.resource_types
            .entry(resource_type.into())
            .or_default()
            .operations
            .insert(operation.into(), binding);
    }

    pub fn add_module_operation(
        &mut self,
        module_path: impl IntoIterator<Item = impl Into<String>>,
        resource_type: impl Into<String>,
        operation: impl Into<String>,
        host_operation: impl Into<String>,
        input_ty: TypeExpr,
        output_ty: TypeExpr,
    ) -> Result<(), LashlangHostCatalogError> {
        self.add_module_operation_binding(
            module_path,
            resource_type,
            operation,
            host_operation,
            ResourceOperationBinding {
                input_ty,
                output_ty,
                output_from_input: None,
            },
        )
    }

    pub fn add_module_operation_binding(
        &mut self,
        module_path: impl IntoIterator<Item = impl Into<String>>,
        resource_type: impl Into<String>,
        operation: impl Into<String>,
        host_operation: impl Into<String>,
        binding: ResourceOperationBinding,
    ) -> Result<(), LashlangHostCatalogError> {
        let path = module_path.into_iter().map(Into::into).collect::<Vec<_>>();
        assert!(!path.is_empty(), "module path must not be empty");
        let resource_type = resource_type.into();
        let operation = operation.into();
        let host_operation = host_operation.into();
        self.add_module_instance(path.iter().map(String::as_str), resource_type.clone())?;
        let key = module_path_key(&path);
        if let Some(existing) = self
            .module_instances
            .get(&key)
            .expect("module instance was just inserted")
            .operations
            .get(&operation)
        {
            let same_resource_binding = self
                .resource_types
                .get(&resource_type)
                .and_then(|resource| resource.operations.get(&operation))
                == Some(&binding);
            if existing.host_operation == host_operation && same_resource_binding {
                return Ok(());
            }
            return Err(LashlangHostCatalogError::ConflictingModuleOperation {
                module: key,
                operation,
                existing: existing.host_operation.clone(),
                incoming: host_operation,
            });
        }
        self.add_operation_binding(resource_type, operation.clone(), binding);
        let module = self
            .module_instances
            .get_mut(&key)
            .expect("module instance was just inserted");
        module
            .operations
            .insert(operation, ModuleOperationBinding { host_operation });
        Ok(())
    }

    pub fn add_value_constructor(
        &mut self,
        path: impl IntoIterator<Item = impl Into<String>>,
        input_ty: TypeExpr,
        output_ty: TypeExpr,
    ) {
        let path = path.into_iter().map(Into::into).collect::<Vec<_>>();
        assert!(!path.is_empty(), "constructor path must not be empty");
        let key = module_path_key(&path);
        self.value_constructors.insert(
            key.clone(),
            ValueConstructorBinding {
                path,
                type_name: format_type_expr(&output_ty),
                input_ty,
                output_ty,
            },
        );
    }

    pub fn add_named_data_type(
        &mut self,
        data_type: NamedDataType,
    ) -> Result<(), LashlangHostCatalogError> {
        self.merge_named_data_type(data_type)
    }

    pub fn add_trigger_source_constructor(
        &mut self,
        path: impl IntoIterator<Item = impl Into<String>>,
        input_ty: TypeExpr,
        event_ty: NamedDataType,
    ) -> Result<(), LashlangHostCatalogError> {
        let path = path.into_iter().map(Into::into).collect::<Vec<_>>();
        assert!(!path.is_empty(), "constructor path must not be empty");
        let source_type = module_path_key(&path);
        self.check_named_data_type(&event_ty)?;
        if let Some(existing) = self.trigger_sources.get(source_type.as_str())
            && existing.event_type() != &event_ty
        {
            return Err(LashlangHostCatalogError::ConflictingTriggerSource {
                source_type,
                existing: existing.event_type().name().to_string(),
                incoming: event_ty.name().to_string(),
            });
        }
        self.add_value_constructor(path, input_ty, TypeExpr::Ref(source_type.clone().into()));
        self.add_trigger_source_type(source_type, event_ty)?;
        Ok(())
    }

    pub(crate) fn add_trigger_source_type(
        &mut self,
        source_ty: impl Into<String>,
        event_ty: NamedDataType,
    ) -> Result<(), LashlangHostCatalogError> {
        self.merge_named_data_type(event_ty.clone())?;
        self.trigger_sources
            .insert(source_ty.into(), TriggerSourceBinding::new(event_ty));
        Ok(())
    }

    pub fn extend(&mut self, other: Self) {
        self.try_extend(other)
            .expect("conflicting host catalog entries");
    }

    pub fn try_extend(&mut self, other: Self) -> Result<(), LashlangHostCatalogError> {
        for (resource_type, incoming) in other.resource_types {
            let entry = self.resource_types.entry(resource_type).or_default();
            entry.operations.extend(incoming.operations);
        }
        for (alias, incoming) in other.module_instances {
            match self.module_instances.get_mut(&alias) {
                Some(existing)
                    if existing.path == incoming.path
                        && existing.resource_type == incoming.resource_type
                        && existing.alias == incoming.alias =>
                {
                    existing.operations.extend(incoming.operations);
                }
                Some(existing) => {
                    return Err(LashlangHostCatalogError::ConflictingModuleInstance {
                        alias,
                        existing: existing.resource_type.clone(),
                        incoming: incoming.resource_type,
                    });
                }
                None => {
                    self.module_instances.insert(alias, incoming);
                }
            }
        }
        for data_type in other.named_data_types.into_values() {
            self.merge_named_data_type(data_type)?;
        }
        self.value_constructors.extend(other.value_constructors);
        self.trigger_sources.extend(other.trigger_sources);
        Ok(())
    }

    pub fn union(mut self, other: Self) -> Self {
        self.extend(other);
        self
    }

    pub fn satisfies(&self, required: &Self) -> bool {
        for (path, required_module) in &required.module_instances {
            let Some(module) = self.module_instances.get(path) else {
                return false;
            };
            if module.path != required_module.path
                || module.resource_type != required_module.resource_type
                || module.alias != required_module.alias
            {
                return false;
            }
            for (operation, required_binding) in &required_module.operations {
                if module.operations.get(operation) != Some(required_binding) {
                    return false;
                }
            }
        }
        for (resource_type, required_catalog) in &required.resource_types {
            let Some(catalog) = self.resource_types.get(resource_type) else {
                return false;
            };
            for (operation, required_binding) in &required_catalog.operations {
                if catalog.operations.get(operation) != Some(required_binding) {
                    return false;
                }
            }
        }
        for (path, required_constructor) in &required.value_constructors {
            if self.value_constructors.get(path) != Some(required_constructor) {
                return false;
            }
        }
        for (name, required_data_type) in &required.named_data_types {
            if self.named_data_types.get(name) != Some(required_data_type) {
                return false;
            }
        }
        for (source_type, required_binding) in &required.trigger_sources {
            if self.trigger_sources.get(source_type) != Some(required_binding) {
                return false;
            }
        }
        true
    }

    pub fn has_resource_type(&self, resource_type: &str) -> bool {
        self.resource_types.contains_key(resource_type)
    }

    pub fn has_named_data_type(&self, name: &str) -> bool {
        self.named_data_types.contains_key(name)
    }

    pub fn is_known_opaque_value_type(&self, name: &str) -> bool {
        self.trigger_sources.contains_key(name)
            || self.value_constructors.values().any(|constructor| {
                matches!(&constructor.output_ty, TypeExpr::Ref(type_name) if type_name == name)
            })
    }

    pub fn decode_host_descriptor_as<T: serde::de::DeserializeOwned>(
        &self,
        source_type: &str,
        value: serde_json::Value,
    ) -> Result<T, crate::HostDescriptorError> {
        if !self.is_known_opaque_value_type(source_type) {
            return Err(crate::HostDescriptorError::UnknownSourceType {
                source_type: source_type.to_string(),
            });
        }
        serde_json::from_value(value).map_err(|err| crate::HostDescriptorError::MalformedPayload {
            source_type: source_type.to_string(),
            message: err.to_string(),
        })
    }

    pub fn module_instances(&self) -> impl Iterator<Item = (&str, &ModuleInstanceCatalog)> {
        self.module_instances
            .iter()
            .map(|(path, module)| (path.as_str(), module))
    }

    pub fn resource_types(&self) -> impl Iterator<Item = (&str, &ResourceTypeCatalog)> {
        self.resource_types
            .iter()
            .map(|(resource_type, catalog)| (resource_type.as_str(), catalog))
    }

    pub fn named_data_types(&self) -> impl Iterator<Item = (&str, &NamedDataType)> {
        self.named_data_types
            .iter()
            .map(|(name, data_type)| (name.as_str(), data_type))
    }

    pub fn value_constructors(&self) -> impl Iterator<Item = (&str, &ValueConstructorBinding)> {
        self.value_constructors
            .iter()
            .map(|(path, constructor)| (path.as_str(), constructor))
    }

    pub fn trigger_sources(&self) -> impl Iterator<Item = (&str, &TriggerSourceBinding)> {
        self.trigger_sources
            .iter()
            .map(|(source_type, binding)| (source_type.as_str(), binding))
    }

    pub fn resolve_named_data_type(&self, name: &str) -> Option<&NamedDataType> {
        self.named_data_types.get(name)
    }

    pub fn resolve_trigger_source(&self, source_ty: &str) -> Option<&TriggerSourceBinding> {
        self.trigger_sources.get(source_ty)
    }

    pub fn resolve_module_path(&self, path: &[impl AsRef<str>]) -> Option<ResourceRefExpr> {
        let key = module_path_key(path);
        let module = self.module_instances.get(&key)?;
        Some(ResourceRefExpr::resolved(
            module
                .path
                .iter()
                .map(|segment| segment.as_str().into())
                .collect(),
            module.resource_type.clone(),
            module.alias.clone(),
        ))
    }

    pub fn resolve_alias(&self, resource: &ResourceRefExpr) -> Option<&ResourceTypeCatalog> {
        if !resource.resource_type.is_empty() {
            return self.resource_types.get(resource.resource_type.as_str());
        }
        let resolved = self.resolve_module_path(&resource.path)?;
        self.resource_types.get(resolved.resource_type.as_str())
    }

    pub fn resolve_operation(
        &self,
        resource_type: &str,
        operation: &str,
    ) -> Option<&ResourceOperationBinding> {
        self.resource_types
            .get(resource_type)?
            .operations
            .get(operation)
    }

    pub fn has_operations(&self) -> bool {
        self.resource_types
            .values()
            .any(|resource_type| !resource_type.operations.is_empty())
    }

    pub fn resolve_module_operation(
        &self,
        resource_type: &str,
        alias: &str,
        operation: &str,
    ) -> Option<&ModuleOperationBinding> {
        let module = self.module_instances.get(alias)?;
        (module.resource_type == resource_type).then_some(())?;
        module.operations.get(operation)
    }

    /// Whether this catalog already provides `operation` on the module at the
    /// dotted `module_path` (e.g. `"web"`, `"web.fetch"`), regardless of the
    /// backing resource type. Used by deferred resolution to skip call-paths
    /// the link-time host environment already binds.
    pub fn provides_module_operation(&self, module_path: &str, operation: &str) -> bool {
        self.module_instances
            .get(module_path)
            .is_some_and(|module| module.operations.contains_key(operation))
    }

    pub fn resolve_value_constructor(
        &self,
        path: &[impl AsRef<str>],
    ) -> Option<&ValueConstructorBinding> {
        self.value_constructors.get(&module_path_key(path))
    }

    pub fn trigger_source_event(&self, source_ty: &TypeExpr) -> Option<TypeExpr> {
        let TypeExpr::Ref(name) = source_ty else {
            return None;
        };
        self.trigger_sources
            .get(name.as_str())
            .map(|binding| binding.event_type().to_ref_ty())
    }

    pub fn operation_suggestions_for_host(&self, host_operation: &str) -> Vec<String> {
        let mut suggestions = Vec::new();
        for module in self.module_instances.values() {
            for (operation, binding) in &module.operations {
                if binding.host_operation == host_operation {
                    suggestions.push(format!("{}.{}", module.alias, operation));
                }
            }
        }
        suggestions.sort();
        suggestions.dedup();
        suggestions
    }

    pub fn operation_suggestions_for_prefix(
        &self,
        prefix: &[impl AsRef<str>],
        operation: &str,
    ) -> Vec<String> {
        let prefix = module_path_key(prefix);
        let mut suggestions = Vec::new();
        for module in self.module_instances.values() {
            if module.alias == prefix || !module.alias.starts_with(&format!("{prefix}.")) {
                continue;
            }
            if self
                .resolve_operation(&module.resource_type, operation)
                .is_some()
            {
                suggestions.push(format!("{}.{}", module.alias, operation));
            }
        }
        suggestions.sort();
        suggestions.dedup();
        suggestions
    }

    pub(crate) fn operation_suggestions_for_operation(&self, operation: &str) -> Vec<String> {
        let mut suggestions = self
            .module_instances
            .values()
            .filter(|module| module.operations.contains_key(operation))
            .map(|module| format!("{}.{}", module.alias, operation))
            .collect::<Vec<_>>();
        suggestions.sort();
        suggestions.dedup();
        suggestions
    }

    pub(crate) fn operation_suggestions_for_resource_type(
        &self,
        resource_type: &str,
    ) -> Vec<String> {
        let mut suggestions = self
            .module_instances
            .values()
            .filter(|module| module.resource_type == resource_type)
            .flat_map(|module| {
                module
                    .operations
                    .keys()
                    .map(move |operation| format!("{}.{}", module.alias, operation))
            })
            .collect::<Vec<_>>();
        if suggestions.is_empty()
            && let Some(resource) = self.resource_types.get(resource_type)
        {
            suggestions.extend(resource.operations.keys().cloned());
        }
        suggestions.sort();
        suggestions.dedup();
        suggestions
    }

    fn check_named_data_type(
        &self,
        data_type: &NamedDataType,
    ) -> Result<(), LashlangHostCatalogError> {
        if let Some(existing) = self.named_data_types.get(data_type.name())
            && existing != data_type
        {
            return Err(LashlangHostCatalogError::ConflictingNamedDataType {
                name: data_type.name().to_string(),
            });
        }
        Ok(())
    }

    fn merge_named_data_type(
        &mut self,
        data_type: NamedDataType,
    ) -> Result<(), LashlangHostCatalogError> {
        self.check_named_data_type(&data_type)?;
        self.named_data_types
            .entry(data_type.name().to_string())
            .or_insert(data_type);
        Ok(())
    }
}
