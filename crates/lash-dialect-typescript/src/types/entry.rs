//! Bounded annotation resolution. Alias identities retain their declaring scope;
//! forward references and nested shadowing never depend on statement order.
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_vocabulary::{AnnotationResolutionReason as Reason, RecordType, RecordTypeField, Type};

use crate::SourceSpan;
use crate::adapter::{Stmt, TypeAnnotation, TypeShape};

type Scope = BTreeMap<String, usize>;
type Resolution = Result<Type, (SourceSpan, Reason)>;

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
            self.scopes.last().map(Arc::as_ref).unwrap_or(&Scope::new()),
            false,
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
        boundary(self.fact(annotation, settled)?, annotation.span)
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
            &self.scope_of(annotation),
            true,
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
        scope: &Scope,
        settled: bool,
        stack: &mut Vec<usize>,
        nodes: &mut usize,
        depth: usize,
    ) -> Resolution {
        let fail = |reason| Err((annotation.span, reason));
        if *nodes == 0 || depth == 0 {
            return fail(Reason::Limit);
        }
        *nodes -= 1;
        let child = |ty: &TypeAnnotation, stack: &mut Vec<usize>, nodes: &mut usize| {
            self.resolve(ty, scope, false, stack, nodes, depth - 1)
        };
        Ok(match &annotation.shape {
            TypeShape::Any | TypeShape::Unknown => Type::Json,
            TypeShape::String => Type::Text,
            TypeShape::StringLiteral(value) => Type::Enum(vec![value.clone()]),
            TypeShape::Number => Type::Number,
            TypeShape::Boolean => Type::Bool,
            TypeShape::Null => Type::Null,
            TypeShape::Undefined => Type::Absent,
            TypeShape::Reference(name) => {
                if let Some(&id) = scope.get(name) {
                    if stack.contains(&id) {
                        return fail(Reason::Cycle { name: name.clone() });
                    }
                    stack.push(id);
                    let alias = &self.aliases[id];
                    let result = self.resolve(
                        &alias.annotation,
                        &alias.scope,
                        settled,
                        stack,
                        nodes,
                        depth - 1,
                    );
                    stack.pop();
                    result?
                } else if name == "JsonValue" {
                    Type::Json
                } else {
                    return fail(Reason::Unresolved { name: name.clone() });
                }
            }
            TypeShape::Generic(name, args) => {
                if scope.contains_key(name) {
                    return fail(Reason::Unsupported {
                        construct: "a generic type reference".into(),
                    });
                }
                match (name.as_str(), args.as_slice()) {
                    ("Promise", [result]) if settled => {
                        self.resolve(result, scope, true, stack, nodes, depth - 1)?
                    }
                    ("Array" | "ReadonlyArray", [element]) => {
                        Type::List(Box::new(child(element, stack, nodes)?))
                    }
                    ("Record", [key, value]) if matches!(child(key, stack, nodes)?, Type::Text) => {
                        Type::Record(RecordType {
                            fields: Vec::new(),
                            rest: Some(Box::new(child(value, stack, nodes)?)),
                        })
                    }
                    _ => {
                        return fail(Reason::Unsupported {
                            construct: "a generic type reference".into(),
                        });
                    }
                }
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
                rest: rest
                    .as_ref()
                    .map(|rest| child(rest, stack, nodes).map(Box::new))
                    .transpose()?,
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
            TypeShape::Unsupported(construct) => {
                return fail(Reason::Unsupported {
                    construct: (*construct).into(),
                });
            }
            TypeShape::Function(result) => Type::Function(Box::new(lash_vocabulary::Signature {
                params: Vec::new(),
                result: self.resolve(result, scope, true, stack, nodes, depth - 1)?,
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
        Type::List(element) => Type::List(Box::new(boundary(*element, span)?)),
        Type::Record(mut record) => {
            for field in &mut record.fields {
                field.ty = boundary(std::mem::replace(&mut field.ty, Type::Json), span)?;
            }
            record.rest = record
                .rest
                .map(|rest| boundary(*rest, span).map(Box::new))
                .transpose()?;
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
