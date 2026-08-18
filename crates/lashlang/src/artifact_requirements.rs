//! Host-requirement collection for a linked module.
//!
//! Split out of `artifact.rs` so the hashing rules and the requirement walk
//! each stay readable on their own; both are driven from `ModuleArtifact`.

use super::*;

#[derive(Clone, Debug)]
enum RequirementBinding {
    Value,
    Resource {
        resource_type: String,
        path: Option<Vec<String>>,
    },
}

pub(super) struct RequirementsCollector<'program> {
    program: &'program Program,
    resource_catalog: Option<&'program LashlangHostCatalog>,
    type_names: BTreeSet<String>,
    requirements: HostRequirements,
}

impl<'program> RequirementsCollector<'program> {
    pub(super) fn new(program: &'program Program) -> Self {
        let type_names = program
            .declarations
            .iter()
            .filter_map(|declaration| match declaration {
                Declaration::Type(type_decl) => Some(type_decl.name.to_string()),
                Declaration::Process(_) | Declaration::Function(_) => None,
            })
            .collect();
        Self {
            program,
            resource_catalog: None,
            type_names,
            requirements: HostRequirements::default(),
        }
    }

    pub(super) fn with_resource_catalog(mut self, catalog: &'program LashlangHostCatalog) -> Self {
        self.resource_catalog = Some(catalog);
        self
    }

    pub(super) fn collect(mut self) -> HostRequirements {
        for declaration in &self.program.declarations {
            match declaration {
                Declaration::Type(type_decl) => self.collect_type(&type_decl.ty),
                // A function needs no host ability, so it contributes only the
                // named data types its signature mentions. Its body cannot
                // reach a global either — the linker binds nothing but the
                // parameters — so there is nothing to collect from it beyond
                // the type references its expressions carry.
                Declaration::Function(function) => {
                    for param in &function.params {
                        self.collect_type(&param.ty);
                    }
                    self.collect_type(&function.return_ty);
                    let mut scope = function
                        .params
                        .iter()
                        .map(|param| (param.name.to_string(), RequirementBinding::Value))
                        .collect();
                    self.collect_expr(&function.body, &mut scope);
                }
                Declaration::Process(process) => {
                    self.requirements.abilities.processes = true;
                    if process.label.is_some() {
                        self.requirements.language_features.label_annotations = true;
                    }
                    let mut scope = BTreeMap::new();
                    for param in &process.params {
                        self.collect_type(&param.ty);
                        if let TypeExpr::Ref(name) = &param.ty
                            && !self.type_names.contains(name.as_str())
                            && self.is_resource_type_name(name)
                        {
                            self.requirements
                                .resources
                                .ensure_resource_type(name.to_string());
                            scope.insert(
                                param.name.to_string(),
                                RequirementBinding::Resource {
                                    resource_type: name.to_string(),
                                    path: None,
                                },
                            );
                        } else {
                            scope.insert(param.name.to_string(), RequirementBinding::Value);
                        }
                    }
                    if let Some(return_ty) = &process.return_ty {
                        self.collect_type(return_ty);
                    }
                    scope.insert("input".to_string(), RequirementBinding::Value);
                    scope.insert("inputs".to_string(), RequirementBinding::Value);
                    self.collect_expr(&process.body, &mut scope);
                }
            }
        }
        let mut top_level = BTreeMap::new();
        self.collect_expr(&self.program.main, &mut top_level);
        self.requirements
    }

    fn collect_type(&mut self, ty: &TypeExpr) {
        match ty {
            TypeExpr::List(item) => self.collect_type(item),
            TypeExpr::Object(fields) => {
                for field in fields {
                    self.collect_type(&field.ty);
                }
            }
            TypeExpr::Union(items) => {
                for item in items {
                    self.collect_type(item);
                }
            }
            TypeExpr::Process { input, output, .. } => {
                self.collect_type(input);
                self.collect_type(output);
            }
            TypeExpr::TriggerHandle(event) => self.collect_type(event),
            TypeExpr::Ref(name)
                if !self.type_names.contains(name.as_str())
                    && self.is_host_data_type_name(name) =>
            {
                let data_type = self
                    .resource_catalog
                    .and_then(|catalog| catalog.resolve_named_data_type(name.as_str()))
                    .expect("checked host data type presence")
                    .clone();
                self.requirements
                    .resources
                    .add_named_data_type(data_type)
                    .expect("host data type requirement came from host catalog");
            }
            TypeExpr::Ref(name)
                if !self.type_names.contains(name.as_str()) && self.is_resource_type_name(name) =>
            {
                self.requirements
                    .resources
                    .ensure_resource_type(name.to_string());
            }
            TypeExpr::Any
            | TypeExpr::Str
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Bool
            | TypeExpr::Dict
            | TypeExpr::Null
            | TypeExpr::Enum(_)
            | TypeExpr::Ref(_) => {}
        }
    }

    fn is_host_data_type_name(&self, name: &str) -> bool {
        self.resource_catalog
            .map(|catalog| catalog.has_named_data_type(name))
            .unwrap_or(false)
    }

    fn is_resource_type_name(&self, name: &str) -> bool {
        self.resource_catalog
            .map(|catalog| catalog.has_resource_type(name))
            .unwrap_or(true)
    }

    fn collect_expr(
        &mut self,
        expr: &Expr,
        scope: &mut BTreeMap<String, RequirementBinding>,
    ) -> Option<RequirementBinding> {
        match expr {
            Expr::Block(expressions) => {
                let mut last = None;
                for expression in expressions {
                    last = self.collect_expr(expression, scope);
                }
                last
            }
            Expr::LabelAnnotated { expr, .. } => {
                self.requirements.language_features.label_annotations = true;
                self.collect_expr(expr, scope)
            }
            Expr::Variable(name) => match scope.get(name.as_str()).cloned() {
                Some(binding) => Some(binding),
                None => {
                    self.requirements.globals.insert(name.to_string());
                    Some(RequirementBinding::Value)
                }
            },
            Expr::Tuple(items) => {
                for item in items {
                    self.collect_expr(item, scope);
                }
                Some(RequirementBinding::Value)
            }
            Expr::List(items) => {
                for item in items {
                    self.collect_expr(item, scope);
                }
                Some(RequirementBinding::Value)
            }
            Expr::ListComprehension { element, clauses } => {
                let mut previous = Vec::new();
                for clause in clauses {
                    match clause {
                        ListComprehensionClause::For { binding, iterable } => {
                            self.collect_expr(iterable, scope);
                            previous.push((
                                binding.to_string(),
                                scope.insert(binding.to_string(), RequirementBinding::Value),
                            ));
                        }
                        ListComprehensionClause::If { condition } => {
                            self.collect_expr(condition, scope);
                        }
                    }
                }
                self.collect_expr(element, scope);
                for (binding, previous) in previous.into_iter().rev() {
                    if let Some(previous) = previous {
                        scope.insert(binding, previous);
                    } else {
                        scope.remove(binding.as_str());
                    }
                }
                Some(RequirementBinding::Value)
            }
            Expr::Record(entries) => {
                for (_, value) in entries {
                    self.collect_expr(value, scope);
                }
                Some(RequirementBinding::Value)
            }
            Expr::Assign { target, expr } => {
                for step in &target.steps {
                    if let AssignPathStep::Index(index) = step {
                        self.collect_expr(index, scope);
                    }
                }
                let binding = self
                    .collect_expr(expr, scope)
                    .unwrap_or(RequirementBinding::Value);
                if target.steps.is_empty() {
                    scope.insert(target.root.to_string(), binding);
                } else if !scope.contains_key(target.root.as_str()) {
                    self.requirements.globals.insert(target.root.to_string());
                }
                Some(RequirementBinding::Value)
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } => {
                self.collect_expr(condition, scope);
                let mut then_scope = scope.clone();
                self.collect_expr(then_block, &mut then_scope);
                let mut else_scope = scope.clone();
                self.collect_expr(else_block, &mut else_scope);
                for (name, binding) in then_scope.into_iter().chain(else_scope) {
                    scope.entry(name).or_insert(binding);
                }
                Some(RequirementBinding::Value)
            }
            Expr::For {
                binding,
                iterable,
                body,
            } => {
                self.collect_expr(iterable, scope);
                let previous = scope.insert(binding.to_string(), RequirementBinding::Value);
                self.collect_expr(body, scope);
                if let Some(previous) = previous {
                    scope.insert(binding.to_string(), previous);
                } else {
                    scope.remove(binding.as_str());
                }
                Some(RequirementBinding::Value)
            }
            Expr::While { condition, body } => {
                self.collect_expr(condition, scope);
                self.collect_expr(body, scope);
                Some(RequirementBinding::Value)
            }
            Expr::StartProcess(start) => {
                self.requirements.abilities.processes = true;
                for (_, value) in &start.args {
                    self.collect_expr(value, scope);
                }
                Some(RequirementBinding::Value)
            }
            Expr::ProcessRef { .. } => {
                self.requirements.abilities.processes = true;
                Some(RequirementBinding::Value)
            }
            Expr::HostDescriptorConstructor { type_name, input } => {
                if let Some(catalog) = self.resource_catalog
                    && let Some(constructor) = catalog
                        .value_constructors()
                        .map(|(_, constructor)| constructor)
                        .find(|constructor| constructor.type_name == type_name.as_str())
                {
                    self.requirements.resources.add_value_constructor(
                        constructor.path.iter().map(String::as_str),
                        constructor.input_ty.clone(),
                        constructor.output_ty.clone(),
                    );
                }
                if let Some(catalog) = self.resource_catalog
                    && let Some(binding) = catalog.resolve_trigger_source(type_name.as_str())
                {
                    self.requirements
                        .resources
                        .add_trigger_source_type(
                            type_name.to_string(),
                            binding.event_type().clone(),
                        )
                        .expect("trigger source requirement came from host catalog");
                }
                self.collect_expr(input, scope);
                Some(RequirementBinding::Value)
            }
            Expr::ResourceRef(resource) => {
                self.require_resource_ref(resource);
                Some(RequirementBinding::Resource {
                    resource_type: resource.resource_type.to_string(),
                    path: Some(resource.path.iter().map(ToString::to_string).collect()),
                })
            }
            Expr::ReceiverCall {
                receiver,
                operation,
                args,
            } => {
                let receiver = self.collect_expr(receiver, scope);
                if let Some(RequirementBinding::Resource {
                    resource_type,
                    path,
                }) = receiver
                {
                    self.require_resource_operation(resource_type, path, operation.as_str());
                }
                for arg in args {
                    self.collect_expr(arg, scope);
                }
                Some(RequirementBinding::Value)
            }
            Expr::SleepFor(expr) | Expr::SleepUntil(expr) => {
                self.requirements.abilities.sleep = true;
                self.collect_expr(expr, scope);
                Some(RequirementBinding::Value)
            }
            Expr::WaitSignal { .. } => {
                self.requirements.abilities.process_signals = true;
                Some(RequirementBinding::Value)
            }
            Expr::SignalRun { run, payload, .. } => {
                self.requirements.abilities.process_signals = true;
                self.collect_expr(run, scope);
                self.collect_expr(payload, scope);
                Some(RequirementBinding::Value)
            }
            Expr::Await(expr)
            | Expr::ResultUnwrap(expr)
            | Expr::Cancel(expr)
            | Expr::Print(expr)
            | Expr::Yield(expr)
            | Expr::Wake(expr)
            | Expr::Fail(expr)
            | Expr::Unary { expr, .. } => {
                self.collect_expr(expr, scope);
                Some(RequirementBinding::Value)
            }
            Expr::Finish(expr) => {
                self.collect_expr(expr, scope);
                Some(RequirementBinding::Value)
            }
            Expr::BuiltinCall { args, .. } | Expr::FunctionCall { args, .. } => {
                for arg in args {
                    self.collect_expr(arg, scope);
                }
                Some(RequirementBinding::Value)
            }
            Expr::Function(function) => {
                for capture in &function.captures {
                    if !scope.contains_key(capture.as_str()) {
                        self.requirements.globals.insert(capture.to_string());
                    }
                }
                let mut function_scope = BTreeMap::new();
                for capture in &function.captures {
                    function_scope.insert(capture.to_string(), RequirementBinding::Value);
                }
                for param in &function.params {
                    function_scope.insert(param.to_string(), RequirementBinding::Value);
                }
                if let Some(name) = &function.name {
                    function_scope.insert(name.to_string(), RequirementBinding::Value);
                }
                self.collect_expr(&function.body, &mut function_scope);
                Some(RequirementBinding::Value)
            }
            Expr::Call { function, args } => {
                self.collect_expr(function, scope);
                for arg in args {
                    self.collect_expr(arg, scope);
                }
                Some(RequirementBinding::Value)
            }
            Expr::Map { items, function } => {
                self.collect_expr(items, scope);
                self.collect_expr(function, scope);
                Some(RequirementBinding::Value)
            }
            Expr::Try(exception) => {
                self.collect_expr(&exception.body, scope);
                if let Some(catch) = &exception.catch {
                    let previous =
                        scope.insert(catch.binding.to_string(), RequirementBinding::Value);
                    self.collect_expr(&catch.body, scope);
                    match previous {
                        Some(previous) => {
                            scope.insert(catch.binding.to_string(), previous);
                        }
                        None => {
                            scope.remove(catch.binding.as_str());
                        }
                    }
                }
                if let Some(finally) = &exception.finally {
                    self.collect_expr(finally, scope);
                }
                Some(RequirementBinding::Value)
            }
            Expr::Throw(value)
            | Expr::Return(value)
            | Expr::JavaScriptUnary { expr: value, .. } => {
                self.collect_expr(value, scope);
                Some(RequirementBinding::Value)
            }
            Expr::Field { target, .. } => {
                self.collect_expr(target, scope);
                Some(RequirementBinding::Value)
            }
            Expr::Index { target, index } => {
                self.collect_expr(target, scope);
                self.collect_expr(index, scope);
                Some(RequirementBinding::Value)
            }
            Expr::Binary { left, right, .. }
            | Expr::JavaScriptBinary { left, right, .. }
            | Expr::JavaScriptLogical { left, right, .. } => {
                self.collect_expr(left, scope);
                self.collect_expr(right, scope);
                Some(RequirementBinding::Value)
            }
            Expr::TypeLiteral(ty) => {
                self.collect_type(ty);
                Some(RequirementBinding::Value)
            }
            Expr::Null
            | Expr::Undefined
            | Expr::Bool(_)
            | Expr::Number(_)
            | Expr::String(_)
            | Expr::Break
            | Expr::Continue => Some(RequirementBinding::Value),
        }
    }

    fn require_resource_ref(&mut self, resource: &ResourceRefExpr) {
        self.requirements
            .resources
            .add_module_instance(
                resource.path.iter().map(|segment| segment.as_str()),
                resource.resource_type.to_string(),
            )
            .expect("resolved resource references cannot conflict");
    }

    fn require_resource_operation(
        &mut self,
        resource_type: String,
        path: Option<Vec<String>>,
        operation: &str,
    ) {
        let (operation, binding) = self.resource_operation_requirement(&resource_type, operation);
        if let (Some(catalog), Some(path)) = (self.resource_catalog, path.as_ref()) {
            let alias = path.join(".");
            if let Some(module_binding) =
                catalog.resolve_module_operation(&resource_type, &alias, &operation)
            {
                self.requirements
                    .resources
                    .add_module_operation_binding(
                        path.iter().map(String::as_str),
                        resource_type,
                        operation,
                        module_binding.host_operation.clone(),
                        binding,
                    )
                    .expect("host catalog operation must not conflict");
                return;
            }
        }
        self.requirements
            .resources
            .add_operation_binding(resource_type, operation, binding);
    }

    fn resource_operation_requirement(
        &self,
        resource_type: &str,
        operation: &str,
    ) -> (String, ResourceOperationBinding) {
        if let Some(catalog) = self.resource_catalog
            && let Some(binding) = catalog.resolve_operation(resource_type, operation)
        {
            return (operation.to_string(), binding.clone());
        }
        (
            operation.to_string(),
            ResourceOperationBinding {
                input_ty: TypeExpr::Any,
                output_ty: TypeExpr::Any,
                output_from_input: None,
            },
        )
    }
}
