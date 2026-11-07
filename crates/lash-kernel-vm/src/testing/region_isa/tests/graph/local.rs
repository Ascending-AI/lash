use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Handle {
    slot: usize,
    generation: u64,
}
#[derive(Default, Debug)]
pub(super) struct Stats {
    pub max_slots: usize,
    pub max_members: usize,
    pub warmed: BTreeSet<u64>,
    pub callbacks: Vec<PrefixSnapshot>,
}
struct SlotEntry {
    id: Option<u64>,
    generation: u64,
}
struct Plan {
    result: Option<(u32, Option<Val>)>,
    member: Option<(u64, Key, Option<Val>)>,
    allocation: Option<(u64, Kind, Members)>,
    incoming: BTreeMap<u64, i128>,
    released: Vec<u64>,
    balance: u64,
    peak: u128,
    changed_price: Option<(u64, u64)>,
}
pub(super) struct Local {
    pub world: World,
    indexes: HashMap<u64, HashMap<Normal, usize>>,
    incoming: HashMap<u64, u64>,
    prices: HashMap<u64, u64>,
    handles: HashMap<u64, Handle>,
    slots: Vec<SlotEntry>,
    free: Vec<usize>,
    pub stats: Stats,
    salt: u64,
    charged: u64,
}

// Independent arithmetic, with no Ledger, Holding or source evaluator calls.
fn price(value: &Val) -> u64 {
    16 + match value {
        Val::Null | Val::Absent | Val::Float(_) => 0,
        Val::Int(n) => {
            let magnitude = n.unsigned_abs();
            u64::from(64 - magnitude.leading_zeros()).div_ceil(8)
        }
        Val::Text(s) => s.len() as u64,
        Val::Bytes(s) => s.len() as u64,
        Val::Ref(_) => 0,
        Val::Tuple(values) => values.iter().map(price).sum(),
    }
}
fn member_price(kind: Kind, key: &Key, value: &Val) -> u64 {
    if kind == Kind::Closure {
        return 8;
    }
    price(value)
        + match kind {
            Kind::Map | Kind::Set => price(&key.value()),
            Kind::Record => price(&key.value()) - 16,
            _ => 0,
        }
}
fn references(value: &Val) -> Vec<u64> {
    let mut result = vec![];
    value.refs(&mut |id| result.push(id));
    result
}
fn adjust(changes: &mut BTreeMap<u64, i128>, value: &Val, sign: i128) {
    value.refs(&mut |id| *changes.entry(id).or_default() += sign);
}

impl Local {
    pub(super) fn new(scene: &Scene, layout: IsaLayout) -> Self {
        let mut this = Self {
            world: World {
                objects: BTreeMap::new(),
                roots: BTreeMap::new(),
                calls: vec![],
                existing_count: scene.initial.len() as u64,
                next_id: scene.initial.len() as u64,
                next: 0,
                balance: 64,
                peak: 64,
                charges: vec![],
                released: vec![],
            },
            indexes: HashMap::new(),
            incoming: HashMap::new(),
            prices: HashMap::new(),
            handles: HashMap::new(),
            slots: vec![],
            free: vec![],
            stats: Stats::default(),
            charged: 0,
            salt: if layout == IsaLayout::Generic {
                10_000
            } else {
                20_000
            },
        };
        for (id, (kind, _, cold)) in scene.initial.iter().enumerate() {
            this.world.objects.insert(
                id as u64,
                Object {
                    body: Body {
                        kind: *kind,
                        entries: vec![],
                        next_seq: 0,
                        version: 0,
                        structural: 0,
                        cold: *cold,
                    },
                    born: 0,
                    invocation: 0,
                },
            );
        }
        for (id, (kind, members, cold)) in scene.initial.iter().enumerate() {
            this.install(id as u64, *kind, members, *cold, 0);
        }
        for (slot, value) in &scene.roots {
            this.world.roots.insert(
                *slot,
                Root {
                    value: value.clone(),
                    producer: 0,
                    invocation: 0,
                    input: None,
                },
            );
            this.world.balance += price(value);
        }
        for node in this.world.objects.values() {
            for entry in &node.body.entries {
                for id in references(&entry.value) {
                    *this.incoming.get_mut(&id).unwrap() += 1;
                }
            }
        }
        for root in this.world.roots.values() {
            for id in references(&root.value) {
                *this.incoming.get_mut(&id).unwrap() += 1;
            }
        }
        // The scenes start with source-retained objects. No unrooted acyclic
        // bootstrap object is silently kept as an artificial local root.
        assert!(this.incoming.values().all(|count| *count > 0));
        this.world.balance += this.prices.values().sum::<u64>();
        this.world.peak = this.world.balance;
        this
    }

    fn install(&mut self, id: u64, kind: Kind, members: &[(Key, Val)], cold: bool, born: usize) {
        let mut index: HashMap<Normal, usize> = HashMap::new();
        let mut entries: Vec<Entry> = vec![];
        for (key, value) in members {
            match index.get(&key.local_key()).copied() {
                Some(at) => entries[at].value = value.clone(),
                None => {
                    index.insert(key.local_key(), entries.len());
                    entries.push(Entry {
                        key: key.clone(),
                        value: value.clone(),
                        seq: entries.len() as u64,
                    });
                }
            }
        }
        let bytes = 32
            + entries
                .iter()
                .map(|entry| member_price(kind, &entry.key, &entry.value))
                .sum::<u64>();
        let body = Body {
            kind,
            next_seq: entries.len() as u64,
            entries,
            version: 0,
            structural: 0,
            cold,
        };
        self.world.objects.insert(
            id,
            Object {
                body,
                born,
                invocation: self.world.calls.last().copied().unwrap_or(0),
            },
        );
        self.indexes.insert(id, index);
        self.incoming.insert(id, 0);
        self.prices.insert(id, bytes);
        let slot = self.free.pop().unwrap_or_else(|| {
            self.slots.push(SlotEntry {
                id: None,
                generation: 0,
            });
            self.slots.len() - 1
        });
        self.slots[slot].id = Some(id);
        self.handles.insert(
            id,
            Handle {
                slot,
                generation: self.slots[slot].generation,
            },
        );
        self.stats.max_slots = self.stats.max_slots.max(self.slots.len());
        self.storage_bound();
    }

    pub(super) fn handle(&self, id: u64) -> Handle {
        self.handles[&id]
    }
    pub(super) fn resolve(&self, handle: Handle) -> Option<u64> {
        self.slots
            .get(handle.slot)
            .filter(|slot| slot.generation == handle.generation)
            .and_then(|slot| slot.id)
    }
    fn materialize(self, ordinal: usize, failure: Option<&str>) -> (PrefixSnapshot, Stats) {
        (
            graph_snapshot(&self.world, ordinal, failure, self.salt),
            self.stats,
        )
    }
    pub(super) fn snapshot(&self, ordinal: usize, failure: Option<&str>) -> PrefixSnapshot {
        graph_snapshot(&self.world, ordinal, failure, self.salt)
    }
    fn storage_bound(&mut self) {
        self.stats.max_members = self.stats.max_members.max(
            self.world
                .objects
                .values()
                .map(|node| node.body.entries.len())
                .sum(),
        );
    }

    fn stage(&self, op: &Op) -> Result<Plan, &'static str> {
        let mut result = None;
        let mut member = None;
        let mut allocation = None;
        let mut changes = BTreeMap::new();
        let mut delta = 0_i128;
        let mut scratch = 0;
        let mut changed_price = None;
        let mut allocation_peak = None;
        match op {
            Op::Allocate { to, kind, members } => {
                let id = self.world.next_id;
                // Compute requested facts without building the target body/index.
                // Descriptors in these scenes have distinct normalized keys.
                let bytes = 32
                    + members
                        .iter()
                        .map(|(key, value)| member_price(*kind, key, value))
                        .sum::<u64>();
                for (_, value) in &**members {
                    adjust(&mut changes, value, 1);
                }
                delta += i128::from(bytes);
                allocation_peak = Some(u128::from(self.world.balance) + u128::from(bytes));
                allocation = Some((id, *kind, Arc::clone(members)));
                changed_price = Some((id, bytes));
                result = Some((*to, Some(Val::Ref(id))));
            }
            Op::Root { to, value } => result = Some((*to, value.clone())),
            Op::Read { object, key, to }
            | Op::Callback {
                object, key, to, ..
            } => {
                if self
                    .handles
                    .get(object)
                    .is_none_or(|handle| self.resolve(*handle) != Some(*object))
                {
                    return Err("dead handle");
                }
                let index = self.indexes[object]
                    .get(&key.local_key())
                    .ok_or("missing member")?;
                result = Some((
                    *to,
                    Some(
                        self.world.objects[object].body.entries[*index]
                            .value
                            .clone(),
                    ),
                ));
            }
            Op::Walk { object, after, to } => {
                let value = self.world.objects[object]
                    .body
                    .entries
                    .iter()
                    .filter(|entry| after.is_none_or(|after| entry.seq > after))
                    .min_by_key(|entry| entry.seq)
                    .map(|entry| Val::Tuple(vec![entry.key.value(), entry.value.clone()].into()))
                    .unwrap_or(Val::Null);
                result = Some((*to, Some(value)));
            }
            Op::Member { object, key, next } => {
                if self
                    .handles
                    .get(object)
                    .is_none_or(|handle| self.resolve(*handle) != Some(*object))
                {
                    return Err("dead handle");
                }
                let body = &self.world.objects[object].body;
                let old = self.indexes[object]
                    .get(&key.local_key())
                    .map(|index| &body.entries[*index]);
                let mut bytes = self.prices[object];
                if let Some(old) = old {
                    bytes -= member_price(body.kind, &old.key, &old.value);
                    adjust(&mut changes, &old.value, -1);
                }
                if let Some(next) = next {
                    bytes += member_price(body.kind, old.map_or(key, |old| &old.key), next);
                    adjust(&mut changes, next, 1);
                }
                delta += i128::from(bytes) - i128::from(self.prices[object]);
                changed_price = Some((*object, bytes));
                member = Some((*object, key.clone(), next.clone()));
            }
            Op::Grow { object, additional } => {
                if self
                    .handles
                    .get(object)
                    .is_none_or(|handle| self.resolve(*handle) != Some(*object))
                {
                    return Err("dead handle");
                }
                scratch = *additional as u64;
            }
            Op::Enter => delta += 64,
            Op::Leave => {
                if self.world.calls.is_empty() {
                    return Err("depth underflow");
                }
                delta -= 64;
            }
        }
        if let Some((to, next)) = &result {
            if let Some(old) = self.world.roots.get(to) {
                delta -= i128::from(price(&old.value));
                adjust(&mut changes, &old.value, -1);
            }
            if let Some(next) = next {
                let bytes = price(next);
                delta += i128::from(bytes);
                adjust(&mut changes, next, 1);
            }
        }
        let before_release = u64::try_from(i128::from(self.world.balance) + delta).unwrap();
        let mut todo: Vec<_> = changes.keys().copied().collect();
        let mut released = BTreeSet::new();
        while let Some(id) = todo.pop() {
            if released.contains(&id) {
                continue;
            }
            let count = i128::from(*self.incoming.get(&id).unwrap_or(&0))
                + changes.get(&id).copied().unwrap_or(0);
            assert!(count >= 0, "local reference underflow at {id}");
            if count != 0 {
                continue;
            }
            released.insert(id);
            let bytes = changed_price
                .filter(|(changed, _)| *changed == id)
                .map_or_else(|| self.prices[&id], |(_, bytes)| bytes);
            delta -= i128::from(bytes);
            // Traversal is limited to the component actually released.
            let mut edges = Vec::new();
            if let Some(node) = self.world.objects.get(&id) {
                for entry in &node.body.entries {
                    if member.as_ref().is_some_and(|(object, key, _)| {
                        *object == id && entry.key.local_key() == key.local_key()
                    }) {
                        continue;
                    }
                    edges.extend(references(&entry.value));
                }
            }
            if let Some((_, _, Some(value))) =
                member.as_ref().filter(|(object, _, _)| *object == id)
            {
                edges.extend(references(value));
            }
            if let Some((_, _, entries)) =
                allocation.as_ref().filter(|(object, _, _)| *object == id)
            {
                for (_, value) in &**entries {
                    edges.extend(references(value));
                }
            }
            for target in edges {
                *changes.entry(target).or_default() -= 1;
                todo.push(target);
            }
        }
        let balance = u64::try_from(i128::from(self.world.balance) + delta).unwrap();
        // Canonical Roots releases immediately; member release occurs after its
        // body frontier. Fresh allocation has a distinct header/body frontier.
        let phase_peak = if member.is_some() {
            before_release.max(balance)
        } else {
            balance
        };
        let peak = u128::from(self.world.peak)
            .max(u128::from(self.world.balance) + u128::from(scratch))
            .max(u128::from(phase_peak))
            .max(allocation_peak.unwrap_or(0));
        let released = released.into_iter().collect();
        Ok(Plan {
            result,
            member,
            allocation,
            incoming: changes,
            released,
            balance,
            peak,
            changed_price,
        })
    }

    pub(super) fn step(&mut self, scene: &Scene, op: &Op) -> Result<(), &'static str> {
        let plan = self.stage(op)?;
        let units = match op {
            Op::Allocate { members, .. } => members.len() as u64 + 3,
            Op::Member { next, .. } => {
                if next.is_some() {
                    3
                } else {
                    2
                }
            }
            Op::Enter => 5,
            Op::Leave | Op::Grow { .. } => 1,
            _ => 2,
        };
        if u128::from(self.charged) + u128::from(units) > u128::from(scene.charge) {
            return Err("charge");
        }
        let next_depth = self.world.calls.len() + usize::from(matches!(op, Op::Enter));
        if next_depth > scene.depth as usize {
            return Err("depth");
        }
        if plan.peak > u128::from(scene.memory) {
            return Err("memory");
        }
        let ordinal = self.world.next + 1;
        let invocation = self.world.calls.last().copied().unwrap_or(0);
        let cold_read = match op {
            Op::Read { object, .. }
            | Op::Callback { object, .. }
            | Op::Walk { object, .. }
            | Op::Member { object, .. }
                if self.world.objects[object].body.cold =>
            {
                Some(*object)
            }
            _ => None,
        };
        if let Some((id, kind, members)) = &plan.allocation {
            self.install(*id, *kind, members, false, ordinal);
            self.world.next_id += 1;
        }
        if let Some((object, key, next)) = plan.member {
            let index = self.indexes.get_mut(&object).unwrap();
            let body = &mut self.world.objects.get_mut(&object).unwrap().body;
            match (index.get(&key.local_key()).copied(), next) {
                (Some(at), Some(next)) => {
                    body.entries[at].value = next;
                    body.version += 1;
                }
                (Some(at), None) => {
                    index.remove(&key.local_key());
                    body.entries.swap_remove(at);
                    if let Some(moved) = body.entries.get(at) {
                        index.insert(moved.key.local_key(), at);
                    }
                    body.version += 1;
                    body.structural += 1;
                }
                (None, Some(next)) => {
                    index.insert(key.local_key(), body.entries.len());
                    body.entries.push(Entry {
                        key,
                        value: next,
                        seq: body.next_seq,
                    });
                    body.next_seq += 1;
                    body.version += 1;
                    body.structural += 1;
                }
                (None, None) => {}
            }
        }
        if let Some((object, bytes)) = plan.changed_price {
            self.prices.insert(object, bytes);
        }
        if let Some((to, next)) = plan.result {
            if let Some(value) = next {
                self.world.roots.insert(
                    to,
                    Root {
                        value,
                        producer: ordinal,
                        invocation: self.world.calls.last().copied().unwrap_or(0),
                        input: match op {
                            Op::Read { object, .. }
                            | Op::Walk { object, .. }
                            | Op::Callback { object, .. } => Some(*object),
                            _ => None,
                        },
                    },
                );
            } else {
                self.world.roots.remove(&to);
            }
        }
        for (id, delta) in plan.incoming {
            let old = self.incoming.get(&id).copied().unwrap_or(0);
            self.incoming
                .insert(id, u64::try_from(i128::from(old) + delta).unwrap());
        }
        for id in &plan.released {
            self.world.objects.remove(id);
            self.indexes.remove(id);
            self.prices.remove(id);
            self.incoming.remove(id);
            let handle = self.handles.remove(id).unwrap();
            self.slots[handle.slot].id = None;
            self.slots[handle.slot].generation += 1;
            self.free.push(handle.slot);
        }
        match op {
            Op::Enter => self.world.calls.push(ordinal as u64),
            Op::Leave => {
                self.world.calls.pop();
            }
            _ => {}
        }
        if let Some(object) = cold_read {
            self.stats.warmed.insert(object);
        }
        self.world.balance = plan.balance;
        self.world.peak = u64::try_from(plan.peak).unwrap();
        self.world.released = plan.released;
        self.charged += units;
        self.world.charges.push(ChargeEntry {
            at: graph_coordinate(ordinal, invocation),
            kind: 100,
            units,
        });
        self.world.next += 1;
        self.storage_bound();
        Ok(())
    }
}

pub(super) fn trace(scene: &Scene, layout: IsaLayout, stop: usize) -> (Vec<PrefixSnapshot>, Stats) {
    let mut local = Local::new(scene, layout);
    let mut out = vec![local.snapshot(0, None)];
    for (index, op) in scene.ops.iter().take(stop).enumerate() {
        let failure = local.step(scene, op).err();
        out.push(local.snapshot(index + 1, failure));
        if failure.is_some() {
            break;
        }
    }
    let last = out.last().unwrap();
    let ordinal = last.facts.at.site as usize;
    let failure = last.facts.failure.as_ref().map(|(why, _)| why.as_str());
    let (frozen, stats) = local.materialize(ordinal, failure);
    *out.last_mut().unwrap() = frozen;
    (out, stats)
}
