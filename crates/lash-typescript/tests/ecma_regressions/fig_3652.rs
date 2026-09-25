//! FIG-3652: ToPrimitive runs an object's own valueOf and toString — array searches keep reference identity and convert a guest fromIndex.

use super::*;

/// FIG-3658: on a heap-held array the search trio compare members by live
/// heap identity — a RegExp needle finds the RegExp member and no other —
/// while a `fromIndex` whose coercion would run a guest `toString`/`valueOf`
/// refuses rather than silently reading `NaN`.
#[test]
fn array_searches_keep_reference_identity_and_convert_a_guest_from_index() {
    assert_eq!(
        finished("const r=/x/; finish([0,true,r,3,false].lastIndexOf(r,2));"),
        Value::Number(2.0)
    );
    assert_eq!(
        finished("const r=/x/; finish([0,true,3,r,false].lastIndexOf(r,2));"),
        Value::Number(-1.0)
    );
    // Same shape, different object: identity, not structure.
    assert_eq!(
        finished("finish([0,/x/,3].indexOf(/x/));"),
        Value::Number(-1.0)
    );
    assert_eq!(
        finished("const r=/x/; finish([[r],r].includes(r));"),
        Value::Bool(true)
    );
    // A fromIndex object's own toString answers its ToNumber (FIG-3652).
    assert_eq!(
        finished("finish([0,1,2].lastIndexOf(2, { toString: function() { return '2'; } }));"),
        Value::Number(2.0)
    );
    // An object with no coercion methods has only a type tag for ECMA: NaN.
    assert_eq!(
        finished("finish([0,1,2].lastIndexOf(2, {}));"),
        Value::Number(-1.0)
    );
}
