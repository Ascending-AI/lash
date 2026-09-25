//! FIG-3728: a member call binds the member before its argument list is
//! evaluated. The composite lowerings for a built-in name that may resolve
//! to a plain object's own member — the callback methods, `replace` and
//! `hasOwnProperty` — decide own-vs-built-in first: an argument that
//! replaces the member still calls the one the call looked up, and an
//! argument that adds a missing member still throws the `TypeError` ECMA's
//! `Call` does.

use super::*;

/// `o.map(f)` looks `o.map` up before `f` is evaluated: replacing the member
/// from an argument calls the function the lookup found.
#[test]
fn callback_methods_bind_the_member_before_evaluating_arguments() {
    assert_eq!(
        finished(
            "const o = { map: function() { return 'old'; } };\n\
             const change = function() { o.map = function() { return 'new'; }; return 0; };\n\
             finish(o.map(change()));"
        ),
        Value::String("old".into())
    );
    // Removing the member mid-call is the same lookup: the bound function
    // still runs.
    assert_eq!(
        finished(
            "const o = { filter: function() { return 'old'; } };\n\
             const change = function() { delete o.filter; return 0; };\n\
             finish(o.filter(change()));"
        ),
        Value::String("old".into())
    );
    // A member the receiver lacks is the TypeError of the looked-up
    // `undefined`, no matter what the argument adds.
    assert_eq!(
        finished(
            "const o = {};\n\
             const add = function() { o.map = function() { return 'added'; }; return 0; };\n\
             try { o.map(add()); finish('no throw'); } catch (e) { finish(e instanceof TypeError); }"
        ),
        Value::Bool(true)
    );
}

/// `o.replace(s, r)` binds `o.replace` before either argument runs.
#[test]
fn replace_binds_the_member_before_evaluating_arguments() {
    assert_eq!(
        finished(
            "const o = { replace: function() { return 'old'; } };\n\
             const change = function() { o.replace = function() { return 'new'; }; return 0; };\n\
             finish(o.replace(change(), 'x'));"
        ),
        Value::String("old".into())
    );
    // A function replacement takes the same composite's own-member arm.
    assert_eq!(
        finished(
            "const o = { replaceAll: function() { return 'old'; } };\n\
             const change = function() { o.replaceAll = function() { return 'new'; }; return 'a'; };\n\
             finish(o.replaceAll(change(), function() { return 'y'; }));"
        ),
        Value::String("old".into())
    );
}

/// `o.hasOwnProperty(k)` binds the member before the key is evaluated.
#[test]
fn has_own_property_binds_the_member_before_evaluating_its_key() {
    assert_eq!(
        finished(
            "const o = { hasOwnProperty: function() { return 'old'; } };\n\
             const change = function() { o.hasOwnProperty = function() { return 'new'; }; return 'k'; };\n\
             finish(o.hasOwnProperty(change()));"
        ),
        Value::String("old".into())
    );
    // Without an own member the built-in still answers — and still after the
    // key is evaluated in the ordinary call order.
    assert_eq!(
        finished(
            "const o = { k: 1 };\n\
             const key = { toString: function() { return 'k'; } };\n\
             finish(o.hasOwnProperty(key));"
        ),
        Value::Bool(true)
    );
}
