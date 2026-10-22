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
        match self.get_or_try_compute(|| Ok::<_, std::convert::Infallible>(Arc::new(compute()))) {
            Ok(value) => value,
            Err(never) => match never {},
        }
    }

    /// Compute outside the lock, then reclassify and publish under the same
    /// lock as the membership check. `compute` preadmits its value and the
    /// amortized registry slots before making either allocation.
    pub(crate) fn get_or_try_compute<T: Any + Send + Sync, E>(
        &self,
        compute: impl FnOnce() -> Result<Arc<T>, E>,
    ) -> Result<Arc<T>, E> {
        let id = TypeId::of::<T>();
        if let Some(found) = self.find::<T>(id) {
            return Ok(found);
        }
        let value = compute()?;
        let mut entries = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, existing)) = entries.iter().find(|(key, _)| *key == id)
            && let Ok(existing) = Arc::clone(existing).downcast::<T>()
        {
            return Ok(existing);
        }
        entries.push((id, Arc::clone(&value) as Arc<dyn Any + Send + Sync>));
        Ok(value)
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
