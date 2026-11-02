//! Source member kind and operand roles are independent of the staged edit.
use super::*;

pub(super) enum MemberEvent {
    Variable,
    Field { name: Arc<str>, removing: bool },
    Index { removing: bool },
}
impl MemberEvent {
    pub(super) fn compile(source: &Source, at: &Point) -> Result<Option<Self>, Refused> {
        if at.phase != Phase::Commit {
            return Ok(None);
        }
        let mut node = doc::Node::Block(&source.body);
        for index in &at.site.path[source.site.path.len()..] {
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        let (member, removing) = match node {
            doc::Node::Stmt(doc::Stmt::Assign {
                place: doc::Place::Variable(_),
                ..
            }) if source.assigns_cell(&at.site) => return Ok(Some(Self::Variable)),
            doc::Node::Stmt(doc::Stmt::Assign {
                place: doc::Place::Member(member),
                ..
            }) => (member, false),
            doc::Node::Stmt(doc::Stmt::Remove { member }) => (member, true),
            _ => return Ok(None),
        };
        Ok(Some(match member {
            doc::Member::Field { field, .. } => Self::Field {
                name: Arc::from(field.as_str()),
                removing,
            },
            doc::Member::Index { .. } => Self::Index { removing },
        }))
    }

    pub(super) fn check(&self, edit: &MemberEdit, operands: &[doc::Value]) -> Result<(), Refused> {
        use doc::Value;
        use logical::Member;
        let next = match (self, operands) {
            (Self::Variable, [value]) if matches!(edit.member, Member::Variable) => Some(*value),
            (Self::Field { name, removing }, [target, rest @ ..]) => {
                if !matches!(target, Value::Record(handle) if *handle == edit.handle)
                    || !matches!(&edit.member, Member::Field(actual) if actual == name)
                {
                    return Err(Refused::Facts);
                }
                match (*removing, rest) {
                    (true, []) => None,
                    (false, [value]) => Some(*value),
                    _ => return Err(Refused::Facts),
                }
            }
            (Self::Index { removing }, [target, key, rest @ ..]) => {
                let valid = match (target, &edit.member) {
                    (Value::List(handle), Member::List(index)) => {
                        *handle == edit.handle && crate::compile::position(key) == Some(*index)
                    }
                    (
                        Value::Map(handle) | Value::Set(handle) | Value::Record(handle),
                        Member::Table(actual),
                    ) => *handle == edit.handle && key.same(actual),
                    _ => false,
                };
                if !valid {
                    return Err(Refused::Facts);
                }
                match (*removing, rest, target) {
                    (true, [], _) => None,
                    (false, [Value::Bool(present)], Value::Set(_)) => {
                        present.then_some(Value::Absent)
                    }
                    (false, [_], Value::Set(_)) => return Err(Refused::Facts),
                    (false, [value], _) => Some(*value),
                    _ => return Err(Refused::Facts),
                }
            }
            _ => return Err(Refused::Facts),
        };
        if match (next, edit.next) {
            (None, None) => true,
            (Some(expected), Some(actual)) => expected.same(&actual),
            _ => false,
        } {
            Ok(())
        } else {
            Err(Refused::Facts)
        }
    }
}
