//! Binding patterns reconstruct the guards and iterator steps on readback.
use lash_kernel_doc as k;

use super::lift::{Lift, Lifter};
use super::source::{Expr, Pattern};

/// A pattern, the value it destructures, the statements it takes, and the
/// operand spelling its reads name.
type Destructured = (Pattern, k::Expr, usize, Option<&'static str>);

/// The operand spelling `target`, a read's lifted target, names.
fn operand_spelling(target: &Expr) -> Option<&'static str> {
    let Expr::Call(callee, _) = target else {
        return None;
    };
    let Expr::Ident(name) = &**callee else {
        return None;
    };
    [
        "value.operand",
        "list.operand",
        "record.operand",
        "value.text_operand",
    ]
    .into_iter()
    .find(|spelling| spelling == name)
}

impl Lifter<'_> {
    pub(super) fn binding_pattern(
        &mut self,
        block: &[k::Stmt],
    ) -> Lift<Option<(Pattern, Expr, usize)>> {
        let Some((pattern, value, taken, spelling)) = self.destructured(block)? else {
            return Ok(None);
        };
        let value = match spelling {
            Some(spelling) => self.operand(&value, spelling)?,
            None => self.expr(&value)?,
        };
        Ok(Some((pattern, value, taken)))
    }

    /// An object or array pattern at the start of `block`, the value it
    /// destructures, how many statements it takes, and the operand
    /// spelling of the kind its reads take (see [`Lifter::operand`]).
    fn destructured(&mut self, block: &[k::Stmt]) -> Lift<Option<Destructured>> {
        if let Some(found) = self.object_binding_pattern(block)? {
            return Ok(Some(found));
        }
        Ok(self
            .array_binding_pattern(block)?
            .map(|(pattern, value, taken)| (pattern, value, taken, None)))
    }

    fn object_binding_pattern(&mut self, block: &[k::Stmt]) -> Lift<Option<Destructured>> {
        let Some(k::Stmt::Do {
            value: k::Expr::Call { function, args },
        }) = block.first()
        else {
            return Ok(None);
        };
        if !self.is(function, "ts.require_object_coercible") {
            return Ok(None);
        }
        let [value] = args.as_slice() else {
            return Ok(None);
        };
        let mut fields = Vec::new();
        let mut index = 1;
        let mut rest = None;
        let mut spelling = None;
        while index < block.len() {
            if let k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::Member(member)),
            } = &block[index]
                && let k::Member::Field { target, field } = &**member
                && target == value
            {
                let (pattern, taken) = match self.default_pattern(&block[index + 1..], name)? {
                    Some((pattern, taken)) => (pattern, taken + 1),
                    None => (Pattern::Name(self.binding(name)?), 1),
                };
                fields.push((field.clone(), pattern));
                index += taken;
                continue;
            }
            // A field of a value the source built as no record (a list, a
            // text) is the read helper's, `ts.read(value, "field")`.
            let text = matches!(value, k::Expr::Variable(name) if self.text_checked.contains(name));
            let read_spelling = if text {
                "value.text_operand"
            } else {
                "list.operand"
            };
            if let k::Stmt::Let { name, value: read } = &block[index]
                && let Some(args) = self.helper_read(read, "ts.read")
                && let [target, k::Expr::Literal(k::Literal::Text(field))] = args.as_slice()
                && target == value
            {
                let (pattern, taken) = match self.default_pattern(&block[index + 1..], name)? {
                    Some((pattern, taken)) => (pattern, taken + 1),
                    None => (Pattern::Name(self.binding(name)?), 1),
                };
                fields.push((field.clone(), pattern));
                spelling.get_or_insert(read_spelling);
                index += taken;
                continue;
            }
            // A nested array pattern iterates that read itself.
            if let Some(k::Stmt::Let { value: open, .. }) = block.get(index)
                && let Some(iterated) = self.helper_read(open, "ts.array.iterator")
                && let [read @ k::Expr::Act(_)] = iterated.as_slice()
                && let Some(args) = self.helper_read(&k::Rhs::Expr(read.clone()), "ts.read")
                && let [target, k::Expr::Literal(k::Literal::Text(field))] = args.as_slice()
                && target == value
                && let Some((pattern, _, taken)) = self.array_binding_pattern(&block[index..])?
            {
                fields.push((field.clone(), pattern));
                spelling.get_or_insert(read_spelling);
                index += taken;
                continue;
            }
            if let Some((temporary, read, count)) = self.read_ladder(&block[index..])? {
                // The exact style reads a record's field by `record.field`
                // and any other kind's through its operand spelling.
                let (field, operand) = match read {
                    Expr::Field(target, field) => (field, operand_spelling(&target)),
                    Expr::Call(callee, mut args)
                        if matches!(&*callee, Expr::Ident(name) if name == "record.field")
                            && matches!(args.as_slice(), [_, Expr::Text(_)]) =>
                    {
                        let Some(Expr::Text(field)) = args.pop() else {
                            return Ok(None);
                        };
                        (field, Some("record.operand"))
                    }
                    _ => return Ok(None),
                };
                if let Some(operand) = operand {
                    spelling.get_or_insert(operand);
                }
                let Some((pattern, taken)) =
                    self.bound_pattern(&block[index + count..], &temporary)?
                else {
                    return Ok(None);
                };
                fields.push((field, pattern));
                index += count + taken;
                continue;
            }
            if let [
                k::Stmt::Let {
                    name: keys,
                    value: k::Rhs::Expr(k::Expr::List(taken)),
                },
                k::Stmt::Let {
                    name,
                    value: rest_value,
                },
                ..,
            ] = &block[index..]
                && let Some(args) = self.helper_read(rest_value, "ts.object_rest")
                && args == [value.clone(), k::Expr::Variable(keys.clone())]
                && taken.iter().eq(fields
                    .iter()
                    .map(|(field, _)| field)
                    .map(|field| k::Expr::Literal(k::Literal::Text(field.clone())))
                    .collect::<Vec<_>>()
                    .iter())
            {
                rest = Some(Box::new(Pattern::Name(self.binding(name)?)));
                index += 2;
            }
            break;
        }
        if fields.is_empty() && rest.is_none() {
            return Ok(None);
        }
        Ok(Some((
            Pattern::Object(fields, rest),
            value.clone(),
            index,
            spelling,
        )))
    }

    fn array_binding_pattern(
        &mut self,
        block: &[k::Stmt],
    ) -> Lift<Option<(Pattern, k::Expr, usize)>> {
        let [
            k::Stmt::Let {
                name: iterator,
                value: open,
            },
            k::Stmt::Let { name: next, .. },
            k::Stmt::Let {
                name: exhausted,
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Bool(false))),
            },
            ..,
        ] = block
        else {
            return Ok(None);
        };
        let Some(args) = self.helper_read(open, "ts.array.iterator") else {
            return Ok(None);
        };
        let [value] = args.as_slice() else {
            return Ok(None);
        };
        let _ = (iterator, next);
        let mut index = 3;
        let mut elements = Vec::new();
        let mut rest = None;
        while index < block.len() {
            if let [
                k::Stmt::Let {
                    name: item,
                    value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Absent)),
                },
                k::Stmt::If {
                    condition: k::Expr::Variable(test),
                    then_block,
                    ..
                },
                ..,
            ] = &block[index..]
                && test == exhausted
                && then_block.is_empty()
            {
                index += 2;
                if let Some((pattern, taken)) = self.bound_pattern(&block[index..], item)? {
                    elements.push(Some(pattern));
                    index += taken;
                } else {
                    elements.push(None);
                }
                continue;
            }
            if let [
                k::Stmt::Let {
                    name: tail,
                    value: k::Rhs::Expr(k::Expr::List(empty)),
                },
                k::Stmt::While {
                    condition: k::Expr::Literal(k::Literal::Bool(true)),
                    ..
                },
                k::Stmt::Let {
                    name,
                    value: k::Rhs::Expr(k::Expr::Variable(read)),
                },
                ..,
            ] = &block[index..]
                && empty.is_empty()
                && tail == read
            {
                rest = Some(Box::new(Pattern::Name(self.binding(name)?)));
                index += 3;
            }
            break;
        }
        if elements.is_empty() && rest.is_none() {
            return Ok(None);
        }
        Ok(Some((Pattern::Array(elements, rest), value.clone(), index)))
    }
    fn bound_pattern(
        &mut self,
        block: &[k::Stmt],
        input: &lash_kernel_value::Name,
    ) -> Lift<Option<(Pattern, usize)>> {
        let Some(k::Stmt::Let {
            name,
            value: k::Rhs::Expr(k::Expr::Variable(read)),
        }) = block.first()
        else {
            // A nested pattern destructures the read itself.
            return Ok(match self.destructured(block)? {
                Some((pattern, k::Expr::Variable(read), taken, _)) if read == *input => {
                    Some((pattern, taken))
                }
                _ => None,
            });
        };
        if read != input {
            return Ok(None);
        }
        if let Some((pattern, taken)) = self.default_pattern(&block[1..], name)? {
            return Ok(Some((pattern, taken + 1)));
        }
        Ok(Some((Pattern::Name(self.binding(name)?), 1)))
    }

    pub(super) fn default_pattern(
        &mut self,
        block: &[k::Stmt],
        name: &lash_kernel_value::Name,
    ) -> Lift<Option<(Pattern, usize)>> {
        if let [
            k::Stmt::If {
                condition: k::Expr::Call { function, args },
                then_block,
                else_block,
            },
            k::Stmt::Let {
                name: binding,
                value: k::Rhs::Expr(k::Expr::Variable(result)),
            },
            ..,
        ] = block
            && self.is(function, "same")
            && args.as_slice()
                == [
                    k::Expr::Variable(name.clone()),
                    k::Expr::Literal(k::Literal::Absent),
                ]
            && else_block.is_empty()
            && result == name
            && let [
                prelude @ ..,
                k::Stmt::Assign {
                    place: k::Place::Variable(target),
                    value,
                    ..
                },
            ] = then_block.as_slice()
            && target == name
        {
            let pattern = Pattern::Name(self.binding(binding)?);
            let value = self.default_value(prelude, value)?;
            return Ok(Some((Pattern::Default(Box::new(pattern), value), 2)));
        }
        Ok(None)
    }

    /// A default's bindings run only in the absent branch. Keep them scoped
    /// around its value so syntax temporaries are consumed in that branch.
    fn default_value(&mut self, prelude: &[k::Stmt], value: &k::Rhs) -> Lift<Expr> {
        let mut value = match value {
            k::Rhs::Expr(value) => value.clone(),
            k::Rhs::Action(action) => k::Expr::Act(Box::new(super::statements::act_of(action))),
        };
        for statement in prelude.iter().rev() {
            let k::Stmt::Let {
                name,
                value: binding,
            } = statement
            else {
                return super::lift::refuse("a statement in a binding default");
            };
            let binding = match binding {
                k::Rhs::Expr(value) => value.clone(),
                k::Rhs::Action(action) => k::Expr::Act(Box::new(super::statements::act_of(action))),
            };
            value = k::Expr::Let {
                name: name.clone(),
                value: Box::new(binding),
                body: Box::new(value),
            };
        }
        self.expr(&value)
    }
}
