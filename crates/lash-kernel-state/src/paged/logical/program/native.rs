//! Native construction follows its bound event schedule before the atomic
//! output transfer. Entity bytes come from canonical bodies, never an envelope.
use super::*;
impl Ledger {
    /// Prepare one construction boundary from canonical bodies in source
    /// allocation order. Even a retired interior allocation consumes its
    /// ordinal. Only ordinary reference counting may retire its membership;
    /// unrooted cycles remain until canonical normalization.
    ///
    /// This prepares storage ownership, not source execution authority. The
    /// caller binds its entry dependencies and authenticated source peak before
    /// shared resource acceptance. No event journal is evaluated here.
    pub fn prepare_construction_boundary<'a>(
        &self,
        mut registration: Registration<'a>,
        heap: &PagedHeap,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
        entities: &[VirtualEntity],
    ) -> Result<(BoundaryTransaction<'a>, Vec<u64>), HeapFault> {
        let graph = outputs.iter().find_map(|(_, value)| match value {
            RootValue::Prepared(value) => Some(value.graph()),
            _ => None,
        });
        if let Some(graph) = graph
            && (!graph.is_current(registration.arena())
                || !std::ptr::eq(entities, graph.entities())
                || outputs.iter().any(|(_, value)| !matches!(value, RootValue::Prepared(value) if graph.same(value.graph()))))
        { return Err(invalid("construction substituted its detached graph proof")); }
        let access = heap.pages();
        let mut fresh = BTreeSet::new();
        let mut headers = Vec::new();
        let mut bodies = Vec::new();
        let mut members = Vec::new();
        let mut pending = Vec::new();
        let mut handles = Vec::new();
        for (index, entity) in entities.iter().enumerate() {
            if !fresh.insert(entity.entity)
                || (graph.is_none()
                    && registration.reserve_entity().map_err(|error| match error {
                        lash_kernel_doc::NativeError::Host(error) => error,
                        error => invalid(&error.to_string()),
                    })? != entity.entity)
                || self.entity_holding(entity.entity, access)?.is_some()
            {
                return Err(invalid(
                    "construction identities are not fresh source ordinals",
                ));
            }
            let body = if let Some(graph) = graph {
                graph.body(index).clone()
            } else {
                entity.body.holding(heap, registration.arena())?
            };
            members.push(
                body.bytes()
                    .checked_sub(Cost::OBJECT)
                    .ok_or_else(|| invalid("construction body has no header"))?,
            );
            body.each_edge(access, &mut |edge| {
                if let Identity::Object(entity) = edge.target {
                    pending.push(entity);
                }
                Ok(())
            })?;
            if graph.is_none() {
                match &entity.body {
                    VirtualBody::NativeList(values) | VirtualBody::NativeSet(values) => {
                        handles.extend(values.iter().filter_map(Value::handle));
                    }
                    VirtualBody::NativeMap(entries) | VirtualBody::NativeRecord(entries) => {
                        handles.extend(
                            entries
                                .iter()
                                .flat_map(|(key, value)| [key.handle(), value.handle()])
                                .flatten(),
                        );
                    }
                    VirtualBody::NativeVariable(value) => handles.extend(value.handle()),
                    _ => {}
                }
            }
            headers.push(Change::Allocate {
                entity: entity.entity,
                body: Holding::scalar(Cost::OBJECT),
            });
            bodies.push(Change::Mutate {
                entity: entity.entity,
                body,
            });
        }
        let roots =
            self.ownership_changes(registration.arena(), access, inputs, outputs, &mut pending)?;
        pending.retain(|entity| !fresh.contains(entity));
        let mut changes =
            self.allocate_membership_from(registration.arena(), access, pending, Some(heap))?;
        changes.extend(headers);
        changes.extend(bodies);
        changes.push(Change::Roots(roots));
        let prepared = self.prepare_event(changes, access)?;
        for (_, value) in outputs {
            match value {
                RootValue::Value(value) => handles.extend(value.handle()),
                RootValue::Capture(handle) => handles.push(*handle),
                _ => {}
            }
        }
        for entity in &prepared.projected {
            if !fresh.contains(entity) {
                handles.push(
                    registration
                        .arena()
                        .logical_handle(*entity)
                        .ok_or_else(|| invalid("construction dependency disappeared"))?,
                );
            }
        }
        if let Some(graph) = graph {
            handles.extend_from_slice(graph.dependencies());
        }
        let dependencies = Dependencies::capture(self, registration.arena(), &handles)
            .ok_or_else(|| invalid("construction facts disappeared"))?;
        if let Some(graph) = graph {
            graph.reserve_for_publication(registration.arena())?;
        }
        Ok((
            BoundaryTransaction::from_program(
                registration,
                dependencies,
                prepared,
                Vec::new(),
                publication::RootProof::new(inputs, outputs),
            ),
            members,
        ))
    }
}
