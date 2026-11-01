//! Only changed source control roles are published. Advancing a block has no
//! value role and never triggers a scan of enclosing controls or cold frames.
use super::{Completion, Control, TryPhase};
use crate::compile::{BlockId, StmtId};
use lash_kernel_doc::{Value, heap::HeapFault};
use lash_kernel_state::kernel_internals::logical::SourceValue;

/// What an active or pending departure holds. Return and raise carry their
/// value; break and continue hold a value-less record.
#[derive(Clone, Copy)]
pub(super) enum Departed {
    Value(Value),
    Record,
}
impl Departed {
    pub(super) fn of(departure: &Completion) -> Option<Self> {
        match departure {
            Completion::Return(value)
            | Completion::Throw(value)
            | Completion::ReturnVia { value, .. } => Some(Self::Value(**value)),
            Completion::Break | Completion::Continue => Some(Self::Record),
            Completion::Normal => None,
        }
    }
    pub(super) fn source(self) -> SourceValue {
        match self {
            Self::Value(value) => SourceValue::Value(value),
            Self::Record => SourceValue::Departure,
        }
    }
}

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
        pending: Option<Departed>,
    },
}
pub(super) fn roles(control: &Control) -> Option<Roles> {
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
                TryPhase::Finally(departure) => Departed::of(departure),
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
/// Reserves only physical block placement. It owns no guest price or root.
pub(super) struct PreparedBlock<'a> {
    controls: &'a mut Controls,
    block: BlockId,
}
impl PreparedBlock<'_> {
    pub(super) fn publish(self) {
        debug_assert!(self.controls.values.len() < self.controls.values.capacity());
        debug_assert!(
            self.controls.marked.len() > self.controls.values.len()
                || self.controls.marked.len() < self.controls.marked.capacity()
        );
        self.controls.push(Control::Block {
            block: self.block,
            next: 0,
        });
    }
}
impl Controls {
    pub(super) fn prepare_block(&mut self, block: BlockId) -> Result<PreparedBlock<'_>, HeapFault> {
        self.values
            .try_reserve(1)
            .map_err(|_| HeapFault::Exhausted {
                resource: lash_kernel_doc::heap::HostResource::StagingBytes,
            })?;
        if self.marked.len() <= self.values.len() {
            self.marked
                .try_reserve(1)
                .map_err(|_| HeapFault::Exhausted {
                    resource: lash_kernel_doc::heap::HostResource::StagingBytes,
                })?;
        }
        Ok(PreparedBlock {
            controls: self,
            block,
        })
    }
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
