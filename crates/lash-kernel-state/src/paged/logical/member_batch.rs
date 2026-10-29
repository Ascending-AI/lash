//! An indivisible source event checks every intermediate member and incoming
//! count in mutable working rows, then publishes only the final row versions.
use super::*;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::iter::Peekable;

type Map<K, V> = HashMap<K, V, foldhash::fast::RandomState>;

struct Count {
    before: u64,
    current: u64,
}
struct Row {
    value: EntityHolding,
    bytes_before: u64,
    incoming_before: u64,
    tasks: u64,
    edges: Map<Identity, Count>,
}
impl Row {
    fn new(value: EntityHolding) -> Self {
        Self {
            bytes_before: value.body.bytes,
            incoming_before: value.incoming,
            tasks: value.body.links.task_edges(),
            value,
            edges: Map::default(),
        }
    }
    fn edge(&mut self, target: Identity, access: &Access<'_>) -> Result<&mut Count, HeapFault> {
        Ok(match self.edges.entry(target) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let before = self
                    .value
                    .body
                    .links
                    .get(&identity_key(target), access)?
                    .map_or(0, |edge| edge.count);
                entry.insert(Count {
                    before,
                    current: before,
                })
            }
        })
    }
    fn members(
        &mut self,
        remove: &Holding,
        add: &Holding,
        removed: &[Edge],
        added: &[Edge],
        access: &Access<'_>,
    ) -> Result<(), HeapFault> {
        self.value.body.bytes = self
            .value
            .body
            .bytes
            .checked_sub(remove.bytes)
            .and_then(|bytes| bytes.checked_add(add.bytes))
            .filter(|bytes| *bytes >= Cost::OBJECT)
            .ok_or_else(|| invalid("member cost overflow or missing header"))?;
        for edge in removed {
            let count = self.edge(edge.target, access)?;
            if count.current == 0 {
                return Err(invalid("removed member edge is absent"));
            }
            count.current = count
                .current
                .checked_sub(edge.count)
                .ok_or_else(|| invalid("member edge underflow"))?;
            if matches!(edge.target, Identity::Task(_)) {
                self.tasks = self
                    .tasks
                    .checked_sub(edge.count)
                    .ok_or_else(|| invalid("source index aggregate overflow"))?;
            }
        }
        for edge in added {
            let count = self.edge(edge.target, access)?;
            count.current = count
                .current
                .checked_add(edge.count)
                .ok_or_else(|| invalid("edge count overflow"))?;
            if matches!(edge.target, Identity::Task(_)) {
                self.tasks = self
                    .tasks
                    .checked_add(edge.count)
                    .ok_or_else(|| invalid("source index aggregate overflow"))?;
            }
        }
        Ok(())
    }
    fn finish(mut self, access: &Access<'_>) -> Result<Option<EntityHolding>, HeapFault> {
        let changed_edges: Vec<_> = self
            .edges
            .into_iter()
            .filter(|(_, count)| count.before != count.current)
            .collect();
        if self.value.body.bytes == self.bytes_before
            && self.value.incoming == self.incoming_before
            && changed_edges.is_empty()
        {
            return Ok(None);
        }
        // Remove first so a transfer cannot overflow a temporary task summary.
        for (target, count) in &changed_edges {
            if count.before != 0 {
                self.value
                    .body
                    .links
                    .remove(&identity_key(*target), access)?;
            }
        }
        for (target, count) in changed_edges {
            if count.current != 0 {
                self.value.body.links.write(
                    identity_key(target),
                    Edge {
                        target,
                        count: count.current,
                    },
                    access,
                )?;
            }
        }
        Ok(Some(self.value))
    }
}

fn row<'a>(
    rows: &'a mut Map<EntityId, Row>,
    state: &State,
    entity: EntityId,
    access: &Access<'_>,
) -> Result<&'a mut Row, HeapFault> {
    Ok(match rows.entry(entity) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            let value = state
                .entities
                .get(&entity_key(entity), access)?
                .ok_or_else(|| invalid("holding reaches absent entity"))?;
            entry.insert(Row::new(value))
        }
    })
}

impl Edit<'_, '_> {
    pub(super) fn member_batch<I: Iterator<Item = Change>>(
        &mut self,
        entity: EntityId,
        remove: Holding,
        add: Holding,
        changes: &mut Peekable<I>,
    ) -> Result<(), HeapFault> {
        let mut rows = Map::default();
        let mut cost = self.state.entities.cost();
        let mut tasks = None;
        let mut removed = Vec::new();
        let mut added = Vec::new();
        let mut current = (entity, remove, add);
        loop {
            let (entity, remove, add) = current;
            removed.clear();
            added.clear();
            remove.each_edge(self.access, &mut |edge| {
                removed.push(edge.clone());
                Ok(())
            })?;
            add.each_edge(self.access, &mut |edge| {
                added.push(edge.clone());
                Ok(())
            })?;
            let body = row(&mut rows, &self.state, entity, self.access)?;
            let before_bytes = body.value.body.bytes;
            let before_tasks = body.tasks;
            body.members(&remove, &add, &removed, &added, self.access)?;
            cost = cost
                .checked_sub(before_bytes)
                .and_then(|cost| cost.checked_add(body.value.body.bytes))
                .ok_or_else(|| invalid("source index aggregate overflow"))?;
            if before_tasks != body.tasks {
                let before = match tasks {
                    Some(tasks) => tasks,
                    None => self.state.entities.read_task_edges(self.access)?,
                };
                tasks = Some(
                    before
                        .checked_sub(before_tasks)
                        .and_then(|tasks| tasks.checked_add(body.tasks))
                        .ok_or_else(|| invalid("source index aggregate overflow"))?,
                );
            }
            // Retain before release, including a member that aliases itself.
            for edge in &added {
                if let Identity::Object(entity) = edge.target {
                    let record = row(&mut rows, &self.state, entity, self.access)?;
                    record.value.incoming = record
                        .value
                        .incoming
                        .checked_add(edge.count)
                        .ok_or_else(|| invalid("incoming ownership overflow"))?;
                }
            }
            for edge in &removed {
                if let Identity::Object(entity) = edge.target {
                    let record = row(&mut rows, &self.state, entity, self.access)?;
                    record.value.incoming = record
                        .value
                        .incoming
                        .checked_sub(edge.count)
                        .ok_or_else(|| invalid("incoming ownership underflow"))?;
                    if record.value.incoming == 0 {
                        self.zero.push_back(entity);
                    }
                }
            }
            if !matches!(changes.peek(), Some(Change::Members { .. })) {
                break;
            }
            let Some(Change::Members {
                entity,
                remove,
                add,
            }) = changes.next()
            else {
                unreachable!()
            };
            current = (entity, remove, *add);
        }
        let mut final_rows = Vec::new();
        for (entity, cached) in rows {
            if let Some(value) = cached.finish(self.access)? {
                final_rows.push((entity_key(entity).to_vec(), value));
            }
        }
        self.state.entities.write_batch(final_rows, self.access)?;
        if self.state.entities.cost() != cost
            || tasks.is_some_and(|tasks| self.state.entities.task_edges() != tasks)
        {
            return Err(invalid("source member batch changed its checked totals"));
        }
        Ok(())
    }
}
