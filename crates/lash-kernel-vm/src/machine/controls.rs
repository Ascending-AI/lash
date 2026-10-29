//! Only changed source control roles are published. Advancing a block has no
//! value role and never triggers a scan of enclosing controls or cold frames.
use super::{Completion, Control, TryPhase};
use crate::compile::StmtId;
use lash_kernel_doc::{Value, heap::HeapFault};

#[derive(Clone, Copy)]
pub(super) enum Roles {
    For {
        stmt: StmtId,
        cursor: Option<Value>,
        header: Option<Value>,
        initial: bool,
    },
    Try {
        stmt: StmtId,
        pending: Option<Value>,
    },
}
fn roles(control: &Control) -> Option<Roles> {
    match control {
        Control::For {
            stmt,
            cursor,
            header,
            started,
            ..
        } => Some(Roles::For {
            stmt: *stmt,
            cursor: cursor.walked(),
            header: header.held(),
            initial: *started == 0 && matches!(header, super::loops::Header::Body),
        }),
        Control::Try { stmt, phase } => Some(Roles::Try {
            stmt: *stmt,
            pending: match phase {
                TryPhase::Finally(
                    Completion::Return(value)
                    | Completion::Throw(value)
                    | Completion::ReturnVia { value, .. },
                ) => Some(**value),
                _ => None,
            },
        }),
        _ => None,
    }
}
#[derive(Default)]
pub(super) struct Controls {
    values: Vec<Control>,
    dirty: Vec<usize>,
    marked: Vec<bool>,
    removed: Vec<Roles>,
}
impl From<Vec<Control>> for Controls {
    fn from(values: Vec<Control>) -> Self {
        Self {
            marked: vec![false; values.len()],
            values,
            ..Self::default()
        }
    }
}
impl<'a> IntoIterator for &'a Controls {
    type Item = &'a Control;
    type IntoIter = std::slice::Iter<'a, Control>;
    fn into_iter(self) -> Self::IntoIter {
        self.values.iter()
    }
}
impl Controls {
    fn changed(&mut self, at: usize) {
        if roles(&self.values[at]).is_some() && !self.marked[at] {
            self.marked[at] = true;
            self.dirty.push(at);
        }
    }
    pub(super) fn activate(&mut self) {
        for at in 0..self.values.len() {
            self.changed(at);
        }
    }
    pub(super) fn push(&mut self, control: Control) {
        let at = self.values.len();
        self.values.push(control);
        if self.marked.len() <= at {
            self.marked.push(false);
        }
        self.changed(at);
    }
    pub(super) fn pop(&mut self) -> Option<Control> {
        let control = self.values.pop()?;
        if let Some(role) = roles(&control) {
            self.removed.push(role);
        }
        Some(control)
    }
    pub(super) fn last_mut(&mut self) -> Option<&mut Control> {
        let at = self.values.len().checked_sub(1)?;
        self.changed(at);
        self.values.last_mut()
    }
    pub(super) fn last(&self) -> Option<&Control> {
        self.values.last()
    }
    pub(super) fn len(&self) -> usize {
        self.values.len()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    pub(super) fn iter(&self) -> std::slice::Iter<'_, Control> {
        self.values.iter()
    }
    pub(super) fn as_slice(&self) -> &[Control] {
        &self.values
    }
    pub(super) fn has_changes(&self) -> bool {
        !self.dirty.is_empty() || !self.removed.is_empty()
    }
    pub(super) fn clear(&mut self) {
        while self.pop().is_some() {}
    }
    pub(super) fn reset(&mut self) {
        self.values.clear();
        self.dirty.clear();
        self.removed.clear();
        self.marked.fill(false);
    }
    pub(super) fn replace(&mut self, values: Vec<Control>) {
        self.clear();
        for value in values {
            self.push(value);
        }
    }
    pub(super) fn flush(
        &mut self,
        mut visit: impl FnMut(Roles, bool) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        for removed in &self.removed {
            visit(*removed, false)?;
        }
        for &at in &self.dirty {
            if let Some(role) = self.values.get(at).and_then(roles) {
                visit(role, true)?;
            }
        }
        for &at in &self.dirty {
            self.marked[at] = false;
        }
        self.removed.clear();
        self.dirty.clear();
        Ok(())
    }
}
