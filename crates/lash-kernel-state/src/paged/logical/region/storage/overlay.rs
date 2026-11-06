//! Table keys use guest hashing. Static list and variable addresses use indexes.
use super::*;
#[derive(Default)]
pub(in crate::paged::logical::region) struct OverlayMap {
    table: std::collections::HashMap<Address, OverlayMember, foldhash::fast::RandomState>,
    indexed: Vec<(Address, OverlayMember)>,
}
fn index(address: &Address) -> (u8, u64) {
    match address {
        Address::List(at) => (0, *at),
        Address::Variable => (1, 0),
        Address::Table(_) => unreachable!("indexed member address"),
    }
}
impl OverlayMap {
    pub(in crate::paged::logical::region) fn get(
        &self,
        address: &Address,
    ) -> Option<&OverlayMember> {
        if matches!(address, Address::Table(_)) {
            self.table.get(address)
        } else {
            self.indexed
                .binary_search_by_key(&index(address), |(key, _)| index(key))
                .ok()
                .map(|at| &self.indexed[at].1)
        }
    }
    pub(in crate::paged::logical::region) fn get_mut(
        &mut self,
        address: &Address,
    ) -> Option<&mut OverlayMember> {
        if matches!(address, Address::Table(_)) {
            self.table.get_mut(address)
        } else {
            self.indexed
                .binary_search_by_key(&index(address), |(key, _)| index(key))
                .ok()
                .map(|at| &mut self.indexed[at].1)
        }
    }
    pub(in crate::paged::logical::region) fn is_empty(&self) -> bool {
        self.table.is_empty() && self.indexed.is_empty()
    }
    pub(in crate::paged::logical::region) fn contains_key(&self, address: &Address) -> bool {
        self.get(address).is_some()
    }
    pub(in crate::paged::logical::region) fn reserve(
        &mut self,
        address: &Address,
        count: usize,
    ) -> Result<(), HeapFault> {
        if matches!(address, Address::Table(_)) {
            self.table.try_reserve(count).map_err(|_| exhausted())
        } else {
            capacity(&mut self.indexed, count)
        }
    }
    pub(in crate::paged::logical::region) fn insert(
        &mut self,
        address: Address,
        value: OverlayMember,
    ) {
        if matches!(address, Address::Table(_)) {
            self.table.insert(address, value);
        } else {
            match self
                .indexed
                .binary_search_by_key(&index(&address), |(key, _)| index(key))
            {
                Ok(at) => self.indexed[at].1 = value,
                Err(at) => self.indexed.insert(at, (address, value)),
            }
        }
    }
    pub(in crate::paged::logical::region) fn remove(&mut self, address: &Address) {
        if matches!(address, Address::Table(_)) {
            self.table.remove(address);
        } else if let Ok(at) = self
            .indexed
            .binary_search_by_key(&index(address), |(key, _)| index(key))
        {
            self.indexed.remove(at);
        }
    }
    pub(in crate::paged::logical::region) fn iter(
        &self,
    ) -> impl Iterator<Item = (&Address, &OverlayMember)> {
        self.table
            .iter()
            .chain(self.indexed.iter().map(|(key, value)| (key, value)))
    }
    pub(in crate::paged::logical::region) fn values(&self) -> impl Iterator<Item = &OverlayMember> {
        self.iter().map(|(_, value)| value)
    }
    pub(in crate::paged::logical::region) fn keys(&self) -> impl Iterator<Item = &Address> {
        self.iter().map(|(key, _)| key)
    }
}
impl std::ops::Index<&Address> for OverlayMap {
    type Output = OverlayMember;
    fn index(&self, address: &Address) -> &OverlayMember {
        self.get(address)
            .unwrap_or_else(|| unreachable!("bound overlay address"))
    }
}
