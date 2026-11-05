//! Source field/index semantics read only D's current graph, including edits.
use super::super::{operands::Operand, *};
use super::{Running, scalar::ScalarAnswer};
use crate::compile::region_isa::{Pc, ReadSpec, Reg, Selector};
use lash_kernel_doc::{NativeError, ValueKind};
use lash_kernel_state::kernel_internals::logical::{LeaseFault, LeaseMember, LeaseValue};

fn refused(kind: &'static str, message: impl Into<String>) -> LeaseFault {
    NativeError::Refused {
        kind,
        message: message.into(),
    }
    .into()
}
impl Running<'_> {
    pub(super) fn read_answer(
        &mut self,
        spec: &ReadSpec,
        base: Reg<LeaseValue>,
        dst: Reg<LeaseValue>,
        next: Pc,
    ) -> Result<ScalarAnswer, LeaseFault> {
        let base = self.value(base).map_err(region_fault)?;
        let kind = self.view.kind(base)?;
        match spec.selector() {
            Selector::Field(_) if !matches!(kind, ValueKind::Record | ValueKind::Error) => {
                return Err(refused(
                    "type_error",
                    "only a record or an error has fields",
                ));
            }
            Selector::Index(_)
                if !matches!(
                    kind,
                    ValueKind::List
                        | ValueKind::Tuple
                        | ValueKind::Record
                        | ValueKind::Map
                        | ValueKind::Set
                ) =>
            {
                return Err(refused(
                    "type_error",
                    "only a list, a tuple, a record, a map or a set is read by index",
                ));
            }
            _ => {}
        }
        let object = self.view.object(base)?;
        // The version and identity come from the same exclusive view as the
        // member. A cached derived read may never outlive this version.
        let _version = self.view.version(object)?;
        let value = match spec.selector() {
            Selector::Field(name) => {
                if !matches!(kind, ValueKind::Record | ValueKind::Error) {
                    return Err(refused(
                        "type_error",
                        "only a record or an error has fields",
                    ));
                }
                self.view
                    .read(object, &LeaseMember::Field(Arc::clone(name)))?
            }
            Selector::Index(index) => {
                let key = self.value(*index).map_err(region_fault)?;
                match kind {
                    ValueKind::List | ValueKind::Tuple => {
                        let position = match self.view.inline(key)? {
                            Some(Value::Int(word)) => usize::try_from(word).ok(),
                            Some(Value::Float(value)) => {
                                let f = value.get();
                                (f.fract() == 0.0 && f >= 0.0 && f < usize::MAX as f64)
                                    .then_some(f as usize)
                            }
                            None if self.view.kind(key)? == ValueKind::Int => None,
                            _ => {
                                return Err(refused("type_error", "a list is indexed by a number"));
                            }
                        };
                        let found = match position {
                            Some(index) => {
                                self.view.lookup(object, &LeaseMember::List(index as u64))?
                            }
                            None => None,
                        };
                        match found {
                            Some(value) => value,
                            None => {
                                return Err(refused(
                                    "index_out_of_range",
                                    format!(
                                        "no element there; the length is {}",
                                        self.view.member_count(object)?
                                    ),
                                ));
                            }
                        }
                    }
                    ValueKind::Record => {
                        if self.view.kind(key)? != ValueKind::Text {
                            return Err(refused("type_error", "a record index must be text"));
                        }
                        self.view.read(object, &LeaseMember::Table(key))?
                    }
                    ValueKind::Map => self
                        .view
                        .lookup(object, &LeaseMember::Table(key))?
                        .ok_or_else(|| {
                            refused("key_missing", "the map has no entry under that key")
                        })?,
                    ValueKind::Set => {
                        let present = self
                            .view
                            .lookup(object, &LeaseMember::Table(key))?
                            .is_some();
                        self.view.import(&Value::Bool(present))?
                    }
                    _ => {
                        return Err(refused(
                            "type_error",
                            "only a list, a tuple, a record, a map or a set is read by index",
                        ));
                    }
                }
            }
        };
        Ok(ScalarAnswer {
            write: Some((
                super::bind::physical(&self.code.program, dst).map_err(region_fault)?,
                Operand::Dynamic(value),
            )),
            next,
            formula: None,
        })
    }
}
fn region_fault(error: RegionError) -> LeaseFault {
    match error {
        RegionError::Heap(fault) => LeaseFault::Heap(fault),
        RegionError::Invalid(_) => LeaseFault::Heap(HeapFault::Malformed(
            "verified read has an invalid register".into(),
        )),
    }
}
