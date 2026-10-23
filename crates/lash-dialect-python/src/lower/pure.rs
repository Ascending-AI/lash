//! Pure builtin argument binding, kept beside the ordinary call lowerer.
use super::calls::{Given, absent, arguments_error, keyword_names};
use super::packages::PackageRef;
use super::{Lowerer, Lowering, Operand};
use crate::scope::Ty;
use lash_kernel_dialect::{NamespaceKind, NamespaceView, SpellKind, Spelling};
use lash_kernel_doc::{Expr, Literal, MapEntry, Stmt};
use lash_kernel_doc::{Place, Rhs};
use ruff_python_ast::{self as ast};
use ruff_text_size::Ranged;

impl Lowerer<'_> {
    pub(super) fn pure_builtin(
        &mut self,
        name: &str,
        call: &ast::ExprCall,
    ) -> Option<Lowering<Operand>> {
        match name {
            "print" | "min" | "max" | "zip" | "dict" | "getattr" | "hasattr" | "bytes"
            | "format" | "ascii" => Some(self.pure_builtin_inner(name, call)),
            _ => None,
        }
    }

    fn pure_builtin_inner(&mut self, name: &str, call: &ast::ExprCall) -> Lowering<Operand> {
        match name {
            "print" => {
                let mut given = self.with_keywords(
                    name,
                    call,
                    call.arguments.args.len(),
                    &["sep", "end", "flush"],
                )?;
                given.resize_with(call.arguments.args.len() + 3, absent);
                let options = given.split_off(call.arguments.args.len());
                let args = Self::tuple_of(&given);
                let value =
                    self.invoke("py.print_text", &[args, Self::tuple_of(&options)], Ty::Str)?;
                self.emit(Stmt::Print { value: value.expr });
                Ok(Operand::none())
            }
            "min" | "max" => {
                if call.arguments.args.is_empty() {
                    return Err(arguments_error(
                        "min/max expected at least one argument",
                        call.range(),
                    ));
                }
                let mut given =
                    self.with_keywords(name, call, call.arguments.args.len(), &["key", "default"])?;
                given.resize_with(call.arguments.args.len() + 2, absent);
                let options = given.split_off(call.arguments.args.len());
                let args = Self::tuple_of(&given);
                self.invoke(
                    "py.extreme",
                    &[
                        args,
                        Operand::literal(Literal::Bool(name == "max"), Ty::Bool),
                        options[0].clone(),
                        options[1].clone(),
                    ],
                    Ty::Unknown,
                )
            }
            "zip" => {
                let mut given =
                    self.with_keywords(name, call, call.arguments.args.len(), &["strict"])?;
                let strict = if given.len() > call.arguments.args.len() {
                    given.pop().unwrap_or_else(absent)
                } else {
                    absent()
                };
                self.invoke("py.zip", &[Self::tuple_of(&given), strict], Ty::List)
            }
            "dict" => {
                if call.arguments.args.len() > 1 {
                    return Err(arguments_error(
                        "dict expected at most one positional argument",
                        call.range(),
                    ));
                }
                let Given {
                    mut positional,
                    keywords,
                    ..
                } = self.given(None, &call.arguments)?;
                let fields = keyword_names(&call.arguments)
                    .zip(keywords)
                    .map(|(name, value)| MapEntry {
                        key: Operand::text(name).expr,
                        value: value.expr,
                    })
                    .collect();
                let fields = Operand::inline(Expr::Map(fields), Ty::Dict);
                self.invoke(
                    "py.dict",
                    &[positional.pop().unwrap_or_else(absent), fields],
                    Ty::Dict,
                )
            }
            "getattr" | "hasattr" => {
                if let Some(result) = self.namespace_attribute(name, call) {
                    return result;
                }
                let values =
                    self.positional(name, call, 2, if name == "getattr" { 3 } else { 2 })?;
                self.invoke(
                    if name == "getattr" {
                        "py.getattr"
                    } else {
                        "py.hasattr"
                    },
                    &values,
                    Ty::Unknown,
                )
            }
            "bytes" => {
                let values =
                    self.with_keywords(name, call, 0, &["source", "encoding", "errors"])?;
                self.invoke("py.bytes", &values, Ty::Unknown)
            }
            "format" => {
                let values = self.positional(name, call, 1, 2)?;
                self.invoke("py.format_spec", &values, Ty::Str)
            }
            "ascii" => {
                let values = self.positional(name, call, 1, 1)?;
                self.invoke("py.ascii", &values, Ty::Str)
            }
            _ => unreachable!("the pure builtin selection is closed"),
        }
    }

    fn namespace_attribute(
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
            let package = lash_vocabulary::PackagePath::parse(&path)
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

    pub(super) fn pure_module_call(&mut self, call: &ast::ExprCall) -> Option<Lowering<Operand>> {
        let (path, bound) = self.call_path(&call.func)?;
        if bound || path.len() != 2 || !matches!(path[0].as_str(), "math" | "json") {
            return None;
        }
        let function = crate::pure_modules::function(&path[0], &path[1])?;
        let params = function.params;
        Some((|| {
            let mut values = self.with_keywords(
                &path.join("."),
                call,
                function.required,
                &params[function.required..],
            )?;
            if path == ["json", "dumps"] {
                values.resize_with(params.len(), absent);
                let options = Self::tuple_of(&values[1..]);
                values.truncate(1);
                values.push(options);
            }
            self.invoke(&format!("py.{}.{}", path[0], path[1]), &values, Ty::Unknown)
        })())
    }
}
