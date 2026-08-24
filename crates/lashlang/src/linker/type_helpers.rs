#[derive(Clone)]
struct Scope {
    bindings: BTreeMap<String, Binding>,
    process_body: bool,
    expected_return: Option<TypeExpr>,
    span: Option<Span>,
}

impl Scope {
    fn new(process_body: bool, span: Option<Span>) -> Self {
        Self {
            bindings: BTreeMap::new(),
            process_body,
            expected_return: None,
            span,
        }
    }

    fn bind(&mut self, name: &str, binding: Binding) -> Option<Binding> {
        self.bindings.insert(name.to_string(), binding)
    }

    fn restore(&mut self, name: &str, previous: Option<Binding>) {
        match previous {
            Some(binding) => {
                self.bindings.insert(name.to_string(), binding);
            }
            None => {
                self.bindings.remove(name);
            }
        }
    }

    fn get(&self, name: &AstString) -> Option<Binding> {
        self.bindings.get(name.as_str()).cloned()
    }

    fn get_str(&self, name: &str) -> Option<Binding> {
        self.bindings.get(name).cloned()
    }

    fn join_branches(&mut self, left: Scope, right: Scope) {
        let names = left
            .bindings
            .keys()
            .chain(right.bindings.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for name in names {
            let binding =
                join_optional_bindings(left.bindings.get(&name), right.bindings.get(&name));
            self.bindings.insert(name, binding);
        }
    }

    fn widen_loop(&mut self, before: Scope, after_one_pass: Scope) {
        self.join_branches(before, after_one_pass);
    }

    fn binding_type(&self, name: &AstString) -> Option<TypeExpr> {
        self.get(name).map(|binding| binding_type(Some(&binding)))
    }

    fn update_path(
        &mut self,
        target: &crate::ast::AssignTarget,
        value_ty: &TypeExpr,
    ) -> Result<(), LinkError> {
        let Some(binding) = self.get(&target.root) else {
            return Err(LinkError::UnknownName {
                name: target.root.to_string(),
                span: self.span,
            });
        };
        let updated = update_binding_path(binding, &target.steps, value_ty, self.span)?;
        self.bind(target.root.as_str(), updated);
        Ok(())
    }
}

struct Completion {
    finishes: Vec<TypeExpr>,
    can_fallthrough: bool,
}

impl Completion {
    fn fallthrough() -> Self {
        Self {
            finishes: Vec::new(),
            can_fallthrough: true,
        }
    }
}

fn any_binding() -> Binding {
    Binding::Value(TypeExpr::Any)
}

fn binding_type(binding: Option<&Binding>) -> TypeExpr {
    match binding {
        Some(Binding::Value(ty)) => ty.clone(),
        Some(Binding::SchemaWitness { .. }) => TypeExpr::Any,
        Some(Binding::Resource { resource_type }) => TypeExpr::Ref(resource_type.as_str().into()),
        None => TypeExpr::Any,
    }
}

fn join_optional_bindings(left: Option<&Binding>, right: Option<&Binding>) -> Binding {
    match (left, right) {
        (Some(left), Some(right)) if left == right => left.clone(),
        (Some(left), Some(right)) => Binding::Value(union_type(vec![
            binding_type(Some(left)),
            binding_type(Some(right)),
        ])),
        // A binding created on only one path is not definitely initialized.
        (Some(_), None) | (None, Some(_)) | (None, None) => any_binding(),
    }
}

fn update_binding_path(
    binding: Binding,
    steps: &[AssignPathStep],
    value_ty: &TypeExpr,
    span: Option<Span>,
) -> Result<Binding, LinkError> {
    let Binding::Value(ty) = binding else {
        return Ok(binding);
    };
    Ok(Binding::Value(update_type_path(ty, steps, value_ty, span)?))
}

fn update_type_path(
    ty: TypeExpr,
    steps: &[AssignPathStep],
    value_ty: &TypeExpr,
    span: Option<Span>,
) -> Result<TypeExpr, LinkError> {
    let Some((step, rest)) = steps.split_first() else {
        return Ok(value_ty.clone());
    };
    match (ty, step) {
        (TypeExpr::Object(mut fields), AssignPathStep::Field(field)) => {
            let Some(existing) = fields.iter_mut().find(|candidate| candidate.name == *field)
            else {
                return Err(LinkError::UnknownObjectField {
                    field: field.to_string(),
                    span,
                });
            };
            existing.ty = update_type_path(existing.ty.clone(), rest, value_ty, span)?;
            Ok(TypeExpr::Object(fields))
        }
        (TypeExpr::List(item), AssignPathStep::Index(_)) => Ok(TypeExpr::List(Box::new(
            update_type_path(*item, rest, value_ty, span)?,
        ))),
        (TypeExpr::Union(items), AssignPathStep::Field(field)) => {
            let mut updated = false;
            let mut missing_error = None;
            let items = items
                .into_iter()
                .map(|item| {
                    if !type_has_field(&item, field) {
                        return Ok(item);
                    }
                    match update_type_path(item.clone(), steps, value_ty, span) {
                        Ok(item) => {
                            updated = true;
                            Ok(item)
                        }
                        Err(error @ LinkError::UnknownObjectField { .. }) => {
                            missing_error = Some(error);
                            Ok(item)
                        }
                        Err(error) => Err(error),
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            if updated {
                Ok(union_type(items))
            } else {
                Err(
                    missing_error.unwrap_or_else(|| LinkError::UnknownObjectField {
                        field: field.to_string(),
                        span,
                    }),
                )
            }
        }
        (TypeExpr::Union(items), _) => Ok(union_type(
            items
                .into_iter()
                .map(|item| update_type_path(item, steps, value_ty, span))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        (TypeExpr::Any, _) => Ok(TypeExpr::Any),
        (TypeExpr::Dict, _) => Ok(TypeExpr::Dict),
        (other, _) => Ok(other),
    }
}

fn type_has_field(ty: &TypeExpr, field: &str) -> bool {
    match ty {
        TypeExpr::Any | TypeExpr::Dict => true,
        TypeExpr::Object(fields) => fields.iter().any(|candidate| candidate.name == field),
        TypeExpr::Union(items) => items.iter().any(|item| type_has_field(item, field)),
        _ => false,
    }
}

fn literal_type(expr: &Expr) -> TypeExpr {
    match expr {
        Expr::Null => TypeExpr::Null,
        Expr::Undefined => TypeExpr::Any,
        Expr::Bool(_) => TypeExpr::Bool,
        Expr::Number(value) if value.is_finite() && value.fract() == 0.0 => TypeExpr::Int,
        Expr::Number(_) => TypeExpr::Float,
        Expr::String(_) => TypeExpr::Str,
        Expr::TypeLiteral(_) => TypeExpr::Any,
        Expr::Break | Expr::Continue => TypeExpr::Null,
        Expr::LabelAnnotated { expr, .. } => literal_type(expr),
        _ => TypeExpr::Any,
    }
}

fn strip_label_annotation(mut expr: &Expr) -> &Expr {
    while let Expr::LabelAnnotated { expr: inner, .. } = expr {
        expr = inner;
    }
    expr
}

fn direct_call_input_field<'a>(args: &'a [Expr], input_field: &str) -> Option<&'a Expr> {
    let [argument] = args else {
        return None;
    };
    let Expr::Record(entries) = strip_label_annotation(argument) else {
        return None;
    };
    entries
        .iter()
        .find_map(|(name, value)| (name == input_field).then_some(value))
}

fn union_type(items: Vec<TypeExpr>) -> TypeExpr {
    let mut flattened = Vec::new();
    for item in items {
        match item {
            TypeExpr::Union(items) => flattened.extend(items),
            other => flattened.push(other),
        }
    }
    let mut unique = Vec::new();
    for item in flattened {
        if !unique.contains(&item) {
            unique.push(item);
        }
    }
    match unique.as_slice() {
        // `lower_list` reaches this path for an empty list. Its element type
        // is therefore the empty-list sentinel `Null`; the JSON-Schema
        // importer deliberately differs because it normalizes a different
        // domain (FIG-1878).
        [] => TypeExpr::Null,
        [one] => one.clone(),
        _ => TypeExpr::Union(unique),
    }
}

fn call_input_type(arg_types: Vec<TypeExpr>) -> TypeExpr {
    match arg_types.as_slice() {
        [] => TypeExpr::Null,
        [one] => one.clone(),
        _ => TypeExpr::List(Box::new(union_type(arg_types))),
    }
}

fn field_type(
    target: &TypeExpr,
    field: &str,
    span: Option<Span>,
    is_opaque: impl Fn(&str) -> bool + Copy,
) -> Result<TypeExpr, LinkError> {
    match target {
        TypeExpr::Any | TypeExpr::Dict => Ok(TypeExpr::Any),
        TypeExpr::Ref(name) if is_opaque(name.as_str()) => {
            Err(LinkError::OpaqueHostDescriptorAccess {
                type_name: name.to_string(),
                access: format!(".{field}"),
                span,
            })
        }
        TypeExpr::Ref(_) => Ok(TypeExpr::Any),
        TypeExpr::Object(fields) => fields
            .iter()
            .find(|candidate| candidate.name.as_str() == field)
            .map(|field| field.ty.clone())
            .ok_or_else(|| LinkError::UnknownObjectField {
                field: field.to_string(),
                span,
            }),
        TypeExpr::Union(items) => {
            let mut fields = Vec::new();
            let mut missing = false;
            for item in items {
                match field_type(item, field, span, is_opaque) {
                    Ok(ty) => fields.push(ty),
                    Err(LinkError::UnknownObjectField { .. }) => missing = true,
                    Err(error) => return Err(error),
                }
            }
            if fields.is_empty() {
                Err(LinkError::UnknownObjectField {
                    field: field.to_string(),
                    span,
                })
            } else if missing {
                Ok(TypeExpr::Any)
            } else {
                Ok(union_type(fields))
            }
        }
        _ => Ok(TypeExpr::Any),
    }
}

fn index_type(
    target: &TypeExpr,
    span: Option<Span>,
    is_opaque: impl Fn(&str) -> bool + Copy,
) -> Result<TypeExpr, LinkError> {
    match target {
        TypeExpr::List(item) => Ok(*item.clone()),
        TypeExpr::Ref(name) if is_opaque(name.as_str()) => {
            Err(LinkError::OpaqueHostDescriptorAccess {
                type_name: name.to_string(),
                access: "[]".to_string(),
                span,
            })
        }
        TypeExpr::Ref(_) => Ok(TypeExpr::Any),
        TypeExpr::Union(items) => {
            let items = items
                .iter()
                .map(|item| index_type(item, span, is_opaque))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(union_type(items))
        }
        _ => Ok(TypeExpr::Any),
    }
}

fn iterable_item_type(target: &TypeExpr, span: Option<Span>) -> Result<TypeExpr, LinkError> {
    match target {
        TypeExpr::List(item) => Ok(*item.clone()),
        TypeExpr::Any | TypeExpr::Dict | TypeExpr::Ref(_) => Ok(TypeExpr::Any),
        TypeExpr::Union(items) => {
            let mut item_types = Vec::new();
            let mut has_non_list = false;
            for item in items {
                match iterable_item_type(item, span) {
                    Ok(item) => item_types.push(item),
                    Err(LinkError::IncompatibleIterationTarget { .. }) => has_non_list = true,
                    Err(error) => return Err(error),
                }
            }
            if item_types.is_empty() {
                Err(LinkError::IncompatibleIterationTarget {
                    actual: format_type_expr(target),
                    span,
                })
            } else if has_non_list {
                Ok(TypeExpr::Any)
            } else {
                Ok(union_type(item_types))
            }
        }
        _ => Err(LinkError::IncompatibleIterationTarget {
            actual: format_type_expr(target),
            span,
        }),
    }
}

fn builtin_return_type(name: &str) -> TypeExpr {
    match name {
        "len" | "find" | "to_int" | "ceil_div" | "floor_div" => TypeExpr::Int,
        "empty" | "contains" | "starts_with" | "ends_with" => TypeExpr::Bool,
        "to_float" => TypeExpr::Float,
        "to_string" | "trim" | "join" | "replace" | "lower" | "upper" => TypeExpr::Str,
        "sum" => TypeExpr::Float,
        "keys" | "values" | "split" | "grep_text" | "range" | "push" | "sort" | "sort_by"
        | "unique" | "reverse" => TypeExpr::List(Box::new(TypeExpr::Any)),
        "json_parse" | "validate" | "format" => TypeExpr::Any,
        _ => TypeExpr::Any,
    }
}

fn shaping_builtin_return_type(name: &str, args: &[TypeExpr]) -> TypeExpr {
    match (name, args) {
        ("sort" | "sort_by" | "unique" | "reverse", [list, ..]) => list.clone(),
        ("min" | "max", [TypeExpr::List(item)]) => *item.clone(),
        _ => builtin_return_type(name),
    }
}

fn shaping_list_item(ty: &TypeExpr) -> Option<TypeExpr> {
    match ty {
        TypeExpr::List(item) => Some(*item.clone()),
        TypeExpr::Any | TypeExpr::Ref(_) => Some(TypeExpr::Any),
        TypeExpr::Union(items) => Some(union_type(
            items
                .iter()
                .map(shaping_list_item)
                .collect::<Option<Vec<_>>>()?,
        )),
        _ => None,
    }
}

fn shaping_number_type(ty: &TypeExpr) -> bool {
    match ty {
        TypeExpr::Any | TypeExpr::Ref(_) | TypeExpr::Int | TypeExpr::Float => true,
        TypeExpr::Union(items) => items.iter().all(shaping_number_type),
        _ => false,
    }
}

fn shaping_text_type(ty: &TypeExpr) -> bool {
    match ty {
        TypeExpr::Any | TypeExpr::Ref(_) | TypeExpr::Str | TypeExpr::Enum(_) => true,
        TypeExpr::Union(items) => items.iter().all(shaping_text_type),
        _ => false,
    }
}

fn shaping_record_type(ty: &TypeExpr) -> bool {
    match ty {
        TypeExpr::Any | TypeExpr::Ref(_) | TypeExpr::Object(_) => true,
        TypeExpr::Union(items) => items.iter().all(shaping_record_type),
        _ => false,
    }
}

fn shaping_comparable_type(ty: &TypeExpr) -> bool {
    match ty {
        TypeExpr::Any
        | TypeExpr::Ref(_)
        | TypeExpr::Str
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Bool
        | TypeExpr::Null
        | TypeExpr::Enum(_) => true,
        TypeExpr::Union(items) => {
            let categories = items
                .iter()
                .map(shaping_comparable_category)
                .collect::<Vec<_>>();
            categories.iter().all(Option::is_some)
                && categories.windows(2).all(|pair| pair[0] == pair[1])
        }
        _ => false,
    }
}

fn shaping_comparable_category(ty: &TypeExpr) -> Option<u8> {
    match ty {
        TypeExpr::Any | TypeExpr::Dict | TypeExpr::Ref(_) => Some(0),
        TypeExpr::Int | TypeExpr::Float => Some(1),
        TypeExpr::Str | TypeExpr::Enum(_) => Some(2),
        TypeExpr::Bool => Some(3),
        TypeExpr::Null => Some(4),
        TypeExpr::Union(items) if shaping_comparable_type(ty) => {
            items.first().and_then(shaping_comparable_category)
        }
        _ => None,
    }
}

fn binary_return_type(op: crate::ast::BinaryOp) -> TypeExpr {
    match op {
        crate::ast::BinaryOp::Equal
        | crate::ast::BinaryOp::NotEqual
        | crate::ast::BinaryOp::Less
        | crate::ast::BinaryOp::LessEqual
        | crate::ast::BinaryOp::Greater
        | crate::ast::BinaryOp::GreaterEqual
        | crate::ast::BinaryOp::In
        | crate::ast::BinaryOp::And
        | crate::ast::BinaryOp::Or => TypeExpr::Bool,
        crate::ast::BinaryOp::Add
        | crate::ast::BinaryOp::Subtract
        | crate::ast::BinaryOp::Multiply
        | crate::ast::BinaryOp::Divide
        | crate::ast::BinaryOp::Modulo => TypeExpr::Float,
    }
}

fn binary_op_source(op: crate::ast::BinaryOp) -> &'static str {
    match op {
        crate::ast::BinaryOp::Add => "+",
        crate::ast::BinaryOp::Subtract => "-",
        crate::ast::BinaryOp::Multiply => "*",
        crate::ast::BinaryOp::Divide => "/",
        crate::ast::BinaryOp::Modulo => "%",
        crate::ast::BinaryOp::Equal => "==",
        crate::ast::BinaryOp::NotEqual => "!=",
        crate::ast::BinaryOp::Less => "<",
        crate::ast::BinaryOp::LessEqual => "<=",
        crate::ast::BinaryOp::Greater => ">",
        crate::ast::BinaryOp::GreaterEqual => ">=",
        crate::ast::BinaryOp::In => "in",
        crate::ast::BinaryOp::And => "and",
        crate::ast::BinaryOp::Or => "or",
    }
}

fn binary_operands_compatible(op: crate::ast::BinaryOp, left: &TypeExpr, right: &TypeExpr) -> bool {
    if type_is_gradual(left) || type_is_gradual(right) {
        return true;
    }
    match op {
        crate::ast::BinaryOp::And | crate::ast::BinaryOp::Or => {
            matches!(left, TypeExpr::Bool) && matches!(right, TypeExpr::Bool)
        }
        crate::ast::BinaryOp::Subtract
        | crate::ast::BinaryOp::Multiply
        | crate::ast::BinaryOp::Divide
        | crate::ast::BinaryOp::Modulo => type_is_scalar(left) && type_is_scalar(right),
        crate::ast::BinaryOp::Add => {
            (type_is_scalar(left) && type_is_scalar(right))
                || matches!((left, right), (TypeExpr::List(_), TypeExpr::List(_)))
        }
        crate::ast::BinaryOp::Equal | crate::ast::BinaryOp::NotEqual => {
            equality_operands_compatible(left, right)
        }
        crate::ast::BinaryOp::Less
        | crate::ast::BinaryOp::LessEqual
        | crate::ast::BinaryOp::Greater
        | crate::ast::BinaryOp::GreaterEqual => type_is_scalar(left) && type_is_scalar(right),
        crate::ast::BinaryOp::In => false,
    }
}

fn equality_operands_compatible(left: &TypeExpr, right: &TypeExpr) -> bool {
    match (left, right) {
        (TypeExpr::Union(items), other) | (other, TypeExpr::Union(items)) => items
            .iter()
            .any(|item| equality_operands_compatible(item, other)),
        _ => {
            type_is_gradual(left)
                || type_is_gradual(right)
                || type_category(left) == type_category(right)
        }
    }
}

fn type_is_gradual(ty: &TypeExpr) -> bool {
    match ty {
        TypeExpr::Any | TypeExpr::Dict | TypeExpr::Ref(_) => true,
        TypeExpr::Union(items) => items.iter().any(type_is_gradual),
        _ => false,
    }
}

fn type_is_scalar(ty: &TypeExpr) -> bool {
    match ty {
        TypeExpr::Str
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Bool
        | TypeExpr::Null
        | TypeExpr::Enum(_) => true,
        TypeExpr::Union(items) => items.iter().all(type_is_scalar),
        _ => false,
    }
}

fn membership_key_type(ty: &TypeExpr) -> bool {
    match ty {
        TypeExpr::Any | TypeExpr::Ref(_) | TypeExpr::Str | TypeExpr::Enum(_) => true,
        TypeExpr::Union(items) => items.iter().all(membership_key_type),
        _ => false,
    }
}

fn type_category(ty: &TypeExpr) -> u8 {
    match ty {
        TypeExpr::Str
        | TypeExpr::Int
        | TypeExpr::Float
        | TypeExpr::Bool
        | TypeExpr::Null
        | TypeExpr::Enum(_) => 1,
        TypeExpr::List(_) => 2,
        TypeExpr::Object(_) => 3,
        TypeExpr::Process { .. } => 4,
        TypeExpr::TriggerHandle(_) => 5,
        TypeExpr::Any | TypeExpr::Dict | TypeExpr::Ref(_) | TypeExpr::Union(_) => 0,
    }
}

fn expected_call_arg_type(input: &TypeExpr, arg_count: usize) -> Option<&TypeExpr> {
    match (arg_count, input) {
        (1, input) => Some(input),
        (_, TypeExpr::List(item)) => Some(item),
        _ => None,
    }
}

fn process_input_type(process: &ProcessDecl) -> TypeExpr {
    match process.params.as_slice() {
        [] => TypeExpr::Null,
        [param] => param.ty.clone(),
        _ => process_input_record_type(process),
    }
}

fn process_input_record_type(process: &ProcessDecl) -> TypeExpr {
    TypeExpr::Object(
        process
            .params
            .iter()
            .map(|param| TypeField {
                name: param.name.clone(),
                ty: param.ty.clone(),
                optional: false,
            })
            .collect(),
    )
}

fn process_type_for_decl(process: &ProcessDecl, output: TypeExpr) -> TypeExpr {
    TypeExpr::Process {
        input: Box::new(process_input_type(process)),
        output: Box::new(output),
        input_count: process.params.len(),
    }
}

fn module_path_for_expr(expr: &Expr) -> Option<Vec<AstString>> {
    match expr {
        Expr::LabelAnnotated { expr, .. } => module_path_for_expr(expr),
        Expr::Variable(name) => Some(vec![name.clone()]),
        Expr::Field { target, field } => {
            let mut path = module_path_for_expr(target)?;
            path.push(field.clone());
            Some(path)
        }
        Expr::ResourceRef(resource) => Some(resource.path.clone()),
        _ => None,
    }
}

fn is_trigger_event_expr(expr: &Expr) -> bool {
    matches!(
        module_path_for_expr(expr).as_deref(),
        Some([trigger, event]) if trigger.as_str() == "trigger" && event.as_str() == "event"
    )
}

fn is_trigger_event_projection_expr(expr: &Expr) -> bool {
    module_path_for_expr(expr).is_some_and(|path| {
        path.len() > 2 && path[0].as_str() == "trigger" && path[1].as_str() == "event"
    })
}

fn trigger_target_process_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::LabelAnnotated { expr, .. } => trigger_target_process_name(expr),
        Expr::Variable(name) | Expr::ProcessRef { process: name } => Some(name.to_string()),
        _ => None,
    }
}

fn trigger_target_process_label(expr: &Expr) -> String {
    trigger_target_process_name(expr).unwrap_or_else(|| "target".to_string())
}

fn expr_has_label_annotation(expr: &Expr) -> bool {
    match expr {
        Expr::LabelAnnotated { .. } => true,
        other => other.children().any(expr_has_label_annotation),
    }
}

/// The path of the first label annotation under `expr`, addressed the way
/// `Program::expression_source_spans` addresses expressions: one child index
/// per level, outermost first.
///
/// Called on `Program::main` — a block of root statements — the leading index
/// is the root statement index, so the returned path resolves to the
/// annotation's own span rather than to whichever statement happens to come
/// first.
fn label_annotation_path(expr: &Expr) -> Option<Vec<u32>> {
    if matches!(expr, Expr::LabelAnnotated { .. }) {
        return Some(Vec::new());
    }
    expr.children().enumerate().find_map(|(index, child)| {
        label_annotation_path(child).map(|mut path| {
            path.insert(0, index as u32);
            path
        })
    })
}
