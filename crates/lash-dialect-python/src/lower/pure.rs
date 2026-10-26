//! Pure builtin argument binding, kept beside the ordinary call lowerer.
use super::calls::arguments_error;
use super::packages::PackageRef;
use super::{Lowerer, Lowering, Operand};
use crate::scope::Ty;
use lash_kernel_dialect::{NamespaceKind, NamespaceView, SpellKind, Spelling};
use lash_kernel_doc::{Expr, Literal};
use lash_kernel_doc::{Place, Rhs};
use ruff_python_ast::{self as ast};
use ruff_text_size::Ranged;

impl Lowerer<'_> {
    pub(super) fn namespace_attribute(
        &mut self,
        builtin: &str,
        call: &ast::ExprCall,
    ) -> Option<Lowering<Operand>> {
        let receiver = call.arguments.args.first()?;
        let Ok(Some(PackageRef::Namespace(path))) = self.package_ref(receiver) else {
            return None;
        };
        Some((|| {
            self.plain_arguments(&call.arguments, builtin)?;
            let count = call.arguments.args.len();
            if count < 2 || count > if builtin == "getattr" { 3 } else { 2 } {
                return Err(arguments_error(
                    "attribute lookup has the wrong argument count",
                    call.range(),
                ));
            }
            let expressions: Vec<_> = call.arguments.args.iter().skip(1).collect();
            let mut values = self
                .operands(&expressions)?
                .into_iter()
                .map(|value| self.pin(value))
                .collect::<Vec<_>>();
            let name = values.remove(0);
            let default = values.pop();
            let is_text = self.native("kind", vec![name.expr.clone()])?;
            let is_text = self.native("same", vec![is_text, Operand::text("text").expr])?;
            let invalid = self.not(is_text)?;
            let raise = self.block(|this| {
                this.invoke_do(
                    "py.fail",
                    &[
                        Operand::text("TypeError"),
                        Operand::text("attribute name must be string"),
                    ],
                )
            })?;
            self.emit_if(invalid, raise, Default::default());
            let result = self.let_rhs(Rhs::Expr(Expr::Literal(Literal::Absent)), Ty::Unknown);
            let Expr::Variable(slot) = result.expr.clone() else {
                unreachable!("an attribute result has a slot")
            };
            let place = Place::Variable(slot);
            let mut otherwise = self.block(|this| {
                let value = if builtin == "hasattr" {
                    Operand::literal(Literal::Bool(false), Ty::Bool)
                } else if let Some(default) = &default {
                    default.clone()
                } else {
                    this.invoke_do(
                        "py.fail",
                        &[
                            Operand::text("AttributeError"),
                            Operand::text(format!(
                                "module '{path}' has no such declared attribute"
                            )),
                        ],
                    )?;
                    Operand::none()
                };
                this.store(place.clone(), value);
                Ok(())
            })?;
            let namespaces = self.namespaces;
            let package = lash_kernel_value::PackagePath::parse(&path)
                .map_err(|_| arguments_error("invalid package path", call.range()))?;
            let entries = namespaces.exports(&package).into_iter().flatten();
            let literal = match call.arguments.args.get(1) {
                Some(ruff_python_ast::Expr::StringLiteral(literal)) => Some(literal.value.to_str()),
                _ => None,
            };
            for (segment, member) in entries.rev() {
                let spelling = Spelling::Python.spell(SpellKind::Member, segment);
                if literal.is_some_and(|literal| literal != spelling) {
                    continue;
                }
                let chosen = self.block(|this| {
                    let value = if builtin == "hasattr" {
                        Operand::literal(Literal::Bool(true), Ty::Bool)
                    } else {
                        match &member.kind {
                            NamespaceKind::Function(function) => this.member_value(
                                function,
                                &format!("{path}.{spelling}"),
                                call.range(),
                            )?,
                            _ => {
                                this.invoke_do(
                                    "py.fail",
                                    &[
                                        Operand::text("TypeError"),
                                        Operand::text(
                                            "the declared member is not a guest function value",
                                        ),
                                    ],
                                )?;
                                Operand::none()
                            }
                        }
                    };
                    this.store(place.clone(), value);
                    Ok(())
                })?;
                let condition = self.native(
                    "same",
                    vec![name.expr.clone(), Operand::text(spelling).expr],
                )?;
                otherwise = self.block(|this| {
                    this.emit_if(condition, chosen, otherwise);
                    Ok(())
                })?;
            }
            self.append(otherwise);
            Ok(result)
        })())
    }

    /// An imported pure module is an ordinary guest record of bound code,
    /// never an ambient loader or host object.
    pub(super) fn imported_value(&mut self, id: &str) -> Option<Lowering<Operand>> {
        let path = self.imported.get(id)?;
        if !matches!(path.first().map(String::as_str), Some("math" | "json")) {
            return None;
        }
        let module = path[0].clone();
        let member = path.get(1).cloned();
        Some((|| {
            let value = self.invoke(&format!("py.module.{module}"), &[], Ty::Unknown)?;
            if let Some(member) = member {
                self.invoke("py.getattr", &[value, Operand::text(member)], Ty::Unknown)
            } else {
                Ok(value)
            }
        })())
    }
}
