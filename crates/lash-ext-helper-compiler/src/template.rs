//! Partial evaluation of helper bodies over immediate operand kinds.
//!
//! Kind dispatch and constant tests disappear during generation. Remaining
//! scalar calls use the registered native's prepared entry, never a second
//! implementation of its semantics. An unsupported path simply declines.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use lash_kernel_doc::{
    ActKind, Action, Atom, Callee, Expr, FunctionDefinition, FunctionId, Literal, Name, Place, Rhs,
    Stmt, Value, ValueKind,
};

use crate::library::Library;

#[derive(Clone)]
struct Symbol {
    code: String,
    constant: Option<Value>,
    kind: Option<ValueKind>,
    calls: usize,
}

impl Symbol {
    fn constant(value: Value) -> Option<Self> {
        let code = match value {
            Value::Null => "Value::Null".into(),
            Value::Absent => "Value::Absent".into(),
            Value::Bool(value) => format!("Value::Bool({value})"),
            Value::Int(value) => format!("Value::Int({value})"),
            Value::Float(value) => format!(
                "Value::Float(lash_kernel_doc::Float::new(f64::from_bits({})))",
                value.get().to_bits()
            ),
            Value::Text(value) => format!(
                "Value::Text(lash_kernel_doc::arena::ShortText::new({:?})?)",
                value.as_str()
            ),
            _ => return None,
        };
        Some(Self {
            code,
            constant: Some(value),
            kind: Some(value.kind()),
            calls: 0,
        })
    }
}

#[derive(Clone, Default)]
struct State {
    vars: BTreeMap<Name, Symbol>,
    steps: Vec<String>,
    depth: u32,
}

struct Evaluator<'a> {
    library: &'a Library,
    callees: Vec<FunctionId>,
    used: usize,
}

impl Evaluator<'_> {
    fn literal(literal: &Literal) -> Option<Symbol> {
        Symbol::constant(match literal {
            Literal::Null => Value::Null,
            Literal::Absent => Value::Absent,
            Literal::Bool(value) => Value::Bool(*value),
            Literal::Int(value) => Value::Int(value.word()?),
            Literal::Float(value) => Value::Float(*value),
            Literal::Text(value) => Value::Text(lash_kernel_doc::arena::ShortText::new(value)?),
            _ => return None,
        })
    }

    fn atom(atom: &Atom, state: &State) -> Option<Symbol> {
        match atom {
            Atom::Literal(literal) => Self::literal(literal),
            Atom::Variable(name) => state.vars.get(name).cloned(),
        }
    }

    fn rhs(&mut self, rhs: &Rhs, state: &mut State, depth: u32) -> Option<Symbol> {
        match rhs {
            Rhs::Expr(expr) => self.expr(expr, state, depth),
            Rhs::Action(Action::Call {
                callee: Callee::Library(function),
                args,
            }) => {
                let args = args
                    .iter()
                    .map(|arg| Self::atom(arg, state))
                    .collect::<Option<Vec<_>>>()?;
                self.call(*function, args, state, depth)
            }
            _ => None,
        }
    }

    fn expr(&mut self, expr: &Expr, state: &mut State, depth: u32) -> Option<Symbol> {
        match expr {
            Expr::Literal(literal) => Self::literal(literal),
            Expr::Variable(name) => state.vars.get(name).cloned(),
            Expr::Call { function, args } => {
                let args = args
                    .iter()
                    .map(|arg| self.expr(arg, state, depth))
                    .collect::<Option<Vec<_>>>()?;
                self.call(*function, args, state, depth)
            }
            Expr::Act(act) => {
                let ActKind::Call(Callee::Library(function)) = act.kind else {
                    return None;
                };
                let args = act
                    .args
                    .iter()
                    .map(|arg| self.expr(arg, state, depth))
                    .collect::<Option<Vec<_>>>()?;
                self.call(function, args, state, depth)
            }
            Expr::Let { name, value, body } => {
                let value = self.expr(value, state, depth)?;
                let previous = state.vars.insert(name.clone(), value);
                let result = self.expr(body, state, depth);
                match previous {
                    Some(value) => {
                        state.vars.insert(name.clone(), value);
                    }
                    None => {
                        state.vars.remove(name);
                    }
                }
                result
            }
            Expr::If { cond, then, r#else } => {
                let test = self.expr(cond, state, depth)?.constant?;
                match test {
                    Value::Bool(true) => self.expr(then, state, depth),
                    Value::Bool(false) => self.expr(r#else, state, depth),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn call(
        &mut self,
        function: FunctionId,
        args: Vec<Symbol>,
        state: &mut State,
        depth: u32,
    ) -> Option<Symbol> {
        let registered = self.library.registry.get(&function)?;
        let definition = registered.definition();
        // Kind is known even though the value is not. This is the only
        // intrinsic reduction; all value computations use native entries.
        if definition.name.as_str() == "kind" {
            let kind = args.first()?.kind?;
            let sample = match kind {
                ValueKind::Null => Value::Null,
                ValueKind::Absent => Value::Absent,
                ValueKind::Bool => Value::Bool(false),
                ValueKind::Int => Value::Int(0),
                ValueKind::Float => Value::Float(lash_kernel_doc::Float::new(0.0)),
                ValueKind::Text => Value::Text(lash_kernel_doc::arena::ShortText::new("")?),
                _ => return None,
            };
            return Symbol::constant((registered.native()?.prepare()?.entry)(&[sample])?);
        }
        if definition.name.as_str().starts_with("py.")
            || definition.name.as_str().starts_with("ts.")
        {
            let body = definition.body()?;
            let mut nested = State {
                steps: Vec::new(),
                vars: definition
                    .signature
                    .params
                    .iter()
                    .zip(args)
                    .map(|(param, arg)| (param.name.clone(), arg))
                    .collect(),
                depth: depth + 1,
            };
            let mut results = self.block(&body.block, &mut nested, depth + 1);
            if results.is_empty() {
                return None;
            }
            for (nested, _) in &results {
                state.depth = state.depth.max(nested.depth);
            }
            if results.len() == 1 {
                let (nested, value) = results.pop()?;
                state.steps.extend(nested.steps);
                return value.or_else(|| Symbol::constant(Value::Null));
            }
            // A helper used as an operand can itself branch on a scalar
            // value (ToBoolean on NaN, for example). Keep those guarded
            // alternatives in its expression, in source evaluation order.
            let result = format!("v{}", self.used);
            self.used += 1;
            let mut code = format!("let {result} = (|| {{\n");
            let mut kind = None;
            let mut calls = 0;
            for (index, (nested, value)) in results.into_iter().enumerate() {
                let value = value.or_else(|| Symbol::constant(Value::Null))?;
                calls = calls.max(value.calls);
                kind = if index == 0 {
                    value.kind
                } else {
                    kind.filter(|kind| Some(*kind) == value.kind)
                };
                code.push_str("if let Some(value) = (|| {\n");
                for step in nested.steps {
                    let _ = writeln!(code, "{step}");
                }
                let _ = writeln!(
                    code,
                    "Some({})\n}})() {{ return Some(value); }}",
                    value.code
                );
            }
            let _ = writeln!(code, "None\n}})()?;\nlet _ = {result};");
            state.steps.push(code);
            return Some(Symbol {
                code: result,
                constant: None,
                kind,
                calls,
            });
        }
        if definition.guard.is_some() {
            return None;
        }
        let entry = registered.native()?.prepare()?;
        if let Some(values) = args
            .iter()
            .map(|arg| arg.constant)
            .collect::<Option<Vec<_>>>()
        {
            return Symbol::constant((entry.entry)(&values)?);
        }
        let index = match self.callees.iter().position(|callee| *callee == function) {
            Some(index) => index,
            None => {
                self.callees.push(function);
                self.callees.len() - 1
            }
        };
        let code = format!("v{}", self.used);
        self.used += 1;
        state.steps.push(format!(
            "let {code} = (natives[{index}].entry)(&[{}])?; let _ = {code};",
            args.iter()
                .map(|arg| arg.code.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        let kind = match definition.signature.result {
            lash_kernel_doc::Type::Bool => Some(ValueKind::Bool),
            lash_kernel_doc::Type::Float => Some(ValueKind::Float),
            lash_kernel_doc::Type::Int => Some(ValueKind::Int),
            _ => None,
        };
        // Boolean negation is a result wrapper; the underlying comparison
        // or truth test remains the one value-producing native.
        let calls = usize::from(definition.name.as_str() != "bool.not")
            + args.iter().map(|arg| arg.calls).sum::<usize>();
        Some(Symbol {
            code,
            constant: None,
            kind,
            calls,
        })
    }

    fn block(
        &mut self,
        block: &[Stmt],
        state: &mut State,
        depth: u32,
    ) -> Vec<(State, Option<Symbol>)> {
        let Some((stmt, rest)) = block.split_first() else {
            return vec![(state.clone(), None)];
        };
        match stmt {
            Stmt::Return { value } => self
                .expr(value, state, depth)
                .map(|value| vec![(state.clone(), Some(value))])
                .unwrap_or_default(),
            Stmt::Let { name, value }
            | Stmt::Assign {
                place: Place::Variable(name),
                value,
                ..
            } => {
                let Some(value) = self.rhs(value, state, depth) else {
                    return Vec::new();
                };
                state.vars.insert(name.clone(), value);
                self.block(rest, state, depth)
            }
            Stmt::If {
                condition,
                then_block,
                else_block,
            } => {
                let Some(test) = self.expr(condition, state, depth) else {
                    return Vec::new();
                };
                let branches = match test.constant {
                    Some(Value::Bool(true)) => vec![(then_block, None)],
                    Some(Value::Bool(false)) => vec![(else_block, None)],
                    None => vec![(then_block, Some(true)), (else_block, Some(false))],
                    _ => return Vec::new(),
                };
                let mut outcomes = Vec::new();
                for (branch, guard) in branches {
                    let mut next = state.clone();
                    if let Some(guard) = guard {
                        next.steps.push(format!(
                            "if !matches!({}, Value::Bool({guard})) {{ return None; }}",
                            test.code
                        ));
                    }
                    for (mut next, returned) in self.block(branch, &mut next, depth) {
                        if let Some(value) = returned {
                            outcomes.push((next, Some(value)));
                            continue;
                        }
                        // Let declarations belong to the arm; assignments
                        // to outer variables survive it.
                        for stmt in branch {
                            if let Stmt::Let { name, .. } = stmt {
                                match state.vars.get(name) {
                                    Some(previous) => {
                                        next.vars.insert(name.clone(), previous.clone());
                                    }
                                    None => {
                                        next.vars.remove(name);
                                    }
                                }
                            }
                        }
                        outcomes.extend(self.block(rest, &mut next, depth));
                    }
                }
                outcomes
            }
            // Discarded calls, allocations, effects, loops and handlers
            // never disappear into a scalar template.
            _ => Vec::new(),
        }
    }
}

/// A generated template descriptor and its prepared code, if a scalar
/// typed path of this body can run using certified native entries.
pub(crate) fn emit(
    library: &Library,
    definition: &FunctionDefinition,
    ident: &str,
) -> Option<(String, String)> {
    if definition.guard.is_some() {
        return None;
    }
    let body = definition.body()?;
    let kinds = [
        ("Null", ValueKind::Null),
        ("Absent", ValueKind::Absent),
        ("Bool(_)", ValueKind::Bool),
        ("Int(_)", ValueKind::Int),
        ("Float(_)", ValueKind::Float),
        ("Text(_)", ValueKind::Text),
    ];
    fn accepts(ty: &lash_kernel_doc::Type, kind: ValueKind) -> bool {
        use lash_kernel_doc::Type;
        match ty {
            Type::Any => true,
            Type::Null => kind == ValueKind::Null,
            Type::Absent => kind == ValueKind::Absent,
            Type::Bool => kind == ValueKind::Bool,
            Type::Int => kind == ValueKind::Int,
            Type::Float => kind == ValueKind::Float,
            Type::Number => matches!(kind, ValueKind::Int | ValueKind::Float),
            Type::Text | Type::Enum(_) => kind == ValueKind::Text,
            Type::Union(types) => types.iter().any(|ty| accepts(ty, kind)),
            _ => false,
        }
    }
    let choices: Vec<Vec<_>> = definition
        .signature
        .params
        .iter()
        .map(|param| {
            kinds
                .iter()
                .copied()
                .filter(|(_, kind)| {
                    accepts(&param.ty, *kind) || param.optional && *kind == ValueKind::Absent
                })
                .collect()
        })
        .collect();
    let combinations = choices
        .iter()
        .try_fold(1usize, |count, choices| count.checked_mul(choices.len()))?;
    let mut evaluator = Evaluator {
        library,
        callees: Vec::new(),
        used: 0,
    };
    let mut arms: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut max_depth = 1;
    for mask in 0..combinations {
        let mut state = State {
            depth: 1,
            ..State::default()
        };
        let mut selected = mask;
        let mut pattern = Vec::new();
        for (index, param) in definition.signature.params.iter().enumerate() {
            let (name, kind) = choices[index][selected % choices[index].len()];
            selected /= choices[index].len();
            pattern.push(format!("Value::{name}"));
            state.vars.insert(
                param.name.clone(),
                Symbol {
                    code: format!("args[{index}]"),
                    constant: match kind {
                        ValueKind::Null => Some(Value::Null),
                        ValueKind::Absent => Some(Value::Absent),
                        _ => None,
                    },
                    kind: Some(kind),
                    calls: 0,
                },
            );
        }
        evaluator.used = 0;
        let results: Vec<_> = evaluator
            .block(&body.block, &mut state, 1)
            .into_iter()
            .filter(|(_, value)| value.as_ref().is_none_or(|value| value.calls <= 1))
            .collect();
        if results.is_empty() {
            continue;
        }
        let mut code = String::new();
        for (state, value) in results {
            let value = value.or_else(|| Symbol::constant(Value::Null))?;
            max_depth = max_depth.max(state.depth);
            let _ = writeln!(code, "            if let Some(value) = (|| {{");
            for step in &state.steps {
                let _ = writeln!(code, "                {step}");
            }
            let _ = writeln!(
                code,
                "                Some({})\n            }})() {{ return Some(value); }}",
                value.code
            );
        }
        arms.entry(code)
            .or_default()
            .push(format!("[{}]", pattern.join(", ")));
    }
    if arms.is_empty() {
        return None;
    }
    let mut descriptor = format!(
        "Some(p::HelperTemplate {{ entry: {ident}_template, depth: {max_depth}, callees: &["
    );
    for callee in &evaluator.callees {
        let _ = write!(descriptor, "p::function({:?}), ", callee.to_string());
    }
    descriptor.push_str("] })");
    let code = format!(
        "fn {ident}_template(args: &[Value], natives: &[lash_kernel_doc::PreparedNative]) -> Option<Value> {{\n    let _ = natives;\n    match args {{\n{}\n        _ => None,\n    }}\n}}\n",
        arms.into_iter()
            .map(|(code, patterns)| format!(
                "        {} => {{\n{code}            None\n        }},",
                patterns.join(" | ")
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
    Some((descriptor, code))
}
