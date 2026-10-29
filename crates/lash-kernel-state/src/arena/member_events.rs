//! Canonical mutators report semantic occurrences, never byte deltas. Physical
//! warming, card marking and table-index changes do not report member events.
use super::*;

#[derive(Clone, Copy, Debug)]
pub(crate) enum MemberEntry {
    Value(Value),
    Pair {
        key: Value,
        value: Value,
        record: bool,
    },
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct MemberEvent {
    pub entity: EntityId,
    pub remove: Option<MemberEntry>,
    pub add: Option<MemberEntry>,
}
impl Arena {
    pub fn logical_member_frontier(&self) -> Option<usize> {
        self.watching_members.then_some(self.member_events.len())
    }

    pub(super) fn pending_member_handles(&self, handles: &mut Vec<ArenaHandle>) {
        let mut value = |value: Value| handles.extend(value.handle());
        for event in &self.member_events {
            for entry in [event.remove, event.add].into_iter().flatten() {
                match entry {
                    MemberEntry::Value(member) => value(member),
                    MemberEntry::Pair {
                        key, value: member, ..
                    } => {
                        value(key);
                        value(member);
                    }
                }
            }
        }
    }
    pub fn watch_logical_members(&mut self) {
        self.watching_members = true;
    }
    pub(crate) fn take_member_events(&mut self) -> Vec<MemberEvent> {
        std::mem::take(&mut self.member_events)
    }
    pub(crate) fn return_member_events(&mut self, mut events: Vec<MemberEvent>, committed: bool) {
        if committed {
            events.clear();
        }
        events.append(&mut self.member_events);
        self.member_events = events;
    }
    pub(super) fn note_member(
        &mut self,
        handle: ArenaHandle,
        old: Option<Value>,
        new: Option<Value>,
    ) -> Result<(), NativeError> {
        self.note_entry(
            handle,
            old.map(MemberEntry::Value),
            new.map(MemberEntry::Value),
        )
    }
    pub(super) fn note_entry(
        &mut self,
        handle: ArenaHandle,
        remove: Option<MemberEntry>,
        add: Option<MemberEntry>,
    ) -> Result<(), NativeError> {
        if self.watching_members && !self.registering {
            // Even an elided last root must leave an itemized cyclic body.
            // The source event retains its identity until logical normalization.
            let entity = self.identify(handle)?;
            self.member_events.push(MemberEvent {
                entity,
                remove,
                add,
            });
        }
        Ok(())
    }
}
