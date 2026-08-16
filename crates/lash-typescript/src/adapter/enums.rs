use std::collections::{BTreeMap, BTreeSet};

use swc_ecma_ast as swc;

use super::{Adapter, EnumMember, Expr, Stmt, source_span};
use crate::{Diagnostic, DiagnosticCode};

#[derive(Clone, Debug)]
pub(super) enum ConstEnumValue {
    Number(f64),
    String(String),
}

impl ConstEnumValue {
    pub(super) fn expression(&self) -> Expr {
        match self {
            Self::Number(value) => Expr::Number(*value),
            Self::String(value) => Expr::String(value.clone()),
        }
    }
}

impl Adapter {
    pub(super) fn with_enum_scope<T>(
        &self,
        convert: impl FnOnce() -> Result<T, Diagnostic>,
    ) -> Result<T, Diagnostic> {
        self.enum_constants.borrow_mut().push(BTreeMap::new());
        self.inline_enums.borrow_mut().push(BTreeSet::new());
        let result = convert();
        self.inline_enums.borrow_mut().pop();
        self.enum_constants.borrow_mut().pop();
        result
    }

    pub(super) fn enum_constant(&self, owner: &str, member: &str) -> Option<ConstEnumValue> {
        self.enum_constants
            .borrow()
            .iter()
            .rev()
            .find_map(|scope| scope.get(owner).map(|members| members.get(member).cloned()))
            .flatten()
    }

    pub(super) fn enum_is_inline(&self, owner: &str) -> bool {
        let constants = self.enum_constants.borrow();
        let inline = self.inline_enums.borrow();
        constants
            .iter()
            .zip(inline.iter())
            .rev()
            .find_map(|(scope, inline)| scope.contains_key(owner).then(|| inline.contains(owner)))
            .unwrap_or(false)
    }

    pub(super) fn convert_enum(&self, declaration: &swc::TsEnumDecl) -> Result<Stmt, Diagnostic> {
        if declaration.declare {
            return Err(Diagnostic::new(
                DiagnosticCode::DeclareUnsupported,
                "Unsupported: ambient enum declarations",
                Some(source_span(declaration.span)),
            ));
        }
        let enum_name = declaration.id.sym.to_string();
        let member_names = declaration
            .members
            .iter()
            .map(|member| enum_member_name(&member.id))
            .collect::<BTreeSet<_>>();
        let mut constants = self
            .enum_constants
            .borrow()
            .last()
            .and_then(|scope| scope.get(&enum_name))
            .cloned()
            .unwrap_or_default();
        let mut next_numeric = Some(0.0);
        let mut members = Vec::with_capacity(declaration.members.len());
        for member in &declaration.members {
            let name = enum_member_name(&member.id);
            let (constant, value) = if let Some(initializer) = member.init.as_deref() {
                let constant = self.eval_enum_constant(initializer, &constants);
                let previous = self
                    .enum_context
                    .replace(Some((enum_name.clone(), member_names.clone())));
                let value = self.convert_expr(initializer);
                self.enum_context.replace(previous);
                (constant, value?)
            } else {
                let value = next_numeric.ok_or_else(|| {
                    Diagnostic::new(
                        DiagnosticCode::UnsupportedExpression,
                        format!(
                            "enum member `{name}` needs an initializer after a computed member"
                        ),
                        Some(source_span(member.span)),
                    )
                })?;
                (Some(ConstEnumValue::Number(value)), Expr::Number(value))
            };
            if declaration.is_const && constant.is_none() {
                return Err(Diagnostic::new(
                    DiagnosticCode::UnsupportedExpression,
                    format!("const enum member `{name}` must have a constant initializer"),
                    Some(source_span(member.span)),
                ));
            }
            if declaration.is_const
                && matches!(constant, Some(ConstEnumValue::Number(value)) if !value.is_finite())
            {
                return Err(Diagnostic::new(
                    DiagnosticCode::UnsupportedExpression,
                    format!("const enum member `{name}` must have a finite value"),
                    Some(source_span(member.span)),
                ));
            }
            next_numeric = match &constant {
                Some(ConstEnumValue::Number(value)) => Some(value + 1.0),
                _ => None,
            };
            let reverse = !matches!(constant, Some(ConstEnumValue::String(_)));
            if let Some(constant) = constant {
                constants.insert(name.clone(), constant);
            }
            members.push(EnumMember {
                name,
                value,
                reverse,
            });
        }
        self.enum_constants
            .borrow_mut()
            .last_mut()
            .expect("enum declarations are converted inside a lexical scope")
            .insert(enum_name.clone(), constants);
        if declaration.is_const {
            self.inline_enums
                .borrow_mut()
                .last_mut()
                .expect("enum declarations are converted inside a lexical scope")
                .insert(enum_name);
            Ok(Stmt::Empty)
        } else {
            Ok(Stmt::Enum {
                name: enum_name,
                members,
            })
        }
    }

    fn eval_enum_constant(
        &self,
        expression: &swc::Expr,
        local: &BTreeMap<String, ConstEnumValue>,
    ) -> Option<ConstEnumValue> {
        use swc::{BinaryOp as B, UnaryOp as U};
        match expression {
            swc::Expr::Lit(swc::Lit::Num(value)) => Some(ConstEnumValue::Number(value.value)),
            swc::Expr::Lit(swc::Lit::Str(value)) => value
                .value
                .as_str()
                .map(|value| ConstEnumValue::String(value.to_string())),
            swc::Expr::Ident(identifier) => local.get(identifier.sym.as_ref()).cloned(),
            swc::Expr::Member(member) => {
                let swc::Expr::Ident(owner) = member.obj.as_ref() else {
                    return None;
                };
                let name = enum_member_property_name(&member.prop)?;
                self.enum_constant(owner.sym.as_ref(), &name)
                    .or_else(|| local.get(&name).cloned())
            }
            swc::Expr::Paren(expression) => self.eval_enum_constant(&expression.expr, local),
            swc::Expr::TsAs(expression) => self.eval_enum_constant(&expression.expr, local),
            swc::Expr::TsSatisfies(expression) => self.eval_enum_constant(&expression.expr, local),
            swc::Expr::TsNonNull(expression) => self.eval_enum_constant(&expression.expr, local),
            swc::Expr::TsTypeAssertion(expression) => {
                self.eval_enum_constant(&expression.expr, local)
            }
            swc::Expr::Unary(expression) => {
                let ConstEnumValue::Number(value) =
                    self.eval_enum_constant(&expression.arg, local)?
                else {
                    return None;
                };
                Some(ConstEnumValue::Number(match expression.op {
                    U::Plus => value,
                    U::Minus => -value,
                    U::Tilde => f64::from(!enum_to_int32(value)),
                    _ => return None,
                }))
            }
            swc::Expr::Bin(expression) => {
                let left = self.eval_enum_constant(&expression.left, local)?;
                let right = self.eval_enum_constant(&expression.right, local)?;
                match (left, right, expression.op) {
                    (ConstEnumValue::String(left), ConstEnumValue::String(right), B::Add) => {
                        Some(ConstEnumValue::String(left + &right))
                    }
                    (ConstEnumValue::String(left), ConstEnumValue::Number(right), B::Add) => Some(
                        ConstEnumValue::String(format!("{left}{}", enum_number_string(right))),
                    ),
                    (ConstEnumValue::Number(left), ConstEnumValue::String(right), B::Add) => Some(
                        ConstEnumValue::String(format!("{}{right}", enum_number_string(left))),
                    ),
                    (ConstEnumValue::Number(left), ConstEnumValue::Number(right), op) => {
                        Some(ConstEnumValue::Number(match op {
                            B::Add => left + right,
                            B::Sub => left - right,
                            B::Mul => left * right,
                            B::Div => left / right,
                            B::Mod => left % right,
                            B::Exp => left.powf(right),
                            B::BitOr => f64::from(enum_to_int32(left) | enum_to_int32(right)),
                            B::BitXor => f64::from(enum_to_int32(left) ^ enum_to_int32(right)),
                            B::BitAnd => f64::from(enum_to_int32(left) & enum_to_int32(right)),
                            B::LShift => {
                                f64::from(enum_to_int32(left) << (enum_to_uint32(right) & 31))
                            }
                            B::RShift => {
                                f64::from(enum_to_int32(left) >> (enum_to_uint32(right) & 31))
                            }
                            B::ZeroFillRShift => {
                                f64::from(enum_to_uint32(left) >> (enum_to_uint32(right) & 31))
                            }
                            _ => return None,
                        }))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

pub(super) fn enum_member_name(member: &swc::TsEnumMemberId) -> String {
    match member {
        swc::TsEnumMemberId::Ident(identifier) => identifier.sym.to_string(),
        swc::TsEnumMemberId::Str(value) => value.value.to_string_lossy().into_owned(),
    }
}

fn enum_to_uint32(value: f64) -> u32 {
    if !value.is_finite() || value == 0.0 {
        0
    } else {
        value.trunc().rem_euclid(4_294_967_296.0) as u32
    }
}

fn enum_to_int32(value: f64) -> i32 {
    enum_to_uint32(value) as i32
}

fn enum_number_string(value: f64) -> String {
    ryu_js::Buffer::new().format(value).to_string()
}

pub(super) fn enum_member_property_name(property: &swc::MemberProp) -> Option<String> {
    match property {
        swc::MemberProp::Ident(identifier) => Some(identifier.sym.to_string()),
        swc::MemberProp::Computed(property) => match property.expr.as_ref() {
            swc::Expr::Lit(swc::Lit::Str(value)) => {
                Some(value.value.to_string_lossy().into_owned())
            }
            _ => None,
        },
        swc::MemberProp::PrivateName(_) => None,
    }
}
