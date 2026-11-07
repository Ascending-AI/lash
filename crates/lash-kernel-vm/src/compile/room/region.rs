//! Canonical scalar source inputs. Region calls use their verified ISA plans.
use super::*;
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_state::kernel_internals::logical::{self, Slot};
#[path = "calls.rs"]
pub(crate) mod calls;
pub(super) enum ScalarInput {
    Literal(Value),
    Binding(Slot),
}
impl Source {
    pub(super) fn scalar_input(
        &self,
        slots: &OwnerSlots,
        at: &Point,
    ) -> Result<ScalarInput, Refused> {
        if at.site == self.site && at.phase == Phase::Fallthrough {
            return Ok(ScalarInput::Literal(Value::Null));
        }
        if at.phase != Phase::Commit
            || at.site.unit != self.site.unit
            || !at.site.path.starts_with(&self.site.path)
        {
            return Err(Refused::Source);
        }
        let mut node = doc::Node::Block(&self.body);
        for index in &at.site.path[self.site.path.len()..] {
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        match node {
            doc::Node::Expr(doc::Expr::Literal(literal)) => {
                let value = match literal {
                    doc::Literal::Null => Value::Null,
                    doc::Literal::Absent => Value::Absent,
                    doc::Literal::Bool(value) => Value::Bool(*value),
                    doc::Literal::Int(value) => Value::Int(value.word().ok_or(Refused::Facts)?),
                    doc::Literal::Float(value) => Value::Float(*value),
                    doc::Literal::Text(value) => {
                        Value::Text(doc::arena::ShortText::new(value).ok_or(Refused::Facts)?)
                    }
                    _ => return Err(Refused::Source),
                };
                Ok(ScalarInput::Literal(value))
            }
            doc::Node::Expr(doc::Expr::Variable(name)) => {
                let binding =
                    root_owners::visible(&slots.scopes, &at.site, name).ok_or(Refused::Source)?;
                let Owner::Binding { declared, name } = binding else {
                    return Err(Refused::Source);
                };
                if self.shared.get(&(declared.clone(), name.clone())) == Some(&true) {
                    return Err(Refused::Source);
                }
                match slots.root(binding, TaskId::MAIN, 0) {
                    Some(logical::Owner::Value {
                        slot: slot @ Slot::Binding(_),
                        ..
                    }) => Ok(ScalarInput::Binding(slot)),
                    _ => Err(Refused::Source),
                }
            }
            _ => Err(Refused::Source),
        }
    }
}
