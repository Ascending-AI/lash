//! Fixture graph rules use zero-byte source roots and one unit per body/member.
//! Production facts remain exclusively D-owned.
use super::storage::{self, Facts, Ready, Step};
use super::storage_fixture::{FixtureValue, FixtureView, Object, bad};
use lash_kernel_doc::{Value, arena::ArenaHandle, heap::HeapFault};
use lash_kernel_state::kernel_internals::logical::LeaseFault;

pub(super) struct FixtureFacts {
    delta: i128,
    peak: u64,
    members: u64,
    released: u64,
}
pub(super) struct FixtureStep<'a> {
    view: &'a mut FixtureView,
    change: Change<'a>,
    facts: FixtureFacts,
}
enum Change<'a> {
    List(&'a [FixtureValue]),
    Member {
        index: usize,
        at: usize,
        next: FixtureValue,
        version: u64,
    },
    Grow {
        index: usize,
        additional: usize,
    },
    References,
}
pub(super) struct FixtureReady<'a> {
    step: FixtureStep<'a>,
    body: Option<Vec<FixtureValue>>,
    result: Option<FixtureValue>,
}
impl Facts for FixtureFacts {
    fn delta(&self) -> i128 {
        self.delta
    }
    fn peak(&self) -> u64 {
        self.peak
    }
    fn measure(&self, selector: storage::FactMeasure) -> Option<u64> {
        Some(match selector {
            storage::FactMeasure::Members => self.members,
            storage::FactMeasure::Payload => 0,
            storage::FactMeasure::Released => self.released,
            storage::FactMeasure::Depth => 0,
        })
    }
}
impl FixtureView {
    fn object_index(&self, value: FixtureValue) -> Result<usize, HeapFault> {
        self.object(value)?;
        let FixtureValue::Object { index, .. } = value else {
            unreachable!()
        };
        Ok(index as usize)
    }
    fn reference(&mut self, value: FixtureValue, amount: i128) -> Result<u64, HeapFault> {
        self.work.push((value, amount));
        let mut released = 0u64;
        while let Some((value, amount)) = self.work.pop() {
            let FixtureValue::Object { .. } = value else {
                continue;
            };
            let index = self.object_index(value)?;
            let at = match self
                .references
                .iter()
                .position(|(candidate, _)| *candidate == index)
            {
                Some(at) => at,
                None => {
                    self.references
                        .try_reserve(1)
                        .map_err(|_| bad("fixture reference scratch exhausted"))?;
                    self.references.push((index, self.objects[index].retained));
                    self.references.len() - 1
                }
            };
            let before = self.references[at].1;
            let after = i128::from(before)
                .checked_add(amount)
                .and_then(|n| u64::try_from(n).ok())
                .ok_or_else(|| bad("fixture reference multiplicity overflow"))?;
            if before == 0 {
                return Err(bad("fixture phase uses a released component"));
            }
            self.references[at].1 = after;
            if after == 0 {
                let members = &self.objects[index].members;
                released = released
                    .checked_add(1 + members.len() as u64)
                    .ok_or_else(|| bad("fixture release overflow"))?;
                self.work
                    .try_reserve(members.len())
                    .map_err(|_| bad("fixture release scratch exhausted"))?;
                self.work.extend(members.iter().map(|value| (*value, -1)));
                self.objects[index]
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| bad("fixture generation overflow"))?;
            }
        }
        Ok(released)
    }
    pub(super) fn stage_phase<'a>(
        &'a mut self,
        operation: storage::Operation<'a, FixtureValue>,
    ) -> Result<FixtureStep<'a>, LeaseFault> {
        if self.frozen {
            return Err(bad("fixture graph is frozen").into());
        }
        self.references.clear();
        self.work.clear();
        self.work
            .try_reserve(1)
            .map_err(|_| bad("fixture release scratch exhausted"))?;
        let mut released = 0;
        let mut allocated = 0;
        let mut members = 0;
        let change = match operation {
            storage::Operation::List(values) | storage::Operation::Tuple(values) => {
                for value in values {
                    released += self.reference(*value, 1)?;
                }
                members = values.len() as u64;
                allocated = members
                    .checked_add(1)
                    .ok_or_else(|| bad("fixture allocation overflow"))?;
                self.next_identity
                    .checked_add(1)
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| bad("fixture identity overflow"))?;
                Change::List(values)
            }
            storage::Operation::Member {
                object,
                member,
                next,
            } => {
                let index = self.object_index(object)?;
                let at = match member {
                    storage::Selector::List(index) => {
                        usize::try_from(index).map_err(|_| bad("fixture member overflow"))?
                    }
                    storage::Selector::Variable => 0,
                    _ => return Err(bad("fixture member selector unsupported").into()),
                };
                let old = *self.objects[index]
                    .members
                    .get(at)
                    .ok_or_else(|| bad("fixture member absent"))?;
                let next = next.ok_or_else(|| bad("fixture list member cannot be deleted"))?;
                let version = self.objects[index]
                    .version
                    .checked_add(1)
                    .ok_or_else(|| bad("fixture version overflow"))?;
                self.reference(next, 1)?;
                released = self.reference(old, -1)?;
                Change::Member {
                    index,
                    at,
                    next,
                    version,
                }
            }
            storage::Operation::Grow { object, additional } => {
                let index = self.object_index(object)?;
                Change::Grow {
                    index,
                    additional: usize::try_from(additional)
                        .map_err(|_| bad("fixture growth overflow"))?,
                }
            }
            storage::Operation::Hold { value, count } => {
                self.reference(value, i128::from(count))?;
                Change::References
            }
            storage::Operation::Release { value, count } => {
                released = self.reference(value, -i128::from(count))?;
                Change::References
            }
            _ => return Err(bad("fixture body kind outside these laws").into()),
        };
        let facts = FixtureFacts {
            delta: i128::from(allocated) - i128::from(released),
            peak: allocated,
            members,
            released,
        };
        Ok(FixtureStep {
            view: self,
            change,
            facts,
        })
    }
}
impl<'a> Step for FixtureStep<'a> {
    type Facts = FixtureFacts;
    type Value = FixtureValue;
    type Ready = FixtureReady<'a>;
    fn facts(&self) -> &FixtureFacts {
        &self.facts
    }
    fn reserve(self) -> Result<FixtureReady<'a>, HeapFault> {
        if self.view.fail_reserve {
            return Err(bad("injected fixture reserve failure"));
        }
        self.view
            .free
            .try_reserve(self.view.references.len())
            .map_err(|_| bad("fixture free-list reserve failed"))?;
        let (body, result) = match self.change {
            Change::List(values) => {
                let mut body = Vec::new();
                body.try_reserve_exact(values.len())
                    .map_err(|_| bad("fixture allocation reserve failed"))?;
                body.extend_from_slice(values);
                let index = match self.view.free.last() {
                    Some(index) => *index,
                    None => {
                        self.view
                            .objects
                            .try_reserve(1)
                            .map_err(|_| bad("fixture slab reserve failed"))?;
                        self.view.objects.len()
                    }
                };
                let generation = self
                    .view
                    .objects
                    .get(index)
                    .map_or(1, |object| object.generation);
                (
                    Some(body),
                    Some(FixtureValue::Object {
                        lease: self.view.lease,
                        index: u32::try_from(index)
                            .map_err(|_| bad("fixture slab index overflow"))?,
                        generation,
                    }),
                )
            }
            Change::Grow { index, additional } => {
                self.view.objects[index]
                    .members
                    .try_reserve(additional)
                    .map_err(|_| bad("fixture growth reserve failed"))?;
                (None, None)
            }
            _ => (None, None),
        };
        Ok(FixtureReady {
            step: self,
            body,
            result,
        })
    }
}
impl Ready for FixtureReady<'_> {
    type Value = FixtureValue;
    fn result(&self) -> Option<FixtureValue> {
        self.result
    }
    fn accept(self) -> Option<FixtureValue> {
        let view = self.step.view;
        match self.step.change {
            Change::List(_) => {
                let FixtureValue::Object {
                    index, generation, ..
                } = self.result.expect("reserved fixture allocation result")
                else {
                    unreachable!()
                };
                let identity = Value::List(ArenaHandle::new(
                    u32::MAX
                        - u32::try_from(view.next_identity)
                            .expect("fixture identity range checked at stage"),
                    1,
                ));
                let object = Object {
                    identity,
                    members: self.body.expect("reserved fixture body"),
                    generation,
                    version: 0,
                    retained: 1,
                    alive: true,
                };
                if index as usize == view.objects.len() {
                    view.objects.push(object);
                } else {
                    view.free.pop();
                    view.objects[index as usize] = object;
                }
                view.next_identity += 1;
            }
            Change::Member {
                index,
                at,
                next,
                version,
            } => {
                view.objects[index].members[at] = next;
                view.objects[index].version = version;
            }
            Change::Grow { .. } | Change::References => (),
        }
        for &(index, retained) in &view.references {
            let object = &mut view.objects[index];
            object.retained = retained;
            if retained == 0 {
                object.members.clear();
                object.alive = false;
                object.generation += 1;
                view.free.push(index);
            }
        }
        view.references.clear();
        self.result
    }
}
