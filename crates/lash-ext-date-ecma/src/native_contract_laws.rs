//! FIG-7532: omitted native holdings and scratch fail the shared allocation law.
#[path = "../../lash-kernel-lib/src/native_contract_observer.rs"]
mod allocation_observer;
#[path = "../../lash-kernel-lib/src/native_scratch_observer.rs"]
pub(crate) mod observer;
pub(crate) use observer::observe;

use lash_kernel_doc::{Datum, Float, Integer, Timestamp};
fn text(value: &str) -> Datum {
    Datum::Text(value.into())
}
fn check(name: &str, input: Vec<Datum>) {
    let native = super::native(
        name,
        lash_kernel_value::names::FunctionId::from_bytes([0x5a; 32]),
    )
    .unwrap();
    assert!(
        allocation_observer::check_case(&*native, name, input, observer::reset, observer::peak)
            .is_some(),
        "{name} uncertified"
    );
}
/// Reuses the fixed-UTC and extended-year corpus from remaining_builtins.rs.
#[test]
fn native_contracts_match_logical_holdings_and_reject_understatements() {
    let mut checked = 0;
    for time in [
        0.0,
        -1.0,
        951_868_737_123.0,
        -62_167_219_200_000.0,
        -62_198_755_200_000.0,
        -8.64e15,
        8.64e15,
        f64::NAN,
    ] {
        check("date.ecma.timestamp", vec![Datum::Float(Float::new(time))]);
        checked += 1;
        let date = if time.is_nan() {
            Datum::Record(vec![
                (
                    "__lash_brand".into(),
                    Datum::Tuple(vec![text("date.ecma"), text("invalid_date")]),
                ),
                (
                    "__lash_built".into(),
                    Datum::Bytes(lash_kernel_doc::Bytes::new(vec![0x5a; 32])),
                ),
            ])
        } else {
            Datum::Timestamp(Timestamp {
                nanoseconds: Integer::new((time as i128) * 1_000_000),
            })
        };
        check("date.ecma.milliseconds", vec![date.clone()]);
        checked += 1;
        for format in [
            "iso",
            "utc",
            "string",
            "date",
            "time",
            "locale_date",
            "locale_time",
            "locale",
            "unknown",
        ] {
            check("date.ecma.format", vec![date.clone(), text(format)]);
            checked += 1;
        }
    }
    for source in [
        "1970-01-01T01:00:00+01:00",
        "2000-02-29T23:58:57.123Z",
        "Thu, 01 Jan 1970 00:00:00 GMT",
        "Thu, 32 Jan 1970 00:00:00 GMT",
        "Thu Jan 01 1970 24:00:00 GMT+0000 (Coordinated Universal Time)",
        "March 7, 2024",
        "",
    ] {
        check("date.ecma.parse", vec![text(source)]);
        checked += 1;
    }
    for operation in super::Operation::ALL {
        check(operation.name(), vec![Datum::Null]);
        check(operation.name(), vec![]);
        checked += 2;
    }
    eprintln!("checked {checked} date input/exit contracts");
}
