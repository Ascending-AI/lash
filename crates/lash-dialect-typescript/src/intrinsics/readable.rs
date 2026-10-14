//! Direct inverses for generated_rules.md; never ordinary authored lowering.
use super::{Reader, array, call_arguments, invalid};
use crate::Diagnostic;
use lash_kernel_doc as k;
use std::collections::BTreeSet;
use swc_common::Spanned;
use swc_ecma_ast as s;

impl Reader {
    pub(super) fn readable_expression(
        &mut self,
        expr: &s::Expr,
    ) -> Option<Result<k::Expr, Diagnostic>> {
        match expr {
            s::Expr::Lit(s::Lit::Num(number)) => {
                Some(native_number(number, false).map(k::Expr::Literal))
            }
            s::Expr::Unary(unary) if unary.op == s::UnaryOp::Minus => {
                let s::Expr::Lit(s::Lit::Num(number)) = unary.arg.as_ref() else {
                    return Some(Err(invalid(
                        "native minus needs a numeric literal",
                        Some(expr.span()),
                    )));
                };
                Some(native_number(number, true).map(k::Expr::Literal))
            }
            s::Expr::Array(_) => Some(array(expr).and_then(|items| {
                items
                    .into_iter()
                    .map(|item| self.expression(item))
                    .collect::<Result<Vec<_>, _>>()
                    .map(k::Expr::List)
            })),
            s::Expr::Object(object) => Some(self.native_record(object).map(k::Expr::Record)),
            s::Expr::Member(member) if !matches!(member.obj.as_ref(), s::Expr::Ident(root) if root.sym == "k") => {
                Some(
                    self.native_member(member)
                        .map(|m| k::Expr::Member(Box::new(m))),
                )
            }
            s::Expr::Call(call) => {
                let s::Callee::Expr(callee) = &call.callee else {
                    return None;
                };
                let s::Expr::Ident(alias) = callee.as_ref() else {
                    return None;
                };
                // A binding call is not a helper, and cannot silently become an action.
                Some(
                    self.id(&s::Expr::Ident(alias.clone()))
                        .and_then(|function| {
                            let args = call_arguments(call)?
                                .into_iter()
                                .map(|arg| self.expression(arg))
                                .collect::<Result<_, _>>()?;
                            Ok(k::Expr::Call { function, args })
                        }),
                )
            }
            _ => None,
        }
    }
    fn native_record(&mut self, object: &s::ObjectLit) -> Result<Vec<k::RecordEntry>, Diagnostic> {
        let mut fields = BTreeSet::new();
        object
            .props
            .iter()
            .map(|prop| {
                let s::PropOrSpread::Prop(prop) = prop else {
                    return Err(invalid(
                        "native records have no spread",
                        Some(object.span()),
                    ));
                };
                let s::Prop::KeyValue(prop) = prop.as_ref() else {
                    return Err(invalid(
                        "native records have explicit values",
                        Some(object.span()),
                    ));
                };
                let (field, special) = match &prop.key {
                    s::PropName::Ident(field) => (field.sym.to_string(), false),
                    s::PropName::Str(field) => (
                        field
                            .value
                            .as_str()
                            .ok_or_else(|| {
                                invalid("record key needs Unicode scalars", Some(object.span()))
                            })?
                            .to_owned(),
                        false,
                    ),
                    s::PropName::Computed(field) => (super::string(&field.expr)?, true),
                    _ => {
                        return Err(invalid(
                            "native record keys are identifiers or text",
                            Some(object.span()),
                        ));
                    }
                };
                if (field == "__proto__" && !special) || !fields.insert(field.clone()) {
                    return Err(invalid(
                        "special keys need computed text; repeated fields need Object.fromEntries",
                        Some(object.span()),
                    ));
                }
                Ok(k::RecordEntry {
                    field,
                    value: self.expression(&prop.value)?,
                })
            })
            .collect()
    }
    pub(super) fn native_member(
        &mut self,
        member: &s::MemberExpr,
    ) -> Result<k::Member, Diagnostic> {
        let target = self.expression(&member.obj)?;
        match &member.prop {
            s::MemberProp::Ident(field) => Ok(k::Member::Field {
                target,
                field: field.sym.to_string(),
            }),
            s::MemberProp::Computed(index) => {
                if let s::Expr::TsConstAssertion(field) = &*index.expr
                    && matches!(&*field.expr, s::Expr::Lit(s::Lit::Str(_)))
                {
                    Ok(k::Member::Field {
                        target,
                        field: super::string(&field.expr)?,
                    })
                } else {
                    Ok(k::Member::Index {
                        target,
                        index: self.expression(&index.expr)?,
                    })
                }
            }
            s::MemberProp::PrivateName(_) => Err(invalid(
                "kernel fields are not private names",
                Some(member.span()),
            )),
        }
    }
}

fn native_number(number: &s::Number, negative: bool) -> Result<k::Literal, Diagnostic> {
    let raw = number
        .raw
        .as_ref()
        .ok_or_else(|| invalid("numeric spelling is required", Some(number.span())))?;
    let text = if negative {
        format!("-{raw}")
    } else {
        raw.to_string()
    };
    if text.contains(['.', 'e']) {
        k::Float::parse(&text)
            .map(k::Literal::Float)
            .map_err(|error| invalid(error.to_string(), Some(number.span())))
    } else {
        let value = k::Integer::parse(&text)
            .map_err(|error| invalid(error.to_string(), Some(number.span())))?;
        if value.bits() > 53 {
            return Err(invalid(
                "use a bigint literal beyond the safe native integer range",
                Some(number.span()),
            ));
        }
        Ok(k::Literal::Int(value))
    }
}
