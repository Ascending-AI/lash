//! Typed source storage descriptors and ordered graph/resource phases.
use super::*;
use crate::compile::room::{Measure, Owner};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HeapKind {
    List,
    Tuple,
    Map,
    Set,
    Record(Box<[Arc<str>]>),
    Cell,
    Closure { code: CodeId, captures: Box<[Slot]> },
    Text(Arc<str>),
    Bytes(Arc<[u8]>),
    BigInt(Arc<doc::Integer>),
    Function(FunctionId),
    Field { name: Arc<str>, removing: bool },
    Index { removing: bool },
    Variable { slot: Slot },
}

pub(super) fn source_kind(
    exe: &Executable,
    parent: CodeId,
    source: &Source,
    at: &Point,
) -> Result<Option<HeapKind>, Refused> {
    let node = resources::node(source, &at.site)?;
    // A let expression binds its value at its Test, a catch at its Catch.
    if at.phase != Phase::Commit
        && !(at.phase == Phase::Test && matches!(node, doc::Node::Expr(doc::Expr::Let { .. })))
        && !(at.phase == Phase::Catch && matches!(node, doc::Node::Stmt(doc::Stmt::Try(_))))
    {
        return Ok(None);
    }
    let kind = match node {
        doc::Node::Stmt(doc::Stmt::Try(doc::TryStmt {
            catch: Some(catch), ..
        })) if at.phase == Phase::Catch
            && source
                .shared
                .get(&(at.site.child(1), catch.binding.clone()))
                == Some(&true) =>
        {
            HeapKind::Cell
        }
        doc::Node::Stmt(doc::Stmt::Try(_)) if at.phase == Phase::Catch => return Ok(None),
        doc::Node::Expr(doc::Expr::Let { name, .. })
            if at.phase == Phase::Test
                && source.shared.get(&(at.site.clone(), name.clone())) == Some(&true) =>
        {
            HeapKind::Cell
        }
        doc::Node::Expr(doc::Expr::Let { .. }) => return Ok(None),
        doc::Node::Expr(doc::Expr::Literal(doc::Literal::Text(text)))
            if doc::arena::ShortText::new(text).is_none() =>
        {
            HeapKind::Text(Arc::from(text.as_str()))
        }
        doc::Node::Expr(doc::Expr::Literal(doc::Literal::Bytes(bytes))) => {
            HeapKind::Bytes(Arc::from(bytes.as_slice()))
        }
        doc::Node::Expr(doc::Expr::Literal(doc::Literal::Int(integer)))
            if integer.word().is_none() =>
        {
            HeapKind::BigInt(Arc::new(integer.clone()))
        }
        doc::Node::Expr(doc::Expr::Literal(doc::Literal::Function(function))) => {
            HeapKind::Function(*function)
        }
        doc::Node::Expr(doc::Expr::Closure(closure)) => {
            let slots = source.owner_slots()?;
            let mut expected = std::collections::BTreeMap::new();
            for name in source.closure_free(&closure.function)? {
                if let Some(binding) =
                    crate::compile::room::root_owners::visible(&slots.scopes, &at.site, &name)
                {
                    let slot = slots
                        .root(binding, doc::TaskId::MAIN, 0)
                        .and_then(resources::root_role)
                        .and_then(Role::slot)
                        .ok_or(Refused::Source)?;
                    expected.insert(name, slot);
                }
            }
            let outer = exe.code(parent);
            let mut matches = exe.closures_at(&at.site).filter_map(|id| {
                let inner = exe.code(id);
                let origin = inner.created_in.as_ref()?;
                if inner.site.unit != lash_kernel_value::sites::Unit::Library(closure.function)
                    || inner.created.as_ref() != Some(&at.site)
                    || !inner.site.path.is_empty()
                    || inner.source_room.site != inner.site
                    || inner.source_room.owner
                        != doc::constant_pool::PoolOwner::Function(closure.function)
                    || !std::ptr::eq(origin.as_ref(), source)
                    || !origin.same_bindings(source)
                    || !inner.free.is_empty()
                    || inner.captures.len() != expected.len()
                    || inner.capture_names != expected.keys().cloned().collect::<Vec<_>>()
                {
                    return None;
                }
                let mut captures = Vec::new();
                let mut remaining = expected.clone();
                let mut last = None;
                for capture in &inner.captures {
                    if last.is_some_and(|last| last >= capture.inner) {
                        return None;
                    }
                    last = Some(capture.inner);
                    let binding = inner.slots.get(capture.inner as usize)?;
                    if binding.declared != inner.site
                        || !binding.shared
                        || inner.slots_by_at.get(capture.inner_at as usize) != Some(&capture.inner)
                        || outer.slots_by_at.get(capture.outer_at as usize) != Some(&capture.outer)
                    {
                        return None;
                    }
                    let inner_slot = Slot::Capture(
                        lash_kernel_state::kernel_internals::logical::SourceRole::Binding {
                            declared: inner.site.clone(),
                            name: binding.name.clone(),
                        }
                        .id(),
                    );
                    if inner
                        .source_room
                        .binding_root(capture.inner, doc::TaskId::MAIN, 0)
                        .and_then(resources::root_role)
                        .and_then(Role::slot)
                        != Some(inner_slot)
                    {
                        return None;
                    }
                    let actual = outer
                        .source_room
                        .binding_root(capture.outer, doc::TaskId::MAIN, 0)
                        .and_then(resources::root_role)
                        .and_then(Role::slot)?;
                    if remaining.remove(&binding.name) != Some(actual) {
                        return None;
                    }
                    captures.push(actual);
                }
                Some((id, captures))
            });
            let (code, captures) = matches.next().ok_or(Refused::Source)?;
            if matches.next().is_some() {
                return Err(Refused::Source);
            }
            HeapKind::Closure {
                code,
                captures: captures.into(),
            }
        }
        doc::Node::Expr(doc::Expr::List(_)) => HeapKind::List,
        doc::Node::Expr(doc::Expr::Tuple(_)) => HeapKind::Tuple,
        doc::Node::Expr(doc::Expr::Map(_)) => HeapKind::Map,
        doc::Node::Expr(doc::Expr::Set(_)) => HeapKind::Set,
        doc::Node::Expr(doc::Expr::Record(entries)) => HeapKind::Record(
            entries
                .iter()
                .map(|entry| Arc::from(entry.field.as_str()))
                .collect(),
        ),
        doc::Node::Stmt(doc::Stmt::Let { name, .. })
            if source.shared.get(&(at.site.clone(), name.clone())) == Some(&true)
                && !source.persistent.contains_key(&at.site) =>
        {
            HeapKind::Cell
        }
        doc::Node::Stmt(doc::Stmt::Assign {
            place: doc::Place::Variable(name),
            ..
        }) if source.assigns_cell(&at.site) => HeapKind::Variable {
            slot: resources::role(
                source,
                &Owner::Visible {
                    at: at.site.clone(),
                    name: name.clone(),
                },
            )
            .and_then(Role::slot)
            .ok_or(Refused::Facts)?,
        },
        doc::Node::Stmt(doc::Stmt::Assign {
            place: doc::Place::Member(member),
            ..
        }) => member_kind(member, false),
        doc::Node::Stmt(doc::Stmt::Remove { member }) => member_kind(member, true),
        _ => return Ok(None),
    };
    Ok(Some(kind))
}
fn member_kind(member: &doc::Member, removing: bool) -> HeapKind {
    match member {
        doc::Member::Field { field, .. } => HeapKind::Field {
            name: Arc::from(field.as_str()),
            removing,
        },
        doc::Member::Index { .. } => HeapKind::Index { removing },
    }
}

impl HeapKernel {
    pub(crate) fn kind(&self) -> &HeapKind {
        &self.kind
    }
    /// Actual occurrences moved into immutable tuple/cell membership at the
    /// same accepted phase as allocation. D derives their prices and edges.
    pub(crate) fn source_moves(&self) -> &[Reg<LeaseValue>] {
        &self.moves
    }
    pub(crate) fn atomic_move(&self) -> bool {
        matches!(self.kind, HeapKind::Tuple | HeapKind::Cell)
    }
    pub(super) fn compile(
        source: &Source,
        roles: &[(Role, Reg<LeaseValue>)],
        event: &Event,
        index: usize,
        kind: HeapKind,
    ) -> Result<Self, Refused> {
        let delta = IndexedAmount::compile(source, roles, &event.delta, event.output.as_ref())?;
        let peak = IndexedAmount::compile(source, roles, &event.peak, event.output.as_ref())?;
        if matches!(kind, HeapKind::Tuple | HeapKind::Cell) {
            let moves = event
                .inputs
                .iter()
                .map(|owner| lower::reg(roles, source, owner))
                .collect::<Result<_, _>>()?;
            return Ok(Self {
                source: event.point.clone(),
                kind,
                moves,
                phases: Box::new([HeapPhase {
                    source: event.point.clone(),
                    kind: HeapPhaseKind::AllocateMove,
                    peak,
                    delta,
                    reached: PrefixId(
                        u32::try_from(
                            index
                                .checked_mul(3)
                                .and_then(|index| index.checked_add(2))
                                .ok_or(Refused::Facts)?,
                        )
                        .map_err(|_| Refused::Facts)?,
                    ),
                }]),
            });
        }
        let mut semantic = delta.clone();
        let mut release = IndexedAmount {
            fixed: 0,
            terms: Box::new([]),
        };
        let (releases, rest): (Vec<_>, Vec<_>) =
            semantic
                .terms
                .into_vec()
                .into_iter()
                .partition(|(input, coefficient)| {
                    *coefficient < 0 && matches!(input, ResourceInput::Release(_))
                });
        semantic.terms = rest.into();
        release.terms = releases.into();
        let prefix = u32::try_from(index.checked_mul(3).ok_or(Refused::Facts)?)
            .map_err(|_| Refused::Facts)?;
        let action = match kind {
            HeapKind::Field { .. } | HeapKind::Index { .. } | HeapKind::Variable { .. } => {
                HeapPhaseKind::Member
            }
            _ => HeapPhaseKind::Allocate,
        };
        Ok(Self {
            source: event.point.clone(),
            kind,
            moves: Box::new([]),
            phases: Box::new([
                HeapPhase {
                    source: event.point.clone(),
                    kind: action,
                    peak,
                    delta: semantic,
                    reached: PrefixId(prefix + 1),
                },
                HeapPhase {
                    source: event.point.clone(),
                    kind: HeapPhaseKind::Release,
                    peak: IndexedAmount {
                        fixed: 0,
                        terms: Box::new([]),
                    },
                    delta: release,
                    reached: PrefixId(prefix + 2),
                },
            ]),
        })
    }
    pub(super) fn verify(
        &self,
        exe: &Executable,
        draft: &RegionDraft,
        step: &Step,
        index: usize,
    ) -> Result<(), Refused> {
        if self.source != step.at
            || source_kind(exe, draft.code, &draft.source, &step.at)?.as_ref() != Some(&self.kind)
        {
            return Err(Refused::Source);
        }
        if matches!(self.kind, HeapKind::Tuple | HeapKind::Cell) {
            let [phase] = self.phases.as_ref() else {
                return Err(Refused::Source);
            };
            let moves = step
                .event
                .inputs
                .iter()
                .map(|owner| lower::reg(&draft.roles, &draft.source, owner))
                .collect::<Result<Vec<_>, _>>()?;
            if self.moves.as_ref() != moves.as_slice()
                || phase.source != step.at
                || phase.kind != HeapPhaseKind::AllocateMove
                || phase.reached != step.after
                || step.before.offset() != index * 3
            {
                return Err(Refused::Source);
            }
            if phase.peak != step.peak {
                return Err(Refused::Peak);
            }
            if phase.delta != step.delta {
                return Err(Refused::Retained);
            }
            return Ok(());
        }
        if self.phases.len() != 2 || !self.moves.is_empty() {
            return Err(Refused::Source);
        }
        let expected_action = match self.kind {
            HeapKind::Field { .. } | HeapKind::Index { .. } | HeapKind::Variable { .. } => {
                HeapPhaseKind::Member
            }
            _ => HeapPhaseKind::Allocate,
        };
        let canonical = draft
            .source
            .room
            .events
            .get(&step.at)
            .ok_or(Refused::Source)?;
        let semantic = &self.phases[0];
        let release = &self.phases[1];
        if semantic.source != step.at
            || release.source != step.at
            || semantic.kind != expected_action
            || release.kind != HeapPhaseKind::Release
            || semantic.reached != step.pending
            || release.reached != step.after
            || step.before.offset() != index * 3
            || semantic.peak
                != IndexedAmount::compile(
                    &draft.source,
                    &draft.roles,
                    &canonical.peak,
                    canonical.output.as_ref(),
                )?
            || release.peak.fixed != 0
            || !release.peak.terms.is_empty()
        {
            return Err(Refused::Peak);
        }
        let delta = IndexedAmount::compile(
            &draft.source,
            &draft.roles,
            &canonical.delta,
            canonical.output.as_ref(),
        )?;
        if semantic.delta.fixed != delta.fixed || release.delta.fixed != 0 {
            return Err(Refused::Retained);
        }
        let expected_release: Vec<_> = delta
            .terms
            .iter()
            .filter(|(input, coefficient)| {
                *coefficient < 0 && matches!(input, ResourceInput::Release(_))
            })
            .cloned()
            .collect();
        let expected_semantic: Vec<_> = delta
            .terms
            .iter()
            .filter(|(input, coefficient)| {
                !(*coefficient < 0 && matches!(input, ResourceInput::Release(_)))
            })
            .cloned()
            .collect();
        if release.delta.terms.as_ref() != expected_release.as_slice()
            || semantic.delta.terms.as_ref() != expected_semantic.as_slice()
        {
            return Err(Refused::Retained);
        }
        if matches!(expected_action, HeapPhaseKind::Member)
            && !canonical
                .peak
                .measures()
                .any(|measure| matches!(measure, Measure::MemberGrowth(_)))
        {
            return Err(Refused::Scratch);
        }
        Ok(())
    }
}
