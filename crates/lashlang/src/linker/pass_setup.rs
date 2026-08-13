#[derive(Clone, Debug, PartialEq, Eq)]
enum Binding {
    Value(TypeExpr),
    /// A compile-time schema descriptor. This metatype is linker-only: its
    /// described shape cannot be written in Lashlang's surface type grammar.
    SchemaWitness {
        described_ty: TypeExpr,
    },
    Resource {
        resource_type: String,
    },
}

struct Linker<'module> {
    program: &'module Program,
    surface: &'module LashlangHostEnvironment,
    process_names: BTreeSet<String>,
    process_types: BTreeMap<String, TypeExpr>,
    type_names: BTreeSet<String>,
    type_defs: BTreeMap<String, TypeExpr>,
    expression_spans: BTreeMap<usize, Span>,
    expected_type_facts: Option<RefCell<ExpectedTypeFacts>>,
}

impl<'module> Linker<'module> {
    fn new(program: &'module Program, surface: &'module LashlangHostEnvironment) -> Self {
        Self {
            program,
            surface,
            process_names: BTreeSet::new(),
            process_types: BTreeMap::new(),
            type_names: BTreeSet::new(),
            type_defs: BTreeMap::new(),
            expression_spans: expression_spans_by_pointer(program),
            expected_type_facts: None,
        }
    }

    fn with_expected_type_facts(mut self) -> Self {
        self.expected_type_facts = Some(RefCell::new(ExpectedTypeFacts::default()));
        self
    }

    fn link_program(&mut self) -> Result<Program, LinkError> {
        // Single walk: collect declaration metadata, then lower (and validate)
        // declarations in source order, then lower main. Declaration errors
        // therefore still surface before main errors, matching the prior
        // two-pass (validate-then-lower) ordering.
        self.collect_declarations()?;
        let declarations = self
            .program
            .declarations
            .iter()
            .enumerate()
            .map(|(index, declaration)| {
                let span = self.program.declaration_spans.get(index).copied();
                self.lower_declaration(declaration, span)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut scope = Scope::new(false, None);
        for name in &self.surface.globals {
            scope.bind(name, any_binding());
        }
        let main = self.lower_expr(&self.program.main, &mut scope)?.0;
        Ok(Program {
            declarations,
            main,
            declaration_spans: self.program.declaration_spans.clone(),
            expression_spans: self.program.expression_spans.clone(),
            expression_source_spans: self.program.expression_source_spans.clone(),
        })
    }

    fn collect_declarations(&mut self) -> Result<(), LinkError> {
        self.ensure_label_annotations_enabled_for_program()?;
        let mut names = BTreeSet::new();
        for (index, declaration) in self.program.declarations.iter().enumerate() {
            let span = self.program.declaration_spans.get(index).copied();
            let (namespace, name) = match declaration {
                Declaration::Type(decl) => {
                    let name = decl.name.as_str();
                    if !names.insert(("type", name.to_string())) {
                        return Err(LinkError::DuplicateDeclaration {
                            name: name.to_string(),
                            span,
                        });
                    }
                    self.type_names.insert(decl.name.to_string());
                    self.type_defs
                        .insert(decl.name.to_string(), decl.ty.clone());
                    continue;
                }
                Declaration::Process(decl) => {
                    self.ensure_feature(self.surface.abilities.processes, "processes", span)?;
                    ("process", decl.name.as_str())
                }
            };
            if !names.insert((namespace, name.to_string())) {
                return Err(LinkError::DuplicateDeclaration {
                    name: name.to_string(),
                    span,
                });
            }
            if let Declaration::Process(decl) = declaration {
                self.process_names.insert(decl.name.to_string());
            }
        }
        for declaration in &self.program.declarations {
            match declaration {
                Declaration::Type(type_decl) => self.validate_type_refs(&type_decl.ty, None)?,
                Declaration::Process(process) => {
                    for param in &process.params {
                        self.validate_type_refs(&param.ty, None)?;
                    }
                    for signal in &process.signals {
                        self.validate_type_refs(&signal.ty, None)?;
                    }
                    if let Some(return_ty) = &process.return_ty {
                        self.validate_type_refs(return_ty, None)?;
                    }
                }
            }
        }
        for declaration in &self.program.declarations {
            if let Declaration::Process(process) = declaration {
                self.process_types.insert(
                    process.name.to_string(),
                    process_type_for_decl(process, TypeExpr::Any),
                );
            }
        }
        for (index, declaration) in self.program.declarations.iter().enumerate() {
            let Declaration::Process(process) = declaration else {
                continue;
            };
            let span = self.program.declaration_spans.get(index).copied();
            let output = self.infer_process_output(process, span)?;
            if let Some(expected) = &process.return_ty
                && !self.is_type_assignable(&output, expected)
            {
                return Err(LinkError::IncompatibleProcessReturn {
                    process: process.name.to_string(),
                    expected: format_type_expr(&self.resolve_type_aliases(expected)),
                    actual: format_type_expr(&self.resolve_type_aliases(&output)),
                    span,
                });
            }
            self.process_types.insert(
                process.name.to_string(),
                process_type_for_decl(process, output),
            );
        }
        Ok(())
    }

    fn ensure_label_annotations_enabled_for_program(&self) -> Result<(), LinkError> {
        if self.surface.language_features.label_annotations {
            return Ok(());
        }
        for (index, declaration) in self.program.declarations.iter().enumerate() {
            let span = self.program.declaration_spans.get(index).copied();
            if let Declaration::Process(process) = declaration
                && (process.label.is_some() || expr_has_label_annotation(&process.body))
            {
                return Err(LinkError::FeatureDisabled {
                    feature: "label annotations",
                    span,
                });
            }
        }
        if expr_has_label_annotation(&self.program.main) {
            return Err(LinkError::FeatureDisabled {
                feature: "label annotations",
                span: self.program.expression_spans.first().copied(),
            });
        }
        Ok(())
    }

    fn binding_for_type(&self, ty: &TypeExpr) -> Binding {
        match self.resource_type_for_type(ty) {
            Some(resource_type) => Binding::Resource { resource_type },
            _ => Binding::Value(ty.clone()),
        }
    }

    fn resource_type_for_type(&self, ty: &TypeExpr) -> Option<String> {
        match self.resolve_type_aliases(ty) {
            TypeExpr::Ref(name) if self.surface.resources.has_resource_type(name.as_str()) => {
                Some(name.to_string())
            }
            _ => None,
        }
    }

    fn resolve_type_aliases(&self, ty: &TypeExpr) -> TypeExpr {
        self.resolve_type_aliases_inner(ty, &mut BTreeSet::new())
    }

    fn closed_schema_witness_binding(&self, expr: &Expr) -> Option<Binding> {
        let described_ty = match strip_label_annotation(expr) {
            Expr::TypeLiteral(ty) => self.close_schema_type_expr(ty, &mut BTreeSet::new())?,
            Expr::Record(entries) => {
                let mut shorthand = serde_json::Map::new();
                for (name, descriptor) in entries {
                    let Expr::String(descriptor) = strip_label_annotation(descriptor) else {
                        return None;
                    };
                    shorthand.insert(
                        name.to_string(),
                        serde_json::Value::String(descriptor.to_string()),
                    );
                }
                let schema =
                    crate::parse_output_schema(Some(&serde_json::Value::Object(shorthand)))
                        .ok()??;
                crate::json_schema_to_type_expr(&schema)
            }
            _ => return None,
        };
        Some(Binding::SchemaWitness { described_ty })
    }

    fn close_schema_type_expr(
        &self,
        ty: &TypeExpr,
        resolving: &mut BTreeSet<String>,
    ) -> Option<TypeExpr> {
        Some(match ty {
            TypeExpr::Ref(name) => {
                if !resolving.insert(name.to_string()) {
                    return None;
                }
                let closed =
                    self.close_schema_type_expr(self.type_defs.get(name.as_str())?, resolving);
                resolving.remove(name.as_str());
                closed?
            }
            TypeExpr::List(item) => {
                TypeExpr::List(Box::new(self.close_schema_type_expr(item, resolving)?))
            }
            TypeExpr::Object(fields) => TypeExpr::Object(
                fields
                    .iter()
                    .map(|field| {
                        Some(TypeField {
                            name: field.name.clone(),
                            ty: self.close_schema_type_expr(&field.ty, resolving)?,
                            optional: field.optional,
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
            ),
            TypeExpr::Union(items) => TypeExpr::Union(
                items
                    .iter()
                    .map(|item| self.close_schema_type_expr(item, resolving))
                    .collect::<Option<Vec<_>>>()?,
            ),
            TypeExpr::Process {
                input,
                output,
                input_count,
            } => TypeExpr::Process {
                input: Box::new(self.close_schema_type_expr(input, resolving)?),
                output: Box::new(self.close_schema_type_expr(output, resolving)?),
                input_count: *input_count,
            },
            TypeExpr::TriggerHandle(event) => {
                TypeExpr::TriggerHandle(Box::new(self.close_schema_type_expr(event, resolving)?))
            }
            TypeExpr::Any
            | TypeExpr::Str
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Bool
            | TypeExpr::Dict
            | TypeExpr::Null
            | TypeExpr::Enum(_) => ty.clone(),
        })
    }

    fn operation_call_output_type(
        &self,
        operation: &ResourceOperationBinding,
        args: &[Expr],
    ) -> TypeExpr {
        let Some(output_from_input) = &operation.output_from_input else {
            return operation.output_ty.clone();
        };
        direct_call_input_field(args, &output_from_input.input_field)
            .and_then(|witness| self.closed_schema_witness_binding(witness))
            .and_then(|binding| match binding {
                Binding::SchemaWitness { described_ty } => Some(described_ty),
                Binding::Value(_) | Binding::Resource { .. } => None,
            })
            .or_else(|| output_from_input.default_schema.clone())
            .unwrap_or(TypeExpr::Any)
    }

    fn resolve_type_aliases_inner(&self, ty: &TypeExpr, seen: &mut BTreeSet<String>) -> TypeExpr {
        match ty {
            TypeExpr::Ref(name) => {
                if !seen.insert(name.to_string()) {
                    return ty.clone();
                }
                let resolved = if let Some(ty) = self.type_defs.get(name.as_str()) {
                    self.resolve_type_aliases_inner(ty, seen)
                } else if let Some(data_type) = self
                    .surface
                    .resources
                    .resolve_named_data_type(name.as_str())
                {
                    data_type.ty().clone()
                } else {
                    ty.clone()
                };
                seen.remove(name.as_str());
                resolved
            }
            TypeExpr::List(item) => {
                TypeExpr::List(Box::new(self.resolve_type_aliases_inner(item, seen)))
            }
            TypeExpr::Object(fields) => TypeExpr::Object(
                fields
                    .iter()
                    .map(|field| TypeField {
                        name: field.name.clone(),
                        ty: self.resolve_type_aliases_inner(&field.ty, seen),
                        optional: field.optional,
                    })
                    .collect(),
            ),
            TypeExpr::Union(items) => TypeExpr::Union(
                items
                    .iter()
                    .map(|item| self.resolve_type_aliases_inner(item, seen))
                    .collect(),
            ),
            TypeExpr::Process {
                input,
                output,
                input_count,
            } => TypeExpr::Process {
                input: Box::new(self.resolve_type_aliases_inner(input, seen)),
                output: Box::new(self.resolve_type_aliases_inner(output, seen)),
                input_count: *input_count,
            },
            TypeExpr::TriggerHandle(event) => {
                TypeExpr::TriggerHandle(Box::new(self.resolve_type_aliases_inner(event, seen)))
            }
            TypeExpr::Any
            | TypeExpr::Str
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Bool
            | TypeExpr::Dict
            | TypeExpr::Null
            | TypeExpr::Enum(_) => ty.clone(),
        }
    }

    fn is_type_assignable(&self, source: &TypeExpr, target: &TypeExpr) -> bool {
        let source = self.resolve_type_aliases(source);
        let target = self.resolve_type_aliases(target);
        crate::trigger::is_resolved_type_assignable(&source, &target)
    }

    fn validate_expected_literals(
        &self,
        expr: &Expr,
        expected: Option<&TypeExpr>,
        span: Option<Span>,
    ) -> Result<(), LinkError> {
        let Some(expected) = expected else {
            return Ok(());
        };
        let expected = self.resolve_type_aliases(expected);
        match (expr, &expected) {
            (Expr::LabelAnnotated { expr, .. }, _) => {
                self.validate_expected_literals(expr, Some(&expected), span)
            }
            (Expr::String(value), TypeExpr::Enum(members)) if !members.contains(value) => {
                Err(LinkError::IncompatibleExpectedLiteral {
                    expected: format_type_expr(&expected),
                    actual: format!("\"{value}\""),
                    span,
                })
            }
            (Expr::String(value), TypeExpr::Union(items)) => {
                let accepts = items.iter().any(|item| match item {
                    TypeExpr::Any | TypeExpr::Str => true,
                    TypeExpr::Enum(members) => members.contains(value),
                    _ => false,
                });
                if accepts {
                    Ok(())
                } else {
                    Err(LinkError::IncompatibleExpectedLiteral {
                        expected: format_type_expr(&expected),
                        actual: format!("\"{value}\""),
                        span,
                    })
                }
            }
            (Expr::Record(entries), TypeExpr::Object(fields)) => {
                for (name, value) in entries {
                    if let Some(field) = fields.iter().find(|field| field.name == *name) {
                        self.validate_expected_literals(value, Some(&field.ty), span)?;
                    }
                }
                Ok(())
            }
            (Expr::List(items) | Expr::Tuple(items), TypeExpr::List(item)) => {
                for value in items {
                    self.validate_expected_literals(value, Some(item), span)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn assignment_target_type(
        &self,
        target: &crate::ast::AssignTarget,
        scope: &Scope,
    ) -> Result<Option<TypeExpr>, LinkError> {
        let Some(mut ty) = scope.binding_type(&target.root) else {
            return Ok(None);
        };
        for step in &target.steps {
            ty = match step {
                AssignPathStep::Field(field) => self.field_type(&ty, field, scope.span)?,
                AssignPathStep::Index(_) => self.index_type(&ty, scope.span)?,
            };
        }
        Ok(Some(ty))
    }

    fn validate_binary_operands(
        &self,
        op: crate::ast::BinaryOp,
        left: &TypeExpr,
        right: &TypeExpr,
        span: Option<Span>,
    ) -> Result<(), LinkError> {
        let left = self.resolve_type_aliases(left);
        let right = self.resolve_type_aliases(right);
        let compatible = if op == crate::ast::BinaryOp::In {
            self.membership_operands_compatible(&left, &right)
        } else {
            binary_operands_compatible(op, &left, &right)
        };
        if compatible {
            Ok(())
        } else {
            Err(LinkError::IncompatibleBinaryOperands {
                operator: binary_op_source(op),
                left: format_type_expr(&left),
                right: format_type_expr(&right),
                span,
            })
        }
    }

    fn membership_operands_compatible(&self, needle: &TypeExpr, haystack: &TypeExpr) -> bool {
        match haystack {
            TypeExpr::Any | TypeExpr::Ref(_) => true,
            TypeExpr::Str | TypeExpr::Enum(_) => {
                matches!(needle, TypeExpr::Str | TypeExpr::Enum(_))
            }
            TypeExpr::List(item) => self.is_type_assignable(needle, item),
            TypeExpr::Object(_) | TypeExpr::Dict => membership_key_type(needle),
            TypeExpr::Union(items) => items
                .iter()
                .all(|item| self.membership_operands_compatible(needle, item)),
            _ => false,
        }
    }

    fn validate_shaping_builtin(
        &self,
        name: &str,
        args: &[TypeExpr],
        span: Option<Span>,
    ) -> Result<(), LinkError> {
        let incompatible = |builtin: &str, expected: &'static str, actual: &TypeExpr| {
            LinkError::IncompatibleBuiltinOperands {
                builtin: builtin.to_string(),
                expected,
                actual: format_type_expr(&self.resolve_type_aliases(actual)),
                span,
            }
        };
        match (name, args) {
            ("sort", [list]) | ("min", [list]) | ("max", [list]) => {
                let item = shaping_list_item(&self.resolve_type_aliases(list))
                    .ok_or_else(|| incompatible(name, "a list or tuple", list))?;
                if shaping_comparable_type(&item) {
                    Ok(())
                } else {
                    Err(incompatible(name, "a list of comparable values", list))
                }
            }
            ("sum", [list]) => {
                let item = shaping_list_item(&self.resolve_type_aliases(list))
                    .ok_or_else(|| incompatible("sum", "a list of numbers", list))?;
                if shaping_number_type(&item) {
                    Ok(())
                } else {
                    Err(incompatible("sum", "a list of numbers", list))
                }
            }
            ("sort_by", [list, path]) => {
                let item = shaping_list_item(&self.resolve_type_aliases(list))
                    .ok_or_else(|| incompatible("sort_by", "a list of records", list))?;
                if !shaping_record_type(&item) {
                    return Err(incompatible("sort_by", "a list of records", list));
                }
                if shaping_text_type(&self.resolve_type_aliases(path)) {
                    Ok(())
                } else {
                    Err(incompatible("sort_by", "a text field path", path))
                }
            }
            ("unique" | "reverse", [list]) => {
                if shaping_list_item(&self.resolve_type_aliases(list)).is_some() {
                    Ok(())
                } else {
                    Err(incompatible(
                        if name == "unique" {
                            "unique"
                        } else {
                            "reverse"
                        },
                        "a list or tuple",
                        list,
                    ))
                }
            }
            ("lower" | "upper", [value]) => {
                if shaping_text_type(&self.resolve_type_aliases(value)) {
                    Ok(())
                } else {
                    Err(incompatible(
                        if name == "lower" { "lower" } else { "upper" },
                        "text",
                        value,
                    ))
                }
            }
            ("replace", [text, needle, replacement]) => {
                for value in [text, needle, replacement] {
                    if !shaping_text_type(&self.resolve_type_aliases(value)) {
                        return Err(incompatible("replace", "three text arguments", value));
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn process_output_type(&self, process: &str) -> TypeExpr {
        match self.process_types.get(process) {
            // Awaited process handles are runtime result envelopes. Preserve
            // the inferred payload as the known branch while keeping the
            // envelope gradual; `?` does not narrow gradual information.
            Some(TypeExpr::Process { output, .. }) => {
                union_type(vec![*output.clone(), TypeExpr::Any])
            }
            _ => TypeExpr::Any,
        }
    }

    fn validate_type_refs(&self, ty: &TypeExpr, span: Option<Span>) -> Result<(), LinkError> {
        match ty {
            TypeExpr::Ref(name) => {
                if self.type_defs.contains_key(name.as_str())
                    || self.surface.resources.has_resource_type(name.as_str())
                    || self.surface.resources.has_named_data_type(name.as_str())
                    || self
                        .surface
                        .resources
                        .is_known_opaque_value_type(name.as_str())
                {
                    Ok(())
                } else {
                    Err(LinkError::UnknownType {
                        name: name.to_string(),
                        span,
                    })
                }
            }
            TypeExpr::List(item) => self.validate_type_refs(item, span),
            TypeExpr::Object(fields) => {
                for field in fields {
                    self.validate_type_refs(&field.ty, span)?;
                }
                Ok(())
            }
            TypeExpr::Union(items) => {
                for item in items {
                    self.validate_type_refs(item, span)?;
                }
                Ok(())
            }
            TypeExpr::Process { input, output, .. } => {
                self.validate_type_refs(input, span)?;
                self.validate_type_refs(output, span)
            }
            TypeExpr::TriggerHandle(event) => self.validate_type_refs(event, span),
            TypeExpr::Any
            | TypeExpr::Str
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Bool
            | TypeExpr::Dict
            | TypeExpr::Null
            | TypeExpr::Enum(_) => Ok(()),
        }
    }

    fn field_type(
        &self,
        target: &TypeExpr,
        field: &str,
        span: Option<Span>,
    ) -> Result<TypeExpr, LinkError> {
        let target = self.resolve_type_aliases(target);
        field_type(&target, field, span, |name| {
            self.surface.resources.is_known_opaque_value_type(name)
        })
    }

    fn index_type(&self, target: &TypeExpr, span: Option<Span>) -> Result<TypeExpr, LinkError> {
        let target = self.resolve_type_aliases(target);
        index_type(&target, span, |name| {
            self.surface.resources.is_known_opaque_value_type(name)
        })
    }

    fn iterable_item_type(
        &self,
        target: &TypeExpr,
        span: Option<Span>,
    ) -> Result<TypeExpr, LinkError> {
        iterable_item_type(&self.resolve_type_aliases(target), span)
    }

    fn ensure_feature(
        &self,
        enabled: bool,
        feature: &'static str,
        span: Option<Span>,
    ) -> Result<(), LinkError> {
        if enabled {
            Ok(())
        } else {
            Err(LinkError::FeatureDisabled { feature, span })
        }
    }

    fn validate_resource_ref(
        &self,
        resource: &ResourceRefExpr,
        span: Option<Span>,
    ) -> Result<ResourceRefExpr, LinkError> {
        if !resource.resource_type.is_empty() {
            return self
                .surface
                .resources
                .resolve_alias(resource)
                .map(|_| resource.clone())
                .ok_or_else(|| LinkError::UnknownResource {
                    path: resource.path_string(),
                    span,
                });
        }
        self.surface
            .resources
            .resolve_module_path(&resource.path)
            .ok_or_else(|| LinkError::UnknownResource {
                path: resource.path_string(),
                span,
            })
    }

    fn lower_declaration(
        &self,
        declaration: &Declaration,
        span: Option<Span>,
    ) -> Result<Declaration, LinkError> {
        Ok(match declaration {
            Declaration::Type(type_decl) => Declaration::Type(type_decl.clone()),
            Declaration::Process(process) => {
                self.ensure_feature(self.surface.abilities.processes, "processes", span)?;
                if process.label.is_some() {
                    self.ensure_feature(
                        self.surface.language_features.label_annotations,
                        "label annotations",
                        span,
                    )?;
                }
                let mut scope = Scope::new(true, span);
                scope.expected_return = process.return_ty.clone();
                let mut seen = BTreeSet::new();
                for param in &process.params {
                    if !seen.insert(param.name.to_string()) {
                        return Err(LinkError::DuplicateProcessParam {
                            name: param.name.to_string(),
                            span,
                        });
                    }
                    scope.bind(param.name.as_str(), self.binding_for_type(&param.ty));
                }
                let mut seen_signals = BTreeSet::new();
                for signal in &process.signals {
                    self.ensure_feature(
                        self.surface.abilities.process_signals,
                        "process signals",
                        span,
                    )?;
                    if !seen_signals.insert(signal.name.to_string()) {
                        return Err(LinkError::DuplicateProcessSignal {
                            name: signal.name.to_string(),
                            span,
                        });
                    }
                }
                scope.bind("input", Binding::Value(process_input_type(process)));
                scope.bind("inputs", Binding::Value(process_input_record_type(process)));
                let body = self.lower_expr(&process.body, &mut scope)?.0;
                Declaration::Process(ProcessDecl {
                    name: process.name.clone(),
                    params: process.params.clone(),
                    signals: process.signals.clone(),
                    return_ty: process.return_ty.clone(),
                    label: process.label.clone(),
                    body,
                })
            }
        })
    }
}
