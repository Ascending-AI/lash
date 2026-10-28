//! BigInt tokens and authored input conversion share construction and recovery.
use lash_kernel_dialect::construct::{Atom, Expression as E};
use lash_kernel_doc::{self as k, Expr, Integer, Literal};
use lash_kernel_value::FunctionId;

pub(crate) const BIGINT_TAG: &str = "ts.bigint";
const BIGINT_LITERAL: E = E::Tuple(&[E::Atom(Atom::Text(BIGINT_TAG)), E::Input(0)]);

fn bigint_field(is: impl Fn(&E) -> bool) -> usize {
    let E::Tuple(fields) = BIGINT_LITERAL else {
        unreachable!("the BigInt token is a tuple")
    };
    fields
        .iter()
        .position(is)
        .unwrap_or_else(|| unreachable!("the declared token contains its tag and value"))
}
pub(crate) fn bigint_tag_slot() -> usize {
    bigint_field(|field| matches!(field, E::Atom(Atom::Text(tag)) if *tag == BIGINT_TAG))
}
pub(crate) fn bigint_value_slot() -> usize {
    bigint_field(|field| matches!(field, E::Input(0)))
}

pub(crate) fn bigint_literal(value: Integer) -> Expr {
    BIGINT_LITERAL
        .emit(&[Expr::Literal(Literal::Int(value))], &mut |_,
                                                           _|
         -> Result<
            Expr,
            std::convert::Infallible,
        > {
            unreachable!("a BigInt literal contains no calls")
        })
        .unwrap_or_else(|never| match never {})
}
pub(crate) fn bigint_value(value: &Expr) -> Option<&Integer> {
    let inputs = BIGINT_LITERAL.recognize(value, &|_, _| false)?;
    let [value] = inputs.as_slice() else {
        return None;
    };
    match *value {
        Expr::Literal(Literal::Int(value)) => Some(value),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputPart {
    Value,
    Shape,
}
pub(crate) struct InputConversion {
    pub helper: &'static str,
    pub scalar: bool,
    order: [InputPart; 2],
}
pub(crate) const BIGINT_INPUT: InputConversion = InputConversion {
    helper: "ts.bigint.input",
    scalar: true,
    order: [InputPart::Value, InputPart::Shape],
};
impl InputConversion {
    pub fn arguments<T>(&self, value: T, shape: T) -> [T; 2] {
        let mut value = Some(value);
        let mut shape = Some(shape);
        self.order.map(|part| {
            match part {
                InputPart::Value => value.take(),
                InputPart::Shape => shape.take(),
            }
            .unwrap_or_else(|| unreachable!("each input part occurs once"))
        })
    }
    fn value<'a>(&self, args: &'a [Expr]) -> Option<&'a Expr> {
        if args.len() != self.order.len() {
            return None;
        }
        args.get(
            self.order
                .iter()
                .position(|part| *part == InputPart::Value)?,
        )
    }
    pub fn named<'a>(&self, name: &str, args: &'a [Expr]) -> Option<&'a Expr> {
        (name == self.helper).then(|| self.value(args)).flatten()
    }
    pub fn call<'a>(
        &self,
        id: &FunctionId,
        args: &'a [Expr],
        is: &impl Fn(&FunctionId, &str) -> bool,
    ) -> Option<&'a Expr> {
        is(id, self.helper).then(|| self.value(args)).flatten()
    }
    pub fn recognize<'a>(
        &self,
        value: &'a Expr,
        is: &impl Fn(&FunctionId, &str) -> bool,
    ) -> Option<&'a Expr> {
        let Expr::Act(act) = value else {
            return None;
        };
        let k::ActKind::Call(k::Callee::Local(id)) = &act.kind else {
            return None;
        };
        self.call(id, &act.args, is)
    }
}

/// Only authored bigint positions change the ordinary Number boundary.
pub(crate) fn input_shape(ty: &lash_kernel_value::Type) -> Option<Expr> {
    use lash_kernel_doc::RecordEntry;
    use lash_kernel_value::Type;
    let text = |value: &str| Expr::Literal(Literal::Text(value.into()));
    match ty {
        Type::Int => Some(Expr::Literal(Literal::Bool(BIGINT_INPUT.scalar))),
        Type::List(element) => {
            input_shape(element).map(|shape| Expr::Tuple(vec![text("list"), shape]))
        }
        Type::Tuple(elements) => {
            let shapes: Vec<_> = elements.iter().map(input_shape).collect();
            shapes.iter().any(Option::is_some).then(|| {
                Expr::Tuple(vec![
                    text("tuple"),
                    Expr::List(
                        shapes
                            .into_iter()
                            .map(|shape| shape.unwrap_or(Expr::Literal(Literal::Absent)))
                            .collect(),
                    ),
                ])
            })
        }
        Type::Record(record) => {
            let fields: Vec<_> = record
                .fields
                .iter()
                .filter_map(|field| {
                    input_shape(&field.ty).map(|value| RecordEntry {
                        field: field.name.clone(),
                        value,
                    })
                })
                .collect();
            let rest = record.rest.kept_type().and_then(input_shape);
            if fields.is_empty() && rest.is_none() {
                return None;
            }
            Some(Expr::Tuple(vec![
                text("record"),
                Expr::Record(fields),
                rest.unwrap_or(Expr::Literal(Literal::Absent)),
            ]))
        }
        Type::Union(members) => {
            let number = members
                .iter()
                .any(|member| matches!(member, Type::Number | Type::Float));
            let mut shapes: Vec<_> = members
                .iter()
                .filter(|member| !(number && matches!(member, Type::Int)))
                .filter_map(input_shape)
                .collect();
            match shapes.len() {
                0 => None,
                1 => shapes.pop(),
                _ => Some(Expr::List(shapes)),
            }
        }
        _ => None,
    }
}
