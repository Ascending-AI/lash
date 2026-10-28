//! One parameter inventory for builtin direct calls and forwarding values.
use super::{Lowerer, Lowering, Operand};
use crate::scope::Ty;
use lash_kernel_doc::{
    CallablePresentation, Expr, FunctionRepresentation, Literal, StartConvention, Stmt,
};
use ruff_python_ast as ast;
use {
    lash_kernel_value::Name, lash_kernel_value::Param, lash_kernel_value::Signature,
    lash_kernel_value::Type,
};

pub(super) use crate::constructs::builtins::{BUILTINS, Builtin};

impl Lowerer<'_> {
    pub(super) fn builtin_value(&mut self, spec: &Builtin) -> Lowering<Operand> {
        let mut metadata = Vec::new();
        let mut params = Vec::new();
        let mut optional = Vec::new();
        let mut variadic = false;
        for spelling in spec.params {
            let (kind, text) = if let Some(s) = spelling.strip_prefix("...") {
                (3, s)
            } else if let Some(s) = spelling.strip_prefix("**") {
                (4, s)
            } else if let Some(s) = spelling.strip_prefix('*') {
                (2, s)
            } else if let Some(s) = spelling.strip_prefix('/') {
                (0, s)
            } else {
                (1, *spelling)
            };
            let default = text.ends_with('=');
            let name = text.trim_end_matches('=');
            variadic |= kind >= 3;
            params.push(Name::new(name));
            optional.push(default);
            metadata.push(Expr::Tuple(vec![
                Expr::Literal(Literal::Text(name.into())),
                Expr::Literal(Literal::Int(kind.into())),
                Expr::Literal(Literal::Bool(default)),
            ]));
        }
        let body = self.block(|this| {
            let args = params
                .iter()
                .map(|p| Operand::temp(p.clone(), Ty::Unknown))
                .collect();
            let value = this.builtin_body(spec, args, true)?;
            this.return_value(value);
            Ok(())
        })?;
        let start = (!variadic).then(|| CallablePresentation {
            signature: Signature {
                params: params
                    .iter()
                    .zip(optional)
                    .map(|(p, default)| Param {
                        name: p.clone(),
                        ty: Type::Any,
                        optional: default,
                    })
                    .collect(),
                result: if spec.name == "print" {
                    Type::Null
                } else {
                    lash_kernel_dialect::Library::resolve(self.library, spec.helper)
                        .and_then(|id| self.library.header(&id))
                        .map_or(Type::Any, |header| header.signature.result.clone())
                },
            },
            representation: FunctionRepresentation::Bare,
            convention: StartConvention::Direct,
        });
        let mut value =
            self.wrap_python_callable(params, body, metadata, start, spec.name, false)?;
        if let Expr::Tuple(parts) = &mut value.expr {
            parts[4] = if spec.class {
                Expr::Tuple(vec![
                    Expr::Literal(Literal::Text("py.type".into())),
                    Expr::Literal(Literal::Text(spec.name.into())),
                ])
            } else {
                Expr::Literal(Literal::Text("py.builtin".into()))
            };
        }
        Ok(value)
    }
    pub(super) fn builtin_direct(
        &mut self,
        spec: &Builtin,
        call: &ast::ExprCall,
    ) -> Lowering<Operand> {
        if matches!(spec.name, "getattr" | "hasattr")
            && let Some(result) = self.namespace_attribute(spec.name, call)
        {
            return result;
        }
        let given = self.given(None, &call.arguments)?;
        let mut position = 0;
        let mut used = std::collections::BTreeSet::new();
        let keywords = call
            .arguments
            .keywords
            .iter()
            .zip(given.keywords)
            .map(|(kw, value)| {
                let Some(name) = &kw.arg else {
                    unreachable!("expanded arguments use the runtime binder");
                };
                (name.id.as_str(), value)
            })
            .collect::<Vec<_>>();
        let mut slots = Vec::new();
        let mut problem = None;
        let mut variadic = false;
        let mut extra = false;
        for spelling in spec.params {
            let (mode, name) = if let Some(name) = spelling.strip_prefix("...") {
                (3, name)
            } else if let Some(name) = spelling.strip_prefix("**") {
                (4, name)
            } else if let Some(name) = spelling.strip_prefix('*') {
                (2, name)
            } else if let Some(name) = spelling.strip_prefix('/') {
                (0, name)
            } else {
                (1, *spelling)
            };
            let optional = name.ends_with('=');
            let name = name.trim_end_matches('=');
            let mut value = None;
            if mode < 2 && position < given.positional.len() {
                value = Some(given.positional[position].clone());
                position += 1;
            }
            if matches!(mode, 1 | 2)
                && let Some((_, keyword)) = keywords.iter().find(|(key, _)| *key == name)
            {
                if value.is_some() {
                    problem = Some(format!("got multiple values for argument '{name}'"));
                }
                value = Some(keyword.clone());
                used.insert(name);
            }
            if mode == 3 {
                variadic = true;
                value = Some(Operand::inline(
                    Expr::Tuple(
                        given.positional[position..]
                            .iter()
                            .map(|v| v.expr.clone())
                            .collect(),
                    ),
                    Ty::Tuple,
                ));
            }
            if mode == 4 {
                extra = true;
                value = Some(Operand::inline(
                    Expr::Map(
                        keywords
                            .iter()
                            .filter(|(key, _)| !used.contains(key))
                            .map(|(key, value)| lash_kernel_doc::MapEntry {
                                key: Expr::Literal(Literal::Text((*key).into())),
                                value: value.expr.clone(),
                            })
                            .collect(),
                    ),
                    Ty::Dict,
                ));
            }
            if value.is_none() && !optional {
                problem.get_or_insert_with(|| {
                    format!("{}() missing a required positional argument", spec.name)
                });
            }
            slots.push(value.unwrap_or_else(super::calls::absent));
        }
        if !variadic && position < given.positional.len() {
            problem = Some("the function takes fewer positional arguments".into());
        }
        if !extra && let Some((key, _)) = keywords.iter().find(|(key, _)| !used.contains(key)) {
            problem = Some(format!("got an unexpected keyword argument '{key}'"));
        }
        if let Some(problem) = problem {
            self.invoke_do(
                "py.fail",
                &[Operand::text("TypeError"), Operand::text(problem)],
            )?;
            return Ok(Operand::none());
        }
        self.builtin_body(spec, slots, false)
    }

    fn builtin_body(
        &mut self,
        spec: &Builtin,
        mut args: Vec<Operand>,
        dynamic: bool,
    ) -> Lowering<Operand> {
        for (index, arg) in args.iter_mut().enumerate() {
            if !spec.params[index].ends_with('=') {
                continue;
            }
            let fallback = match (spec.name, index) {
                ("str", 0) | ("format", 1) => Some(Expr::Literal(Literal::Text(String::new()))),
                ("int" | "float", 0) => Some(Expr::Literal(Literal::Int(0.into()))),
                ("bool", 0) => Some(Expr::Literal(Literal::Bool(false))),
                ("list" | "tuple" | "set" | "dict", 0) => Some(Expr::Tuple(Vec::new())),
                ("print", 1) => Some(Expr::Literal(Literal::Text(" ".into()))),
                _ => None,
            };
            if let Some(fallback) = fallback {
                if arg.expr == Expr::Literal(Literal::Absent) {
                    let ty = match &fallback {
                        Expr::Literal(Literal::Text(_)) => Ty::Str,
                        Expr::Literal(Literal::Int(_)) => Ty::Int,
                        Expr::Literal(Literal::Bool(_)) => Ty::Bool,
                        _ => Ty::Unknown,
                    };
                    *arg = Operand::inline(fallback, ty);
                } else if dynamic {
                    let missing = self.native(
                        "same",
                        vec![arg.expr.clone(), Expr::Literal(Literal::Absent)],
                    )?;
                    arg.expr = Expr::If {
                        cond: Box::new(missing),
                        then: Box::new(fallback),
                        r#else: Box::new(arg.expr.clone()),
                    };
                }
            }
        }
        if matches!(spec.name, "min" | "max") {
            args.insert(
                1,
                Operand::literal(Literal::Bool(spec.name == "max"), Ty::Bool),
            );
        }
        if matches!(spec.name, "vars" | "dir") {
            let missing = self.native(
                "same",
                vec![args[0].expr.clone(), Expr::Literal(Literal::Absent)],
            )?;
            let snapshot = self.namespace_copy(false)?;
            args[0].expr = Expr::If {
                cond: Box::new(missing),
                then: Box::new(snapshot.expr),
                r#else: Box::new(args[0].expr.clone()),
            };
        }
        if matches!(spec.name, "globals" | "locals") {
            // The dictionary is captured at the guest read, never from a host frame.
            args.push(self.namespace_copy(spec.name == "globals")?);
        }
        if spec.name == "type" {
            let mut constructors = Vec::new();
            for class in BUILTINS
                .iter()
                .filter(|class| class.class && class.name != "type")
            {
                let value = self.builtin_value(class)?;
                constructors.push(lash_kernel_doc::MapEntry {
                    key: Expr::Literal(Literal::Text(class.name.into())),
                    value: value.expr,
                });
            }
            args.push(Operand::inline(Expr::Map(constructors), Ty::Dict));
        }
        if spec.name == "str" {
            return self.string_value(args.remove(0));
        }
        let ty = match spec.name {
            "str" | "repr" | "format" | "hex" | "oct" | "bin" | "ascii" => Ty::Str,
            "bool" | "callable" | "isinstance" | "any" | "all" => Ty::Bool,
            "len" | "int" | "ord" => Ty::Int,
            "float" => Ty::Float,
            "list" | "sorted" | "dir" | "range" | "reversed" | "enumerate" | "zip" => Ty::List,
            "tuple" => Ty::Tuple,
            "dict" | "globals" | "locals" | "vars" => Ty::Dict,
            "set" => Ty::Set,
            _ => Ty::Unknown,
        };
        if spec.name == "print" {
            let options = Self::tuple_of(&args[1..]);
            args.truncate(1);
            args.push(options);
        }
        let value = self.invoke(spec.helper, &args, ty)?;
        if spec.name == "print" {
            self.emit(Stmt::Print { value: value.expr });
            Ok(Operand::none())
        } else {
            Ok(value)
        }
    }
}

impl Lowerer<'_> {
    pub(super) fn namespace_copy(&mut self, global: bool) -> Lowering<Operand> {
        let index = if global {
            self.module_environment
                .as_ref()
                .map_or(0, |(scope, _)| *scope)
        } else {
            self.scopes.len() - 1
        };
        let names = self.scopes[index]
            .bindings
            .locals
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let mut entries = Vec::new();
        for name in names {
            // Unbound slots are filtered by the helper rather than raising.
            entries.push(lash_kernel_doc::MapEntry {
                key: Expr::Literal(Literal::Text(name.clone())),
                value: Expr::Variable(
                    self.module_environment
                        .as_ref()
                        .filter(|(scope, _)| *scope == index)
                        .and_then(|(_, bindings)| bindings.get(&name))
                        .or_else(|| self.scopes[index].names.get(&name))
                        .cloned()
                        .unwrap_or_else(|| Name::new(name)),
                ),
            });
        }
        self.invoke(
            "py.namespace",
            &[Operand::inline(Expr::Map(entries), Ty::Dict)],
            Ty::Dict,
        )
    }
}
