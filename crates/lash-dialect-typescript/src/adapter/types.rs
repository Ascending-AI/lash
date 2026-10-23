//! Declared TypeScript types, as written.
//!
//! This module only *records* an annotation and never refuses on its own:
//! the type analysis decides what a written type lets the lowerer believe,
//! and a shape it cannot use is one it believes nothing about.

use swc_ecma_ast as swc;

use crate::SourceSpan;

use super::Expr;

use super::rejections::source_span;

/// One written type annotation, with the source extent it was written at so a
/// refusal can point at it.
#[derive(Clone, Debug)]
pub(crate) struct TypeAnnotation {
    pub(crate) span: SourceSpan,
    pub(crate) shape: TypeShape,
}

/// The modeled TypeScript type syntax, including function annotations that
/// cannot cross a durable entry boundary, plus one leaf for everything outside it.
#[derive(Clone, Debug)]
pub(crate) enum TypeShape {
    /// Explicit unconstrained annotations, distinct from an absent annotation.
    Any,
    Unknown,
    Never,
    String,
    Number,
    BigInt,
    Boolean,
    Null,
    /// `undefined` and `void`. Source facts retain absence; the entry
    /// boundary separately records the convention's null normalization.
    Undefined,
    /// A string literal retains its exact kernel Enum member.
    StringLiteral(String),
    Array(Box<TypeAnnotation>),
    Object(Vec<TypeAnnotationField>, Option<Box<TypeAnnotation>>),
    Generic(String, Vec<TypeAnnotation>),
    Union(Vec<TypeAnnotation>),
    Tuple(Vec<TypeAnnotation>),
    OptionalTupleElement(Box<TypeAnnotation>),
    Intersection(Vec<TypeAnnotation>),
    GenericAlias {
        parameters: Vec<(String, Option<TypeAnnotation>)>,
        body: Box<TypeAnnotation>,
    },
    Function(Box<TypeAnnotation>),
    /// A named type, resolved by declaration identity in the source scope.
    Reference(String),
    /// A construct the type analysis believes nothing about, carrying the
    /// words that name it.
    Unsupported(&'static str),
}

#[derive(Clone, Debug)]
pub(crate) struct TypeAnnotationField {
    pub(crate) name: String,
    pub(crate) optional: bool,
    pub(crate) ty: TypeAnnotation,
}

/// Total by construction: anything outside the durable subset becomes
/// [`TypeShape::Unsupported`] rather than an error, because a non-process function may legally
/// carry it.
pub(crate) fn convert_type(ty: &swc::TsType) -> TypeAnnotation {
    let span = source_span(swc_common::Spanned::span(ty));
    let shape = match ty {
        swc::TsType::TsKeywordType(keyword) => keyword_shape(keyword.kind),
        swc::TsType::TsParenthesizedType(inner) => return convert_type(&inner.type_ann),
        swc::TsType::TsArrayType(array) => {
            TypeShape::Array(Box::new(convert_type(&array.elem_type)))
        }
        swc::TsType::TsLitType(literal) => match &literal.lit {
            swc::TsLit::Str(value) => {
                TypeShape::StringLiteral(value.value.to_string_lossy().into_owned())
            }
            swc::TsLit::Bool(_) => TypeShape::Boolean,
            _ => TypeShape::Unsupported("a numeric or template literal type"),
        },
        swc::TsType::TsTypeLit(literal) => match object_shape(literal) {
            Some((fields, rest)) => TypeShape::Object(fields, rest),
            None => TypeShape::Unsupported("an object type with a non-property member"),
        },
        swc::TsType::TsUnionOrIntersectionType(ty) => match ty {
            swc::TsUnionOrIntersectionType::TsUnionType(union) => {
                TypeShape::Union(union.types.iter().map(|ty| convert_type(ty)).collect())
            }
            swc::TsUnionOrIntersectionType::TsIntersectionType(intersection) => {
                TypeShape::Intersection(
                    intersection
                        .types
                        .iter()
                        .map(|ty| convert_type(ty))
                        .collect(),
                )
            }
        },
        swc::TsType::TsTypeRef(reference) => reference_shape(reference),
        swc::TsType::TsFnOrConstructorType(function) => match function {
            swc::TsFnOrConstructorType::TsFnType(function) => {
                TypeShape::Function(Box::new(convert_type(&function.type_ann.type_ann)))
            }
            swc::TsFnOrConstructorType::TsConstructorType(_) => {
                TypeShape::Unsupported("a constructor type")
            }
        },
        swc::TsType::TsTupleType(tuple) => TypeShape::Tuple(
            tuple
                .elem_types
                .iter()
                .map(|element| convert_type(&element.ty))
                .collect(),
        ),
        swc::TsType::TsMappedType(_) => TypeShape::Unsupported("a mapped type"),
        swc::TsType::TsConditionalType(_) => TypeShape::Unsupported("a conditional type"),
        swc::TsType::TsIndexedAccessType(_) => TypeShape::Unsupported("an indexed access type"),
        swc::TsType::TsTypeOperator(operator) if operator.op == swc::TsTypeOperatorOp::ReadOnly => {
            return convert_type(&operator.type_ann);
        }
        swc::TsType::TsTypeOperator(_) => TypeShape::Unsupported("a type operator"),
        swc::TsType::TsTypeQuery(_) => TypeShape::Unsupported("a `typeof` type"),
        swc::TsType::TsImportType(_) => TypeShape::Unsupported("an imported type"),
        swc::TsType::TsTypePredicate(_) => TypeShape::Unsupported("a type predicate"),
        swc::TsType::TsThisType(_) => TypeShape::Unsupported("a `this` type"),
        swc::TsType::TsInferType(_) => TypeShape::Unsupported("an `infer` type"),
        swc::TsType::TsOptionalType(element) => {
            TypeShape::OptionalTupleElement(Box::new(convert_type(&element.type_ann)))
        }
        swc::TsType::TsRestType(_) => TypeShape::Unsupported("an unbounded tuple rest element"),
    };
    TypeAnnotation { span, shape }
}

fn keyword_shape(kind: swc::TsKeywordTypeKind) -> TypeShape {
    match kind {
        swc::TsKeywordTypeKind::TsStringKeyword => TypeShape::String,
        swc::TsKeywordTypeKind::TsNumberKeyword => TypeShape::Number,
        swc::TsKeywordTypeKind::TsBooleanKeyword => TypeShape::Boolean,
        swc::TsKeywordTypeKind::TsNullKeyword => TypeShape::Null,
        swc::TsKeywordTypeKind::TsUndefinedKeyword | swc::TsKeywordTypeKind::TsVoidKeyword => {
            TypeShape::Undefined
        }
        swc::TsKeywordTypeKind::TsAnyKeyword => TypeShape::Any,
        swc::TsKeywordTypeKind::TsUnknownKeyword => TypeShape::Unknown,
        swc::TsKeywordTypeKind::TsBigIntKeyword => TypeShape::BigInt,
        swc::TsKeywordTypeKind::TsSymbolKeyword => TypeShape::Unsupported("a `symbol` type"),
        swc::TsKeywordTypeKind::TsObjectKeyword => TypeShape::Unsupported("the `object` type"),
        swc::TsKeywordTypeKind::TsNeverKeyword => TypeShape::Never,
        swc::TsKeywordTypeKind::TsIntrinsicKeyword => TypeShape::Unsupported("an intrinsic type"),
    }
}

fn object_shape(
    literal: &swc::TsTypeLit,
) -> Option<(Vec<TypeAnnotationField>, Option<Box<TypeAnnotation>>)> {
    let mut rest = None;
    let mut fields = Vec::with_capacity(literal.members.len());
    for member in &literal.members {
        if let swc::TsTypeElement::TsIndexSignature(index) = member {
            if rest.is_some() || index.params.len() != 1 {
                return None;
            }
            let swc::TsFnParam::Ident(key) = &index.params[0] else {
                return None;
            };
            if !matches!(key.type_ann.as_ref()?.type_ann.as_ref(), swc::TsType::TsKeywordType(keyword) if keyword.kind == swc::TsKeywordTypeKind::TsStringKeyword)
            {
                return None;
            }
            rest = Some(Box::new(convert_type(&index.type_ann.as_ref()?.type_ann)));
            continue;
        }
        let swc::TsTypeElement::TsPropertySignature(property) = member else {
            return None;
        };
        let name = match property.key.as_ref() {
            swc::Expr::Ident(name) => name.sym.to_string(),
            swc::Expr::Lit(swc::Lit::Str(name)) => name.value.to_string_lossy().into_owned(),
            _ => return None,
        };
        let ty = match &property.type_ann {
            Some(annotation) => convert_type(&annotation.type_ann),
            None => TypeAnnotation {
                span: source_span(property.span),
                shape: TypeShape::Unsupported("a property without an annotation"),
            },
        };
        fields.push(TypeAnnotationField {
            name,
            optional: property.optional,
            ty,
        });
    }
    Some((fields, rest))
}

/// References retain their qualified spelling and authored type arguments.
fn reference_shape(reference: &swc::TsTypeRef) -> TypeShape {
    let Some(name) = entity_name(&reference.type_name) else {
        return TypeShape::Unsupported("a qualified type reference");
    };
    match &reference.type_params {
        None => TypeShape::Reference(name),
        Some(params) => TypeShape::Generic(
            name,
            params.params.iter().map(|ty| convert_type(ty)).collect(),
        ),
    }
}

fn entity_name(name: &swc::TsEntityName) -> Option<String> {
    match name {
        swc::TsEntityName::Ident(name) => Some(name.sym.to_string()),
        swc::TsEntityName::TsQualifiedName(qualified) => Some(format!(
            "{}.{}",
            entity_name(&qualified.left)?,
            qualified.right.sym
        )),
    }
}

/// `T | undefined`: what an optional parameter or property holds.
fn or_undefined(ty: TypeAnnotation) -> TypeAnnotation {
    let span = ty.span;
    let undefined = TypeAnnotation {
        span,
        shape: TypeShape::Undefined,
    };
    TypeAnnotation {
        span,
        shape: TypeShape::Union(vec![ty, undefined]),
    }
}

fn unsupported(span: swc_common::Span, what: &'static str) -> TypeAnnotation {
    TypeAnnotation {
        span: source_span(span),
        shape: TypeShape::Unsupported(what),
    }
}

/// Interfaces combine their admitted bases with their own declared fields.
pub(crate) fn convert_interface(interface: &swc::TsInterfaceDecl) -> TypeAnnotation {
    let literal = swc::TsTypeLit {
        span: interface.body.span,
        members: interface.body.body.clone(),
    };
    let shape = match object_shape(&literal) {
        Some((fields, rest)) => TypeShape::Object(fields, rest),
        None => TypeShape::Unsupported("an object type with a non-property member"),
    };
    let mut parts = Vec::new();
    for base in &interface.extends {
        fn path(expr: &swc::Expr) -> Option<String> {
            match expr {
                swc::Expr::Ident(name) => Some(name.sym.to_string()),
                swc::Expr::Member(member) => match &member.prop {
                    swc::MemberProp::Ident(name) => {
                        Some(format!("{}.{}", path(&member.obj)?, name.sym))
                    }
                    _ => None,
                },
                _ => None,
            }
        }
        let Some(name) = path(&base.expr) else {
            return unsupported(interface.span, "a non-static interface base");
        };
        let shape = match &base.type_args {
            Some(args) => TypeShape::Generic(
                name,
                args.params.iter().map(|ty| convert_type(ty)).collect(),
            ),
            None => TypeShape::Reference(name),
        };
        parts.push(TypeAnnotation {
            span: source_span(base.span),
            shape,
        });
    }
    let mut body = TypeAnnotation {
        span: source_span(interface.span),
        shape,
    };
    if !parts.is_empty() {
        parts.push(body);
        body = TypeAnnotation {
            span: source_span(interface.span),
            shape: TypeShape::Intersection(parts),
        };
    }
    generic_declaration(body, interface.type_params.as_deref(), interface.span)
}

/// Alias parameters and defaults retain declaration scope for bounded substitution.
pub(crate) fn convert_alias(alias: &swc::TsTypeAliasDecl) -> TypeAnnotation {
    generic_declaration(
        convert_type(&alias.type_ann),
        alias.type_params.as_deref(),
        alias.span,
    )
}

fn generic_declaration(
    body: TypeAnnotation,
    parameters: Option<&swc::TsTypeParamDecl>,
    span: swc_common::Span,
) -> TypeAnnotation {
    match parameters {
        None => body,
        Some(parameters) => TypeAnnotation {
            span: source_span(span),
            shape: TypeShape::GenericAlias {
                parameters: parameters
                    .params
                    .iter()
                    .map(|parameter| {
                        (
                            parameter.name.sym.to_string(),
                            parameter.default.as_ref().map(|ty| convert_type(ty)),
                        )
                    })
                    .collect(),
                body: Box::new(body),
            },
        },
    }
}

/// The annotation on a bound name. An optional parameter's is its type or
/// `undefined`.
pub(crate) fn convert_binding(name: &swc::BindingIdent) -> Option<TypeAnnotation> {
    let ty = convert_type(&name.type_ann.as_ref()?.type_ann);
    Some(if name.id.optional {
        or_undefined(ty)
    } else {
        ty
    })
}

/// An expression without the type assertions around it: an assertion
/// changes what a value is believed to be, never what a target is.
pub(crate) fn unasserted(expr: Expr) -> Expr {
    match expr {
        Expr::As { value, .. } => unasserted(*value),
        other => other,
    }
}
