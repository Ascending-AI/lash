//! Element proofs run the existing symbolic evaluator under the operation's
//! admission predicates. No helper name participates in the proof.

use super::*;
use lash_kernel_doc::plan::{Element, NativeFact};
use lash_kernel_doc::scalar::ScalarOp;
use lash_kernel_doc::{NativeSpecialization, ResidentRead, SymbolicNative};

const KEY: &str = "args[1]";
const POSITION: &str = "element_position";
const LENGTH: &str = "element_length";
const FLOAT_LENGTH: &str = "element_float_length";
const ITEM: &str = "element_item";
const TUPLE_LENGTH: &str = "element_tuple_length";

pub(super) struct Proof {
    operation: Element,
    kind: ValueKind,
    hole: bool,
    requires: Vec<(FunctionId, NativeFact)>,
}

fn symbol(code: &str, kind: ValueKind) -> Symbol {
    Symbol::computed(code.into(), Some(kind), 0)
}

fn key(value: &Symbol) -> bool {
    matches!(value.code.as_str(), KEY | POSITION)
}

fn length(value: &Symbol) -> bool {
    matches!(value.code.as_str(), LENGTH | FLOAT_LENGTH)
}

fn number(value: &Symbol) -> Option<f64> {
    match value.constant? {
        Value::Int(value) => Some(value as f64),
        Value::Float(value) => Some(value.get()),
        _ => None,
    }
}

impl Proof {
    fn require(&mut self, function: FunctionId, fact: NativeFact) {
        if !self.requires.contains(&(function, fact)) {
            self.requires.push((function, fact));
        }
    }

    pub(super) fn heap(&self, form: &Form, state: &mut State) -> Option<Symbol> {
        match form {
            Form::Len {
                collection: "List",
                target,
            } if target == "args[0]" => Some(symbol(LENGTH, ValueKind::Int)),
            Form::Len {
                collection: "Tuple",
                target,
            } if target == ITEM => {
                if self.kind != ValueKind::Tuple {
                    return None;
                }
                if self.hole {
                    Symbol::constant(Value::Int(0))
                } else {
                    Some(symbol(TUPLE_LENGTH, ValueKind::Int))
                }
            }
            Form::Index { target, index }
                if target == "args[0]" && matches!(index.as_str(), KEY | POSITION) =>
            {
                state.steps.push(Step::Heap {
                    name: ITEM.into(),
                    form: form.clone(),
                });
                Some(symbol(ITEM, self.kind))
            }
            _ => None,
        }
    }

    pub(super) fn call(
        &mut self,
        function: FunctionId,
        native: &NativeSpecialization,
        args: &[Symbol],
    ) -> Option<Symbol> {
        if let Some(read) = native.resident {
            let form = super::read(read, args)?;
            let answer = self.heap(&form, &mut State::default())?;
            self.require(function, NativeFact::Resident(read));
            return Some(answer);
        }
        let fact = native.symbolic?;
        self.require(function, NativeFact::Symbolic(fact));
        // A constant computation still uses the native's actual entry. Only
        // certified semantics may erase it from an element proof.
        if let Some(values) = args
            .iter()
            .map(|arg| arg.constant)
            .collect::<Option<Vec<_>>>()
        {
            if !matches!(fact, SymbolicNative::Kind | SymbolicNative::Same)
                && !values
                    .iter()
                    .all(|value| matches!(value, Value::Int(_) | Value::Float(_)))
            {
                return None;
            }
            return Symbol::constant((native.immediate?.entry)(&values)?);
        }
        match (fact, args) {
            (SymbolicNative::Kind, [arg]) => {
                let name = ValueKind::NAMES[arg.kind? as usize].1;
                Symbol::constant(Value::Text(lash_kernel_doc::arena::ShortText::new(name)?))
            }
            (SymbolicNative::ToFloat, [arg]) if key(arg) => Some(arg.clone()),
            (SymbolicNative::ToFloat, [arg]) if length(arg) => {
                Some(symbol(FLOAT_LENGTH, ValueKind::Float))
            }
            (SymbolicNative::Floor, [arg]) if key(arg) => Some(arg.clone()),
            (SymbolicNative::NumberIndex, [arg, bound])
                if key(arg)
                    && bound.kind == Some(ValueKind::Float)
                    && (self.operation == Element::Read && length(bound)
                        || number(bound)
                            == Some(lash_kernel_doc::plan::ELEMENT_POSITIONS as f64)) =>
            {
                Some(symbol(POSITION, ValueKind::Float))
            }
            (SymbolicNative::Same, [a, b]) => self
                .same(a, b)
                .and_then(|value| Symbol::constant(Value::Bool(value))),
            (SymbolicNative::Numeric(op), [a, b]) => self.numeric(op, a, b),
            _ => None,
        }
    }

    fn same(&self, a: &Symbol, b: &Symbol) -> Option<bool> {
        if a.code == b.code {
            return Some(true);
        }
        if a.kind != b.kind && a.kind.is_some() && b.kind.is_some() {
            return Some(false);
        }
        if key(a) && number(b).is_some_and(|n| n < 0.0)
            || key(b) && number(a).is_some_and(|n| n < 0.0)
            || a.code == TUPLE_LENGTH && number(b) == Some(0.0)
            || b.code == TUPLE_LENGTH && number(a) == Some(0.0)
        {
            return Some(false);
        }
        None
    }

    fn numeric(&self, op: ScalarOp, a: &Symbol, b: &Symbol) -> Option<Symbol> {
        if op == ScalarOp::Add && key(a) && number(b) == Some(0.0) {
            return if matches!(b.constant, Some(Value::Float(value)) if value.get().is_sign_negative())
            {
                Some(a.clone())
            } else {
                Some(symbol(POSITION, ValueKind::Float))
            };
        }
        // Compare symbolic mathematical values. The key is whole, finite,
        // nonnegative and below ELEMENT_POSITIONS. A read also has key < len;
        // a write permits key == len. A non-hole tuple has positive length.
        use std::cmp::Ordering::{Equal, Greater, Less};
        let order = if key(a) && key(b) {
            Some(Equal)
        } else if key(a) && length(b) && self.operation == Element::Read
            || number(a).is_some_and(|n| n < 0.0) && key(b)
            || key(a)
                && number(b).is_some_and(|n| n >= lash_kernel_doc::plan::ELEMENT_POSITIONS as f64)
        {
            Some(Less)
        } else if length(a) && key(b) && self.operation == Element::Read
            || key(a) && number(b).is_some_and(|n| n < 0.0)
            || a.code == TUPLE_LENGTH && number(b) == Some(0.0)
        {
            Some(Greater)
        } else {
            None
        };
        let truth = match (op, order) {
            (ScalarOp::Eq, Some(order)) => order == Equal,
            (ScalarOp::Lt, Some(order)) => order == Less,
            (ScalarOp::Le, Some(order)) => order != Greater,
            (ScalarOp::Gt, Some(order)) => order == Greater,
            (ScalarOp::Ge, Some(order)) => order != Less,
            (ScalarOp::Le, None) if number(a) == Some(0.0) && key(b) => true,
            (ScalarOp::Lt, None) if key(a) && number(b) == Some(0.0) => false,
            (ScalarOp::Lt, None) if length(a) && key(b) => false,
            _ => return None,
        };
        Symbol::constant(Value::Bool(truth))
    }
}

pub(super) fn derive(library: &Library, definition: &FunctionDefinition) -> Option<String> {
    if definition.signature.result != lash_kernel_value::Type::Any
        || definition
            .signature
            .params
            .iter()
            .enumerate()
            .any(|(index, param)| {
                if index == 1 {
                    !accepts(&param.ty, ValueKind::Float)
                } else {
                    param.ty != lash_kernel_value::Type::Any
                }
            })
    {
        return None;
    }
    let operation = match definition.signature.params.len() {
        2 => Element::Read,
        3 => Element::Write,
        _ => return None,
    };
    let body = definition.body()?;
    let mut requires = Vec::new();
    let mut depth = 1;
    // A read must preserve every kind, both empty and nonempty tuples.
    // A write's third operand stays wholly unknown, so it cannot be inspected.
    let cases: Vec<_> = if operation == Element::Read {
        ValueKind::NAMES
            .iter()
            .map(|(kind, _)| (*kind, false))
            .chain(std::iter::once((ValueKind::Tuple, true)))
            .collect()
    } else {
        vec![(ValueKind::Absent, false)]
    };
    for (kind, hole) in cases {
        let mut evaluator = Evaluator {
            library,
            callees: Vec::new(),
            heap_callees: Vec::new(),
            used: 0,
            heap: true,
            captured: captured(&body.block, library)?,
            element: Some(Proof {
                operation,
                kind,
                hole,
                requires: Vec::new(),
            }),
        };
        let mut state = State {
            heap: true,
            charging: matches!(definition.metering(), lash_kernel_doc::Metering::Statements)
                .then_some(definition.identity()),
            ..Default::default()
        };
        for (index, param) in definition.signature.params.iter().enumerate() {
            state.vars.insert(
                param.name.clone(),
                Symbol::computed(
                    format!("args[{index}]"),
                    match index {
                        0 => Some(ValueKind::List),
                        1 => Some(ValueKind::Float),
                        _ => None,
                    },
                    0,
                ),
            );
        }
        let outcomes = evaluator.block(&body.block, &mut state, 1);
        let [(state, Some(value))] = outcomes.as_slice() else {
            return None;
        };
        if state
            .steps
            .iter()
            .filter(|step| matches!(step, Step::Bind { .. }))
            .count()
            > 2
        {
            return None;
        }
        depth = depth.max(state.depth);
        let effects: Vec<_> = state
            .steps
            .iter()
            .filter(|step| !matches!(step, Step::Bind { .. }))
            .collect();
        let valid = match (operation, effects.as_slice()) {
            (
                Element::Read,
                [
                    Step::Heap {
                        name,
                        form: Form::Index { target, index },
                    },
                ],
            ) => {
                name == ITEM
                    && target == "args[0]"
                    && matches!(index.as_str(), KEY | POSITION)
                    && if hole {
                        matches!(value.constant, Some(Value::Absent))
                    } else {
                        value.code == ITEM
                    }
            }
            (
                Element::Write,
                [
                    Step::SetIndex {
                        target,
                        index,
                        value: assigned,
                    },
                ],
            ) => {
                target == "args[0]"
                    && matches!(index.as_str(), KEY | POSITION)
                    && assigned == "args[2]"
                    && value.code == "args[2]"
            }
            _ => false,
        };
        if !valid {
            return None;
        }
        for requirement in evaluator.element?.requires {
            if !requires.contains(&requirement) {
                requires.push(requirement);
            }
        }
    }
    describe(operation, depth, &requires)
}

pub(super) fn describe(
    operation: Element,
    depth: u32,
    requires: &[(FunctionId, NativeFact)],
) -> Option<String> {
    let mut code = format!(
        "Some(p::ElementPath {{ operation: p::Element::{operation:?}, depth: {depth}, room: 2, requires: &["
    );
    for (function, fact) in requires {
        let fact = match *fact {
            NativeFact::IntegerPair(op) => {
                format!("p::NativeFact::IntegerPair(lash_kernel_doc::scalar::ScalarOp::{op:?})")
            }
            NativeFact::Symbolic(SymbolicNative::Numeric(op)) => format!(
                "p::NativeFact::Symbolic(lash_kernel_doc::SymbolicNative::Numeric(lash_kernel_doc::scalar::ScalarOp::{op:?}))"
            ),
            NativeFact::Symbolic(fact) => {
                format!("p::NativeFact::Symbolic(lash_kernel_doc::SymbolicNative::{fact:?})")
            }
            NativeFact::Resident(ResidentRead::Length(collection)) => format!(
                "p::NativeFact::Resident(lash_kernel_doc::ResidentRead::Length(p::Collection::{collection:?}))"
            ),
            NativeFact::Resident(_) => return None,
        };
        let _ = write!(
            code,
            "p::NativeRequirement {{ function: p::function({:?}), fact: {fact} }},",
            function.to_string()
        );
    }
    code.push_str("] })");
    Some(code)
}
