//! Immediate relation contracts cannot claim graph reads or native scratch.
use super::*;
use lash_kernel_doc::arena::native::Semantics;
use lash_kernel_doc::arena::{ArenaHandle, Objects, ShortText};
use lash_kernel_doc::heap::NormalKey;
use lash_kernel_doc::{
    Args, LogicalHeldV1, NativeResourceContract, NativeResourceExit, NativeResourceOrigin,
};
use lash_kernel_value::data_limits::KernelDataLimits;

struct NoGraph;
impl Semantics for NoGraph {
    fn equal(&self, _: &Value, _: &Value) -> Result<bool, NativeError> {
        panic!("a scalar relation must not read graph equality")
    }
    fn check_key(&self, _: &Value) -> Result<(), NativeError> {
        panic!("a scalar relation must not inspect keys")
    }
    fn lookup(&self, _: ArenaHandle, _: &Value) -> Result<Option<&Value>, NativeError> {
        panic!("a scalar relation must not read graph members")
    }
    fn normal_key(&self, _: &Value) -> Result<Option<NormalKey>, NativeError> {
        panic!("a scalar relation must not normalize keys")
    }
    fn depth(&self, _: &Value) -> u32 {
        panic!("a scalar relation must not traverse a graph")
    }
    fn limits(&self) -> KernelDataLimits {
        KernelDataLimits::default()
    }
}

#[test]
fn relate_scalar_contract() {
    let objects = Objects::default();
    for name in ["ts.lt", "ts.le", "ts.gt", "ts.ge"] {
        let native = native(name, lash_kernel_lib::native).unwrap();
        for (left, right, lt, le, gt, ge) in [
            (
                Value::Float(lash_kernel_doc::Float::new(-0.0)),
                Value::Float(lash_kernel_doc::Float::new(0.0)),
                false,
                true,
                false,
                true,
            ),
            (
                Value::Float(lash_kernel_doc::Float::new(f64::NAN)),
                Value::Float(lash_kernel_doc::Float::new(1.0)),
                false,
                false,
                false,
                false,
            ),
            (
                Value::Float(lash_kernel_doc::Float::new(1.0)),
                Value::Float(lash_kernel_doc::Float::new(2.0)),
                true,
                true,
                false,
                false,
            ),
            (
                Value::Text(ShortText::new("\u{10000}").unwrap()),
                Value::Text(ShortText::new("\u{e000}").unwrap()),
                true,
                true,
                false,
                false,
            ),
        ] {
            let values = [left, right];
            let expected = match name {
                "ts.lt" => lt,
                "ts.le" => le,
                "ts.gt" => gt,
                _ => ge,
            };
            let prepared = native.prepare().unwrap();
            assert!(
                matches!((prepared.entry)(&values), Some(Value::Bool(value)) if value == expected)
            );
            let contract = native
                .resource_contract(Args::new(&values, &objects, &NoGraph))
                .unwrap()
                .expect(
                    "a supported scalar relation must authenticate its immutable no-scratch result",
                );
            assert_eq!(
                contract,
                NativeResourceContract::value(
                    NativeResourceExit::Returned,
                    LogicalHeldV1::REFERENCE,
                    NativeResourceOrigin::Immutable
                )
            );
        }
        for values in [
            [Value::Int(1), Value::Int(2)],
            [Value::Float(lash_kernel_doc::Float::new(1.0)), Value::Null],
            [
                Value::LongText(ArenaHandle::new(0, 1)),
                Value::LongText(ArenaHandle::new(1, 1)),
            ],
        ] {
            assert!(
                native
                    .resource_contract(Args::new(&values, &objects, &NoGraph))
                    .unwrap()
                    .is_none(),
                "declined and delegated graph paths need their own resource proof"
            );
        }
    }
}
