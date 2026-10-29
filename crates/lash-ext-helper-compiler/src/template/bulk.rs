//! Source-derived copy induction and ordinary read-closure charges.
use super::*;
use lash_kernel_doc::plan::Element;
use lash_kernel_doc::scalar::ScalarOp;
use lash_kernel_doc::{Formula, Measure, Operand, ResidentRead, SymbolicNative};

#[derive(Clone, Copy)]
struct Amount {
    fixed: u64,
    nested: u64,
    unknown: bool,
}
impl Amount {
    fn constant(fixed: u64) -> Self {
        Self {
            fixed,
            nested: 0,
            unknown: false,
        }
    }
    fn add(self, other: Self) -> Option<Self> {
        Some(Self {
            fixed: self.fixed.checked_add(other.fixed)?,
            nested: self.nested.checked_add(other.nested)?,
            unknown: self.unknown || other.unknown,
        })
    }
}

fn measurement(symbol: &Symbol, measure: Measure) -> Option<Amount> {
    if measure == Measure::Magnitude {
        return None;
    }
    if symbol.code == "element_item" {
        return Some(match measure {
            Measure::NestedSize => Amount {
                fixed: 0,
                nested: 1,
                unknown: false,
            },
            Measure::Size => Amount {
                fixed: 1,
                nested: 0,
                unknown: true,
            },
            Measure::DeepSize => Amount {
                fixed: 1,
                nested: 1,
                unknown: true,
            },
            Measure::Magnitude => return None,
        });
    }
    let size = match symbol.constant {
        Some(Value::Int(word)) => 1 + u64::from(lash_kernel_doc::scalar::word_size_bit(word)),
        Some(Value::Text(text)) => 1 + text.as_bytes().len() as u64,
        Some(Value::Null | Value::Absent | Value::Bool(_) | Value::Float(_)) => 1,
        Some(_) => return None,
        None => match symbol.code.as_str() {
            "element_length" => 2, // A read's index is below this nonempty length.
            "args[1]" | "element_position" | "element_float_length" | "element_unit" => 1,
            _ => return None,
        },
    };
    Some(Amount::constant(if measure == Measure::NestedSize {
        0
    } else {
        size
    }))
}

fn formula(
    form: &Formula,
    measure: &impl Fn(&Operand, Measure) -> Option<Amount>,
) -> Option<Amount> {
    match form {
        Formula::Constant(n) => Some(Amount::constant(*n)),
        Formula::Size(value) => measure(value, Measure::Size),
        Formula::DeepSize(value) => measure(value, Measure::DeepSize),
        Formula::NestedSize(value) => measure(value, Measure::NestedSize),
        Formula::Magnitude(value) => measure(value, Measure::Magnitude),
        Formula::Sum(terms) => terms.iter().try_fold(Amount::constant(0), |total, term| {
            total.add(formula(term, measure)?)
        }),
        Formula::Product(terms) => terms.iter().try_fold(Amount::constant(1), |a, term| {
            let b = formula(term, measure)?;
            if a.unknown || b.unknown || a.nested != 0 && b.nested != 0 {
                return None;
            }
            Some(Amount {
                fixed: a.fixed.checked_mul(b.fixed)?,
                nested: a
                    .nested
                    .checked_mul(b.fixed)?
                    .checked_add(b.nested.checked_mul(a.fixed)?)?,
                unknown: false,
            })
        }),
        Formula::Min(terms) | Formula::Max(terms) => {
            let terms = terms
                .iter()
                .map(|term| formula(term, measure))
                .collect::<Option<Vec<_>>>()?;
            if matches!(form, Formula::Min(_)) {
                // Unknown sizes have a proven lower bound of one. This is
                // the canonical same(item, empty-tuple) size minimum.
                if let Some(constant) = terms
                    .iter()
                    .filter(|t| !t.unknown && t.nested == 0)
                    .map(|t| t.fixed)
                    .min()
                    && terms.iter().all(|term| term.fixed >= constant)
                {
                    return Some(Amount::constant(constant));
                }
            }
            if terms.iter().any(|t| t.unknown || t.nested != 0) {
                return None;
            }
            Some(Amount::constant(if matches!(form, Formula::Min(_)) {
                terms.iter().map(|t| t.fixed).min()?
            } else {
                terms.iter().map(|t| t.fixed).max()?
            }))
        }
        Formula::CeilLog2(term) => {
            let amount = formula(term, measure)?;
            if amount.unknown || amount.nested != 0 {
                return None;
            }
            Some(Amount::constant(
                Formula::CeilLog2(Box::new(Formula::Constant(amount.fixed)))
                    .evaluate(&mut |_, _| 0),
            ))
        }
    }
}

struct Description<'a> {
    library: &'a Library,
    requires: Vec<(FunctionId, lash_kernel_doc::plan::NativeFact)>,
    depth: u32,
    room: usize,
}
impl<'a> Description<'a> {
    fn new(library: &'a Library) -> Self {
        Self {
            library,
            requires: Vec::new(),
            depth: 1,
            room: 0,
        }
    }
    fn evaluate(
        &mut self,
        function: FunctionId,
        args: Vec<Symbol>,
        kind: ValueKind,
        hole: bool,
    ) -> Option<(State, Symbol)> {
        let mut evaluator = Evaluator {
            library: self.library,
            callees: Vec::new(),
            heap_callees: Vec::new(),
            used: 0,
            heap: true,
            captured: Default::default(),
            element: Some(element::Proof {
                copy: true,
                operation: Element::Read,
                kind,
                hole,
                requires: Vec::new(),
            }),
        };
        let mut state = State {
            describe: true,
            heap: true,
            ..Default::default()
        };
        let value = evaluator.call(function, args, &mut state, 1)?;
        self.depth = self.depth.max(state.depth);
        self.room = self.room.max(
            state
                .steps
                .iter()
                .filter(|s| matches!(s, Step::Bind { .. }))
                .count(),
        );
        for requirement in evaluator.element?.requires {
            if !self.requires.contains(&requirement) {
                self.requires.push(requirement);
            }
        }
        Some((state, value))
    }
    fn helper(
        &mut self,
        rhs: &Rhs,
        variables: &[(&Name, Symbol)],
        expected: &str,
        kind: ValueKind,
        hole: bool,
    ) -> Option<()> {
        let (function, args) = call(rhs)?;
        let args = args
            .iter()
            .map(|arg| match arg {
                Expr::Variable(name) => variables
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.clone()),
                Expr::Literal(value) => Evaluator::literal(value),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        let (state, value) = self.evaluate(function, args, kind, hole)?;
        if value.code != expected
            || state
                .steps
                .iter()
                .any(|step| !matches!(step, Step::Bind { .. } | Step::Meter(_)))
        {
            return None;
        }
        Some(())
    }
    fn native(
        &mut self,
        function: FunctionId,
        fact: lash_kernel_doc::plan::NativeFact,
    ) -> Option<()> {
        let registered = self.library.registry.get(&function)?;
        if registered.definition().guard.is_some()
            || !fact.accepts(&registered.native()?.specialization())
        {
            return None;
        }
        if !self.requires.contains(&(function, fact)) {
            self.requires.push((function, fact));
        }
        Some(())
    }
    fn read(&mut self, rhs: &Rhs, source: &Name, index: &Name) -> Option<u64> {
        let (function, args) = call(rhs)?;
        if args
            != [
                Expr::Variable(source.clone()),
                Expr::Variable(index.clone()),
            ]
        {
            return None;
        }
        let mut price = None;
        for (kind, hole) in ValueKind::NAMES
            .iter()
            .map(|(kind, _)| (*kind, false))
            .chain([(ValueKind::Tuple, true)])
        {
            let (state, value) = self.evaluate(
                function,
                vec![
                    symbol("args[0]", ValueKind::List),
                    symbol("args[1]", ValueKind::Float),
                ],
                kind,
                hole,
            )?;
            if if hole {
                !matches!(value.constant, Some(Value::Absent))
            } else {
                value.code != "element_item"
            } {
                return None;
            }
            if state
                .steps
                .iter()
                .filter(|step| {
                    matches!(
                        step,
                        Step::Heap {
                            form: Form::Index { .. },
                            ..
                        }
                    )
                })
                .count()
                != 1
                || state.steps.iter().any(|step| {
                    !matches!(
                        step,
                        Step::Heap {
                            form: Form::Index { .. },
                            ..
                        } | Step::Bind { .. }
                            | Step::Meter(_)
                    )
                })
            {
                return None;
            }
            let mut total = Amount::constant(0);
            for charge in &state.charges {
                let amount = match charge {
                    SourceCharge::Form(form) => Amount::constant(form.measurement().0),
                    SourceCharge::Bill {
                        function,
                        args,
                        result,
                    } => {
                        let definition = self.library.definition(function)?;
                        formula(definition.charge()?, &|operand, measure| {
                            let symbol = match operand {
                                Operand::Result => result,
                                Operand::Param(name) => args.get(
                                    definition
                                        .signature
                                        .params
                                        .iter()
                                        .position(|p| p.name == *name)?,
                                )?,
                            };
                            measurement(symbol, measure)
                        })?
                    }
                };
                total = total.add(amount)?;
            }
            if total.unknown || total.nested != 1 || price.is_some_and(|price| price != total.fixed)
            {
                return None;
            }
            price = Some(total.fixed);
        }
        price
    }
    fn append(&mut self, stmt: &Stmt, output: &Name, value: &Name) -> Option<()> {
        let Stmt::Assign {
            place: Place::Member(Member::Index { target, index }),
            value: Rhs::Expr(Expr::Variable(actual)),
            ..
        } = stmt
        else {
            return None;
        };
        if *target != Expr::Variable(output.clone()) || actual != value {
            return None;
        }
        let Expr::Call { function, args } = index else {
            return None;
        };
        if args != &[Expr::Variable(output.clone())] {
            return None;
        }
        self.native(
            *function,
            lash_kernel_doc::plan::NativeFact::Resident(ResidentRead::Length(
                lash_kernel_doc::plan::Collection::List,
            )),
        )
    }
    fn loop_body(
        &mut self,
        stmt: &Stmt,
        source: &Name,
        length: &Name,
        index: &Name,
        output: &Name,
        dense: bool,
    ) -> Option<(u64, u64)> {
        let Stmt::While {
            metered,
            condition,
            body,
            else_block,
        } = stmt
        else {
            return None;
        };
        if !else_block.is_empty() || !dense && *metered != 0 {
            return None;
        }
        let Expr::Call { function, args } = condition else {
            return None;
        };
        if args
            != &[
                Expr::Variable(index.clone()),
                Expr::Variable(length.clone()),
            ]
        {
            return None;
        }
        self.native(
            *function,
            lash_kernel_doc::plan::NativeFact::Symbolic(SymbolicNative::Numeric(ScalarOp::Lt)),
        )?;
        let (read, value, append, increment) = if dense {
            let [Stmt::Let { name, value }, append, increment] = &body[..] else {
                return None;
            };
            (value, name, append, increment)
        } else {
            let [
                Stmt::Let {
                    name: present,
                    value: membership,
                },
                Stmt::Let {
                    name: value,
                    value: Rhs::Expr(Expr::Tuple(empty)),
                },
                Stmt::If {
                    condition: Expr::Variable(test),
                    then_block,
                    else_block,
                },
                append,
                increment,
            ] = &body[..]
            else {
                return None;
            };
            if !empty.is_empty() || present != test || !else_block.is_empty() {
                return None;
            }
            for hole in [false, true] {
                let expected = format!("Value::Bool({})", !hole);
                // Membership reads the item too; no ordinary closure or guest effect may survive it.
                let (function, args) = call(membership)?;
                if args
                    != [
                        Expr::Variable(source.clone()),
                        Expr::Variable(index.clone()),
                    ]
                {
                    return None;
                }
                let (state, result) = self.evaluate(
                    function,
                    vec![
                        symbol("args[0]", ValueKind::List),
                        symbol("args[1]", ValueKind::Float),
                    ],
                    ValueKind::Tuple,
                    hole,
                )?;
                if result.code != expected
                    || !state.charges.is_empty()
                    || state.steps.iter().any(|s| {
                        !matches!(
                            s,
                            Step::Heap {
                                form: Form::Index { .. },
                                ..
                            } | Step::Bind { .. }
                        )
                    })
                {
                    return None;
                }
            }
            let [
                Stmt::Assign {
                    place: Place::Variable(destination),
                    value: read,
                    ..
                },
            ] = &then_block[..]
            else {
                return None;
            };
            if destination != value {
                return None;
            }
            (read, value, append, increment)
        };
        self.append(append, output, value)?;
        let Stmt::Assign {
            place: Place::Variable(destination),
            value: Rhs::Expr(Expr::Call { function, args }),
            ..
        } = increment
        else {
            return None;
        };
        if destination != index
            || args.len() != 2
            || args[0] != Expr::Variable(index.clone())
            || !matches!(&args[1], Expr::Literal(Literal::Int(n)) if n.word() == Some(1))
        {
            return None;
        }
        self.native(
            *function,
            lash_kernel_doc::plan::NativeFact::Symbolic(SymbolicNative::Numeric(ScalarOp::Add)),
        )?;
        Some((self.read(read, source, index)?, *metered))
    }
}

fn symbol(code: &str, kind: ValueKind) -> Symbol {
    Symbol::computed(code.into(), Some(kind), 0)
}
fn call(rhs: &Rhs) -> Option<(FunctionId, Vec<Expr>)> {
    match rhs {
        Rhs::Expr(Expr::Call { function, args }) => Some((*function, args.clone())),
        Rhs::Expr(Expr::Act(act)) => {
            let ActKind::Call(Callee::Local(function)) = act.kind else {
                return None;
            };
            Some((function, act.args.clone()))
        }
        Rhs::Action(Action::Call {
            callee: Callee::Local(function),
            args,
        }) => Some((
            *function,
            args.iter()
                .map(|arg| match arg {
                    Atom::Variable(name) => Expr::Variable(name.clone()),
                    Atom::Literal(value) => Expr::Literal(value.clone()),
                })
                .collect(),
        )),
        _ => None,
    }
}
fn empty(rhs: &Rhs) -> bool {
    matches!(rhs, Rhs::Expr(Expr::List(items)) if items.is_empty())
}
fn zero(rhs: &Rhs) -> bool {
    matches!(rhs, Rhs::Expr(Expr::Literal(Literal::Float(n))) if n.get().to_bits() == 0)
}
fn returned(stmt: &Stmt, name: &Name) -> bool {
    matches!(stmt, Stmt::Return { value: Expr::Variable(value) } if value == name)
}

pub(super) fn emit(
    library: &Library,
    definition: &FunctionDefinition,
    ident: &str,
) -> Option<(String, String)> {
    // Keep the source induction for ledger verification. Execution uses the
    // original body in the common packet stream.
    if definition.charge().is_none() || definition.guard.is_some() {
        return None;
    }
    let mut proof = Description::new(library);
    let params = &definition.signature.params;
    let (dense, units, loop_units) = match params.as_slice() {
        [source] => {
            let [
                Stmt::Let {
                    name: length,
                    value: length_call,
                },
                Stmt::If {
                    condition: guard,
                    else_block,
                    ..
                },
                Stmt::Let {
                    name: output,
                    value: output_value,
                },
                Stmt::Let {
                    name: index,
                    value: initial,
                },
                while_,
                return_,
            ] = &definition.body()?.block[..]
            else {
                return None;
            };
            if !empty(output_value)
                || !zero(initial)
                || !else_block.is_empty()
                || !returned(return_, output)
            {
                return None;
            }
            proof.helper(
                length_call,
                &[(&source.name, symbol("args[0]", ValueKind::List))],
                "element_float_length",
                ValueKind::Absent,
                false,
            )?;
            let Expr::Call { function, args } = guard else {
                return None;
            };
            if args.len() != 2
                || !matches!(&args[0], Expr::Literal(Literal::Float(n)) if n.get() == lash_kernel_doc::plan::ELEMENT_POSITIONS as f64)
                || args[1] != Expr::Variable(length.clone())
            {
                return None;
            }
            proof.native(
                *function,
                lash_kernel_doc::plan::NativeFact::Symbolic(SymbolicNative::Numeric(ScalarOp::Lt)),
            )?;
            let (units, metered) =
                proof.loop_body(while_, &source.name, length, index, output, true)?;
            (true, units, metered)
        }
        [receiver, arguments] => {
            let [
                Stmt::Do { value: require },
                Stmt::Let {
                    name: sources,
                    value: Rhs::Expr(Expr::List(initial)),
                },
                Stmt::For {
                    binding: added,
                    iterable,
                    via: None,
                    body: add,
                    metered: 0,
                    else_block: add_else,
                },
                Stmt::Let {
                    name: output,
                    value: output_value,
                },
                Stmt::For {
                    binding: source,
                    iterable: Expr::Variable(walked),
                    via: None,
                    body: copy,
                    metered: 0,
                    else_block: copy_else,
                },
                return_,
            ] = &definition.body()?.block[..]
            else {
                return None;
            };
            if initial != &[Expr::Variable(receiver.name.clone())]
                || *iterable != Expr::Variable(arguments.name.clone())
                || sources != walked
                || !add_else.is_empty()
                || !copy_else.is_empty()
                || !empty(output_value)
                || !returned(return_, output)
            {
                return None;
            }
            let [add] = &add[..] else {
                return None;
            };
            proof.append(add, sources, added)?;
            let [
                Stmt::Let {
                    name: array,
                    value: is_array,
                },
                Stmt::If {
                    condition: Expr::Variable(test),
                    then_block,
                    else_block,
                },
            ] = &copy[..]
            else {
                return None;
            };
            if array != test {
                return None;
            }
            let [append] = &else_block[..] else {
                return None;
            };
            proof.append(append, output, source)?;
            for (kind, _) in ValueKind::NAMES.iter().filter(|(kind, _)| {
                !matches!(
                    kind,
                    ValueKind::Null | ValueKind::Absent | ValueKind::Record
                )
            }) {
                let argument = symbol("args[0]", *kind);
                proof.helper(
                    &Rhs::Expr(require.clone()),
                    &[(&receiver.name, argument.clone())],
                    "args[0]",
                    *kind,
                    false,
                )?;
                proof.helper(
                    is_array,
                    &[(source, argument)],
                    &format!("Value::Bool({})", *kind == ValueKind::List),
                    *kind,
                    false,
                )?;
            }
            let [
                Stmt::Let {
                    name: length,
                    value: length_call,
                },
                Stmt::Let {
                    name: index,
                    value: initial,
                },
                while_,
            ] = &then_block[..]
            else {
                return None;
            };
            if !zero(initial) {
                return None;
            }
            proof.helper(
                length_call,
                &[(source, symbol("args[0]", ValueKind::List))],
                "element_float_length",
                ValueKind::Absent,
                false,
            )?;
            let (units, metered) = proof.loop_body(while_, source, length, index, output, false)?;
            (false, units, metered)
        }
        _ => return None,
    };
    let depth = proof.depth.checked_add(1)?;
    let room = definition
        .body()?
        .block
        .iter()
        .map(Node::Stmt)
        .flat_map(|node| node.children())
        .count()
        .checked_add(proof.room)?;
    let read = element::describe_room(
        Element::Read,
        depth,
        u32::try_from(proof.room).ok()?,
        &proof.requires,
    )?;
    let read = read.strip_prefix("Some(")?.strip_suffix(')')?;
    let kind = if dense { "Dense" } else { "Concat" };
    let copy_ident = format!("{ident}_copy").to_uppercase();
    let code = format!(
        "static {copy_ident}: p::CopyLoop = p::CopyLoop {{ kind: p::CopyKind::{kind}, read: {read}, read_units: {units}, loop_units: {loop_units}, temporary_values: {room} }};\n"
    );
    let descriptor = format!(
        "Some(p::HelperTemplate {{ entry: None, metered: None, depth: {depth}, callees: &[], heap: Some(p::HeapTemplate {{ entry: None, depth: {depth}, writes: false, copy: Some(&{copy_ident}), element: None, record_write: None, callees: &[] }}), forwards: p::Forwards {{ floats: None, ints: None, texts: None }} }})"
    );
    Some((descriptor, code))
}
