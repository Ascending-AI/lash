//! FIG-7532: omitted native holdings and scratch fail the shared allocation law.
#[path = "../../lash-kernel-lib/src/native_contract_observer.rs"]
mod allocation_observer;
#[path = "../../lash-kernel-lib/src/native_scratch_observer.rs"]
pub(crate) mod observer;
pub(crate) use observer::observe;

use crate::{Operation, matcher::Found};
use lash_kernel_doc::{Bytes, Datum, Integer};
use std::ops::Range;

pub(crate) fn range_bytes(range: &Option<Range<usize>>) -> u64 {
    match range {
        None => 16,
        Some(range) => {
            48 + Integer::new(range.start).bits().div_ceil(8)
                + Integer::new(range.end).bits().div_ceil(8)
        }
    }
}
pub(crate) fn found_bytes(found: &Found) -> u64 {
    96 + range_bytes(&Some(found.range.clone()))
        + found.captures.iter().map(range_bytes).sum::<u64>()
        + found
            .named
            .iter()
            .map(|(name, range)| name.len() as u64 + range_bytes(range))
            .sum::<u64>()
}
fn text(value: &str) -> Datum {
    Datum::Text(value.into())
}
fn regex(pattern: &str, flags: &str, last_index: i64) -> Datum {
    Datum::Record(vec![
        (
            "__lash_brand".into(),
            Datum::Tuple(vec![text(crate::BRAND), text("reg_exp")]),
        ),
        (
            "__lash_built".into(),
            Datum::Bytes(Bytes::new(vec![0x5a; 32])),
        ),
        ("pattern".into(), text(pattern)),
        ("flags".into(), text(flags)),
        ("lastIndex".into(), Datum::Int(Integer::from(last_index))),
    ])
}
fn check(native: &dyn lash_kernel_doc::NativeFunction, operation: Operation, input: Vec<Datum>) {
    assert!(
        allocation_observer::check_case(
            native,
            operation.name(),
            input,
            observer::reset,
            observer::peak
        )
        .is_some(),
        "{:?} uncertified",
        operation
    );
}
/// The existing regex corpus exercises empty/global/sticky matches, named and
/// optional captures, shared index spans, replacement expansion and late errors.
#[test]
fn native_contracts_match_logical_holdings_and_reject_understatements() {
    let mut checked = 0;
    for capacity in [0, 8] {
        let engine = std::sync::Arc::new(crate::Engine::new(capacity));
        for operation in Operation::ALL {
            let native =
                crate::native(operation.name(), &engine, crate::testsupport::built()).unwrap();
            if operation == Operation::CompileCheck {
                for (pattern, flags) in [
                    ("a+", "yg"),
                    ("a(b", ""),
                    ("a", "gg"),
                    ("a", "x"),
                    ("a", "v"),
                    ("a**", ""),
                ] {
                    check(&*native, operation, vec![text(pattern), text(flags)]);
                    checked += 1;
                }
            } else {
                for (pattern, flags, input, last) in [
                    ("a(b+)(?<tail>c)?", "g", "xxabbbc ab", 0),
                    ("(?<year>\\d{4})-(x)?(\\d\\d)", "d", "on 2026-10", 0),
                    ("(?<same>a)|(?<same>b)", "dgu", "ab", 0),
                    ("", "gu", "a😀b", 0),
                    ("", "g", "a😀b", 0),
                    ("a", "gy", "aaXa", 0),
                    ("a", "y", "aXaXa", 1),
                    ("a", "g", "aXaXa", 9),
                    ("b", "", "aXaXa", 4),
                    ("(\\d)", "", "a1b2c", 0),
                    ("(x)?b", "", "ab", 0),
                    ("x*", "", "", 0),
                    ("x", "", "", 0),
                    (".", "", "😀", 0),
                    (".", "u", "😀", 0),
                    ("a(b", "", "ab", 0),
                    ("a", "g", "a", -1),
                    ("(?<first>\\w+) (\\w+)", "", "<hello world>", 7),
                    ("a|.", "d", "a😀", 0),
                ] {
                    let mut args = vec![regex(pattern, flags, last), text(input)];
                    if operation == Operation::Replace {
                        args.push(text("$2 $<first>|$$|$&|$`|$'|$3|$02|$<none>"));
                    }
                    check(&*native, operation, args);
                    checked += 1;
                }
                let mut args = vec![
                    regex("a(b+)(?<tail>c)?", "dg", 0),
                    text(&"xxabbbc ab".repeat(65)),
                ];
                if operation == Operation::Replace {
                    args.push(text("[$&]"));
                }
                check(&*native, operation, args);
                checked += 1;
                if operation == Operation::Split {
                    for limit in [0, 2, -1] {
                        check(
                            &*native,
                            operation,
                            vec![
                                regex("(\\d)", "", 0),
                                text("a1b2c"),
                                Datum::Int(Integer::from(limit)),
                            ],
                        );
                        checked += 1;
                    }
                }
            }
            check(&*native, operation, vec![Datum::Null]);
            check(&*native, operation, vec![]);
            checked += 2;
        }
    }
    eprintln!("checked {checked} regex input/exit contracts");
}
