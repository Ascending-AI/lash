//! Constructor meaning is compiled from source, independently of storage recipes.
use super::*;
use crate::compile::room::publication::{ExactRoots, ExternalFacts, Occurrence};
use lash_kernel_state::kernel_internals::logical::{self, PublicationValue as PV};
use lash_kernel_state::{arena::Arena, kernel_internals::PagedHeap};

pub(in crate::compile::room) struct ConstructorFacts<'a> {
    pub(in crate::compile::room) construction: Option<&'a doc::NewObject>,
    pub(in crate::compile::room) literal:
        Option<(&'a doc::Literal, lash_kernel_value::content_ids::LinkId)>,
    pub(in crate::compile::room) closure: Option<(
        &'a doc::arena::ClosureIdentity,
        &'a [doc::arena::ArenaHandle],
        &'a [Name],
    )>,
    pub(in crate::compile::room) cell: Option<doc::Value>,
}
pub(in crate::compile::room) enum ConstructionEvent {
    List,
    Tuple,
    Map,
    Set,
    Record(Box<[String]>),
    Literal(doc::Literal),
    Closure {
        function: FunctionId,
        captures: Box<[(Name, Owner)]>,
    },
    Cell,
}
impl ConstructionEvent {
    pub(in crate::compile::room) fn compile(
        source: &Source,
        at: &Point,
    ) -> Result<Option<Self>, Refused> {
        if at.phase != Phase::Commit && at.phase != Phase::Test {
            return Ok(None);
        }
        if at.phase == Phase::Test
            && !matches!(
                source
                    .room
                    .events
                    .get(at)
                    .and_then(|event| event.output.as_ref()),
                Some(Owner::Binding { .. })
            )
        {
            return Ok(None);
        }
        let mut node = doc::Node::Block(&source.body);
        for index in &at.site.path[source.site.path.len()..] {
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        Ok(Some(match node {
            doc::Node::Expr(doc::Expr::List(_)) => Self::List,
            doc::Node::Expr(doc::Expr::Tuple(_)) => Self::Tuple,
            doc::Node::Expr(doc::Expr::Map(_)) => Self::Map,
            doc::Node::Expr(doc::Expr::Set(_)) => Self::Set,
            doc::Node::Expr(doc::Expr::Record(entries)) => {
                Self::Record(entries.iter().map(|entry| entry.field.clone()).collect())
            }
            doc::Node::Expr(doc::Expr::Literal(literal)) => Self::Literal(literal.clone()),
            doc::Node::Expr(doc::Expr::Closure(closure)) => {
                let Some(seed) = source.seed.as_ref() else {
                    return Ok(None);
                };
                let catalog = &seed.catalog;
                let definition = catalog
                    .get(&closure.function)
                    .ok_or(Refused::Source)?
                    .definition();
                let body = definition.body().ok_or(Refused::Source)?;
                let params = definition
                    .signature
                    .params
                    .iter()
                    .map(|param| param.name.clone())
                    .collect::<Vec<_>>();
                let slots = source.owner_slots()?;
                let captures = doc::snapshot::CodeReads::of(&params, &body.block, catalog.as_ref())
                    .free
                    .into_iter()
                    .map(|name| {
                        let binding =
                            super::super::root_owners::visible(&slots.scopes, &at.site, &name)
                                .cloned()
                                .ok_or(Refused::Source)?;
                        Ok((name, binding))
                    })
                    .collect::<Result<_, Refused>>()?;
                Self::Closure {
                    function: closure.function,
                    captures,
                }
            }
            doc::Node::Stmt(doc::Stmt::Let { name, .. })
                if source.shared.get(&(at.site.clone(), name.clone())) == Some(&true)
                    && source.persistent.get(&at.site) != Some(name) =>
            {
                Self::Cell
            }
            doc::Node::Expr(doc::Expr::Let { name, .. })
                if at.phase == Phase::Test
                    && source.shared.get(&(at.site.clone(), name.clone())) == Some(&true)
                    && source.persistent.get(&at.site) != Some(name) =>
            {
                Self::Cell
            }
            _ => return Ok(None),
        }))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "source constructor, actual graph and entry roles"
    )]
    pub(in crate::compile::room) fn check(
        &self,
        context: (&OwnerSlots, &Occurrence, &Arena, &PagedHeap),
        event: &Event,
        actual: &ExactRoots<'_>,
        external: &ExternalFacts<'_>,
        construction: Option<&ConstructorFacts<'_>>,
        link: lash_kernel_value::content_ids::LinkId,
        check_cell: impl Fn(logical::Owner, doc::arena::ArenaHandle) -> Result<(), Refused>,
    ) -> Result<(), Refused> {
        let (slots, occurrence, arena, heap) = context;
        // Scalar reads already authenticate their native value against source.
        if let Self::Literal(literal) = self
            && construction.is_none()
        {
            return if external.entities.is_empty()
                && matches!(actual.outputs.as_slice(), [(_, PV::Native(value))] if matches_literal(arena, literal, link, value))
            {
                Ok(())
            } else {
                Err(Refused::Facts)
            };
        }
        let construction = construction.ok_or(Refused::Facts)?;
        if external.native.is_some()
            || external.member.is_some()
            || !external.scratch.is_empty()
            || actual.outputs.len() != 1
            || external.operands.len() != event.inputs.len()
        {
            return Err(Refused::Facts);
        }
        let inputs = event
            .inputs
            .iter()
            .map(|owner| {
                slots
                    .root(owner, occurrence.task, occurrence.invocation)
                    .ok_or(Refused::Source)
            })
            .collect::<Result<Vec<_>, _>>()?;
        if inputs != actual.inputs {
            return Err(Refused::Source);
        }
        let claims = usize::from(construction.construction.is_some())
            + usize::from(construction.literal.is_some())
            + usize::from(construction.closure.is_some())
            + usize::from(construction.cell.is_some());
        if claims != 1 {
            return Err(Refused::Facts);
        }
        if let PV::Prepared(proof) = &actual.outputs[0].1 {
            if !proof.graph().is_current(arena)
                || !std::ptr::eq(external.entities, proof.graph().entities())
            {
                return Err(Refused::Facts);
            }
            return check_prepared_constructor(
                self,
                arena,
                event,
                external,
                construction,
                proof,
                link,
                slots,
                occurrence,
                check_cell,
            );
        }
        let body = match external.entities {
            [entity] if matches!(&actual.outputs[0].1, PV::Page(page) if page.object() == Some(entity.entity) || matches!(page, doc::heap::PageValue::Ref(doc::heap::Identity::Object(id)) if *id == entity.entity)) => {
                Some(&entity.body)
            }
            [] => None,
            _ => return Err(Refused::Facts),
        };
        match self {
            Self::List | Self::Tuple | Self::Map | Self::Set | Self::Record(_) => {
                let object = construction.construction.ok_or(Refused::Facts)?;
                let values = external.operands;
                match (self, object) {
                    (Self::List, doc::NewObject::List(actual))
                    | (Self::Tuple, doc::NewObject::Tuple(actual))
                    | (Self::Set, doc::NewObject::Set(actual))
                        if same_values(actual, values) => {}
                    (Self::Map, doc::NewObject::Map(entries))
                        if entries.len() * 2 == values.len()
                            && entries
                                .iter()
                                .flat_map(|(key, value)| [key, value])
                                .zip(values)
                                .all(|(a, b)| a.same(b)) => {}
                    (Self::Record(names), doc::NewObject::Record(entries))
                        if entries.len() == names.len()
                            && entries.len() == values.len()
                            && entries.iter().zip(names.iter()).zip(values).all(
                                |(((key, value), name), expected)| {
                                    arena.semantics().text(key).is_ok_and(|text| text == name)
                                        && value.same(expected)
                                },
                            ) => {}
                    _ => return Err(Refused::Facts),
                }
                check_body(arena, heap, self, object, body, &actual.outputs[0].1)
            }
            Self::Literal(expected) => {
                let (literal, actual_link) = construction.literal.ok_or(Refused::Facts)?;
                if literal != expected
                    || actual_link != link
                    || body.is_some()
                    || !inputs.is_empty()
                {
                    return Err(Refused::Facts);
                }
                // The canonical literal constructor emits its page directly from
                // this authenticated payload and defining Link.
                check_literal(expected, link, &actual.outputs[0].1, heap)
            }
            Self::Closure { function, captures } => {
                let (identity, cells, names) = construction.closure.ok_or(Refused::Facts)?;
                if identity.callable.link != link
                    || identity.callable.function != *function
                    || identity.site.link != link
                    || identity.site.site != event.point.site
                    || identity.capture_names.len() != captures.len()
                    || cells.len() != captures.len()
                    || names.len() != captures.len()
                    || !identity
                        .capture_names
                        .iter()
                        .eq(captures.iter().map(|(name, _)| name))
                {
                    return Err(Refused::Facts);
                }
                let mut entities = Vec::new();
                let mut seen = std::collections::BTreeSet::new();
                for (actual_name, cell) in names.iter().zip(cells) {
                    if !seen.insert(actual_name) {
                        return Err(Refused::Facts);
                    }
                    let (_, owner) = captures
                        .iter()
                        .find(|(name, _)| name == actual_name)
                        .ok_or(Refused::Facts)?;
                    let owner = slots
                        .root(owner, occurrence.task, occurrence.invocation)
                        .ok_or(Refused::Source)?;
                    check_cell(owner, *cell)?;
                    entities.push(
                        arena
                            .facts(*cell)
                            .map_err(|_| Refused::Facts)?
                            .ledger_entity()
                            .ok_or(Refused::Facts)?,
                    );
                }
                if matches!(
                    &actual.outputs[0].1,
                    PV::Page(doc::heap::PageValue::Closure(_))
                ) && matches!(body, Some(logical::VirtualBody::Closure(actual)) if *actual == entities)
                {
                    Ok(())
                } else {
                    Err(Refused::Facts)
                }
            }
            Self::Cell => {
                let value = construction.cell.ok_or(Refused::Facts)?;
                if !matches!(
                    &actual.outputs[0].1,
                    PV::Page(doc::heap::PageValue::Ref(doc::heap::Identity::Object(_)))
                ) || !matches!(external.operands, [rhs] if rhs.same(&value))
                    || !matches!(body, Some(logical::VirtualBody::NativeVariable(actual)) if actual.same(&value))
                {
                    return Err(Refused::Facts);
                }
                Ok(())
            }
        }
    }
}
#[expect(
    clippy::too_many_arguments,
    reason = "source constructor and its detached storage proof"
)]
fn check_prepared_constructor(
    kind: &ConstructionEvent,
    arena: &Arena,
    event: &Event,
    external: &ExternalFacts<'_>,
    construction: &ConstructorFacts<'_>,
    proof: &lash_kernel_state::arena::PreparedValueProof,
    link: lash_kernel_value::content_ids::LinkId,
    slots: &OwnerSlots,
    occurrence: &Occurrence,
    check_cell: impl Fn(logical::Owner, doc::arena::ArenaHandle) -> Result<(), Refused>,
) -> Result<(), Refused> {
    match kind {
        ConstructionEvent::List
        | ConstructionEvent::Tuple
        | ConstructionEvent::Map
        | ConstructionEvent::Set
        | ConstructionEvent::Record(_) => {
            let object = construction.construction.ok_or(Refused::Facts)?;
            let values = external.operands;
            match (kind, object) {
                (ConstructionEvent::List, doc::NewObject::List(actual))
                | (ConstructionEvent::Tuple, doc::NewObject::Tuple(actual))
                | (ConstructionEvent::Set, doc::NewObject::Set(actual))
                    if same_values(actual, values) => {}
                (ConstructionEvent::Map, doc::NewObject::Map(entries))
                    if entries.len() * 2 == values.len()
                        && entries
                            .iter()
                            .flat_map(|(key, value)| [key, value])
                            .zip(values)
                            .all(|(a, b)| a.same(b)) => {}
                (ConstructionEvent::Record(names), doc::NewObject::Record(entries))
                    if entries.len() == names.len()
                        && entries.len() == values.len()
                        && entries.iter().zip(names.iter()).zip(values).all(
                            |(((key, value), name), expected)| {
                                arena.semantics().text(key).is_ok_and(|text| text == name)
                                    && value.same(expected)
                            },
                        ) => {}
                _ => return Err(Refused::Facts),
            }
            if !same_prepared_collection(arena, object, proof.collection().ok_or(Refused::Facts)?)?
            {
                return Err(Refused::Facts);
            }
        }
        ConstructionEvent::Literal(expected) => {
            let (literal, actual_link) = construction.literal.ok_or(Refused::Facts)?;
            if literal != expected
                || actual_link != link
                || !external.entities.is_empty()
                || !event.inputs.is_empty()
                || proof.literal() != Some(expected)
                || proof
                    .callable()
                    .is_some_and(|callable| callable.link != link)
            {
                return Err(Refused::Facts);
            }
        }
        ConstructionEvent::Closure { function, captures } => {
            let (identity, cells, names) = construction.closure.ok_or(Refused::Facts)?;
            let (stored, stored_cells) = proof.closure().ok_or(Refused::Facts)?;
            if identity.callable.link != link
                || identity.callable.function != *function
                || identity.site.link != link
                || identity.site.site != event.point.site
                || identity.capture_names.len() != captures.len()
                || cells.len() != captures.len()
                || names.len() != captures.len()
                || !identity
                    .capture_names
                    .iter()
                    .eq(captures.iter().map(|(name, _)| name))
                || stored.callable != identity.callable
                || stored.site != identity.site
                || stored.capture_names != identity.capture_names
                || stored_cells != cells
                || external.entities.len() != 1
                || proof.entity() != external.entities.first().map(|entity| entity.entity)
            {
                return Err(Refused::Facts);
            }
            let mut entities = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            for (name, cell) in names.iter().zip(cells) {
                if !seen.insert(name) {
                    return Err(Refused::Facts);
                }
                let (_, owner) = captures
                    .iter()
                    .find(|(expected, _)| expected == name)
                    .ok_or(Refused::Facts)?;
                check_cell(
                    slots
                        .root(owner, occurrence.task, occurrence.invocation)
                        .ok_or(Refused::Source)?,
                    *cell,
                )?;
                entities.push(
                    arena
                        .facts(*cell)
                        .map_err(|_| Refused::Facts)?
                        .ledger_entity()
                        .ok_or(Refused::Facts)?,
                );
            }
            if !matches!(&external.entities[0].body, logical::VirtualBody::Closure(actual) if *actual == entities)
            {
                return Err(Refused::Facts);
            }
        }
        ConstructionEvent::Cell => {
            let value = construction.cell.ok_or(Refused::Facts)?;
            if proof.entity() != external.entities.first().map(|entity| entity.entity)
                || !matches!(proof.value(), doc::Value::ObjectRef(_))
                || !matches!(external.operands, [rhs] if rhs.same(&value))
                || !matches!(external.entities, [logical::VirtualEntity { body: logical::VirtualBody::NativeVariable(actual), .. }] if actual.same(&value))
            {
                return Err(Refused::Facts);
            }
        }
    }
    Ok(())
}
fn same_prepared_collection(
    arena: &Arena,
    expected: &doc::NewObject,
    actual: &doc::NewObject,
) -> Result<bool, Refused> {
    Ok(match (expected, actual) {
        (doc::NewObject::List(a), doc::NewObject::List(b))
        | (doc::NewObject::Tuple(a), doc::NewObject::Tuple(b)) => same_values(a, b),
        (doc::NewObject::Set(a), doc::NewObject::Set(b)) => {
            let mut seen = std::collections::HashSet::new();
            let mut normalized = Vec::new();
            for value in a {
                if seen.insert(arena.key(value).map_err(|_| Refused::Facts)?) {
                    normalized.push(*value);
                }
            }
            same_values(&normalized, b)
        }
        (doc::NewObject::Map(a), doc::NewObject::Map(b))
        | (doc::NewObject::Record(a), doc::NewObject::Record(b)) => {
            let mut normalized: Vec<(doc::Value, doc::Value)> = Vec::new();
            let mut indices =
                std::collections::HashMap::<lash_kernel_state::arena::Key, usize>::new();
            for (key, value) in a {
                let key_value = arena.key(key).map_err(|_| Refused::Facts)?;
                if let Some(index) = indices.get(&key_value).copied() {
                    normalized[index].1 = *value;
                } else {
                    indices.insert(key_value, normalized.len());
                    normalized.push((*key, *value));
                }
            }
            normalized.len() == b.len()
                && normalized
                    .iter()
                    .zip(b)
                    .all(|((ak, av), (bk, bv))| ak.same(bk) && av.same(bv))
        }
        _ => false,
    })
}

fn same_values(actual: &[doc::Value], expected: &[doc::Value]) -> bool {
    actual.len() == expected.len() && actual.iter().zip(expected).all(|(a, b)| a.same(b))
}

fn check_body(
    arena: &lash_kernel_state::arena::Arena,
    heap: &PagedHeap,
    kind: &ConstructionEvent,
    object: &doc::NewObject,
    body: Option<&logical::VirtualBody>,
    output: &PV,
) -> Result<(), Refused> {
    use logical::VirtualBody as B;
    let PV::Page(page) = output else {
        return Err(Refused::Facts);
    };
    if !matches!(
        (kind, page),
        (ConstructionEvent::List, doc::heap::PageValue::List(_))
            | (ConstructionEvent::Tuple, doc::heap::PageValue::Tuple(_))
            | (ConstructionEvent::Map, doc::heap::PageValue::Map(_))
            | (ConstructionEvent::Set, doc::heap::PageValue::Set(_))
            | (
                ConstructionEvent::Record(_),
                doc::heap::PageValue::Record(_)
            )
    ) {
        return Err(Refused::Facts);
    }
    let valid = match (kind, object, body) {
        (ConstructionEvent::List, doc::NewObject::List(expected), Some(B::List(actual))) => {
            same_pages(arena, heap, actual, expected)
        }
        (ConstructionEvent::Tuple, doc::NewObject::Tuple(expected), None) => {
            let doc::heap::PageValue::Tuple(tuple) = page else {
                return Err(Refused::Facts);
            };
            let members =
                lash_kernel_state::kernel_internals::page_reads::tuple(heap.pages(), tuple)
                    .map_err(|_| Refused::Facts)?;
            members.len() == expected.len()
                && members
                    .iter()
                    .zip(expected)
                    .all(|(page, value)| matches_page(arena, heap, page, value))
        }
        (ConstructionEvent::Set, doc::NewObject::Set(values), Some(B::Set(actual))) => {
            let mut keys = std::collections::HashSet::new();
            let mut expected = Vec::new();
            for value in values {
                if keys.insert(arena.key(value).map_err(|_| Refused::Facts)?) {
                    expected.push(*value);
                }
            }
            same_pages(arena, heap, actual, &expected)
        }
        (ConstructionEvent::Map, doc::NewObject::Map(expected), Some(B::Map(actual)))
        | (
            ConstructionEvent::Record(_),
            doc::NewObject::Record(expected),
            Some(B::Record(actual)),
        ) => {
            let mut entries: Vec<(doc::Value, doc::Value)> = Vec::new();
            let mut indices =
                std::collections::HashMap::<lash_kernel_state::arena::Key, usize>::new();
            for (key, value) in expected {
                let normalized = arena.key(key).map_err(|_| Refused::Facts)?;
                if let Some(index) = indices.get(&normalized).copied() {
                    entries[index].1 = *value;
                } else {
                    indices.insert(normalized, entries.len());
                    entries.push((*key, *value));
                }
            }
            actual.len() == entries.len()
                && actual.iter().zip(entries).all(|((a, b), (c, d))| {
                    matches_page(arena, heap, a, &c) && matches_page(arena, heap, b, &d)
                })
        }
        _ => false,
    };
    if valid { Ok(()) } else { Err(Refused::Facts) }
}

fn check_literal(
    literal: &doc::Literal,
    link: lash_kernel_value::content_ids::LinkId,
    output: &PV,
    heap: &lash_kernel_state::kernel_internals::PagedHeap,
) -> Result<(), Refused> {
    use doc::{Literal as L, heap::PageValue as P};
    let PV::Page(page) = output else {
        return Err(Refused::Facts);
    };
    let valid = match (literal, page) {
        (L::Null, P::Null) | (L::Absent, P::Absent) => true,
        (L::Bool(a), P::Bool(b)) => a == b,
        (L::Float(a), P::Float(b)) => a == b,
        (L::Function(a), P::Function(b)) => b.function == *a && b.link == link,
        (L::Int(a), P::Int(b)) => {
            lash_kernel_state::kernel_internals::page_reads::int(heap.pages(), b)
                .is_ok_and(|actual| actual == *a)
        }
        (L::Text(a), P::Text(b)) => {
            lash_kernel_state::kernel_internals::page_reads::text(heap.pages(), b)
                .is_ok_and(|actual| actual.as_ref() == a)
        }
        (L::Bytes(a), P::Bytes(b)) => {
            lash_kernel_state::kernel_internals::page_reads::bytes(heap.pages(), b)
                .is_ok_and(|actual| actual.as_ref() == a.as_slice())
        }
        _ => false,
    };
    if valid { Ok(()) } else { Err(Refused::Facts) }
}

fn matches_page(
    arena: &Arena,
    heap: &PagedHeap,
    page: &doc::heap::PageValue,
    value: &doc::Value,
) -> bool {
    use doc::{Value as V, heap::PageValue as P};
    use lash_kernel_state::kernel_internals::page_reads;
    if let Some(handle) = value.handle()
        && let Ok(facts) = arena.facts(handle)
        && let Some(expected) = facts.page.get()
        && lash_kernel_value::tokens::encode_body(expected)
            == lash_kernel_value::tokens::encode_body(page)
    {
        return true;
    }
    match (page, value) {
        (P::Null, V::Null) | (P::Absent, V::Absent) => true,
        (P::Bool(a), V::Bool(b)) => a == b,
        (P::Float(a), V::Float(b)) => a == b,
        (P::Int(a), V::Int(_) | V::BigInt(_)) => {
            page_reads::int(heap.pages(), a).ok() == arena.semantics().integer(value).ok().flatten()
        }
        (P::Text(a), V::Text(_) | V::LongText(_)) => match (
            page_reads::text(heap.pages(), a),
            arena.semantics().text(value),
        ) {
            (Ok(a), Ok(b)) => a.as_ref() == b,
            _ => false,
        },
        (P::Bytes(a), V::Bytes(handle)) => match (
            page_reads::bytes(heap.pages(), a),
            arena.semantics().bytes(*handle),
        ) {
            (Ok(a), Ok(b)) => a.as_ref() == b,
            _ => false,
        },
        (P::List(a), V::List(b))
        | (P::Map(a), V::Map(b))
        | (P::Set(a), V::Set(b))
        | (P::Record(a), V::Record(b))
        | (P::Closure(a), V::Closure(b))
        | (P::Ref(doc::heap::Identity::Object(a)), V::ObjectRef(b)) => {
            arena.facts(*b).ok().and_then(|facts| facts.ledger_entity()) == Some(*a)
        }
        (P::Task(a), V::Task(b)) | (P::Ref(doc::heap::Identity::Task(a)), V::TaskRef(b)) => a == b,
        (P::Tuple(a), V::Tuple(b)) => {
            match (page_reads::tuple(heap.pages(), a), arena.sequence(*b)) {
                (Ok(a), Ok(b)) => {
                    a.len() == b.len()
                        && a.iter()
                            .zip(b)
                            .all(|(p, v)| matches_page(arena, heap, p, v))
                }
                _ => false,
            }
        }
        (P::Function(a), V::Function(b)) => {
            matches!(arena.body(*b), Ok(doc::arena::Body::Function(b)) if a.as_ref() == b.as_ref())
        }
        _ => false,
    }
}

fn matches_literal(
    arena: &Arena,
    literal: &doc::Literal,
    link: lash_kernel_value::content_ids::LinkId,
    value: &doc::Value,
) -> bool {
    use doc::{Literal as L, Value as V};
    match (literal, value) {
        (L::Null, V::Null) | (L::Absent, V::Absent) => true,
        (L::Bool(a), V::Bool(b)) => a == b,
        (L::Float(a), V::Float(b)) => a == b,
        (L::Int(a), V::Int(_) | V::BigInt(_)) => arena
            .semantics()
            .integer(value)
            .ok()
            .flatten()
            .is_some_and(|b| b == *a),
        (L::Text(a), V::Text(_) | V::LongText(_)) => {
            arena.semantics().text(value).is_ok_and(|b| b == a)
        }
        (L::Bytes(a), V::Bytes(b)) => arena.semantics().bytes(*b).is_ok_and(|b| b == a.as_slice()),
        (L::Function(a), V::Function(b)) => {
            matches!(arena.body(*b), Ok(doc::arena::Body::Function(b)) if b.function == *a && b.link == link)
        }
        _ => false,
    }
}

fn same_pages(
    arena: &Arena,
    heap: &PagedHeap,
    actual: &[doc::heap::PageValue],
    expected: &[doc::Value],
) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .all(|(page, value)| matches_page(arena, heap, page, value))
}
