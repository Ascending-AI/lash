//! A workflow module's `TypedDict` classes: record types its entries'
//! parameters and results may be annotated with, and whose fields `#:`
//! lines caption. A `TypedDict` declares a type and nothing else: it emits
//! no statement and is no value.

use std::collections::BTreeMap;

use lash_kernel_doc::Caption;
use lash_vocabulary::{AnnotationResolutionReason as Reason, RecordType, RecordTypeField, Type};
use ruff_python_ast::{self as ast, Expr as PyExpr, Stmt as PyStmt};
use ruff_text_size::{Ranged, TextSize};

/// One `TypedDict` a module declares: its fields in declaration order.
#[derive(Clone, Debug)]
pub(crate) struct TypedDict {
    fields: Vec<TypedField>,
    rest: Option<PyExpr>,
    closed: bool,
}

#[derive(Clone, Debug)]
struct TypedField {
    name: String,
    /// The field's annotation, without its `NotRequired[...]`.
    annotation: PyExpr,
    optional: bool,
    /// Where the field's line begins, which its caption stands above.
    start: TextSize,
}

/// The source's top-level `TypedDict`s, by name.
pub(crate) type TypedDicts = BTreeMap<String, TypedDict>;

/// The `TypedDict` `class` declares, when it is one: a single `TypedDict`
/// base, an optional `extra_items` annotation, no decorator or type parameter, and a body of annotated
/// field names, `pass` and docstrings. A field declared twice keeps its
/// first place and its last annotation, as Python's does.
pub(crate) fn declared(class: &ast::StmtClassDef) -> Option<TypedDict> {
    let arguments = class.arguments.as_deref()?;
    let [PyExpr::Name(base)] = &*arguments.args else {
        return None;
    };
    if base.id.as_str() != "TypedDict"
        || !class.decorator_list.is_empty()
        || class.type_params.is_some()
    {
        return None;
    }
    let (rest, closed) = match arguments.keywords.as_slice() {
        [] => (None, false),
        [keyword]
            if keyword
                .arg
                .as_ref()
                .is_some_and(|name| name.id.as_str() == "closed")
                && matches!(&keyword.value, PyExpr::BooleanLiteral(value) if value.value) =>
        {
            (None, true)
        }
        [keyword]
            if keyword
                .arg
                .as_ref()
                .is_some_and(|name| name.id.as_str() == "extra_items") =>
        {
            (Some(keyword.value.clone()), false)
        }
        _ => return None,
    };
    let mut fields: Vec<TypedField> = Vec::new();
    for statement in &class.body {
        match statement {
            PyStmt::Pass(_) => {}
            PyStmt::Expr(expression) if expression.value.is_string_literal_expr() => {}
            PyStmt::AnnAssign(field) if field.value.is_none() && field.simple => {
                let PyExpr::Name(name) = field.target.as_ref() else {
                    return None;
                };
                let (annotation, optional) = match field.annotation.as_ref() {
                    PyExpr::Subscript(subscript)
                        if matches!(subscript.value.as_ref(), PyExpr::Name(wrapper)
                            if matches!(wrapper.id.as_str(), "NotRequired" | "Required")) =>
                    {
                        let PyExpr::Name(wrapper) = subscript.value.as_ref() else {
                            return None;
                        };
                        (
                            subscript.slice.as_ref().clone(),
                            wrapper.id.as_str() == "NotRequired",
                        )
                    }
                    other => (other.clone(), false),
                };
                let declared = TypedField {
                    name: name.id.to_string(),
                    annotation,
                    optional,
                    start: field.start(),
                };
                match fields.iter_mut().find(|field| field.name == declared.name) {
                    Some(earlier) => {
                        earlier.annotation = declared.annotation;
                        earlier.optional = declared.optional;
                        earlier.start = declared.start;
                    }
                    None => fields.push(declared),
                }
            }
            _ => return None,
        }
    }
    Some(TypedDict {
        fields,
        rest,
        closed,
    })
}

/// Resolve the written data contract, retaining why an annotation cannot
/// describe an entry. The caller decides whether to enforce that contract.
pub(crate) fn annotation_type(
    annotation: &PyExpr,
    typed: &TypedDicts,
    entry_input: bool,
    max_depth: usize,
    mut max_nodes: usize,
) -> Result<Type, Reason> {
    kernel_type(
        annotation,
        typed,
        &mut Vec::new(),
        entry_input,
        max_depth,
        &mut max_nodes,
    )
}

fn kernel_type<'t>(
    annotation: &'t PyExpr,
    typed: &'t TypedDicts,
    open: &mut Vec<&'t str>,
    entry_input: bool,
    depth: usize,
    nodes: &mut usize,
) -> Result<Type, Reason> {
    if depth == 0 || *nodes == 0 {
        return Err(Reason::Limit);
    }
    *nodes -= 1;
    let unsupported = || Reason::Unsupported {
        construct: "annotation".into(),
    };
    match annotation {
        PyExpr::NoneLiteral(_) => Ok(Type::Null),
        PyExpr::Name(name) => match name.id.as_str() {
            "int" => Ok(Type::Int),
            "float" => Ok(Type::Float),
            "bool" => Ok(Type::Bool),
            "str" => Ok(Type::Text),
            "bytes" => Ok(Type::Bytes),
            "None" => Ok(Type::Null),
            "Any" | "object" | "JsonValue" => Ok(Type::Json),
            "list" => Ok(Type::List(Box::new(Type::Json))),
            "set" => Ok(Type::Set(Box::new(Type::Json))),
            "dict" => Ok(map(Type::Json, Type::Json)),
            other => {
                let (name, record) = typed
                    .get_key_value(other)
                    .ok_or_else(|| Reason::Unresolved { name: other.into() })?;
                if open.contains(&name.as_str()) {
                    return Err(Reason::Cycle { name: name.clone() });
                }
                open.push(name);
                let resolved =
                    (|| {
                        let fields = record
                            .fields
                            .iter()
                            .map(|field| {
                                Ok(RecordTypeField {
                                    name: field.name.clone(),
                                    ty: kernel_type(
                                        &field.annotation,
                                        typed,
                                        open,
                                        entry_input,
                                        depth - 1,
                                        nodes,
                                    )?,
                                    optional: field.optional,
                                })
                            })
                            .collect::<Result<Vec<_>, Reason>>()?;
                        let rest =
                            match &record.rest {
                                Some(rest) => lash_vocabulary::RecordRest::Keep(Box::new(
                                    kernel_type(rest, typed, open, entry_input, depth - 1, nodes)?,
                                )),
                                None if entry_input && !record.closed => {
                                    lash_vocabulary::RecordRest::Drop
                                }
                                None => lash_vocabulary::RecordRest::Closed,
                            };
                        Ok(Type::Record(RecordType { fields, rest }))
                    })();
                open.pop();
                resolved
            }
        },
        PyExpr::Subscript(subscript) => {
            let PyExpr::Name(name) = subscript.value.as_ref() else {
                return Err(unsupported());
            };
            let members = members(subscript);
            let supported = match name.id.as_str() {
                "list" | "set" | "Optional" => members.len() == 1,
                "dict" => members.len() == 2,
                "tuple" | "Union" => !members.is_empty(),
                _ => false,
            };
            if !supported {
                return Err(Reason::Unsupported {
                    construct: name.id.to_string(),
                });
            }
            let types = members
                .into_iter()
                .map(|member| kernel_type(member, typed, open, entry_input, depth - 1, nodes))
                .collect::<Result<Vec<_>, _>>()?;
            match (name.id.as_str(), types.as_slice()) {
                ("list", [item]) => Ok(Type::List(Box::new(item.clone()))),
                ("set", [item]) => Ok(Type::Set(Box::new(item.clone()))),
                ("dict", [key, value]) => Ok(map(key.clone(), value.clone())),
                ("tuple", members) => Ok(Type::Tuple(members.to_vec())),
                ("Optional", [item]) => Ok(union(vec![item.clone(), Type::Null])),
                ("Union", members) => Ok(union(members.to_vec())),
                _ => unreachable!("validated constructor arity"),
            }
        }
        PyExpr::BinOp(operation) if operation.op == ast::Operator::BitOr => Ok(union(vec![
            kernel_type(&operation.left, typed, open, entry_input, depth - 1, nodes)?,
            kernel_type(&operation.right, typed, open, entry_input, depth - 1, nodes)?,
        ])),
        _ => Err(unsupported()),
    }
}

fn members(subscript: &ast::ExprSubscript) -> Vec<&PyExpr> {
    match subscript.slice.as_ref() {
        PyExpr::Tuple(tuple) => tuple.elts.iter().collect(),
        single => vec![single],
    }
}

fn map(key: Type, value: Type) -> Type {
    Type::Map(Box::new(lash_vocabulary::MapType { key, value }))
}

/// One type of `members`: no nested unions or duplicate members.
fn union(members: Vec<Type>) -> Type {
    let mut types: Vec<Type> = Vec::new();
    for member in members {
        let flat = match member {
            Type::Union(inner) => inner,
            other => vec![other],
        };
        for member in flat {
            if !types.contains(&member) {
                types.push(member);
            }
        }
    }
    match types.len() {
        1 => types.remove(0),
        _ => Type::Union(types),
    }
}

/// The captions `#:` lines spell above the fields of the `TypedDict`s
/// `annotation` names, by path below it: through lists and unions, as a
/// field path walks a list and an optional value. `caption` answers the
/// caption above the line starting at an offset.
pub(crate) fn field_captions(
    annotation: &PyExpr,
    typed: &TypedDicts,
    caption: &dyn Fn(TextSize) -> Option<Caption>,
) -> Vec<(Vec<String>, Caption)> {
    let mut out = Vec::new();
    walk(
        annotation,
        typed,
        caption,
        &mut Vec::new(),
        &mut Vec::new(),
        &mut out,
    );
    out
}

fn walk<'t>(
    annotation: &'t PyExpr,
    typed: &'t TypedDicts,
    caption: &dyn Fn(TextSize) -> Option<Caption>,
    path: &mut Vec<String>,
    open: &mut Vec<&'t str>,
    out: &mut Vec<(Vec<String>, Caption)>,
) {
    match annotation {
        PyExpr::Name(name) => {
            let Some((name, record)) = typed.get_key_value(name.id.as_str()) else {
                return;
            };
            if open.contains(&name.as_str()) {
                return;
            }
            open.push(name);
            for field in &record.fields {
                path.push(field.name.clone());
                if let Some(caption) = caption(field.start) {
                    out.push((path.clone(), caption));
                }
                walk(&field.annotation, typed, caption, path, open, out);
                path.pop();
            }
            open.pop();
        }
        PyExpr::Subscript(subscript)
            if matches!(subscript.value.as_ref(), PyExpr::Name(name)
                if matches!(name.id.as_str(), "list" | "Optional" | "Union")) =>
        {
            for member in members(subscript) {
                walk(member, typed, caption, path, open, out);
            }
        }
        PyExpr::BinOp(operation) if operation.op == ast::Operator::BitOr => {
            walk(&operation.left, typed, caption, path, open, out);
            walk(&operation.right, typed, caption, path, open, out);
        }
        _ => {}
    }
}
