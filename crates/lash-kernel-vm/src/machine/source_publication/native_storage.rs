//! Native storage recipes are shared by precise and packet publication.
pub(in crate::machine) mod collection;
pub(in crate::machine) mod text;
pub(in crate::machine) mod value;
use super::*;
use crate::compile::LibRun;
use lash_kernel_doc::Value;
use lash_kernel_state::kernel_internals::logical::{Owner, PublicationValue as PV};
use room::{Measure, Point};

pub(in crate::machine) enum NativeStorage<'a> {
    Text(text::TextNative<'a>),
    Collection(collection::CollectionNative<'a>),
    Value(value::ValueNative<'a>),
}
pub(in crate::machine) enum PreparedNative {
    Text(text::PreparedText),
    Collection(collection::PreparedCollection),
    Value(value::PreparedValue),
}
impl StoragePreparation for NativeStorage<'_> {
    type Prepared = PreparedNative;
    const MATERIALIZES: bool = true;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Self::Prepared>>, HeapFault> {
        match self {
            Self::Value(recipe) => Ok(recipe.prepare(arena, actual)?.map(|p| PreparedStorage {
                registration: p.registration,
                storage: PreparedNative::Value(p.storage),
                actual: p.actual,
            })),
            Self::Text(recipe) => Ok(recipe.prepare(arena, actual)?.map(|p| PreparedStorage {
                registration: p.registration,
                storage: PreparedNative::Text(p.storage),
                actual: p.actual,
            })),
            Self::Collection(recipe) => {
                Ok(recipe.prepare(arena, actual)?.map(|p| PreparedStorage {
                    registration: p.registration,
                    storage: PreparedNative::Collection(p.storage),
                    actual: p.actual,
                }))
            }
        }
    }
    fn allowance(prepared: &PreparedNative, allowance: &mut Allowance) {
        match prepared {
            PreparedNative::Value(p) => {
                <value::ValueNative<'_> as StoragePreparation>::allowance(p, allowance)
            }
            PreparedNative::Text(p) => {
                <text::TextNative<'_> as StoragePreparation>::allowance(p, allowance)
            }
            PreparedNative::Collection(p) => {
                <collection::CollectionNative<'_> as StoragePreparation>::allowance(p, allowance)
            }
        }
    }
    fn facts(prepared: &PreparedNative, facts: &mut LeafExecution) -> Result<(), HeapFault> {
        match prepared {
            PreparedNative::Value(p) => {
                <value::ValueNative<'_> as StoragePreparation>::facts(p, facts)
            }
            PreparedNative::Text(p) => {
                <text::TextNative<'_> as StoragePreparation>::facts(p, facts)
            }
            PreparedNative::Collection(p) => {
                <collection::CollectionNative<'_> as StoragePreparation>::facts(p, facts)
            }
        }
    }
}
impl PreparedNative {
    pub(in crate::machine) fn known(&self) -> bool {
        matches!(self, Self::Value(value::PreparedValue { known: true, .. }))
    }
    pub(in crate::machine) fn allocated_immutable(&self) -> bool {
        matches!(
            self,
            Self::Value(value::PreparedValue {
                allocated: true,
                ..
            })
        )
    }
    pub(in crate::machine) fn value(&self) -> Value {
        match self {
            Self::Text(p) => p.value,
            Self::Collection(p) => p.value,
            Self::Value(p) => p.value,
        }
    }
    pub(in crate::machine) fn units(&self) -> u64 {
        match self {
            Self::Text(p) => p.units,
            Self::Collection(p) => p.units,
            Self::Value(p) => p.units,
        }
    }
    pub(in crate::machine) fn work(&self) -> u64 {
        match self {
            Self::Text(p) => p.work,
            Self::Collection(p) => p.work,
            Self::Value(p) => p.work,
        }
    }
    pub(in crate::machine) fn publish(self, arena: &mut Arena) {
        match self {
            Self::Text(p) => p.batch.publish(arena),
            Self::Collection(p) => p.batch.publish(arena),
            Self::Value(p) => p.batch.publish(arena),
        }
    }
}
