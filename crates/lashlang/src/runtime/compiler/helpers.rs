fn expr_supports_forced_effect_site(expr: &Expr) -> bool {
    matches!(expr, Expr::ReceiverCall { .. } | Expr::Await(_))
        || matches!(
            expr,
            Expr::ResultUnwrap(inner)
                if matches!(inner.as_ref(), Expr::ReceiverCall { .. } | Expr::Await(_))
        )
}

/// Maps a builtin name to the [`IntrinsicOp`] the VM dispatches on, threading
/// `argc` into the arity-carrying ops. Returns `None` for names that are not
/// builtins (the caller decides whether that is an `Unknown` op or a const-fold
/// miss). This is the single name -> op authority shared by `resolve_intrinsic`
/// and the const folder.
fn intrinsic_for_builtin(name: &str, argc: usize) -> Option<IntrinsicOp> {
    Some(match name {
        "len" => IntrinsicOp::Len,
        "empty" => IntrinsicOp::Empty,
        "keys" => IntrinsicOp::Keys,
        "values" => IntrinsicOp::Values,
        "contains" => IntrinsicOp::Contains,
        "find" => IntrinsicOp::Find(argc),
        "grep_text" => IntrinsicOp::GrepText,
        "starts_with" => IntrinsicOp::StartsWith,
        "ends_with" => IntrinsicOp::EndsWith,
        "split" => IntrinsicOp::Split,
        "join" => IntrinsicOp::Join,
        "__typescript_split" => IntrinsicOp::JavaScriptSplit,
        "__typescript_join" => IntrinsicOp::JavaScriptJoin,
        "__typescript_stdlib" => IntrinsicOp::JavaScriptStdlib(argc),
        "__typescript_heap_new" => IntrinsicOp::JavaScriptHeapNew(argc),
        "__typescript_heap_instanceof" => IntrinsicOp::JavaScriptHeapInstanceOf,
        "__typescript_global_delete" => IntrinsicOp::JavaScriptGlobalDelete,
        "__typescript_global_has" => IntrinsicOp::JavaScriptGlobalHas,
        "__typescript_global_set" => IntrinsicOp::JavaScriptGlobalSet,
        "__typescript_encode_uri_component" => {
            IntrinsicOp::JavaScriptUriCodec(JavaScriptUriCodec::EncodeComponent)
        }
        "__typescript_decode_uri_component" => {
            IntrinsicOp::JavaScriptUriCodec(JavaScriptUriCodec::DecodeComponent)
        }
        "__typescript_encode_uri" => IntrinsicOp::JavaScriptUriCodec(JavaScriptUriCodec::EncodeUri),
        "__typescript_decode_uri" => IntrinsicOp::JavaScriptUriCodec(JavaScriptUriCodec::DecodeUri),
        "trim" => IntrinsicOp::Trim,
        "slice" => IntrinsicOp::Slice,
        "to_string" => IntrinsicOp::ToString,
        "to_int" => IntrinsicOp::ToInt,
        "to_float" => IntrinsicOp::ToFloat,
        "json_parse" => IntrinsicOp::JsonParse,
        "format" => IntrinsicOp::Format(argc),
        "validate" => IntrinsicOp::Validate,
        "range" => IntrinsicOp::Range(argc),
        "ceil_div" => IntrinsicOp::CeilDiv,
        "floor_div" => IntrinsicOp::FloorDiv,
        "push" => IntrinsicOp::Push,
        "sort" => IntrinsicOp::Sort,
        "sort_by" => IntrinsicOp::SortBy,
        "sum" => IntrinsicOp::Sum,
        "min" => IntrinsicOp::Min,
        "max" => IntrinsicOp::Max,
        "replace" => IntrinsicOp::Replace,
        "lower" => IntrinsicOp::Lower,
        "upper" => IntrinsicOp::Upper,
        "unique" => IntrinsicOp::Unique,
        "reverse" => IntrinsicOp::Reverse,
        _ => return None,
    })
}

fn expr_key(expr: &Expr) -> usize {
    expr as *const Expr as usize
}

fn lashlang_execution_paths(program: &Program) -> FxHashMap<usize, LashlangAstPath> {
    let mut paths = FxHashMap::default();
    let mut path = Vec::new();
    collect_lashlang_execution_paths(&program.main, &mut path, &mut paths);
    paths
}

fn expression_source_spans(program: &Program) -> FxHashMap<usize, Span> {
    let spans_by_path = program
        .expression_source_spans
        .iter()
        .map(|source_span| (source_span.path.clone(), source_span.span))
        .collect::<FxHashMap<_, _>>();
    let mut spans = FxHashMap::default();
    let mut path = Vec::new();
    collect_expression_source_spans(&program.main, &mut path, &spans_by_path, &mut spans);
    spans
}

fn collect_expression_source_spans(
    expr: &Expr,
    path: &mut Vec<u32>,
    spans_by_path: &FxHashMap<Vec<u32>, Span>,
    spans: &mut FxHashMap<usize, Span>,
) {
    if let Some(span) = spans_by_path.get(path.as_slice()).copied() {
        spans.insert(expr_key(expr), span);
    }
    for (index, child) in expr.children().enumerate() {
        path.push(index as u32);
        collect_expression_source_spans(child, path, spans_by_path, spans);
        path.pop();
    }
}

fn collect_lashlang_execution_paths(
    expr: &Expr,
    path: &mut Vec<u32>,
    paths: &mut FxHashMap<usize, LashlangAstPath>,
) {
    paths.insert(expr_key(expr), LashlangAstPath::from_indices(path));
    if let Expr::LabelAnnotated { expr, .. } = expr {
        collect_lashlang_execution_paths(expr, path, paths);
        return;
    }
    for (index, child) in expr.children().enumerate() {
        path.push(index as u32);
        collect_lashlang_execution_paths(child, path, paths);
        path.pop();
    }
}

fn label_attaches_to_concrete_node(expr: &Expr) -> bool {
    match expr {
        Expr::LabelAnnotated { .. } => false,
        Expr::Assign { expr, .. } => label_attaches_to_assignment_value(expr),
        Expr::Await(expr) | Expr::ResultUnwrap(expr) => label_attaches_to_concrete_node(expr),
        Expr::ReceiverCall { .. }
        | Expr::StartProcess(_)
        | Expr::SleepFor(_)
        | Expr::SleepUntil(_)
        | Expr::WaitSignal { .. }
        | Expr::SignalRun { .. }
        | Expr::Yield(_)
        | Expr::Wake(_)
        | Expr::Finish(_)
        | Expr::Fail(_)
        | Expr::If { .. } => true,
        Expr::Block(_)
        | Expr::Null
        | Expr::Undefined
        | Expr::Bool(_)
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::Variable(_)
        | Expr::Tuple(_)
        | Expr::List(_)
        | Expr::ListComprehension { .. }
        | Expr::Record(_)
        | Expr::For { .. }
        | Expr::While { .. }
        | Expr::Break
        | Expr::Continue
        | Expr::ProcessRef { .. }
        | Expr::HostDescriptorConstructor { .. }
        | Expr::ResourceRef(_)
        | Expr::Cancel(_)
        | Expr::Print(_)
        | Expr::BuiltinCall { .. }
        | Expr::Function(_)
        | Expr::Call { .. }
        | Expr::Map { .. }
        | Expr::Try(_)
        | Expr::Throw(_)
        | Expr::Return(_)
        | Expr::Field { .. }
        | Expr::Index { .. }
        | Expr::Unary { .. }
        | Expr::Binary { .. }
        | Expr::JavaScriptUnary { .. }
        | Expr::JavaScriptBinary { .. }
        | Expr::JavaScriptLogical { .. }
        | Expr::TypeLiteral(_) => false,
    }
}

fn label_attaches_to_assignment_value(expr: &Expr) -> bool {
    match expr {
        Expr::Await(expr) | Expr::ResultUnwrap(expr) => label_attaches_to_assignment_value(expr),
        Expr::ReceiverCall { .. }
        | Expr::StartProcess(_)
        | Expr::SleepFor(_)
        | Expr::SleepUntil(_)
        | Expr::WaitSignal { .. }
        | Expr::SignalRun { .. }
        | Expr::Yield(_)
        | Expr::Wake(_)
        | Expr::Finish(_)
        | Expr::Fail(_)
        | Expr::If { .. } => true,
        _ => false,
    }
}

/// Whether a value produced by `expr` must be isolated before a durable store.
///
/// Container literals and comprehensions already isolate every member they
/// admit and build a fresh container around those copies, so the stored value is
/// exclusively owned by construction. Everything else — a variable, a field or
/// index read, a concatenation, a builtin result — can hand back a value whose
/// members are still reachable from another binding, and so has to be copied.
pub(crate) fn store_needs_isolation(expr: &Expr) -> bool {
    match expr {
        Expr::LabelAnnotated { expr, .. } => store_needs_isolation(expr),
        Expr::Tuple(_) | Expr::List(_) | Expr::Record(_) | Expr::ListComprehension { .. } => false,
        _ => true,
    }
}

pub(crate) fn is_pure_expr(expr: &Expr) -> bool {
    match expr {
        Expr::LabelAnnotated { expr, .. } => is_pure_expr(expr),
        Expr::Null
        | Expr::Undefined
        | Expr::Bool(_)
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::Variable(_)
        | Expr::ProcessRef { .. }
        | Expr::ResourceRef(_) => true,
        Expr::Tuple(items) => items.iter().all(is_pure_expr),
        Expr::List(items) => items.iter().all(is_pure_expr),
        Expr::Record(entries) => entries.iter().all(|(_, value)| is_pure_expr(value)),
        Expr::ResultUnwrap(expr) => is_pure_expr(expr),
        Expr::HostDescriptorConstructor { input, .. } => is_pure_expr(input),
        Expr::BuiltinCall { args, .. } => args.iter().all(is_pure_expr),
        Expr::Function(function) => function.captures.is_empty(),
        Expr::Call { .. } | Expr::Map { .. } | Expr::Try(_) | Expr::Throw(_) | Expr::Return(_) => {
            false
        }
        Expr::Field { target, .. } => is_pure_expr(target),
        Expr::Index { target, index } => is_pure_expr(target) && is_pure_expr(index),
        Expr::Unary { expr, .. } => is_pure_expr(expr),
        Expr::JavaScriptUnary { expr, .. } => is_pure_expr(expr),
        Expr::If {
            condition,
            then_block,
            else_block,
        } => is_pure_expr(condition) && is_pure_expr(then_block) && is_pure_expr(else_block),
        Expr::Binary { left, right, .. } => is_pure_expr(left) && is_pure_expr(right),
        Expr::JavaScriptBinary { left, right, .. }
        | Expr::JavaScriptLogical { left, right, .. } => is_pure_expr(left) && is_pure_expr(right),
        Expr::TypeLiteral(ty) => fold_type(ty).is_some(),
        Expr::Block(_)
        | Expr::Assign { .. }
        | Expr::For { .. }
        | Expr::ListComprehension { .. }
        | Expr::While { .. }
        | Expr::Break
        | Expr::Continue
        | Expr::ReceiverCall { .. }
        | Expr::StartProcess(_)
        | Expr::Await(_)
        | Expr::SleepFor(_)
        | Expr::SleepUntil(_)
        | Expr::WaitSignal { .. }
        | Expr::SignalRun { .. }
        | Expr::Cancel(_)
        | Expr::Print(_)
        | Expr::Yield(_)
        | Expr::Wake(_)
        | Expr::Finish(_)
        | Expr::Fail(_) => false,
    }
}

fn contains_type_literal(expr: &Expr) -> bool {
    // `TypeLiteral` is the only node that introduces a type literal directly;
    // every other node contains one only via a child expression. `children()`
    // already yields an `Assign` target's dynamic index steps, so the generic
    // structural recursion covers the path-assignment case too.
    matches!(expr, Expr::TypeLiteral(_)) || expr.children().any(contains_type_literal)
}

/// The JSON-Schema vocabulary the language emits. Both the compile-time
/// builder ([`fold_type`]) and the runtime instruction builder
/// ([`Compiler::compile_type_expr`]) reference these names so the schema shape
/// is defined exactly once.
mod schema_keys {
    pub(super) const TYPE: &str = "type";
    pub(super) const ITEMS: &str = "items";
    pub(super) const PROPERTIES: &str = "properties";
    pub(super) const REQUIRED: &str = "required";
    pub(super) const ADDITIONAL_PROPERTIES: &str = "additionalProperties";
    pub(super) const ANY_OF: &str = "anyOf";
    pub(super) const ENUM: &str = "enum";

    pub(super) const ARRAY: &str = "array";
    pub(super) const OBJECT: &str = "object";
    pub(super) const STRING: &str = "string";
}

/// Best-effort compile-time construction of a JSON-Schema Value for a
/// [`TypeExpr`]. This is the single authority for the language's type -> schema
/// shape; the runtime instruction builder mirrors only the dynamic `Ref` paths
/// and shares the same key vocabulary ([`schema_keys`]).
///
/// Returns `None` when the expression contains a [`TypeExpr::Ref`] (or a nested
/// composite that contains one) — those must be resolved at runtime via
/// [`Instruction::ResolveTypeRef`].
fn fold_type(ty: &TypeExpr) -> Option<Value> {
    use schema_keys::*;
    match ty {
        TypeExpr::Any => Some(interned_scalar_schema(ScalarSchemaKind::Any)),
        TypeExpr::Str => Some(interned_scalar_schema(ScalarSchemaKind::Str)),
        TypeExpr::Int => Some(interned_scalar_schema(ScalarSchemaKind::Int)),
        TypeExpr::Float => Some(interned_scalar_schema(ScalarSchemaKind::Float)),
        TypeExpr::Bool => Some(interned_scalar_schema(ScalarSchemaKind::Bool)),
        TypeExpr::Dict => Some(interned_scalar_schema(ScalarSchemaKind::Dict)),
        TypeExpr::Null => Some(interned_scalar_schema(ScalarSchemaKind::Null)),
        TypeExpr::Enum(values) => {
            let mut rec = record_with_capacity(2);
            rec.insert(TYPE.into(), Value::String(STRING.into()));
            let items: Vec<Value> = values.iter().map(|v| Value::String(v.clone())).collect();
            rec.insert(ENUM.into(), Value::List(items.into()));
            Some(Value::Record(Arc::new(rec)))
        }
        TypeExpr::List(inner) => {
            let inner_value = fold_type(inner)?;
            let mut rec = record_with_capacity(2);
            rec.insert(TYPE.into(), Value::String(ARRAY.into()));
            rec.insert(ITEMS.into(), inner_value);
            Some(Value::Record(Arc::new(rec)))
        }
        TypeExpr::Object(fields) => {
            let mut properties = record_with_capacity(fields.len());
            for field in fields {
                properties.insert(field.name.to_string(), fold_type(&field.ty)?);
            }
            let required: Vec<Value> = fields
                .iter()
                .filter(|f| !f.optional)
                .map(|f| Value::String(f.name.clone()))
                .collect();
            let mut rec = record_with_capacity(4);
            rec.insert(TYPE.into(), Value::String(OBJECT.into()));
            rec.insert(PROPERTIES.into(), Value::Record(Arc::new(properties)));
            rec.insert(REQUIRED.into(), Value::List(required.into()));
            rec.insert(ADDITIONAL_PROPERTIES.into(), Value::Bool(false));
            Some(Value::Record(Arc::new(rec)))
        }
        TypeExpr::Union(variants) => {
            let folded: Option<Vec<Value>> = variants.iter().map(fold_type).collect();
            let folded = folded?;
            let mut rec = record_with_capacity(1);
            rec.insert(ANY_OF.into(), Value::List(folded.into()));
            Some(Value::Record(Arc::new(rec)))
        }
        TypeExpr::Process { .. } | TypeExpr::TriggerHandle(_) => {
            Some(interned_scalar_schema(ScalarSchemaKind::Any))
        }
        TypeExpr::Ref(_) => None,
    }
}

fn wrap_type_schema_value(schema: Value) -> Value {
    let mut wrapper = record_with_capacity(1);
    wrapper.insert(LASH_TYPE_KEY.to_string(), schema);
    Value::Record(Arc::new(wrapper))
}

fn is_terminal_expr(expr: &Expr) -> bool {
    match expr {
        Expr::LabelAnnotated { expr, .. } => is_terminal_expr(expr),
        Expr::Finish(_) | Expr::Fail(_) => true,
        Expr::Block(expressions) => expressions.last().is_some_and(is_terminal_expr),
        Expr::If {
            then_block,
            else_block,
            ..
        } => is_terminal_expr(then_block) && is_terminal_expr(else_block),
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum ScalarSchemaKind {
    Any,
    Str,
    Int,
    Float,
    Bool,
    Dict,
    Null,
}

/// Returns an `Arc`-shared schema for a scalar. All sites referencing `str`
/// point at the same `Arc<Record>`, so emitting a Type literal with N string
/// fields allocates one record, not N.
fn interned_scalar_schema(kind: ScalarSchemaKind) -> Value {
    static CACHE: OnceLock<[Value; 7]> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        let build = |ty: &str| {
            let mut rec = record_with_capacity(1);
            rec.insert(schema_keys::TYPE.into(), Value::String(ty.into()));
            Value::Record(Arc::new(rec))
        };
        [
            Value::Record(Arc::new(record_with_capacity(0))), // Any == {}
            build(schema_keys::STRING),
            build("integer"),
            build("number"),
            build("boolean"),
            build(schema_keys::OBJECT),
            build("null"),
        ]
    });
    cache[kind as usize].clone()
}
