//! Values derived from an immutable value and kept beside it.

use std::any::{Any, TypeId};
use std::sync::{Arc, Mutex};

/// What readers derived from one immutable value, one entry per type: a
/// decoded event, a stored fragment encoding. It lives exactly as long as
/// the value it sits beside, so it never goes stale; a changed value is a
/// new value with an empty memo.
#[derive(Default)]
pub(crate) struct DerivedMemo(Mutex<Vec<(TypeId, Arc<dyn Any + Send + Sync>)>>);

impl DerivedMemo {
    /// The `T` kept here, or what `compute` returns, kept for the next
    /// reader. Two readers that race may both compute; the first kept wins.
    pub(crate) fn get_or_compute<T: Any + Send + Sync>(
        &self,
        compute: impl FnOnce() -> T,
    ) -> Arc<T> {
        let id = TypeId::of::<T>();
        if let Some(found) = self.find::<T>(id) {
            return found;
        }
        let value = Arc::new(compute());
        let mut entries = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, existing)) = entries.iter().find(|(key, _)| *key == id)
            && let Ok(existing) = Arc::clone(existing).downcast::<T>()
        {
            return existing;
        }
        entries.push((id, Arc::clone(&value) as Arc<dyn Any + Send + Sync>));
        value
    }

    fn find<T: Any + Send + Sync>(&self, id: TypeId) -> Option<Arc<T>> {
        let entries = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_, value) = entries.iter().find(|(key, _)| *key == id)?;
        Arc::clone(value).downcast::<T>().ok()
    }
}
