//! Bidirectional kernel expression recipes. A recipe emits and recognizes the same tree.
use lash_kernel_doc::{Expr, Literal};
use lash_kernel_value::FunctionId;

#[derive(Clone, Copy, Debug)]
pub enum Atom {
    Null,
    Int(i64),
    Bool(bool),
    Text(&'static str),
}
impl Atom {
    fn expression(self) -> Expr {
        Expr::Literal(match self {
            Self::Null => Literal::Null,
            Self::Int(value) => Literal::Int(value.into()),
            Self::Bool(value) => Literal::Bool(value),
            Self::Text(value) => Literal::Text(value.into()),
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Expression {
    Input(usize),
    Atom(Atom),
    Tuple(&'static [Self]),
    Call {
        function: &'static str,
        args: &'static [Self],
    },
}
impl Expression {
    pub fn emit<E>(
        &self,
        inputs: &[Expr],
        call: &mut impl FnMut(&str, Vec<Expr>) -> Result<Expr, E>,
    ) -> Result<Expr, E> {
        match self {
            Self::Input(index) => Ok(inputs[*index].clone()),
            Self::Atom(atom) => Ok(atom.expression()),
            Self::Tuple(items) => Ok(Expr::Tuple(
                items
                    .iter()
                    .map(|item| item.emit(inputs, call))
                    .collect::<Result<_, _>>()?,
            )),
            Self::Call { function, args } => {
                let args = args
                    .iter()
                    .map(|arg| arg.emit(inputs, call))
                    .collect::<Result<_, _>>()?;
                call(function, args)
            }
        }
    }
    pub fn recognize<'a>(
        &self,
        expression: &'a Expr,
        is: &impl Fn(&FunctionId, &str) -> bool,
    ) -> Option<Vec<&'a Expr>> {
        fn walk<'a>(
            pattern: &Expression,
            value: &'a Expr,
            inputs: &mut Vec<Option<&'a Expr>>,
            is: &impl Fn(&FunctionId, &str) -> bool,
        ) -> Option<()> {
            match (pattern, value) {
                (Expression::Input(index), value) => {
                    if inputs.len() <= *index {
                        inputs.resize(*index + 1, None);
                    }
                    match inputs[*index] {
                        Some(found) if found != value => None,
                        _ => {
                            inputs[*index] = Some(value);
                            Some(())
                        }
                    }
                }
                (Expression::Atom(atom), value) => (*value == atom.expression()).then_some(()),
                (Expression::Tuple(items), Expr::Tuple(values)) if items.len() == values.len() => {
                    for (pattern, value) in items.iter().zip(values) {
                        walk(pattern, value, inputs, is)?;
                    }
                    Some(())
                }
                (
                    Expression::Call { function, args },
                    Expr::Call {
                        function: id,
                        args: values,
                    },
                ) if is(id, function) && args.len() == values.len() => {
                    for (pattern, value) in args.iter().zip(values) {
                        walk(pattern, value, inputs, is)?;
                    }
                    Some(())
                }
                _ => None,
            }
        }
        let mut inputs = Vec::new();
        walk(self, expression, &mut inputs, is)?;
        inputs.into_iter().collect()
    }
}
