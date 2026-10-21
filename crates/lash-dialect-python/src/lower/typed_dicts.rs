//! A workflow module's `TypedDict` classes: record types its entries'
//! parameters and results may be annotated with, and whose fields `#:`
//! lines caption. A `TypedDict` declares a type and nothing else: it emits
//! no statement and is no value.

use std::collections::BTreeMap;

use lash_kernel_doc::Caption;
use lash_vocabulary::{RecordType, RecordTypeField, Type};
use ruff_python_ast::{self as ast, Expr as PyExpr, Stmt as PyStmt};
use ruff_text_size::{Ranged, TextSize};

/// One `TypedDict` a module declares: its fields in declaration order.
#[derive(Clone, Debug)]
pub(crate) struct TypedDict {
    fields: Vec<TypedField>,
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

/// The module's `TypedDict`s, by name.
pub(crate) type TypedDicts = BTreeMap<String, TypedDict>;

/// The `TypedDict` `class` declares, when it is one: a single `TypedDict`
/// base, no keyword, decorator or type parameter, and a body of annotated
/// field names, `pass` and docstrings. A field declared twice keeps its
/// first place and its last annotation, as Python's does.
pub(crate) fn declared(class: &ast::StmtClassDef) -> Option<TypedDict> {
    let arguments = class.arguments.as_deref()?;
    let [PyExpr::Name(base)] = &*arguments.args else {
        return None;
    };
    if base.id.as_str() != "TypedDict"
        || !arguments.keywords.is_empty()
        || !class.decorator_list.is_empty()
        || class.type_params.is_some()
    {
        return None;
    }
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
    Some(TypedDict { fields })
}

/// What a Python annotation means as a kernel type: `int`, `float`, `bool`,
/// `str`, `bytes` and `None`; `list`, `set` and `dict`, bare or with their
/// item types; `tuple` with its member types; `Any` and `object`; a
/// `TypedDict` the module declares, as a closed record; and unions written
/// with `|`, `Optional` or `Union`. `None` when it is none of these, or a
/// `TypedDict` that holds itself.
pub(crate) fn annotation_type(annotation: &PyExpr, typed: &TypedDicts) -> Option<Type> {
    kernel_type(annotation, typed, &mut Vec::new())
}

fn kernel_type<'t>(
    annotation: &'t PyExpr,
    typed: &'t TypedDicts,
    open: &mut Vec<&'t str>,
) -> Option<Type> {
    match annotation {
        PyExpr::NoneLiteral(_) => Some(Type::Null),
        PyExpr::Name(name) => match name.id.as_str() {
            "int" => Some(Type::Int),
            "float" => Some(Type::Float),
            "bool" => Some(Type::Bool),
            "str" => Some(Type::Text),
            "bytes" => Some(Type::Bytes),
            "None" => Some(Type::Null),
            "Any" | "object" => Some(Type::Any),
            "list" => Some(Type::List(Box::new(Type::Any))),
            "set" => Some(Type::Set(Box::new(Type::Any))),
            "dict" => Some(map(Type::Any, Type::Any)),
            other => {
                let (name, record) = typed.get_key_value(other)?;
                if open.contains(&name.as_str()) {
                    return None;
                }
                open.push(name);
                let fields = record
                    .fields
                    .iter()
                    .map(|field| {
                        Some(RecordTypeField {
                            name: field.name.clone(),
                            ty: kernel_type(&field.annotation, typed, open)?,
                            optional: field.optional,
                        })
                    })
                    .collect::<Option<Vec<_>>>();
                open.pop();
                Some(Type::Record(RecordType {
                    fields: fields?,
                    rest: None,
                }))
            }
        },
        PyExpr::Subscript(subscript) => {
            let PyExpr::Name(name) = subscript.value.as_ref() else {
                return None;
            };
            let types = members(subscript)
                .into_iter()
                .map(|member| kernel_type(member, typed, open))
                .collect::<Option<Vec<_>>>()?;
            match (name.id.as_str(), types.as_slice()) {
                ("list", [item]) => Some(Type::List(Box::new(item.clone()))),
                ("set", [item]) => Some(Type::Set(Box::new(item.clone()))),
                ("dict", [key, value]) => Some(map(key.clone(), value.clone())),
                ("tuple", members) if !members.is_empty() => Some(Type::Tuple(members.to_vec())),
                ("Optional", [item]) => Some(union(vec![item.clone(), Type::Null])),
                ("Union", members) if !members.is_empty() => Some(union(members.to_vec())),
                _ => None,
            }
        }
        PyExpr::BinOp(operation) if operation.op == ast::Operator::BitOr => Some(union(vec![
            kernel_type(&operation.left, typed, open)?,
            kernel_type(&operation.right, typed, open)?,
        ])),
        _ => None,
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

/// One type of `members`: none nested, none twice, and any value if one
/// member is.
fn union(members: Vec<Type>) -> Type {
    let mut types: Vec<Type> = Vec::new();
    for member in members {
        let flat = match member {
            Type::Any => return Type::Any,
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
