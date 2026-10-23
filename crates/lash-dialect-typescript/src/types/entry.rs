//! Bounded annotation resolution. Alias identities retain their declaring scope;
//! forward references and nested shadowing never depend on statement order.
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_vocabulary::{AnnotationResolutionReason as Reason, RecordType, RecordTypeField, Type};

use crate::SourceSpan;
use crate::adapter::{Stmt, TypeAnnotation, TypeShape};

type Scope = BTreeMap<String, usize>;
type Resolution = Result<Type, (SourceSpan, Reason)>;

#[derive(Clone, Copy)]
struct ResolutionScope<'a> {
    names: &'a Scope,
    substitutions: &'a BTreeMap<String, Type>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Position {
    Value,
    Parameter,
    Result,
}

#[derive(Debug)]
struct Alias {
    annotation: TypeAnnotation,
    scope: Arc<Scope>,
}

#[derive(Debug, Default)]
pub(crate) struct Annotations {
    scopes: Vec<Arc<Scope>>,
    aliases: Vec<Alias>,
    resolved: BTreeMap<SourceSpan, Resolution>,
    contexts: BTreeMap<SourceSpan, Arc<Scope>>,
    limits: (usize, usize),
    pub(crate) packages: BTreeMap<String, Type>,
}

impl Annotations {
    pub(crate) fn limits(&mut self, limits: (usize, usize)) {
        self.limits = limits;
    }
    pub(crate) fn push(&mut self) {
        self.scopes
            .push(self.scopes.last().cloned().unwrap_or_default());
    }
    pub(crate) fn pop(&mut self) {
        self.scopes.pop();
    }

    pub(crate) fn declare(&mut self, statements: &[Stmt]) {
        let mut added = Vec::new();
        for statement in statements {
            if let Stmt::TypeAlias { name, ty } = statement.unspanned() {
                let id = self.aliases.len();
                self.aliases.push(Alias {
                    annotation: ty.clone(),
                    scope: Arc::default(),
                });
                if let Some(scope) = self.scopes.last_mut() {
                    Arc::make_mut(scope).insert(name.clone(), id);
                }
                added.push(id);
            }
        }
        let scope = self.scopes.last().cloned().unwrap_or_default();
        for id in added {
            self.aliases[id].scope = scope.clone();
        }
    }

    pub(crate) fn parameters(&mut self, names: &[String], span: SourceSpan) {
        for name in names {
            let id = self.aliases.len();
            self.aliases.push(Alias {
                annotation: TypeAnnotation {
                    span,
                    shape: TypeShape::Unsupported("a generic type parameter"),
                },
                scope: Arc::default(),
            });
            if let Some(scope) = self.scopes.last_mut() {
                Arc::make_mut(scope).insert(name.clone(), id);
            }
        }
    }

    pub(crate) fn record(&mut self, annotation: &TypeAnnotation) {
        let mut nodes = self.limits.1;
        let result = self.resolve(
            annotation,
            ResolutionScope {
                names: self.scopes.last().map(Arc::as_ref).unwrap_or(&Scope::new()),
                substitutions: &BTreeMap::new(),
            },
            Position::Value,
            &mut Vec::new(),
            &mut nodes,
            self.limits.0,
        );
        self.contexts.insert(
            annotation.span,
            self.scopes.last().cloned().unwrap_or_default(),
        );
        self.resolved.insert(annotation.span, result);
    }

    pub(crate) fn get(&self, annotation: &TypeAnnotation, settled: bool) -> Resolution {
        let mut nodes = self.limits.1;
        let ty = self.resolve(
            annotation,
            ResolutionScope {
                names: &self.scope_of(annotation),
                substitutions: &BTreeMap::new(),
            },
            if settled {
                Position::Result
            } else {
                Position::Parameter
            },
            &mut Vec::new(),
            &mut nodes,
            self.limits.0,
        )?;
        boundary(ty, annotation.span)
    }

    pub(crate) fn fact(&self, annotation: &TypeAnnotation, settled: bool) -> Resolution {
        // A Promise is only a result wrapper. Resolving it as a parameter is
        // refused by the same walk, rather than silently admitting its value.
        if !settled {
            return self
                .resolved
                .get(&annotation.span)
                .cloned()
                .unwrap_or_else(|| {
                    Err((
                        annotation.span,
                        Reason::Unresolved {
                            name: "annotation scope".into(),
                        },
                    ))
                });
        }
        let mut nodes = self.limits.1;
        self.resolve(
            annotation,
            ResolutionScope {
                names: &self.scope_of(annotation),
                substitutions: &BTreeMap::new(),
            },
            Position::Result,
            &mut Vec::new(),
            &mut nodes,
            self.limits.0,
        )
    }

    fn scope_of(&self, annotation: &TypeAnnotation) -> Arc<Scope> {
        self.contexts
            .get(&annotation.span)
            .cloned()
            .unwrap_or_default()
    }

    fn resolve(
        &self,
        annotation: &TypeAnnotation,
        context: ResolutionScope<'_>,
        position: Position,
        stack: &mut Vec<usize>,
        nodes: &mut usize,
        depth: usize,
    ) -> Resolution {
        let ResolutionScope {
            names: scope,
            substitutions,
        } = context;
        let fail = |reason| Err((annotation.span, reason));
        if *nodes == 0 || depth == 0 {
            return fail(Reason::Limit);
        }
        *nodes -= 1;
        let child = |ty: &TypeAnnotation, stack: &mut Vec<usize>, nodes: &mut usize| {
            self.resolve(
                ty,
                ResolutionScope {
                    names: scope,
                    substitutions,
                },
                if position == Position::Parameter {
                    Position::Parameter
                } else {
                    Position::Value
                },
                stack,
                nodes,
                depth - 1,
            )
        };
        Ok(match &annotation.shape {
            TypeShape::Any | TypeShape::Unknown => Type::Json,
            TypeShape::String => Type::Text,
            TypeShape::StringLiteral(value) => Type::Enum(vec![value.clone()]),
            TypeShape::Number => Type::Number,
            TypeShape::BigInt => Type::Int,
            TypeShape::Boolean => Type::Bool,
            TypeShape::Null => Type::Null,
            TypeShape::Undefined => Type::Absent,
            TypeShape::Reference(name) => {
                if let Some(ty) = substitutions.get(name) {
                    return copy_type(ty, nodes, depth).ok_or((annotation.span, Reason::Limit));
                }
                if let Some(&id) = scope.get(name) {
                    if stack.contains(&id) {
                        return fail(Reason::Cycle { name: name.clone() });
                    }
                    stack.push(id);
                    let alias = &self.aliases[id];
                    let result = self.resolve(
                        &alias.annotation,
                        ResolutionScope {
                            names: &alias.scope,
                            substitutions: &BTreeMap::new(),
                        },
                        position,
                        stack,
                        nodes,
                        depth - 1,
                    );
                    stack.pop();
                    result?
                } else if let Some(ty) = self.packages.get(name) {
                    copy_type(ty, nodes, depth).ok_or((annotation.span, Reason::Limit))?
                } else if name == "JsonValue" {
                    Type::Json
                } else {
                    return fail(Reason::Unresolved { name: name.clone() });
                }
            }
            TypeShape::Generic(name, args) => {
                if let Some(&id) = scope.get(name) {
                    if stack.contains(&id) {
                        return fail(Reason::Cycle { name: name.clone() });
                    }
                    let alias = &self.aliases[id];
                    let TypeShape::GenericAlias { parameters, body } = &alias.annotation.shape
                    else {
                        return fail(Reason::Unsupported {
                            construct: "a generic type reference".into(),
                        });
                    };
                    if args.len() > parameters.len() {
                        return fail(Reason::Unsupported {
                            construct: "generic alias arity".into(),
                        });
                    }
                    let mut bindings = BTreeMap::new();
                    for (index, (parameter, default)) in parameters.iter().enumerate() {
                        let ty = match args.get(index) {
                            Some(argument) => child(argument, stack, nodes)?,
                            None => {
                                let Some(default) = default else {
                                    return fail(Reason::Unsupported {
                                        construct: "generic alias arity".into(),
                                    });
                                };
                                self.resolve(
                                    default,
                                    ResolutionScope {
                                        names: &alias.scope,
                                        substitutions: &bindings,
                                    },
                                    Position::Value,
                                    stack,
                                    nodes,
                                    depth - 1,
                                )?
                            }
                        };
                        bindings.insert(parameter.clone(), ty);
                    }
                    stack.push(id);
                    let result = self.resolve(
                        body,
                        ResolutionScope {
                            names: &alias.scope,
                            substitutions: &bindings,
                        },
                        position,
                        stack,
                        nodes,
                        depth - 1,
                    );
                    stack.pop();
                    return result;
                }
                match (name.as_str(), args.as_slice()) {
                    ("Promise", [result]) if position == Position::Result => self.resolve(
                        result,
                        ResolutionScope {
                            names: scope,
                            substitutions,
                        },
                        Position::Result,
                        stack,
                        nodes,
                        depth - 1,
                    )?,
                    ("Array" | "ReadonlyArray", [element]) => {
                        Type::List(Box::new(child(element, stack, nodes)?))
                    }
                    ("Record", [key, value]) if matches!(child(key, stack, nodes)?, Type::Text) => {
                        Type::Record(RecordType {
                            fields: Vec::new(),
                            rest: lash_vocabulary::RecordRest::Keep(Box::new(child(
                                value, stack, nodes,
                            )?)),
                        })
                    }
                    _ => {
                        return fail(Reason::Unsupported {
                            construct: "a generic type reference".into(),
                        });
                    }
                }
            }
            TypeShape::GenericAlias { parameters, body } => {
                let mut bindings = BTreeMap::new();
                for (name, default) in parameters {
                    let Some(default) = default else {
                        return fail(Reason::Unsupported {
                            construct: "a generic type alias".into(),
                        });
                    };
                    let ty = self.resolve(
                        default,
                        ResolutionScope {
                            names: scope,
                            substitutions: &bindings,
                        },
                        Position::Value,
                        stack,
                        nodes,
                        depth - 1,
                    )?;
                    bindings.insert(name.clone(), ty);
                }
                self.resolve(
                    body,
                    ResolutionScope {
                        names: scope,
                        substitutions: &bindings,
                    },
                    position,
                    stack,
                    nodes,
                    depth - 1,
                )?
            }
            TypeShape::OptionalTupleElement(_) => {
                return fail(Reason::Unsupported {
                    construct: "an optional type outside a tuple".into(),
                });
            }
            TypeShape::Tuple(elements) => {
                let mut prefix = Vec::new();
                let mut alternatives = Vec::new();
                for element in elements {
                    let element = if let TypeShape::OptionalTupleElement(element) = &element.shape {
                        alternatives.push(
                            copy_type(&Type::Tuple(prefix.clone()), nodes, depth)
                                .ok_or((annotation.span, Reason::Limit))?,
                        );
                        element.as_ref()
                    } else {
                        element
                    };
                    prefix.push(child(element, stack, nodes)?);
                }
                alternatives.push(Type::Tuple(prefix));
                let ty = if alternatives.len() == 1 {
                    alternatives.remove(0)
                } else {
                    Type::Union(alternatives)
                };
                copy_type(&ty, nodes, depth).ok_or((annotation.span, Reason::Limit))?
            }
            TypeShape::Intersection(elements) => {
                let mut result = Type::Json;
                for element in elements {
                    result = intersection(
                        result,
                        self.resolve(
                            element,
                            ResolutionScope {
                                names: scope,
                                substitutions,
                            },
                            position,
                            stack,
                            nodes,
                            depth - 1,
                        )?,
                        nodes,
                        depth,
                    )
                    .ok_or_else(|| {
                        (
                            annotation.span,
                            if *nodes == 0 {
                                Reason::Limit
                            } else {
                                Reason::Unsupported {
                                    construct: "an unrepresentable intersection".into(),
                                }
                            },
                        )
                    })?;
                }
                result =
                    copy_type(&result, nodes, depth).ok_or((annotation.span, Reason::Limit))?;
                result
            }
            TypeShape::Array(element) => Type::List(Box::new(child(element, stack, nodes)?)),
            TypeShape::Object(fields, rest) => Type::Record(RecordType {
                fields: fields
                    .iter()
                    .map(|field| {
                        Ok(RecordTypeField {
                            name: field.name.clone(),
                            optional: field.optional,
                            ty: child(&field.ty, stack, nodes)?,
                        })
                    })
                    .collect::<Result<_, (SourceSpan, Reason)>>()?,
                rest: match rest.as_deref() {
                    Some(rest) if matches!(rest.shape, TypeShape::Never) => {
                        lash_vocabulary::RecordRest::Closed
                    }
                    Some(rest) => {
                        lash_vocabulary::RecordRest::Keep(Box::new(child(rest, stack, nodes)?))
                    }
                    None if position == Position::Parameter => lash_vocabulary::RecordRest::Drop,
                    None => lash_vocabulary::RecordRest::Closed,
                },
            }),
            TypeShape::Union(members) => {
                let mut types = Vec::new();
                let mut literals = Vec::new();
                for member in members {
                    match child(member, stack, nodes)? {
                        Type::Enum(values) => {
                            for value in values {
                                if !literals.contains(&value) {
                                    literals.push(value);
                                }
                            }
                        }
                        Type::Union(inner) => types.extend(inner),
                        other => types.push(other),
                    }
                }
                if !literals.is_empty() {
                    types.insert(0, Type::Enum(literals));
                }
                types.dedup();
                if types.len() == 1 {
                    types.remove(0)
                } else {
                    Type::Union(types)
                }
            }
            TypeShape::Never => {
                return fail(Reason::Unsupported {
                    construct: "the `never` type".into(),
                });
            }
            TypeShape::Unsupported(construct) => {
                return fail(Reason::Unsupported {
                    construct: (*construct).into(),
                });
            }
            TypeShape::Function(result) => Type::Function(Box::new(lash_vocabulary::Signature {
                params: Vec::new(),
                result: self.resolve(
                    result,
                    ResolutionScope {
                        names: scope,
                        substitutions,
                    },
                    Position::Result,
                    stack,
                    nodes,
                    depth - 1,
                )?,
            })),
        })
    }
}

/// Apply the JSON boundary's absence convention after resolving source facts.
fn boundary(ty: Type, span: SourceSpan) -> Resolution {
    Ok(match ty {
        Type::Absent => Type::Union(vec![Type::Null, Type::Absent]),
        Type::Function(_) => {
            return Err((
                span,
                Reason::Unsupported {
                    construct: "a callable type".into(),
                },
            ));
        }
        Type::Tuple(elements) => Type::Tuple(
            elements
                .into_iter()
                .map(|element| boundary(element, span))
                .collect::<Result<_, _>>()?,
        ),
        Type::List(element) => Type::List(Box::new(boundary(*element, span)?)),
        Type::Record(mut record) => {
            for field in &mut record.fields {
                field.ty = boundary(std::mem::replace(&mut field.ty, Type::Json), span)?;
            }
            if let lash_vocabulary::RecordRest::Keep(rest) = record.rest {
                record.rest = lash_vocabulary::RecordRest::Keep(Box::new(boundary(*rest, span)?));
            }
            Type::Record(record)
        }
        Type::Union(members) => {
            let mut types = Vec::new();
            for member in members {
                match boundary(member, span)? {
                    Type::Union(inner) => types.extend(inner),
                    other => types.push(other),
                }
            }
            let mut distinct = Vec::new();
            for ty in types {
                if !distinct.contains(&ty) {
                    distinct.push(ty);
                }
            }
            if distinct.len() == 1 {
                distinct.remove(0)
            } else {
                Type::Union(distinct)
            }
        }
        other => other,
    })
}

/// A structural meet keeps every field, combining repeated field contracts.
fn intersection(left: Type, right: Type, nodes: &mut usize, depth: usize) -> Option<Type> {
    use lash_vocabulary::RecordRest;
    if depth == 0 || *nodes == 0 {
        *nodes = 0;
        return None;
    }
    *nodes -= 1;
    let result = match (left, right) {
        (left, right) if left == right => left,
        (Type::Any | Type::Json, other) | (other, Type::Any | Type::Json) => other,
        (Type::Number, other @ (Type::Int | Type::Float))
        | (other @ (Type::Int | Type::Float), Type::Number) => other,
        (Type::Text, other @ Type::Enum(_)) | (other @ Type::Enum(_), Type::Text) => other,
        (Type::Enum(left), Type::Enum(right)) => {
            let values: Vec<_> = left
                .into_iter()
                .filter(|value| right.contains(value))
                .collect();
            if values.is_empty() {
                return None;
            }
            Type::Enum(values)
        }
        (Type::Union(members), other) | (other, Type::Union(members)) => {
            let mut met = Vec::new();
            for member in members {
                if let Some(member) = intersection(member, other.clone(), nodes, depth - 1) {
                    let members = match member {
                        Type::Union(members) => members,
                        member => vec![member],
                    };
                    for member in members {
                        if !met.contains(&member) {
                            met.push(member);
                        }
                    }
                }
                if *nodes == 0 {
                    return None;
                }
            }
            match met.len() {
                0 => return None,
                1 => met.remove(0),
                _ => Type::Union(met),
            }
        }
        (Type::Record(mut left), Type::Record(right)) => {
            if let RecordRest::Keep(rest) = &right.rest {
                for field in &mut left.fields {
                    if !right.fields.iter().any(|other| other.name == field.name) {
                        field.ty = intersection(field.ty.clone(), *rest.clone(), nodes, depth - 1)?;
                    }
                }
            }
            for mut field in right.fields {
                if !left.fields.iter().any(|other| other.name == field.name)
                    && let RecordRest::Keep(rest) = &left.rest
                {
                    field.ty = intersection(field.ty, *rest.clone(), nodes, depth - 1)?;
                }
                if let Some(earlier) = left
                    .fields
                    .iter_mut()
                    .find(|earlier| earlier.name == field.name)
                {
                    earlier.ty = intersection(earlier.ty.clone(), field.ty, nodes, depth - 1)?;
                    earlier.optional &= field.optional;
                } else {
                    left.fields.push(field);
                }
            }
            left.rest = match (left.rest, right.rest) {
                (RecordRest::Keep(left), RecordRest::Keep(right)) => {
                    RecordRest::Keep(Box::new(intersection(*left, *right, nodes, depth - 1)?))
                }
                (RecordRest::Keep(rest), _) | (_, RecordRest::Keep(rest)) => RecordRest::Keep(rest),
                (RecordRest::Drop, RecordRest::Drop) => RecordRest::Drop,
                _ => RecordRest::Closed,
            };
            Type::Record(left)
        }
        (Type::Tuple(left), Type::Tuple(right)) if left.len() == right.len() => Type::Tuple(
            left.into_iter()
                .zip(right)
                .map(|(left, right)| intersection(left, right, nodes, depth - 1))
                .collect::<Option<_>>()?,
        ),
        (Type::List(left), Type::List(right)) => {
            Type::List(Box::new(intersection(*left, *right, nodes, depth - 1)?))
        }
        _ => return None,
    };
    copy_type(&result, nodes, depth)
}

/// Charge expanded nodes and depth before cloning an instantiated argument.
fn copy_type(ty: &Type, nodes: &mut usize, depth: usize) -> Option<Type> {
    let mut pending = vec![(ty, depth)];
    while let Some((ty, depth)) = pending.pop() {
        if depth == 0 || *nodes == 0 {
            *nodes = 0;
            return None;
        }
        *nodes -= 1;
        let mut add = |ty| pending.push((ty, depth - 1));
        match ty {
            Type::Tuple(types) | Type::Union(types) => {
                for ty in types {
                    add(ty);
                }
            }
            Type::List(ty) | Type::Set(ty) | Type::Task(ty) => add(ty),
            Type::Map(map) => {
                add(&map.key);
                add(&map.value);
            }
            Type::Record(record) => {
                for field in &record.fields {
                    add(&field.ty);
                }
                if let lash_vocabulary::RecordRest::Keep(rest) = &record.rest {
                    add(rest);
                }
            }
            Type::Function(signature) => {
                for param in &signature.params {
                    add(&param.ty);
                }
                add(&signature.result);
            }
            _ => {}
        }
    }
    Some(ty.clone())
}
