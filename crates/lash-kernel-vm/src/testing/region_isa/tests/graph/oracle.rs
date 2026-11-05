use super::*;

pub(super) struct Source {
    world: World,
    ledger: Ledger,
    heap: PagedHeap,
    keys: Arena,
    tables: BTreeMap<u64, lash_kernel_doc::arena::ArenaHandle>,
}

impl Source {
    pub(super) fn new(scene: &Scene) -> Self {
        let keys = Arena::new();
        let heap = PagedHeap::fresh(
            Box::new(crate::laws::embedder::Source::default()),
            EditEpoch(0),
        );
        let mut world = World {
            objects: BTreeMap::new(),
            roots: BTreeMap::new(),
            calls: vec![],
            existing_count: scene.initial.len() as u64,
            next_id: scene.initial.len() as u64,
            next: 0,
            balance: 0,
            peak: 0,
            charges: vec![],
            released: vec![],
        };
        for (index, (kind, members, cold)) in scene.initial.iter().enumerate() {
            world.objects.insert(
                index as u64,
                Object {
                    body: body(*kind, members, &keys, *cold),
                    born: 0,
                    invocation: 0,
                },
            );
        }
        for (slot, value) in &scene.roots {
            world.roots.insert(
                *slot,
                Root {
                    value: value.clone(),
                    producer: 0,
                    invocation: 0,
                    input: None,
                },
            );
        }
        let mut changes: Vec<_> = world
            .objects
            .keys()
            .map(|id| Change::Allocate {
                entity: entity(*id),
                body: Holding::scalar(Cost::OBJECT),
            })
            .collect();
        changes.extend(world.objects.iter().map(|(id, node)| Change::Mutate {
            entity: entity(*id),
            body: source_body_holding(&node.body, &heap),
        }));
        let mut roots = vec![(
            Owner::Frame {
                task: TaskId(0),
                invocation: 0,
            },
            Some(Holding::frame()),
        )];
        roots.extend(
            world
                .roots
                .iter()
                .map(|(slot, root)| (binding(*slot), Some(source_holding(&root.value, &heap)))),
        );
        changes.push(Change::Roots(roots));
        let mut ledger = Ledger::fresh();
        let prepared = ledger.prepare(changes, heap.pages()).unwrap();
        let accepted = prepared
            .admit(u64::MAX)
            .unwrap_or_else(|_| panic!("unbounded initial fixture"));
        ledger.commit(accepted).unwrap();
        world.balance = ledger.balance().unwrap().total().unwrap();
        world.peak = world.balance;
        let mut this = Self {
            world,
            ledger,
            heap,
            keys,
            tables: BTreeMap::new(),
        };
        let tables: Vec<_> = this
            .world
            .objects
            .iter()
            .filter(|(_, node)| matches!(node.body.kind, Kind::Map | Kind::Set | Kind::Record))
            .map(|(id, node)| (*id, node.body.entries.clone()))
            .collect();
        for (id, entries) in tables {
            this.install_table(id, &entries);
        }
        this.retention();
        this.world.released.clear();
        this
    }

    fn install_table(&mut self, id: u64, entries: &[Entry]) {
        let value = self
            .keys
            .create_map(
                entries
                    .iter()
                    .map(|entry| (entry.key.native(), lash_kernel_doc::Value::Null)),
            )
            .unwrap();
        self.tables.insert(id, value.handle().unwrap());
        self.check_table(id, entries);
    }

    fn check_table(&self, id: u64, entries: &[Entry]) {
        let mut expected: Vec<_> = entries.iter().collect();
        expected.sort_by_key(|entry| entry.seq);
        let mut after = None;
        for entry in expected {
            let (sequence, spelling) = self
                .keys
                .table_after(self.tables[&id], after)
                .unwrap()
                .unwrap();
            assert_eq!(
                sequence, entry.seq,
                "source model disagreed with canonical insertion sequence"
            );
            assert!(
                spelling.same(&entry.key.native()),
                "source model lost canonical original spelling"
            );
            after = Some(sequence);
        }
        assert!(
            self.keys
                .table_after(self.tables[&id], after)
                .unwrap()
                .is_none()
        );
    }

    fn retention(&mut self) {
        self.world.released.clear();
        self.world.objects.retain(|id, _| {
            let retained = self
                .ledger
                .entity_holding(entity(*id), self.heap.pages())
                .unwrap()
                .is_some();
            if !retained {
                self.world.released.push(*id);
            }
            retained
        });
    }

    pub(super) fn snapshot(&self, ordinal: usize, failure: Option<&str>) -> PrefixSnapshot {
        graph_snapshot(&self.world, ordinal, failure, 0)
    }

    pub(super) fn step(&mut self, scene: &Scene, op: &Op) -> Result<(), &'static str> {
        // Source speculation is deliberately itemized and may copy its model.
        // The fake lease below never calls this evaluator or ledger machinery.
        let mut next = self.world.clone();
        let mut changes = Vec::new();
        let root = |slot, value: Option<&Val>| {
            Change::Roots(vec![(
                binding(slot),
                value.map(|v| source_holding(v, &self.heap)),
            )])
        };
        let ordinal = self.world.next + 1;
        let mut scratch = 0;
        match op {
            Op::Allocate { to, kind, members } => {
                let id = next.next_id;
                next.next_id += 1;
                next.objects.insert(
                    id,
                    Object {
                        body: body(*kind, members, &self.keys, false),
                        born: ordinal,
                        invocation: self.world.calls.last().copied().unwrap_or(0),
                    },
                );
                changes.push(Change::Allocate {
                    entity: entity(id),
                    body: source_body_holding(&next.objects[&id].body, &self.heap),
                });
                let value = Val::Ref(id);
                changes.push(root(*to, Some(&value)));
                next.roots.insert(
                    *to,
                    Root {
                        value,
                        producer: ordinal,
                        invocation: self.world.calls.last().copied().unwrap_or(0),
                        input: None,
                    },
                );
            }
            Op::Root { to, value } => {
                changes.push(root(*to, value.as_ref()));
                if let Some(value) = value {
                    next.roots.insert(
                        *to,
                        Root {
                            value: value.clone(),
                            producer: ordinal,
                            invocation: self.world.calls.last().copied().unwrap_or(0),
                            input: None,
                        },
                    );
                } else {
                    next.roots.remove(to);
                }
            }
            Op::Member {
                object,
                key,
                next: value,
            } => {
                let node = next.objects.get_mut(object).ok_or("dead handle")?;
                let found = node
                    .body
                    .entries
                    .iter()
                    .position(|entry| source_equal(&self.keys, &entry.key, key));
                match (found, value) {
                    (Some(index), Some(value)) => {
                        node.body.entries[index].value = value.clone();
                        node.body.version += 1;
                    }
                    (Some(index), None) => {
                        node.body.entries.remove(index);
                        node.body.version += 1;
                        node.body.structural += 1;
                    }
                    (None, Some(value)) => {
                        let seq = node.body.next_seq;
                        node.body.next_seq += 1;
                        node.body.entries.push(Entry {
                            key: key.clone(),
                            value: value.clone(),
                            seq,
                        });
                        node.body.version += 1;
                        node.body.structural += 1;
                    }
                    (None, None) => {}
                }
                let changed = node.body.clone();
                changes.push(Change::Mutate {
                    entity: entity(*object),
                    body: source_body_holding(&changed, &self.heap),
                });
            }
            Op::Read { object, key, to }
            | Op::Callback {
                object, key, to, ..
            } => {
                let node = next.objects.get(object).ok_or("dead handle")?;
                let value = node
                    .body
                    .entries
                    .iter()
                    .find(|entry| source_equal(&self.keys, &entry.key, key))
                    .map(|entry| entry.value.clone())
                    .ok_or("missing member")?;
                changes.push(root(*to, Some(&value)));
                next.roots.insert(
                    *to,
                    Root {
                        value,
                        producer: ordinal,
                        invocation: self.world.calls.last().copied().unwrap_or(0),
                        input: Some(*object),
                    },
                );
            }
            Op::Walk { object, after, to } => {
                let value = next.objects[object]
                    .body
                    .entries
                    .iter()
                    .filter(|entry| after.is_none_or(|after| entry.seq > after))
                    .min_by_key(|entry| entry.seq)
                    .map(|entry| tuple(vec![entry.key.value(), entry.value.clone()]))
                    .unwrap_or(Val::Null);
                changes.push(root(*to, Some(&value)));
                next.roots.insert(
                    *to,
                    Root {
                        value,
                        producer: ordinal,
                        invocation: self.world.calls.last().copied().unwrap_or(0),
                        input: Some(*object),
                    },
                );
            }
            Op::Grow { object, additional } => {
                if !next.objects.contains_key(object) {
                    return Err("dead handle");
                }
                scratch = *additional as u64;
            }
            Op::Enter => {
                let call = ordinal as u64;
                next.calls.push(call);
                changes.push(Change::Roots(vec![(
                    Owner::Frame {
                        task: TaskId(0),
                        invocation: call,
                    },
                    Some(Holding::frame()),
                )]));
            }
            Op::Leave => {
                let call = next.calls.pop().ok_or("depth underflow")?;
                changes.push(Change::Roots(vec![(
                    Owner::Frame {
                        task: TaskId(0),
                        invocation: call,
                    },
                    None,
                )]));
            }
        }
        let units = source_units(op);
        let charged: u64 = self.world.charges.iter().map(|entry| entry.units).sum();
        if charged.checked_add(units).is_none_or(|n| n > scene.charge) {
            return Err("charge");
        }
        if next.calls.len() > scene.depth as usize {
            return Err("depth");
        }
        let prepared = self
            .ledger
            .prepare(changes, self.heap.pages())
            .unwrap()
            .require_source_room(scratch)
            .ok_or("memory")?;
        let phase_peak = prepared.peak();
        if phase_peak > scene.memory {
            return Err("memory");
        }
        next.balance = prepared.balance().unwrap().total().unwrap();
        next.peak = self.world.peak.max(phase_peak);
        next.charges.push(ChargeEntry {
            at: graph_coordinate(ordinal, self.world.calls.last().copied().unwrap_or(0)),
            kind: 100,
            units,
        });
        next.next += 1;
        self.ledger
            .commit(
                prepared
                    .admit(scene.memory)
                    .unwrap_or_else(|_| panic!("checked source bound")),
            )
            .unwrap();
        if let Op::Allocate {
            kind: Kind::Map | Kind::Set | Kind::Record,
            ..
        } = op
        {
            let id = next.next_id - 1;
            self.install_table(id, &next.objects[&id].body.entries);
        }
        if let Op::Member {
            object,
            key,
            next: value,
        } = op
            && let Some(handle) = self.tables.get(object).copied()
        {
            let before_position = self.keys.table_version(handle).unwrap();
            let before_content = self.keys.content_version(handle).unwrap();
            if value.is_some() {
                self.keys
                    .table_write(handle, key.native(), lash_kernel_doc::Value::Null)
                    .unwrap();
            } else {
                self.keys.table_delete(handle, &key.native()).unwrap();
            }
            let old = &self.world.objects[object].body;
            let changed = &next.objects[object].body;
            assert_eq!(
                before_position != self.keys.table_version(handle).unwrap(),
                old.structural != changed.structural
            );
            assert_eq!(
                !self.keys.content_is(&before_content),
                old.version != changed.version
            );
            self.check_table(*object, &changed.entries);
        }
        self.world = next;
        self.retention();
        Ok(())
    }
}

pub(super) fn trace(scene: &Scene, stop: usize) -> Vec<PrefixSnapshot> {
    let mut source = Source::new(scene);
    let mut out = vec![source.snapshot(0, None)];
    for (index, op) in scene.ops.iter().take(stop).enumerate() {
        let failure = source.step(scene, op).err();
        out.push(source.snapshot(index + 1, failure));
        if failure.is_some() {
            break;
        }
    }
    out
}
