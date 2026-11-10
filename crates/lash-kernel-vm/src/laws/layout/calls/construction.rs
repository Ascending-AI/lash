//! K-EVAL-008 and K-ID-004 bind constructor contents to source roles.
use super::*;

/// K-CLO-001 and K-EVAL-008 preserve real capture aliases across cold
/// reopens at every finite slice.
#[test]
fn constructors_and_capture_aliases_survive_cold_slices() {
    use crate::laws::embedder::ROOMY;
    const BODY: &str = "let run = fn(value) { let left = [value] let right = [value] let getter = fn() { return (right, left) } let alias = getter let empty_a = () let empty_b = () let values = [left, right] let pair = (left, right) let table = map{1: left, 1.0: right} let members = set{1, 1.0, 2} let record = {rght: right, left: left} do \"a deferred literal larger than the inline text capacity\" do 1000000000000000000000000 do b\"0011ff\" do perform 0(values) as Any set right = left let got = apply alias() return (got, values, pair, table, members, record, empty_a, empty_b) } return apply run(7)";
    const NATIVE: &str = "let f = fn(value) { return resident.len([value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value]) } return apply f(7)";
    const RETURN: &str = "let f = fn(value) { return [value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value, value] } return apply f(7)";
    const SESSION: &str = "let saved = [7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7] set saved = [saved, saved] return saved";

    for body in [SOURCE_CELL, BODY, NATIVE, RETURN, SESSION] {
        let text = format!("main {{ {body} }}");
        for taped in [false, true] {
            let whole = observe(&text, u64::MAX, ROOMY, taped);
            for slice in [1, 3, 7] {
                assert_eq!(
                    observe_reopening(&text, slice, ROOMY, taped, true).outcome(),
                    whole.outcome(),
                    "cold slice {slice}, taped={taped}: {body}"
                );
            }
        }
    }
}
