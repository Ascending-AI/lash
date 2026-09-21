//! Declared TypeScript types, carried from a parameter annotation to the
//! process signature.
//!
//! A process-literal parameter is the one place where an annotation is
//! load-bearing rather than decorative: it is the process's durable input
//! shape, and a trigger registration is checked against it before any
//! foreground effect runs (FIG-3071). Everywhere else the dialect stays
//! structurally typed and an annotation is ignored, so this module only
//! *records* what was written and never refuses on its own — the refusal is
//! raised at the process, where the parameter has a name to blame.

use swc_ecma_ast as swc;

use crate::SourceSpan;

use super::rejections::source_span;

/// One written type annotation, with the source extent it was written at so a
/// refusal can point at it.
#[derive(Clone, Debug)]
pub(crate) struct TypeAnnotation {
    pub(crate) span: SourceSpan,
    pub(crate) shape: TypeShape,
}

/// The durable-representable subset of TypeScript's type syntax, plus one leaf
/// for everything outside it.
#[derive(Clone, Debug)]
pub(crate) enum TypeShape {
    /// `any` and `unknown`: the gradual type an unannotated parameter already
    /// lowers to, so writing it changes nothing.
    Unknown,
    String,
    Number,
    Boolean,
    Null,
    /// `undefined` and `void`. The runtime's type language has no separate
    /// absent type, so both land on `null` — the shape a missing durable input
    /// arrives as.
    Undefined,
    StringLiteral(String),
    Array(Box<TypeAnnotation>),
    Object(Vec<TypeAnnotationField>),
    Union(Vec<TypeAnnotation>),
    /// A named type: a local `type` alias or a host named-data type such as
    /// `timer.Tick`. It is resolved against the artifact's alias table and the
    /// host catalog at registration, not here.
    Reference(String),
    /// A construct with no durable representation, carrying the words the
    /// refusal names it with.
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
            Some(fields) => TypeShape::Object(fields),
            None => TypeShape::Unsupported("an object type with a non-property member"),
        },
        swc::TsType::TsUnionOrIntersectionType(ty) => match ty {
            swc::TsUnionOrIntersectionType::TsUnionType(union) => {
                TypeShape::Union(union.types.iter().map(|ty| convert_type(ty)).collect())
            }
            swc::TsUnionOrIntersectionType::TsIntersectionType(_) => {
                TypeShape::Unsupported("an intersection type")
            }
        },
        swc::TsType::TsTypeRef(reference) => reference_shape(reference),
        swc::TsType::TsFnOrConstructorType(_) => TypeShape::Unsupported("a function type"),
        swc::TsType::TsTupleType(_) => TypeShape::Unsupported("a tuple type"),
        swc::TsType::TsMappedType(_) => TypeShape::Unsupported("a mapped type"),
        swc::TsType::TsConditionalType(_) => TypeShape::Unsupported("a conditional type"),
        swc::TsType::TsIndexedAccessType(_) => TypeShape::Unsupported("an indexed access type"),
        swc::TsType::TsTypeOperator(_) => TypeShape::Unsupported("a type operator"),
        swc::TsType::TsTypeQuery(_) => TypeShape::Unsupported("a `typeof` type"),
        swc::TsType::TsImportType(_) => TypeShape::Unsupported("an imported type"),
        swc::TsType::TsTypePredicate(_) => TypeShape::Unsupported("a type predicate"),
        swc::TsType::TsThisType(_) => TypeShape::Unsupported("a `this` type"),
        swc::TsType::TsInferType(_) => TypeShape::Unsupported("an `infer` type"),
        swc::TsType::TsOptionalType(_) | swc::TsType::TsRestType(_) => {
            TypeShape::Unsupported("a tuple element modifier")
        }
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
        swc::TsKeywordTypeKind::TsAnyKeyword | swc::TsKeywordTypeKind::TsUnknownKeyword => {
            TypeShape::Unknown
        }
        swc::TsKeywordTypeKind::TsBigIntKeyword => TypeShape::Unsupported("a `bigint` type"),
        swc::TsKeywordTypeKind::TsSymbolKeyword => TypeShape::Unsupported("a `symbol` type"),
        swc::TsKeywordTypeKind::TsObjectKeyword => TypeShape::Unsupported("the `object` type"),
        swc::TsKeywordTypeKind::TsNeverKeyword => TypeShape::Unsupported("the `never` type"),
        swc::TsKeywordTypeKind::TsIntrinsicKeyword => TypeShape::Unsupported("an intrinsic type"),
    }
}

fn object_shape(literal: &swc::TsTypeLit) -> Option<Vec<TypeAnnotationField>> {
    let mut fields = Vec::with_capacity(literal.members.len());
    for member in &literal.members {
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
                shape: TypeShape::Unknown,
            },
        };
        fields.push(TypeAnnotationField {
            name,
            optional: property.optional,
            ty,
        });
    }
    Some(fields)
}

/// `Array<T>` is the one generic the durable subset has; every other type
/// argument list names a shape the runtime cannot carry.
fn reference_shape(reference: &swc::TsTypeRef) -> TypeShape {
    let Some(name) = entity_name(&reference.type_name) else {
        return TypeShape::Unsupported("a qualified type reference");
    };
    match (&reference.type_params, name.as_str()) {
        (None, _) => TypeShape::Reference(name),
        (Some(params), "Array" | "ReadonlyArray") if params.params.len() == 1 => {
            TypeShape::Array(Box::new(convert_type(&params.params[0])))
        }
        (Some(_), _) => TypeShape::Unsupported("a generic type reference"),
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
