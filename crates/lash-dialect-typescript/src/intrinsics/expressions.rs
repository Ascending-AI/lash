use lash_kernel_doc as k;
use swc_common::Spanned;
use swc_ecma_ast as s;

use super::{Reader, array, invalid, invocation, string};
use crate::Diagnostic;

impl Reader<'_> {
    pub(super) fn function(&mut self, expr: &s::Expr) -> Result<k::Function, Diagnostic> {
        let s::Expr::Fn(function) = expr else {
            return Err(invalid(
                "expected a kernel function body",
                Some(expr.span()),
            ));
        };
        if function.ident.is_some() || function.function.is_async || function.function.is_generator
        {
            return Err(invalid(
                "kernel functions have no async/generator marker or expression name",
                Some(expr.span()),
            ));
        }
        let params = function
            .function
            .params
            .iter()
            .map(|param| match &param.pat {
                s::Pat::Ident(binding) => self.name(&binding.id),
                _ => Err(invalid("kernel parameters are names", Some(param.span()))),
            })
            .collect::<Result<_, _>>()?;
        let body = function
            .function
            .body
            .as_ref()
            .ok_or_else(|| invalid("kernel functions have a body", Some(expr.span())))?;
        Ok(k::Function {
            params,
            body: self.block(&body.stmts)?,
        })
    }
    pub(super) fn expression(&mut self, expr: &s::Expr) -> Result<k::Expr, Diagnostic> {
        match expr {
            s::Expr::Paren(value) => self.expression(&value.expr),
            s::Expr::Ident(ident) => Ok(k::Expr::Variable(self.name(ident)?)),
            s::Expr::Fn(_) => {
                let function = self.function(expr)?;
                Ok(k::Expr::Closure(Box::new(k::Closure {
                    params: function.params,
                    body: function.body,
                })))
            }
            s::Expr::Lit(s::Lit::Null(_)) => Ok(k::Expr::Literal(k::Literal::Null)),
            s::Expr::Lit(s::Lit::Bool(value)) => {
                Ok(k::Expr::Literal(k::Literal::Bool(value.value)))
            }
            s::Expr::Lit(s::Lit::Str(_)) => Ok(k::Expr::Literal(k::Literal::Text(string(expr)?))),
            s::Expr::Lit(s::Lit::Num(value)) => Ok(k::Expr::Literal(k::Literal::Float(
                k::Float::new(value.value),
            ))),
            s::Expr::Member(member)
                if matches!(member.obj.as_ref(), s::Expr::Ident(root) if root.sym == "k")
                    && matches!(&member.prop, s::MemberProp::Ident(name) if name.sym == "absent") =>
            {
                Ok(k::Expr::Literal(k::Literal::Absent))
            }
            _ => self.operation(expr),
        }
    }
    fn operation(&mut self, expr: &s::Expr) -> Result<k::Expr, Diagnostic> {
        let (name, args) = invocation(expr)?;
        let literal = match (name.as_str(), args.as_slice()) {
            ("int", [value]) => Some(k::Literal::Int(
                k::Integer::parse(&string(value)?)
                    .map_err(|error| invalid(error.to_string(), Some(value.span())))?,
            )),
            ("float", [value]) => Some(k::Literal::Float(
                k::Float::parse(&string(value)?)
                    .map_err(|error| invalid(error.to_string(), Some(value.span())))?,
            )),
            ("bytes", [value]) => Some(k::Literal::Bytes(
                k::Bytes::parse_hex(&string(value)?)
                    .map_err(|error| invalid(error.to_string(), Some(value.span())))?,
            )),
            ("function", [value]) => Some(k::Literal::Function(k::Name::new(string(value)?))),
            ("absent", []) => Some(k::Literal::Absent),
            _ => None,
        };
        if let Some(literal) = literal {
            return Ok(k::Expr::Literal(literal));
        }
        Ok(match (name.as_str(), args.as_slice()) {
            ("tuple", _) => k::Expr::Tuple(self.expressions(&args)?),
            ("list", _) => k::Expr::List(self.expressions(&args)?),
            ("set", _) => k::Expr::Set(self.expressions(&args)?),
            ("map", _) => k::Expr::Map(
                args.iter()
                    .map(|entry| {
                        let pair = array(entry)?;
                        let [key, value] = pair.as_slice() else {
                            return Err(invalid(
                                "map entry needs key and value",
                                Some(entry.span()),
                            ));
                        };
                        Ok(k::MapEntry {
                            key: self.expression(key)?,
                            value: self.expression(value)?,
                        })
                    })
                    .collect::<Result<_, _>>()?,
            ),
            ("record", _) => k::Expr::Record(
                args.iter()
                    .map(|entry| {
                        let pair = array(entry)?;
                        let [field, value] = pair.as_slice() else {
                            return Err(invalid(
                                "record entry needs field and value",
                                Some(entry.span()),
                            ));
                        };
                        Ok(k::RecordEntry {
                            field: string(field)?,
                            value: self.expression(value)?,
                        })
                    })
                    .collect::<Result<_, _>>()?,
            ),
            ("field", [_, _]) | ("index", [_, _]) => k::Expr::Member(Box::new(self.member(expr)?)),
            ("clock", []) => k::Expr::Clock,
            ("random", []) => k::Expr::Random,
            ("read", [handle, request]) => k::Expr::Read(Box::new(k::ProjectionRead {
                handle: self.expression(handle)?,
                request: self.expression(request)?,
            })),
            ("invoke", [id, args]) => k::Expr::Call {
                function: self.id(id)?,
                args: self.expressions(&array(args)?)?,
            },
            // Named kernel operations never resolve to TypeScript coercion helpers.
            (
                "add" | "sub" | "mul" | "div" | "div_floor" | "div_trunc" | "rem_floor"
                | "rem_trunc" | "neg" | "lt" | "le",
                _,
            ) => {
                let qualified = format!("num.{name}");
                let id = self
                    .environment
                    .library
                    .resolve(&qualified)
                    .ok_or_else(|| {
                        invalid(
                            format!("kernel function {qualified} is unavailable"),
                            Some(expr.span()),
                        )
                    })?;
                self.require(id, expr)?;
                k::Expr::Call {
                    function: id,
                    args: self.expressions(&args)?,
                }
            }
            ("eq" | "same" | "ref" | "deref", _) => {
                let id = self.environment.library.resolve(&name).ok_or_else(|| {
                    invalid(
                        format!("kernel function {name} is unavailable"),
                        Some(expr.span()),
                    )
                })?;
                self.require(id, expr)?;
                k::Expr::Call {
                    function: id,
                    args: self.expressions(&args)?,
                }
            }
            _ => {
                return Err(invalid(
                    format!("unknown or misplaced kernel expression k.{name}"),
                    Some(expr.span()),
                ));
            }
        })
    }
    fn expressions(&mut self, args: &[&s::Expr]) -> Result<Vec<k::Expr>, Diagnostic> {
        args.iter().map(|arg| self.expression(arg)).collect()
    }
    pub(super) fn member(&mut self, expr: &s::Expr) -> Result<k::Member, Diagnostic> {
        let (name, args) = invocation(expr)?;
        match (name.as_str(), args.as_slice()) {
            ("field", [target, field]) => Ok(k::Member::Field {
                target: self.expression(target)?,
                field: string(field)?,
            }),
            ("index", [target, index]) => Ok(k::Member::Index {
                target: self.expression(target)?,
                index: self.expression(index)?,
            }),
            _ => Err(invalid("expected k.field or k.index", Some(expr.span()))),
        }
    }
    fn atom(&mut self, expr: &s::Expr) -> Result<k::Atom, Diagnostic> {
        match self.expression(expr)? {
            k::Expr::Literal(value) => Ok(k::Atom::Literal(value)),
            k::Expr::Variable(name) => Ok(k::Atom::Variable(name)),
            _ => Err(invalid(
                "an action operand must be a variable or literal",
                Some(expr.span()),
            )),
        }
    }
    fn atoms(&mut self, expr: &s::Expr) -> Result<Vec<k::Atom>, Diagnostic> {
        array(expr)?.iter().map(|expr| self.atom(expr)).collect()
    }
    fn callee(&mut self, expr: &s::Expr) -> Result<k::Callee, Diagnostic> {
        let (name, args) = invocation(expr)?;
        match (name.as_str(), args.as_slice()) {
            ("declared", [value]) => Ok(k::Callee::Declared(k::Name::new(string(value)?))),
            ("value", [value]) => Ok(k::Callee::Value(k::Name::new(string(value)?))),
            ("library", [value]) => Ok(k::Callee::Library(self.id(value)?)),
            _ => Err(invalid(
                "expected a declared/value/library callee",
                Some(expr.span()),
            )),
        }
    }
    pub(super) fn action(&mut self, expr: &s::Expr) -> Result<k::Action, Diagnostic> {
        let (name, args) = invocation(expr)?;
        Ok(match (name.as_str(), args.as_slice()) {
            ("call", [callee, args]) => k::Action::Call {
                callee: self.callee(callee)?,
                args: self.atoms(args)?,
            },
            ("spawn", [callee, args]) => k::Action::Spawn {
                callee: self.callee(callee)?,
                args: self.atoms(args)?,
            },
            ("perform", [effect, args, ty]) => {
                let (name, operands) = invocation(ty)?;
                let [value] = operands.as_slice() else {
                    return Err(invalid("perform result needs k.type", Some(ty.span())));
                };
                if name != "type" {
                    return Err(invalid("perform result needs k.type", Some(ty.span())));
                }
                let effect = k::EffectName::new(string(effect)?)
                    .map_err(|error| invalid(error.to_string(), Some(effect.span())))?;
                if let Some(signature) = self.environment.effects.get(&effect) {
                    self.effects.insert(effect.clone(), signature.clone());
                }
                k::Action::Perform {
                    effect,
                    args: self.atoms(args)?,
                    result: super::decode(&string(value)?)
                        .map_err(|error| invalid(error.to_string(), Some(value.span())))?,
                }
            }
            ("sleep", [duration]) => k::Action::Sleep {
                duration: self.atom(duration)?,
            },
            ("join", [task]) => k::Action::Join {
                task: self.atom(task)?,
            },
            ("joinMany", [mode, tasks]) => k::Action::JoinMany {
                mode: match string(mode)?.as_str() {
                    "all" => k::JoinMode::All,
                    "all_settled" => k::JoinMode::AllSettled,
                    "race" => k::JoinMode::Race,
                    "any" => k::JoinMode::Any,
                    _ => return Err(invalid("unknown join mode", Some(mode.span()))),
                },
                tasks: self.atom(tasks)?,
            },
            ("yield", []) => k::Action::Yield,
            ("cancel", [task]) => k::Action::Cancel {
                task: self.atom(task)?,
            },
            _ => {
                return Err(invalid(
                    format!("unknown or misplaced kernel action k.{name}"),
                    Some(expr.span()),
                ));
            }
        })
    }
    pub(super) fn rhs(&mut self, expr: &s::Expr) -> Result<k::Rhs, Diagnostic> {
        if invocation(expr).is_ok_and(|(name, _)| {
            matches!(
                name.as_str(),
                "call" | "spawn" | "perform" | "sleep" | "join" | "joinMany" | "yield" | "cancel"
            )
        }) {
            self.action(expr).map(k::Rhs::Action)
        } else {
            self.expression(expr).map(k::Rhs::Expr)
        }
    }
}
