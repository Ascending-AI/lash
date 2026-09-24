use super::*;

#[derive(Clone)]
pub(crate) struct IterState {
    pub(super) cursor: IterCursor,
    pub(super) binding: usize,
    pub(super) restore: LoopRestore,
    /// Whether this iterator's captured values have already been imported into
    /// the heap.
    ///
    /// A cursor's values are written exactly once, when the iterator is created
    /// or restored from a continuation; stepping only advances an index. Once
    /// the values have been heapified they can never regress to inline
    /// compounds, so later instructions skip them instead of rescanning the
    /// whole sequence — which is what made iterating a long list quadratic.
    pub(super) heapified: bool,
}

/// Where a loop's next value comes from.
///
/// `for...of` follows ECMA-262 live (FIG-3625): an array or a
/// `URLSearchParams` is read at the cursor's index on every step, so the loop
/// sees what its body appended, removed or replaced, and a `Map` or `Set`
/// visits entries added during the loop and skips ones deleted before their
/// turn. Anything else iterates a snapshot, which for a string or a fresh
/// conversion is indistinguishable from live.
#[derive(Clone)]
pub(super) enum IterCursor {
    /// A snapshot, or, with `collection`, the keys (a `Map`) or values (a
    /// `Set`) of that collection still to visit. The collection's mutations
    /// keep the pending tail current, as ECMA's live entry list does: an added
    /// entry joins the tail, a deleted one leaves it, `clear` empties it.
    List {
        values: ListValue,
        index: usize,
        collection: Option<Value>,
    },
    /// An array or a `URLSearchParams`, read at `index` on every step.
    Live {
        source: Value,
        index: usize,
    },
    Range {
        next: i64,
        end: i64,
        step: i64,
    },
}

impl IterCursor {
    pub(super) fn snapshot(values: ListValue) -> Self {
        Self::List {
            values,
            index: 0,
            collection: None,
        }
    }

    /// The heap reference a live cursor reads through, which the cursor keeps
    /// alive.
    pub(super) fn source(&self) -> Option<&Value> {
        match self {
            Self::List { collection, .. } => collection.as_ref(),
            Self::Live { source, .. } => Some(source),
            Self::Range { .. } => None,
        }
    }

    pub(super) fn next_value(&mut self, heap: &Heap) -> Result<Option<Value>, RuntimeError> {
        match self {
            Self::List {
                values,
                index,
                collection,
            } => {
                let Some(value) = values.get(*index).cloned() else {
                    return Ok(None);
                };
                *index += 1;
                // A `Map` entry is its key and the value it holds now: `set`
                // on a pending key updates what the loop will see.
                if let Some(Value::Ref(map)) = collection
                    && let HeapObject::Map(_) = heap.get(*map)?
                {
                    let current = heap.map_get(*map, &value)?.unwrap_or(Value::Undefined);
                    return Ok(Some(Value::List(vec![value, current].into())));
                }
                Ok(Some(value))
            }
            Self::Live { source, index } => {
                let Value::Ref(id) = source else {
                    return Ok(None);
                };
                let value = match heap.get(*id)? {
                    HeapObject::List(values) => values.get(*index).cloned(),
                    HeapObject::UrlSearchParams(params) => {
                        params.entries.get(*index).map(|(name, value)| {
                            Value::List(
                                vec![
                                    Value::String(name.as_str().into()),
                                    Value::String(value.as_str().into()),
                                ]
                                .into(),
                            )
                        })
                    }
                    _ => None,
                };
                if value.is_some() {
                    *index += 1;
                }
                Ok(value)
            }
            Self::Range { next, end, step } => {
                if (*step > 0 && *next >= *end) || (*step < 0 && *next <= *end) {
                    return Ok(None);
                }
                let value = *next;
                *next = (*next).saturating_add(*step);
                Ok(Some(Value::Number(value as f64)))
            }
        }
    }

    /// Whether the loop will run its body at least once, as it stands now.
    pub(super) fn has_next(&self, heap: &Heap) -> Result<bool, RuntimeError> {
        Ok(match self {
            Self::List { values, index, .. } => *index < values.len(),
            Self::Live {
                source: Value::Ref(id),
                index,
            } => match heap.get(*id)? {
                HeapObject::List(values) => *index < values.len(),
                HeapObject::UrlSearchParams(params) => *index < params.entries.len(),
                _ => false,
            },
            Self::Live { .. } => false,
            Self::Range { next, end, step } => range_has_next(*next, *end, *step),
        })
    }

    /// The pending tail of a live `Map` or `Set` cursor over `collection`.
    fn pending_for(&mut self, collection: HeapId) -> Option<(&mut ListValue, usize)> {
        match self {
            Self::List {
                values,
                index,
                collection: Some(Value::Ref(id)),
            } if *id == collection => Some((values, *index)),
            _ => None,
        }
    }
}

/// Keeps every live `for...of` cursor over `collection` in step with a
/// mutation of it: `Added` joins the pending tail, `Deleted` leaves it,
/// `Cleared` empties it. The same shapes `forEach`'s live queue follows.
pub(super) enum CollectionMutation<'a> {
    Added(Value),
    Deleted(&'a Value),
    Cleared,
}

pub(super) fn update_live_cursors<'a>(
    iterators: impl Iterator<Item = &'a mut IterState>,
    collection: HeapId,
    mutation: &CollectionMutation<'_>,
) {
    for iterator in iterators {
        let Some((values, index)) = iterator.cursor.pending_for(collection) else {
            continue;
        };
        let values = values.make_mut();
        match mutation {
            CollectionMutation::Added(value) => {
                values.push(value.clone());
                // A pushed key may be an inline compound the heap has not
                // imported yet.
                iterator.heapified = false;
            }
            CollectionMutation::Deleted(value) => {
                let mut position = index;
                while position < values.len() {
                    if same_value_zero(&values[position], value) {
                        values.remove(position);
                    } else {
                        position += 1;
                    }
                }
            }
            CollectionMutation::Cleared => values.truncate(index),
        }
    }
}

#[derive(Clone)]
pub(super) struct LoopRestore {
    pub(super) previous: Option<Value>,
}

pub(super) fn range_has_next(start: i64, end: i64, step: i64) -> bool {
    (step > 0 && start < end) || (step < 0 && start > end)
}

/// Re-binds restored projections held by live iterators (FIG-2865).
pub(super) fn refresh_iterators(
    iterators: &mut [IterState],
    bindings: &crate::runtime::ProjectedBindings,
) {
    use crate::runtime::projected_refresh;
    for iterator in iterators {
        if let IterCursor::List { values, .. } = &mut iterator.cursor {
            projected_refresh::refresh_values(values.make_mut(), bindings);
        }
        if let Some(value) = iterator.restore.previous.as_mut() {
            projected_refresh::refresh_value(value, bindings);
        }
    }
}

impl<H: ExecutionHost> Vm<'_, H> {
    /// The cursor a loop walks `iterable` with (FIG-3625). An array, a
    /// `URLSearchParams`, a `Map` or a `Set` is iterated live, as ECMA-262's
    /// iterators are; anything else is first converted exactly as
    /// `Array.from` converts it, which also raises the same error for a value
    /// that is not iterable.
    pub(super) async fn iteration_cursor(
        &mut self,
        iterable: Value,
    ) -> Result<IterCursor, RuntimeError> {
        if let Value::Ref(id) = iterable {
            let pending = match self.heap.get(id)? {
                HeapObject::List(_) | HeapObject::UrlSearchParams(_) => {
                    return Ok(IterCursor::Live {
                        source: iterable,
                        index: 0,
                    });
                }
                HeapObject::Map(map) => {
                    Some(map.entries.iter().map(|(key, _)| key.clone()).collect())
                }
                HeapObject::Set(set) => Some(set.values.clone()),
                _ => None,
            };
            if let Some(pending) = pending {
                return Ok(IterCursor::List {
                    values: ListValue::from(pending),
                    index: 0,
                    collection: Some(iterable),
                });
            }
        }
        let values = match iterable {
            Value::List(values) | Value::Tuple(values) => values,
            // No conversion makes these iterable; the loop's own refusal
            // names what it expects.
            Value::Null | Value::Undefined | Value::Bool(_) | Value::Number(_) => {
                return Err(RuntimeError::NonListIteration);
            }
            other => {
                self.stack
                    .push(Value::String("Lash.ArrayFromIterable".into()));
                self.stack.push(other);
                self.execute_javascript_stdlib(2)?;
                let converted = self.pop_stack()?;
                self.iterable_values_for_dialect(converted).await?
            }
        };
        Ok(IterCursor::snapshot(values))
    }

    /// Every loop cursor of this run, the active frame's and the suspended
    /// callers'.
    pub(super) fn all_iterators(&mut self) -> impl Iterator<Item = &mut IterState> {
        self.iter_stack.iter_mut().chain(
            self.frames
                .iter_mut()
                .flat_map(|frame| frame.iter_stack.iter_mut()),
        )
    }
}
