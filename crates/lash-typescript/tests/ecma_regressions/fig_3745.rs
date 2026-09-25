//! FIG-3745: the ToPropertyKey and ToLength gaps FIG-3652's guest hooks
//! exposed. A compound member assignment or update converts its computed key
//! once, `RegExp.prototype.exec` reads `lastIndex` through the guest
//! coercion even when the flags discard the number, and a computed
//! object-literal key is converted before its value expression is evaluated.

use super::*;

/// The member reference a compound assignment evaluates holds the converted
/// key: ToPropertyKey runs once, during the member's evaluation, and the
/// read and the store share it. Every compound operator takes the one path.
#[test]
fn compound_assignment_converts_a_computed_key_once() {
    for op in [
        "+=", "-=", "*=", "/=", "%=", "**=", "<<=", ">>=", ">>>=", "&=", "^=", "|=",
    ] {
        let source = format!(
            "const s = {{ n: 0 }};\n\
             const k = {{ toString: function() {{ s.n = s.n + 1; return 'x'; }} }};\n\
             const o = {{ x: 4 }};\n\
             o[k] {op} 2;\n\
             finish(s.n);"
        );
        assert_eq!(finished(&source), Value::Number(1.0), "{op}");
    }
    // Plain member assignment converts once too, before the value runs.
    assert_eq!(
        finished(
            "const s = { n: 0 };\n\
             const k = { toString: function() { s.n = s.n + 1; return 'x'; } };\n\
             const o = {};\n\
             o[k] = 9;\n\
             finish(s.n);"
        ),
        Value::Number(1.0)
    );
}

/// The update operators evaluate the same reference: `o[k]++`, `o[k]--`,
/// `++o[k]` and `--o[k]` each convert `k` once.
#[test]
fn member_updates_convert_a_computed_key_once() {
    for op in ["o[k]++", "o[k]--", "++o[k]", "--o[k]"] {
        let source = format!(
            "const s = {{ n: 0 }};\n\
             const k = {{ toString: function() {{ s.n = s.n + 1; return 'x'; }} }};\n\
             const o = {{ x: 4 }};\n\
             {op};\n\
             finish([s.n, o.x]);"
        );
        let expected = Value::List(
            vec![
                Value::Number(1.0),
                Value::Number(if op.contains("++") { 5.0 } else { 3.0 }),
            ]
            .into(),
        );
        assert_eq!(finished(&source), expected, "{op}");
    }
}

/// ToPropertyKey runs when the member is evaluated — before `GetValue` on
/// the reference and before the right-hand side.
#[test]
fn the_converted_key_is_evaluated_before_the_member_read_and_the_rhs() {
    assert_eq!(
        finished(
            "const log = [];\n\
             const k = { toString: function() { log.push('k'); return 'x'; } };\n\
             const o = { x: 1 };\n\
             const rhs = function() { log.push('v'); o.x = 9; return 2; };\n\
             o[k] += rhs();\n\
             finish([log.join(','), o.x]);"
        ),
        Value::List(
            vec![
                Value::String("k,v".into()),
                // The read saw the old 1; the rhs's write to o.x is then
                // overwritten by the compound store: 1 + 2.
                Value::Number(3.0),
            ]
            .into()
        )
    );
}

/// `RegExpBuiltinExec` performs `ToLength(Get(R, "lastIndex"))` on every
/// call: a guest `valueOf` on the stored value runs even when `global` and
/// `sticky` are both unset and the number is discarded, and a stateful
/// failure still writes `0` through the ordinary `Set`.
#[test]
fn exec_reads_last_index_through_the_guest_coercion() {
    // No flags: one read whose number is discarded, and `lastIndex` keeps
    // the raw stored value.
    assert_eq!(
        finished(
            "const s = { n: 0 };\n\
             const c = { valueOf: function() { s.n = s.n + 1; return 0; } };\n\
             const r = /a/;\n\
             r.lastIndex = c;\n\
             const m = r.exec('nbc');\n\
             finish([m === null, r.lastIndex === c, s.n]);"
        ),
        Value::List(vec![Value::Bool(true), Value::Bool(true), Value::Number(1.0)].into())
    );
    // Same shape with a match: still one read, still no write.
    assert_eq!(
        finished(
            "const s = { n: 0 };\n\
             const c = { valueOf: function() { s.n = s.n + 1; return 0; } };\n\
             const r = /./;\n\
             r.lastIndex = c;\n\
             const m = r.exec('abc');\n\
             finish([m[0], r.lastIndex === c, s.n]);"
        ),
        Value::List(
            vec![
                Value::String("a".into()),
                Value::Bool(true),
                Value::Number(1.0),
            ]
            .into()
        )
    );
    // Global and the match fails: the read runs once and the failure writes
    // `0`, as ECMA's `Set(R, "lastIndex", 0)` does.
    assert_eq!(
        finished(
            "const s = { n: 0 };\n\
             const c = { valueOf: function() { s.n = s.n + 1; return 42; } };\n\
             const r = /a/g;\n\
             r.lastIndex = c;\n\
             const m = r.exec('abc');\n\
             finish([m === null, r.lastIndex, s.n]);"
        ),
        Value::List(vec![Value::Bool(true), Value::Number(0.0), Value::Number(1.0)].into())
    );
    // A ToLength of a nonpositive answer still starts at 0: no match, one
    // read, `0` written.
    assert_eq!(
        finished(
            "const s = { n: 0 };\n\
             const c = { valueOf: function() { s.n = s.n + 1; return -1; } };\n\
             const r = /a/g;\n\
             r.lastIndex = c;\n\
             const m = r.exec('nbc');\n\
             finish([m === null, r.lastIndex, s.n]);"
        ),
        Value::List(vec![Value::Bool(true), Value::Number(0.0), Value::Number(1.0)].into())
    );
    // The coerced index selects the start: `3` begins the search after the
    // first match, and the success writes the match's end.
    assert_eq!(
        finished(
            "const s = { n: 0 };\n\
             const c = { valueOf: function() { s.n = s.n + 1; return 3; } };\n\
             const r = /a/g;\n\
             r.lastIndex = c;\n\
             const m = r.exec('aabaa');\n\
             finish([m[0], m.index, r.lastIndex, s.n]);"
        ),
        Value::List(
            vec![
                Value::String("a".into()),
                Value::Number(3.0),
                Value::Number(4.0),
                Value::Number(1.0),
            ]
            .into()
        )
    );
}

/// `PropertyDefinitionEvaluation` evaluates the `ComputedPropertyName` —
/// `ToPropertyKey` included — before the value's `AssignmentExpression`.
#[test]
fn a_computed_object_literal_key_is_converted_before_its_value() {
    assert_eq!(
        finished(
            "const s = { v: 'bad' };\n\
             const key = { toString: function() { s.v = 'ok'; return 'p'; } };\n\
             const obj = { [key]: s.v };\n\
             finish(obj.p);"
        ),
        Value::String("ok".into())
    );
    // And the conversion still runs once for the member it writes.
    assert_eq!(
        finished(
            "const s = { n: 0 };\n\
             const key = { toString: function() { s.n = s.n + 1; return 'p'; } };\n\
             const obj = { [key]: 7 };\n\
             finish([s.n, obj.p]);"
        ),
        Value::List(vec![Value::Number(1.0), Value::Number(7.0)].into())
    );
}
