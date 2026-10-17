//! Persistent ordered queues. Keys leave room on both ends across reopen.
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_state::kernel_internals::tree::{Access, Cell, Tree};

pub(super) struct Queue<T: Cell> {
    pub(super) rows: Tree<T>,
    access: Access<'static>,
}
impl<T: Cell> Queue<T> {
    pub(super) fn open(rows: Tree<T>, access: Access<'static>) -> Self {
        Self { rows, access }
    }
    pub(super) fn is_empty(&self) -> bool {
        self.rows.len() == 0
    }
    fn key(&self, front: bool) -> Result<Vec<u8>, HeapFault> {
        let edge = if front {
            0
        } else {
            self.rows.len().saturating_sub(1)
        };
        let next = match self.rows.rank_get(edge, &self.access)? {
            None => Some(u128::MAX / 2),
            Some((key, _)) => {
                let key: [u8; 16] = key
                    .try_into()
                    .map_err(|_| HeapFault::Malformed("queue key has the wrong width".into()))?;
                let key = u128::from_be_bytes(key);
                if front {
                    key.checked_sub(1)
                } else {
                    key.checked_add(1)
                }
            }
        }
        .ok_or_else(|| HeapFault::Malformed("queue position overflow".into()))?;
        Ok(next.to_be_bytes().to_vec())
    }
    pub(super) fn push_back(&mut self, value: T) -> Result<(), HeapFault> {
        self.rows.write(self.key(false)?, value, &self.access)
    }
    pub(super) fn push_front(&mut self, value: T) -> Result<(), HeapFault> {
        self.rows.write(self.key(true)?, value, &self.access)
    }
    pub(super) fn pop_front(&mut self) -> Result<Option<T>, HeapFault> {
        match self.rows.rank_get(0, &self.access)? {
            None => Ok(None),
            Some((key, _)) => self.rows.remove(&key, &self.access),
        }
    }
}
