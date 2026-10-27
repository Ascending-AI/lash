//! FIG-7139: inspect implicit conversions before their static facts disappear.
use super::{Lowerer, Lowering, Operand, Ty};
use crate::adapter::BinaryOp;
use crate::{Diagnostic, SourceSpan};
use lash_kernel_dialect::{
    SourceOperandType as Static, workflow::ImplicitTextConversionPolicy as Policy,
};

impl Lowerer<'_> {
    /// Keep admission temporaries out of the recursively lowered operand frames.
    pub(super) fn binary_with_admission(
        &mut self,
        op: BinaryOp,
        left: Operand,
        right: Operand,
        spans: Option<[SourceSpan; 2]>,
    ) -> Lowering<Operand> {
        if op == BinaryOp::Add {
            if left.ty == Ty::Text {
                self.check_implicit_text(&right.ty, spans.map(|spans| spans[1]))?;
            }
            if right.ty == Ty::Text {
                self.check_implicit_text(&left.ty, spans.map(|spans| spans[0]))?;
            }
        }
        self.binary(op, left, right)
    }

    pub(super) fn check_implicit_text(
        &mut self,
        ty: &Ty,
        span: Option<SourceSpan>,
    ) -> Lowering<()> {
        if *ty == Ty::Never {
            return Ok(());
        }
        let fact = lash_kernel_doc::ImplicitTextConversion {
            span,
            operand_type: static_type(ty),
        };
        // Primitive conversions cannot be refused by any policy.
        if !fact.refused(true) {
            return Ok(());
        }
        for frame in &mut self.functions {
            frame.implicit_text_conversions.push(fact.clone());
        }
        if self.implicit_text_conversion == Policy::Allow
            || !fact.refused(self.implicit_text_conversion == Policy::Strict)
        {
            return Ok(());
        }
        Err(Diagnostic {
            reason: lash_kernel_dialect::DiagnosticReason::ImplicitTextConversion(Box::new(fact)),
            message: "implicit text conversion refused by source admission policy".into(),
            suggestions: Vec::new(),
        })
    }
}

fn static_type(ty: &Ty) -> Static {
    match ty {
        Ty::Unknown | Ty::Never | Ty::BigInt | Ty::Builtin => Static::Any,
        Ty::Undefined => Static::Undefined,
        Ty::Null => Static::Null,
        Ty::Bool => Static::Boolean,
        Ty::Float | Ty::Number => Static::Number,
        Ty::Text => Static::Text,
        Ty::List(element) => Static::List(Box::new(static_type(element))),
        Ty::Record(fields) => Static::Record(
            fields
                .iter()
                .map(|(name, ty)| (name.clone(), static_type(ty)))
                .collect(),
        ),
        Ty::Object => Static::Object,
        Ty::Array => Static::Array,
        Ty::Map => Static::Map,
        Ty::Set => Static::Set,
        Ty::Function(result) | Ty::Callable(result) => Static::Function {
            result: Box::new(static_type(result)),
        },
        Ty::Union(parts) => Static::Union(parts.iter().map(static_type).collect()),
        Ty::Package(brand) => Static::Package {
            package: brand.package_text(),
            name: brand.ty.as_str().to_owned(),
        },
    }
}
