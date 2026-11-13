//! Recover nested assignment and iteration patterns from unpack projections.
use super::{Renderer, k};
use lash_kernel_value::Name;

enum Pattern {
    Name(Name),
    Tuple(Vec<(bool, Pattern)>),
}

impl Renderer<'_> {
    fn unpack_pattern(&self, statements: &[k::Stmt]) -> Option<(Pattern, k::Expr, usize)> {
        let k::Stmt::Let {
            name: packed,
            value,
        } = statements.first()?
        else {
            return None;
        };
        let value = match value {
            k::Rhs::Expr(value) => value.clone(),
            k::Rhs::Action(action) => action.clone().into(),
        };
        let k::Expr::Act(act) = value else {
            return None;
        };
        let k::ActKind::Call(k::Callee::Local(id)) = &act.kind else {
            return None;
        };
        let (input, count, star) = match act.args.as_slice() {
            [input, k::Expr::Literal(k::Literal::Int(count))]
                if self.operators.is(id, "py.unpack") =>
            {
                (input, count, None)
            }
            [
                input,
                k::Expr::Literal(k::Literal::Int(count)),
                k::Expr::Literal(k::Literal::Int(star)),
            ] if self.operators.is(id, "py.unpack.star") => {
                (input, count, Some(star.to_string().parse::<usize>().ok()?))
            }
            _ => return None,
        };
        let count = count.to_string().parse::<usize>().ok()?;
        if count >= statements.len() {
            return None;
        }
        let mut taken = 1;
        let mut elements = Vec::new();
        for index in 0..count {
            let expected = k::Expr::Member(Box::new(k::Member::Index {
                target: k::Expr::Variable(packed.clone()),
                index: k::Expr::Literal(k::Literal::Int(k::Integer::from(index as i64))),
            }));
            let pattern = if let Some((pattern, input, count)) =
                self.unpack_pattern(&statements[taken..])
                && input == expected
            {
                taken += count;
                pattern
            } else {
                let (name, value) = match statements.get(taken)? {
                    k::Stmt::Let {
                        name,
                        value: k::Rhs::Expr(value),
                    }
                    | k::Stmt::Assign {
                        place: k::Place::Variable(name),
                        value: k::Rhs::Expr(value),
                        ..
                    } => (name, value),
                    _ => return None,
                };
                if *value != expected {
                    return None;
                }
                taken += 1;
                Pattern::Name(name.clone())
            };
            elements.push((star == Some(index), pattern));
        }
        Some((Pattern::Tuple(elements), input.clone(), taken))
    }
    fn unpack_source(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Name(name) => {
                self.name(name);
                self.scope.bind(name, None);
            }
            Pattern::Tuple(elements) => {
                self.out.write("(");
                for (index, (star, pattern)) in elements.iter().enumerate() {
                    if index > 0 {
                        self.out.write(", ");
                    }
                    if *star {
                        self.out.write("*");
                    }
                    self.unpack_source(pattern);
                }
                if elements.len() == 1 {
                    self.out.write(",");
                }
                self.out.write(")");
            }
        }
    }
    pub(super) fn unpack(&mut self, statements: &[k::Stmt]) -> usize {
        let Some((pattern, input, taken)) = self.unpack_pattern(statements) else {
            return 0;
        };
        self.padding();
        self.unpack_source(&pattern);
        self.out.write(" = ");
        self.inline_expr(&input);
        self.out.write("\n");
        for _ in 0..taken {
            self.implicit();
        }
        taken
    }
    pub(super) fn unpacked_loop(&mut self, binding: &Name, body: &[k::Stmt]) -> Option<usize> {
        let (pattern, input, taken) = self.unpack_pattern(body)?;
        if input != k::Expr::Variable(binding.clone()) {
            return None;
        }
        self.unpack_source(&pattern);
        Some(taken)
    }
}
